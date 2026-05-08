//! PAM full-rebuild path for the "local" layout (per-submesh contiguous
//! vertex+index blocks following the submesh table).
//!
//! Mirrors `_serialize_pam_local_layout` and friends from
//! `core/mesh_importer.py`. Used when topology / UV / submesh count
//! changed and the in-place position-only patch path can't be taken.

use half::f16;

use crate::repack::mesh::donor::choose_pac_donor_indices;
use crate::repack::mesh::layout::SubmeshEntry;
use crate::repack::mesh::quant::quantize_u16;
use crate::repack::mesh::ParsedMesh;

const HDR_BBOX_MIN: usize = 0x14;
const HDR_BBOX_MAX: usize = 0x20;
const HDR_GEOM_SIZE: usize = 0x40;
const SUBMESH_TABLE: usize = 0x410;
const SUBMESH_STRIDE: usize = 0x218;

/// Rebuild a single-submesh-or-multi local-layout PAM from scratch.
pub fn serialize_local_layout(
    mesh: &ParsedMesh,
    original_mesh: &ParsedMesh,
    original_data: &[u8],
    geom_off: usize,
    entries: &[SubmeshEntry],
    old_geom_end: usize,
    bmin: [f32; 3],
    bmax: [f32; 3],
) -> Vec<u8> {
    let mut result: Vec<u8> = original_data[..geom_off].to_vec();

    write_vec3(&mut result, HDR_BBOX_MIN, bmin);
    write_vec3(&mut result, HDR_BBOX_MAX, bmax);

    let mut geom_data: Vec<u8> = Vec::new();
    let mut current_voff: u32 = 0;

    for ((sm, orig_sm), entry) in mesh
        .submeshes
        .iter()
        .zip(original_mesh.submeshes.iter())
        .zip(entries.iter())
    {
        let stride = entry.stride;
        let nv = sm.vertices.len() as u32;
        let ni = (sm.faces.len() * 3) as u32;

        // Update submesh descriptor: nv, ni, ve (offset into geometry block),
        // ie=0 (local layout doesn't use a separate index region).
        write_u32(&mut result, entry.desc_off, nv);
        write_u32(&mut result, entry.desc_off + 4, ni);
        write_u32(&mut result, entry.desc_off + 8, current_voff);
        write_u32(&mut result, entry.desc_off + 12, 0);

        let orig_vert_base = geom_off + entry.ve;
        let orig_nv = entry.nv;
        let has_uvs = sm.uvs.len() == sm.vertices.len();

        // Donor matching: spatial-only (the Python falls through to spatial
        // for any meaningfully-sized topology change anyway, since the DP
        // alignment bails at >3M states).
        let donor_indices = choose_pac_donor_indices(orig_sm, sm);

        for (vi, &vertex) in sm.vertices.iter().enumerate() {
            let donor_idx = donor_indices.get(vi).copied().unwrap_or(vi);
            let mut rec =
                make_vertex_template_record(original_data, orig_vert_base, stride, donor_idx, orig_nv);
            let uv = if has_uvs { Some(sm.uvs[vi]) } else { None };
            pack_static_vertex_record(&mut rec, stride, vertex, uv, bmin, bmax);
            geom_data.extend_from_slice(&rec);
        }

        for face in &sm.faces {
            geom_data.extend_from_slice(&(face[0] as u16).to_le_bytes());
            geom_data.extend_from_slice(&(face[1] as u16).to_le_bytes());
            geom_data.extend_from_slice(&(face[2] as u16).to_le_bytes());
        }

        current_voff += nv * stride as u32 + ni * 2;
    }

    let new_geom_end = geom_off + geom_data.len();
    result.extend_from_slice(&geom_data);

    sync_geom_size_header(&mut result, original_data, geom_off, old_geom_end, new_geom_end);
    result.extend_from_slice(&original_data[old_geom_end..]);

    sync_header_mirrors(&mut result, original_mesh, mesh, geom_off);

    result
}

/// Copy a template vertex record from the original file when possible. When
/// the donor index is past the original vertex count the record is zero-
/// initialised, matching Python's fallback.
fn make_vertex_template_record(
    data: &[u8],
    base_off: usize,
    stride: usize,
    index: usize,
    fallback_count: usize,
) -> Vec<u8> {
    if fallback_count > 0 {
        let src_idx = index.min(fallback_count - 1);
        let rec_off = base_off + src_idx * stride;
        if rec_off + stride <= data.len() {
            return data[rec_off..rec_off + stride].to_vec();
        }
    }
    vec![0u8; stride]
}

/// Write XYZ (u16 quantised) and optional UV (f16) into a static-mesh
/// vertex record. Bytes 12..stride keep whatever the donor record had.
fn pack_static_vertex_record(
    rec: &mut Vec<u8>,
    stride: usize,
    vertex: [f32; 3],
    uv: Option<[f32; 2]>,
    bmin: [f32; 3],
    bmax: [f32; 3],
) {
    if rec.len() < stride {
        rec.resize(stride, 0);
    }

    let xu = quantize_u16(vertex[0], bmin[0], bmax[0]);
    let yu = quantize_u16(vertex[1], bmin[1], bmax[1]);
    let zu = quantize_u16(vertex[2], bmin[2], bmax[2]);
    rec[0..2].copy_from_slice(&xu.to_le_bytes());
    rec[2..4].copy_from_slice(&yu.to_le_bytes());
    rec[4..6].copy_from_slice(&zu.to_le_bytes());

    if stride >= 12 {
        if let Some([u, v]) = uv {
            // f16 (IEEE 754 binary16). Out-of-range floats fall back to 0.0,
            // matching Python's struct.pack `<e` overflow handling.
            let pack = |x: f32| -> [u8; 2] {
                let h = f16::from_f32(x);
                if h.is_finite() {
                    h.to_le_bytes()
                } else {
                    f16::from_f32(0.0).to_le_bytes()
                }
            };
            rec[8..10].copy_from_slice(&pack(u));
            rec[10..12].copy_from_slice(&pack(v));
        }
    }
}

/// Update the header field at 0x40 if it mirrors the geometry block length.
fn sync_geom_size_header(
    result: &mut Vec<u8>,
    original_data: &[u8],
    geom_off: usize,
    old_geom_end: usize,
    new_geom_end: usize,
) -> bool {
    if result.len() < HDR_GEOM_SIZE + 4
        || original_data.len() < HDR_GEOM_SIZE + 4
        || geom_off == 0
        || old_geom_end < geom_off
        || new_geom_end < geom_off
    {
        return false;
    }
    let original_geom_len = (old_geom_end - geom_off) as u32;
    let header_geom_len = u32::from_le_bytes(
        original_data[HDR_GEOM_SIZE..HDR_GEOM_SIZE + 4].try_into().unwrap(),
    );
    if header_geom_len != original_geom_len {
        return false;
    }
    let new_len = (new_geom_end - geom_off) as u32;
    result[HDR_GEOM_SIZE..HDR_GEOM_SIZE + 4].copy_from_slice(&new_len.to_le_bytes());
    true
}

/// Update mirrored count/bbox/(nv, ni) pairs that the engine keeps inside
/// the per-submesh descriptor between the table tail and `geom_off`. Mirrors
/// `_sync_pam_header_mirrors` from the Python reference; needed so a mesh
/// with a different vertex count actually loads in-game.
fn sync_header_mirrors(
    result: &mut Vec<u8>,
    original_mesh: &ParsedMesh,
    new_mesh: &ParsedMesh,
    geom_off: usize,
) -> usize {
    let mesh_count = original_mesh.submeshes.len().min(new_mesh.submeshes.len());
    let region_start = SUBMESH_TABLE + mesh_count * SUBMESH_STRIDE;
    let region_end = geom_off.max(region_start).min(result.len());
    if region_start >= region_end {
        return 0;
    }

    let mut patched = 0usize;

    for (orig_sm, new_sm) in original_mesh.submeshes.iter().zip(new_mesh.submeshes.iter()) {
        let orig_nv = orig_sm.vertices.len() as u32;
        let orig_ni = (orig_sm.faces.len() * 3) as u32;
        let new_nv = new_sm.vertices.len() as u32;
        let new_ni = (new_sm.faces.len() * 3) as u32;

        let old_bbox = bbox6(&orig_sm.vertices);
        let new_bbox = if !new_sm.vertices.is_empty() {
            bbox6(&new_sm.vertices)
        } else {
            old_bbox
        };
        let old_bbox_bytes = pack_f32x6(old_bbox);
        let new_bbox_bytes = pack_f32x6(new_bbox);

        // (ni, bbox6) co-located patch.
        let mut a = orig_ni.to_le_bytes().to_vec();
        a.extend_from_slice(&old_bbox_bytes);
        let mut b = new_ni.to_le_bytes().to_vec();
        b.extend_from_slice(&new_bbox_bytes);
        patched += replace_all_in_region(result, region_start, region_end, &a, &b);

        // Standalone bbox patch.
        patched += replace_all_in_region(result, region_start, region_end, &old_bbox_bytes, &new_bbox_bytes);

        // Scan for [u32 count, 6×f32 bbox] tuples that match orig_ni + old_bbox.
        let mut off = region_start;
        while off + 28 <= region_end {
            let count = u32::from_le_bytes(result[off..off + 4].try_into().unwrap());
            if count == orig_ni && bbox6_close(&result[off + 4..off + 28], old_bbox, 1e-3) {
                result[off..off + 4].copy_from_slice(&new_ni.to_le_bytes());
                result[off + 4..off + 28].copy_from_slice(&new_bbox_bytes);
                patched += 1;
            }
            off += 4;
        }

        // Standalone bbox match scan.
        let mut off = region_start;
        while off + 24 <= region_end {
            if bbox6_close(&result[off..off + 24], old_bbox, 1e-3) {
                result[off..off + 24].copy_from_slice(&new_bbox_bytes);
                patched += 1;
            }
            off += 4;
        }

        // (nv, ni) pair patch — anchored on texture/material name.
        let old_pair: [u8; 8] = {
            let mut p = [0u8; 8];
            p[..4].copy_from_slice(&orig_nv.to_le_bytes());
            p[4..].copy_from_slice(&orig_ni.to_le_bytes());
            p
        };
        let new_pair: [u8; 8] = {
            let mut p = [0u8; 8];
            p[..4].copy_from_slice(&new_nv.to_le_bytes());
            p[4..].copy_from_slice(&new_ni.to_le_bytes());
            p
        };
        if old_pair == new_pair {
            continue;
        }

        for anchor in [orig_sm.texture.as_str(), orig_sm.material.as_str()] {
            if anchor.is_empty() {
                continue;
            }
            let needle = anchor.as_bytes();
            let mut cursor = region_start;
            while cursor + needle.len() <= region_end {
                let Some(pos) = find_subslice(&result[cursor..region_end], needle) else { break };
                let abs_pos = cursor + pos;
                let pair_off = abs_pos.checked_sub(8);
                if let Some(pair_off) = pair_off {
                    if pair_off >= region_start && &result[pair_off..pair_off + 8] == old_pair {
                        result[pair_off..pair_off + 8].copy_from_slice(&new_pair);
                        patched += 1;
                    }
                }
                cursor = abs_pos + needle.len();
            }
        }
    }

    patched
}

fn bbox6(verts: &[[f32; 3]]) -> [f32; 6] {
    if verts.is_empty() {
        return [0.0; 6];
    }
    let mut mn = verts[0];
    let mut mx = verts[0];
    for v in verts {
        for i in 0..3 {
            if v[i] < mn[i] { mn[i] = v[i]; }
            if v[i] > mx[i] { mx[i] = v[i]; }
        }
    }
    [mn[0], mn[1], mn[2], mx[0], mx[1], mx[2]]
}

fn pack_f32x6(b: [f32; 6]) -> [u8; 24] {
    let mut out = [0u8; 24];
    for (i, v) in b.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

fn bbox6_close(buf: &[u8], reference: [f32; 6], tol: f32) -> bool {
    if buf.len() < 24 {
        return false;
    }
    for i in 0..6 {
        let v = f32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
        if !v.is_finite() || (v - reference[i]).abs() > tol {
            return false;
        }
    }
    true
}

fn replace_all_in_region(data: &mut [u8], start: usize, end: usize, old: &[u8], new: &[u8]) -> usize {
    if old.is_empty() || old == new || start >= end || old.len() != new.len() {
        return 0;
    }
    let mut hits = 0usize;
    let mut cursor = start;
    while cursor + old.len() <= end {
        if let Some(pos) = find_subslice(&data[cursor..end], old) {
            let abs_pos = cursor + pos;
            data[abs_pos..abs_pos + new.len()].copy_from_slice(new);
            hits += 1;
            cursor = abs_pos + new.len();
        } else {
            break;
        }
    }
    hits
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn write_vec3(buf: &mut [u8], off: usize, v: [f32; 3]) {
    buf[off..off + 4].copy_from_slice(&v[0].to_le_bytes());
    buf[off + 4..off + 8].copy_from_slice(&v[1].to_le_bytes());
    buf[off + 8..off + 12].copy_from_slice(&v[2].to_le_bytes());
}

fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

