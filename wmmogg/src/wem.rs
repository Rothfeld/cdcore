/// Parsed representation of a Wwise RIFF Vorbis (.wem) file.
///
/// Only the fields needed for roundtrip conversion are extracted.
/// Raw chunk bytes are preserved so the builder can reconstruct the file.
use byteorder::{ReadBytesExt, LE};
use std::io::{Cursor, Read, Seek, SeekFrom};

use crate::error::{Result, WmmoggError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little, // RIFF
    Big,    // RIFX
}

/// Packet header format used in the data chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketHeaderFormat {
    /// 2 bytes: u16 packet_size (no granule)
    TwoByte,
    /// 6 bytes: u32 granule + u16 packet_size  (RIFF=LE, RIFX=BE)
    SixByte,
    /// 8 bytes: u32 packet_size + u32 granule (oldest variant)
    EightByte,
}

/// Which packed codebook library the setup header references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CodebookKind {
    PackedDefault,
    PackedAoTuV603,
    Inline,
}

/// Fields extracted from the fmt chunk (WAVEFORMATEX + Wwise extension).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FmtInfo {
    pub channels: u16,
    pub sample_rate: u32,
    /// Total sample count stored in the Wwise extension.
    pub sample_count: u32,
    /// Exponent for small Vorbis block (2^n).
    pub blocksize_0_exp: u8,
    /// Exponent for large Vorbis block (2^n).
    pub blocksize_1_exp: u8,
    /// Offset from start of data chunk to the setup packet.
    pub setup_packet_offset: u32,
    /// Offset from start of data chunk to the first audio packet.
    pub first_audio_packet_offset: u32,
    /// Whether a loop is present.
    pub has_loop: bool,
    pub loop_start: u32,
    pub loop_end: u32,
    /// Size of the fmt extension block (0x28=40, 0x2c=44, 0x2e=46, 0x30=48, …).
    pub ext_size: u16,
    /// Raw fmt chunk bytes, preserved verbatim for WEM reconstruction.
    pub raw: Vec<u8>,
}

/// Top-level parsed WEM structure.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Wem {
    pub endian: Endian,
    pub fmt: FmtInfo,
    pub packet_fmt: PacketHeaderFormat,
    pub codebook_kind: CodebookKind,
    /// Raw bytes of every chunk after fmt (hash, smpl, cue, data, …), keyed by 4-byte id.
    /// Preserved for roundtrip rebuild.
    pub extra_chunks: Vec<(FourCC, Vec<u8>)>,
    /// The raw data chunk bytes (packet stream).
    pub data: Vec<u8>,
}

pub type FourCC = [u8; 4];

impl Wem {
    pub fn parse(input: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(input);

        // RIFF header
        let mut riff_id = [0u8; 4];
        c.read_exact(&mut riff_id).map_err(|_| WmmoggError::WemParse("truncated riff header".into()))?;
        let endian = match &riff_id {
            b"RIFF" => Endian::Little,
            b"RIFX" => Endian::Big,
            other => return Err(WmmoggError::WemParse(format!("not RIFF/RIFX: {:?}", other))),
        };

        let _riff_size = read_u32(&mut c, endian)?;

        let mut wave_id = [0u8; 4];
        c.read_exact(&mut wave_id).map_err(|_| WmmoggError::WemParse("truncated wave id".into()))?;
        if &wave_id != b"WAVE" {
            return Err(WmmoggError::WemParse("missing WAVE id".into()));
        }

        let mut fmt_raw: Option<Vec<u8>> = None;
        let mut data_bytes: Option<Vec<u8>> = None;
        let mut extra_chunks: Vec<(FourCC, Vec<u8>)> = Vec::new();

        loop {
            let mut id = [0u8; 4];
            if c.read_exact(&mut id).is_err() {
                break;
            }
            let sz = read_u32(&mut c, endian)? as usize;
            let mut chunk = vec![0u8; sz];
            c.read_exact(&mut chunk).map_err(|e| WmmoggError::WemParse(format!("chunk {:?} truncated: {e}", id)))?;
            // pad byte
            if sz & 1 != 0 {
                let _ = c.seek(SeekFrom::Current(1));
            }
            match &id {
                b"fmt " => fmt_raw = Some(chunk),
                b"data" => data_bytes = Some(chunk),
                _ => extra_chunks.push((id, chunk)),
            }
        }

        let fmt_raw = fmt_raw.ok_or_else(|| WmmoggError::WemParse("missing fmt chunk".into()))?;
        let data = data_bytes.ok_or_else(|| WmmoggError::WemParse("missing data chunk".into()))?;

        let fmt = parse_fmt(&fmt_raw, endian)?;
        let (packet_fmt, codebook_kind) = detect_packet_and_codebook_format(&fmt, &data, endian)?;

        Ok(Wem { endian, fmt, packet_fmt, codebook_kind, extra_chunks, data })
    }
}

fn parse_fmt(raw: &[u8], _endian: Endian) -> Result<FmtInfo> {
    if raw.len() < 18 {
        return Err(WmmoggError::WemParse("fmt chunk too small".into()));
    }
    let mut c = Cursor::new(raw);
    let codec   = c.read_u16::<LE>().unwrap();
    let channels = c.read_u16::<LE>().unwrap();
    let sample_rate = c.read_u32::<LE>().unwrap();
    let _avg_bps    = c.read_u32::<LE>().unwrap();
    let _block_align = c.read_u16::<LE>().unwrap();
    let _bits        = c.read_u16::<LE>().unwrap();
    let ext_size     = c.read_u16::<LE>().unwrap();

    if codec != 0xFFFF && codec != 0xFFFE {
        return Err(WmmoggError::WemParse(format!("unexpected codec 0x{codec:04x}, expected 0xFFFF")));
    }

    let ext_start = 18usize;
    let ext_end = ext_start + ext_size as usize;
    if raw.len() < ext_end {
        return Err(WmmoggError::WemParse(format!(
            "fmt ext_size={ext_size} but fmt chunk only {} bytes", raw.len()
        )));
    }
    let ext = &raw[ext_start..ext_end];

    // Wwise fmt extension layout — determined by ext_size:
    //
    // 0x28 (40): oldest — sample_count u32, unk u32, unk u32, blocksize_0+1 (1 byte each packed)
    // 0x2a (42): + has_loop flag at offset 40
    // 0x2c (44): + loop_start/end
    // 0x30 (48): current; adds setup/audio packet offsets; used by Crimson Desert
    //
    // All offsets below are relative to the start of the extension block.

    // For ext_size=0x30 (fmt_size=0x42), ww2ogg uses a "no vorb chunk" variant:
    // the first 6 bytes of ext are ext_unk(u16) + subtype(u32), then 42 bytes
    // of embedded vorb data (vorb_size=0x2A).
    //
    // Vorb field offsets (relative to vorb start = ext[6]):
    //   +0x00  sample_count u32
    //   +0x04  mod_signal u32   (determines mod_packets; not stored here)
    //   +0x10  setup_packet_offset u32
    //   +0x14  first_audio_packet_offset u32
    //   +0x24  uid u32
    //   +0x28  blocksize_0_pow u8
    //   +0x29  blocksize_1_pow u8
    //
    // All other ext sizes have a separate vorb chunk; field positions differ.
    let (sample_count, blocksize_0_exp, blocksize_1_exp,
         setup_packet_offset, first_audio_packet_offset,
         has_loop, loop_start, loop_end) = match ext_size {
        0x30 => {
            // Vorb data at ext[6..48] (42 bytes, vorb_size=0x2A).
            let v = &ext[6..]; // v[n] = vorb[n]
            let sc    = u32::from_le_bytes(v[0x00..0x04].try_into().unwrap());
            let spo   = u32::from_le_bytes(v[0x10..0x14].try_into().unwrap());
            let fapo  = u32::from_le_bytes(v[0x14..0x18].try_into().unwrap());
            let b0    = v[0x28];
            let b1    = v[0x29];
            // Loop points for this format live in a separate `smpl` chunk,
            // not in the fmt extension. Default to no-loop here.
            (sc, b0, b1, spo, fapo, false, 0u32, 0u32)
        }
        other => {
            return Err(WmmoggError::UnsupportedVariant(
                format!("fmt ext_size=0x{other:02x} not handled (only 0x30 supported)")
            ));
        }
    };

    Ok(FmtInfo {
        channels,
        sample_rate,
        sample_count,
        blocksize_0_exp,
        blocksize_1_exp,
        setup_packet_offset,
        first_audio_packet_offset,
        has_loop,
        loop_start,
        loop_end,
        ext_size,
        raw: raw.to_vec(),
    })
}

/// Determine packet header format and codebook kind.
///
/// Modern Wwise (ext_size=0x30) always uses 2-byte packet headers and packed
/// codebooks.  Older variants are detected from data-chunk heuristics.
fn detect_packet_and_codebook_format(
    fmt: &FmtInfo,
    _data: &[u8],
    _endian: Endian,
) -> Result<(PacketHeaderFormat, CodebookKind)> {
    // ext_size=0x30 (fmt_size=0x42, vorb_size=0x2A): always 2-byte packet headers,
    // no granule positions (ww2ogg: no_granule=true).
    assert_eq!(fmt.ext_size, 0x30, "only ext_size=0x30 is supported");
    let pkt_fmt = PacketHeaderFormat::TwoByte;

    // Codebook kind: Crimson Desert and most modern Wwise use packed_default.
    // We'll try default first during conversion and fall back to aoTuV.
    // For the metadata comment we record the actual library used.
    let cb_kind = CodebookKind::PackedDefault;

    Ok((pkt_fmt, cb_kind))
}

fn read_u32<R: Read>(r: &mut R, endian: Endian) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).map_err(|e| WmmoggError::WemParse(format!("read u32: {e}")))?;
    Ok(match endian {
        Endian::Little => u32::from_le_bytes(buf),
        Endian::Big    => u32::from_be_bytes(buf),
    })
}
