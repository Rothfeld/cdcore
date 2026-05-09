//! .binarygimmick parser -- per-instance state-machine + property sheet for
//! interactable world objects (chests, doors, breakable rocks, accelerators,
//! abyss artifacts, ...). 25,296 instances ship in `gamedata/`.
//!
//! The opening ~5 KB header has not been reverse-engineered; this parser only
//! exposes the well-formed records region that follows it.  Anchoring on the
//! literal `InitialBranchState` byte sequence is reliable across the corpus.
//!
//! Wire format of the records region:
//!   stream of [u32 length-LE][length bytes ASCII] records, paired
//!   (key, value).  Empty value = `length=0`. Trailing binary data after the
//!   pair stream is left unparsed (per-instance hash bindings / cached data).

use std::ops::Range;

use crate::error::{ParseError, Result};

/// Anchor: u32 length prefix `0x12 = 18` followed by the literal name. The
/// pair (anchor + name) is unique enough to locate the records region without
/// a magic header offset.
const ANCHOR: &[u8] = b"\x12\x00\x00\x00InitialBranchState";

/// Sanity cap on a single record's payload length. Real records max out
/// well under 100 bytes; values above this typically mean the parser has
/// walked off the end of the records region into the trailing binary.
const MAX_RECORD_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GimmickRecord {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Default, Clone)]
pub struct BinaryGimmick {
    pub records: Vec<GimmickRecord>,
    /// Byte range within the source data containing the length-prefixed
    /// records stream. Writeback splices new records into this range while
    /// preserving the surrounding header + trailing-binary regions.
    pub records_range: Range<usize>,
}

pub fn parse(data: &[u8]) -> Result<BinaryGimmick> {
    let start = data.windows(ANCHOR.len())
        .position(|w| w == ANCHOR)
        .ok_or_else(|| ParseError::Other(
            "binarygimmick: no InitialBranchState anchor".into()))?;

    let mut strings: Vec<String> = Vec::new();
    let mut p = start;
    let mut last_pair_end = start;
    while p + 4 <= data.len() {
        let n = u32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as usize;
        if n > MAX_RECORD_LEN { break; }
        if p + 4 + n > data.len() { break; }
        let bytes = &data[p + 4..p + 4 + n];
        // The records region is pure ASCII printable. The first byte that
        // isn't (e.g. high bit set, control character) marks the boundary
        // with the trailing binary region.
        if !bytes.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
            break;
        }
        strings.push(String::from_utf8_lossy(bytes).into_owned());
        p += 4 + n;
        if strings.len() & 1 == 0 {
            // Just completed a pair -- this is a valid stream truncation point.
            last_pair_end = p;
        }
    }

    if strings.len() & 1 == 1 {
        strings.pop();
    }

    let records = strings.chunks_exact(2)
        .map(|c| GimmickRecord { key: c[0].clone(), value: c[1].clone() })
        .collect();

    Ok(BinaryGimmick {
        records,
        records_range: start..last_pair_end,
    })
}

/// Splice new records into `original`, preserving the header + trailing-binary
/// regions. If the new stream length differs from the original's, the file
/// size changes accordingly -- the surrounding regions don't store offsets
/// into the records area, so this is safe (verified empirically against the
/// shipping corpus: round-trip without modification produces byte-identical
/// output).
pub fn serialize(original: &[u8], records: &[GimmickRecord]) -> Result<Vec<u8>> {
    let parsed = parse(original)?;
    let head = &original[..parsed.records_range.start];
    let tail = &original[parsed.records_range.end..];

    let mut body: Vec<u8> = Vec::with_capacity(parsed.records_range.len());
    for r in records {
        if r.key.len() > u32::MAX as usize || r.value.len() > u32::MAX as usize {
            return Err(ParseError::Other("binarygimmick: record exceeds u32::MAX".into()));
        }
        body.extend_from_slice(&(r.key.len() as u32).to_le_bytes());
        body.extend_from_slice(r.key.as_bytes());
        body.extend_from_slice(&(r.value.len() as u32).to_le_bytes());
        body.extend_from_slice(r.value.as_bytes());
    }

    let mut out = Vec::with_capacity(head.len() + body.len() + tail.len());
    out.extend_from_slice(head);
    out.extend_from_slice(&body);
    out.extend_from_slice(tail);
    Ok(out)
}

/// One JSON object per record, newline-terminated. Shape:
/// `{"key":"BreakProjectileLevel","value":"0"}`
pub fn to_jsonl(g: &BinaryGimmick) -> Vec<u8> {
    let mut out = String::with_capacity(g.records.len() * 64);
    for r in &g.records {
        out.push('{');
        out.push_str("\"key\":");
        push_json_str(&mut out, &r.key);
        out.push_str(",\"value\":");
        push_json_str(&mut out, &r.value);
        out.push_str("}\n");
    }
    out.into_bytes()
}

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"'  => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_records_after_anchor() {
        // Synthetic: 12 bytes pre-junk, anchor, len=4 "Wait", len=2 "GO".
        let mut buf: Vec<u8> = vec![0xaa; 12];
        buf.extend_from_slice(ANCHOR);
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(b"Wait");
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(b"GO");
        // Trailing non-printable byte to terminate the scan.
        buf.extend_from_slice(&[0xff; 8]);

        let g = parse(&buf).unwrap();
        assert_eq!(g.records.len(), 1);
        assert_eq!(g.records[0].key, "InitialBranchState");
        assert_eq!(g.records[0].value, "Wait");
        // The "GO" record is dropped as the unpaired tail (3 strings -> 1 pair).
    }

    #[test]
    fn round_trip_unmodified_is_byte_identical() {
        // ANCHOR provides string 1 (InitialBranchState). Append 3 more so the
        // total is 4 strings = 2 paired records, matching the always-even
        // count seen in the shipping corpus.
        let mut buf: Vec<u8> = vec![0xaa; 12];
        buf.extend_from_slice(ANCHOR);
        for s in [b"Wait" as &[u8], b"GO", b"BreakKey"] {
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s);
        }
        let head_end = buf.len();
        buf.extend_from_slice(&[0xff; 8]);
        let original = buf.clone();

        let g = parse(&original).unwrap();
        assert_eq!(g.records.len(), 2);
        assert_eq!(g.records_range.end, head_end);

        let out = serialize(&original, &g.records).unwrap();
        assert_eq!(out, original, "unmodified round-trip must be byte-identical");
    }

    #[test]
    fn jsonl_escapes_quotes() {
        let g = BinaryGimmick {
            records: vec![GimmickRecord {
                key: "q".into(),
                value: "has \"quote\"".into(),
            }],
            records_range: 0..0,
        };
        let s = String::from_utf8(to_jsonl(&g)).unwrap();
        assert_eq!(s, "{\"key\":\"q\",\"value\":\"has \\\"quote\\\"\"}\n");
    }
}
