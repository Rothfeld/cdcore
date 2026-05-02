//! Prefab ReflectObject parser.
//!
//! Magic: ff ff 04 00 00 00 at offset 0.
//!
//! Header (14 bytes):
//!   0x00  6B  magic
//!   0x06  u32 LE  file_hash_1
//!   0x0A  u32 LE  file_hash_2
//!   0x0E  u32 LE  version/component_count (always 15 observed)
//!
//! Body: linear stream of Pearl Abyss ReflectObject serialization with
//! length-prefixed string values.

use crate::error::{read_u32_le, Result, ParseError};

const MAGIC_V3: &[u8] = &[0xFF, 0xFF, 0x03, 0x00, 0x00, 0x00];
const MAGIC_V4: &[u8] = &[0xFF, 0xFF, 0x04, 0x00, 0x00, 0x00];

/// Classification of a prefab string value.
#[derive(Debug, Clone, PartialEq)]
pub enum PrefabStringKind {
    FileRef,
    EnumTag,
    PropertyName,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct PrefabString {
    pub prefix_offset: usize,
    pub value_offset: usize,
    pub length: u32,
    pub value: String,
    pub kind: PrefabStringKind,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedPrefab {
    pub path: String,
    pub file_hash_1: u32,
    pub file_hash_2: u32,
    pub component_count: u32,
    pub strings: Vec<PrefabString>,
}

pub fn parse(data: &[u8], filename: &str) -> Result<ParsedPrefab> {
    let magic_ok = data.len() >= 6
        && (&data[..6] == MAGIC_V3 || &data[..6] == MAGIC_V4);
    if data.len() < 14 || !magic_ok {
        return Err(ParseError::magic(MAGIC_V4, &data[..6.min(data.len())], 0));
    }

    let file_hash_1     = read_u32_le(data, 6)?;
    let file_hash_2     = read_u32_le(data, 10)?;
    let component_count = read_u32_le(data, 14)?;

    let strings = scan_strings(data);

    Ok(ParsedPrefab {
        path: filename.to_string(),
        file_hash_1,
        file_hash_2,
        component_count,
        strings,
    })
}

/// Scan all length-prefixed UTF-8 strings from the prefab body.
fn scan_strings(data: &[u8]) -> Vec<PrefabString> {
    let mut strings = Vec::new();
    let mut off = 18usize; // start after header

    while off + 4 < data.len() {
        let length = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        if length == 0 || length > 4096 {
            off += 1;
            continue;
        }
        let text_start = off + 4;
        let text_end   = text_start + length as usize;
        if text_end > data.len() {
            off += 1;
            continue;
        }

        let chunk = &data[text_start..text_end];
        if !chunk.iter().all(|&b| b >= 0x20 || b == 0x09) {
            off += 1;
            continue;
        }

        if let Ok(text) = std::str::from_utf8(chunk) {
            let kind = classify_prefab_string(text);
            strings.push(PrefabString {
                prefix_offset: off,
                value_offset: text_start,
                length,
                value: text.to_string(),
                kind,
            });
            off = text_end;
            continue;
        }
        off += 1;
    }
    strings
}

fn classify_prefab_string(s: &str) -> PrefabStringKind {
    const FILE_EXTS: &[&str] = &[
        ".pac", ".pab", ".pam", ".pamlod", ".xml", ".dds", ".pah",
        ".app_xml", ".pac_xml", ".prefabdata_xml",
    ];
    const ENUM_PREFIXES: &[&str] = &[
        "Upperbody", "Cloak", "CD_", "Lower", "Hair", "Hat", "Face",
        "Glove", "Boot", "Chest", "Pants", "Accessory",
    ];

    let lower = s.to_lowercase();
    if FILE_EXTS.iter().any(|ext| lower.ends_with(ext)) || lower.contains('/') {
        return PrefabStringKind::FileRef;
    }
    if ENUM_PREFIXES.iter().any(|pfx| s.starts_with(pfx)) {
        return PrefabStringKind::EnumTag;
    }
    if s.starts_with('_') || s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        if s.contains("Component") || s.contains("Mesh") || s.contains("Bone") || s.contains("Tag") {
            return PrefabStringKind::PropertyName;
        }
    }
    PrefabStringKind::Unknown
}

/// Edit a string in a prefab (same-length mode only).
///
/// Returns an error if `new_value` has a different byte length than the original.
pub fn edit_string_same_length(
    data: &mut Vec<u8>,
    ps: &PrefabString,
    new_value: &str,
) -> Result<()> {
    let new_bytes = new_value.as_bytes();
    if new_bytes.len() != ps.length as usize {
        return Err(ParseError::Other(format!(
            "same-length edit: new value {} bytes != original {} bytes",
            new_bytes.len(), ps.length
        )));
    }
    let end = ps.value_offset + ps.length as usize;
    if end > data.len() {
        return Err(ParseError::eof(ps.value_offset, ps.length as usize, data.len() - ps.value_offset));
    }
    data[ps.value_offset..end].copy_from_slice(new_bytes);
    Ok(())
}
