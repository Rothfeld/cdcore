//! PAZ archive reader -- opaque binary blobs; all metadata is in PAMT.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use crate::error::Result;

/// Open a PAZ file with full read/write/delete sharing on Windows.
///
/// The default share modes on Windows would let one open block any other
/// (e.g. a write attempt fails while a memmap read is active, or a backup
/// `fs::copy` fails because we are appending).  PAZ is append-only -- new
/// entries land past the previous EOF -- so concurrent readers and a single
/// writer never touch the same bytes.  Full sharing makes that safe in
/// practice and prevents `os error 32` (sharing violation) from killing
/// repacks when the same archive is being read elsewhere in the process.
fn open_paz(paz_path: &str, write: bool) -> std::io::Result<std::fs::File> {
    let mut opts = OpenOptions::new();
    if write { opts.write(true);  } else { opts.read(true); }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        opts.share_mode(0x1 | 0x2 | 0x4);
    }
    opts.open(paz_path)
}

/// Read `size` bytes from a PAZ file at `offset`.
pub fn read_bytes(paz_path: &str, offset: u64, size: usize) -> Result<Vec<u8>> {
    let mut f = open_paz(paz_path, false)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; size];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Get the total file size of a PAZ file.
pub fn file_size(paz_path: &str) -> Result<u64> {
    Ok(std::fs::metadata(paz_path)?.len())
}

/// Write bytes to a PAZ file at a 16-byte-aligned offset.
///
/// Returns the offset where the data was written.
pub fn append_entry(paz_path: &str, data: &[u8]) -> Result<u64> {
    use std::io::Write;
    let current_size = file_size(paz_path).unwrap_or(0);
    let aligned_offset = align16(current_size);
    let padding = (aligned_offset - current_size) as usize;

    let mut f = open_paz(paz_path, true)?;
    f.seek(SeekFrom::End(0))?;
    if padding > 0 {
        f.write_all(&vec![0u8; padding])?;
    }
    f.write_all(data)?;
    Ok(aligned_offset)
}

/// Align a byte offset to the next 16-byte boundary.
#[inline]
pub fn align16(offset: u64) -> u64 {
    (offset + 15) & !15
}
