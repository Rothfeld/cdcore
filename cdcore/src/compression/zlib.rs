use crate::error::{ParseError, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{Read, Write};

pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)
        .map_err(|e| ParseError::Compression(format!("zlib decompress: {e}")))?;
    Ok(out)
}

pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)
        .map_err(|e| ParseError::Compression(format!("zlib compress: {e}")))?;
    encoder.finish()
        .map_err(|e| ParseError::Compression(format!("zlib finish: {e}")))
}
