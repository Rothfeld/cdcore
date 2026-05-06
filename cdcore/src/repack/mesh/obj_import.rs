//! OBJ importer for the round-trip mesh edit pipeline.
//!
//! Reads a Wavefront OBJ exported by CrimsonForge plus the optional
//! `.cfmeta.json` sidecar, returns a `ParsedMesh` ready for `build_pac`,
//! `build_pam`, or `build_pamlod`.
//!
//! Mirrors `core/mesh_importer.py::import_obj` byte-for-byte:
//!   - tolerates negative + 1-based indices
//!   - tolerates v / v/vt / v/vt/vn / v//vn corner formats
//!   - triangulates polygons by fan
//!   - flips V back (the exporter wrote `1 - v`)
//!   - parses `# source_path:` and `# source_format:` metadata comments
//!   - clones vertices when one position is referenced with mismatched
//!     UV/normal across faces, propagating bone bindings + source-vertex
//!     map onto the clones (the v1.22.3 fix that prevented exploding skin
//!     meshes after UV-seam splits)

use std::path::Path;

use crate::error::{ParseError, Result};
use crate::repack::mesh::{cfmeta, ParsedMesh, SubMesh};

/// Resolve a Wavefront OBJ index token to a zero-based usize.
///
/// Returns `Err` for value 0 (OBJ indices are 1-based; zero is invalid)
/// or unparseable input. Negative indices count back from the *current*
/// item count, which is why this takes `item_count`.
fn resolve_obj_index(raw: &str, item_count: usize) -> Result<i64> {
    let value: i64 = raw
        .parse()
        .map_err(|_| ParseError::Other(format!("invalid OBJ index {raw:?}")))?;
    if value > 0 {
        Ok(value - 1)
    } else if value < 0 {
        Ok(item_count as i64 + value)
    } else {
        Err(ParseError::Other(
            "OBJ indices are 1-based and cannot be zero".into(),
        ))
    }
}

#[derive(Debug, Default)]
struct PendingSubmesh {
    name: String,
    material: String,
    /// Each face is 3 corners, each corner = (vi, ti, ni). All indices are
    /// global (OBJ-file-wide), already 0-based and resolved.
    faces_global: Vec<[(i64, i64, i64); 3]>,
}

/// Parse an OBJ file and produce a `ParsedMesh` ready for building.
///
/// `obj_path` is used to locate the optional `<obj_path>.cfmeta.json` sidecar.
pub fn import_obj(obj_path: &Path) -> Result<ParsedMesh> {
    let bytes = std::fs::read_to_string(obj_path)
        .map_err(|e| ParseError::Other(format!("read {}: {e}", obj_path.display())))?;
    let sidecar = cfmeta::load_sidecar(obj_path);

    let mut source_path = String::new();
    let mut source_format = String::new();

    let mut all_verts: Vec<[f32; 3]> = Vec::new();
    let mut all_uvs: Vec<[f32; 2]> = Vec::new();
    let mut all_normals: Vec<[f32; 3]> = Vec::new();

    let mut submesh_list: Vec<PendingSubmesh> = Vec::new();
    let mut current = PendingSubmesh::default();
    let mut current_started = false;

    // First pass: tokenize, collect all_verts/uvs/normals + per-submesh face lists.
    for line in bytes.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# source_path:") {
            source_path = rest.trim().to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix("# source_format:") {
            source_format = rest.trim().to_string();
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let head = match parts.next() {
            Some(h) => h,
            None => continue,
        };
        let rest: Vec<&str> = parts.collect();
        match head {
            "v" if rest.len() >= 3 => {
                all_verts.push([
                    parse_f(rest[0])?,
                    parse_f(rest[1])?,
                    parse_f(rest[2])?,
                ]);
            }
            "vt" if rest.len() >= 2 => {
                all_uvs.push([parse_f(rest[0])?, 1.0 - parse_f(rest[1])?]);
            }
            "vn" if rest.len() >= 3 => {
                all_normals.push([
                    parse_f(rest[0])?,
                    parse_f(rest[1])?,
                    parse_f(rest[2])?,
                ]);
            }
            "o" => {
                if current_started && !current.faces_global.is_empty() {
                    submesh_list.push(std::mem::take(&mut current));
                }
                let name = rest
                    .first()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("submesh_{}", submesh_list.len()));
                current = PendingSubmesh { name, ..Default::default() };
                current_started = true;
            }
            "usemtl" => {
                current.material = rest.first().map(|s| s.to_string()).unwrap_or_default();
            }
            "f" if rest.len() >= 3 => {
                // Parse each corner (v, v/vt, v/vt/vn, v//vn) -> resolve to 0-based indices.
                let mut corners: Vec<(i64, i64, i64)> = Vec::with_capacity(rest.len());
                for fp in &rest {
                    let idx_strs: Vec<&str> = fp.split('/').collect();
                    let vi = resolve_obj_index(idx_strs[0], all_verts.len())?;
                    let ti = if idx_strs.len() > 1 && !idx_strs[1].is_empty() {
                        resolve_obj_index(idx_strs[1], all_uvs.len())?
                    } else {
                        -1
                    };
                    let ni = if idx_strs.len() > 2 && !idx_strs[2].is_empty() {
                        resolve_obj_index(idx_strs[2], all_normals.len())?
                    } else {
                        -1
                    };
                    corners.push((vi, ti, ni));
                }
                if corners.len() < 3 {
                    continue;
                }
                // Fan-triangulate (Blender exports quads commonly).
                for tri_idx in 1..corners.len() - 1 {
                    current.faces_global.push([
                        corners[0],
                        corners[tri_idx],
                        corners[tri_idx + 1],
                    ]);
                }
            }
            _ => {}
        }
    }
    if current_started && !current.faces_global.is_empty() {
        submesh_list.push(current);
    }

    // Second pass: count vertices/UVs/normals per submesh from the file structure
    // (vertices between successive `o` markers belong to that submesh).
    let (sm_v, sm_vt, sm_vn) = count_per_submesh(&bytes);

    // Sidecar lookup by submesh name.
    let mut sidecar_by_name: std::collections::HashMap<&str, &cfmeta::CfmetaSubmesh> =
        std::collections::HashMap::new();
    if let Some(c) = &sidecar {
        for sm in &c.submeshes {
            if !sm.name.is_empty() {
                sidecar_by_name.insert(sm.name.as_str(), sm);
            }
        }
    }

    let mut submeshes: Vec<SubMesh> = Vec::with_capacity(submesh_list.len());
    let mut v_offset: usize = 0;
    let mut vt_offset: usize = 0;
    let mut vn_offset: usize = 0;

    for (si, pending) in submesh_list.iter().enumerate() {
        let nv = sm_v.get(si).copied().unwrap_or(0);
        let nvt = sm_vt.get(si).copied().unwrap_or(0);
        let nvn = sm_vn.get(si).copied().unwrap_or(0);

        // Base arrays sized to the original exported vertex count (preserves
        // any unused vertices that the build_pac path needs to round-trip).
        let mut local_verts: Vec<[f32; 3]> = Vec::with_capacity(nv);
        let mut local_uvs:   Vec<[f32; 2]> = Vec::with_capacity(nv);
        let mut local_norms: Vec<[f32; 3]> = Vec::with_capacity(nv);
        for i in 0..nv {
            local_verts.push(*all_verts.get(v_offset + i).unwrap_or(&[0.0, 0.0, 0.0]));
            local_uvs.push(if i < nvt {
                *all_uvs.get(vt_offset + i).unwrap_or(&[0.0, 0.0])
            } else {
                [0.0, 0.0]
            });
            local_norms.push(if i < nvn {
                *all_normals.get(vn_offset + i).unwrap_or(&[0.0, 1.0, 0.0])
            } else {
                [0.0, 1.0, 0.0]
            });
        }

        // Sidecar skin data, indexed identically to local_verts initially.
        let sidecar_record = sidecar_by_name.get(pending.name.as_str()).copied();
        let mut sidecar_bi: Vec<Vec<u32>> = Vec::with_capacity(nv);
        let mut sidecar_bw: Vec<Vec<f32>> = Vec::with_capacity(nv);
        if let Some(rec) = sidecar_record {
            for i in 0..nv {
                sidecar_bi.push(rec.bone_indices.get(i).cloned().unwrap_or_default());
                sidecar_bw.push(rec.bone_weights.get(i).cloned().unwrap_or_default());
            }
        }

        let mut source_vertex_map: Vec<i64> = (0..nv as i64).collect();

        // UV / normal-aware vertex split: when the same position is referenced
        // by faces with different (UV, normal) corners, clone the slot and
        // propagate skin bindings + source-vertex back-pointer onto the clone.
        let mut assigned_uv: Vec<Option<[f32; 2]>> = vec![None; nv];
        let mut assigned_n:  Vec<Option<[f32; 3]>> = vec![None; nv];
        let mut split_map: std::collections::HashMap<(i64, i64, i64), usize> =
            std::collections::HashMap::new();

        let mut local_faces: Vec<[u32; 3]> = Vec::with_capacity(pending.faces_global.len());

        for face in &pending.faces_global {
            let mut local_corners = [0u32; 3];
            for (corner_idx, &(vi, ti, ni)) in face.iter().enumerate() {
                let local_vi = vi - v_offset as i64;
                if !(0..nv as i64).contains(&local_vi) {
                    local_corners[corner_idx] = 0;
                    continue;
                }
                let local_vi_us = local_vi as usize;
                let key = (local_vi, ti, ni);
                if let Some(&existing) = split_map.get(&key) {
                    local_corners[corner_idx] = existing as u32;
                    continue;
                }

                let uv_value: [f32; 2] = if (0..all_uvs.len() as i64).contains(&ti) {
                    all_uvs[ti as usize]
                } else {
                    *local_uvs.get(local_vi_us).unwrap_or(&[0.0, 0.0])
                };
                let normal_value: [f32; 3] = if (0..all_normals.len() as i64).contains(&ni) {
                    all_normals[ni as usize]
                } else {
                    *local_norms.get(local_vi_us).unwrap_or(&[0.0, 1.0, 0.0])
                };

                let cur_uv = assigned_uv[local_vi_us];
                let cur_n  = assigned_n[local_vi_us];
                let resolved = if cur_uv.is_none() && cur_n.is_none() {
                    assigned_uv[local_vi_us] = Some(uv_value);
                    assigned_n[local_vi_us]  = Some(normal_value);
                    local_uvs[local_vi_us]   = uv_value;
                    local_norms[local_vi_us] = normal_value;
                    local_vi_us
                } else if cur_uv == Some(uv_value) && cur_n == Some(normal_value) {
                    local_vi_us
                } else {
                    // Clone slot; propagate skin + source map.
                    let clone_idx = local_verts.len();
                    local_verts.push(local_verts[local_vi_us]);
                    local_uvs.push(uv_value);
                    local_norms.push(normal_value);
                    source_vertex_map.push(source_vertex_map[local_vi_us]);
                    if sidecar_record.is_some() {
                        sidecar_bi.push(sidecar_bi[local_vi_us].clone());
                        sidecar_bw.push(sidecar_bw[local_vi_us].clone());
                    }
                    clone_idx
                };
                split_map.insert(key, resolved);
                local_corners[corner_idx] = resolved as u32;
            }
            local_faces.push(local_corners);
        }

        let vertex_count = local_verts.len();
        let face_count = local_faces.len();
        let uvs_out = if local_uvs.len() == vertex_count { local_uvs } else { vec![] };
        let normals_out = if local_norms.len() == vertex_count { local_norms } else { vec![] };

        submeshes.push(SubMesh {
            name: pending.name.clone(),
            material: pending.material.clone(),
            texture: String::new(),
            vertices: local_verts,
            uvs: uvs_out,
            normals: normals_out,
            faces: local_faces,
            bone_indices: if sidecar_record.is_some() { sidecar_bi } else { vec![] },
            bone_weights: if sidecar_record.is_some() { sidecar_bw } else { vec![] },
            vertex_count,
            face_count,
            source_vertex_map,
            ..Default::default()
        });

        v_offset += nv;
        vt_offset += nvt;
        vn_offset += nvn;
    }

    let total_vertices: usize = submeshes.iter().map(|s| s.vertices.len()).sum();
    let total_faces: usize = submeshes.iter().map(|s| s.faces.len()).sum();
    let has_uvs = submeshes.iter().any(|s| !s.uvs.is_empty());
    // NOTE: Python's import_obj leaves has_bones at its dataclass default
    // (false) even when the cfmeta sidecar provides skin data. Mirror that
    // exactly -- callers that care about skin presence inspect
    // submeshes[i].bone_indices directly.
    let has_bones = false;

    let mut bbox_min = [0.0f32; 3];
    let mut bbox_max = [0.0f32; 3];
    if !submeshes.is_empty() {
        let mut started = false;
        for s in &submeshes {
            for v in &s.vertices {
                if !started {
                    bbox_min = *v;
                    bbox_max = *v;
                    started = true;
                    continue;
                }
                for i in 0..3 {
                    if v[i] < bbox_min[i] { bbox_min[i] = v[i]; }
                    if v[i] > bbox_max[i] { bbox_max[i] = v[i]; }
                }
            }
        }
    }

    let result = ParsedMesh {
        path: source_path,
        format: source_format,
        bbox_min,
        bbox_max,
        submeshes,
        total_vertices,
        total_faces,
        has_uvs,
        has_bones,
        ..Default::default()
    };

    log::info!(
        "Imported OBJ {}: {} submeshes, {} verts, {} faces, source={} ({})",
        obj_path.display(),
        result.submeshes.len(),
        result.total_vertices,
        result.total_faces,
        result.path,
        result.format,
    );
    Ok(result)
}

fn parse_f(s: &str) -> Result<f32> {
    s.parse::<f32>()
        .map_err(|_| ParseError::Other(format!("invalid float {s:?}")))
}

/// Count vertices/UVs/normals per `o`-delimited submesh. Mirrors the Python
/// second-pass scan; we re-walk the buffer rather than tracking indices in
/// the first pass because the Python does it that way and the cost is small.
fn count_per_submesh(buf: &str) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    let mut sm_v = Vec::new();
    let mut sm_vt = Vec::new();
    let mut sm_vn = Vec::new();
    let mut cv = 0usize;
    let mut cvt = 0usize;
    let mut cvn = 0usize;
    let mut started = false;
    for line in buf.lines() {
        let line = line.trim_start();
        if line.starts_with("o ") {
            if started {
                sm_v.push(cv);
                sm_vt.push(cvt);
                sm_vn.push(cvn);
            }
            cv = 0;
            cvt = 0;
            cvn = 0;
            started = true;
        } else if line.starts_with("vt ") {
            cvt += 1;
        } else if line.starts_with("vn ") {
            cvn += 1;
        } else if line.starts_with("v ") {
            cv += 1;
        }
    }
    if started {
        sm_v.push(cv);
        sm_vt.push(cvt);
        sm_vn.push(cvn);
    }
    (sm_v, sm_vt, sm_vn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cdml_obj_test_{}_{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn resolve_obj_index_positive_negative_zero() {
        assert_eq!(resolve_obj_index("1", 100).unwrap(), 0);
        assert_eq!(resolve_obj_index("100", 100).unwrap(), 99);
        assert_eq!(resolve_obj_index("-1", 100).unwrap(), 99);
        assert_eq!(resolve_obj_index("-100", 100).unwrap(), 0);
        assert!(resolve_obj_index("0", 100).is_err());
        assert!(resolve_obj_index("foo", 100).is_err());
    }

    #[test]
    fn imports_minimal_quad_with_metadata() {
        let obj = write_tmp(
            "quad.obj",
            "# source_path: test/quad.pac\n\
             # source_format: pac\n\
             o quad\n\
             v 0 0 0\n\
             v 1 0 0\n\
             v 1 1 0\n\
             v 0 1 0\n\
             vt 0 0\n\
             vt 1 0\n\
             vt 1 1\n\
             vt 0 1\n\
             f 1/1 2/2 3/3 4/4\n",
        );
        let m = import_obj(&obj).unwrap();
        assert_eq!(m.path, "test/quad.pac");
        assert_eq!(m.format, "pac");
        assert_eq!(m.submeshes.len(), 1);
        let sm = &m.submeshes[0];
        assert_eq!(sm.name, "quad");
        assert_eq!(sm.vertices.len(), 4);
        // Quad fan-triangulated to 2 tris.
        assert_eq!(sm.faces.len(), 2);
        // V flipped on read-side.
        assert!((sm.uvs[0][1] - 1.0).abs() < 1e-6, "got {:?}", sm.uvs[0]);
        assert!((sm.uvs[2][1] - 0.0).abs() < 1e-6, "got {:?}", sm.uvs[2]);
        // Source-vertex map is identity for unmodified import.
        assert_eq!(sm.source_vertex_map, vec![0, 1, 2, 3]);
    }

    #[test]
    fn uv_seam_clones_vertex() {
        // Two triangles sharing vertex 1 with two different UVs trigger a clone.
        let obj = write_tmp(
            "seam.obj",
            "o seam\n\
             v 0 0 0\n\
             v 1 0 0\n\
             v 0 1 0\n\
             vt 0 0\n\
             vt 1 0\n\
             vt 0 1\n\
             vt 0.5 0.5\n\
             f 1/1 2/2 3/3\n\
             f 1/4 2/2 3/3\n",
        );
        let m = import_obj(&obj).unwrap();
        let sm = &m.submeshes[0];
        // Vertex 0 referenced with UV idx 1 then UV idx 4 -> clone created.
        assert_eq!(sm.vertices.len(), 4, "expected one cloned vertex");
        // The clone's source_vertex_map points back to the original slot.
        assert_eq!(sm.source_vertex_map.len(), 4);
        assert_eq!(sm.source_vertex_map[3], sm.source_vertex_map[0]);
    }

    #[test]
    fn negative_indices() {
        let obj = write_tmp(
            "neg.obj",
            "o tri\n\
             v 0 0 0\n\
             v 1 0 0\n\
             v 0 1 0\n\
             f -3 -2 -1\n",
        );
        let m = import_obj(&obj).unwrap();
        let sm = &m.submeshes[0];
        assert_eq!(sm.faces, vec![[0u32, 1, 2]]);
    }

    #[test]
    fn loads_cfmeta_sidecar_skin() {
        let obj = write_tmp(
            "rigged.obj",
            "o body\n\
             v 0 0 0\n\
             v 1 0 0\n\
             v 0 1 0\n\
             f 1 2 3\n",
        );
        let mut sidecar_path = obj.as_os_str().to_owned();
        sidecar_path.push(".cfmeta.json");
        std::fs::write(
            sidecar_path,
            r#"{
                "schema_version": 1,
                "source_path": "x.pac",
                "source_format": "pac",
                "submeshes": [
                    {
                        "name": "body",
                        "vertex_count": 3,
                        "bone_indices": [[0], [1], [2]],
                        "bone_weights": [[1.0], [1.0], [1.0]]
                    }
                ]
            }"#,
        )
        .unwrap();
        let m = import_obj(&obj).unwrap();
        let sm = &m.submeshes[0];
        assert_eq!(sm.bone_indices, vec![vec![0u32], vec![1], vec![2]]);
        assert_eq!(sm.bone_weights.len(), 3);
        // has_bones stays false (Python parity); callers inspect bone_indices directly.
        assert!(!m.has_bones);
        assert!(!sm.bone_indices.is_empty());
    }
}
