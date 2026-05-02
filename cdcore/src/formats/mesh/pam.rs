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
// XAR is an extended PAR variant found in a small number of .pam files.
// The layout is structurally identical but geom_off=0, so no geometry is
// extracted; we return an empty mesh rather than a hard error.
const XAR_MAGIC: &[u8] = b"XAR ";
const HDR_MESH_COUNT:       usize = 0x10;
const HDR_BBOX_MIN:         usize = 0x14;
const HDR_BBOX_MAX:         usize = 0x20;
const HDR_GEOM_OFF:         usize = 0x3C;
// When non-zero, field 0x44 is the compressed size of the geometry section
// and field 0x40 is the expected decompressed size.  The geometry must be
// LZ4-decompressed before parsing vertices and indices.
const HDR_GEOM_DECOMP_SIZE: usize = 0x40;
const HDR_GEOM_COMP_SIZE:   usize = 0x44;
const SUBMESH_TABLE:  usize = 0x410;
const SUBMESH_STRIDE: usize = 0x218;
const SUBMESH_TEX_OFF: usize = 0x010;
const SUBMESH_MAT_OFF: usize = 0x110;
pub(super) const STRIDE_CANDIDATES: &[usize] = &[
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
    if data.len() < 0x50 {
        return Err(ParseError::magic(PAR_MAGIC, &data[..4.min(data.len())], 0));
    }
    if &data[..4] == XAR_MAGIC {
        return Ok(ParsedMesh { path: filename.to_string(), format: "pam".to_string(), ..Default::default() });
    }
    if &data[..4] != PAR_MAGIC {
        return Err(ParseError::magic(PAR_MAGIC, &data[..4], 0));
    }

    let bbox_min = read_bbox(data, HDR_BBOX_MIN)?;
    let bbox_max = read_bbox(data, HDR_BBOX_MAX)?;
    let geom_off      = read_u32_le(data, HDR_GEOM_OFF)?       as usize;
    let geom_decomp   = read_u32_le(data, HDR_GEOM_DECOMP_SIZE)? as usize;
    let geom_comp     = read_u32_le(data, HDR_GEOM_COMP_SIZE)?  as usize;
    let mesh_count    = read_u32_le(data, HDR_MESH_COUNT)?      as usize;

    // If field 0x44 is non-zero the geometry section is LZ4-compressed.
    // Decompress it and splice into a temporary buffer so the rest of the
    // parser sees a flat layout regardless of whether compression was used.
    let owned: Vec<u8>;
    let data: &[u8] = if geom_comp != 0 {
        let comp_end = geom_off + geom_comp;
        if comp_end > data.len() {
            return Err(ParseError::eof(geom_off, geom_comp, data.len() - geom_off));
        }
        let decompressed = crate::compression::decompress(
            &data[geom_off..comp_end],
            geom_decomp,
            crate::compression::COMP_LZ4,
        ).map_err(|e| ParseError::Other(format!("{filename}: geometry decompress: {e}")))?;
        let mut buf = data[..geom_off].to_vec();
        buf.extend_from_slice(&decompressed);
        owned = buf;
        &owned
    } else {
        data
    };

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
        parse_combined(data, &raw_entries, geom_off, bbox_min, bbox_max, &mut result, geom_decomp);
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

pub(super) fn cstr(data: &[u8], max: usize) -> String {
    let end = max.min(data.len());
    let nul = data[..end].iter().position(|&b| b == 0).unwrap_or(end);
    String::from_utf8_lossy(&data[..nul]).into_owned()
}

pub(super) fn read_bbox(data: &[u8], off: usize) -> Result<[f32; 3]> {
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

pub(super) fn dequant(v: u16, mn: f32, mx: f32) -> f32 {
    mn + (v as f32 / 65535.0) * (mx - mn)
}

fn parse_combined(
    data: &[u8],
    entries: &[RawEntry],
    geom_off: usize,
    bmin: [f32; 3],
    bmax: [f32; 3],
    result: &mut ParsedMesh,
    geom_decomp: usize,
) {
    let total_v: usize = entries.iter().map(|e| e.nv).sum();
    let total_i: usize = entries.iter().map(|e| e.ni).sum();
    let stride = match detect_stride(data, geom_off, total_v, total_i, geom_decomp) {
        Some(s) => s,
        None    => return,
    };
    let idx_base = geom_off + total_v * stride;
    let has_uv   = stride >= 12;

    for e in entries {
        let vert_base = geom_off + e.ve * stride;
        let idx_off   = idx_base + e.ie * 2;
        if idx_off + e.ni * 2 > data.len() { continue; }

        let indices: Vec<usize> = (0..e.ni)
            .map(|j| u16::from_le_bytes(data[idx_off+j*2..idx_off+j*2+2].try_into().unwrap()) as usize)
            .collect();

        let mut unique: Vec<usize> = indices.iter().copied()
            .collect::<std::collections::HashSet<_>>().into_iter().collect();
        unique.sort_unstable();
        let idx_map: std::collections::HashMap<usize, usize> =
            unique.iter().enumerate().map(|(i, &g)| (g, i)).collect();

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

/// Detect vertex stride for a combined vertex+index buffer.
///
/// When `geom_decomp` is non-zero (the known total geometry size), stride is
/// derived algebraically: stride = (geom_decomp - total_i*2) / total_v.
/// This is essential when total_v > 65535, where any u16 index trivially
/// satisfies `< total_v` and the 50-sample probe always accepts stride=6.
pub(super) fn detect_stride(
    data: &[u8],
    geom_off: usize,
    total_v: usize,
    total_i: usize,
    geom_decomp: usize,
) -> Option<usize> {
    let idx_bytes = total_i * 2;

    if geom_decomp > idx_bytes && total_v > 0 {
        let remainder = geom_decomp - idx_bytes;
        if remainder % total_v == 0 {
            let candidate = remainder / total_v;
            if STRIDE_CANDIDATES.contains(&candidate) {
                let idx_base = geom_off + total_v * candidate;
                if idx_base + idx_bytes <= data.len() {
                    return Some(candidate);
                }
            }
        }
    }

    for &s in STRIDE_CANDIDATES {
        let idx_base = geom_off + total_v * s;
        if idx_base + idx_bytes > data.len() { continue; }
        let valid = (0..total_i.min(50)).all(|j| {
            let v = u16::from_le_bytes(data[idx_base+j*2..idx_base+j*2+2].try_into().unwrap()) as usize;
            v < total_v
        });
        if valid { return Some(s); }
    }
    None
}

pub(super) fn find_local_stride(data: &[u8], geom_off: usize, voff: usize, nv: usize, ni: usize) -> Option<(usize, usize)> {
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

pub(super) fn extract_verts(
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

pub(super) fn extract_faces(indices: &[usize], idx_map: &std::collections::HashMap<usize, usize>) -> Vec<[u32; 3]> {
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
