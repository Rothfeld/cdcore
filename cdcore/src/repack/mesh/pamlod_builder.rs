//! PAMLOD rebuilder -- in-place position-only patch over each LOD level.
//!
//! Mirrors the Python `build_pamlod` fast path. PAMLOD has its own header
//! offsets (bbox at 0x10/0x1C, no PAR magic) and uses a linear-scan search
//! over the entire blob (the compressed LZ4 LOD geometry happens to leave
//! the quantized XYZ literal sequences findable in many real files).
//!
//! Unlike `build_pam`, alias conflicts are *averaged* (`allow_average_conflicts=true`)
//! because the engine shares one record across LODs and conflicting edits
//! across LODs at the same byte offset are common.
//!
//! Out of scope for this MVP: layout-aware full rebuild. If the modified
//! mesh changes vertex count / face indices / UVs, the patches simply do
//! nothing for those slots (returns 0 patched).

use std::collections::HashMap;

use crate::error::{ParseError, Result};
use crate::formats::mesh::pamlod::parse_all_lods;
use crate::repack::mesh::pam_builder::{
    apply_quantized_vertex_patches, collect_vertex_offset_refs, expand_bbox_to_vertices,
    BuildResult, PamBuildError,
};
use crate::repack::mesh::ParsedMesh;

const HDR_BBOX_MIN: usize = 0x10;
const HDR_BBOX_MAX: usize = 0x1C;

/// Rebuild a PAMLOD binary by patching vertex positions in-place.
///
/// `mesh.lod_levels` (when non-empty) carries one Vec<SubMesh> per LOD;
/// each non-empty inner vec replaces the corresponding original LOD's
/// submeshes. If `mesh.lod_levels` is empty but `mesh.submeshes` is not,
/// the first non-empty original LOD is replaced with `mesh.submeshes`.
pub fn build_pamlod(mesh: &ParsedMesh, original_data: &[u8]) -> BuildResult<Vec<u8>> {
    if original_data.len() < 0x20 {
        return Err(PamBuildError::Parse(ParseError::Other(
            "Original PAMLOD data required for rebuild".into(),
        )));
    }

    let mut result = original_data.to_vec();
    let orig_bmin = read_vec3(original_data, HDR_BBOX_MIN)?;
    let orig_bmax = read_vec3(original_data, HDR_BBOX_MAX)?;

    // parse_all_lods returns one ParsedMesh per LOD; each has its own submeshes
    // populated (with source_vertex_offsets pointing into the decompressed LOD
    // buffer, not the original file). For PAMLOD we rely on the linear-scan
    // fallback in collect_vertex_offset_refs to locate quantized triples in
    // the compressed-on-disk stream.
    let orig_lods = parse_all_lods(original_data, &mesh.path)
        .map_err(PamBuildError::from)?;
    if orig_lods.is_empty() {
        return Ok(result);
    }

    // Build target_lod_levels: one Vec<SubMesh> per LOD, defaulting to the
    // original LOD's submeshes, replaced where the user provided edits.
    let mut target: Vec<Vec<crate::repack::mesh::SubMesh>> = orig_lods
        .iter()
        .map(|lod| lod.submeshes.clone())
        .collect();

    if !mesh.lod_levels.is_empty() {
        for (i, edited) in mesh.lod_levels.iter().enumerate() {
            if i < target.len() && !edited.is_empty() {
                target[i] = edited.clone();
            }
        }
    } else if !mesh.submeshes.is_empty() {
        let replace_idx = target
            .iter()
            .position(|lod| !lod.is_empty())
            .unwrap_or(0);
        target[replace_idx] = mesh.submeshes.clone();
    }

    // Bbox = union of every LOD's submesh vertices.
    let all_v: Vec<[f32; 3]> = target
        .iter()
        .flat_map(|lod| lod.iter())
        .flat_map(|sm| sm.vertices.iter().copied())
        .collect();
    let (bmin, bmax) = expand_bbox_to_vertices(orig_bmin, orig_bmax, &all_v);
    write_vec3(&mut result, HDR_BBOX_MIN, bmin);
    write_vec3(&mut result, HDR_BBOX_MAX, bmax);

    // Collect alias refs from every LOD into a single map. Linear scan over
    // the entire original_data; PAMLOD's compressed sections still expose
    // the quantized XYZ triples in many cases.
    let mut offset_refs = HashMap::new();
    for (lod_idx, orig_lod) in orig_lods.iter().enumerate() {
        let new_lod = match target.get(lod_idx) {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        if orig_lod.submeshes.is_empty() || new_lod.is_empty() {
            continue;
        }
        let level_orig = make_temp_mesh(&mesh.path, orig_lod.submeshes.clone());
        let level_new  = make_temp_mesh(&mesh.path, new_lod.clone());
        let level_refs = collect_vertex_offset_refs(
            original_data, &level_orig, &level_new, orig_bmin, orig_bmax, /*search_start=*/ 0,
        );
        for (off, refs) in level_refs {
            offset_refs.entry(off).or_insert_with(Vec::new).extend(refs);
        }
    }

    let _patched = apply_quantized_vertex_patches(
        &mut result, &offset_refs, bmin, bmax, /*allow_average_conflicts=*/ true,
    )?;

    log::info!(
        "Built PAMLOD {}: {} bytes (patched {} verts in-place)",
        mesh.path,
        result.len(),
        _patched
    );
    Ok(result)
}

fn make_temp_mesh(path: &str, submeshes: Vec<crate::repack::mesh::SubMesh>) -> ParsedMesh {
    ParsedMesh {
        path: path.to_string(),
        format: "pamlod".into(),
        submeshes,
        ..Default::default()
    }
}

#[inline]
fn read_vec3(data: &[u8], off: usize) -> Result<[f32; 3]> {
    if off + 12 > data.len() {
        return Err(ParseError::eof(off, 12, data.len() - off.min(data.len())));
    }
    let f = |i: usize| f32::from_le_bytes(data[off + i..off + i + 4].try_into().unwrap());
    Ok([f(0), f(4), f(8)])
}

#[inline]
fn write_vec3(data: &mut [u8], off: usize, v: [f32; 3]) {
    data[off..off + 4].copy_from_slice(&v[0].to_le_bytes());
    data[off + 4..off + 8].copy_from_slice(&v[1].to_le_bytes());
    data[off + 8..off + 12].copy_from_slice(&v[2].to_le_bytes());
}
