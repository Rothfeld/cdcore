use crate::error::{Result, WmmoggError};
use super::bit_io::{BitReader, BitWriter};
use super::codebook_lookup::CodebookLut;

/// Strip a standard Vorbis setup packet (from OGG) to Wwise packed form.
///
/// Each expanded codebook is looked up in `lut` by hash; its 10-bit index is
/// written in place of the full expanded bytes.  Returns an error if any
/// codebook is not found in the provided library.
pub fn strip_setup_header(packet: &[u8], channels: usize, lut: &CodebookLut) -> Result<Vec<u8>> {
    if packet.len() < 7 || &packet[0..7] != b"\x05vorbis" {
        return Err(WmmoggError::VorbisParse("not a vorbis setup packet".into()));
    }
    let mut r = BitReader::new(&packet[7..]);
    let mut w = BitWriter::new();

    // Codebooks
    let cb_count_m1 = r.read_bits(8)? as usize;
    w.write_bits(cb_count_m1 as u32, 8);
    for _ in 0..=cb_count_m1 {
        let cb_bytes = read_full_codebook_bits(&mut r)?;
        let idx = lut.lookup(&cb_bytes).ok_or(WmmoggError::CodebookNotFound)?;
        w.write_bits(idx, 10);
    }

    // Time domain — standard Vorbis has count+1 entries of 0; Wwise omits entirely.
    let time_count_m1 = r.read_bits(6)? as usize;
    for _ in 0..=time_count_m1 {
        let v = r.read_bits(16)?;
        assert_eq!(v, 0, "time domain type must be 0");
    }

    // Floors
    let floor_count_m1 = r.read_bits(6)? as usize;
    w.write_bits(floor_count_m1 as u32, 6);
    for _ in 0..=floor_count_m1 {
        let floor_type = r.read_bits(16)?;
        assert_eq!(floor_type, 1, "only floor type 1 supported");
        strip_floor_type1(&mut r, &mut w)?;
    }

    // Residues
    let res_count_m1 = r.read_bits(6)? as usize;
    w.write_bits(res_count_m1 as u32, 6);
    for _ in 0..=res_count_m1 {
        let res_type = r.read_bits(16)?;
        w.write_bits(res_type, 2);
        strip_residue(&mut r, &mut w)?;
    }

    // Mappings
    let map_count_m1 = r.read_bits(6)? as usize;
    w.write_bits(map_count_m1 as u32, 6);
    for _ in 0..=map_count_m1 {
        let map_type = r.read_bits(16)?;
        assert_eq!(map_type, 0, "only mapping type 0 supported");
        strip_mapping(&mut r, &mut w, channels)?;
    }

    // Modes
    let mode_count_m1 = r.read_bits(6)? as usize;
    w.write_bits(mode_count_m1 as u32, 6);
    for _ in 0..=mode_count_m1 {
        let blockflag      = r.read_bit()?;
        let _window_type   = r.read_bits(16)?;
        let _transform_type = r.read_bits(16)?;
        let mapping_idx    = r.read_bits(8)?;
        w.write_bit(blockflag);
        w.write_bits(mapping_idx, 8);
    }

    let framing = r.read_bit()?;
    assert!(framing, "setup framing bit must be 1");

    Ok(w.finish())
}

fn strip_floor_type1(r: &mut BitReader, w: &mut BitWriter) -> Result<()> {
    let partitions = r.read_bits(5)?;
    w.write_bits(partitions, 5);
    let mut pc = vec![0u32; partitions as usize];
    for i in 0..partitions as usize {
        let cls = r.read_bits(4)?;
        w.write_bits(cls, 4);
        pc[i] = cls;
    }
    let max_class = pc.iter().copied().max().unwrap_or(0) as usize;
    let mut class_dims = vec![0u32; max_class + 1];
    for c in 0..=max_class {
        let dim_m1 = r.read_bits(3)?;
        w.write_bits(dim_m1, 3);
        class_dims[c] = dim_m1 + 1;
        let subclasses = r.read_bits(2)?;
        w.write_bits(subclasses, 2);
        if subclasses != 0 { w.write_bits(r.read_bits(8)?, 8); }
        for _ in 0..(1u32 << subclasses) { w.write_bits(r.read_bits(8)?, 8); }
    }
    w.write_bits(r.read_bits(2)?, 2); // mult_m1
    let rangebits = r.read_bits(4)?;
    w.write_bits(rangebits, 4);
    for i in 0..partitions as usize {
        for _ in 0..class_dims[pc[i] as usize] {
            w.write_bits(r.read_bits(rangebits as u8)?, rangebits as u8);
        }
    }
    Ok(())
}

fn strip_residue(r: &mut BitReader, w: &mut BitWriter) -> Result<()> {
    for _ in 0..3 { w.write_bits(r.read_bits(24)?, 24); } // begin, end, part_size_m1
    let cls_m1 = r.read_bits(6)?;
    w.write_bits(cls_m1, 6);
    w.write_bits(r.read_bits(8)?, 8); // classbook
    let classifications = cls_m1 as usize + 1;
    let mut cascade = vec![[false; 8]; classifications];
    for c in 0..classifications {
        let high = r.read_bits(3)?; w.write_bits(high, 3);
        let flag = r.read_bit()?;   w.write_bit(flag);
        let low  = if flag { let v = r.read_bits(5)?; w.write_bits(v, 5); v } else { 0 };
        let combined = high * 8 + if flag { low } else { 0 };
        for pass in 0..8 { cascade[c][pass] = (combined >> pass) & 1 != 0; }
    }
    for c in 0..classifications {
        for pass in 0..8 {
            if cascade[c][pass] { w.write_bits(r.read_bits(8)?, 8); }
        }
    }
    Ok(())
}

fn strip_mapping(r: &mut BitReader, w: &mut BitWriter, channels: usize) -> Result<()> {
    let submaps_flag = r.read_bit()?; w.write_bit(submaps_flag);
    let submaps = if submaps_flag {
        let s = r.read_bits(4)? + 1; w.write_bits(s - 1, 4); s as usize
    } else { 1 };
    let coupling_flag = r.read_bit()?; w.write_bit(coupling_flag);
    if coupling_flag {
        let ilog_ch = ilog((channels - 1) as u32) as u8;
        let steps = r.read_bits(8)? + 1; w.write_bits(steps - 1, 8);
        for _ in 0..steps {
            w.write_bits(r.read_bits(ilog_ch)?, ilog_ch);
            w.write_bits(r.read_bits(ilog_ch)?, ilog_ch);
        }
    }
    // 2 reserved bits are present in BOTH expanded Vorbis and Wwise stripped form.
    // ww2ogg rebuild_mapping reads them from the stripped input.
    let reserved = r.read_bits(2)?;
    assert_eq!(reserved, 0, "mapping reserved bits must be 0");
    w.write_bits(reserved, 2);
    if submaps > 1 {
        for _ in 0..channels { w.write_bits(r.read_bits(4)?, 4); }
    }
    for _ in 0..submaps {
        w.write_bits(r.read_bits(8)?, 8); // time_config
        w.write_bits(r.read_bits(8)?, 8); // floor_number
        w.write_bits(r.read_bits(8)?, 8); // residue_number
    }
    Ok(())
}

fn ilog(v: u32) -> u32 {
    if v == 0 { 0 } else { 32 - v.leading_zeros() }
}

// ---------------------------------------------------------------------------
// Codebook reading
//
// A single Vorbis codebook begins with:
//   sync_pattern (24 bits = 0x564342 "BCV")
//   dimensions (16 bits)
//   entries (24 bits)
//   ordered (1 bit)
//   if ordered: ...
//   else: ...
//   lookup_type (4 bits)
//   ...
//
// We read the codebook into a contiguous byte slice so we can hash it.
// We rebuild it identically to what ww2ogg emits (since that is exactly
// what ends up in the OGG we're reading).
// ---------------------------------------------------------------------------

fn read_full_codebook_bits(r: &mut BitReader) -> Result<Vec<u8>> {
    let mut w = BitWriter::new();

    let sync = r.read_bits(24)?;
    if sync != 0x564342 {
        return Err(WmmoggError::VorbisParse(
            format!("codebook sync pattern 0x{sync:06x} != 0x564342")
        ));
    }
    w.write_bits(sync, 24);

    let dimensions = r.read_bits(16)?;
    w.write_bits(dimensions, 16);
    let entries = r.read_bits(24)?;
    w.write_bits(entries, 24);

    let ordered = r.read_bit()?;
    w.write_bit(ordered);

    if ordered {
        let initial_len = r.read_bits(5)?;
        w.write_bits(initial_len, 5);
        let mut current_entry = 0u32;
        while current_entry < entries {
            let bits_needed = ilog(entries - current_entry) as u8;
            let count = r.read_bits(bits_needed)?;
            w.write_bits(count, bits_needed);
            current_entry += count;
        }
    } else {
        let sparse = r.read_bit()?;
        w.write_bit(sparse);
        for _ in 0..entries {
            if sparse {
                let present = r.read_bit()?;
                w.write_bit(present);
                if !present { continue; }
            }
            let length_minus1 = r.read_bits(5)?;
            w.write_bits(length_minus1, 5);
        }
    }

    let lookup_type = r.read_bits(4)?;
    w.write_bits(lookup_type, 4);

    if lookup_type == 1 || lookup_type == 2 {
        let min_val_bits = r.read_bits(32)?;
        w.write_bits(min_val_bits, 32);
        let delta_bits = r.read_bits(32)?;
        w.write_bits(delta_bits, 32);
        let value_bits = r.read_bits(4)? + 1;
        w.write_bits(value_bits - 1, 4);
        let sequence_flag = r.read_bit()?;
        w.write_bit(sequence_flag);

        let lookup_values = if lookup_type == 1 {
            book_map_type1_quantvals(entries, dimensions)
        } else {
            entries * dimensions
        };
        for _ in 0..lookup_values {
            let v = r.read_bits(value_bits as u8)?;
            w.write_bits(v, value_bits as u8);
        }
    }

    Ok(w.finish())
}

/// Skip a full expanded Vorbis codebook without capturing the bits.
pub fn read_full_codebook_bits_skip(r: &mut BitReader) -> crate::error::Result<()> {
    read_full_codebook_bits(r)?;
    Ok(())
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
