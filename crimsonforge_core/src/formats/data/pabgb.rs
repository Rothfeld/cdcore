//! PABGB / PABGH game data table parser.
//!
//! PABGH header:
//!   [0:2] row_count (u16 LE)
//!   Followed by row_count descriptors in one of two formats:
//!
//!   Simple (5B): [row_id:u8][data_offset:u32]
//!     — detected when first id == 0x01 AND 2 + count*5 == pabgh size
//!
//!   Hashed (8B): [row_hash:u32][data_offset:u32]
//!     — general case
//!
//! PABGB body (hashed): row regions starting with the row hash.

use crate::error::{read_u16_le, Result, ParseError};

#[derive(Debug, Clone)]
pub enum FieldValue {
    U32(u32),
    I32(i32),
    F32(f32),
    Str(String),
    Blob(Vec<u8>),
}

impl FieldValue {
    pub fn display(&self) -> String {
        match self {
            Self::U32(v)  => if *v > 0xFFFF { format!("0x{v:08X}") } else { v.to_string() },
            Self::I32(v)  => v.to_string(),
            Self::F32(v)  => format!("{v:.4}"),
            Self::Str(s)  => s.clone(),
            Self::Blob(b) => {
                let hex: String = b.iter().take(20).map(|x| format!("{x:02x}")).collect();
                if b.len() > 20 { format!("{hex}...") } else { hex }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PabgbField {
    pub offset: usize,
    pub size: usize,
    pub raw: Vec<u8>,
    pub value: FieldValue,
}

#[derive(Debug, Clone)]
pub struct PabgbRow {
    pub index: usize,
    pub row_hash: u32,
    pub data_offset: u32,
    pub data_size: usize,
    pub name: String,
    pub fields: Vec<PabgbField>,
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PabgbTable {
    pub file_name: String,
    pub rows: Vec<PabgbRow>,
    pub is_simple: bool,
    pub row_size: usize,
}

/// Parse a PABGH header + PABGB body pair.
pub fn parse(pabgh_data: &[u8], pabgb_data: &[u8], filename: &str) -> Result<PabgbTable> {
    if pabgh_data.len() < 2 {
        return Err(ParseError::eof(0, 2, pabgh_data.len()));
    }

    let row_count = read_u16_le(pabgh_data, 0)? as usize;

    // Detect simple format: first byte == 1 and 2 + count*5 == file size
    let is_simple = pabgh_data.len() >= 3
        && pabgh_data[2] == 0x01
        && 2 + row_count * 5 == pabgh_data.len();

    let rows = if is_simple {
        parse_simple_rows(pabgh_data, pabgb_data, row_count, filename)?
    } else {
        parse_hashed_rows(pabgh_data, pabgb_data, row_count, filename)?
    };

    let row_size = if is_simple && rows.len() >= 2 {
        (rows[1].data_offset - rows[0].data_offset) as usize
    } else { 0 };

    Ok(PabgbTable {
        file_name: filename.to_string(),
        rows,
        is_simple,
        row_size,
    })
}

fn parse_simple_rows(
    pabgh: &[u8],
    pabgb: &[u8],
    count: usize,
    _filename: &str,
) -> Result<Vec<PabgbRow>> {
    let mut rows = Vec::with_capacity(count);
    let mut off = 2usize;

    let offsets: Vec<u32> = (0..count)
        .filter_map(|_| {
            if off + 5 > pabgh.len() { return None; }
            let _id = pabgh[off];
            let data_off = u32::from_le_bytes(pabgh[off+1..off+5].try_into().unwrap());
            off += 5;
            Some(data_off)
        })
        .collect();

    for (index, &data_off) in offsets.iter().enumerate() {
        let data_size = if index + 1 < offsets.len() {
            (offsets[index + 1] - data_off) as usize
        } else {
            pabgb.len().saturating_sub(data_off as usize)
        };
        let end = (data_off as usize + data_size).min(pabgb.len());
        let raw = pabgb.get(data_off as usize..end).unwrap_or(&[]).to_vec();
        let fields = parse_row_fields_blob(&raw);

        rows.push(PabgbRow {
            index,
            row_hash: 0,
            data_offset: data_off,
            data_size,
            name: format!("{}", index + 1),
            fields,
            raw,
        });
    }
    Ok(rows)
}

fn parse_hashed_rows(
    pabgh: &[u8],
    pabgb: &[u8],
    count: usize,
    _filename: &str,
) -> Result<Vec<PabgbRow>> {
    let mut rows = Vec::with_capacity(count);
    let mut off = 2usize;

    let header_rows: Vec<(u32, u32)> = (0..count)
        .filter_map(|_| {
            if off + 8 > pabgh.len() { return None; }
            let hash     = u32::from_le_bytes(pabgh[off..off+4].try_into().unwrap());
            let data_off = u32::from_le_bytes(pabgh[off+4..off+8].try_into().unwrap());
            off += 8;
            Some((hash, data_off))
        })
        .collect();

    let mut sorted_offsets: Vec<u32> = header_rows.iter().map(|&(_, o)| o).collect();
    sorted_offsets.sort_unstable();
    sorted_offsets.dedup();

    for (index, &(hash, data_off)) in header_rows.iter().enumerate() {
        // Find data_size = gap to next offset (or EOF)
        let next_off = sorted_offsets.iter()
            .find(|&&o| o > data_off)
            .copied()
            .unwrap_or(pabgb.len() as u32);
        let data_size = (next_off - data_off) as usize;
        let end = (data_off as usize + data_size).min(pabgb.len());
        let raw = pabgb.get(data_off as usize..end).unwrap_or(&[]).to_vec();

        // First field of hashed rows is the hash itself
        let fields = parse_row_fields(&raw);
        let name = extract_name(&fields, hash);

        rows.push(PabgbRow {
            index,
            row_hash: hash,
            data_offset: data_off,
            data_size,
            name,
            fields,
            raw,
        });
    }
    Ok(rows)
}

fn parse_row_fields(data: &[u8]) -> Vec<PabgbField> {
    let mut fields = Vec::new();
    let mut pos = 0usize;

    while pos < data.len() {
        let remaining = data.len() - pos;
        if remaining >= 8 && looks_like_string(data, pos) {
            let slen = u32::from_le_bytes(data[pos..pos+4].try_into().unwrap()) as usize;
            let text_end = pos + 4 + slen;
            if text_end <= data.len() {
                let text = std::str::from_utf8(&data[pos+4..text_end]).unwrap_or("").to_string();
                let raw = data[pos..text_end.min(data.len())].to_vec();
                fields.push(PabgbField {
                    offset: pos, size: 4 + slen, raw,
                    value: FieldValue::Str(text),
                });
                pos = text_end;
                // Skip null terminator if present
                if pos < data.len() && data[pos] == 0 { pos += 1; }
                continue;
            }
        }
        if remaining >= 4 {
            let raw: [u8; 4] = data[pos..pos+4].try_into().unwrap();
            let u = u32::from_le_bytes(raw);
            let f = f32::from_bits(u);
            let value = if looks_like_float(u, f) {
                FieldValue::F32(f)
            } else {
                FieldValue::U32(u)
            };
            fields.push(PabgbField { offset: pos, size: 4, raw: raw.to_vec(), value });
            pos += 4;
            continue;
        }
        // Leftover bytes → blob
        fields.push(PabgbField {
            offset: pos, size: remaining,
            raw: data[pos..].to_vec(),
            value: FieldValue::Blob(data[pos..].to_vec()),
        });
        break;
    }
    fields
}

fn parse_row_fields_blob(data: &[u8]) -> Vec<PabgbField> {
    if data.is_empty() { return vec![]; }
    vec![PabgbField {
        offset: 0, size: data.len(),
        raw: data.to_vec(),
        value: FieldValue::Blob(data.to_vec()),
    }]
}

fn looks_like_string(data: &[u8], pos: usize) -> bool {
    if pos + 8 > data.len() { return false; }
    let slen = u32::from_le_bytes(data[pos..pos+4].try_into().unwrap()) as usize;
    if slen == 0 || slen > 500 || pos + 4 + slen > data.len() { return false; }
    let chunk = &data[pos+4..pos+4+slen.min(20)];
    let printable = chunk.iter().filter(|&&b| (32..127).contains(&b)).count();
    printable >= (chunk.len() * 4) / 5
}

fn looks_like_float(u: u32, f: f32) -> bool {
    if u == 0 || u == 0xFFFFFFFF { return false; }
    if f.is_nan() || f.is_infinite() { return false; }
    let af = f.abs();
    af > 0.0001 && af < 100_000.0 && u > 0xFF
}

fn extract_name(fields: &[PabgbField], hash: u32) -> String {
    for f in fields.iter().skip(1) {
        if let FieldValue::Str(s) = &f.value {
            if !s.is_empty() { return s.clone(); }
        }
    }
    format!("0x{hash:08X}")
}
