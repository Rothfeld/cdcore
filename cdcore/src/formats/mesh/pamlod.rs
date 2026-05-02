//! PAMLOD LOD mesh parser.
//!
//! Header (no PAR magic; starts directly with PAMLOD fields):
//!   0x00 lod_count  (u32)
//!   0x04 geom_off   (u32) — byte offset to first LOD's geometry in the file
//!   0x10 bbox_min   (3×f32)
//!   0x1C bbox_max   (3×f32)
//!   0x50 lod_entry_table — per-LOD submesh descriptors (DDS-string-anchored)
//!
//! Per-LOD geometry table at geom_off - (lod_count+1)*12:
//!   Three layouts are encountered in the wild (all 12 bytes per entry):
//!
//!   Format A/C — [start_offset, decomp_size, lz4_size] per LOD.
//!     The entry whose start_offset == geom_off is LOD 0 (may be at index 0
//!     or index 1 with an all-zero placeholder at index 0).
//!     lz4_size=0 means the section is stored raw.
//!     e.g. cd_barricade_gaurd_02.pamlod (A), cd_spot_tower_10_stairs_01.pamlod (C)
//!
//!   Format B — [decomp_size, lz4_size, section_end_offset] per LOD.
//!     entries[0] = [0, 0, geom_off] anchors the geometry start.
//!     For LOD k: data starts at entries[k].f3, decomp/lz4 in entries[k+1].
//!     e.g. cd_puzzle_anamorphic_north_01.pamlod

use std::collections::{HashMap, HashSet};
use crate::error::{read_u32_le, Result, ParseError};
use super::pam::{
    ParsedMesh, SubMesh,
    detect_stride, extract_verts, extract_faces,
    compute_smooth_normals, read_bbox, cstr,
};

const LOD_COUNT_OFF: usize = 0x00;
const GEOM_OFF_OFF:  usize = 0x04;
const BBOX_MIN_OFF:  usize = 0x10;
const BBOX_MAX_OFF:  usize = 0x1C;
const ENTRY_TABLE:   usize = 0x50;

// ── Public entry points ───────────────────────────────────────────────

pub fn parse_lod0(data: &[u8], filename: &str) -> Result<ParsedMesh> {
    parse_lod(data, filename, 0)
}

pub fn parse_all_lods(data: &[u8], filename: &str) -> Result<Vec<ParsedMesh>> {
    let lod_count = read_u32_le(data, LOD_COUNT_OFF)? as usize;
    let mut out = Vec::with_capacity(lod_count.min(8));
    for i in 0..lod_count.min(8) {
        out.push(parse_lod(data, filename, i)?);
    }
    Ok(out)
}

fn parse_lod(data: &[u8], filename: &str, lod_level: usize) -> Result<ParsedMesh> {
    if data.len() < ENTRY_TABLE + 12 {
        return Err(ParseError::Other(format!("{filename}: too short for PAMLOD")));
    }

    let lod_count = read_u32_le(data, LOD_COUNT_OFF)? as usize;
    let geom_off  = read_u32_le(data, GEOM_OFF_OFF)?  as usize;

    if lod_count == 0 || geom_off == 0 || geom_off >= data.len() {
        return Ok(ParsedMesh { path: filename.into(), format: "pamlod".into(), ..Default::default() });
    }

    let bbox_min = read_bbox(data, BBOX_MIN_OFF)?;
    let bbox_max = read_bbox(data, BBOX_MAX_OFF)?;

    // Group per-submesh descriptors into LOD levels
    let mut lod_groups = scan_lod_groups(data, geom_off, lod_count);
    if lod_groups.is_empty() {
        return Ok(ParsedMesh { path: filename.into(), format: "pamlod".into(), ..Default::default() });
    }

    // Obtain decompressed geometry chunk for the requested LOD.
    let chunks = get_lod_chunks(data, geom_off, lod_count);

    // For sequential-scan files (no chunk table), sort groups by total vertex
    // count descending so the highest-quality LOD is always first.  Some large
    // composite objects store DDS entries for LOD0 after other LOD entries in
    // the header, causing the file-position sort to pick a smaller LOD as LOD0.
    if chunks.is_none() && lod_groups.len() > 1 {
        lod_groups.sort_by(|a, b| {
            let nv_a: usize = a.iter().map(|e| e.nv).sum();
            let nv_b: usize = b.iter().map(|e| e.nv).sum();
            nv_b.cmp(&nv_a)
        });
    }

    // Pre-compute the algebraic stride for sequential-scan files.
    // When total_nv > 65535 the index-value probe is trivially satisfied, so
    // stride=6 is always accepted.  Deriving stride from the total geometry
    // budget gives the correct answer without ambiguity.
    let seq_alg_stride: Option<usize> = if chunks.is_none() {
        let all_nv: usize = lod_groups.iter().flat_map(|g| g.iter()).map(|e| e.nv).sum();
        let all_ni: usize = lod_groups.iter().flat_map(|g| g.iter()).map(|e| e.ni).sum();
        let geom_size = data.len().saturating_sub(geom_off);
        if all_nv > 0 && geom_size > all_ni * 2 {
            let s_est = (geom_size - all_ni * 2) as f64 / all_nv as f64;
            super::pam::STRIDE_CANDIDATES.iter()
                .find(|&&s| (s as f64 - s_est).abs() < 2.0)
                .copied()
        } else { None }
    } else { None };

    let lod_idx = lod_level.min(lod_groups.len().saturating_sub(1));

    // When per-LOD chunks are available, match each chunk to its lod_group by
    // geometry size so the table's LOD order is honoured even when the header
    // stores entries in non-nv-descending order (e.g. eggs where LOD0 < LOD1).
    let (lod_buf, buf_start, group, stride) = if let Some(ref ch) = chunks {
        let matched = match_chunks_to_groups(ch, &lod_groups);
        if lod_idx < matched.len() {
            if let Some((grp_idx, s)) = matched[lod_idx] {
                (ch[lod_idx].as_slice(), 0usize, &lod_groups[grp_idx], s)
            } else {
                // Fallback: use unmatched chunk with its group
                let grp = &lod_groups[lod_idx];
                let tv: usize = grp.iter().map(|e| e.nv).sum();
                let ti: usize = grp.iter().map(|e| e.ni).sum();
                let chunk_slice = ch[lod_idx].as_slice();
                let gd = chunk_slice.len();
                let s = match detect_stride(chunk_slice, 0, tv, ti, gd) {
                    Some(s) => s,
                    None => return Ok(ParsedMesh { path: filename.into(), format: "pamlod".into(), ..Default::default() }),
                };
                (chunk_slice, 0, grp, s)
            }
        } else {
            return Ok(ParsedMesh { path: filename.into(), format: "pamlod".into(), ..Default::default() });
        }
    } else {
        let grp = &lod_groups[lod_idx];
        let tv: usize = grp.iter().map(|e| e.nv).sum();
        let ti: usize = grp.iter().map(|e| e.ni).sum();
        let gd = seq_alg_stride.map(|s| tv * s + ti * 2).unwrap_or(0);
        let s = match detect_stride(data, geom_off, tv, ti, gd) {
            Some(s) => s,
            None => return Ok(ParsedMesh { path: filename.into(), format: "pamlod".into(), ..Default::default() }),
        };
        (data, geom_off, grp, s)
    };

    let total_v: usize = group.iter().map(|e| e.nv).sum();
    let total_i: usize = group.iter().map(|e| e.ni).sum();
    let idx_base_in_buf = buf_start + total_v * stride;
    let has_uv = stride >= 12;

    let mut all_verts: Vec<[f32; 3]> = Vec::new();
    let mut all_uvs:   Vec<[f32; 2]> = Vec::new();
    let mut all_faces: Vec<[u32; 3]> = Vec::new();
    let mut vert_offset: usize = 0;

    for e in group {
        let vert_base = buf_start + e.voff * stride;
        let idx_off   = idx_base_in_buf + e.ioff * 2;
        if idx_off + e.ni * 2 > lod_buf.len() { continue; }

        let indices: Vec<usize> = (0..e.ni)
            .map(|j| u16::from_le_bytes(
                lod_buf[idx_off+j*2..idx_off+j*2+2].try_into().unwrap()
            ) as usize)
            .collect();

        let mut unique: Vec<usize> = indices.iter().copied()
            .collect::<HashSet<_>>().into_iter().collect();
        unique.sort_unstable();
        let idx_map: HashMap<usize, usize> = unique.iter().enumerate()
            .map(|(li, &gi)| (gi, li + vert_offset)).collect();

        let (verts, uvs) = extract_verts(lod_buf, vert_base, stride, &unique, bbox_min, bbox_max, has_uv);
        let faces        = extract_faces(&indices, &idx_map);

        vert_offset += verts.len();
        all_verts.extend(verts);
        all_uvs.extend(uvs);
        all_faces.extend(faces);
    }

    let mat_name = group.first().map(|e| e.mat.as_str()).unwrap_or("lod");
    let normals  = compute_smooth_normals(&all_verts, &all_faces);
    let total_v  = all_verts.len();
    let total_f  = all_faces.len();

    let sm = SubMesh {
        name:         format!("lod{lod_idx:02}_{mat_name}"),
        material:     mat_name.to_string(),
        texture:      group.first().map(|e| e.tex.clone()).unwrap_or_default(),
        vertices:     all_verts,
        uvs:          all_uvs,
        normals,
        faces:        all_faces,
        vertex_count: total_v,
        face_count:   total_f,
    };

    let mut result = ParsedMesh {
        path:      filename.into(),
        format:    "pamlod".into(),
        bbox_min,
        bbox_max,
        ..Default::default()
    };
    if total_v > 0 {
        result.submeshes.push(sm);
    }
    result.total_vertices = total_v;
    result.total_faces    = total_f;
    result.has_uvs        = !result.submeshes.iter().all(|s| s.uvs.is_empty());
    Ok(result)
}

// ── Chunk ↔ group matching ────────────────────────────────────────────

/// Match each per-LOD chunk to the lod_group whose geometry size equals
/// `len(chunk)` at some stride.  Returns indices into `groups`.
///
/// Strides are tried in descending order so that the larger (higher-quality)
/// stride wins when two (group, stride) pairs produce the same chunk size.
fn match_chunks_to_groups(
    chunks: &[Vec<u8>],
    groups: &[Vec<LodEntry>],
) -> Vec<Option<(usize, usize)>> {
    let mut matched: Vec<Option<(usize, usize)>> = vec![None; chunks.len()];
    let mut used = vec![false; groups.len()];

    for (k, chunk) in chunks.iter().enumerate() {
        let s = chunk.len();
        'outer: for &stride in super::pam::STRIDE_CANDIDATES.iter().rev() {
            for (gi, grp) in groups.iter().enumerate() {
                if used[gi] { continue; }
                let tv: usize = grp.iter().map(|e| e.nv).sum();
                let ti: usize = grp.iter().map(|e| e.ni).sum();
                if tv * stride + ti * 2 != s { continue; }
                // Validate that indices at idx_base are all < tv
                let idx_base = tv * stride;
                let probe = ti.min(100);
                if idx_base + probe * 2 > chunk.len() { continue; }
                let valid = (0..probe).all(|j| {
                    let v = u16::from_le_bytes(
                        chunk[idx_base+j*2..idx_base+j*2+2].try_into().unwrap()
                    ) as usize;
                    v < tv
                });
                if valid {
                    matched[k] = Some((gi, stride));
                    used[gi]   = true;
                    break 'outer;
                }
            }
        }
    }
    matched
}

// ── LOD entry scanning ────────────────────────────────────────────────

/// Per-submesh descriptor recovered from the PAMLOD entry table.
struct LodEntry {
    nv:   usize,   // vertex count
    ni:   usize,   // index count
    voff: usize,   // vertex offset (element units in combined buffer)
    ioff: usize,   // index offset  (element units)
    tex:  String,
    mat:  String,
    /// byte offset of the texture string in the file (used for sorting)
    tex_pos: usize,
}

/// Scan the header region for submesh descriptors, then group them into LODs.
///
/// Each submesh descriptor is found by locating texture strings ending in
/// `dds\0`.  Most files use a full path (`name.dds\0`); some assets (caves,
/// large composites) store just `dds\0` with no prefix.  nv/ni/voff/ioff are
/// read from fixed offsets relative to the string start.  Entries are grouped
/// into LOD levels by sequential voff/ioff accumulation.
fn scan_lod_groups(data: &[u8], geom_off: usize, lod_count: usize) -> Vec<Vec<LodEntry>> {
    if geom_off <= ENTRY_TABLE { return vec![]; }
    let region = &data[ENTRY_TABLE..geom_off];

    // Search for `dds\0`; the name start is found by scanning backward to the
    // nearest preceding null byte (handles both "name.dds\0" and bare "dds\0").
    let needle = b"dds\0";
    let mut raw_entries: Vec<LodEntry> = Vec::new();

    let mut pos = 0usize;
    while pos + needle.len() <= region.len() {
        if &region[pos..pos+needle.len()] != needle {
            pos += 1;
            continue;
        }
        // tex_start is the beginning of the texture name string, not the `.dds` suffix.
        // Scan backward for the start of the non-null run.
        // nv/ni are 0x10 bytes before the string start — but we need the start.
        // The Python parser finds the DDS *end* (including null), so tex_start
        // = ENTRY_TABLE + m.start() where m.start() is the start of the match
        // including leading chars.  We need to find where the name starts.
        // Search backward for null or beginning of region.
        let mut name_start = pos;
        while name_start > 0 && region[name_start - 1] != 0 { name_start -= 1; }
        let tex_start_abs = ENTRY_TABLE + name_start;
        let nv_off = tex_start_abs.wrapping_sub(0x10);
        if nv_off < ENTRY_TABLE || nv_off + 8 > geom_off { pos += 1; continue; }

        let nv   = u32::from_le_bytes(data[nv_off..nv_off+4].try_into().unwrap())    as usize;
        let ni   = u32::from_le_bytes(data[nv_off+4..nv_off+8].try_into().unwrap())  as usize;
        if nv == 0 || nv > 131072 || ni == 0 || ni % 3 != 0 { pos += 1; continue; }

        let voff_off = tex_start_abs.wrapping_sub(8);
        let ioff_off = tex_start_abs.wrapping_sub(4);
        if voff_off < ENTRY_TABLE || ioff_off < ENTRY_TABLE { pos += 1; continue; }
        let voff = u32::from_le_bytes(data[voff_off..voff_off+4].try_into().unwrap()) as usize;
        let ioff = u32::from_le_bytes(data[ioff_off..ioff_off+4].try_into().unwrap()) as usize;

        // Material names also end in "dds\0" and produce false-positive matches.
        // Their voff values are nonsensical (exceed the geometry section size);
        // filtering voff*6 > geom_size eliminates them without any brittle
        // position arithmetic.
        let geom_size = data.len() - geom_off;
        if voff.saturating_mul(6) > geom_size { pos += 1; continue; }

        let tex = cstr(&data[tex_start_abs..], 256);
        let mat_off = tex_start_abs + 0x100;
        let mat = if mat_off + 4 < geom_off { cstr(&data[mat_off..], 256) } else { String::new() };

        raw_entries.push(LodEntry { nv, ni, voff, ioff, tex, mat, tex_pos: tex_start_abs });
        pos += needle.len();
    }

    raw_entries.sort_by_key(|e| e.tex_pos);

    // Group into LOD levels by tracking running voff/ioff accumulator.
    let mut groups: Vec<Vec<LodEntry>> = Vec::new();
    let mut cur_group: Vec<LodEntry>   = Vec::new();
    let (mut ve_acc, mut ie_acc) = (0usize, 0usize);

    for e in raw_entries {
        if e.voff == ve_acc && e.ioff == ie_acc {
            ve_acc += e.nv;
            ie_acc += e.ni;
            cur_group.push(e);
        } else {
            if !cur_group.is_empty() { groups.push(cur_group); }
            ve_acc = e.nv;
            ie_acc = e.ni;
            cur_group = vec![e];
        }
    }
    if !cur_group.is_empty() { groups.push(cur_group); }

    groups.truncate(lod_count);
    groups
}

// ── Per-LOD geometry table decoder ───────────────────────────────────

/// Read and optionally decompress per-LOD geometry chunks.
///
/// Returns `Some(chunks)` when the table is valid and at least one LOD is
/// LZ4-compressed.  Returns `None` to fall back to the sequential scan.
fn get_lod_chunks(data: &[u8], geom_off: usize, lod_count: usize) -> Option<Vec<Vec<u8>>> {
    let n_entries = lod_count + 1;
    let table_base = geom_off.checked_sub(n_entries * 12)?;

    let mut entries: Vec<(u32, u32, u32)> = Vec::with_capacity(n_entries);
    for i in 0..n_entries {
        let off = table_base + i * 12;
        if off + 12 > data.len() { return None; }
        let f1 = u32::from_le_bytes(data[off..off+4].try_into().unwrap());
        let f2 = u32::from_le_bytes(data[off+4..off+8].try_into().unwrap());
        let f3 = u32::from_le_bytes(data[off+8..off+12].try_into().unwrap());
        entries.push((f1, f2, f3));
    }

    // Format A/C: find the entry whose f1 == geom_off (LOD 0 start pointer).
    let lod0_idx = entries.iter().position(|&(f1, _, _)| f1 as usize == geom_off);
    if let Some(start) = lod0_idx {
        if start + lod_count <= entries.len() {
            let lod_entries = &entries[start..start + lod_count];
            let has_compressed = lod_entries.iter().any(|&(_, _, cs)| cs > 0);
            if !has_compressed { return None; }
            return read_direct_chunks(data, lod_entries);
        }
    }

    // Format B: entries[0].f3 == geom_off anchors the geometry start.
    if entries[0].2 as usize == geom_off {
        let has_data = (0..lod_count).any(|k| entries[k + 1].0 > 0);
        if !has_data { return None; }
        return read_end_offset_chunks(data, &entries, lod_count);
    }

    // Format D — entries[k] = [lz4_size_of_prev, start_offset, decomp_size].
    // entries[0].f2 == geom_off; LZ4 block size for LOD k is entries[k+1].f1.
    // e.g. cd_aka_house_module_b_roof_0002.pamlod
    if entries[0].1 as usize == geom_off {
        let has_data = (0..lod_count).any(|k| entries[k].2 > 0);
        if !has_data { return None; }
        // For all-raw Format D, validate decomp sum ≈ geom_size to avoid
        // false positives.  Compressed tables have all_decomp >> geom_size,
        // so skip this check when any LZ4 entry is present.
        let has_lz4 = (0..lod_count).any(|k| entries[k + 1].0 > 0);
        if !has_lz4 {
            let all_decomp: usize = (0..lod_count).map(|k| entries[k].2 as usize).sum();
            let geom_size = data.len().saturating_sub(geom_off);
            if all_decomp.abs_diff(geom_size) > 32 * lod_count { return None; }
        }
        let mut chunks = Vec::with_capacity(lod_count);
        for k in 0..lod_count {
            let start  = entries[k].1 as usize;   // f2 = section start
            let decomp = entries[k].2 as usize;   // f3 = decompressed size
            let lz4    = entries[k + 1].0 as usize; // next entry's f1 = lz4 block size
            chunks.push(read_chunk(data, start, decomp, lz4)?);
        }
        return Some(chunks);
    }

    None
}

/// Format A/C: each entry = [start_offset, decomp_size, lz4_size].
fn read_direct_chunks(data: &[u8], entries: &[(u32, u32, u32)]) -> Option<Vec<Vec<u8>>> {
    let mut chunks = Vec::with_capacity(entries.len());
    for &(start, decomp, comp) in entries {
        chunks.push(read_chunk(data, start as usize, decomp as usize, comp as usize)?);
    }
    Some(chunks)
}

/// Format B: entries[k].f3 = section start; entries[k+1].f1/f2 = decomp/lz4.
fn read_end_offset_chunks(
    data: &[u8],
    entries: &[(u32, u32, u32)],
    lod_count: usize,
) -> Option<Vec<Vec<u8>>> {
    let mut chunks = Vec::with_capacity(lod_count);
    for k in 0..lod_count {
        let start  = entries[k].2 as usize;
        let decomp = entries[k + 1].0 as usize;
        let lz4    = entries[k + 1].1 as usize;
        chunks.push(read_chunk(data, start, decomp, lz4)?);
    }
    Some(chunks)
}

/// Decompress or slice one LOD's geometry data.
///
/// `comp=0` means raw: return `data[start..start+decomp]`.
/// `comp>0` means LZ4: decompress `data[start..start+comp]` to `decomp` bytes.
fn read_chunk(data: &[u8], start: usize, decomp: usize, comp: usize) -> Option<Vec<u8>> {
    if start >= data.len() || decomp == 0 { return None; }
    if comp > 0 {
        if comp >= decomp || start + comp > data.len() { return None; }
        crate::compression::decompress(
            &data[start..start + comp],
            decomp,
            crate::compression::COMP_LZ4,
        ).ok()
    } else {
        if start + decomp > data.len() { return None; }
        Some(data[start..start + decomp].to_vec())
    }
}
