//! PALOC localization file parser.
//!
//! Format: stream of length-prefixed UTF-8 string triplets.
//!   Triplet: [empty_marker:u32-len][empty_str] [id_len:u32][numeric_id] [text_len:u32][text]
//! Also supports symbolic pair format: [key_len:u32][key] [text_len:u32][text]

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct PalocEntry {
    pub key: String,
    pub value: String,
    pub key_offset: usize,
    pub value_offset: usize,
}

#[derive(Debug, Default, Clone)]
pub struct PalocData {
    pub path: String,
    pub entries: Vec<PalocEntry>,
}

const MAX_STR_LEN: usize = 50_000_000;

pub fn parse(data: &[u8], filename: &str) -> Result<PalocData> {
    let entries = scan_entries(data);
    Ok(PalocData { path: filename.to_string(), entries })
}

fn scan_entries(data: &[u8]) -> Vec<PalocEntry> {
    let strings = scan_strings(data);
    let mut entries = Vec::new();
    let mut i = 0;
    let count = strings.len();

    while i < count {
        let (off0, len0, ref s0) = strings[i];

        // Numeric triplet: empty marker, numeric id, text
        if len0 == 0 && i + 2 < count {
            let (off1, _len1, ref s1) = strings[i + 1];
            let (off2, _len2, ref s2) = strings[i + 2];
            if s1.chars().all(|c| c.is_ascii_digit()) {
                let value_offset = off2 + 4; // skip length prefix
                entries.push(PalocEntry {
                    key: s1.clone(),
                    value: s2.clone(),
                    key_offset: off1 + 4,
                    value_offset,
                });
                i += 3;
                continue;
            }
        }

        // Symbolic pair: key, text
        if is_symbolic_key(s0) && i + 1 < count {
            let (off1, _len1, ref s1) = strings[i + 1];
            entries.push(PalocEntry {
                key: s0.clone(),
                value: s1.clone(),
                key_offset: off0 + 4,
                value_offset: off1 + 4,
            });
            i += 2;
            continue;
        }

        i += 1;
    }
    entries
}

fn scan_strings(data: &[u8]) -> Vec<(usize, usize, String)> {
    let data_len = data.len();
    let mut result = Vec::new();
    let mut off = 4usize; // skip file header u32

    while off + 4 <= data_len {
        let slen = u32::from_le_bytes(data[off..off+4].try_into().unwrap()) as usize;
        if slen > MAX_STR_LEN || off + 4 + slen > data_len {
            off += 4;
            continue;
        }
        if slen == 0 {
            result.push((off, 0, String::new()));
            off += 4;
            continue;
        }

        let chunk = &data[off + 4..off + 4 + slen];
        // Reject strings with control characters (below 0x09 or 0x0E-0x1F)
        if chunk.iter().any(|&b| b < 0x09 || (0x0E..=0x1F).contains(&b)) {
            off += 4;
            continue;
        }
        match std::str::from_utf8(chunk) {
            Ok(text) => {
                result.push((off, slen, text.to_string()));
                off += 4 + slen;
            }
            Err(_) => { off += 4; }
        }
    }
    result
}

fn is_symbolic_key(s: &str) -> bool {
    if s.is_empty() || s.len() > 200 { return false; }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii() || !(first.is_alphabetic() || first == '_') { return false; }
    s.chars().all(|c| c.is_ascii() && (c.is_alphanumeric() || c == '_' || c == '.' || c == '-'))
}

/// Serialize entries back to paloc format (for writing modified strings).
pub fn serialize(entries: &[PalocEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    // Placeholder header u32
    out.extend_from_slice(&0u32.to_le_bytes());

    for entry in entries {
        // Empty marker
        out.extend_from_slice(&0u32.to_le_bytes());
        // Key
        let kb = entry.key.as_bytes();
        out.extend_from_slice(&(kb.len() as u32).to_le_bytes());
        out.extend_from_slice(kb);
        // Value
        let vb = entry.value.as_bytes();
        out.extend_from_slice(&(vb.len() as u32).to_le_bytes());
        out.extend_from_slice(vb);
    }
    out
}
