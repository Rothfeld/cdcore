use crate::error::{ParseError, Result};

/// LZ4 block decompress. `orig_size` must match the expected output size.
pub fn decompress(data: &[u8], orig_size: usize) -> Result<Vec<u8>> {
    lz4_flex::block::decompress(data, orig_size)
        .map_err(|e| ParseError::Compression(format!("lz4 decompress: {e}")))
}

/// LZ4 block compress.
pub fn compress(data: &[u8]) -> Vec<u8> {
    lz4_flex::block::compress(data)
}
