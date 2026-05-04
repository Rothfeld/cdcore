//! PAMT per-group index parser/writer.
//!
//! Structure:
//!   [0:4]   self_crc (PaChecksum over data[12..])
//!   [4:8]   paz_count
//!   [8:12]  hash + zero
//!   PAZ table: paz_count x {crc:u32, size:u32} with 4-byte separator between
//!              entries (not after the last one)
//!   Folder section: [size:u32] + entries {parent:u32, name_len:u8, name:UTF-8}
//!   Node section:   [size:u32] + entries {parent:u32, name_len:u8, name:UTF-8}
//!   After nodes:    [folder_count:u32][hash:u32][folder_countx16 bytes]
//!   File records:   {node_ref:u32, offset:u32, comp_size:u32, orig_size:u32, flags:u32}
//!
//! paz_index = flags & 0xFF
//! compression_type = (flags >> 16) & 0x0F

use std::collections::HashMap;
use crate::crypto::pa_checksum;
use crate::error::{read_u8, read_u32_le, Result};

#[derive(Debug, Clone)]
pub struct PazTableEntry {
    pub index: usize,
    pub checksum: u32,
    pub size: u32,
    pub entry_offset: usize,
}

#[derive(Debug, Clone)]
pub struct PamtFileEntry {
    pub path: String,
    pub paz_file: String,
    pub offset: u64,
    pub comp_size: u32,
    pub orig_size: u32,
    pub flags: u32,
    pub paz_index: u8,
    pub record_offset: usize,
}

impl PamtFileEntry {
    pub fn compressed(&self) -> bool {
        self.comp_size != self.orig_size
    }

    pub fn compression_type(&self) -> u8 {
        ((self.flags >> 16) & 0x0F) as u8
    }

    pub fn encrypted(&self) -> bool {
        crate::crypto::is_encrypted(&self.path)
    }
}

#[derive(Debug, Clone)]
pub struct PamtData {
    pub path: String,
    pub self_crc: u32,
    pub paz_count: u32,
    pub paz_table: Vec<PazTableEntry>,
    pub file_entries: Vec<PamtFileEntry>,
    pub folder_prefix: String,
    pub raw_data: Vec<u8>,
}

pub fn parse_pamt(pamt_path: &str, paz_dir: Option<&str>) -> Result<PamtData> {
    let raw = std::fs::read(pamt_path)?;
    let paz_dir = paz_dir.unwrap_or_else(|| {
        std::path::Path::new(pamt_path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or(".")
    });
    let pamt_stem: u32 = std::path::Path::new(pamt_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    parse_pamt_bytes(&raw, pamt_path, paz_dir, pamt_stem)
}

pub fn parse_pamt_bytes(
    data: &[u8],
    path: &str,
    paz_dir: &str,
    pamt_stem: u32,
) -> Result<PamtData> {
    let self_crc  = read_u32_le(data, 0)?;
    let paz_count = read_u32_le(data, 4)?;
    let mut off = 16usize; // skip self_crc(4) + paz_count(4) + 8-byte hash/zero region

    // PAZ table: paz_count entries, each 8 bytes, with 4-byte separator between
    let mut paz_table = Vec::with_capacity(paz_count as usize);
    for i in 0..paz_count as usize {
        let entry_offset = off;
        let checksum = read_u32_le(data, off)?;
        let size     = read_u32_le(data, off + 4)?;
        paz_table.push(PazTableEntry { index: i, checksum, size, entry_offset });
        off += 8;
        if i + 1 < paz_count as usize {
            off += 4; // separator
        }
    }

    // Folder section
    let folder_size = read_u32_le(data, off)? as usize;
    off += 4;
    let folder_end = off + folder_size;
    let mut folder_prefix = String::new();

    while off < folder_end && off + 5 <= data.len() {
        let parent = read_u32_le(data, off)?;
        let slen   = read_u8(data, off + 4)? as usize;
        let name_start = off + 5;
        if name_start + slen > data.len() { break; }
        let name = std::str::from_utf8(&data[name_start..name_start + slen])
            .unwrap_or("")
            .to_string();
        if parent == 0xFFFFFFFF {
            folder_prefix = name;
        }
        off += 5 + slen;
    }
    off = folder_end;

    // Node section
    let node_size = read_u32_le(data, off)? as usize;
    off += 4;
    let node_start = off;
    let node_end   = off + node_size;

    let mut nodes: HashMap<u32, (u32, String)> = HashMap::new();
    while off < node_end && off + 5 <= data.len() {
        let rel    = (off - node_start) as u32;
        let parent = read_u32_le(data, off)?;
        let slen   = read_u8(data, off + 4)? as usize;
        let name_start = off + 5;
        if name_start + slen > data.len() { break; }
        let name = std::str::from_utf8(&data[name_start..name_start + slen])
            .unwrap_or("")
            .to_string();
        nodes.insert(rel, (parent, name));
        off += 5 + slen;
    }
    off = node_end;

    // Skip: folder_count(u32) + hash(u32) + folder_count*16 bytes
    if off + 4 > data.len() {
        return Ok(PamtData {
            path: path.to_string(), self_crc, paz_count, paz_table,
            file_entries: vec![], folder_prefix, raw_data: data.to_vec(),
        });
    }
    let folder_count = read_u32_le(data, off)? as usize;
    off += 4 + 4 + folder_count * 16;

    // File records: 20 bytes each
    let mut file_entries = Vec::new();
    while off + 20 <= data.len() {
        let record_offset = off;
        let node_ref  = read_u32_le(data, off)?;
        let paz_off   = read_u32_le(data, off + 4)?;
        let comp_size = read_u32_le(data, off + 8)?;
        let orig_size = read_u32_le(data, off + 12)?;
        let flags     = read_u32_le(data, off + 16)?;
        off += 20;

        let paz_index = (flags & 0xFF) as u8;
        let node_path = build_path(node_ref, &nodes);
        let full_path = if folder_prefix.is_empty() {
            node_path
        } else {
            format!("{}/{}", folder_prefix, node_path)
        };

        let paz_num = pamt_stem + paz_index as u32;
        let paz_file = format!("{}/{}.paz", paz_dir, paz_num);

        file_entries.push(PamtFileEntry {
            path: full_path,
            paz_file,
            offset: paz_off as u64,
            comp_size,
            orig_size,
            flags,
            paz_index,
            record_offset,
        });
    }

    Ok(PamtData {
        path: path.to_string(),
        self_crc,
        paz_count,
        paz_table,
        file_entries,
        folder_prefix,
        raw_data: data.to_vec(),
    })
}

fn build_path(node_ref: u32, nodes: &HashMap<u32, (u32, String)>) -> String {
    let mut parts = Vec::new();
    let mut cur = node_ref;
    let mut depth = 0;
    while cur != 0xFFFFFFFF && depth < 64 {
        match nodes.get(&cur) {
            Some((parent, name)) => {
                parts.push(name.as_str());
                cur = *parent;
            }
            None => break,
        }
        depth += 1;
    }
    parts.reverse();
    parts.join("")
}

/// Update a file record's comp_size, orig_size, and optionally offset.
pub fn update_file_record(
    raw: &mut [u8],
    record_offset: usize,
    new_comp_size: u32,
    new_orig_size: u32,
    new_offset: Option<u32>,
) {
    if let Some(off) = new_offset {
        raw[record_offset + 4..record_offset + 8].copy_from_slice(&off.to_le_bytes());
    }
    raw[record_offset + 8..record_offset + 12].copy_from_slice(&new_comp_size.to_le_bytes());
    raw[record_offset + 12..record_offset + 16].copy_from_slice(&new_orig_size.to_le_bytes());
}

/// Update a PAZ table entry's checksum and size.
pub fn update_paz_entry(raw: &mut [u8], entry: &PazTableEntry, new_checksum: u32, new_size: u32) {
    raw[entry.entry_offset..entry.entry_offset + 4]
        .copy_from_slice(&new_checksum.to_le_bytes());
    raw[entry.entry_offset + 4..entry.entry_offset + 8]
        .copy_from_slice(&new_size.to_le_bytes());
}

/// Recompute and write the PAMT self-CRC (at offset 0, over data[12..]).
pub fn update_self_crc(raw: &mut [u8]) -> u32 {
    let crc = pa_checksum(&raw[12..]);
    raw[..4].copy_from_slice(&crc.to_le_bytes());
    crc
}
