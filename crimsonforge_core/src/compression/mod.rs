pub mod lz4;
pub mod zlib;

use crate::error::{ParseError, Result};

pub const COMP_NONE: u8 = 0;
pub const COMP_TYPE1: u8 = 1;
pub const COMP_LZ4: u8 = 2;
pub const COMP_CUSTOM: u8 = 3;
pub const COMP_ZLIB: u8 = 4;

/// Decompress data according to the compression type from PAMT flags.
pub fn decompress(data: &[u8], orig_size: usize, compression_type: u8) -> Result<Vec<u8>> {
    match compression_type {
        COMP_NONE => Ok(data.to_vec()),
        COMP_TYPE1 => decompress_type1(data, orig_size),
        COMP_LZ4 => lz4::decompress(data, orig_size),
        COMP_CUSTOM => Err(ParseError::Compression(
            "compression type 3 (custom) not implemented".into(),
        )),
        COMP_ZLIB => zlib::decompress(data),
        t => Err(ParseError::Compression(format!("unknown compression type {t}"))),
    }
}

/// Compress data with LZ4 (the standard repacking compression type).
pub fn compress_lz4(data: &[u8]) -> Vec<u8> {
    lz4::compress(data)
}

/// Type-1 PAR: 80-byte uncompressed header + per-section LZ4 blocks.
fn decompress_type1(data: &[u8], orig_size: usize) -> Result<Vec<u8>> {
    if data.len() >= 4 && &data[..4] == b"PAR " {
        if let Ok(result) = try_decompress_type1_par(data) {
            if result.len() >= orig_size {
                return Ok(result[..orig_size].to_vec());
            }
        }
    }
    if orig_size > data.len() {
        if let Ok(result) = try_decompress_type1_prefixed_lz4(data, orig_size) {
            if result.len() >= orig_size {
                return Ok(result[..orig_size].to_vec());
            }
        }
    }
    // Fallback: return raw data (caller handles partial decode)
    Ok(data.to_vec())
}

fn try_decompress_type1_par(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 0x50 {
        return Err(ParseError::Other("too short for PAR".into()));
    }
    let mut output = data[..0x50].to_vec();
    let mut file_offset = 0x50usize;
    let mut saw_compressed = false;

    for slot in 0..8usize {
        let slot_off = 0x10 + slot * 8;
        if slot_off + 8 > data.len() {
            break;
        }
        let comp_size = u32::from_le_bytes(data[slot_off..slot_off + 4].try_into().unwrap()) as usize;
        let decomp_size = u32::from_le_bytes(data[slot_off + 4..slot_off + 8].try_into().unwrap()) as usize;

        if decomp_size == 0 {
            continue;
        }

        if comp_size > 0 {
            saw_compressed = true;
            let blob = data.get(file_offset..file_offset + comp_size)
                .ok_or_else(|| ParseError::eof(file_offset, comp_size, data.len() - file_offset))?;
            let decompressed = lz4::decompress(blob, decomp_size)?;
            output.extend_from_slice(&decompressed);
            file_offset += comp_size;
        } else {
            let raw = data.get(file_offset..file_offset + decomp_size)
                .ok_or_else(|| ParseError::eof(file_offset, decomp_size, data.len() - file_offset))?;
            output.extend_from_slice(raw);
            file_offset += decomp_size;
        }
    }

    if !saw_compressed {
        return Err(ParseError::Other("no compressed sections in PAR".into()));
    }

    // Zero out slot sizes in header to mark as fully decompressed
    for slot in 0..8usize {
        let off = 0x10 + slot * 8;
        if off + 4 <= output.len() {
            output[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
        }
    }

    Ok(output)
}

fn try_decompress_type1_prefixed_lz4(data: &[u8], orig_size: usize) -> Result<Vec<u8>> {
    if data.len() < 128 || &data[..4] != b"DDS " {
        return Err(ParseError::Other("not a DDS header".into()));
    }
    let header = data[..128].to_vec();
    let payload = &data[128..];
    let expected = orig_size.saturating_sub(128);
    if expected == 0 {
        return Ok(data.to_vec());
    }
    let decompressed = lz4::decompress(payload, expected)?;
    let mut out = header;
    out.extend_from_slice(&decompressed);
    Ok(out)
}
