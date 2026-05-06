//! Persistent vector store: raw f32 embeddings + parallel paths file.
//!
//! Layout of `embeddings.bin`:
//!   magic   : [u8; 4]  = b"CDML"
//!   version : u32 le   = 1
//!   dim     : u32 le
//!   count   : u64 le
//!   data    : count * dim * f32 le   (row-major, L2-normalized)
//!
//! Paired file `paths.txt`: one UTF-8 path per line, exactly `count` lines.
//!
//! Why raw + mmap instead of lancedb/parquet:
//!   100k * 768 * 4 = ~300 MB. mmap is instant, no schema, no query planner.
//!   Brute-force cosine over the whole corpus is 5-15 ms on a modern CPU.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use memmap2::Mmap;

use crate::error::{CdmlError, Result};

const MAGIC: &[u8; 4] = b"CDML";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 4 + 4 + 4 + 8; // magic + version + dim + count

/// Append-only writer. Header is rewritten on `finish()` once final count is known.
pub struct IndexWriter {
    bin_path: PathBuf,
    paths_path: PathBuf,
    bin: BufWriter<File>,
    paths: BufWriter<File>,
    dim: usize,
    count: u64,
}

impl IndexWriter {
    /// Create a fresh index at `dir/embeddings.bin` + `dir/paths.txt`.
    /// Overwrites if they already exist.
    pub fn create(dir: &Path, dim: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let bin_path = dir.join("embeddings.bin");
        let paths_path = dir.join("paths.txt");

        let bin_file = OpenOptions::new()
            .write(true).create(true).truncate(true).open(&bin_path)?;
        let paths_file = OpenOptions::new()
            .write(true).create(true).truncate(true).open(&paths_path)?;

        let mut bin = BufWriter::new(bin_file);
        // Reserve header bytes; rewritten in finish() with actual count.
        write_header(&mut bin, dim as u32, 0)?;

        Ok(Self {
            bin_path,
            paths_path,
            bin,
            paths: BufWriter::new(paths_file),
            dim,
            count: 0,
        })
    }

    /// Append one embedding + path. Vector must already be L2-normalized.
    pub fn push(&mut self, vec: &[f32], path: &str) -> Result<()> {
        if vec.len() != self.dim {
            return Err(CdmlError::Index(format!(
                "push: vector len {} != index dim {}",
                vec.len(),
                self.dim
            )));
        }
        for &f in vec {
            self.bin.write_f32::<LittleEndian>(f)?;
        }
        // Forbid newlines in stored paths so paths.txt stays line-delimited.
        if path.contains('\n') {
            return Err(CdmlError::Index(format!("path contains newline: {path:?}")));
        }
        writeln!(self.paths, "{path}")?;
        self.count += 1;
        Ok(())
    }

    /// Flush both files and rewrite the bin header with the final count.
    pub fn finish(mut self) -> Result<(PathBuf, PathBuf, u64)> {
        self.bin.flush()?;
        self.paths.flush()?;

        // Rewrite header with the final count.
        let mut bin_file = self.bin.into_inner().map_err(|e| e.into_error())?;
        bin_file.seek(SeekFrom::Start(0))?;
        write_header(&mut bin_file, self.dim as u32, self.count)?;
        bin_file.flush()?;

        Ok((self.bin_path, self.paths_path, self.count))
    }
}

fn write_header<W: Write>(w: &mut W, dim: u32, count: u64) -> Result<()> {
    w.write_all(MAGIC)?;
    w.write_u32::<LittleEndian>(VERSION)?;
    w.write_u32::<LittleEndian>(dim)?;
    w.write_u64::<LittleEndian>(count)?;
    Ok(())
}

/// mmap-backed read-only view of an index.
pub struct IndexReader {
    pub dim: usize,
    pub count: usize,
    pub paths: Vec<String>,
    mmap: Mmap,
}

impl IndexReader {
    /// Open an existing index dir. Validates header + file size + path count.
    pub fn open(dir: &Path) -> Result<Self> {
        let bin_path = dir.join("embeddings.bin");
        let paths_path = dir.join("paths.txt");

        let mut bin = File::open(&bin_path)?;
        let (dim, count) = read_header(&mut bin)?;
        let mmap = unsafe { Mmap::map(&bin)? };

        let expected = HEADER_LEN + (count as usize) * (dim as usize) * 4;
        if mmap.len() != expected {
            return Err(CdmlError::Index(format!(
                "embeddings.bin size {} != expected {} (dim={}, count={})",
                mmap.len(), expected, dim, count
            )));
        }

        let paths_file = File::open(&paths_path)?;
        let paths: Vec<String> = BufReader::new(paths_file)
            .lines()
            .collect::<std::io::Result<Vec<_>>>()?;
        if paths.len() != count as usize {
            return Err(CdmlError::Index(format!(
                "paths.txt has {} lines, header says count={}",
                paths.len(), count
            )));
        }

        Ok(Self {
            dim: dim as usize,
            count: count as usize,
            paths,
            mmap,
        })
    }

    /// Borrow the full embedding matrix as a contiguous `f32` slice
    /// of length `count * dim` (row-major).
    pub fn vectors(&self) -> &[f32] {
        let bytes = &self.mmap[HEADER_LEN..];
        // Safety: header validated len; alignment of mmap may not be 4 on all
        // platforms, so go through `from_le_bytes` per element via cast trick.
        // memmap2 returns page-aligned mappings, which are 4-byte aligned.
        let ptr = bytes.as_ptr() as *const f32;
        debug_assert_eq!((ptr as usize) % std::mem::align_of::<f32>(), 0);
        unsafe { std::slice::from_raw_parts(ptr, self.count * self.dim) }
    }

    /// Borrow the i-th embedding row.
    pub fn row(&self, i: usize) -> &[f32] {
        assert!(i < self.count, "row index {i} out of bounds (count={})", self.count);
        let off = i * self.dim;
        &self.vectors()[off..off + self.dim]
    }
}

fn read_header<R: Read>(r: &mut R) -> Result<(u32, u64)> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(CdmlError::Index(format!(
            "bad magic: {magic:?} (expected {MAGIC:?})"
        )));
    }
    let version = r.read_u32::<LittleEndian>()?;
    if version != VERSION {
        return Err(CdmlError::Index(format!(
            "unsupported index version: {version}"
        )));
    }
    let dim = r.read_u32::<LittleEndian>()?;
    let count = r.read_u64::<LittleEndian>()?;
    Ok((dim, count))
}
