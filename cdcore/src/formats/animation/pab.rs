//! PAB skeleton parser (PAR v5.1).
//!
//! Header (verified 2026-04-29):
//!   [0x00] magic        "PAR "
//!   [0x04] version      u32
//!   [0x14] bone_count   u16 LE  (NOT u8 -- skeletons with > 255 bones silently
//!                                truncated to their low byte with the old read)
//!   [0x16] padding      1 byte
//!   [0x17] bone records start
//!
//! Per-bone record (305 + name_len bytes):
//!   [3B]   hash low24
//!   [1B]   name_length (u8, length-prefix -- NOT null-terminated)
//!   [N B]  name (no terminator)
//!   [4B]   parent_index (i32 LE; -1 = root)
//!   [64B]  bind_matrix        (4x4 f32)
//!   [64B]  inv_bind_matrix    (4x4 f32)
//!   [64B]  bind_matrix_copy
//!   [64B]  inv_bind_copy
//!   [12B]  scale              (3x f32)
//!   [16B]  rotation_quat      (4x f32, xyzw)
//!   [12B]  position           (3x f32)
//!   [1B]   alignment / record terminator
//!   => total = 4 + name_len + 4 + 256 + 40 + 1 = 305 + name_len

use crate::error::{read_u16_le, read_u32_le, read_i32_le, read_f32_le, Result, ParseError};

const PAR_MAGIC: &[u8] = b"PAR ";

#[derive(Debug, Clone)]
pub struct Bone {
    pub index: usize,
    pub name: String,
    pub bone_hash: u32,
    pub parent_index: i32,
    pub bind_matrix:     [[f32; 4]; 4],
    pub inv_bind_matrix: [[f32; 4]; 4],
    pub scale:    [f32; 3],
    pub rotation: [f32; 4],
    pub position: [f32; 3],
}

#[derive(Debug, Default, Clone)]
pub struct Skeleton {
    pub path: String,
    pub bones: Vec<Bone>,
}

pub fn parse(data: &[u8], filename: &str) -> Result<Skeleton> {
    if data.len() < 0x18 || &data[..4] != PAR_MAGIC {
        return Err(ParseError::magic(PAR_MAGIC, &data[..4.min(data.len())], 0));
    }

    let bone_count = read_u16_le(data, 0x14)? as usize;
    let mut off = 0x17usize; // skip u16 bone_count + 1 padding byte
    let mut bones = Vec::with_capacity(bone_count);

    for index in 0..bone_count {
        if off + 4 > data.len() { break; }

        // 3-byte hash low24 + 1-byte name_length (length-prefix, not null-terminated)
        let bone_hash = read_u32_le(data, off)?;
        let name_len = (bone_hash >> 24) as usize;
        let bone_hash = bone_hash & 0x00FF_FFFF;
        off += 4;

        if off + name_len > data.len() { break; }
        let name = std::str::from_utf8(&data[off..off + name_len])
            .unwrap_or("")
            .to_string();
        off += name_len;

        if off + 4 > data.len() { break; }
        let parent_index = read_i32_le(data, off)?;
        off += 4;

        // 4 matrices x 16 floats
        let bind_matrix     = read_mat4(data, off)?; off += 64;
        let inv_bind_matrix = read_mat4(data, off)?; off += 64;
        off += 128; // skip two cache copies

        if off + 41 > data.len() { break; }
        let scale    = [read_f32_le(data, off)?, read_f32_le(data, off+4)?, read_f32_le(data, off+8)?];
        off += 12;
        let rotation = [read_f32_le(data, off)?, read_f32_le(data, off+4)?, read_f32_le(data, off+8)?, read_f32_le(data, off+12)?];
        off += 16;
        let position = [read_f32_le(data, off)?, read_f32_le(data, off+4)?, read_f32_le(data, off+8)?];
        off += 12;
        off += 1; // alignment / record terminator

        bones.push(Bone {
            index, name, bone_hash, parent_index,
            bind_matrix, inv_bind_matrix, scale, rotation, position,
        });
    }

    Ok(Skeleton { path: filename.to_string(), bones })
}

fn read_mat4(data: &[u8], off: usize) -> Result<[[f32; 4]; 4]> {
    if off + 64 > data.len() {
        return Err(ParseError::eof(off, 64, data.len().saturating_sub(off)));
    }
    let mut mat = [[0.0f32; 4]; 4];
    for row in 0..4 {
        for col in 0..4 {
            mat[row][col] = read_f32_le(data, off + (row * 4 + col) * 4)?;
        }
    }
    Ok(mat)
}
