//! PAC skinned character mesh parser.
//!
//! Section 0: per-submesh descriptors (names, materials, bbox, vertex counts)
//! Sections 1+: geometry per-submesh (positions, UVs, bone indices, weights)
//!
//! Vertices include:
//!   uint16[3] position (quantized)
//!   marker u32 == 0x3C000000 at byte offset +12
//!   f16[2] UV at +8/+10
//!   bone indices + weights follow

use half::f16;
use crate::error::{read_u32_le, read_f32_le, Result, ParseError};
use super::pam::SubMesh;

const PAR_MAGIC: &[u8] = b"PAR ";

/// PAC vertex bone data.
#[derive(Debug, Default, Clone)]
pub struct BoneVertex {
    pub bone_indices: [u8; 4],
    pub bone_weights: [f32; 4],
}

/// Extended submesh with bone data.
#[derive(Debug, Default, Clone)]
pub struct PacSubMesh {
    pub base: SubMesh,
    pub bone_vertices: Vec<BoneVertex>,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedPac {
    pub path: String,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub submeshes: Vec<PacSubMesh>,
    pub total_vertices: usize,
    pub total_faces: usize,
    pub has_uvs: bool,
    pub has_bones: bool,
}

pub fn parse(data: &[u8], filename: &str) -> Result<ParsedPac> {
    if data.len() < 0x40 || &data[..4] != PAR_MAGIC {
        return Err(ParseError::magic(PAR_MAGIC, &data[..4.min(data.len())], 0));
    }

    // Read bounding box from standard PAM header offsets
    let bbox_min = [
        read_f32_le(data, 0x14)?,
        read_f32_le(data, 0x18)?,
        read_f32_le(data, 0x1C)?,
    ];
    let bbox_max = [
        read_f32_le(data, 0x20)?,
        read_f32_le(data, 0x24)?,
        read_f32_le(data, 0x28)?,
    ];
    let geom_off = read_u32_le(data, 0x3C)? as usize;
    let mesh_count = read_u32_le(data, 0x10)? as usize;

    let mut submeshes = Vec::new();

    // PAC vertex stride detection: look for the 0x3C000000 marker at +12
    let stride = detect_pac_stride(data, geom_off);

    // Parse submesh table (same layout as PAM)
    for i in 0..mesh_count {
        let off = 0x410 + i * 0x218;
        if off + 0x218 > data.len() { break; }

        let nv  = u32::from_le_bytes(data[off..off+4].try_into().unwrap()) as usize;
        let ni  = u32::from_le_bytes(data[off+4..off+8].try_into().unwrap()) as usize;
        let ve  = u32::from_le_bytes(data[off+8..off+12].try_into().unwrap()) as usize;
        let _ie  = u32::from_le_bytes(data[off+12..off+16].try_into().unwrap()) as usize;
        let tex = nul_str(&data[off + 0x10..], 256);
        let mat = nul_str(&data[off + 0x110..], 256);

        let vert_base = geom_off + ve * stride;
        let idx_off   = vert_base + nv * stride;

        if idx_off + ni * 2 > data.len() { continue; }

        let indices: Vec<usize> = (0..ni)
            .map(|j| u16::from_le_bytes(data[idx_off+j*2..idx_off+j*2+2].try_into().unwrap()) as usize)
            .collect();

        let mut unique: Vec<usize> = indices.iter().copied()
            .collect::<std::collections::HashSet<_>>().into_iter().collect();
        unique.sort_unstable();
        let idx_map: std::collections::HashMap<usize,usize> = unique.iter().enumerate().map(|(i,&g)|(g,i)).collect();

        let mut vertices  = Vec::with_capacity(unique.len());
        let mut uvs       = Vec::with_capacity(unique.len());
        let mut bone_verts = Vec::with_capacity(unique.len());

        for &gi in &unique {
            let foff = vert_base + gi * stride;
            if foff + 6 > data.len() { vertices.push([0.0f32; 3]); continue; }

            let xu = u16::from_le_bytes(data[foff..foff+2].try_into().unwrap());
            let yu = u16::from_le_bytes(data[foff+2..foff+4].try_into().unwrap());
            let zu = u16::from_le_bytes(data[foff+4..foff+6].try_into().unwrap());

            vertices.push([
                dequant(xu, bbox_min[0], bbox_max[0]),
                dequant(yu, bbox_min[1], bbox_max[1]),
                dequant(zu, bbox_min[2], bbox_max[2]),
            ]);

            if foff + 12 <= data.len() {
                let u = f16::from_le_bytes(data[foff+8..foff+10].try_into().unwrap()).to_f32();
                let v = f16::from_le_bytes(data[foff+10..foff+12].try_into().unwrap()).to_f32();
                uvs.push([u, v]);
            }

            // Bone indices and weights (if stride large enough — typically at +16)
            let bv = if stride >= 24 && foff + 24 <= data.len() {
                let bi = [data[foff+16], data[foff+17], data[foff+18], data[foff+19]];
                let bw = [
                    data[foff+20] as f32 / 255.0,
                    data[foff+21] as f32 / 255.0,
                    data[foff+22] as f32 / 255.0,
                    data[foff+23] as f32 / 255.0,
                ];
                BoneVertex { bone_indices: bi, bone_weights: bw }
            } else {
                BoneVertex::default()
            };
            bone_verts.push(bv);
        }

        let mut faces = Vec::with_capacity(ni / 3);
        let mut j = 0;
        while j + 2 < indices.len() {
            let (a, b, c) = (indices[j], indices[j+1], indices[j+2]);
            if let (Some(&la), Some(&lb), Some(&lc)) = (idx_map.get(&a), idx_map.get(&b), idx_map.get(&c)) {
                faces.push([la as u32, lb as u32, lc as u32]);
            }
            j += 3;
        }

        let normals = super::pam::compute_smooth_normals(&vertices, &faces);
        let _has_bones = bone_verts.iter().any(|bv| bv.bone_indices != [0,0,0,0]);

        submeshes.push(PacSubMesh {
            base: SubMesh {
                name: format!("mesh_{i:02}_{mat}"),
                material: mat,
                texture: tex,
                vertex_count: vertices.len(),
                face_count: faces.len(),
                vertices,
                uvs,
                normals,
                faces,
                ..Default::default()
            },
            bone_vertices: bone_verts,
        });
    }

    let total_vertices = submeshes.iter().map(|s| s.base.vertices.len()).sum();
    let total_faces    = submeshes.iter().map(|s| s.base.faces.len()).sum();
    let has_uvs   = submeshes.iter().any(|s| !s.base.uvs.is_empty());
    let has_bones = submeshes.iter().any(|s| s.bone_vertices.iter().any(|bv| bv.bone_indices != [0,0,0,0]));

    Ok(ParsedPac {
        path: filename.to_string(),
        bbox_min, bbox_max,
        submeshes,
        total_vertices, total_faces,
        has_uvs, has_bones,
    })
}

fn detect_pac_stride(data: &[u8], geom_off: usize) -> usize {
    const MARKER: u32 = 0x3C000000;
    let candidates = [40usize, 36, 32, 44, 48, 52, 56, 60, 64, 28, 24, 20, 16];
    let mut best = 40usize;
    let mut best_hits = 0i32;

    let region_size = data.len().saturating_sub(geom_off);
    for &stride in &candidates {
        let sample = (region_size / stride).min(64);
        if sample < 4 { continue; }
        let hits: i32 = (0..sample)
            .filter(|&i| {
                let off = geom_off + i * stride + 12;
                off + 4 <= data.len()
                    && u32::from_le_bytes(data[off..off+4].try_into().unwrap()) == MARKER
            })
            .count() as i32;
        if hits > best_hits {
            best_hits = hits;
            best = stride;
        }
    }
    best
}

fn dequant(v: u16, mn: f32, mx: f32) -> f32 {
    mn + (v as f32 / 65535.0) * (mx - mn)
}

fn nul_str(data: &[u8], max: usize) -> String {
    let end = max.min(data.len());
    let nul = data[..end].iter().position(|&b| b == 0).unwrap_or(end);
    String::from_utf8_lossy(&data[..nul]).into_owned()
}

