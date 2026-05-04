//! PAA_METABIN animation metadata parser.
//!
//! Header (80 bytes, fixed across all shipping files):
//!   0x00  ff ff 04 00        magic
//!   0x04  10 zero bytes
//!   0x0E  u16 = 15           schema constant
//!   ...   (see spec)
//! Record stream after 0x50:
//!   0x05 u8 marker
//!   u16 subtype
//!   u8 pad
//!   u8 tag
//!   var payload

use crate::error::{read_u8, read_u16_le, Result, ParseError};

const MAGIC: &[u8] = &[0xFF, 0xFF, 0x04, 0x00];
const HEADER_SIZE: usize = 0x50;

#[derive(Debug, Clone)]
pub struct MetabinRecord {
    pub subtype: u16,
    pub tag: u8,
    pub payload: Vec<u8>,
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct PaaMetabin {
    pub path: String,
    pub records: Vec<MetabinRecord>,
    pub file_size: usize,
}

pub fn parse(data: &[u8], filename: &str) -> Result<PaaMetabin> {
    if data.len() < 4 || &data[..4] != MAGIC {
        return Err(ParseError::magic(MAGIC, &data[..4.min(data.len())], 0));
    }

    let mut records = Vec::new();
    let mut off = HEADER_SIZE;

    while off + 5 <= data.len() {
        let marker = read_u8(data, off)?;
        if marker != 0x05 { off += 1; continue; }

        let subtype = read_u16_le(data, off + 1)?;
        let _pad    = read_u8(data, off + 3)?;
        let tag     = read_u8(data, off + 4)?;
        let record_start = off;
        off += 5;

        // Payload length depends on tag; we consume until next marker or EOF
        let payload_start = off;
        while off < data.len() {
            if data[off] == 0x05 && off + 4 < data.len() {
                // Possible next record -- stop here
                break;
            }
            off += 1;
        }

        records.push(MetabinRecord {
            subtype,
            tag,
            payload: data[payload_start..off].to_vec(),
            offset: record_start,
        });
    }

    Ok(PaaMetabin {
        path: filename.to_string(),
        records,
        file_size: data.len(),
    })
}
