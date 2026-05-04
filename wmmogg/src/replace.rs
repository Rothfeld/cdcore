/// Replace the audio content of a WEM file with audio from a standard Vorbis OGG.
///
/// The original WEM provides the structural template (fmt chunk, extra RIFF chunks).
/// The new OGG provides the audio. Channel count and sample rate must match.
///
/// The new OGG must be encoded with standard libvorbis codebooks (default packed
/// library) or aoTuV 6.03 codebooks. OGGs from arbitrary encoders with
/// non-standard codebooks will return WmmoggError::CodebookNotFound.

use crate::error::{Result, WmmoggError};
use crate::wem::{Wem, PacketHeaderFormat};
use crate::reverse::{
    extract_vorbis_packets, extract_modes_from_expanded_setup,
    strip_audio_packet, strip_setup_header,
    default_lut, aotuv_lut,
};

/// Replace the audio in `original_wem` with the audio from `new_ogg`.
///
/// `new_ogg` must be a standard Vorbis OGG (e.g. produced by ffmpeg -c:a libvorbis)
/// with the same channel count and sample rate as the original WEM.
pub fn replace_wem_audio(original_wem: &[u8], new_ogg: &[u8]) -> Result<Vec<u8>> {
    let orig = Wem::parse(original_wem)?;

    if orig.packet_fmt != PacketHeaderFormat::TwoByte {
        return Err(WmmoggError::UnsupportedVariant(
            "replace_wem_audio: only 2-byte packet header format supported".into()
        ));
    }

    // Parse the new OGG into logical Vorbis packets.
    let packets = extract_vorbis_packets(new_ogg)?;
    if packets.len() < 4 {
        return Err(WmmoggError::OggParse(
            "new OGG needs at least 4 packets (id, comment, setup, 1+ audio)".into()
        ));
    }

    // Parse the Vorbis ID header for codec parameters.
    let (channels, sample_rate, blocksize_0_exp, blocksize_1_exp) =
        parse_id_header(&packets[0])?;

    // Validate codec parameters match the original WEM.
    if channels != orig.fmt.channels {
        return Err(WmmoggError::UnsupportedVariant(format!(
            "channel count mismatch: original={} new_ogg={}",
            orig.fmt.channels, channels
        )));
    }
    if sample_rate != orig.fmt.sample_rate {
        return Err(WmmoggError::UnsupportedVariant(format!(
            "sample rate mismatch: original={} new_ogg={}",
            orig.fmt.sample_rate, sample_rate
        )));
    }

    // Strip the setup packet — try default packed codebooks, fall back to aoTuV.
    let setup_packet = &packets[2];
    let stripped_setup = strip_setup_header(setup_packet, channels as usize, default_lut()?)
        .or_else(|e| {
            if matches!(e, WmmoggError::CodebookNotFound) {
                log::debug!("default codebooks: codebook not found, trying aoTuV 6.03");
                strip_setup_header(setup_packet, channels as usize, aotuv_lut()?)
            } else {
                Err(e)
            }
        })?;

    // Extract mode table from the expanded setup for audio packet stripping.
    let modes = extract_modes_from_expanded_setup(setup_packet, channels as usize)?;

    // Strip audio packets.
    let audio_packets: Vec<Vec<u8>> = packets[3..]
        .iter()
        .enumerate()
        .map(|(i, pkt)| {
            strip_audio_packet(pkt, &modes)
                .map_err(|e| { log::warn!("audio packet {i}: {e}"); e })
        })
        .collect::<Result<_>>()?;

    // Compute sample count from the last granule position in the new OGG.
    let sample_count = last_granule_position(new_ogg)?;

    // Build data chunk: [2-byte size + stripped_setup][2-byte size + audio]...
    let setup_packet_offset: u32 = 0; // no preamble (seek table) for new files
    let mut data_content = Vec::new();
    data_content.extend_from_slice(&(stripped_setup.len() as u16).to_le_bytes());
    data_content.extend_from_slice(&stripped_setup);
    let first_audio_packet_offset = data_content.len() as u32;
    for pkt in &audio_packets {
        data_content.extend_from_slice(&(pkt.len() as u16).to_le_bytes());
        data_content.extend_from_slice(pkt);
    }

    // Build fmt chunk: copy original verbatim, patch the fields we know.
    let mut new_fmt = orig.fmt.raw.clone();
    patch_fmt_vorb(
        &mut new_fmt,
        sample_count,
        setup_packet_offset,
        first_audio_packet_offset,
        blocksize_0_exp,
        blocksize_1_exp,
    );

    // Assemble RIFF.
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&[0u8; 4]); // size — patched below
    out.extend_from_slice(b"WAVE");
    write_chunk(&mut out, b"fmt ", &new_fmt);
    for (id, data) in &orig.extra_chunks {
        write_chunk(&mut out, id, data);
    }
    write_chunk(&mut out, b"data", &data_content);
    let riff_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());

    Ok(out)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse the Vorbis ID header packet.
///
/// Returns (channels, sample_rate, blocksize_0_exp, blocksize_1_exp).
fn parse_id_header(packet: &[u8]) -> Result<(u16, u32, u8, u8)> {
    if packet.len() < 30 || &packet[0..7] != b"\x01vorbis" {
        return Err(WmmoggError::OggParse("not a vorbis ID header".into()));
    }
    let channels    = packet[11] as u16;
    let sample_rate = u32::from_le_bytes(packet[12..16].try_into().unwrap());
    // The byte at offset 28 packs both blocksize exponents (LSB-first):
    //   bits[0..4] = blocksize_0_exp, bits[4..8] = blocksize_1_exp
    let packed_bs   = packet[28];
    let blocksize_0_exp = packed_bs & 0x0F;
    let blocksize_1_exp = (packed_bs >> 4) & 0x0F;

    if channels == 0 {
        return Err(WmmoggError::OggParse("ID header: channels=0".into()));
    }
    if sample_rate == 0 {
        return Err(WmmoggError::OggParse("ID header: sample_rate=0".into()));
    }
    Ok((channels, sample_rate, blocksize_0_exp, blocksize_1_exp))
}

/// Find the last non-negative granule position in an OGG stream.
///
/// The final granule position equals the total sample count for the stream.
fn last_granule_position(ogg: &[u8]) -> Result<u32> {
    let mut last = 0u64;
    let mut pos = 0usize;
    while pos + 27 <= ogg.len() {
        if &ogg[pos..pos + 4] != b"OggS" { break; }
        let granule = u64::from_le_bytes(ogg[pos + 6..pos + 14].try_into().unwrap());
        let n_segs  = ogg[pos + 26] as usize;
        if pos + 27 + n_segs > ogg.len() { break; }
        let payload_len: usize = ogg[pos + 27..pos + 27 + n_segs].iter().map(|&s| s as usize).sum();
        pos += 27 + n_segs + payload_len;
        // granule 0xFFFFFFFFFFFFFFFF means "no packets end on this page"
        if granule != u64::MAX {
            last = granule;
        }
    }
    Ok(last as u32)
}

/// Patch the vorb fields inside a raw fmt chunk buffer (ext_size=0x30 layout).
///
/// Vorb data starts at offset 24 (18-byte WAVEFORMATEX base + 6-byte ext prefix).
/// Field offsets are relative to the start of vorb:
///   +0x00 sample_count u32
///   +0x10 setup_packet_offset u32
///   +0x14 first_audio_packet_offset u32
///   +0x28 blocksize_0_exp u8
///   +0x29 blocksize_1_exp u8
fn patch_fmt_vorb(
    fmt: &mut Vec<u8>,
    sample_count: u32,
    setup_packet_offset: u32,
    first_audio_packet_offset: u32,
    blocksize_0_exp: u8,
    blocksize_1_exp: u8,
) {
    const VORB: usize = 24; // 18 (WAVEFORMATEX) + 6 (ext prefix)
    assert!(fmt.len() >= VORB + 0x2A, "fmt chunk too small for 0x30 ext");
    fmt[VORB + 0x00..VORB + 0x04].copy_from_slice(&sample_count.to_le_bytes());
    fmt[VORB + 0x10..VORB + 0x14].copy_from_slice(&setup_packet_offset.to_le_bytes());
    fmt[VORB + 0x14..VORB + 0x18].copy_from_slice(&first_audio_packet_offset.to_le_bytes());
    fmt[VORB + 0x28] = blocksize_0_exp;
    fmt[VORB + 0x29] = blocksize_1_exp;
}

fn write_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() & 1 != 0 { out.push(0); }
}
