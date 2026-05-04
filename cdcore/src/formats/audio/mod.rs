pub mod ogg;
pub mod wem;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WemError {
    #[error("wem parse error: {0}")]
    WemParse(String),

    #[error("ogg parse error: {0}")]
    OggParse(String),

    #[error("vorbis parse error: {0}")]
    VorbisParse(String),

    #[error("codebook not found in packed library (inline codebooks not yet supported)")]
    CodebookNotFound,

    #[error("unsupported wem variant: {0}")]
    UnsupportedVariant(String),

    #[error("forward conversion failed: {0}")]
    Forward(#[from] ww2ogg::WemError),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, WemError>;

use self::wem::{Wem, PacketHeaderFormat, patch_fmt_vorb, write_chunk};
use self::ogg::{
    build_roundtrip_comment, parse_roundtrip_comment, replace_comment_packet,
    extract_vorbis_packets, extract_granule_positions, extract_modes,
    last_granule_position, parse_id_header,
    strip_setup_header, strip_audio_packet,
    build_data_chunk_2byte, build_data_chunk_6byte,
    default_lut, aotuv_lut,
};
use ww2ogg::{CodebookLibrary, WwiseRiffVorbis};

// ---------------------------------------------------------------------------
// WEM -> OGG
// ---------------------------------------------------------------------------

pub fn wem_to_ogg(wem_bytes: &[u8]) -> Result<Vec<u8>> {
    let wem = Wem::parse(wem_bytes)?;
    let ogg = convert_with_auto_detect(wem_bytes)?;
    replace_comment_packet(ogg, &build_roundtrip_comment(&wem, wem_bytes))
}

fn convert_with_auto_detect(wem_bytes: &[u8]) -> Result<Vec<u8>> {
    match try_convert(wem_bytes, CodebookLibrary::default_codebooks()?) {
        Err(WemError::Forward(ww2ogg::WemError::SizeMismatch { .. })) => {
            log::debug!("default codebooks: size mismatch, trying aoTuV 6.03");
        }
        Err(e) => return Err(e),
        Ok(ogg) => {
            if ww2ogg::validate(&ogg).is_ok() { return Ok(ogg); }
            log::debug!("default codebooks: undecodable stream, trying aoTuV 6.03");
        }
    }
    let ogg = try_convert(wem_bytes, CodebookLibrary::aotuv_codebooks()?)?;
    if let Err(e) = ww2ogg::validate(&ogg) {
        log::warn!("aoTuV codebooks also produced undecodable stream: {e}");
    }
    Ok(ogg)
}

fn try_convert(wem_bytes: &[u8], codebooks: CodebookLibrary) -> Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(wem_bytes);
    let mut converter = WwiseRiffVorbis::new(cursor, codebooks)?;
    let mut out = Vec::new();
    converter.generate_ogg(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// OGG -> WEM (roundtrip -- requires WEM_ROUNDTRIP_V1 tag)
// ---------------------------------------------------------------------------

pub fn ogg_to_wem(ogg_bytes: &[u8]) -> Result<Vec<u8>> {
    let packets = extract_vorbis_packets(ogg_bytes)?;
    if packets.len() < 3 {
        return Err(WemError::OggParse("need at least 3 vorbis packets".into()));
    }
    let meta = parse_roundtrip_comment(&packets[1])
        .ok_or_else(|| WemError::OggParse(
            "WEM_ROUNDTRIP_V1 comment not found -- ogg was not produced by cdcore audio".into()
        ))?;
    log::debug!("roundtrip meta: {meta:?}");

    let setup_packet = &meta.setup_packet;
    assert!(setup_packet.len() >= 2);
    let setup_sz = u16::from_le_bytes(setup_packet[..2].try_into().unwrap()) as usize;
    assert_eq!(setup_sz + 2, setup_packet.len());

    let modes = extract_modes(&packets[2], meta.channels as usize)?;
    let audio_packets: Vec<Vec<u8>> = packets[3..].iter().enumerate()
        .map(|(i, pkt)| strip_audio_packet(pkt, &modes)
            .map_err(|e| { log::warn!("audio packet {i}: {e}"); e }))
        .collect::<Result<_>>()?;

    let pkt_fmt = match meta.packet_header {
        2 => PacketHeaderFormat::TwoByte,
        6 => PacketHeaderFormat::SixByte,
        8 => PacketHeaderFormat::EightByte,
        n => return Err(WemError::UnsupportedVariant(format!("packet_header={n}"))),
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
            let pairs: Vec<(u32, Vec<u8>)> = granules.into_iter().zip(audio_packets).collect();
            data_content.extend_from_slice(&build_data_chunk_6byte(&pairs));
        }
        PacketHeaderFormat::EightByte => {
            return Err(WemError::UnsupportedVariant("8-byte packet header not implemented".into()));
        }
    }

    let mut out = meta.wem_header.clone();
    assert!(out.len() >= 8);
    let hlen = out.len();
    out[hlen-4..hlen].copy_from_slice(&(data_content.len() as u32).to_le_bytes());
    let riff_sz = (out.len() + data_content.len() - 8) as u32; out[4..8].copy_from_slice(&riff_sz.to_le_bytes());
    out.extend_from_slice(&data_content);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Replace audio in an existing WEM with a new OGG recording
// ---------------------------------------------------------------------------

/// Replace the audio in `original_wem` with audio from `new_ogg`.
///
/// `new_ogg` must be standard Vorbis (e.g. ffmpeg -c:a libvorbis) with
/// the same channel count and sample rate as the original WEM.
///
/// Hard constraints: channel count and sample rate must match exactly.
/// Codebook requirement: standard libvorbis or aoTuV 6.03.
pub fn replace_wem_audio(original_wem: &[u8], new_ogg: &[u8]) -> Result<Vec<u8>> {
    let orig = Wem::parse(original_wem)?;
    if orig.packet_fmt != PacketHeaderFormat::TwoByte {
        return Err(WemError::UnsupportedVariant(
            "replace_wem_audio: only 2-byte packet header format supported".into()
        ));
    }

    let packets = extract_vorbis_packets(new_ogg)?;
    if packets.len() < 4 {
        return Err(WemError::OggParse("new OGG needs at least 4 packets".into()));
    }
    let (channels, sample_rate, bs0, bs1) = parse_id_header(&packets[0])?;
    if channels != orig.fmt.channels {
        return Err(WemError::UnsupportedVariant(format!(
            "channel count mismatch: original={} new_ogg={}", orig.fmt.channels, channels
        )));
    }
    if sample_rate != orig.fmt.sample_rate {
        return Err(WemError::UnsupportedVariant(format!(
            "sample rate mismatch: original={} new_ogg={}", orig.fmt.sample_rate, sample_rate
        )));
    }

    let setup_packet = &packets[2];
    let stripped_setup = strip_setup_header(setup_packet, channels as usize, default_lut()?)
        .or_else(|e| {
            if matches!(e, WemError::CodebookNotFound) {
                log::debug!("default codebooks: not found, trying aoTuV 6.03");
                strip_setup_header(setup_packet, channels as usize, aotuv_lut()?)
            } else { Err(e) }
        })?;

    let modes = extract_modes(setup_packet, channels as usize)?;
    let audio_packets: Vec<Vec<u8>> = packets[3..].iter().enumerate()
        .map(|(i, pkt)| strip_audio_packet(pkt, &modes)
            .map_err(|e| { log::warn!("audio packet {i}: {e}"); e }))
        .collect::<Result<_>>()?;

    let sample_count = last_granule_position(new_ogg)?;
    let mut data_content = Vec::new();
    data_content.extend_from_slice(&(stripped_setup.len() as u16).to_le_bytes());
    data_content.extend_from_slice(&stripped_setup);
    let first_audio_offset = data_content.len() as u32;
    data_content.extend_from_slice(&build_data_chunk_2byte(&audio_packets));

    let mut new_fmt = orig.fmt.raw.clone();
    patch_fmt_vorb(&mut new_fmt, sample_count, 0, first_audio_offset, bs0, bs1);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(b"WAVE");
    write_chunk(&mut out, b"fmt ", &new_fmt);
    for (id, data) in &orig.extra_chunks { write_chunk(&mut out, id, data); }
    write_chunk(&mut out, b"data", &data_content);
    let riff_sz = (out.len() - 8) as u32; out[4..8].copy_from_slice(&riff_sz.to_le_bytes());
    Ok(out)
}
