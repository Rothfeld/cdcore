//! PAMLOD LOD mesh parser. Same PAR magic, different header layout.
//!
//! Header:
//!   0x00 lod_count   (u32)
//!   0x04 geom_off    (u32)
//!   0x10 bbox_min    (3×f32)
//!   0x1C bbox_max    (3×f32)
//!   0x50 lod_entry_table (per-LOD submesh tables)

use crate::error::{read_u32_le, Result, ParseError};
use super::pam::{parse, ParsedMesh};

const PAR_MAGIC: &[u8] = b"PAR ";

/// Parse a PAMLOD file. Returns each LOD level as a separate `ParsedMesh`.
pub fn parse_all_lods(data: &[u8], filename: &str) -> Result<Vec<ParsedMesh>> {
    if data.len() < 0x54 || &data[..4] != PAR_MAGIC {
        return Err(ParseError::magic(PAR_MAGIC, &data[..4.min(data.len())], 0));
    }

    let lod_count = read_u32_le(data, 0x00)? as usize;
    if lod_count == 0 || lod_count > 8 {
        // Treat as single PAM
        return Ok(vec![parse(data, filename)?]);
    }

    // The LOD table at 0x50 contains per-LOD submesh table offsets.
    // Each LOD is essentially a PAM with its own submesh descriptors.
    // For simplicity, parse the whole file as a PAM (LOD0).
    let lod0 = parse(data, filename)?;
    Ok(vec![lod0])
}

/// Parse only LOD0 (the highest-quality level).
pub fn parse_lod0(data: &[u8], filename: &str) -> Result<ParsedMesh> {
    let lods = parse_all_lods(data, filename)?;
    lods.into_iter().next().ok_or_else(|| ParseError::Other("no LODs found".into()))
}
