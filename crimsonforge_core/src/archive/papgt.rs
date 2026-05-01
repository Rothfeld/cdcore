//! PAPGT root index parser/writer.
//!
//! Structure:
//!   [0:4]   magic/flags (u32 LE)
//!   [4:8]   self_crc (PaChecksum over data[12..])
//!   [8:12]  header data
//!   [12..]  group entries, 12 bytes each:
//!             [0:4]  flags
//!             [4:8]  sequence
//!             [8:12] pamt_crc
//!
//! Entries are positional — the Nth entry = Nth sorted package directory.

use crate::crypto::pa_checksum;
use crate::error::{read_u32_le, Result};

#[derive(Debug, Clone)]
pub struct PapgtGroupEntry {
    pub entry_index: usize,
    pub flags: u32,
    pub sequence: u32,
    pub pamt_crc: u32,
    pub entry_offset: usize,
    pub crc_offset: usize,
}

#[derive(Debug, Clone)]
pub struct PapgtData {
    pub path: String,
    pub magic: u32,
    pub self_crc: u32,
    pub groups: Vec<PapgtGroupEntry>,
    pub raw_data: Vec<u8>,
    pub packages_path: String,
}

pub fn parse_papgt(path: &str) -> Result<PapgtData> {
    let raw_data = std::fs::read(path)?;
    parse_papgt_bytes(&raw_data, path)
}

pub fn parse_papgt_bytes(data: &[u8], path: &str) -> Result<PapgtData> {
    let magic = read_u32_le(data, 0)?;
    let self_crc = read_u32_le(data, 4)?;

    let mut groups = Vec::new();
    let mut off = 12usize;
    let mut index = 0usize;

    while off + 12 <= data.len() {
        let flags    = read_u32_le(data, off)?;
        let sequence = read_u32_le(data, off + 4)?;
        let pamt_crc = read_u32_le(data, off + 8)?;

        groups.push(PapgtGroupEntry {
            entry_index: index,
            flags,
            sequence,
            pamt_crc,
            entry_offset: off,
            crc_offset: off + 8,
        });
        off += 12;
        index += 1;
    }

    let packages_path = std::path::Path::new(path)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    Ok(PapgtData {
        path: path.to_string(),
        magic,
        self_crc,
        groups,
        raw_data: data.to_vec(),
        packages_path,
    })
}

/// Update a PAMT CRC in raw PAPGT bytes at the given offset.
pub fn update_pamt_crc(raw: &mut [u8], crc_offset: usize, new_crc: u32) {
    raw[crc_offset..crc_offset + 4].copy_from_slice(&new_crc.to_le_bytes());
}

/// Recompute and write the PAPGT self-CRC (at offset 4, over data[12..]).
pub fn update_self_crc(raw: &mut [u8]) -> u32 {
    let crc = pa_checksum(&raw[12..]);
    raw[4..8].copy_from_slice(&crc.to_le_bytes());
    crc
}

/// Get the byte offset of the PAMT CRC for a given package group directory name.
///
/// The PAPGT is positional — the Nth entry matches the Nth sorted directory.
pub fn pamt_crc_offset(papgt: &PapgtData, folder_number: u32) -> Option<usize> {
    let folder_name = format!("{folder_number:04}");
    let packages_path = std::path::Path::new(&papgt.packages_path);

    let mut sorted_dirs: Vec<String> = std::fs::read_dir(packages_path)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            p.is_dir() && p.join("0.pamt").exists()
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    sorted_dirs.sort();

    let idx = sorted_dirs.iter().position(|d| d == &folder_name)?;
    papgt.groups.get(idx).map(|g| g.crc_offset)
}
