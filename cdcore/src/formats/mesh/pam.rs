//! PAM static mesh parser.
//!
//! Header offsets:
//!   0x10 mesh_count (u32)
//!   0x14 bbox_min   (3×f32)
//!   0x20 bbox_max   (3×f32)
//!   0x3C geom_off   (u32)
//!
//! Submesh table starts at 0x410, stride 0x218 per entry.
//!   +0x000 vertex_count (u32)
//!   +0x004 index_count  (u32)
//!   +0x008 vert_elem    (u32)  — vertex start in element units
//!   +0x00C idx_elem     (u32)  — index start in element units
//!   +0x010 texture      (256-byte null-padded ASCII string)
//!   +0x110 material     (256-byte null-padded ASCII string)

use half::f16;
use crate::error::{read_u32_le, read_f32_le, Result, ParseError};

const PAR_MAGIC: &[u8] = b"PAR ";
const HDR_MESH_COUNT: usize = 0x10;
const HDR_BBOX_MIN:   usize = 0x14;
const HDR_BBOX_MAX:   usize = 0x20;
const HDR_GEOM_OFF:   usize = 0x3C;
const SUBMESH_TABLE:  usize = 0x410;
const SUBMESH_STRIDE: usize = 0x218;
const SUBMESH_TEX_OFF: usize = 0x010;
const SUBMESH_MAT_OFF: usize = 0x110;
const STRIDE_CANDIDATES: &[usize] = &[
    6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 36, 40, 44, 48, 52, 56, 60, 64,
];

#[derive(Debug, Default, Clone)]
pub struct SubMesh {
    pub name: String,
    pub material: String,
    pub texture: String,
    pub vertices: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub normals: Vec<[f32; 3]>,
    pub faces: Vec<[u32; 3]>,
    pub vertex_count: usize,
    pub face_count: usize,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedMesh {
    pub path: String,
    pub format: String,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub submeshes: Vec<SubMesh>,
    pub total_vertices: usize,
    pub total_faces: usize,
    pub has_uvs: bool,
}

pub fn parse(data: &[u8], filename: &str) -> Result<ParsedMesh> {
    if data.len() < 0x40 || &data[..4] != PAR_MAGIC {
        return Err(ParseError::magic(PAR_MAGIC, &data[..4.min(data.len())], 0));
    }

    let bbox_min = read_bbox(data, HDR_BBOX_MIN)?;
    let bbox_max = read_bbox(data, HDR_BBOX_MAX)?;
    let geom_off = read_u32_le(data, HDR_GEOM_OFF)? as usize;
    let mesh_count = read_u32_le(data, HDR_MESH_COUNT)? as usize;

    // Read submesh descriptors
    let mut raw_entries: Vec<RawEntry> = Vec::new();
    for i in 0..mesh_count {
        let off = SUBMESH_TABLE + i * SUBMESH_STRIDE;
        if off + SUBMESH_STRIDE > data.len() { break; }
        raw_entries.push(read_raw_entry(data, off, i));
    }

    let mut result = ParsedMesh {
        path: filename.to_string(),
        format: "pam".to_string(),
        bbox_min,
        bbox_max,
        ..Default::default()
    };

    if raw_entries.is_empty() {
        return Ok(result);
    }

    // Detect combined-buffer layout
    let is_combined = detect_combined(&raw_entries);
    if is_combined {
        parse_combined(data, &raw_entries, geom_off, bbox_min, bbox_max, &mut result);
    } else {
        parse_independent(data, &raw_entries, geom_off, bbox_min, bbox_max, &mut result);
    }

    // Fallback: scan-based if the primary parse found no usable geometry.
    // Compute vertex sum now — result.total_vertices is not set yet.
    let primary_verts: usize = result.submeshes.iter().map(|s| s.vertices.len()).sum();
    if primary_verts == 0 {
        result.submeshes.clear();
        parse_scan_fallback(data, &raw_entries, geom_off, bbox_min, bbox_max, &mut result);
    }

    for sm in &mut result.submeshes {
        sm.normals = compute_smooth_normals(&sm.vertices, &sm.faces);
    }

    result.total_vertices = result.submeshes.iter().map(|s| s.vertices.len()).sum();
    result.total_faces    = result.submeshes.iter().map(|s| s.faces.len()).sum();
    result.has_uvs = result.submeshes.iter().any(|s| !s.uvs.is_empty());

    Ok(result)
}

// ── Internal helpers ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct RawEntry {
    index: usize,
    nv: usize,
    ni: usize,
    ve: usize,
    ie: usize,
    texture: String,
    material: String,
}

fn read_raw_entry(data: &[u8], off: usize, index: usize) -> RawEntry {
    let nv  = u32::from_le_bytes(data[off..off+4].try_into().unwrap()) as usize;
    let ni  = u32::from_le_bytes(data[off+4..off+8].try_into().unwrap()) as usize;
    let ve  = u32::from_le_bytes(data[off+8..off+12].try_into().unwrap()) as usize;
    let ie  = u32::from_le_bytes(data[off+12..off+16].try_into().unwrap()) as usize;
    let tex = cstr(&data[off + SUBMESH_TEX_OFF..], 256);
    let mat = cstr(&data[off + SUBMESH_MAT_OFF..], 256);
    RawEntry { index, nv, ni, ve, ie, texture: tex, material: mat }
}

fn cstr(data: &[u8], max: usize) -> String {
    let end = max.min(data.len());
    let nul = data[..end].iter().position(|&b| b == 0).unwrap_or(end);
    String::from_utf8_lossy(&data[..nul]).into_owned()
}

fn read_bbox(data: &[u8], off: usize) -> Result<[f32; 3]> {
    Ok([
        read_f32_le(data, off)?,
        read_f32_le(data, off + 4)?,
        read_f32_le(data, off + 8)?,
    ])
}

fn detect_combined(entries: &[RawEntry]) -> bool {
    if entries.len() <= 1 { return false; }
    let (mut ve_acc, mut ie_acc) = (0, 0);
    for e in entries {
        if e.ve != ve_acc || e.ie != ie_acc { return false; }
        ve_acc += e.nv;
        ie_acc += e.ni;
    }
    true
}

fn dequant(v: u16, mn: f32, mx: f32) -> f32 {
    mn + (v as f32 / 65535.0) * (mx - mn)
}

fn parse_combined(
    data: &[u8],
    entries: &[RawEntry],
    geom_off: usize,
    bmin: [f32; 3],
    bmax: [f32; 3],
    result: &mut ParsedMesh,
) {
    let total_v: usize = entries.iter().map(|e| e.nv).sum();
    let total_i: usize = entries.iter().map(|e| e.ni).sum();

    // Try each stride candidate
    for &stride in STRIDE_CANDIDATES {
        let idx_base = geom_off + total_v * stride;
        if idx_base + total_i * 2 > data.len() { continue; }

        // Validate first 50 indices
        let valid = (0..total_i.min(50)).all(|j| {
            let v = u16::from_le_bytes(data[idx_base + j*2..idx_base + j*2+2].try_into().unwrap()) as usize;
            v < total_v
        });
        if !valid { continue; }

        let has_uv = stride >= 12;
        for e in entries {
            let vert_base = geom_off + e.ve * stride;
            let idx_off   = idx_base  + e.ie * 2;
            if idx_off + e.ni * 2 > data.len() { continue; }

            let indices: Vec<usize> = (0..e.ni)
                .map(|j| u16::from_le_bytes(data[idx_off+j*2..idx_off+j*2+2].try_into().unwrap()) as usize)
                .collect();

            let mut unique: Vec<usize> = indices.iter().copied().collect::<std::collections::HashSet<_>>().into_iter().collect();
            unique.sort_unstable();
            let idx_map: std::collections::HashMap<usize, usize> = unique.iter().enumerate().map(|(i,&g)| (g,i)).collect();

            let (verts, uvs) = extract_verts(data, vert_base, stride, &unique, bmin, bmax, has_uv);
            let faces = extract_faces(&indices, &idx_map);

            result.submeshes.push(SubMesh {
                name: format!("mesh_{:02}_{}", e.index, &e.material),
                material: e.material.clone(),
                texture: e.texture.clone(),
                vertices: verts,
                uvs,
                faces,
                vertex_count: unique.len(),
                face_count: faces_count(&indices),
                ..Default::default()
            });
        }
        return;
    }
}

fn parse_independent(
    data: &[u8],
    entries: &[RawEntry],
    geom_off: usize,
    bmin: [f32; 3],
    bmax: [f32; 3],
    result: &mut ParsedMesh,
) {
    for e in entries {
        if let Some((stride, idx_off)) = find_local_stride(data, geom_off, e.ve, e.nv, e.ni) {
            let vert_base = geom_off + e.ve * stride;
            let has_uv = stride >= 12;
            let indices: Vec<usize> = (0..e.ni)
                .map(|j| u16::from_le_bytes(data[idx_off+j*2..idx_off+j*2+2].try_into().unwrap()) as usize)
                .collect();
            let mut unique: Vec<usize> = indices.iter().copied().collect::<std::collections::HashSet<_>>().into_iter().collect();
            unique.sort_unstable();
            let idx_map: std::collections::HashMap<usize,usize> = unique.iter().enumerate().map(|(i,&g)|(g,i)).collect();
            let (verts, uvs) = extract_verts(data, vert_base, stride, &unique, bmin, bmax, has_uv);
            let faces = extract_faces(&indices, &idx_map);
            result.submeshes.push(SubMesh {
                name: format!("mesh_{:02}_{}", e.index, &e.material),
                material: e.material.clone(),
                texture: e.texture.clone(),
                vertex_count: verts.len(),
                face_count: faces_count(&indices),
                vertices: verts,
                uvs,
                faces,
                ..Default::default()
            });
        }
    }
}

fn parse_scan_fallback(
    data: &[u8],
    entries: &[RawEntry],
    geom_off: usize,
    bmin: [f32; 3],
    bmax: [f32; 3],
    result: &mut ParsedMesh,
) {
    let total_v: usize = entries.iter().map(|e| e.nv).sum();
    let total_i: usize = entries.iter().map(|e| e.ni).sum();
    if total_v < 3 || total_i < 3 || geom_off + 60 > data.len() { return; }

    let search_limit = (data.len() - 100).min(geom_off + (data.len() / 2).min(2_000_000));
    let step = if search_limit - geom_off < 500_000 { 2 } else { 4 };

    let mut scan = geom_off;
    while scan < search_limit {
        scan += step;
        if scan + 60 > data.len() { break; }

        let vals: Vec<u16> = (0..30).map(|j| {
            u16::from_le_bytes(data[scan+j*2..scan+j*2+2].try_into().unwrap())
        }).collect();
        let spread = vals.iter().max().unwrap() - vals.iter().min().unwrap();
        if spread < 5000 { continue; }

        for &stride in &[6usize, 8, 10, 12, 14, 16, 20, 24, 28, 32] {
            let idx_off = scan + total_v * stride;
            if idx_off + total_i * 2 > data.len() { continue; }

            let valid = (0..total_i.min(50)).all(|j| {
                let v = u16::from_le_bytes(data[idx_off+j*2..idx_off+j*2+2].try_into().unwrap()) as usize;
                v < total_v
            });
            if !valid { continue; }

            let has_uv = stride >= 12;
            let idx_base = idx_off;

            for e in entries {
                let vert_base = scan + e.ve * stride;
                let eidx_off  = idx_base + e.ie * 2;
                if eidx_off + e.ni * 2 > data.len() { continue; }

                let indices: Vec<usize> = (0..e.ni)
                    .map(|j| u16::from_le_bytes(data[eidx_off+j*2..eidx_off+j*2+2].try_into().unwrap()) as usize)
                    .collect();
                let mut unique: Vec<usize> = indices.iter().copied()
                    .collect::<std::collections::HashSet<_>>().into_iter().collect();
                unique.sort_unstable();
                let idx_map: std::collections::HashMap<usize,usize> = unique.iter().enumerate().map(|(i,&g)|(g,i)).collect();
                let (verts, uvs) = extract_verts(data, vert_base, stride, &unique, bmin, bmax, has_uv);
                let faces = extract_faces(&indices, &idx_map);

                result.submeshes.push(SubMesh {
                    name: format!("mesh_{:02}_{}", e.index, &e.material),
                    material: e.material.clone(),
                    texture: e.texture.clone(),
                    vertex_count: verts.len(),
                    face_count: faces_count(&indices),
                    vertices: verts,
                    uvs,
                    faces,
                    ..Default::default()
                });
            }
            return;
        }
    }
}

fn find_local_stride(data: &[u8], geom_off: usize, voff: usize, nv: usize, ni: usize) -> Option<(usize, usize)> {
    for &stride in STRIDE_CANDIDATES {
        let vert_start = geom_off + voff * stride;
        let idx_off    = vert_start + nv * stride;
        if idx_off + ni * 2 > data.len() { continue; }
        let valid = (0..ni).all(|j| {
            let v = u16::from_le_bytes(data[idx_off+j*2..idx_off+j*2+2].try_into().unwrap()) as usize;
            v < nv
        });
        if valid { return Some((stride, idx_off)); }
    }
    None
}

fn extract_verts(
    data: &[u8],
    vert_base: usize,
    stride: usize,
    unique: &[usize],
    bmin: [f32; 3],
    bmax: [f32; 3],
    has_uv: bool,
) -> (Vec<[f32; 3]>, Vec<[f32; 2]>) {
    let mut verts = Vec::with_capacity(unique.len());
    let mut uvs   = Vec::with_capacity(if has_uv { unique.len() } else { 0 });

    for &gi in unique {
        let foff = vert_base + gi * stride;
        if foff + 6 > data.len() { verts.push([0.0; 3]); continue; }
        let xu = u16::from_le_bytes(data[foff..foff+2].try_into().unwrap());
        let yu = u16::from_le_bytes(data[foff+2..foff+4].try_into().unwrap());
        let zu = u16::from_le_bytes(data[foff+4..foff+6].try_into().unwrap());
        verts.push([dequant(xu, bmin[0], bmax[0]), dequant(yu, bmin[1], bmax[1]), dequant(zu, bmin[2], bmax[2])]);
        if has_uv && foff + 12 <= data.len() {
            let u = f16::from_le_bytes(data[foff+8..foff+10].try_into().unwrap()).to_f32();
            let v = f16::from_le_bytes(data[foff+10..foff+12].try_into().unwrap()).to_f32();
            uvs.push([u, v]);
        }
    }
    (verts, uvs)
}

fn extract_faces(indices: &[usize], idx_map: &std::collections::HashMap<usize, usize>) -> Vec<[u32; 3]> {
    let mut faces = Vec::with_capacity(indices.len() / 3);
    let mut j = 0;
    while j + 2 < indices.len() {
        let (a, b, c) = (indices[j], indices[j+1], indices[j+2]);
        if let (Some(&la), Some(&lb), Some(&lc)) = (idx_map.get(&a), idx_map.get(&b), idx_map.get(&c)) {
            faces.push([la as u32, lb as u32, lc as u32]);
        }
        j += 3;
    }
    faces
}

fn faces_count(indices: &[usize]) -> usize { indices.len() / 3 }

pub fn compute_smooth_normals(vertices: &[[f32; 3]], faces: &[[u32; 3]]) -> Vec<[f32; 3]> {
    let n = vertices.len();
    if n == 0 { return vec![]; }
    let mut normals = vec![[0.0f32; 3]; n];

    for face in faces {
        let (a, b, c) = (face[0] as usize, face[1] as usize, face[2] as usize);
        if a >= n || b >= n || c >= n { continue; }
        let fn_ = face_normal(vertices[a], vertices[b], vertices[c]);
        for &idx in &[a, b, c] {
            normals[idx][0] += fn_[0];
            normals[idx][1] += fn_[1];
            normals[idx][2] += fn_[2];
        }
    }

    normals.iter_mut().for_each(|n| {
        let len = (n[0]*n[0] + n[1]*n[1] + n[2]*n[2]).sqrt();
        if len > 1e-8 { n[0] /= len; n[1] /= len; n[2] /= len; }
        else { *n = [0.0, 1.0, 0.0]; }
    });
    normals
}

fn face_normal(v0: [f32; 3], v1: [f32; 3], v2: [f32; 3]) -> [f32; 3] {
    let ax = v1[0]-v0[0]; let ay = v1[1]-v0[1]; let az = v1[2]-v0[2];
    let bx = v2[0]-v0[0]; let by = v2[1]-v0[1]; let bz = v2[2]-v0[2];
    let nx = ay*bz - az*by;
    let ny = az*bx - ax*bz;
    let nz = ax*by - ay*bx;
    let len = (nx*nx + ny*ny + nz*nz).sqrt();
    if len > 1e-8 { [nx/len, ny/len, nz/len] } else { [0.0, 1.0, 0.0] }
}
