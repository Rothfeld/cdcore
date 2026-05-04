/// OGG → WEM reverse conversion.
///
/// Requires the OGG to have been produced by wemcodec's forward path
/// (i.e. contain a WEM_ROUNDTRIP_V1 Vorbis comment).

mod bit_io;
mod codebook_lookup;
mod packet_strip;
mod riff_build;
mod setup_strip;

use crate::error::{Result, WmmoggError};
use crate::ogg_comment::parse_roundtrip_comment;
use crate::wem::PacketHeaderFormat;
use riff_build::{build_data_chunk_2byte, build_data_chunk_6byte};

fn ilog_ch(v: u32) -> u32 { if v == 0 { 0 } else { 32 - v.leading_zeros() } }

pub(crate) use codebook_lookup::{default_lut, aotuv_lut};
pub(crate) use packet_strip::{strip_audio_packet, Mode};
pub(crate) use setup_strip::strip_setup_header;

pub fn ogg_to_wem(ogg_bytes: &[u8]) -> Result<Vec<u8>> {
    let packets = extract_vorbis_packets(ogg_bytes)?;

    if packets.len() < 3 {
        return Err(WmmoggError::OggParse("need at least 3 vorbis packets (id, comment, setup)".into()));
    }

    // Packet 0: ID header — gives us sample_rate, channels, blocksizes.
    // Packet 1: comment — carries WEM_ROUNDTRIP_V1 tag.
    // Packet 2: setup header.
    // Packets 3..: audio.

    let meta = parse_roundtrip_comment(&packets[1])
        .ok_or_else(|| WmmoggError::OggParse(
            "WEM_ROUNDTRIP_V1 comment not found — ogg was not produced by wemcodec forward path".into()
        ))?;

    log::debug!("roundtrip meta: {meta:?}");

    // The original stripped setup packet bytes are stored verbatim in the
    // roundtrip comment (2-byte size header + data).  Use them directly.
    let setup_packet = &meta.setup_packet;
    assert!(setup_packet.len() >= 2, "setup_packet too short");
    let setup_sz = u16::from_le_bytes(setup_packet[..2].try_into().unwrap()) as usize;
    assert_eq!(setup_sz + 2, setup_packet.len(), "setup_packet size field mismatch");
    let _stripped_setup = setup_packet[2..].to_vec();

    // Extract mode table from the EXPANDED OGG setup packet (packet index 2).
    // Using the expanded standard Vorbis format is more reliable than parsing
    // the stripped form.
    let modes = extract_modes_from_expanded_setup(&packets[2], meta.channels as usize)?;

    // Strip audio packets.
    let audio_packets: Vec<Vec<u8>> = packets[3..]
        .iter()
        .enumerate()
        .map(|(i, pkt)| {
            strip_audio_packet(pkt, &modes)
                .map_err(|e| { log::warn!("audio packet {i}: {e}"); e })
        })
        .collect::<Result<_>>()?;

    // Build data chunk content: [preamble][setup_packet][audio_packets...]
    let pkt_fmt = match meta.packet_header {
        2 => PacketHeaderFormat::TwoByte,
        6 => PacketHeaderFormat::SixByte,
        8 => PacketHeaderFormat::EightByte,
        n => return Err(WmmoggError::UnsupportedVariant(format!("packet_header={n}"))),
    };

    let mut data_content = Vec::new();
    data_content.extend_from_slice(&meta.preamble);
    data_content.extend_from_slice(&meta.setup_packet);

    match pkt_fmt {
        PacketHeaderFormat::TwoByte => {
            data_content.extend_from_slice(&build_data_chunk_2byte(&audio_packets));
        }
        PacketHeaderFormat::SixByte => {
            let granules = extract_granule_positions(ogg_bytes, audio_packets.len())?;
            let pairs: Vec<(u32, Vec<u8>)> = granules.iter().copied()
                .zip(audio_packets).collect();
            data_content.extend_from_slice(&build_data_chunk_6byte(&pairs));
        }
        PacketHeaderFormat::EightByte => {
            return Err(WmmoggError::UnsupportedVariant(
                "8-byte packet header reverse not yet implemented".into()
            ));
        }
    };

    // Reconstruct the full WEM file:
    //   wem_header (stored verbatim, includes all chunks + data chunk id/size)
    //   + data_content (rebuilt audio data)
    // The wem_header ends with the data chunk header (id + old size).
    // We patch the size field to the new data_content length.
    let mut out = meta.wem_header.clone();
    assert!(out.len() >= 8, "wem_header too short");

    // Patch data chunk size (last 4 bytes of the header = the data chunk size field).
    let new_data_sz = data_content.len() as u32;
    let hlen = out.len();
    out[hlen-4..hlen].copy_from_slice(&new_data_sz.to_le_bytes());

    // Patch RIFF file size (bytes 4-7 = total - 8).
    let riff_sz = (out.len() + data_content.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff_sz.to_le_bytes());

    out.extend_from_slice(&data_content);
    Ok(out)
}

// ---------------------------------------------------------------------------
// OGG logical packet extraction
// ---------------------------------------------------------------------------

pub(crate) fn extract_vorbis_packets(ogg_bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut pos = 0usize;
    let mut current_packet: Vec<u8> = Vec::new();

    while pos + 27 <= ogg_bytes.len() {
        if &ogg_bytes[pos..pos+4] != b"OggS" {
            return Err(WmmoggError::OggParse(format!("bad OggS magic at {pos}")));
        }
        let n_segs = ogg_bytes[pos + 26] as usize;
        if pos + 27 + n_segs > ogg_bytes.len() {
            return Err(WmmoggError::OggParse("segment table truncated".into()));
        }
        let seg_table = &ogg_bytes[pos+27..pos+27+n_segs];
        let mut payload_pos = pos + 27 + n_segs;

        for &seg_size in seg_table {
            let seg_end = payload_pos + seg_size as usize;
            if seg_end > ogg_bytes.len() {
                return Err(WmmoggError::OggParse("segment payload truncated".into()));
            }
            current_packet.extend_from_slice(&ogg_bytes[payload_pos..seg_end]);
            payload_pos = seg_end;
            // A segment < 255 bytes signals end-of-packet.
            if seg_size < 255 {
                if !current_packet.is_empty() {
                    packets.push(std::mem::take(&mut current_packet));
                }
            }
        }
        pos = payload_pos;
    }

    // Any partial packet at end of stream.
    if !current_packet.is_empty() {
        packets.push(current_packet);
    }

    Ok(packets)
}

// ---------------------------------------------------------------------------
// Extract granule positions from OGG pages for audio packets.
//
// Each OGG page has a granule position.  A granule applies to the last
// packet that ends on that page.  We collect them in order.
// ---------------------------------------------------------------------------

fn extract_granule_positions(ogg_bytes: &[u8], audio_count: usize) -> Result<Vec<u32>> {
    let mut granules = Vec::new();
    let mut pos = 0usize;

    while pos + 27 <= ogg_bytes.len() {
        if &ogg_bytes[pos..pos+4] != b"OggS" { break; }
        let granule = u64::from_le_bytes(ogg_bytes[pos+6..pos+14].try_into().unwrap());
        let n_segs  = ogg_bytes[pos+26] as usize;
        let payload_len: usize = ogg_bytes[pos+27..pos+27+n_segs].iter().map(|&s| s as usize).sum();
        pos += 27 + n_segs + payload_len;

        // Skip the first 3 pages (id, comment, setup).
        if granules.len() < 3 {
            granules.push(0u32); // placeholder
            continue;
        }
        granules.push(granule as u32);
    }

    // Trim to audio packet count (granule per audio page, roughly).
    granules.truncate(audio_count + 3);
    let audio_granules: Vec<u32> = granules.into_iter().skip(3).collect();
    Ok(audio_granules)
}

// ---------------------------------------------------------------------------
// Extract mode table from the EXPANDED standard Vorbis setup packet.
// This is more reliable than parsing the stripped form.
// ---------------------------------------------------------------------------

pub(crate) fn extract_modes_from_expanded_setup(packet: &[u8], channels: usize) -> Result<Vec<Mode>> {
    use crate::reverse::bit_io::BitReader;
    use crate::reverse::setup_strip::read_full_codebook_bits_skip;

    if packet.len() < 7 || &packet[0..7] != b"\x05vorbis" {
        return Err(WmmoggError::VorbisParse("not a vorbis setup packet".into()));
    }
    let mut r = BitReader::new(&packet[7..]);

    // Skip codebooks.
    let cb_count = r.read_bits(8)? as usize + 1;
    for _ in 0..cb_count {
        read_full_codebook_bits_skip(&mut r)?;
    }

    // Skip time domain (count+1 entries, each 16 bits).
    let time_count = r.read_bits(6)? as usize + 1;
    for _ in 0..time_count { r.read_bits(16)?; }

    // Skip floors (type 1 always).
    let floor_count = r.read_bits(6)? as usize + 1;
    for _ in 0..floor_count {
        r.read_bits(16)?; // floor_type
        skip_expanded_floor_type1(&mut r)?;
    }

    // Skip residues.
    let residue_count = r.read_bits(6)? as usize + 1;
    for _ in 0..residue_count {
        r.read_bits(16)?; // residue_type
        skip_expanded_residue(&mut r)?;
    }

    // Skip mappings (type 0 always).
    let mapping_count = r.read_bits(6)? as usize + 1;
    for _ in 0..mapping_count {
        r.read_bits(16)?; // mapping_type
        skip_expanded_mapping(&mut r, channels)?;
    }

    // Read modes.
    let mode_count = r.read_bits(6)? as usize + 1;
    let mut modes = Vec::with_capacity(mode_count);
    for _ in 0..mode_count {
        let blockflag      = r.read_bit()?;
        let _window_type   = r.read_bits(16)?;
        let _transform_type = r.read_bits(16)?;
        let _mapping        = r.read_bits(8)?;
        modes.push(Mode { blockflag });
    }

    Ok(modes)
}

// Skip a full codebook (expanded standard Vorbis form) without hashing.
// Same as read_full_codebook_bits but discards output.

// Floor type 1 (expanded) skip — identical to stripped form bits.
fn skip_expanded_floor_type1(r: &mut crate::reverse::bit_io::BitReader) -> Result<()> {
    let partitions = r.read_bits(5)? as usize;
    let mut pc = vec![0usize; partitions];
    for i in 0..partitions { pc[i] = r.read_bits(4)? as usize; }
    let max_class = pc.iter().copied().max().unwrap_or(0);
    let mut class_dims = vec![0u32; max_class + 1];
    for c in 0..=max_class {
        let dim_m1 = r.read_bits(3)?;
        class_dims[c] = dim_m1 + 1;
        let subclasses = r.read_bits(2)? as u32;
        if subclasses != 0 { r.read_bits(8)?; }
        for _ in 0..(1u32 << subclasses) { r.read_bits(8)?; }
    }
    r.read_bits(2)?; // mult_m1
    let rangebits = r.read_bits(4)? as u8;
    for i in 0..partitions {
        for _ in 0..class_dims[pc[i]] { r.read_bits(rangebits)?; }
    }
    Ok(())
}

// Residue (expanded) skip — same as stripped form.
fn skip_expanded_residue(r: &mut crate::reverse::bit_io::BitReader) -> Result<()> {
    r.read_bits(24)?; r.read_bits(24)?; r.read_bits(24)?;
    let cls_m1 = r.read_bits(6)? as usize;
    r.read_bits(8)?;
    let classifications = cls_m1 + 1;
    let mut cascade = vec![[false; 8]; classifications];
    for c in 0..classifications {
        let high = r.read_bits(3)?;
        let flag = r.read_bit()?;
        let low = if flag { r.read_bits(5)? } else { 0 };
        let combined = high * 8 + if flag { low } else { 0 };
        for pass in 0..8 { cascade[c][pass] = (combined >> pass) & 1 != 0; }
    }
    for c in 0..classifications {
        for pass in 0..8 { if cascade[c][pass] { r.read_bits(8)?; } }
    }
    Ok(())
}

// Mapping type 0 (expanded) skip — includes the 2 reserved bits absent from stripped.
fn skip_expanded_mapping(r: &mut crate::reverse::bit_io::BitReader, channels: usize) -> Result<()> {
    let submaps_flag = r.read_bit()?;
    let submaps = if submaps_flag { r.read_bits(4)? as usize + 1 } else { 1 };
    let coupling_flag = r.read_bit()?;
    if coupling_flag {
        let ilog_ch = ilog_ch((channels - 1) as u32) as u8;
        let steps = r.read_bits(8)? as usize + 1;
        for _ in 0..steps {
            r.read_bits(ilog_ch)?; // magnitude index
            r.read_bits(ilog_ch)?; // angle index
        }
    }
    r.read_bits(2)?; // reserved (always 0 in expanded Vorbis)
    if submaps > 1 {
        for _ in 0..channels { r.read_bits(4)?; } // channel mux
    }
    for _ in 0..submaps {
        r.read_bits(8)?; // time_config
        r.read_bits(8)?; // floor_number
        r.read_bits(8)?; // residue_number
    }
    Ok(())
}

