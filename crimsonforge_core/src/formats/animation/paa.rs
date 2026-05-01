//! PAA skeletal animation parser.
//!
//! Two variants, selected by (flags >> 24) & 0xFF:
//!   0x00 — gimmick/object poses, 1-2 keyframes
//!   0xC0 — full character animation with sparse keyframe tracks
//!
//! Header (22 bytes):
//!   0x00 magic   "PAR " (4B)
//!   0x04 version 0x02030001 (4B)
//!   0x08 sentinel 02..09 (8B)
//!   0x10 flags   (u32 LE) — upper byte is variant selector
//!   0x14 str_len (u16 LE) — metadata tag length
//!   0x16 metadata tags (UTF-8)
//!
//! Body (0xC0 variant):
//!   Bind-pose block: 2 × SRT records (10×f32 each = 80 bytes)
//!   Sparse keyframe stream: 10-byte records aligned to 2 bytes
//!     i16 axis_x, axis_y, axis_z   (scaled /32768)
//!     f16 w                         (quaternion real)
//!     u16 frame_idx

use half::f16;
use crate::error::{read_u16_le, read_u32_le, read_f32_le, Result, ParseError};

const PAR_MAGIC: &[u8] = b"PAR ";

#[derive(Debug, Clone, Default)]
pub struct Keyframe {
    pub rotation:    [f32; 4],
    pub translation: [f32; 3],
    pub scale:       [f32; 3],
}

#[derive(Debug, Clone)]
pub struct ParsedAnimation {
    pub path: String,
    pub variant: AnimVariant,
    pub frame_count: u32,
    pub bone_count: u32,
    pub fps: f32,
    pub metadata_tags: String,
    /// Dense keyframes[frame_idx][bone_idx] — gaps filled by repeat.
    pub keyframes: Vec<Vec<Keyframe>>,
    /// Bind pose SRT (if present).
    pub bind_poses: Vec<Keyframe>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AnimVariant {
    Gimmick,
    Character,
}

pub fn parse(data: &[u8], filename: &str) -> Result<ParsedAnimation> {
    if data.len() < 22 || &data[..4] != PAR_MAGIC {
        return Err(ParseError::magic(PAR_MAGIC, &data[..4.min(data.len())], 0));
    }

    let flags   = read_u32_le(data, 0x10)?;
    let str_len = read_u16_le(data, 0x14)? as usize;
    let variant_byte = (flags >> 24) as u8;

    let tags_start = 0x16usize;
    let metadata_tags = if str_len > 0 && tags_start + str_len <= data.len() {
        std::str::from_utf8(&data[tags_start..tags_start + str_len])
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    };

    let body_start = tags_start + str_len;
    // Align to 4 bytes
    let body_start = (body_start + 3) & !3;

    let variant = if variant_byte == 0xC0 { AnimVariant::Character } else { AnimVariant::Gimmick };

    match variant {
        AnimVariant::Character => parse_c0_body(data, body_start, filename, metadata_tags),
        AnimVariant::Gimmick   => parse_gimmick_body(data, body_start, filename, metadata_tags),
    }
}

fn parse_c0_body(
    data: &[u8],
    body_start: usize,
    filename: &str,
    metadata_tags: String,
) -> Result<ParsedAnimation> {
    if body_start + 80 > data.len() {
        return Ok(empty_anim(filename, AnimVariant::Character, metadata_tags));
    }

    // Bind-pose block: 2 × 40-byte SRT records
    let mut bind_poses = Vec::new();
    let mut off = body_start;
    for _ in 0..2 {
        if off + 40 > data.len() { break; }
        let scale       = [read_f32_le(data, off)?, read_f32_le(data, off+4)?, read_f32_le(data, off+8)?];
        let rotation    = [read_f32_le(data, off+12)?, read_f32_le(data, off+16)?, read_f32_le(data, off+20)?, read_f32_le(data, off+24)?];
        let translation = [read_f32_le(data, off+28)?, read_f32_le(data, off+32)?, read_f32_le(data, off+36)?];
        bind_poses.push(Keyframe { rotation, translation, scale });
        off += 40;
    }

    // Skip ~28-byte internal header, then locate sparse keyframes.
    // Locate the keyframe stream by scanning for 5 consecutive incrementing
    // frame_idx values (the frame_idx is at bytes 8-9 of each 10-byte record).
    let kf_start = find_keyframe_stream(data, off);
    if kf_start.is_none() {
        return Ok(ParsedAnimation {
            path: filename.to_string(),
            variant: AnimVariant::Character,
            frame_count: 0,
            bone_count: 0,
            fps: 30.0,
            metadata_tags,
            keyframes: vec![],
            bind_poses,
        });
    }
    let kf_start = kf_start.unwrap();

    // Parse sparse keyframes
    let mut sparse: Vec<(u16, Keyframe)> = Vec::new();
    let mut pos = kf_start;

    while pos + 10 <= data.len() {
        let ax = i16::from_le_bytes(data[pos..pos+2].try_into().unwrap());
        let ay = i16::from_le_bytes(data[pos+2..pos+4].try_into().unwrap());
        let az = i16::from_le_bytes(data[pos+4..pos+6].try_into().unwrap());
        let w_bits = u16::from_le_bytes(data[pos+6..pos+8].try_into().unwrap());
        let frame_idx = u16::from_le_bytes(data[pos+8..pos+10].try_into().unwrap());
        pos += 10;

        let rx = ax as f32 / 32768.0;
        let ry = ay as f32 / 32768.0;
        let rz = az as f32 / 32768.0;
        let rw = f16::from_bits(w_bits).to_f32();

        let len = (rx*rx + ry*ry + rz*rz + rw*rw).sqrt();
        let rot = if len > 1e-6 {
            [rx/len, ry/len, rz/len, rw/len]
        } else {
            [0.0, 0.0, 0.0, 1.0]
        };

        sparse.push((frame_idx, Keyframe {
            rotation: rot,
            translation: [0.0; 3],
            scale: [1.0; 3],
        }));
    }

    if sparse.is_empty() {
        return Ok(empty_anim(filename, AnimVariant::Character, metadata_tags));
    }

    let frame_count = sparse.iter().map(|(f, _)| *f as u32).max().unwrap_or(0) + 1;

    // Densify: fill gaps by repeating last keyframe
    let mut dense = Vec::with_capacity(frame_count as usize);
    let mut last = Keyframe { rotation: [0.0, 0.0, 0.0, 1.0], translation: [0.0; 3], scale: [1.0; 3] };
    let mut si = 0;
    for f in 0..frame_count {
        while si < sparse.len() && sparse[si].0 as u32 <= f {
            last = sparse[si].1.clone();
            si += 1;
        }
        dense.push(vec![last.clone()]);
    }

    Ok(ParsedAnimation {
        path: filename.to_string(),
        variant: AnimVariant::Character,
        frame_count,
        bone_count: 1,
        fps: 30.0,
        metadata_tags,
        keyframes: dense,
        bind_poses,
    })
}

fn parse_gimmick_body(
    data: &[u8],
    body_start: usize,
    filename: &str,
    metadata_tags: String,
) -> Result<ParsedAnimation> {
    // Gimmick animations: compact 1-2 keyframe format
    // Parse as best-effort int16 quaternion records
    let mut keyframes = Vec::new();
    let mut pos = body_start;

    while pos + 10 <= data.len() {
        let ax = i16::from_le_bytes(data[pos..pos+2].try_into().unwrap());
        let ay = i16::from_le_bytes(data[pos+2..pos+4].try_into().unwrap());
        let az = i16::from_le_bytes(data[pos+4..pos+6].try_into().unwrap());
        let aw = i16::from_le_bytes(data[pos+6..pos+8].try_into().unwrap());
        pos += 8;

        let rx = ax as f32 / 32767.0;
        let ry = ay as f32 / 32767.0;
        let rz = az as f32 / 32767.0;
        let rw = aw as f32 / 32767.0;
        let len = (rx*rx + ry*ry + rz*rz + rw*rw).sqrt();
        let rot = if len > 1e-6 { [rx/len, ry/len, rz/len, rw/len] } else { [0.0, 0.0, 0.0, 1.0] };

        keyframes.push(vec![Keyframe { rotation: rot, translation: [0.0; 3], scale: [1.0; 3] }]);
        if keyframes.len() >= 2 { break; }
    }

    let frame_count = keyframes.len() as u32;
    Ok(ParsedAnimation {
        path: filename.to_string(),
        variant: AnimVariant::Gimmick,
        frame_count,
        bone_count: 1,
        fps: 30.0,
        metadata_tags,
        keyframes,
        bind_poses: vec![],
    })
}

fn find_keyframe_stream(data: &[u8], after: usize) -> Option<usize> {
    // Scan for 5 consecutive incrementing frame_idx values
    let limit = data.len().saturating_sub(50);
    let mut pos = after;
    while pos < limit {
        // Check alignment to 2 bytes
        let aligned = (pos + 1) & !1;
        if aligned + 50 > data.len() { break; }

        let mut ok = true;
        for i in 0..5 {
            let off = aligned + i * 10;
            if off + 10 > data.len() { ok = false; break; }
            let f_cur = u16::from_le_bytes(data[off+8..off+10].try_into().unwrap());
            if i > 0 {
                let off_prev = aligned + (i-1) * 10;
                let f_prev = u16::from_le_bytes(data[off_prev+8..off_prev+10].try_into().unwrap());
                if f_cur != f_prev + 1 { ok = false; break; }
            }
        }
        if ok { return Some(aligned); }
        pos += 2;
    }
    None
}

fn empty_anim(filename: &str, variant: AnimVariant, tags: String) -> ParsedAnimation {
    ParsedAnimation {
        path: filename.to_string(),
        variant,
        frame_count: 0,
        bone_count: 0,
        fps: 30.0,
        metadata_tags: tags,
        keyframes: vec![],
        bind_poses: vec![],
    }
}
