/// WEM (Wwise Encoded Media) RIFF container parser and builder.
use byteorder::{ReadBytesExt, LE};
use std::io::{Cursor, Read, Seek, SeekFrom};

use super::{Result, WemError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian { Little, Big }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketHeaderFormat {
    TwoByte,   // u16 packet_size
    SixByte,   // u32 granule + u16 packet_size
    EightByte, // u32 packet_size + u32 granule (oldest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CodebookKind { PackedDefault, PackedAoTuV603, Inline }

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FmtInfo {
    pub channels: u16,
    pub sample_rate: u32,
    pub sample_count: u32,
    pub blocksize_0_exp: u8,
    pub blocksize_1_exp: u8,
    pub setup_packet_offset: u32,
    pub first_audio_packet_offset: u32,
    pub has_loop: bool,
    pub loop_start: u32,
    pub loop_end: u32,
    pub ext_size: u16,
    /// Raw fmt chunk bytes preserved verbatim for reconstruction.
    pub raw: Vec<u8>,
}

pub type FourCC = [u8; 4];

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Wem {
    pub endian: Endian,
    pub fmt: FmtInfo,
    pub packet_fmt: PacketHeaderFormat,
    pub codebook_kind: CodebookKind,
    pub extra_chunks: Vec<(FourCC, Vec<u8>)>,
    pub data: Vec<u8>,
}

impl Wem {
    pub fn parse(input: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(input);

        let mut riff_id = [0u8; 4];
        c.read_exact(&mut riff_id).map_err(|_| WemError::WemParse("truncated riff header".into()))?;
        let endian = match &riff_id {
            b"RIFF" => Endian::Little,
            b"RIFX" => Endian::Big,
            other   => return Err(WemError::WemParse(format!("not RIFF/RIFX: {:?}", other))),
        };
        let _riff_size = read_u32(&mut c, endian)?;

        let mut wave_id = [0u8; 4];
        c.read_exact(&mut wave_id).map_err(|_| WemError::WemParse("truncated wave id".into()))?;
        if &wave_id != b"WAVE" {
            return Err(WemError::WemParse("missing WAVE id".into()));
        }

        let mut fmt_raw: Option<Vec<u8>> = None;
        let mut data_bytes: Option<Vec<u8>> = None;
        let mut extra_chunks: Vec<(FourCC, Vec<u8>)> = Vec::new();

        loop {
            let mut id = [0u8; 4];
            if c.read_exact(&mut id).is_err() { break; }
            let sz = read_u32(&mut c, endian)? as usize;
            let mut chunk = vec![0u8; sz];
            c.read_exact(&mut chunk)
                .map_err(|e| WemError::WemParse(format!("chunk {:?} truncated: {e}", id)))?;
            if sz & 1 != 0 { let _ = c.seek(SeekFrom::Current(1)); }
            match &id {
                b"fmt " => fmt_raw = Some(chunk),
                b"data" => data_bytes = Some(chunk),
                _       => extra_chunks.push((id, chunk)),
            }
        }

        let fmt_raw = fmt_raw.ok_or_else(|| WemError::WemParse("missing fmt chunk".into()))?;
        let data    = data_bytes.ok_or_else(|| WemError::WemParse("missing data chunk".into()))?;
        let fmt     = parse_fmt(&fmt_raw, endian)?;
        let (packet_fmt, codebook_kind) = detect_format(&fmt)?;
        Ok(Wem { endian, fmt, packet_fmt, codebook_kind, extra_chunks, data })
    }
}

/// Patch the vorb fields in a raw fmt chunk buffer (ext_size=0x30 layout).
///
/// Vorb data starts at offset 24 (18-byte WAVEFORMATEX + 6-byte ext prefix).
pub fn patch_fmt_vorb(
    fmt: &mut Vec<u8>,
    sample_count: u32,
    setup_packet_offset: u32,
    first_audio_packet_offset: u32,
    blocksize_0_exp: u8,
    blocksize_1_exp: u8,
) {
    const VORB: usize = 24;
    assert!(fmt.len() >= VORB + 0x2A, "fmt chunk too small for 0x30 ext");
    fmt[VORB + 0x00..VORB + 0x04].copy_from_slice(&sample_count.to_le_bytes());
    fmt[VORB + 0x10..VORB + 0x14].copy_from_slice(&setup_packet_offset.to_le_bytes());
    fmt[VORB + 0x14..VORB + 0x18].copy_from_slice(&first_audio_packet_offset.to_le_bytes());
    fmt[VORB + 0x28] = blocksize_0_exp;
    fmt[VORB + 0x29] = blocksize_1_exp;
}

pub fn write_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() & 1 != 0 { out.push(0); }
}

fn parse_fmt(raw: &[u8], _endian: Endian) -> Result<FmtInfo> {
    if raw.len() < 18 {
        return Err(WemError::WemParse("fmt chunk too small".into()));
    }
    let mut c = Cursor::new(raw);
    let codec        = c.read_u16::<LE>().unwrap();
    let channels     = c.read_u16::<LE>().unwrap();
    let sample_rate  = c.read_u32::<LE>().unwrap();
    let _avg_bps     = c.read_u32::<LE>().unwrap();
    let _block_align = c.read_u16::<LE>().unwrap();
    let _bits        = c.read_u16::<LE>().unwrap();
    let ext_size     = c.read_u16::<LE>().unwrap();

    if codec != 0xFFFF && codec != 0xFFFE {
        return Err(WemError::WemParse(format!("unexpected codec 0x{codec:04x}")));
    }
    let ext_end = 18 + ext_size as usize;
    if raw.len() < ext_end {
        return Err(WemError::WemParse(format!(
            "fmt ext_size={ext_size} but chunk is {} bytes", raw.len()
        )));
    }
    let ext = &raw[18..ext_end];

    let (sample_count, blocksize_0_exp, blocksize_1_exp,
         setup_packet_offset, first_audio_packet_offset,
         has_loop, loop_start, loop_end) = match ext_size {
        0x30 => {
            // Vorb data at ext[6..48]. Vorb field offsets relative to ext[6]:
            //   +0x00 sample_count, +0x10 setup_packet_offset,
            //   +0x14 first_audio_packet_offset, +0x28 blocksize_0, +0x29 blocksize_1
            let v = &ext[6..];
            (
                u32::from_le_bytes(v[0x00..0x04].try_into().unwrap()),
                v[0x28], v[0x29],
                u32::from_le_bytes(v[0x10..0x14].try_into().unwrap()),
                u32::from_le_bytes(v[0x14..0x18].try_into().unwrap()),
                false, 0u32, 0u32,
            )
        }
        other => return Err(WemError::UnsupportedVariant(
            format!("fmt ext_size=0x{other:02x} (only 0x30 supported)")
        )),
    };

    Ok(FmtInfo {
        channels, sample_rate, sample_count,
        blocksize_0_exp, blocksize_1_exp,
        setup_packet_offset, first_audio_packet_offset,
        has_loop, loop_start, loop_end, ext_size,
        raw: raw.to_vec(),
    })
}

fn detect_format(fmt: &FmtInfo) -> Result<(PacketHeaderFormat, CodebookKind)> {
    assert_eq!(fmt.ext_size, 0x30, "only ext_size=0x30 supported");
    Ok((PacketHeaderFormat::TwoByte, CodebookKind::PackedDefault))
}

fn read_u32<R: Read>(r: &mut R, endian: Endian) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).map_err(|e| WemError::WemParse(format!("read u32: {e}")))?;
    Ok(match endian {
        Endian::Little => u32::from_le_bytes(buf),
        Endian::Big    => u32::from_be_bytes(buf),
    })
}
