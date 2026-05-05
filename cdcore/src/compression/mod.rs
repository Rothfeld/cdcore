pub mod lz4;
pub mod zlib;

use log::warn;

use crate::error::{ParseError, Result};

pub const COMP_NONE: u8 = 0;
pub const COMP_TYPE1: u8 = 1;
pub const COMP_LZ4: u8 = 2;
pub const COMP_CUSTOM: u8 = 3;
pub const COMP_ZLIB: u8 = 4;

/// Decompress data according to the compression type from PAMT flags.
pub fn decompress(data: &[u8], orig_size: usize, compression_type: u8) -> Result<Vec<u8>> {
    match compression_type {
        COMP_NONE => Ok(data.to_vec()),
        COMP_TYPE1 => decompress_type1(data, orig_size),
        COMP_LZ4 => lz4::decompress(data, orig_size),
        COMP_CUSTOM => Err(ParseError::Compression(
            "compression type 3 (custom) not implemented".into(),
        )),
        COMP_ZLIB => zlib::decompress(data),
        t => Err(ParseError::Compression(format!("unknown compression type {t}"))),
    }
}

/// Compress data with LZ4 (the standard repacking compression type).
pub fn compress_lz4(data: &[u8]) -> Vec<u8> {
    lz4::compress(data)
}

/// Type-1 dispatcher.  Tries each strategy in order.  Each strategy collects
/// its failure reason so callers see exactly which decoder hit which obstacle.
///
/// Recognised type-1 containers carry a "DDS " or "PAR " magic at offset 0.
/// Files with no recognised magic and no size hint are treated as "no external
/// compression" -- the comp_type=1 flag is set in PAMT for many PAM/PAMLOD
/// mesh files that store their geometry raw and decompress internal LZ4
/// chunks inside the format parser.  We log a warn so the case is visible
/// and return the raw bytes; the parser does the real work.
///
/// Earlier versions silently returned raw bytes on ALL strategy failures,
/// which masked real DDS decompression bugs (e.g. ui/cd_worldmap_*_sdf_*.dds
/// tiles rendered as half-black L8 images because the per-mip-sizes strategy
/// was incorrectly gated behind a parse_dds_mip_info that didn't recognise
/// DDPF_LUMINANCE).
fn decompress_type1(data: &[u8], orig_size: usize) -> Result<Vec<u8>> {
    // Some files are flagged compressed but stored raw -- accept when sizes match.
    if data.len() == orig_size {
        return Ok(data.to_vec());
    }

    let has_dds_magic = data.len() >= 4 && &data[..4] == b"DDS ";

    // DDS files with comp_type=1 must succeed via one of the four DDS-aware
    // strategies; failure is a real bug we want to surface, not paper over.
    if has_dds_magic {
        let mut reasons: Vec<String> = Vec::new();

        // Strategy 2: DDS header + single LZ4 body
        if orig_size > data.len() {
            match try_decompress_type1_prefixed_lz4(data, orig_size) {
                Ok(r) if r.len() >= orig_size => return Ok(r[..orig_size].to_vec()),
                Ok(r) => reasons.push(format!("S2(LZ4 body): undersized output {} < {orig_size}", r.len())),
                Err(e) => reasons.push(format!("S2(LZ4 body): {e}")),
            }
        }

        // Strategy 3: per-mip on-disk sizes in DDS reserved area (offset 0x20)
        if orig_size > data.len() {
            match try_decompress_type1_dds_per_mip_sizes(data, orig_size) {
                Ok(r) if r.len() >= orig_size => return Ok(r[..orig_size].to_vec()),
                Ok(r) => reasons.push(format!("S3(per-mip sizes): undersized output {} < {orig_size}", r.len())),
                Err(e) => reasons.push(format!("S3(per-mip sizes): {e}")),
            }
        }

        // Strategy 4: LZ4 first mip + raw mip tail
        if orig_size > data.len() {
            match try_decompress_type1_dds_first_mip_lz4_tail(data, orig_size) {
                Ok(r) if r.len() >= orig_size => return Ok(r[..orig_size].to_vec()),
                Ok(r) => reasons.push(format!("S4(LZ4 head + raw tail): undersized output {} < {orig_size}", r.len())),
                Err(e) => reasons.push(format!("S4(LZ4 head + raw tail): {e}")),
            }
        }

        return Err(ParseError::Compression(format!(
            "type-1 DDS decompression failed for input {} -> {orig_size} bytes; tried: [{}]",
            data.len(),
            reasons.join("; "),
        )));
    }

    // PAR magic without DDS context is ambiguous: a DDS-PAR container expands
    // via Strategy 1 to a full DDS file; a mesh-PAR file (PAM, PAC) shares
    // the magic but has a different internal layout that the mesh parser
    // expands itself.  Try Strategy 1 -- if it produces something that ends
    // up DDS-shaped, accept it; otherwise treat the file as raw and let the
    // format parser take over.
    let has_par_magic = data.len() >= 4 && &data[..4] == b"PAR ";
    if has_par_magic {
        if let Ok(r) = try_decompress_type1_par(data) {
            if r.len() >= orig_size {
                return Ok(r[..orig_size].to_vec());
            }
        }
        // Fall through: PAR-magic file that isn't a DDS-PAR container.
    }

    // Non-DDS file with comp_type=1 (typically PAM/PAMLOD/PAC where the PAMT
    // flag is set but the file is stored raw at the PAZ layer; the format
    // parser handles internal LZ4 chunks).  Return the raw bytes and log so
    // the situation is visible -- we make no false claim about decompressing.
    warn!(
        "type-1: input magic {:?} not a DDS container (input {} bytes, PAMT orig_size {orig_size}); returning raw, format parser handles internal compression",
        &data[..4.min(data.len())], data.len()
    );
    Ok(data.to_vec())
}

// ---------------------------------------------------------------------------
// Strategy 1: PAR container with per-section LZ4
// ---------------------------------------------------------------------------

fn try_decompress_type1_par(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 0x50 {
        return Err(ParseError::Other("too short for PAR".into()));
    }
    let mut output = data[..0x50].to_vec();
    let mut file_offset = 0x50usize;
    let mut saw_compressed = false;

    for slot in 0..8usize {
        let slot_off = 0x10 + slot * 8;
        if slot_off + 8 > data.len() {
            break;
        }
        let comp_size   = u32::from_le_bytes(data[slot_off..slot_off+4].try_into().unwrap()) as usize;
        let decomp_size = u32::from_le_bytes(data[slot_off+4..slot_off+8].try_into().unwrap()) as usize;

        if decomp_size == 0 { continue; }

        if comp_size > 0 {
            saw_compressed = true;
            let blob = data.get(file_offset..file_offset + comp_size)
                .ok_or_else(|| ParseError::eof(file_offset, comp_size, data.len() - file_offset))?;
            let decompressed = lz4::decompress(blob, decomp_size)?;
            output.extend_from_slice(&decompressed);
            file_offset += comp_size;
        } else {
            let raw = data.get(file_offset..file_offset + decomp_size)
                .ok_or_else(|| ParseError::eof(file_offset, decomp_size, data.len() - file_offset))?;
            output.extend_from_slice(raw);
            file_offset += decomp_size;
        }
    }

    if !saw_compressed {
        return Err(ParseError::Other("no compressed sections in PAR".into()));
    }

    for slot in 0..8usize {
        let off = 0x10 + slot * 8;
        if off + 4 <= output.len() {
            output[off..off+4].copy_from_slice(&0u32.to_le_bytes());
        }
    }

    Ok(output)
}

// ---------------------------------------------------------------------------
// Strategy 2: DDS header uncompressed, rest is a single LZ4 block
// ---------------------------------------------------------------------------

fn try_decompress_type1_prefixed_lz4(data: &[u8], orig_size: usize) -> Result<Vec<u8>> {
    if data.len() < 128 || &data[..4] != b"DDS " {
        return Err(ParseError::Other("not a DDS header".into()));
    }
    let header = data[..128].to_vec();
    let payload = &data[128..];
    let expected = orig_size.saturating_sub(128);
    if expected == 0 {
        return Ok(data.to_vec());
    }
    let decompressed = lz4::decompress(payload, expected)?;
    let mut out = header;
    out.extend_from_slice(&decompressed);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Strategy 3: DDS with per-mip on-disk sizes in reserved area (offset 0x20)
//
// The DDS header's 11 reserved DWORDs at 0x20..0x4B encode each mip's
// on-disk (possibly LZ4-compressed) byte count.  A zero entry means "all
// remaining mips are stored raw and sequentially".
// ---------------------------------------------------------------------------

/// Minimal DDS format info needed to compute per-mip raw sizes.
struct DdsMipInfo {
    width:          u32,
    height:         u32,
    mip_count:      usize,
    data_offset:    usize,
    bytes_per_block: usize, // 0 = uncompressed
    bpp:            usize,  // bits per pixel (uncompressed only)
}

fn parse_dds_mip_info(data: &[u8]) -> Option<DdsMipInfo> {
    if data.len() < 128 || &data[..4] != b"DDS " { return None; }

    let height    = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let width     = u32::from_le_bytes(data[16..20].try_into().unwrap());
    let mip_count = u32::from_le_bytes(data[28..32].try_into().unwrap()).max(1) as usize;
    let pf_flags  = u32::from_le_bytes(data[80..84].try_into().unwrap());
    let fourcc    = &data[84..88];
    let bpp       = u32::from_le_bytes(data[88..92].try_into().unwrap()) as usize;

    const DDPF_FOURCC:    u32 = 0x4;
    const DDPF_RGB:       u32 = 0x40;
    const DDPF_LUMINANCE: u32 = 0x20000;
    const DDPF_ALPHA:     u32 = 0x2;

    let (data_offset, bytes_per_block, bpp_out) = if pf_flags & DDPF_FOURCC != 0 {
        let (off, bpb, bpp_dx) = match fourcc {
            b"DXT1" | b"BC4U" | b"BC4S"           => (128, 8,  0),
            b"DXT3" | b"DXT5" | b"BC5U" | b"BC5S" => (128, 16, 0),
            b"DX10" => {
                if data.len() < 148 { return None; }
                let dxgi = u32::from_le_bytes(data[128..132].try_into().unwrap());
                // (bytes_per_block, bits_per_pixel) for each DXGI format
                let (bpb, bpp_d) = match dxgi {
                    71 | 72                              => (8,  0),   // BC1
                    74|75|77|78|80|81|83|84|95|96|98|99 => (16, 0),   // BC2-BC7
                    28|29|30|31 | 87|88|89|90|91         => (0,  32),  // RGBA8 / BGRA8
                    24 | 25                              => (0,  32),  // R10G10B10A2
                    10                                   => (0,  64),  // R16G16B16A16F
                     2                                   => (0,  128), // R32G32B32A32F
                    54 | 55                              => (0,  16),  // R16F
                    41 | 43                              => (0,  32),  // R32F
                    61 | 62                              => (0,  8),   // R8
                    _ => return None,
                };
                (148, bpb, bpp_d)
            }
            _ => return None,
        };
        (off, bpb, bpp_dx)
    } else if pf_flags & DDPF_RGB != 0 {
        (128, 0, bpp)
    } else if pf_flags & (DDPF_LUMINANCE | DDPF_ALPHA) != 0 {
        // Single-channel grayscale or alpha-only.  bpp comes straight from the
        // header (typically 8 or 16).  Used by signed-distance-field worldmap
        // tiles in /cd (e.g. ui/cd_worldmap_*_sdf_*.dds).
        if bpp == 0 { return None; }
        (128, 0, bpp)
    } else {
        return None;
    };

    Some(DdsMipInfo { width, height, mip_count, data_offset, bytes_per_block, bpp: bpp_out })
}

fn raw_mip_size(info: &DdsMipInfo, level: usize) -> usize {
    let w = (info.width  >> level).max(1);
    let h = (info.height >> level).max(1);
    if info.bytes_per_block > 0 {
        let bx = ((w + 3) / 4).max(1);
        let by = ((h + 3) / 4).max(1);
        (bx * by) as usize * info.bytes_per_block
    } else {
        (w * h) as usize * info.bpp / 8
    }
}

fn expected_total_size(info: &DdsMipInfo) -> usize {
    info.data_offset + (0..info.mip_count).map(|l| raw_mip_size(info, l)).sum::<usize>()
}

fn try_decompress_type1_dds_per_mip_sizes(data: &[u8], orig_size: usize) -> Result<Vec<u8>> {
    let info = parse_dds_mip_info(data)
        .ok_or_else(|| ParseError::Other("not a decodable DDS".into()))?;

    if expected_total_size(&info) != orig_size {
        return Err(ParseError::Other("total size mismatch".into()));
    }

    let max_explicit = info.mip_count.min(11);
    let reserved: Vec<usize> = (0..max_explicit)
        .map(|i| u32::from_le_bytes(data[0x20 + i*4..0x24 + i*4].try_into().unwrap()) as usize)
        .collect();

    // Validate: every explicit non-zero entry must not exceed its raw mip size (+16 slack).
    for (i, &on_disk) in reserved.iter().enumerate() {
        if on_disk > 0 && on_disk > raw_mip_size(&info, i) + 16 {
            return Err(ParseError::Other("reserved entry exceeds raw mip size".into()));
        }
    }
    // Require at least one non-zero reserved entry.
    if reserved.iter().all(|&v| v == 0) {
        return Err(ParseError::Other("no per-mip sizes in reserved area".into()));
    }

    let body = &data[info.data_offset..];
    let mut out = data[..info.data_offset].to_vec();
    let mut pos = 0usize;

    let mut lvl = 0;
    while lvl < info.mip_count {
        let on_disk = if lvl < max_explicit { reserved[lvl] } else { 0 };

        if on_disk == 0 {
            // Trailing mips stored raw sequentially.
            for remaining in lvl..info.mip_count {
                let size = raw_mip_size(&info, remaining);
                let chunk = body.get(pos..pos + size)
                    .ok_or_else(|| ParseError::Other("truncated raw tail".into()))?;
                out.extend_from_slice(chunk);
                pos += size;
            }
            let _ = info.mip_count;
            break;
        }

        let chunk = body.get(pos..pos + on_disk)
            .ok_or_else(|| ParseError::Other("truncated mip chunk".into()))?;
        pos += on_disk;

        let expected_raw = raw_mip_size(&info, lvl);
        if on_disk == expected_raw {
            out.extend_from_slice(chunk);
        } else {
            let decoded = lz4::decompress(chunk, expected_raw)?;
            out.extend_from_slice(&decoded);
        }
        lvl += 1;
    }

    if pos != body.len() {
        return Err(ParseError::Other("leftover body bytes after per-mip decode".into()));
    }
    if out.len() != orig_size {
        return Err(ParseError::Other("output size mismatch after per-mip decode".into()));
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Strategy 4: DDS with LZ4-compressed first mip, raw mip tail
//
// Walks the raw LZ4 block stream manually to extract exactly first_mip_size
// decompressed bytes, then appends the remaining body bytes as the raw tail.
// ---------------------------------------------------------------------------

fn try_decompress_type1_dds_first_mip_lz4_tail(data: &[u8], orig_size: usize) -> Result<Vec<u8>> {
    let info = parse_dds_mip_info(data)
        .ok_or_else(|| ParseError::Other("not a decodable DDS".into()))?;

    if expected_total_size(&info) != orig_size {
        return Err(ParseError::Other("total size mismatch".into()));
    }

    let first_mip_size = raw_mip_size(&info, 0);
    let tail_size: usize = (0..info.mip_count).skip(1).map(|l| raw_mip_size(&info, l)).sum();

    let body = &data[info.data_offset..];
    let top_mip = decode_lz4_top_mip(body, first_mip_size, tail_size)
        .ok_or_else(|| ParseError::Other("first-mip LZ4 decode failed".into()))?;

    let mut out = data[..info.data_offset].to_vec();
    out.extend_from_slice(&top_mip);
    Ok(out)
}

fn decode_lz4_top_mip(body: &[u8], first_mip_size: usize, tail_size: usize) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(first_mip_size + tail_size);
    let mut i = 0usize;

    while i < body.len() {
        let token = body[i];
        i += 1;

        // Literal length
        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            loop {
                if i >= body.len() { return None; }
                let extra = body[i] as usize;
                i += 1;
                lit_len += extra;
                if extra != 255 { break; }
            }
        }

        if i + lit_len > body.len() { return None; }

        let remaining_top = first_mip_size.saturating_sub(out.len());
        if lit_len >= remaining_top {
            out.extend_from_slice(&body[i..i + remaining_top]);
            i += remaining_top;
            // The rest of body is the raw tail.
            if body.len() - i != tail_size { return None; }
            out.extend_from_slice(&body[i..]);
            return Some(out);
        }

        out.extend_from_slice(&body[i..i + lit_len]);
        i += lit_len;

        // Last sequence has no offset/match.
        if i + 2 > body.len() { break; }

        let offset = body[i] as usize | ((body[i + 1] as usize) << 8);
        if offset == 0 { return None; }
        i += 2;

        let mut match_len = (token & 0x0F) as usize;
        if match_len == 15 {
            loop {
                if i >= body.len() { return None; }
                let extra = body[i] as usize;
                i += 1;
                match_len += extra;
                if extra != 255 { break; }
            }
        }
        match_len += 4;

        let remaining_top = first_mip_size.saturating_sub(out.len());
        if match_len > remaining_top { return None; }

        let match_start = out.len().checked_sub(offset)?;
        for k in 0..match_len {
            let b = out[match_start + k];
            out.push(b);
        }
    }

    None
}
