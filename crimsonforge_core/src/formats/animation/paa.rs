//! PAA skeletal animation parser.
//!
//! Three variants:
//!   0x00 — gimmick/object poses, 1-2 keyframes
//!   0xC0 — full character animation, two sub-formats:
//!     a) sparse keyframe stream (i16 axis_x/y/z + f16 w + u16 frame_idx)
//!     b) link-variant with embedded per-bone tracks (4 x fp16 quat + u16 frame_idx)
//!
//! Header (22 bytes):
//!   0x00 magic   "PAR " (4B)
//!   0x04 version 0x02030001 (4B)
//!   0x08 sentinel 02..09 (8B)
//!   0x10 flags   (u32 LE) — upper byte is variant selector
//!   0x14 str_len (u16 LE) — metadata tag length
//!   0x16 metadata tags (UTF-8)
//!
//! Embedded-tracks keyframe record (10 bytes):
//!   bytes 0-1: fp16 LE quat X
//!   bytes 2-3: fp16 LE quat Y
//!   bytes 4-5: fp16 LE quat Z
//!   bytes 6-7: fp16 LE quat W
//!   bytes 8-9: u16 LE frame index
//!
//! Tracks are bone-major; a frame index drop signals a new bone boundary.
//! Quaternions are DELTA rotations (compose with bind), not absolute poses.

use half::f16;
use crate::error::{read_u16_le, read_u32_le, read_f32_le, Result, ParseError};

const PAR_MAGIC: &[u8] = b"PAR ";
const MAX_BONES: usize = 1024;
const MAX_FRAMES_PER_TRACK: u16 = 4096;

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
    /// True for the embedded-tracks variant where quaternions are
    /// absolute local rotations (not deltas added to bind pose).
    pub embedded_tracks_absolute: bool,
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
    let metadata_tags = std::str::from_utf8(
        data.get(tags_start..tags_start + str_len).unwrap_or(&[])
    ).unwrap_or("").to_string();

    let body_start = (tags_start + str_len + 3) & !3;

    let variant = if variant_byte == 0xC0 { AnimVariant::Character } else { AnimVariant::Gimmick };

    match variant {
        AnimVariant::Character => parse_c0_body(data, body_start, filename, metadata_tags),
        AnimVariant::Gimmick   => parse_gimmick_body(data, body_start, filename, metadata_tags),
    }
}

// ---------------------------------------------------------------------------
// 0xC0 body — tries embedded-tracks then falls back to sparse stream
// ---------------------------------------------------------------------------

fn parse_c0_body(
    data: &[u8],
    body_start: usize,
    filename: &str,
    metadata_tags: String,
) -> Result<ParsedAnimation> {
    if body_start + 80 > data.len() {
        return Ok(empty_anim(filename, AnimVariant::Character, metadata_tags));
    }

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

    // Look for a link-path string (starts with b'%') after the bind-pose block.
    // Scan forward up to 512 bytes.
    let link_start = find_link_path(data, off);

    if let Some((lstart, lend)) = link_start {
        // tracks begin after the path, 4-byte aligned
        let tracks_start = (lend + 3) & !3;
        if let Some(tracks) = decode_embedded_tracks(data, tracks_start) {
            let (frame_count, bone_count, keyframes) = densify_tracks(&tracks);
            return Ok(ParsedAnimation {
                path: filename.to_string(),
                variant: AnimVariant::Character,
                frame_count,
                bone_count,
                fps: 30.0,
                metadata_tags,
                keyframes,
                bind_poses,
                embedded_tracks_absolute: false,
            });
        }
        let _ = lstart; // suppress warning
    }

    // Fallback: sparse keyframe stream (i16 axis + f16 w encoding)
    parse_sparse_stream(data, off, filename, metadata_tags, bind_poses)
}

// ---------------------------------------------------------------------------
// Link path detection
// ---------------------------------------------------------------------------

fn find_link_path(data: &[u8], after: usize) -> Option<(usize, usize)> {
    let limit = (after + 512).min(data.len());
    for i in after..limit {
        if data[i] == b'%' {
            // Read until null, non-printable, or end
            let end = data[i..].iter().position(|&b| b == 0 || (b < 0x20 && b != 0))
                .map(|n| i + n)
                .unwrap_or(data.len());
            if end > i + 4 {
                return Some((i, end));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Embedded-tracks decoder (fp16 x/y/z/w + u16 frame_idx, bone-major)
// ---------------------------------------------------------------------------

#[inline]
fn fp16_le(data: &[u8], off: usize) -> f32 {
    f16::from_le_bytes([data[off], data[off + 1]]).to_f32()
}

fn looks_like_keyframe(data: &[u8], p: usize) -> Option<([f32; 4], u16)> {
    if p + 10 > data.len() { return None; }
    let qx = fp16_le(data, p);
    let qy = fp16_le(data, p + 2);
    let qz = fp16_le(data, p + 4);
    let qw = fp16_le(data, p + 6);
    let m2 = qx*qx + qy*qy + qz*qz + qw*qw;
    if !(0.95..=1.05).contains(&m2) { return None; }
    let frame = u16::from_le_bytes([data[p + 8], data[p + 9]]);
    if frame > MAX_FRAMES_PER_TRACK { return None; }
    Some(([qx, qy, qz, qw], frame))
}

fn decode_embedded_tracks(data: &[u8], tracks_start: usize)
    -> Option<Vec<Vec<([f32; 4], u16)>>>
{
    if tracks_start + 20 > data.len() { return None; }

    let mut tracks: Vec<Vec<([f32; 4], u16)>> = Vec::new();
    let mut p = tracks_start;

    while p + 20 <= data.len() && tracks.len() < MAX_BONES {
        // Two-record gate: both must be unit quats, frame2 > frame1
        let r1 = looks_like_keyframe(data, p)?;
        let r2 = looks_like_keyframe(data, p + 10)?;
        if r1.1 > 4 || r2.1 <= r1.1 || r2.1 > r1.1 + 8 {
            // Not a valid track start; advance one byte and retry
            // — but only if we haven't started any tracks yet
            if tracks.is_empty() {
                p += 1;
                continue;
            }
            break;
        }

        // Valid track — walk forward
        let mut kfs = vec![(r1.0, r1.1), (r2.0, r2.1)];
        let mut last_frame = r2.1;
        let mut q = p + 20;

        while q + 10 <= data.len() {
            if let Some((quat, frame)) = looks_like_keyframe(data, q) {
                if frame < last_frame { break; } // bone boundary
                kfs.push((quat, frame));
                last_frame = frame;
                q += 10;
            } else {
                break;
            }
        }

        tracks.push(kfs);
        p = q;
    }

    if tracks.is_empty() { None } else { Some(tracks) }
}

// ---------------------------------------------------------------------------
// Densify: sparse per-bone tracks -> dense per-frame keyframes
// ---------------------------------------------------------------------------

fn densify_tracks(tracks: &[Vec<([f32; 4], u16)>]) -> (u32, u32, Vec<Vec<Keyframe>>) {
    let bone_count = tracks.len() as u32;
    let max_frame = tracks.iter()
        .filter_map(|t| t.last().map(|kf| kf.1 as u32))
        .max()
        .unwrap_or(0);
    let total_frames = max_frame + 1;

    let mut dense: Vec<Vec<Keyframe>> = Vec::with_capacity(total_frames as usize);
    for _ in 0..total_frames {
        dense.push(vec![Keyframe { rotation: [0.0, 0.0, 0.0, 1.0], translation: [0.0; 3], scale: [1.0; 3] }; bone_count as usize]);
    }

    for (bi, track) in tracks.iter().enumerate() {
        if track.is_empty() { continue; }
        let mut ci = 0usize;
        let mut cur = track[0].0;
        for f in 0..total_frames as usize {
            while ci + 1 < track.len() && track[ci + 1].1 as usize <= f {
                ci += 1;
            }
            cur = track[ci].0;
            let rot = normalize_quat(cur);
            dense[f][bi].rotation = rot;
        }
    }

    (total_frames, bone_count, dense)
}

#[inline]
fn normalize_quat(q: [f32; 4]) -> [f32; 4] {
    let len = (q[0]*q[0] + q[1]*q[1] + q[2]*q[2] + q[3]*q[3]).sqrt();
    if len > 1e-6 { [q[0]/len, q[1]/len, q[2]/len, q[3]/len] }
    else { [0.0, 0.0, 0.0, 1.0] }
}

// ---------------------------------------------------------------------------
// Sparse stream fallback (i16 axis + f16 w + u16 frame_idx)
// ---------------------------------------------------------------------------

fn parse_sparse_stream(
    data: &[u8],
    after: usize,
    filename: &str,
    metadata_tags: String,
    bind_poses: Vec<Keyframe>,
) -> Result<ParsedAnimation> {
    let kf_start = find_keyframe_stream(data, after);
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
            embedded_tracks_absolute: false,
        });
    }
    let kf_start = kf_start.unwrap();

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
        let rot = normalize_quat([rx, ry, rz, rw]);

        sparse.push((frame_idx, Keyframe { rotation: rot, translation: [0.0; 3], scale: [1.0; 3] }));
    }

    if sparse.is_empty() {
        return Ok(empty_anim(filename, AnimVariant::Character, metadata_tags));
    }

    let frame_count = sparse.iter().map(|(f, _)| *f as u32).max().unwrap_or(0) + 1;
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
        embedded_tracks_absolute: false,
    })
}

fn find_keyframe_stream(data: &[u8], after: usize) -> Option<usize> {
    let limit = data.len().saturating_sub(50);
    let mut pos = after;
    while pos < limit {
        let aligned = (pos + 1) & !1;
        if aligned + 50 > data.len() { break; }
        let mut ok = true;
        for i in 0..5 {
            let off = aligned + i * 10;
            if off + 10 > data.len() { ok = false; break; }
            let f_cur = u16::from_le_bytes(data[off+8..off+10].try_into().unwrap());
            if i > 0 {
                let f_prev = u16::from_le_bytes(data[aligned+(i-1)*10+8..aligned+(i-1)*10+10].try_into().unwrap());
                if f_cur != f_prev + 1 { ok = false; break; }
            }
        }
        if ok { return Some(aligned); }
        pos += 2;
    }
    None
}

// ---------------------------------------------------------------------------
// Gimmick variant (0x00)
// ---------------------------------------------------------------------------

fn parse_gimmick_body(
    data: &[u8],
    body_start: usize,
    filename: &str,
    metadata_tags: String,
) -> Result<ParsedAnimation> {
    let mut keyframes = Vec::new();
    let mut pos = body_start;
    while pos + 10 <= data.len() {
        let ax = i16::from_le_bytes(data[pos..pos+2].try_into().unwrap());
        let ay = i16::from_le_bytes(data[pos+2..pos+4].try_into().unwrap());
        let az = i16::from_le_bytes(data[pos+4..pos+6].try_into().unwrap());
        let aw = i16::from_le_bytes(data[pos+6..pos+8].try_into().unwrap());
        pos += 8;
        let rot = normalize_quat([ax as f32 / 32767.0, ay as f32 / 32767.0, az as f32 / 32767.0, aw as f32 / 32767.0]);
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
        embedded_tracks_absolute: false,
    })
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
        embedded_tracks_absolute: false,
    }
}
