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

    // Build a Geometry-id → Model-id map from the Connections section so we
    // can apply each Model's Lcl Translation/Rotation/Scaling to its
    // Geometry's vertices below. Without this, object-level transforms in
    // Blender (move / rotate / scale of the mesh object, common when an FBX
    // is exported from a non-trivial scene) are lost on import and the
    // rebuilt PAM ends up with vertex positions in the wrong place.
    let mut geo_id_to_model_id: std::collections::HashMap<i64, i64> =
        std::collections::HashMap::new();
    let geo_ids: std::collections::HashSet<i64> = objects
        .children_named("Geometry")
        .filter_map(|g| g.props.first().and_then(|p| match p { FbxProp::I64(i) => Some(*i), _ => None }))
        .collect();
    let model_ids: std::collections::HashSet<i64> = objects
        .children_named("Model")
        .filter_map(|m| m.props.first().and_then(|p| match p { FbxProp::I64(i) => Some(*i), _ => None }))
        .collect();
    if let Some(conns) = root.child("Connections") {
        for c in conns.children_named("C") {
            if c.props.len() < 3 { continue; }
            let kind = match &c.props[0] { FbxProp::Str(s) => s.as_str(), _ => continue };
            if kind != "OO" { continue; }
            let src = match &c.props[1] { FbxProp::I64(i) => *i, _ => continue };
            let dst = match &c.props[2] { FbxProp::I64(i) => *i, _ => continue };
            if geo_ids.contains(&src) && model_ids.contains(&dst) {
                geo_id_to_model_id.insert(src, dst);
            }
        }
    }
    // Index Model nodes by id for quick lookup during transform extraction.
    let model_by_id: std::collections::HashMap<i64, &FbxNode> = objects
        .children_named("Model")
        .filter_map(|m| m.props.first().and_then(|p| match p { FbxProp::I64(i) => Some((*i, m)), _ => None }))
        .collect();

    // For each Geometry node pull verts + polygon indices + UVs + normals.
    let mut submeshes: Vec<SubMesh> = Vec::new();
    for geom in objects.children_named("Geometry") {
        let geo_id = match geom.props.first() {
            Some(FbxProp::I64(i)) => *i,
            _ => 0,
        };
        let model = geo_id_to_model_id.get(&geo_id).and_then(|mid| model_by_id.get(mid)).copied();
        let mut sm = match extract_geometry(geom) {
            Some(sm) => sm,
            None => continue,
        };
        if let Some(m) = model {
            apply_model_transform(m, &mut sm.vertices);
        }
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

/// One layer-element source extracted from an FBX `LayerElementUV` /
/// `LayerElementNormal` node. The (mapping, reference) pair determines how
/// to look up an attribute for a given (poly_vi, vi) corner.
struct LayerElement {
    flat: Vec<f32>,
    by_poly_vert: bool,
    indexed: bool,
    index: Vec<i32>,
}

impl LayerElement {
    fn lookup_uv(&self, poly_vi: usize, vi: u32) -> [f32; 2] {
        let slot = self.slot(poly_vi, vi);
        let base = slot * 2;
        if base + 1 >= self.flat.len() {
            return [0.0, 0.0];
        }
        // Flip V back to game convention (Python: `1.0 - v`).
        [self.flat[base], 1.0 - self.flat[base + 1]]
    }
    fn lookup_normal(&self, poly_vi: usize, vi: u32) -> [f32; 3] {
        let slot = self.slot(poly_vi, vi);
        let base = slot * 3;
        if base + 2 >= self.flat.len() {
            return [0.0, 1.0, 0.0];
        }
        [self.flat[base], self.flat[base + 1], self.flat[base + 2]]
    }
    fn slot(&self, poly_vi: usize, vi: u32) -> usize {
        if self.by_poly_vert {
            if self.indexed && poly_vi < self.index.len() {
                self.index[poly_vi].max(0) as usize
            } else {
                poly_vi
            }
        } else {
            vi as usize
        }
    }
}

fn extract_layer_element(node: &FbxNode, payload_child: &str, index_child: &str) -> Option<LayerElement> {
    let payload_arr = match node.child(payload_child).and_then(|n| n.props.first())? {
        FbxProp::F64Arr(arr) => arr.iter().map(|&x| x as f32).collect::<Vec<f32>>(),
        FbxProp::F32Arr(arr) => arr.clone(),
        _ => return None,
    };
    let mapping = node.child("MappingInformationType").and_then(string_prop);
    let reference = node.child("ReferenceInformationType").and_then(string_prop);
    let by_poly_vert = mapping.as_deref() != Some("ByVertice");
    let indexed = reference.as_deref() == Some("IndexToDirect");
    let index: Vec<i32> = if indexed {
        match node.child(index_child).and_then(|n| n.props.first()) {
            Some(FbxProp::I32Arr(arr)) => arr.clone(),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    Some(LayerElement { flat: payload_arr, by_poly_vert, indexed, index })
}

fn extract_geometry(geom: &FbxNode) -> Option<SubMesh> {
    // First prop is i64 id, second is "name\x00\x01Geometry", third is "Mesh"
    let raw_label = match geom.props.get(1) {
        Some(FbxProp::Str(s)) => s.clone(),
        _ => String::new(),
    };
    let name = raw_label.split('\u{0}').next().unwrap_or("").to_string();

    let verts_node = geom.child("Vertices")?;
    let base_verts: Vec<[f32; 3]> = match verts_node.props.first() {
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

    // Reconstruct polygons (variable arity) from the FBX flat list. Last
    // index of each polygon has its high bit XORed with -1, marking the
    // polygon end.
    let mut polygons: Vec<Vec<u32>> = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    for &raw in pvi {
        let (vi, end) = if raw < 0 {
            ((!raw) as u32, true)
        } else {
            (raw as u32, false)
        };
        current.push(vi);
        if end {
            polygons.push(std::mem::take(&mut current));
        }
    }

    let uv_layer = geom.child("LayerElementUV").and_then(|n| extract_layer_element(n, "UV", "UVIndex"));
    let n_layer = geom.child("LayerElementNormal").and_then(|n| extract_layer_element(n, "Normals", "NormalsIndex"));

    // Corner-expansion: walk every polygon corner, fetch its UV+normal,
    // and either (a) reuse the base vertex slot if its UV/normal is
    // unassigned or already matches, or (b) clone a fresh slot. Mirrors
    // `_resolve` in core/mesh_importer.py::import_fbx.
    let n_base = base_verts.len();
    let mut local_verts: Vec<[f32; 3]> = base_verts.clone();
    let mut local_uvs: Vec<[f32; 2]> = vec![[0.0, 0.0]; n_base];
    let mut local_norms: Vec<[f32; 3]> = vec![[0.0, 1.0, 0.0]; n_base];
    let mut assigned: Vec<bool> = vec![false; n_base];
    let mut corner_cache: std::collections::HashMap<CornerKey, u32> = std::collections::HashMap::new();
    let mut faces: Vec<[u32; 3]> = Vec::with_capacity(polygons.len());

    // Bit-exact float keys: matches Python's tuple comparison, which uses
    // `==` on float members. Anything else merges corners that Python keeps
    // separate (off-by-N verts when adjacent UVs differ within rounding).
    let key_uv = |uv: [f32; 2]| -> [u32; 2] { [uv[0].to_bits(), uv[1].to_bits()] };
    let key_n  = |n: [f32; 3]|  -> [u32; 3] { [n[0].to_bits(), n[1].to_bits(), n[2].to_bits()] };

    let mut poly_vi: usize = 0;
    for poly in &polygons {
        let mut corners: Vec<u32> = Vec::with_capacity(poly.len());
        for &vi in poly {
            let uv = uv_layer.as_ref().map(|l| l.lookup_uv(poly_vi, vi)).unwrap_or([0.0, 0.0]);
            let nm = n_layer.as_ref().map(|l| l.lookup_normal(poly_vi, vi)).unwrap_or([0.0, 1.0, 0.0]);
            poly_vi += 1;

            let key = (vi, key_uv(uv), key_n(nm));
            if let Some(&hit) = corner_cache.get(&key) {
                corners.push(hit);
                continue;
            }
            let resolved = if (vi as usize) < n_base && !assigned[vi as usize] {
                local_uvs[vi as usize] = uv;
                local_norms[vi as usize] = nm;
                assigned[vi as usize] = true;
                vi
            } else if (vi as usize) < n_base
                && local_uvs[vi as usize] == uv
                && local_norms[vi as usize] == nm
            {
                vi
            } else {
                let clone = local_verts.len() as u32;
                let src_pos = if (vi as usize) < n_base { base_verts[vi as usize] } else { [0.0; 3] };
                local_verts.push(src_pos);
                local_uvs.push(uv);
                local_norms.push(nm);
                clone
            };
            corner_cache.insert(key, resolved);
            corners.push(resolved);
        }
        // Fan-triangulate.
        for i in 1..corners.len().saturating_sub(1) {
            faces.push([corners[0], corners[i], corners[i + 1]]);
        }
    }

    let has_uv_data = uv_layer.is_some();
    let uvs = if has_uv_data { local_uvs } else { Vec::new() };

    // If the FBX shipped no normals, fall back to smooth normals on the
    // post-expansion vertex set.
    let normals = if n_layer.is_some() {
        local_norms
    } else if !local_verts.is_empty() && !faces.is_empty() {
        compute_smooth_normals(&local_verts, &faces)
    } else {
        Vec::new()
    };

    let vertex_count = local_verts.len();
    let face_count = faces.len();
    Some(SubMesh {
        name,
        vertices: local_verts,
        uvs,
        normals,
        faces,
        vertex_count,
        face_count,
        ..Default::default()
    })
}

type CornerKey = (u32, [u32; 2], [u32; 3]);

/// Apply a Model node's Lcl Translation/Rotation/Scaling to vertex positions
/// in place. Mirrors `_apply_model_transform` from `core/mesh_importer.py`:
///
///     V_world = R(V_local) + T / S
///
/// where R is built from Euler XYZ angles in degrees and S is the Lcl
/// Scaling factor (Blender bakes UnitScaleFactor into Lcl Scaling on
/// export; dividing T by S converts back to game-space units). Lcl Scaling
/// is assumed uniform — we use sx as the representative factor.
fn apply_model_transform(model: &FbxNode, verts: &mut [[f32; 3]]) {
    let p70 = match model.child("Properties70") { Some(p) => p, None => return };

    let mut tx = 0.0f64; let mut ty = 0.0f64; let mut tz = 0.0f64;
    let mut rx = 0.0f64; let mut ry = 0.0f64; let mut rz = 0.0f64;
    let mut sx = 1.0f64;

    for p in p70.children_named("P") {
        let name = match p.props.first() { Some(FbxProp::Str(s)) => s.as_str(), _ => continue };
        if p.props.len() < 7 { continue; }
        let f = |idx: usize| -> Option<f64> {
            match &p.props[idx] {
                FbxProp::F64(v) => Some(*v),
                FbxProp::F32(v) => Some(*v as f64),
                FbxProp::I32(v) => Some(*v as f64),
                FbxProp::I64(v) => Some(*v as f64),
                _ => None,
            }
        };
        match name {
            "Lcl Translation" => {
                if let (Some(a), Some(b), Some(c)) = (f(4), f(5), f(6)) { tx = a; ty = b; tz = c; }
            }
            "Lcl Rotation" => {
                if let (Some(a), Some(b), Some(c)) = (f(4), f(5), f(6)) { rx = a; ry = b; rz = c; }
            }
            "Lcl Scaling" => {
                if let Some(a) = f(4) { sx = a; }
            }
            _ => {}
        }
    }

    let no_translation = tx.abs() < 1e-8 && ty.abs() < 1e-8 && tz.abs() < 1e-8;
    let no_rotation = rx.abs() < 1e-8 && ry.abs() < 1e-8 && rz.abs() < 1e-8;
    if no_translation && no_rotation { return; }

    let rx_r = rx.to_radians();
    let ry_r = ry.to_radians();
    let rz_r = rz.to_radians();
    let cx = rx_r.cos(); let sx_ = rx_r.sin();
    let cy = ry_r.cos(); let sy_ = ry_r.sin();
    let cz = rz_r.cos(); let sz_ = rz_r.sin();

    // R = Rz * Ry * Rx (FBX Euler XYZ order).
    let r00 =  cy * cz;
    let r01 =  cz * sx_ * sy_ - cx * sz_;
    let r02 =  cx * cz * sy_ + sx_ * sz_;
    let r10 =  cy * sz_;
    let r11 =  cx * cz + sx_ * sy_ * sz_;
    let r12 = -cz * sx_ + cx * sy_ * sz_;
    let r20 = -sy_;
    let r21 =  cy * sx_;
    let r22 =  cy * cx;

    let s = if sx.abs() > 1e-8 { sx } else { 1.0 };
    let ttx = tx / s; let tty = ty / s; let ttz = tz / s;

    for v in verts.iter_mut() {
        let x = v[0] as f64; let y = v[1] as f64; let z = v[2] as f64;
        v[0] = (r00 * x + r01 * y + r02 * z + ttx) as f32;
        v[1] = (r10 * x + r11 * y + r12 * z + tty) as f32;
        v[2] = (r20 * x + r21 * y + r22 * z + ttz) as f32;
    }
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
