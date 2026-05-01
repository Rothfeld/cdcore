//! PABC / PABV morph-target parser (PAR v4-v7).
//!
//! Header (20 bytes):
//!   [0:4]   magic "PAR "
//!   [4]     version ASCII '4'..'9'
//!   [5:8]   flags (3 bytes)
//!   [8:16]  signature run 0x02..0x09
//!   [16:20] u32 LE count (morph-target row count)
//!   [20..]  count * N fp32 payload + optional 0-3 byte trailer

use crate::error::{ParseError, Result};

const MAGIC: &[u8; 4] = b"PAR ";
const SIGNATURE_RUN: &[u8; 8] = &[0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
const HEADER_SIZE: usize = 20;

#[derive(Debug, Clone)]
pub struct PabcFile {
    pub version: u8,          // 4-9 (decoded from ASCII byte)
    pub flags: [u8; 3],
    pub count: u32,
    pub floats: Vec<f32>,
    pub trailer: Vec<u8>,     // 0-3 bytes after the fp32 grid
    pub raw_size: usize,
}

impl PabcFile {
    pub fn n_floats(&self) -> usize { self.floats.len() }

    pub fn row_floats_hint(&self) -> usize {
        if self.count == 0 || self.floats.is_empty() { return 0; }
        self.floats.len() / self.count as usize
    }

    pub fn in_range_ratio(&self) -> f32 {
        if self.floats.is_empty() { return 0.0; }
        let n = self.floats.iter().filter(|&&f| f > -2.0 && f < 2.0).count();
        n as f32 / self.floats.len() as f32
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + self.floats.len() * 4 + self.trailer.len());
        out.extend_from_slice(MAGIC);
        out.push(b'0' + self.version);
        out.extend_from_slice(&self.flags);
        out.extend_from_slice(SIGNATURE_RUN);
        out.extend_from_slice(&self.count.to_le_bytes());
        for &f in &self.floats {
            out.extend_from_slice(&f.to_le_bytes());
        }
        out.extend_from_slice(&self.trailer);
        out
    }
}

pub fn parse(data: &[u8]) -> Result<PabcFile> {
    if data.len() < HEADER_SIZE {
        return Err(ParseError::Other(format!(
            "PABC too small: {} bytes (need >= {})", data.len(), HEADER_SIZE
        )));
    }
    if &data[0..4] != MAGIC {
        return Err(ParseError::magic(MAGIC, &data[0..4], 0));
    }

    let version_byte = data[4];
    if !(b'4'..=b'9').contains(&version_byte) {
        return Err(ParseError::Other(format!(
            "unknown PABC version at offset 4: 0x{:02x}", version_byte
        )));
    }
    let version = version_byte - b'0';

    let flags = [data[5], data[6], data[7]];

    if &data[8..16] != SIGNATURE_RUN {
        return Err(ParseError::Other(format!(
            "bad PABC signature run at offset 8: {:?}", &data[8..16]
        )));
    }

    let count = u32::from_le_bytes(data[16..20].try_into().unwrap());

    let payload = &data[HEADER_SIZE..];
    let trailer_len = payload.len() % 4;
    let (floats_bytes, trailer) = if trailer_len > 0 {
        (&payload[..payload.len() - trailer_len], payload[payload.len() - trailer_len..].to_vec())
    } else {
        (payload, vec![])
    };

    let n = floats_bytes.len() / 4;
    let floats: Vec<f32> = (0..n)
        .map(|i| f32::from_le_bytes(floats_bytes[i*4..i*4+4].try_into().unwrap()))
        .collect();

    Ok(PabcFile { version, flags, count, floats, trailer, raw_size: data.len() })
}

pub fn is_par_file(data: &[u8]) -> bool {
    data.len() >= HEADER_SIZE
        && &data[0..4] == MAGIC
        && (b'4'..=b'9').contains(&data[4])
        && &data[8..16] == SIGNATURE_RUN
}
