//! Binary FBX 7.x import for the round-trip mesh pipeline.
//!
//! Mirrors the geometry-extraction part of `core/mesh_importer.py::import_fbx`.
//! Parses the FBX node tree, walks each `Geometry` mesh, and reconstructs a
//! `ParsedMesh` ready for `build_pam` / `build_pamlod` / `build_pac`.
//!
//! Out of scope for this MVP:
//!   - FBX 6.x ASCII / 6.x binary (legacy Blender exports)
//!   - LCL Translation/Rotation/Scaling on Models (ignored; identity assumed)
//!   - Skin cluster reconstruction (skin sidecar v2 still required for bones)
//!   - Animation curves
//!
//! Polygon vertex indices in FBX use the trailing-bit-XOR convention to mark
//! the last vertex of each polygon: face vertex `i` has its high bit XORed
//! with -1 (so `^ -1`). We undo this on read and split arbitrary-arity polys
//! into triangles via fan triangulation.

use std::io::{Cursor, Read};

use flate2::read::ZlibDecoder;

use crate::error::{ParseError, Result};
use crate::repack::mesh::cfmeta;
use crate::repack::mesh::quant::compute_smooth_normals;
use crate::repack::mesh::{ParsedMesh, SubMesh};

const FBX_MAGIC: &[u8] = b"Kaydara FBX Binary  \x00";

/// One FBX property value. Many cases are intentionally unhandled in this
/// MVP -- we only decode the property kinds present in mesh + transform
/// data emitted by Blender 2.8+ FBX exporter.
#[derive(Debug, Clone)]
pub enum FbxProp {
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    F32Arr(Vec<f32>),
    F64Arr(Vec<f64>),
    I32Arr(Vec<i32>),
    I64Arr(Vec<i64>),
    BoolArr(Vec<bool>),
}

/// One FBX node. Recursive: children may carry their own props + children.
#[derive(Debug, Clone)]
pub struct FbxNode {
    pub name: String,
    pub props: Vec<FbxProp>,
    pub children: Vec<FbxNode>,
}

impl FbxNode {
    pub fn child(&self, name: &str) -> Option<&FbxNode> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a FbxNode> {
        self.children.iter().filter(move |c| c.name == name)
    }
}

/// Parse a binary FBX 7.x file into a flat node tree (root has children =
/// the top-level FBXHeaderExtension/GlobalSettings/Documents/Objects/etc.).
pub fn parse_fbx(data: &[u8]) -> Result<FbxNode> {
    if data.len() < 27 || !data.starts_with(FBX_MAGIC) {
        return Err(ParseError::Other("not a binary FBX 7.x file".into()));
    }
    let version = u32::from_le_bytes(data[23..27].try_into().unwrap());
    let is_v75 = version >= 7500;
    let mut cursor = Cursor::new(data);
    cursor.set_position(27);
    let mut children = Vec::new();
    loop {
        let pos = cursor.position() as usize;
        if pos + node_header_size(is_v75) > data.len() {
            break;
        }
        let n = read_node(&mut cursor, is_v75, data)?;
        match n {
            None => break,
            Some(node) => children.push(node),
        }
    }
    Ok(FbxNode { name: "<root>".into(), props: vec![], children })
}

fn node_header_size(is_v75: bool) -> usize {
    if is_v75 { 25 } else { 13 }
}

fn read_u32(cur: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut buf = [0u8; 4];
    cur.read_exact(&mut buf).map_err(|_| ParseError::Other("eof in u32".into()))?;
    Ok(u32::from_le_bytes(buf))
}
fn read_u64(cur: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut buf = [0u8; 8];
    cur.read_exact(&mut buf).map_err(|_| ParseError::Other("eof in u64".into()))?;
    Ok(u64::from_le_bytes(buf))
}
fn read_u8(cur: &mut Cursor<&[u8]>) -> Result<u8> {
    let mut buf = [0u8; 1];
    cur.read_exact(&mut buf).map_err(|_| ParseError::Other("eof in u8".into()))?;
    Ok(buf[0])
}

fn read_node(cur: &mut Cursor<&[u8]>, is_v75: bool, data: &[u8]) -> Result<Option<FbxNode>> {
    // Header: end_offset, num_props, prop_list_len, name_len(u8), name_bytes
    let (end_offset, num_props, prop_list_len) = if is_v75 {
        (read_u64(cur)? as u64, read_u64(cur)? as u64, read_u64(cur)? as u64)
    } else {
        (read_u32(cur)? as u64, read_u32(cur)? as u64, read_u32(cur)? as u64)
    };
    let name_len = read_u8(cur)? as usize;

    if end_offset == 0 && num_props == 0 && prop_list_len == 0 && name_len == 0 {
        return Ok(None); // sentinel
    }

    let mut name_bytes = vec![0u8; name_len];
    cur.read_exact(&mut name_bytes).map_err(|_| ParseError::Other("eof in node name".into()))?;
    let name = String::from_utf8_lossy(&name_bytes).to_string();

    let props_start = cur.position() as usize;
    let mut props = Vec::with_capacity(num_props as usize);
    for _ in 0..num_props {
        props.push(read_prop(cur)?);
    }
    // Skip any trailing prop bytes if present.
    let props_end = cur.position() as usize;
    let consumed = props_end - props_start;
    if (consumed as u64) < prop_list_len {
        cur.set_position(props_end as u64 + (prop_list_len - consumed as u64));
    }

    let mut children = Vec::new();
    while (cur.position() as u64) < end_offset {
        // If the remaining slice is just the null sentinel, consume it and stop.
        let remaining = end_offset - cur.position();
        if remaining as usize == node_header_size(is_v75) {
            // Sentinel block: drain it and stop.
            let _ = read_node(cur, is_v75, data)?;
            break;
        }
        match read_node(cur, is_v75, data)? {
            None => break,
            Some(c) => children.push(c),
        }
    }
    cur.set_position(end_offset);
    Ok(Some(FbxNode { name, props, children }))
}

fn read_prop(cur: &mut Cursor<&[u8]>) -> Result<FbxProp> {
    let t = read_u8(cur)?;
    Ok(match t as char {
        'C' => FbxProp::Bool(read_u8(cur)? != 0),
        'Y' => {
            let mut b = [0u8; 2];
            cur.read_exact(&mut b).map_err(|_| ParseError::Other("eof Y".into()))?;
            FbxProp::I16(i16::from_le_bytes(b))
        }
        'I' => {
            let mut b = [0u8; 4];
            cur.read_exact(&mut b).map_err(|_| ParseError::Other("eof I".into()))?;
            FbxProp::I32(i32::from_le_bytes(b))
        }
        'L' => {
            let mut b = [0u8; 8];
            cur.read_exact(&mut b).map_err(|_| ParseError::Other("eof L".into()))?;
            FbxProp::I64(i64::from_le_bytes(b))
        }
        'F' => {
            let mut b = [0u8; 4];
            cur.read_exact(&mut b).map_err(|_| ParseError::Other("eof F".into()))?;
            FbxProp::F32(f32::from_le_bytes(b))
        }
        'D' => {
            let mut b = [0u8; 8];
            cur.read_exact(&mut b).map_err(|_| ParseError::Other("eof D".into()))?;
            FbxProp::F64(f64::from_le_bytes(b))
        }
        'S' => {
            let len = read_u32(cur)? as usize;
            let mut buf = vec![0u8; len];
            cur.read_exact(&mut buf).map_err(|_| ParseError::Other("eof S".into()))?;
            FbxProp::Str(String::from_utf8_lossy(&buf).to_string())
        }
        'R' => {
            let len = read_u32(cur)? as usize;
            let mut buf = vec![0u8; len];
            cur.read_exact(&mut buf).map_err(|_| ParseError::Other("eof R".into()))?;
            FbxProp::Bytes(buf)
        }
        'b' | 'i' | 'l' | 'f' | 'd' => {
            let kind = t as char;
            let arr_len  = read_u32(cur)? as usize;
            let encoding = read_u32(cur)?;
            let comp_len = read_u32(cur)? as usize;
            let mut raw = vec![0u8; comp_len];
            cur.read_exact(&mut raw).map_err(|_| ParseError::Other("eof array".into()))?;
            let bytes = if encoding == 1 {
                let mut out = Vec::new();
                ZlibDecoder::new(&raw[..])
                    .read_to_end(&mut out)
                    .map_err(|e| ParseError::Other(format!("zlib decode: {e}")))?;
                out
            } else {
                raw
            };
            decode_array(kind, &bytes, arr_len)?
        }
        other => return Err(ParseError::Other(format!("unknown FBX prop type {other:?}"))),
    })
}

fn decode_array(kind: char, bytes: &[u8], n: usize) -> Result<FbxProp> {
    Ok(match kind {
        'b' => FbxProp::BoolArr(bytes.iter().take(n).map(|&b| b != 0).collect()),
        'i' => {
            if bytes.len() < n * 4 {
                return Err(ParseError::Other("short i32 array".into()));
            }
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()));
            }
            FbxProp::I32Arr(out)
        }
        'l' => {
            if bytes.len() < n * 8 {
                return Err(ParseError::Other("short i64 array".into()));
            }
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(i64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap()));
            }
            FbxProp::I64Arr(out)
        }
        'f' => {
            if bytes.len() < n * 4 {
                return Err(ParseError::Other("short f32 array".into()));
            }
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()));
            }
            FbxProp::F32Arr(out)
        }
        'd' => {
            if bytes.len() < n * 8 {
                return Err(ParseError::Other("short f64 array".into()));
            }
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(f64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap()));
            }
            FbxProp::F64Arr(out)
        }
        _ => unreachable!(),
    })
}

// ---- High-level: extract ParsedMesh ----------------------------------------

/// Import a binary FBX into a `ParsedMesh`. Geometry-only MVP.
///
/// Reads the optional `<fbx_path>.cfmeta.json` sidecar; when present the
/// returned mesh carries skin bindings + source_vertex_map per submesh
/// (matched on submesh name, identical to the OBJ importer's convention).
pub fn import_fbx(fbx_path: &std::path::Path) -> Result<ParsedMesh> {
    let bytes = std::fs::read(fbx_path)
        .map_err(|e| ParseError::Other(format!("read {}: {e}", fbx_path.display())))?;
    let root = parse_fbx(&bytes)?;
    let sidecar = cfmeta::load_sidecar(fbx_path);

    let objects = root
        .child("Objects")
        .ok_or_else(|| ParseError::Other("FBX missing Objects".into()))?;

    // For each Geometry node pull verts + polygon indices + UVs + normals.
    let mut submeshes: Vec<SubMesh> = Vec::new();
    for geom in objects.children_named("Geometry") {
        let sm = match extract_geometry(geom) {
            Some(sm) => sm,
            None => continue,
        };
        submeshes.push(sm);
    }

    // Apply sidecar skin bindings + source-vertex-map by submesh name.
    if let Some(c) = &sidecar {
        let by_name: std::collections::HashMap<&str, &cfmeta::CfmetaSubmesh> =
            c.submeshes.iter().filter(|s| !s.name.is_empty()).map(|s| (s.name.as_str(), s)).collect();
        for sm in &mut submeshes {
            if let Some(rec) = by_name.get(sm.name.as_str()) {
                let n = sm.vertices.len();
                let mut bi: Vec<Vec<u32>> = Vec::with_capacity(n);
                let mut bw: Vec<Vec<f32>> = Vec::with_capacity(n);
                for i in 0..n {
                    bi.push(rec.bone_indices.get(i).cloned().unwrap_or_default());
                    bw.push(rec.bone_weights.get(i).cloned().unwrap_or_default());
                }
                sm.bone_indices = bi;
                sm.bone_weights = bw;
                if !rec.source_vertex_map.is_empty() {
                    let mut svm: Vec<i64> = Vec::with_capacity(n);
                    for i in 0..n {
                        svm.push(*rec.source_vertex_map.get(i).unwrap_or(&-1));
                    }
                    sm.source_vertex_map = svm;
                } else {
                    sm.source_vertex_map = (0..n as i64).collect();
                }
            }
        }
    }

    let total_vertices: usize = submeshes.iter().map(|s| s.vertices.len()).sum();
    let total_faces: usize = submeshes.iter().map(|s| s.faces.len()).sum();
    let has_uvs = submeshes.iter().any(|s| !s.uvs.is_empty());

    let (path, format) = sidecar
        .as_ref()
        .map(|c| (c.source_path.clone(), c.source_format.clone()))
        .unwrap_or_else(|| (String::new(), String::new()));

    let mut bbox_min = [0.0f32; 3];
    let mut bbox_max = [0.0f32; 3];
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

    Ok(ParsedMesh {
        path,
        format,
        bbox_min,
        bbox_max,
        submeshes,
        total_vertices,
        total_faces,
        has_uvs,
        ..Default::default()
    })
}

fn extract_geometry(geom: &FbxNode) -> Option<SubMesh> {
    // First prop is i64 id, second is "name\x00\x01Geometry", third is "Mesh"
    let raw_label = match geom.props.get(1) {
        Some(FbxProp::Str(s)) => s.clone(),
        _ => String::new(),
    };
    let name = raw_label.split('\u{0}').next().unwrap_or("").to_string();

    let verts_node = geom.child("Vertices")?;
    let verts: Vec<[f32; 3]> = match verts_node.props.first() {
        Some(FbxProp::F64Arr(arr)) => arr
            .chunks_exact(3)
            .map(|c| [c[0] as f32, c[1] as f32, c[2] as f32])
            .collect(),
        _ => return None,
    };

    let pvi_node = geom.child("PolygonVertexIndex")?;
    let pvi: &Vec<i32> = match pvi_node.props.first() {
        Some(FbxProp::I32Arr(arr)) => arr,
        _ => return None,
    };

    // Convert FBX polygon index list (last index of each poly XOR -1) into faces.
    // Fan-triangulate polys with > 3 corners.
    let mut faces: Vec<[u32; 3]> = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    for &raw in pvi {
        let (vi, end) = if raw < 0 {
            ((!raw) as u32, true)   // ^ -1 in two's complement
        } else {
            (raw as u32, false)
        };
        current.push(vi);
        if end {
            for i in 1..current.len().saturating_sub(1) {
                faces.push([current[0], current[i], current[i + 1]]);
            }
            current.clear();
        }
    }

    // UVs: LayerElementUV[0] / UV (f64 array, ByVertice or ByPolygonVertex+Direct/IndexToDirect).
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    if let Some(uv_node) = geom.child("LayerElementUV") {
        if let (Some(uv_arr), mapping, _ref_type) = (
            uv_node.child("UV").and_then(|n| n.props.first()),
            uv_node.child("MappingInformationType").and_then(|n| string_prop(n)),
            uv_node.child("ReferenceInformationType").and_then(|n| string_prop(n)),
        ) {
            if let FbxProp::F64Arr(arr) = uv_arr {
                let raw_uvs: Vec<[f32; 2]> = arr
                    .chunks_exact(2)
                    .map(|c| [c[0] as f32, 1.0 - c[1] as f32]) // flip V back to game convention
                    .collect();
                if mapping.as_deref() == Some("ByVertice") && raw_uvs.len() == verts.len() {
                    uvs = raw_uvs;
                }
                // ByPolygonVertex paths intentionally skipped in this MVP --
                // OBJ-style v/vt/vn round-trips already handle the seam case;
                // the FBX exporter we ship writes ByVertice.
            }
        }
    }

    // Normals: ByVertice / Direct = same shape as Vertices.
    let mut normals: Vec<[f32; 3]> = Vec::new();
    if let Some(n_node) = geom.child("LayerElementNormal") {
        if let (Some(n_arr), mapping) = (
            n_node.child("Normals").and_then(|n| n.props.first()),
            n_node.child("MappingInformationType").and_then(|n| string_prop(n)),
        ) {
            if let FbxProp::F64Arr(arr) = n_arr {
                if mapping.as_deref() == Some("ByVertice") {
                    let raw_n: Vec<[f32; 3]> = arr
                        .chunks_exact(3)
                        .map(|c| [c[0] as f32, c[1] as f32, c[2] as f32])
                        .collect();
                    if raw_n.len() == verts.len() {
                        normals = raw_n;
                    }
                }
            }
        }
    }
    // If no normals on disk, recompute smooth.
    if normals.is_empty() && !verts.is_empty() && !faces.is_empty() {
        normals = compute_smooth_normals(&verts, &faces);
    }

    let vertex_count = verts.len();
    let face_count   = faces.len();
    Some(SubMesh {
        name,
        vertices: verts,
        uvs,
        normals,
        faces,
        vertex_count,
        face_count,
        ..Default::default()
    })
}

fn string_prop(n: &FbxNode) -> Option<String> {
    n.props.first().and_then(|p| match p {
        FbxProp::Str(s) => Some(s.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::mesh::{parse_pam, submeshes_to_textured_fbx, TextureRef};

    #[test]
    fn roundtrip_fbx_export_then_import_geometry_matches() {
        // Build a tiny mesh, export to FBX via cdcore::fbx, parse back via fbx_import.
        let bytes = std::fs::read("/cd/effect/pafx_plane_triangle_001a_lkh.pam").ok();
        // /cd may not exist in test env; skip when missing.
        let bytes = match bytes { Some(b) => b, None => return };
        let mesh = parse_pam(&bytes, "test.pam").unwrap();
        if mesh.submeshes.is_empty() { return; }
        let sm_refs: Vec<&_> = mesh.submeshes.iter().collect();
        let mut tex_refs: Vec<Option<TextureRef<'_>>> = Vec::with_capacity(sm_refs.len());
        for _ in 0..sm_refs.len() { tex_refs.push(None); }
        let fbx_bytes = submeshes_to_textured_fbx(&sm_refs, "test", &tex_refs);
        let tmp = std::env::temp_dir().join("cdcore_fbx_rt.fbx");
        std::fs::write(&tmp, &fbx_bytes).unwrap();
        let imported = import_fbx(&tmp).unwrap();

        assert_eq!(imported.submeshes.len(), mesh.submeshes.len());
        for (a, b) in imported.submeshes.iter().zip(mesh.submeshes.iter()) {
            assert_eq!(a.vertices.len(), b.vertices.len(), "submesh vertex count");
            for (va, vb) in a.vertices.iter().zip(b.vertices.iter()) {
                for i in 0..3 {
                    assert!((va[i] - vb[i]).abs() < 1e-4, "vertex differs: {va:?} vs {vb:?}");
                }
            }
            assert_eq!(a.faces.len(), b.faces.len(), "submesh face count");
        }
    }
}
