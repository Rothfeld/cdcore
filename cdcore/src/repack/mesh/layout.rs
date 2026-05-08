//! PAM layout detection for the topology-changing rebuild path.
//!
//! Mirrors `_inspect_pam_layout` from `core/mesh_importer.py`. Currently
//! only the `Local` variant is implemented — that's the layout used by the
//! single-submesh static props (cd_gimmick_*) where each submesh has its
//! own contiguous vertex+index region following the submesh table. The
//! combined / forward-scan / backward-scan branches return `Unsupported`
//! and let the caller fall back to position-only patching.

use crate::formats::mesh::pam::find_local_stride;

const HDR_GEOM_OFF: usize = 0x3C;
const HDR_MESH_COUNT: usize = 0x10;
const SUBMESH_TABLE: usize = 0x410;
const SUBMESH_STRIDE: usize = 0x218;
const PAM_IDX_OFF: usize = 0x19840;
const PAR_MAGIC: &[u8] = b"PAR ";

#[derive(Debug, Clone)]
pub struct SubmeshEntry {
    pub desc_off: usize,
    pub nv: usize,
    pub ni: usize,
    pub ve: usize,
    pub ie: usize,
    pub stride: usize,
    pub idx_off: usize,
}

#[derive(Debug, Clone)]
pub enum PamLayout {
    Local {
        geom_off: usize,
        entries: Vec<SubmeshEntry>,
        old_geom_end: usize,
    },
    Unsupported {
        reason: &'static str,
    },
}

/// Detect the PAM geometry layout. Right now only the Local variant is
/// surfaced; everything else returns Unsupported so the caller falls back
/// to the position-only patch path.
pub fn inspect_pam_layout(data: &[u8]) -> PamLayout {
    if data.len() < 0x40 || &data[..4] != PAR_MAGIC {
        return PamLayout::Unsupported { reason: "missing PAM header" };
    }

    let geom_off = u32::from_le_bytes(data[HDR_GEOM_OFF..HDR_GEOM_OFF + 4].try_into().unwrap()) as usize;
    let mesh_count = u32::from_le_bytes(data[HDR_MESH_COUNT..HDR_MESH_COUNT + 4].try_into().unwrap()) as usize;
    if mesh_count == 0 {
        return PamLayout::Unsupported { reason: "mesh table is empty" };
    }

    // Read submesh table.
    let mut raw: Vec<(usize, usize, usize, usize, usize)> = Vec::with_capacity(mesh_count);
    for i in 0..mesh_count {
        let desc_off = SUBMESH_TABLE + i * SUBMESH_STRIDE;
        if desc_off + SUBMESH_STRIDE > data.len() {
            return PamLayout::Unsupported { reason: "submesh table is truncated" };
        }
        let nv = u32::from_le_bytes(data[desc_off..desc_off + 4].try_into().unwrap()) as usize;
        let ni = u32::from_le_bytes(data[desc_off + 4..desc_off + 8].try_into().unwrap()) as usize;
        let ve = u32::from_le_bytes(data[desc_off + 8..desc_off + 12].try_into().unwrap()) as usize;
        let ie = u32::from_le_bytes(data[desc_off + 12..desc_off + 16].try_into().unwrap()) as usize;
        raw.push((desc_off, nv, ni, ve, ie));
    }

    // Combined-buffer detection (mirrors the Python). When mesh_count > 1 and
    // ve/ie accumulate by nv/ni, the mesh uses one shared buffer rather than
    // per-submesh local buffers. We don't implement a combined-layout
    // serializer yet, so flag it as unsupported.
    let mut is_combined = mesh_count > 1;
    if is_combined {
        let mut ve_acc = 0usize;
        let mut ie_acc = 0usize;
        for &(_, nv, ni, ve, ie) in &raw {
            if ve != ve_acc || ie != ie_acc {
                is_combined = false;
                break;
            }
            ve_acc += nv;
            ie_acc += ni;
        }
    }
    if is_combined {
        return PamLayout::Unsupported { reason: "combined PAM layout: full rebuild not yet ported" };
    }

    // Local-layout detection: each submesh has its own [vertices][indices]
    // block at geom_off + ve. Use find_local_stride to locate the index block
    // (and hence the per-submesh stride) by validating that the bytes after
    // nv * stride parse as in-range u16 indices.
    let pam_idx_avail = data.len().saturating_sub(PAM_IDX_OFF) / 2;
    let mut local_entries: Vec<SubmeshEntry> = Vec::with_capacity(mesh_count);
    let mut uses_global = false;
    let mut old_geom_end = geom_off;

    for &(desc_off, nv, ni, ve, ie) in &raw {
        if let Some((stride, idx_off)) = find_local_stride(data, geom_off, ve, nv, ni) {
            let entry_end = idx_off + ni * 2;
            if entry_end > old_geom_end {
                old_geom_end = entry_end;
            }
            local_entries.push(SubmeshEntry {
                desc_off, nv, ni, ve, ie, stride, idx_off,
            });
            continue;
        }
        if ie + ni <= pam_idx_avail {
            uses_global = true;
        } else {
            return PamLayout::Unsupported { reason: "PAM uses scan-fallback geometry layout" };
        }
    }

    if uses_global {
        return PamLayout::Unsupported { reason: "global-buffer PAM rebuild is not implemented yet" };
    }

    PamLayout::Local { geom_off, entries: local_entries, old_geom_end }
}
