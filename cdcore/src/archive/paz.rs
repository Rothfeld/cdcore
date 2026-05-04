//! PAZ archive reader -- opaque binary blobs; all metadata is in PAMT.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use crate::error::Result;

/// Read `size` bytes from a PAZ file at `offset`.
pub fn read_bytes(paz_path: &str, offset: u64, size: usize) -> Result<Vec<u8>> {
    let mut f = File::open(paz_path)?;
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

    let mut f = std::fs::OpenOptions::new().write(true).open(paz_path)?;
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
