/// Packed codebook library → index lookup table.
///
/// Builds a HashMap<sha256(expanded_codebook), library_index> from the embedded
/// packed library binaries (sourced from ww2ogg, BSD-3-Clause).
/// Used by strip_setup_header to convert each expanded Vorbis codebook back to
/// its 10-bit index in the packed library.

use super::bit_io::{BitReader, BitWriter};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::sync::OnceLock;
use crate::error::Result;

// Packed codebook libraries (sourced from ww2ogg 0.1.0, BSD-3-Clause).
const PACKED_DEFAULT: &[u8] = include_bytes!("../codebooks/packed_codebooks.bin");
const PACKED_AOTUV:   &[u8] = include_bytes!("../codebooks/packed_codebooks_aoTuV_603.bin");

pub struct CodebookLut {
    map: HashMap<[u8; 32], u32>,
}

impl CodebookLut {
    /// Return the library index for an expanded codebook, or None if not found.
    pub fn lookup(&self, expanded_cb_bytes: &[u8]) -> Option<u32> {
        let hash: [u8; 32] = Sha256::digest(expanded_cb_bytes).into();
        self.map.get(&hash).copied()
    }
}

pub fn default_lut() -> Result<&'static CodebookLut> {
    static LUT: OnceLock<CodebookLut> = OnceLock::new();
    Ok(LUT.get_or_init(|| {
        build_lut(PACKED_DEFAULT)
            .expect("embedded packed_codebooks.bin failed to build LUT")
    }))
}

pub fn aotuv_lut() -> Result<&'static CodebookLut> {
    static LUT: OnceLock<CodebookLut> = OnceLock::new();
    Ok(LUT.get_or_init(|| {
        build_lut(PACKED_AOTUV)
            .expect("embedded packed_codebooks_aoTuV_603.bin failed to build LUT")
    }))
}

fn build_lut(packed: &[u8]) -> std::result::Result<CodebookLut, String> {
    let (codebook_data, offsets) = parse_packed_library(packed)?;
    let n = offsets.len().saturating_sub(1);
    let mut map = HashMap::with_capacity(n);
    for i in 0..n {
        let cb = &codebook_data[offsets[i]..offsets[i + 1]];
        let expanded = expand_packed_codebook(cb)
            .map_err(|e| format!("expand codebook {i}: {e}"))?;
        let hash: [u8; 32] = Sha256::digest(&expanded).into();
        map.insert(hash, i as u32);
    }
    Ok(CodebookLut { map })
}

/// Parse the packed library binary.
///
/// Layout: [codebook_data][offset_table: (n+1) u32le][table_offset: u32le]
/// offset_table[i]..offset_table[i+1] = byte range of packed codebook i.
fn parse_packed_library(packed: &[u8]) -> std::result::Result<(&[u8], Vec<usize>), String> {
    if packed.len() < 8 {
        return Err("packed library too small".into());
    }
    let table_offset = u32::from_le_bytes(
        packed[packed.len() - 4..].try_into().unwrap()
    ) as usize;
    if table_offset + 8 > packed.len() {
        return Err(format!("table_offset {table_offset} out of range"));
    }
    let n_offsets = (packed.len() - 4 - table_offset) / 4;
    let mut offsets = Vec::with_capacity(n_offsets);
    for i in 0..n_offsets {
        let pos = table_offset + i * 4;
        let v = u32::from_le_bytes(packed[pos..pos + 4].try_into().unwrap()) as usize;
        offsets.push(v);
    }
    Ok((&packed[..table_offset], offsets))
}

/// Expand one packed library entry to standard Vorbis codebook form.
///
/// Packed format (from ww2ogg rebuild_internal / rebuild_codebook_data):
///   dimensions  : 4 bits  (standard Vorbis: 16 bits)
///   entries     : 14 bits (standard Vorbis: 24 bits)
///   ordered     : 1 bit
///   if ordered  : initial_length (5), run-length counts (ilog bits each)
///   if !ordered : codeword_length_length (3) + sparse (1)
///                 then per-entry: [if sparse: present (1)] + length (cll bits → 5 bits out)
///   lookup_type : 1 bit   (standard Vorbis: 4 bits)
///   if lookup==1: min (32), max (32), value_length (4), seq_flag (1), values
fn expand_packed_codebook(packed_cb: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut r = BitReader::new(packed_cb);
    let mut w = BitWriter::new();

    let dimensions = r.read_bits(4).map_err(|e| format!("dims: {e}"))?;
    let entries    = r.read_bits(14).map_err(|e| format!("entries: {e}"))?;

    w.write_bits(0x564342, 24); // Vorbis sync
    w.write_bits(dimensions, 16);
    w.write_bits(entries, 24);

    let ordered = r.read_bit().map_err(|e| format!("ordered: {e}"))?;
    w.write_bit(ordered);

    if ordered {
        let init_len = r.read_bits(5).map_err(|e| format!("init_len: {e}"))?;
        w.write_bits(init_len, 5);
        let mut current = 0u32;
        while current < entries {
            let bits = ilog(entries - current) as u8;
            let count = r.read_bits(bits).map_err(|e| format!("count: {e}"))?;
            w.write_bits(count, bits);
            current += count;
        }
    } else {
        // Packed: codeword_length_length (3 bits) then sparse (1 bit).
        let cll = r.read_bits(3).map_err(|e| format!("cll: {e}"))? as u8;
        let sparse = r.read_bit().map_err(|e| format!("sparse: {e}"))?;
        if cll == 0 || cll > 5 {
            return Err(format!("nonsense codeword_length_length {cll}"));
        }
        // Standard Vorbis: just sparse flag, no cll.
        w.write_bit(sparse);
        for _ in 0..entries {
            let present = if sparse {
                let p = r.read_bit().map_err(|e| format!("present: {e}"))?;
                w.write_bit(p);
                p
            } else {
                true
            };
            if present {
                let len = r.read_bits(cll).map_err(|e| format!("len: {e}"))?;
                w.write_bits(len, 5); // always 5 bits in standard Vorbis
            }
        }
    }

    // Packed: 1-bit lookup type. Standard Vorbis: 4-bit lookup type.
    let lookup_type = r.read_bit().map_err(|e| format!("lookup_type: {e}"))?;
    w.write_bits(if lookup_type { 1 } else { 0 }, 4);

    if lookup_type {
        let min = r.read_bits(32).map_err(|e| format!("min: {e}"))?;
        let max = r.read_bits(32).map_err(|e| format!("max: {e}"))?;
        let value_length = r.read_bits(4).map_err(|e| format!("value_length: {e}"))?;
        let seq = r.read_bit().map_err(|e| format!("seq: {e}"))?;
        w.write_bits(min, 32);
        w.write_bits(max, 32);
        w.write_bits(value_length, 4);
        w.write_bit(seq);
        let quantvals = book_map_type1_quantvals(entries, dimensions);
        for _ in 0..quantvals {
            let v = r.read_bits((value_length + 1) as u8)
                .map_err(|e| format!("val: {e}"))?;
            w.write_bits(v, (value_length + 1) as u8);
        }
    }

    Ok(w.finish())
}

fn ilog(v: u32) -> u32 {
    if v == 0 { 0 } else { 32 - v.leading_zeros() }
}

fn book_map_type1_quantvals(entries: u32, dimensions: u32) -> u32 {
    let mut vals = (entries as f64).powf(1.0 / dimensions as f64) as u32;
    loop {
        if vals.saturating_pow(dimensions) > entries { vals -= 1; break; }
        if (vals + 1).saturating_pow(dimensions) > entries { break; }
        vals += 1;
    }
    vals
}
