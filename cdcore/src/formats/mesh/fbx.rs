//! Binary FBX 7.4 writer for geometry-only mesh export.
//!
//! Ported from core/mesh_exporter.py (export_fbx).
//! Produces files compatible with Blender 2.8+, Maya, Unity, Unreal Engine 4+.
//!
//! Phase 1: geometry only (vertices / normals / UVs / faces).
//! Phase 2 (future): skeleton + skin deformers for PAC files.

use std::io::Write;
use flate2::{write::ZlibEncoder, Compression};

use super::pam::SubMesh;

// ---------------------------------------------------------------------------
// FBX property encoding
// ---------------------------------------------------------------------------

enum Prop<'a> {
    Bool(bool),
    I32(i32),
    I64(i64),
    F64(f64),
    Str(&'a str),
    F64s(&'a [f64]),
    I32s(&'a [i32]),
}

fn encode_prop(buf: &mut Vec<u8>, p: &Prop<'_>) {
    match p {
        Prop::Bool(v)  => { buf.push(b'C'); buf.push(*v as u8); }
        Prop::I32(v)   => { buf.push(b'I'); buf.extend_from_slice(&v.to_le_bytes()); }
        Prop::I64(v)   => { buf.push(b'L'); buf.extend_from_slice(&v.to_le_bytes()); }
        Prop::F64(v)   => { buf.push(b'D'); buf.extend_from_slice(&v.to_le_bytes()); }
        Prop::Str(s)   => {
            buf.push(b'S');
            let b = s.as_bytes();
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Prop::F64s(vs) => {
            buf.push(b'd');
            let raw: Vec<u8> = vs.iter().flat_map(|v| v.to_le_bytes()).collect();
            encode_array(buf, vs.len() as u32, raw);
        }
        Prop::I32s(vs) => {
            buf.push(b'i');
            if vs.is_empty() {
                buf.extend_from_slice(&0u32.to_le_bytes());
                buf.extend_from_slice(&0u32.to_le_bytes());
                buf.extend_from_slice(&0u32.to_le_bytes());
                return;
            }
            let raw: Vec<u8> = vs.iter().flat_map(|v| v.to_le_bytes()).collect();
            encode_array(buf, vs.len() as u32, raw);
        }
    }
}

fn encode_array(buf: &mut Vec<u8>, count: u32, raw: Vec<u8>) {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).ok();
    let compressed = enc.finish().unwrap_or_default();
    let (encoding, data) = if compressed.len() < raw.len() {
        (1u32, compressed)
    } else {
        (0u32, raw)
    };
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&encoding.to_le_bytes());
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(&data);
}

// ---------------------------------------------------------------------------
// FBX node writer
// ---------------------------------------------------------------------------

// Writes one FBX node.  Children is a closure that writes child nodes into
// the same buffer so end_offset values are absolute (same as Python approach).
fn node(buf: &mut Vec<u8>, name: &str, props: &[Prop<'_>], children: impl FnOnce(&mut Vec<u8>)) {
    let end_placeholder = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // end_offset placeholder

    let mut pbuf: Vec<u8> = Vec::new();
    for p in props { encode_prop(&mut pbuf, p); }

    buf.extend_from_slice(&(props.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(pbuf.len() as u32).to_le_bytes());
    let nb = name.as_bytes();
    buf.push(nb.len() as u8);
    buf.extend_from_slice(nb);
    buf.extend_from_slice(&pbuf);

    let child_start = buf.len();
    children(buf);
    if buf.len() > child_start {
        buf.extend_from_slice(&[0u8; 13]); // null terminator after last child
    }

    let end_offset = buf.len() as u32;
    buf[end_placeholder..end_placeholder + 4].copy_from_slice(&end_offset.to_le_bytes());
}

// Convenience: leaf node (no children).
fn leaf(buf: &mut Vec<u8>, name: &str, props: &[Prop<'_>]) {
    node(buf, name, props, |_| {});
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Encode a slice of SubMesh objects to binary FBX 7.4.
/// Works for PAM, PAMLOD, and PAC (phase 1 — geometry only, no skeleton).
pub fn submeshes_to_fbx(submeshes: &[&SubMesh], mesh_name: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256 * 1024);

    // ---- File header --------------------------------------------------------
    buf.extend_from_slice(b"Kaydara FBX Binary  \x00");
    buf.extend_from_slice(b"\x1a\x00");
    buf.extend_from_slice(&7400u32.to_le_bytes());

    // Unique IDs: start well above 0 to avoid collisions with root (id=0).
    let mut id_ctr: i64 = 3_000_000_000;
    let mut uid = || { id_ctr += 1; id_ctr };

    let geom_ids:  Vec<i64> = (0..submeshes.len()).map(|_| uid()).collect();
    let model_ids: Vec<i64> = (0..submeshes.len()).map(|_| uid()).collect();
    let mat_ids:   Vec<i64> = (0..submeshes.len()).map(|_| uid()).collect();

    // ---- FBXHeaderExtension -------------------------------------------------
    node(&mut buf, "FBXHeaderExtension", &[], |b| {
        leaf(b, "FBXHeaderVersion", &[Prop::I32(1003)]);
        leaf(b, "FBXVersion",       &[Prop::I32(7400)]);
        leaf(b, "Creator",          &[Prop::Str("cdwinfs")]);
    });

    // ---- GlobalSettings (Y-up, 100 cm unit) --------------------------------
    node(&mut buf, "GlobalSettings", &[], |b| {
        node(b, "Properties70", &[], |b2| {
            leaf(b2, "P", &[Prop::Str("UpAxis"),                 Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(1)]);
            leaf(b2, "P", &[Prop::Str("UpAxisSign"),             Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(1)]);
            leaf(b2, "P", &[Prop::Str("FrontAxis"),              Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(2)]);
            leaf(b2, "P", &[Prop::Str("FrontAxisSign"),          Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(1)]);
            leaf(b2, "P", &[Prop::Str("CoordAxis"),              Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(0)]);
            leaf(b2, "P", &[Prop::Str("CoordAxisSign"),          Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(1)]);
            leaf(b2, "P", &[Prop::Str("UnitScaleFactor"),        Prop::Str("double"), Prop::Str("Number"),  Prop::Str(""), Prop::F64(100.0)]);
            leaf(b2, "P", &[Prop::Str("OriginalUnitScaleFactor"),Prop::Str("double"), Prop::Str("Number"),  Prop::Str(""), Prop::F64(100.0)]);
        });
    });

    // ---- Objects ------------------------------------------------------------
    {
        let geom_ids  = &geom_ids;
        let model_ids = &model_ids;
        let mat_ids   = &mat_ids;
        let sms       = submeshes;

        node(&mut buf, "Objects", &[], move |b| {
            for (idx, sm) in sms.iter().enumerate() {
                let geom_id  = geom_ids[idx];
                let model_id = model_ids[idx];
                let mat_id   = mat_ids[idx];
                let sm_name  = if sm.name.is_empty() { mesh_name } else { &sm.name };

                // Geometry -------------------------------------------------
                let verts_flat: Vec<f64> = sm.vertices.iter()
                    .flat_map(|[x, y, z]| [*x as f64, *y as f64, *z as f64])
                    .collect();

                // FBX polygon vertex index: last index of each triangle is XOR -1.
                let poly_idx: Vec<i32> = sm.faces.iter()
                    .flat_map(|[a, b, c]| [*a as i32, *b as i32, *c as i32 ^ -1])
                    .collect();

                let normals_flat: Vec<f64> = sm.normals.iter()
                    .flat_map(|[nx, ny, nz]| [*nx as f64, *ny as f64, *nz as f64])
                    .collect();

                let uvs_flat: Vec<f64> = sm.uvs.iter()
                    .flat_map(|[u, v]| [*u as f64, 1.0 - *v as f64])
                    .collect();

                let has_normals = !normals_flat.is_empty();
                let has_uvs    = !uvs_flat.is_empty();

                let geom_label = format!("{sm_name}\x00\x01Geometry");
                node(b, "Geometry",
                    &[Prop::I64(geom_id), Prop::Str(&geom_label), Prop::Str("Mesh")],
                    |b2| {
                        leaf(b2, "Vertices",            &[Prop::F64s(&verts_flat)]);
                        leaf(b2, "PolygonVertexIndex",  &[Prop::I32s(&poly_idx)]);

                        if has_normals {
                            node(b2, "LayerElementNormal", &[Prop::I32(0)], |b3| {
                                leaf(b3, "Version",                  &[Prop::I32(101)]);
                                leaf(b3, "Name",                     &[Prop::Str("")]);
                                leaf(b3, "MappingInformationType",   &[Prop::Str("ByVertice")]);
                                leaf(b3, "ReferenceInformationType", &[Prop::Str("Direct")]);
                                leaf(b3, "Normals",                  &[Prop::F64s(&normals_flat)]);
                            });
                        }

                        if has_uvs {
                            node(b2, "LayerElementUV", &[Prop::I32(0)], |b3| {
                                leaf(b3, "Version",                  &[Prop::I32(101)]);
                                leaf(b3, "Name",                     &[Prop::Str("UVMap")]);
                                leaf(b3, "MappingInformationType",   &[Prop::Str("ByVertice")]);
                                leaf(b3, "ReferenceInformationType", &[Prop::Str("Direct")]);
                                leaf(b3, "UV",                       &[Prop::F64s(&uvs_flat)]);
                            });
                        }

                        node(b2, "Layer", &[Prop::I32(0)], |b3| {
                            leaf(b3, "Version", &[Prop::I32(100)]);
                            if has_normals {
                                node(b3, "LayerElement", &[], |b4| {
                                    leaf(b4, "Type",       &[Prop::Str("LayerElementNormal")]);
                                    leaf(b4, "TypedIndex", &[Prop::I32(0)]);
                                });
                            }
                            if has_uvs {
                                node(b3, "LayerElement", &[], |b4| {
                                    leaf(b4, "Type",       &[Prop::Str("LayerElementUV")]);
                                    leaf(b4, "TypedIndex", &[Prop::I32(0)]);
                                });
                            }
                        });
                    });

                // Model -------------------------------------------------------
                let model_label = format!("{sm_name}\x00\x01Model");
                node(b, "Model",
                    &[Prop::I64(model_id), Prop::Str(&model_label), Prop::Str("Mesh")],
                    |b2| {
                        leaf(b2, "Version", &[Prop::I32(232)]);
                        node(b2, "Properties70", &[], |b3| {
                            leaf(b3, "P", &[Prop::Str("Lcl Translation"), Prop::Str("Lcl Translation"), Prop::Str(""), Prop::Str("A"), Prop::F64(0.0), Prop::F64(0.0), Prop::F64(0.0)]);
                            leaf(b3, "P", &[Prop::Str("Lcl Rotation"),    Prop::Str("Lcl Rotation"),    Prop::Str(""), Prop::Str("A"), Prop::F64(0.0), Prop::F64(0.0), Prop::F64(0.0)]);
                            leaf(b3, "P", &[Prop::Str("Lcl Scaling"),     Prop::Str("Lcl Scaling"),     Prop::Str(""), Prop::Str("A"), Prop::F64(1.0), Prop::F64(1.0), Prop::F64(1.0)]);
                        });
                    });

                // Material ----------------------------------------------------
                let mat_name  = if sm.material.is_empty() { sm_name } else { &sm.material };
                let mat_label = format!("{mat_name}\x00\x01Material");
                node(b, "Material",
                    &[Prop::I64(mat_id), Prop::Str(&mat_label), Prop::Str("")],
                    |b2| {
                        leaf(b2, "Version",      &[Prop::I32(102)]);
                        leaf(b2, "ShadingModel", &[Prop::Str("phong")]);
                        node(b2, "Properties70", &[], |b3| {
                            leaf(b3, "P", &[Prop::Str("DiffuseColor"), Prop::Str("Color"), Prop::Str(""), Prop::Str("A"), Prop::F64(0.8), Prop::F64(0.8), Prop::F64(0.8)]);
                        });
                    });
            }
        });
    }

    // ---- Connections --------------------------------------------------------
    {
        let geom_ids  = &geom_ids;
        let model_ids = &model_ids;
        let mat_ids   = &mat_ids;
        let n = submeshes.len();

        node(&mut buf, "Connections", &[], move |b| {
            for i in 0..n {
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(model_ids[i]), Prop::I64(0)]);
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(geom_ids[i]),  Prop::I64(model_ids[i])]);
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(mat_ids[i]),   Prop::I64(model_ids[i])]);
            }
        });
    }

    // ---- Footer -------------------------------------------------------------
    buf.extend_from_slice(&[0u8; 13]); // top-level null terminator
    // Magic padding
    buf.extend_from_slice(b"\xfa\xbc\xab\x09\xd0\xc8\xd4\x66\xb1\x76\xfb\x83\x1c\xf7\x26\x7e");
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&7400u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 120]);
    buf.extend_from_slice(b"\xf8\x5a\x8c\x6a\xde\xf5\xd9\x7e\xec\xe9\x0c\xe3\x75\x8f\x29\x0b");

    buf
}
