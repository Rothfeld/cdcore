//! PAM rebuilder -- MVP in-place position-only patch path.
//!
//! Mirrors the Python `build_pam` *fast path*: when the new mesh has
//! identical submesh count, vertex count, face indices, and UVs as the
//! original, the only thing that changed is vertex positions. We
//! re-quantize each new XYZ triple and overwrite the 6-byte slot in the
//! original PAM at the matching byte offset.
//!
//! Out of scope for this MVP (returns `PamFullRebuildRequired`):
//!   - vertex count change
//!   - face index change
//!   - UV edit
//!   - submesh add/remove
//!   - all four `_serialize_pam_*_layout` paths
//!
//! Full rebuild is stage 4b. Once the in-place fast path is byte-equivalent
//! against the Python oracle on a real `/cd` corpus, we extend.

use std::collections::HashMap;

use crate::error::{ParseError, Result};
use crate::formats::mesh::pam::parse as parse_pam;
use crate::repack::mesh::quant::quantize_u16;
use crate::repack::mesh::{ParsedMesh, SubMesh};

const HDR_BBOX_MIN: usize = 0x14;
const HDR_BBOX_MAX: usize = 0x20;
const HDR_GEOM_OFF: usize = 0x3C;
const PAR_MAGIC: &[u8] = b"PAR ";

#[derive(Debug)]
pub enum PamBuildError {
    /// Edit triggers the topology / UV / layout serializer path. Stage 4b.
    FullRebuildRequired { reason: &'static str },
    /// Conflicting edits on a shared-byte-offset vertex group.
    ConflictingAliases { byte_off: usize, sm_idx: usize, vert_idx: usize },
    Parse(ParseError),
}

impl From<ParseError> for PamBuildError {
    fn from(e: ParseError) -> Self {
        PamBuildError::Parse(e)
    }
}

impl std::fmt::Display for PamBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FullRebuildRequired { reason } => {
                write!(f, "PAM full rebuild required: {reason}")
            }
            Self::ConflictingAliases { byte_off, sm_idx, vert_idx } => write!(
                f,
                "linked vertices share source bytes at offset 0x{byte_off:X} \
                 but were edited differently (submesh {sm_idx} vertex {vert_idx})"
            ),
            Self::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PamBuildError {}

pub type BuildResult<T> = std::result::Result<T, PamBuildError>;

/// Rebuild a PAM binary from a modified mesh, in-place position-only.
///
/// Returns `PamFullRebuildRequired` for any edit beyond moving existing
/// vertices to new positions.
pub fn build_pam(mesh: &ParsedMesh, original_data: &[u8]) -> BuildResult<Vec<u8>> {
    if original_data.len() < 0x40 || &original_data[..4] != PAR_MAGIC {
        return Err(PamBuildError::Parse(ParseError::Other(
            "Original PAM data required for rebuild".into(),
        )));
    }

    let mut result = original_data.to_vec();
    let original_mesh = parse_pam(original_data, &mesh.path)?;

    // Working copy of the new mesh -- we may need to reorder submeshes to
    // match the original. Cheap clone since we only mutate the submesh slice.
    let mut working = mesh.clone();
    align_submesh_order_like_original(&original_mesh, &mut working);

    if pam_needs_full_rebuild(&original_mesh, &working) {
        return Err(PamBuildError::FullRebuildRequired {
            reason: "topology, UV, or submesh count changed",
        });
    }

    // Empty mesh: leave bytes untouched (matches Python early return).
    if original_mesh.submeshes.is_empty() {
        return Ok(result);
    }

    let orig_bmin = read_vec3(original_data, HDR_BBOX_MIN)?;
    let orig_bmax = read_vec3(original_data, HDR_BBOX_MAX)?;

    // Bbox: union of original + new vertex set so quantization covers both
    // historical and edited extents.
    let all_v: Vec<[f32; 3]> = working
        .submeshes
        .iter()
        .flat_map(|s| s.vertices.iter().copied())
        .collect();
    let (bmin, bmax) = expand_bbox_to_vertices(orig_bmin, orig_bmax, &all_v);

    write_vec3(&mut result, HDR_BBOX_MIN, bmin);
    write_vec3(&mut result, HDR_BBOX_MAX, bmax);

    let geom_off = read_u32_le(original_data, HDR_GEOM_OFF)? as usize;
    let offset_refs =
        collect_vertex_offset_refs(original_data, &original_mesh, &working, orig_bmin, orig_bmax, geom_off);
    let _patched = apply_quantized_vertex_patches(&mut result, &offset_refs, bmin, bmax, false)?;

    log::info!(
        "Built PAM {}: {} bytes (patched {} verts in-place)",
        mesh.path,
        result.len(),
        _patched
    );
    Ok(result)
}

/// Return true when edits go beyond in-place XYZ patching.
pub fn pam_needs_full_rebuild(original: &ParsedMesh, new_: &ParsedMesh) -> bool {
    if original.submeshes.len() != new_.submeshes.len() {
        return true;
    }
    for (orig_sm, new_sm) in original.submeshes.iter().zip(new_.submeshes.iter()) {
        if orig_sm.vertices.len() != new_sm.vertices.len() {
            return true;
        }
        if orig_sm.faces.len() != new_sm.faces.len() {
            return true;
        }
        if orig_sm.faces != new_sm.faces {
            return true;
        }
        if !submesh_uvs_match(orig_sm, new_sm, 1e-6) {
            return true;
        }
    }
    false
}

/// Two submeshes have equivalent UV payloads up to `eps`.
pub fn submesh_uvs_match(orig: &SubMesh, new_: &SubMesh, eps: f32) -> bool {
    let orig_has_uv = orig.uvs.len() == orig.vertices.len();
    let new_has_uv = new_.uvs.len() == new_.vertices.len();
    if orig_has_uv != new_has_uv {
        return false;
    }
    if !orig_has_uv {
        return true;
    }
    if orig.uvs.len() != new_.uvs.len() {
        return false;
    }
    for (a, b) in orig.uvs.iter().zip(new_.uvs.iter()) {
        if (a[0] - b[0]).abs() > eps || (a[1] - b[1]).abs() > eps {
            return false;
        }
    }
    true
}

/// Reorder `new_mesh.submeshes` to match the original's order by name when
/// names form a bijection. Otherwise leave unchanged.
pub fn align_submesh_order_like_original(original: &ParsedMesh, new_: &mut ParsedMesh) {
    if original.submeshes.len() != new_.submeshes.len() {
        return;
    }
    let orig_names: Vec<&str> = original.submeshes.iter().map(|s| s.name.as_str()).collect();
    let new_names: Vec<&str> = new_.submeshes.iter().map(|s| s.name.as_str()).collect();
    if orig_names == new_names {
        return;
    }
    let mut by_name: HashMap<String, SubMesh> = HashMap::with_capacity(new_.submeshes.len());
    for sm in std::mem::take(&mut new_.submeshes) {
        if sm.name.is_empty() || by_name.contains_key(&sm.name) {
            // Restore + bail; can't safely reorder.
            new_.submeshes = by_name.into_values().chain(std::iter::once(sm)).collect();
            return;
        }
        by_name.insert(sm.name.clone(), sm);
    }
    let orig_set: std::collections::HashSet<&str> = orig_names.iter().copied().collect();
    let new_set: std::collections::HashSet<String> = by_name.keys().cloned().collect();
    if new_set.iter().map(|s| s.as_str()).collect::<std::collections::HashSet<_>>() != orig_set {
        // Names don't match exactly; abort the reorder.
        new_.submeshes = by_name.into_values().collect();
        return;
    }
    new_.submeshes = orig_names
        .iter()
        .map(|name| by_name.remove(*name).unwrap())
        .collect();
}

/// Bbox union of an existing bbox + a new vertex list.
pub fn expand_bbox_to_vertices(
    orig_bmin: [f32; 3],
    orig_bmax: [f32; 3],
    vertices: &[[f32; 3]],
) -> ([f32; 3], [f32; 3]) {
    if vertices.is_empty() {
        return (orig_bmin, orig_bmax);
    }
    let mut bmin = orig_bmin;
    let mut bmax = orig_bmax;
    for v in vertices {
        for i in 0..3 {
            if v[i] < bmin[i] { bmin[i] = v[i]; }
            if v[i] > bmax[i] { bmax[i] = v[i]; }
        }
    }
    (bmin, bmax)
}

/// One byte-offset entry: (orig_pos, new_pos, sm_idx, vert_idx).
type AliasRef = ([f32; 3], [f32; 3], usize, usize);

/// Map source byte offsets to original/new vertex pairs. Uses the read-side
/// `source_vertex_offsets` when populated (fast path); otherwise falls back
/// to a forward linear scan from `search_start` for each vertex.
pub fn collect_vertex_offset_refs(
    original_data: &[u8],
    original: &ParsedMesh,
    new_: &ParsedMesh,
    orig_bmin: [f32; 3],
    orig_bmax: [f32; 3],
    search_start: usize,
) -> HashMap<usize, Vec<AliasRef>> {
    let mut offset_refs: HashMap<usize, Vec<AliasRef>> = HashMap::new();
    let mut cursor = search_start;

    for (sm_idx, (orig_sm, new_sm)) in original
        .submeshes
        .iter()
        .zip(new_.submeshes.iter())
        .enumerate()
    {
        let n = orig_sm.vertices.len().min(new_sm.vertices.len());

        // Resolve byte offsets per-vertex. If the parser populated source_vertex_offsets,
        // use them verbatim (fast). Otherwise fall back to scanning the original blob
        // for the quantized 6-byte XYZ triple.
        let mut sm_offsets: Vec<i64> =
            if orig_sm.source_vertex_offsets.len() == orig_sm.vertices.len() {
                orig_sm.source_vertex_offsets.clone()
            } else {
                Vec::with_capacity(orig_sm.vertices.len())
            };

        if sm_offsets.is_empty() {
            for vi in 0..orig_sm.vertices.len() {
                let [vx, vy, vz] = orig_sm.vertices[vi];
                let xu = quantize_u16(vx, orig_bmin[0], orig_bmax[0]);
                let yu = quantize_u16(vy, orig_bmin[1], orig_bmax[1]);
                let zu = quantize_u16(vz, orig_bmin[2], orig_bmax[2]);
                let target = [
                    (xu & 0xFF) as u8, (xu >> 8) as u8,
                    (yu & 0xFF) as u8, (yu >> 8) as u8,
                    (zu & 0xFF) as u8, (zu >> 8) as u8,
                ];
                let mut found: i64 = -1;
                if cursor + 6 <= original_data.len() {
                    for scan in cursor..original_data.len().saturating_sub(6) {
                        if original_data[scan..scan + 6] == target {
                            found = scan as i64;
                            cursor = scan + 6;
                            break;
                        }
                    }
                }
                sm_offsets.push(found);
            }
        }

        for vi in 0..n {
            if vi >= sm_offsets.len() || sm_offsets[vi] < 0 {
                continue;
            }
            let byte_off = sm_offsets[vi] as usize;
            offset_refs.entry(byte_off).or_default().push((
                orig_sm.vertices[vi],
                new_sm.vertices[vi],
                sm_idx,
                vi,
            ));
        }
    }
    offset_refs
}

/// Patch quantized XYZ values at the collected byte offsets.
/// Returns the number of distinct offsets patched.
///
/// `allow_average_conflicts=true` is the PAMLOD path: when multiple aliased
/// verts at the same byte offset move to different positions, the engine
/// shares one record across LODs, so the Python averages them. PAM rejects.
pub fn apply_quantized_vertex_patches(
    result: &mut [u8],
    offset_refs: &HashMap<usize, Vec<AliasRef>>,
    bmin: [f32; 3],
    bmax: [f32; 3],
    allow_average_conflicts: bool,
) -> BuildResult<usize> {
    let mut patched = 0usize;
    for (&byte_off, refs) in offset_refs {
        if byte_off + 6 > result.len() {
            continue;
        }
        let [vx, vy, vz] = resolve_pam_alias_vertex(byte_off, refs, allow_average_conflicts)?;
        let xu = quantize_u16(vx, bmin[0], bmax[0]);
        let yu = quantize_u16(vy, bmin[1], bmax[1]);
        let zu = quantize_u16(vz, bmin[2], bmax[2]);
        result[byte_off..byte_off + 2].copy_from_slice(&xu.to_le_bytes());
        result[byte_off + 2..byte_off + 4].copy_from_slice(&yu.to_le_bytes());
        result[byte_off + 4..byte_off + 6].copy_from_slice(&zu.to_le_bytes());
        patched += 1;
    }
    Ok(patched)
}

/// Choose one final position for a shared vertex byte offset.
///
/// `allow_average_conflicts=false` (PAM): conflicting edits at the same
/// byte offset error out. `=true` (PAMLOD): conflicts get averaged across
/// the changed aliases.
pub fn resolve_pam_alias_vertex(
    byte_off: usize,
    refs: &[AliasRef],
    allow_average_conflicts: bool,
) -> BuildResult<[f32; 3]> {
    let mut changed: Vec<([f32; 3], usize, usize)> = Vec::new();
    for &(orig_v, new_v, sm_idx, vi) in refs {
        let dx = orig_v[0] - new_v[0];
        let dy = orig_v[1] - new_v[1];
        let dz = orig_v[2] - new_v[2];
        if (dx * dx + dy * dy + dz * dz).sqrt() > 1e-6 {
            changed.push((new_v, sm_idx, vi));
        }
    }
    if changed.is_empty() {
        return Ok(refs[0].1);
    }
    let chosen = changed[0].0;
    for &(new_v, sm_idx, vi) in &changed[1..] {
        let dx = chosen[0] - new_v[0];
        let dy = chosen[1] - new_v[1];
        let dz = chosen[2] - new_v[2];
        if (dx * dx + dy * dy + dz * dz).sqrt() > 1e-6 {
            if allow_average_conflicts {
                let n = changed.len() as f32;
                let sum = changed.iter().fold([0.0f32; 3], |acc, (p, _, _)| {
                    [acc[0] + p[0], acc[1] + p[1], acc[2] + p[2]]
                });
                return Ok([sum[0] / n, sum[1] / n, sum[2] / n]);
            }
            return Err(PamBuildError::ConflictingAliases {
                byte_off,
                sm_idx,
                vert_idx: vi,
            });
        }
    }
    Ok(chosen)
}

#[inline]
fn read_u32_le(data: &[u8], off: usize) -> Result<u32> {
    if off + 4 > data.len() {
        return Err(ParseError::eof(off, 4, data.len() - off.min(data.len())));
    }
    Ok(u32::from_le_bytes(data[off..off + 4].try_into().unwrap()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sm(name: &str, verts: &[[f32; 3]], faces: &[[u32; 3]]) -> SubMesh {
        SubMesh {
            name: name.to_string(),
            vertices: verts.to_vec(),
            faces: faces.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn submesh_uvs_match_handles_missing_uvs() {
        let a = sm("x", &[[0.0; 3], [1.0; 3]], &[]);
        let b = sm("x", &[[0.0; 3], [1.0; 3]], &[]);
        assert!(submesh_uvs_match(&a, &b, 1e-6));
    }

    #[test]
    fn pam_needs_full_rebuild_detects_each_change() {
        let orig = ParsedMesh {
            submeshes: vec![sm("a", &[[0.0; 3], [1.0; 3]], &[[0u32, 1, 0]])],
            ..Default::default()
        };

        // Identity copy -> no rebuild.
        let same = orig.clone();
        assert!(!pam_needs_full_rebuild(&orig, &same));

        // Vertex count change -> rebuild.
        let mut bigger = orig.clone();
        bigger.submeshes[0].vertices.push([2.0; 3]);
        assert!(pam_needs_full_rebuild(&orig, &bigger));

        // Face change -> rebuild.
        let mut faces = orig.clone();
        faces.submeshes[0].faces[0] = [0, 1, 0];
        assert!(!pam_needs_full_rebuild(&orig, &faces)); // identical
        faces.submeshes[0].faces[0] = [1, 0, 0];
        assert!(pam_needs_full_rebuild(&orig, &faces));

        // Submesh count change -> rebuild.
        let mut more_sm = orig.clone();
        more_sm.submeshes.push(sm("b", &[], &[]));
        assert!(pam_needs_full_rebuild(&orig, &more_sm));
    }

    #[test]
    fn align_submesh_order_swaps_when_names_unique() {
        let orig = ParsedMesh {
            submeshes: vec![
                sm("a", &[], &[]),
                sm("b", &[], &[]),
                sm("c", &[], &[]),
            ],
            ..Default::default()
        };
        let mut new_ = ParsedMesh {
            submeshes: vec![
                sm("c", &[], &[]),
                sm("a", &[], &[]),
                sm("b", &[], &[]),
            ],
            ..Default::default()
        };
        align_submesh_order_like_original(&orig, &mut new_);
        let names: Vec<_> = new_.submeshes.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn expand_bbox_unions_with_new_verts() {
        let (mn, mx) = expand_bbox_to_vertices(
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            &[[-1.0, 0.5, 0.5], [0.5, 2.0, 0.5]],
        );
        assert_eq!(mn, [-1.0, 0.0, 0.0]);
        assert_eq!(mx, [1.0, 2.0, 1.0]);
    }

    #[test]
    fn resolve_pam_alias_vertex_consistent_edit() {
        let new_pos = [5.0, 5.0, 5.0];
        let refs: Vec<AliasRef> = vec![
            ([0.0; 3], new_pos, 0, 0),
            ([0.0; 3], new_pos, 0, 1),
        ];
        let r = resolve_pam_alias_vertex(0, &refs, false).unwrap();
        assert_eq!(r, new_pos);
    }

    #[test]
    fn resolve_pam_alias_vertex_conflict_errors() {
        let refs: Vec<AliasRef> = vec![
            ([0.0; 3], [5.0; 3], 0, 0),
            ([0.0; 3], [-5.0; 3], 0, 1),
        ];
        let err = resolve_pam_alias_vertex(0x100, &refs, false).unwrap_err();
        match err {
            PamBuildError::ConflictingAliases { byte_off, .. } => assert_eq!(byte_off, 0x100),
            other => panic!("expected ConflictingAliases, got {other:?}"),
        }
    }

    #[test]
    fn resolve_pam_alias_vertex_unchanged_returns_original() {
        let refs: Vec<AliasRef> = vec![([3.0, 4.0, 5.0], [3.0, 4.0, 5.0], 0, 0)];
        let r = resolve_pam_alias_vertex(0, &refs, false).unwrap();
        assert_eq!(r, [3.0, 4.0, 5.0]);
    }
}
