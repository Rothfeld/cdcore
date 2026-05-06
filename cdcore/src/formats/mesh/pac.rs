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

/// Parse a decompressed PAC skinned mesh file.
///
/// Two-tier layout: the shipping PAC format stores per-submesh descriptors in
/// section 0 + per-LOD geometry sections (1..=n_lods). When the section table
/// is missing or descriptors don't match (small or non-standard files), we
/// fall back to the simpler PAM-style layout via `parse_pam_style`. Mirrors
/// `core/mesh_parser.py::parse_pac` line-for-line on the descriptor path.
pub fn parse(data: &[u8], filename: &str) -> Result<ParsedPac> {
    if data.len() < 0x40 || &data[..4] != PAR_MAGIC {
        return Err(ParseError::magic(PAR_MAGIC, &data[..4.min(data.len())], 0));
    }

    if let Some(parsed) = parse_with_descriptors(data, filename) {
        return Ok(parsed);
    }
    parse_pam_style(data, filename)
}

/// Attempt the descriptor-driven layout used by every shipping character PAC.
/// Returns None when the section table is missing, descriptors don't anchor,
/// or the geometry section's vertex/index split can't be solved -- in which
/// case the caller falls back to `parse_pam_style`.
fn parse_with_descriptors(data: &[u8], filename: &str) -> Option<ParsedPac> {
    let sections = parse_par_sections(data);
    let sec0 = sections.iter().find(|s| s.index == 0)?;
    if sec0.size < 5 {
        return None;
    }
    let n_lods = data[sec0.offset + 4] as usize;
    if n_lods == 0 || n_lods > 10 {
        return None;
    }
    let descriptors = find_pac_descriptors(data, sec0.offset, sec0.size, n_lods);
    if descriptors.is_empty() {
        return None;
    }

    // Pick the highest-numbered LOD section the file actually contains; the
    // shipping convention is sec_idx = n_lods + 1 - lod_idx (LOD0 -> sec n_lods).
    // Try [4, 3, 2, 1] in order matching Python's preference.
    let geom_idx = [4usize, 3, 2, 1].into_iter().find(|i| sections.iter().any(|s| s.index == *i))?;
    let geom_sec = sections.iter().find(|s| s.index == geom_idx).copied()?;
    let lod = 4 - geom_idx;

    let preliminary_split = geom_sec.offset + geom_sec.size;
    let stride = detect_pac_vertex_stride(data, geom_sec.offset, preliminary_split);

    let total_indices: usize = descriptors
        .iter()
        .map(|d| d.index_counts.get(lod).copied().unwrap_or(0) as usize)
        .sum();

    let (vert_base, mut idx_byte_offset) = find_pac_section_layout(
        data, &geom_sec, &descriptors, lod, total_indices, stride,
    );
    let index_region_start = idx_byte_offset;

    // Per-descriptor vertex offset within the section (relative to sec_off).
    let mut desc_vert_offsets: Vec<usize> = Vec::with_capacity(descriptors.len());
    let mut vert_cursor = vert_base;
    for d in &descriptors {
        desc_vert_offsets.push(vert_cursor);
        vert_cursor += (d.vertex_counts.get(lod).copied().unwrap_or(0) as usize) * stride;
    }

    let bbox_min_init = descriptors[0].bbox_min;
    let bbox_max_init = [
        bbox_min_init[0] + descriptors[0].bbox_extent[0],
        bbox_min_init[1] + descriptors[0].bbox_extent[1],
        bbox_min_init[2] + descriptors[0].bbox_extent[2],
    ];
    let mut bbox_min = bbox_min_init;
    let mut bbox_max = bbox_max_init;
    let mut submeshes: Vec<PacSubMesh> = Vec::with_capacity(descriptors.len());
    let mut has_bones_overall = false;

    for (di, desc) in descriptors.iter().enumerate() {
        let vc = desc.vertex_counts.get(lod).copied().unwrap_or(0) as usize;
        let ic = desc.index_counts.get(lod).copied().unwrap_or(0) as usize;
        if vc == 0 && ic == 0 {
            continue;
        }
        let indices = read_pac_indices(data, geom_sec.offset, geom_sec.size, idx_byte_offset, ic);

        // Determine which descriptor this submesh's vertices actually live
        // under: real PACs occasionally route a submesh's index buffer to a
        // sibling descriptor's vertex range (multi-material rendering off
        // shared geometry). If max(idx) >= our vc, look for a sibling whose
        // vc accommodates it.
        let max_idx = indices.iter().copied().max().unwrap_or(0);
        let mut vertex_owner_idx = di;
        let mut owner_vc = vc;
        if !indices.is_empty() && max_idx >= vc {
            let partner = descriptors.iter().enumerate().find(|(pj, partner)| {
                *pj != di && (partner.vertex_counts.get(lod).copied().unwrap_or(0) as usize) > max_idx
            });
            if let Some((pj, partner)) = partner {
                vertex_owner_idx = pj;
                owner_vc = partner.vertex_counts.get(lod).copied().unwrap_or(0) as usize;
            } else {
                let available_vc = (index_region_start.saturating_sub(desc_vert_offsets[di])) / stride.max(1);
                if max_idx < available_vc {
                    owner_vc = max_idx + 1;
                }
            }
        }

        let vertex_start = desc_vert_offsets[vertex_owner_idx];
        let mut verts: Vec<[f32; 3]> = Vec::with_capacity(owner_vc);
        let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(owner_vc);
        let mut normals: Vec<[f32; 3]> = Vec::with_capacity(owner_vc);
        let mut source_offsets: Vec<i64> = Vec::with_capacity(owner_vc);
        let mut bone_verts: Vec<BoneVertex> = Vec::with_capacity(owner_vc);
        let mut bone_indices_per_vert: Vec<Vec<u32>> = Vec::with_capacity(owner_vc);
        let mut bone_weights_per_vert: Vec<Vec<f32>> = Vec::with_capacity(owner_vc);

        for vi in 0..owner_vc {
            let rec_off = geom_sec.offset + vertex_start + vi * stride;
            if rec_off + stride > data.len() {
                break;
            }
            let (pos, uv, normal, bones, weights) = decode_pac_vertex_record(data, rec_off, desc, stride);
            verts.push(pos);
            uvs.push(uv);
            normals.push(normal);
            source_offsets.push(rec_off as i64);
            // Pack into the legacy BoneVertex structure (4-slot layout) so
            // existing in-place patching code keeps working on these PACs.
            let mut bi4 = [0u8; 4];
            let mut bw4 = [0f32; 4];
            for (i, (&b, &w)) in bones.iter().zip(weights.iter()).take(4).enumerate() {
                bi4[i] = (b & 0xFF) as u8;
                bw4[i] = w;
            }
            bone_verts.push(BoneVertex { bone_indices: bi4, bone_weights: bw4 });
            bone_indices_per_vert.push(bones.iter().map(|&b| b as u32).collect());
            bone_weights_per_vert.push(weights);
        }

        // Build face list: discard triangles whose indices fall outside the
        // owner vertex set (matches Python's tolerant strip).
        let mut faces: Vec<[u32; 3]> = Vec::with_capacity(indices.len() / 3);
        let mut i = 0;
        while i + 2 < indices.len() {
            let (a, b, c) = (indices[i], indices[i + 1], indices[i + 2]);
            if a < verts.len() && b < verts.len() && c < verts.len() {
                faces.push([a as u32, b as u32, c as u32]);
            }
            i += 3;
        }

        // Use parser-recovered normals when present; else compute smooth.
        let normals_final = if normals.iter().all(|n| n[0] == 0.0 && n[1] == 1.0 && n[2] == 0.0) {
            super::pam::compute_smooth_normals(&verts, &faces)
        } else {
            normals
        };

        let bbox_max_d = [
            desc.bbox_min[0] + desc.bbox_extent[0],
            desc.bbox_min[1] + desc.bbox_extent[1],
            desc.bbox_min[2] + desc.bbox_extent[2],
        ];
        if submeshes.is_empty() {
            bbox_min = desc.bbox_min;
            bbox_max = bbox_max_d;
        } else {
            for k in 0..3 {
                if desc.bbox_min[k] < bbox_min[k] { bbox_min[k] = desc.bbox_min[k]; }
                if bbox_max_d[k] > bbox_max[k] { bbox_max[k] = bbox_max_d[k]; }
            }
        }

        let has_any_bones = bone_indices_per_vert.iter().any(|b| !b.is_empty());
        has_bones_overall |= has_any_bones;

        let vertex_count = verts.len();
        let face_count = faces.len();
        submeshes.push(PacSubMesh {
            base: SubMesh {
                name: desc.name.clone(),
                material: desc.material.clone(),
                texture: String::new(),
                vertex_count,
                face_count,
                vertices: verts,
                uvs,
                normals: normals_final,
                faces,
                bone_indices: bone_indices_per_vert,
                bone_weights: bone_weights_per_vert,
                source_vertex_offsets: source_offsets,
                source_index_offset: (geom_sec.offset + idx_byte_offset) as i64,
                source_index_count: indices.len(),
                source_vertex_stride: stride,
                source_descriptor_offset: desc.descriptor_offset as i64,
                source_bbox_min: desc.bbox_min,
                source_bbox_extent: desc.bbox_extent,
                source_lod_count: desc.stored_lod_count,
                ..Default::default()
            },
            bone_vertices: bone_verts,
        });

        idx_byte_offset += ic * 2;
    }

    if submeshes.is_empty() {
        return None;
    }

    let total_vertices = submeshes.iter().map(|s| s.base.vertices.len()).sum();
    let total_faces = submeshes.iter().map(|s| s.base.faces.len()).sum();
    let has_uvs = submeshes.iter().any(|s| !s.base.uvs.is_empty());

    log::info!(
        "Parsed PAC {}: {} submeshes, {} verts, {} faces",
        filename, submeshes.len(), total_vertices, total_faces,
    );

    Some(ParsedPac {
        path: filename.to_string(),
        bbox_min, bbox_max,
        submeshes,
        total_vertices, total_faces,
        has_uvs, has_bones: has_bones_overall,
    })
}

/// Legacy PAM-style layout fallback for PACs the descriptor parser can't
/// resolve (no section 0, malformed descriptors, or non-standard layouts).
/// Empty result is preferred over a parse error so virtual-FBX readers see
/// a `ParsedPac` with 0 submeshes rather than a hard failure.
fn parse_pam_style(data: &[u8], filename: &str) -> Result<ParsedPac> {
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

            // Bone indices and weights (if stride large enough -- typically at +16)
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

        // Per-vertex byte offsets in the original blob and the descriptor /
        // index offsets that the writer uses for in-place patching.
        let source_vertex_offsets: Vec<i64> =
            unique.iter().map(|&gi| (vert_base + gi * stride) as i64).collect();

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
                source_vertex_offsets,
                source_index_offset: idx_off as i64,
                source_index_count: ni,
                source_vertex_stride: stride,
                source_descriptor_offset: off as i64,
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

/// Detect PAC vertex record stride by counting hits of the constant marker
/// `0x3C000000` at byte offset +12 of each sampled record. The sample window
/// is bounded by `[vert_start, split_off)` so this works equally well on
/// LOD-section starts and full-file scans. Mirrors Python
/// `_detect_pac_vertex_stride` (defaults to 40 on ties / no hits).
pub fn detect_pac_vertex_stride(data: &[u8], vert_start: usize, split_off: usize) -> usize {
    const MARKER: u32 = 0x3C00_0000;
    let region_size = split_off.saturating_sub(vert_start);
    if region_size == 0 {
        return 40;
    }
    let candidates = [40usize, 36, 32, 44, 48, 52, 56, 60, 64, 28, 24, 20, 16, 12, 8, 6];
    let mut best_stride = 40usize;
    let mut best_hits: i32 = -1;
    for &stride in &candidates {
        let sample = (region_size / stride.max(1)).min(64);
        if sample < 4 {
            continue;
        }
        let mut hits: i32 = 0;
        for i in 0..sample {
            let rec_off = vert_start + i * stride;
            if rec_off + 16 > split_off {
                break;
            }
            if rec_off + 16 > data.len() {
                break;
            }
            let val = u32::from_le_bytes(data[rec_off + 12..rec_off + 16].try_into().unwrap());
            if val == MARKER {
                hits += 1;
            }
        }
        // Tie-break toward stride 40 (the shipping standard).
        if hits > best_hits
            || (hits == best_hits && (stride as i32 - 40).abs() < (best_stride as i32 - 40).abs())
        {
            best_stride = stride;
            best_hits = hits;
        }
    }
    best_stride
}

/// Read `index_count` u16 indices from `section_offset + index_start`,
/// clamping at the section bound. Mirrors `_read_pac_indices`.
pub fn read_pac_indices(
    data: &[u8],
    section_offset: usize,
    section_size: usize,
    index_start: usize,
    index_count: usize,
) -> Vec<usize> {
    if index_count == 0 {
        return Vec::new();
    }
    let max_count = index_count.min(section_size.saturating_sub(index_start) / 2);
    let base = section_offset + index_start;
    (0..max_count)
        .map(|i| {
            let p = base + i * 2;
            u16::from_le_bytes(data[p..p + 2].try_into().unwrap()) as usize
        })
        .collect()
}

/// Decode a single PAC vertex record (position + UV + normal + skin slots).
/// Returns (pos, uv, normal, bone_indices, bone_weights). Bone count varies
/// per record (0..4 active slots after filtering 0xFF / zero-weight); the
/// caller is responsible for merging into a fixed-arity layout if needed.
fn decode_pac_vertex_record(
    data: &[u8],
    rec_off: usize,
    desc: &PacDescriptor,
    stride: usize,
) -> ([f32; 3], [f32; 2], [f32; 3], Vec<u8>, Vec<f32>) {
    // Position (u16 quantized).
    let xu = u16::from_le_bytes(data[rec_off..rec_off + 2].try_into().unwrap());
    let yu = u16::from_le_bytes(data[rec_off + 2..rec_off + 4].try_into().unwrap());
    let zu = u16::from_le_bytes(data[rec_off + 4..rec_off + 6].try_into().unwrap());
    let pos = [
        decode_pac_position_u16(xu, desc.bbox_min[0], desc.bbox_extent[0]),
        decode_pac_position_u16(yu, desc.bbox_min[1], desc.bbox_extent[1]),
        decode_pac_position_u16(zu, desc.bbox_min[2], desc.bbox_extent[2]),
    ];

    // UV (f16 pair). Records shorter than 12 bytes (rare) get UV=0,0.
    let uv = if rec_off + 12 <= data.len() {
        let u = f16::from_le_bytes(data[rec_off + 8..rec_off + 10].try_into().unwrap()).to_f32();
        let v = f16::from_le_bytes(data[rec_off + 10..rec_off + 12].try_into().unwrap()).to_f32();
        if u.is_nan() || v.is_nan() { [0.0, 0.0] } else { [u, v] }
    } else {
        [0.0, 0.0]
    };

    // Normal at +16 (10:10:10 packed). Python: nx = ny_raw, ny = nz_raw, nz = nx_raw
    // (the raw bit positions are interleaved unintuitively); we mirror that.
    let normal = if rec_off + 20 <= data.len() {
        let packed = u32::from_le_bytes(data[rec_off + 16..rec_off + 20].try_into().unwrap());
        let nx_raw = packed & 0x3FF;
        let ny_raw = (packed >> 10) & 0x3FF;
        let nz_raw = (packed >> 20) & 0x3FF;
        let nx = (ny_raw as f32) / 511.5 - 1.0;
        let ny = (nz_raw as f32) / 511.5 - 1.0;
        let nz = (nx_raw as f32) / 511.5 - 1.0;
        [nx, ny, nz]
    } else {
        [0.0, 1.0, 0.0]
    };

    // Skin slots: bytes 28..32 are weights, 32..36 are bone palette indices.
    // Skip records whose stride doesn't include this region.
    let mut bones: Vec<u8> = Vec::new();
    let mut weights: Vec<f32> = Vec::new();
    if stride >= 36 && rec_off + 36 <= data.len() {
        let raw_weights = [
            data[rec_off + 28], data[rec_off + 29], data[rec_off + 30], data[rec_off + 31],
        ];
        let raw_slots = [
            data[rec_off + 32], data[rec_off + 33], data[rec_off + 34], data[rec_off + 35],
        ];
        let weight_sum: u32 = raw_weights.iter().map(|&w| w as u32).sum();
        let inv_sum = if weight_sum > 0 { 1.0 / (weight_sum as f32) } else { 0.0 };
        for (slot, weight) in raw_slots.iter().zip(raw_weights.iter()) {
            if *slot == 0xFF || *weight == 0 {
                continue;
            }
            let palette_idx = if (*slot as usize) < desc.palette.len() {
                desc.palette[*slot as usize]
            } else {
                *slot
            };
            bones.push(palette_idx);
            weights.push((*weight as f32) * inv_sum);
        }
    }

    (pos, uv, normal, bones, weights)
}

/// Mirrors `_decode_pac_position_u16`. Linear dequantization across the
/// descriptor's bbox extent. Zero extent collapses to bbox_min.
fn decode_pac_position_u16(v: u16, bbox_min: f32, bbox_extent: f32) -> f32 {
    if bbox_extent.abs() < 1e-8 {
        return bbox_min;
    }
    bbox_min + ((v as f32) / 32767.0) * bbox_extent
}

/// Find the (vert_start, idx_byte_offset) split inside a decompressed PAC
/// geometry section. Mirrors `_find_pac_section_layout`. The shipping format
/// usually has zero gap (all vertices first, then indices); some files have
/// a small gap of secondary vertex records that the solver treats as
/// alternative layouts and scores by triangle-edge sanity.
pub fn find_pac_section_layout(
    data: &[u8],
    geom_sec: &ParSection,
    descriptors: &[PacDescriptor],
    lod: usize,
    total_indices: usize,
    stride: usize,
) -> (usize, usize) {
    let sec_off = geom_sec.offset;
    let sec_size = geom_sec.size;
    let total_verts: usize = descriptors
        .iter()
        .map(|d| d.vertex_counts.get(lod).copied().unwrap_or(0) as usize)
        .sum();
    let primary_bytes = total_verts * stride;
    let index_bytes = total_indices * 2;

    if primary_bytes + index_bytes >= sec_size {
        return (0, primary_bytes);
    }
    let gap = sec_size - primary_bytes - index_bytes;
    if gap == 0 {
        return (0, primary_bytes);
    }

    let first_desc = match descriptors.iter().find(|d| {
        d.vertex_counts.get(lod).copied().unwrap_or(0) > 0
    }) {
        Some(d) => d,
        None => return (0, primary_bytes),
    };
    let first_vc = first_desc.vertex_counts[lod] as usize;

    let available_vertices = |v_start: usize, i_start: usize| -> usize {
        if i_start <= v_start { 0 } else { (i_start - v_start) / stride.max(1) }
    };

    // Scan forward from `after_verts` (in 2-byte steps) for the first
    // candidate index region: a triple where index 0 is 0 and indices 1,2
    // are below the first descriptor's vc.
    let scan_idx_start = |after_verts: usize| -> Option<usize> {
        let mut adj = 0usize;
        while after_verts + adj + 6 <= sec_size {
            let trial = after_verts + adj;
            let abs = sec_off + trial;
            if abs + 6 > data.len() { break; }
            let v0 = u16::from_le_bytes(data[abs..abs + 2].try_into().unwrap()) as usize;
            let v1 = u16::from_le_bytes(data[abs + 2..abs + 4].try_into().unwrap()) as usize;
            let v2 = u16::from_le_bytes(data[abs + 4..abs + 6].try_into().unwrap()) as usize;
            if v0 == 0 && v1 < first_vc && v2 < first_vc {
                return Some(trial);
            }
            adj += 2;
        }
        None
    };

    let measure_quality = |v_start: usize, i_start_opt: Option<usize>| -> f64 {
        let i_start = match i_start_opt {
            Some(v) => v,
            None => return f64::INFINITY,
        };
        if i_start + total_indices * 2 > sec_size {
            return f64::INFINITY;
        }

        let first_ic = descriptors
            .iter()
            .find(|d| d.index_counts.get(lod).copied().unwrap_or(0) > 0)
            .map(|d| d.index_counts[lod] as usize)
            .unwrap_or(0);
        let n_tris = first_ic / 3;
        if n_tris == 0 {
            return 0.0;
        }

        let sample_step = (n_tris / 30).max(1);
        let mut sample_set: std::collections::BTreeSet<usize> = (0..n_tris.min(12)).collect();
        let mut k = 0;
        while k < n_tris {
            sample_set.insert(k);
            k += sample_step;
        }

        let mut sample_tris: Vec<(usize, usize, usize)> = Vec::new();
        let mut sample_max_idx: i64 = -1;
        for &tri_idx in &sample_set {
            let idx_base = sec_off + i_start + tri_idx * 6;
            if idx_base + 6 > data.len() {
                return f64::INFINITY;
            }
            let i0 = u16::from_le_bytes(data[idx_base..idx_base + 2].try_into().unwrap()) as usize;
            let i1 = u16::from_le_bytes(data[idx_base + 2..idx_base + 4].try_into().unwrap()) as usize;
            let i2 = u16::from_le_bytes(data[idx_base + 4..idx_base + 6].try_into().unwrap()) as usize;
            sample_tris.push((i0, i1, i2));
            sample_max_idx = sample_max_idx.max(i0 as i64).max(i1 as i64).max(i2 as i64);
        }

        let needed_vc = (first_vc as i64).max(sample_max_idx + 1) as usize;
        if needed_vc == 0 || needed_vc > available_vertices(v_start, i_start) {
            return f64::INFINITY;
        }

        let mut preview_positions: Vec<[f32; 3]> = Vec::with_capacity(needed_vc);
        for i in 0..needed_vc {
            let rec_off = sec_off + v_start + i * stride;
            if rec_off + stride > data.len() {
                return f64::INFINITY;
            }
            let xu = u16::from_le_bytes(data[rec_off..rec_off + 2].try_into().unwrap());
            let yu = u16::from_le_bytes(data[rec_off + 2..rec_off + 4].try_into().unwrap());
            let zu = u16::from_le_bytes(data[rec_off + 4..rec_off + 6].try_into().unwrap());
            preview_positions.push([
                decode_pac_position_u16(xu, first_desc.bbox_min[0], first_desc.bbox_extent[0]),
                decode_pac_position_u16(yu, first_desc.bbox_min[1], first_desc.bbox_extent[1]),
                decode_pac_position_u16(zu, first_desc.bbox_min[2], first_desc.bbox_extent[2]),
            ]);
        }

        let mut total_edge = 0.0f64;
        for (i0, i1, i2) in &sample_tris {
            let (a, b, c) = (*i0, *i1, *i2);
            if a.max(b).max(c) >= preview_positions.len() {
                return f64::INFINITY;
            }
            let p0 = preview_positions[a];
            let p1 = preview_positions[b];
            let p2 = preview_positions[c];
            let dist = |a: [f32; 3], b: [f32; 3]| -> f64 {
                let dx = (a[0] - b[0]) as f64;
                let dy = (a[1] - b[1]) as f64;
                let dz = (a[2] - b[2]) as f64;
                (dx * dx + dy * dy + dz * dz).sqrt()
            };
            let e0 = dist(p0, p1);
            let e1 = dist(p1, p2);
            let e2 = dist(p2, p0);
            total_edge += e0.max(e1).max(e2);
        }
        total_edge
    };

    let secondary_bytes = (gap / stride.max(1)) * stride;
    let mut best_v_start = 0usize;
    let mut best_i_start = primary_bytes + secondary_bytes;
    let mut best_quality = measure_quality(best_v_start, Some(best_i_start));

    let mut n_secondary = 0usize;
    while n_secondary <= gap / stride.max(1) {
        let v_start = n_secondary * stride;
        let all_verts_end = v_start + primary_bytes;
        if all_verts_end >= sec_size {
            break;
        }
        if let Some(idx_start) = scan_idx_start(all_verts_end) {
            if idx_start + total_indices * 2 <= sec_size {
                let q = measure_quality(v_start, Some(idx_start));
                if q < best_quality {
                    best_quality = q;
                    best_v_start = v_start;
                    best_i_start = idx_start;
                }
            }
        }
        n_secondary += 1;
    }

    (best_v_start, best_i_start)
}

fn dequant(v: u16, mn: f32, mx: f32) -> f32 {
    mn + (v as f32 / 65535.0) * (mx - mn)
}

fn nul_str(data: &[u8], max: usize) -> String {
    let end = max.min(data.len());
    let nul = data[..end].iter().position(|&b| b == 0).unwrap_or(end);
    String::from_utf8_lossy(&data[..nul]).into_owned()
}

// PAR section table + PAC descriptor recovery -- mirrors mesh_parser.py
// helpers `_parse_par_sections` and `_find_pac_descriptors`. Used by the
// repack/mesh full-rebuild path which re-parses descriptors from the
// original blob instead of carrying them through ParsedMesh.

#[derive(Debug, Clone, Copy)]
pub struct ParSection {
    pub index: usize,
    pub offset: usize,
    pub size: usize,
}

#[derive(Debug, Clone)]
pub struct PacDescriptor {
    pub name: String,
    pub material: String,
    pub bbox_min: [f32; 3],
    pub bbox_extent: [f32; 3],
    pub vertex_counts: Vec<u16>,
    pub index_counts: Vec<u32>,
    pub palette: Vec<u8>,
    /// Absolute byte offset into the file where descriptor begins.
    pub descriptor_offset: usize,
    pub stored_lod_count: usize,
}

/// Parse the 8-slot PAR section table at offset 0x10. Slot stride is 8 bytes
/// (comp_size, decomp_size); slots with decomp_size=0 are skipped. Returns
/// the absolute file offset + decompressed size for each populated slot.
/// Returns an empty vector for non-PAR data.
pub fn parse_par_sections(data: &[u8]) -> Vec<ParSection> {
    if data.len() < 0x50 || &data[..4] != PAR_MAGIC {
        return Vec::new();
    }
    let mut sections = Vec::new();
    let mut offset = 0x50usize;
    for i in 0..8 {
        let slot_off = 0x10 + i * 8;
        let comp_size = u32::from_le_bytes(data[slot_off..slot_off + 4].try_into().unwrap()) as usize;
        let decomp_size = u32::from_le_bytes(data[slot_off + 4..slot_off + 8].try_into().unwrap()) as usize;
        let stored_size = if comp_size > 0 { comp_size } else { decomp_size };
        if decomp_size == 0 {
            continue;
        }
        if offset + stored_size > data.len() {
            return Vec::new();
        }
        sections.push(ParSection { index: i, offset, size: decomp_size });
        offset += stored_size;
    }
    sections
}

/// Walk back from a known descriptor body offset to recover the two
/// length-prefixed ASCII strings (name + material) preceding it. Returns
/// fallbacks `unknown_<hex>` when no valid prefix is found, matching the
/// Python helper `_find_name_strings`.
fn find_name_strings(region: &[u8], desc_start: usize) -> (String, String) {
    let mut names: Vec<String> = Vec::with_capacity(2);
    let mut cursor = desc_start;

    for _ in 0..2 {
        let mut found = false;
        for back in 1..200usize {
            if back > cursor {
                break;
            }
            let pos = cursor - back;
            let candidate_len = region[pos] as usize;
            if candidate_len == 0 || candidate_len != back - 1 {
                continue;
            }
            let name_bytes = &region[pos + 1..cursor];
            if name_bytes.is_empty() || !name_bytes.iter().all(|&c| (32..127).contains(&c)) {
                continue;
            }
            names.push(String::from_utf8_lossy(name_bytes).into_owned());
            cursor = pos;
            found = true;
            break;
        }
        if !found {
            names.push(format!("unknown_{:x}", desc_start));
        }
    }
    names.reverse();
    (names[0].clone(), names[1].clone())
}

/// Recover PAC descriptors by matching the known palette-byte patterns.
///
/// PAC section 0 contains per-submesh descriptors but no length / index
/// table for them. Each descriptor begins with a 1-byte LOD count, then a
/// palette of LOD slot indices (0,1,2,...), then a 35-byte body containing
/// the bbox floats + LOD vertex/index count tables. We anchor on the
/// palette byte sequence (varies by LOD count) and back-fill 35 bytes to
/// the descriptor start.
///
/// Mirrors `_find_pac_descriptors`. Three palette layouts are supported:
///   4 LODs: `04 00 01 02 03`
///   3 LODs: `03 00 01 01 02` (Kliff/Macduff variant)
///   3 LODs: `03 00 01 02`
///   2 LODs: `02 00 01`
/// Dedup by descriptor start so the same submesh is not emitted twice when
/// patterns overlap.
pub fn find_pac_descriptors(
    data: &[u8],
    sec0_offset: usize,
    sec0_size: usize,
    n_lods: usize,
) -> Vec<PacDescriptor> {
    let region_end = (sec0_offset + sec0_size).min(data.len());
    if region_end <= sec0_offset {
        return Vec::new();
    }
    let region = &data[sec0_offset..region_end];
    if region.is_empty() {
        return Vec::new();
    }

    let mut found: Vec<(usize, PacDescriptor)> = Vec::new();
    let mut seen_starts: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let pad_len = n_lods.max(4);

    let append_descriptor = |found: &mut Vec<(usize, PacDescriptor)>,
                                 seen_starts: &mut std::collections::HashSet<usize>,
                                 idx: usize,
                                 stored_lod_count: usize,
                                 vc_off: usize,
                                 ic_off: usize| {
        if idx < 35 {
            return;
        }
        let desc_start = idx - 35;
        if seen_starts.contains(&desc_start) {
            return;
        }
        if desc_start + ic_off + stored_lod_count * 4 > region.len() {
            return;
        }
        if region[desc_start] != 0x01 {
            return;
        }
        if desc_start + 3 + 8 * 4 > region.len() {
            return;
        }
        let mut floats = [0f32; 8];
        for (k, slot) in floats.iter_mut().enumerate() {
            let pos = desc_start + 3 + k * 4;
            *slot = f32::from_le_bytes(region[pos..pos + 4].try_into().unwrap());
        }

        let mut vert_counts: Vec<u16> = Vec::with_capacity(stored_lod_count);
        for i in 0..stored_lod_count {
            let pos = desc_start + vc_off + i * 2;
            vert_counts.push(u16::from_le_bytes(region[pos..pos + 2].try_into().unwrap()));
        }
        let mut idx_counts: Vec<u32> = Vec::with_capacity(stored_lod_count);
        for i in 0..stored_lod_count {
            let pos = desc_start + ic_off + i * 4;
            idx_counts.push(u32::from_le_bytes(region[pos..pos + 4].try_into().unwrap()));
        }

        if !vert_counts.iter().any(|&v| v > 0) {
            return;
        }
        // Python cap at 200000 verts is unreachable on u16; index-count cap stays.
        if idx_counts.iter().any(|&v| v > 20_000_000) {
            return;
        }

        let (name, material) = find_name_strings(region, desc_start);
        let palette: Vec<u8> = (idx + 1..idx + 1 + stored_lod_count)
            .filter_map(|p| region.get(p).copied())
            .collect();

        let mut padded_vc = vert_counts.clone();
        padded_vc.resize(pad_len.max(stored_lod_count), 0);
        let mut padded_ic = idx_counts.clone();
        padded_ic.resize(pad_len.max(stored_lod_count), 0);

        found.push((
            desc_start,
            PacDescriptor {
                name,
                material,
                bbox_min: [floats[2], floats[3], floats[4]],
                bbox_extent: [floats[5], floats[6], floats[7]],
                vertex_counts: padded_vc,
                index_counts: padded_ic,
                palette,
                descriptor_offset: sec0_offset + desc_start,
                stored_lod_count,
            },
        ));
        seen_starts.insert(desc_start);
    };

    let pattern_specs: &[(&[u8], usize, usize, usize, fn(&[u8], usize) -> bool)] = &[
        (&[0x04, 0x00, 0x01, 0x02, 0x03], 4, 40, 48, |_r, _i| true),
        (&[0x03, 0x00, 0x01, 0x01, 0x02], 3, 40, 46, |_r, _i| true),
        (&[0x03, 0x00, 0x01, 0x02], 3, 40, 46, |r, i| i < 1 || r[i - 1] != 0x04),
        (&[0x02, 0x00, 0x01], 2, 40, 44, |r, i| i < 1 || !matches!(r[i - 1], 0x03 | 0x04)),
    ];

    for (pattern, lod_count, vc_off, ic_off, accept) in pattern_specs {
        let mut pos = 0usize;
        while pos + pattern.len() <= region.len() {
            let Some(rel) = region[pos..].windows(pattern.len()).position(|w| w == *pattern) else {
                break;
            };
            let idx = pos + rel;
            if accept(region, idx) {
                append_descriptor(&mut found, &mut seen_starts, idx, *lod_count, *vc_off, *ic_off);
            }
            pos = idx + pattern.len();
        }
    }

    found.sort_by_key(|item| item.0);
    found.into_iter().map(|(_, d)| d).collect()
}

