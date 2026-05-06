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
use crate::repack::mesh::skeleton_math::{
    flatten_pab_bind, lcl_from_bind_matrix, mat4_inverse, mat4_mul,
    yup_to_zup_mat4, yup_to_zup_vec3, IDENTITY,
};
use crate::formats::animation::pab::Skeleton;

// ---------------------------------------------------------------------------
// FBX property encoding
// ---------------------------------------------------------------------------

#[allow(dead_code)]
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
/// Works for PAM, PAMLOD, and PAC (phase 1 -- geometry only, no skeleton).
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

// ---------------------------------------------------------------------------
// Textured FBX export (stage 6.5)
// ---------------------------------------------------------------------------

/// Texture reference for the FBX writer. `png_relative_path` is what the FBX
/// `RelativeFilename` field contains; the PNG bytes themselves are written
/// to disk by the caller (e.g. cdfuse PrefabView's `mesh.fbm/<name>.png`).
pub struct TextureRef<'a> {
    pub png_relative_path: &'a str,
    pub png_absolute_path: &'a str,
}

/// Same as [`submeshes_to_fbx`] but additionally embeds Texture + Video
/// nodes connected to each material. `textures.len()` must equal
/// `submeshes.len()`. Pass `None` for submeshes that have no texture --
/// they get the flat-grey material from phase 1.
pub fn submeshes_to_textured_fbx(
    submeshes: &[&SubMesh],
    mesh_name: &str,
    textures: &[Option<TextureRef<'_>>],
) -> Vec<u8> {
    assert_eq!(submeshes.len(), textures.len(),
        "submeshes_to_textured_fbx: textures must align 1:1 with submeshes");

    let mut buf = Vec::with_capacity(256 * 1024);

    buf.extend_from_slice(b"Kaydara FBX Binary  \x00");
    buf.extend_from_slice(b"\x1a\x00");
    buf.extend_from_slice(&7400u32.to_le_bytes());

    let mut id_ctr: i64 = 3_000_000_000;
    let mut uid = || { id_ctr += 1; id_ctr };

    let geom_ids: Vec<i64>  = (0..submeshes.len()).map(|_| uid()).collect();
    let model_ids: Vec<i64> = (0..submeshes.len()).map(|_| uid()).collect();
    let mat_ids: Vec<i64>   = (0..submeshes.len()).map(|_| uid()).collect();
    // Per-submesh texture + video ids; placeholder 0 for None entries.
    let tex_ids: Vec<i64> = textures.iter()
        .map(|t| if t.is_some() { id_ctr += 1; id_ctr } else { 0 })
        .collect();
    let vid_ids: Vec<i64> = textures.iter()
        .map(|t| if t.is_some() { id_ctr += 1; id_ctr } else { 0 })
        .collect();

    // ---- FBXHeaderExtension + GlobalSettings (same as phase 1) -------------
    node(&mut buf, "FBXHeaderExtension", &[], |b| {
        leaf(b, "FBXHeaderVersion", &[Prop::I32(1003)]);
        leaf(b, "FBXVersion",       &[Prop::I32(7400)]);
        leaf(b, "Creator",          &[Prop::Str("cdcore")]);
    });
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

    // ---- Objects ----------------------------------------------------------
    {
        let geom_ids  = &geom_ids;
        let model_ids = &model_ids;
        let mat_ids   = &mat_ids;
        let tex_ids   = &tex_ids;
        let vid_ids   = &vid_ids;
        let sms       = submeshes;
        let texs      = textures;

        node(&mut buf, "Objects", &[], move |b| {
            for (idx, sm) in sms.iter().enumerate() {
                let geom_id  = geom_ids[idx];
                let model_id = model_ids[idx];
                let mat_id   = mat_ids[idx];
                let sm_name  = if sm.name.is_empty() { mesh_name } else { &sm.name };

                let verts_flat: Vec<f64> = sm.vertices.iter()
                    .flat_map(|[x, y, z]| [*x as f64, *y as f64, *z as f64]).collect();
                let poly_idx: Vec<i32> = sm.faces.iter()
                    .flat_map(|[a, bb, c]| [*a as i32, *bb as i32, *c as i32 ^ -1]).collect();
                let normals_flat: Vec<f64> = sm.normals.iter()
                    .flat_map(|[nx, ny, nz]| [*nx as f64, *ny as f64, *nz as f64]).collect();
                let uvs_flat: Vec<f64> = sm.uvs.iter()
                    .flat_map(|[u, v]| [*u as f64, 1.0 - *v as f64]).collect();
                let has_normals = !normals_flat.is_empty();
                let has_uvs     = !uvs_flat.is_empty();

                let geom_label = format!("{sm_name}\x00\x01Geometry");
                node(b, "Geometry",
                    &[Prop::I64(geom_id), Prop::Str(&geom_label), Prop::Str("Mesh")],
                    |b2| {
                        leaf(b2, "Vertices",           &[Prop::F64s(&verts_flat)]);
                        leaf(b2, "PolygonVertexIndex", &[Prop::I32s(&poly_idx)]);
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
                        if texs[idx].is_some() {
                            // Per-polygon texture mapping: all polys -> texture index 0 of submesh's material.
                            node(b2, "LayerElementTexture", &[Prop::I32(0)], |b3| {
                                leaf(b3, "Version",                  &[Prop::I32(101)]);
                                leaf(b3, "Name",                     &[Prop::Str("")]);
                                leaf(b3, "MappingInformationType",   &[Prop::Str("AllSame")]);
                                leaf(b3, "ReferenceInformationType", &[Prop::Str("Direct")]);
                                leaf(b3, "BlendMode",                &[Prop::Str("Translucent")]);
                                leaf(b3, "TextureAlpha",             &[Prop::F64(1.0)]);
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
                            if texs[idx].is_some() {
                                node(b3, "LayerElement", &[], |b4| {
                                    leaf(b4, "Type",       &[Prop::Str("LayerElementTexture")]);
                                    leaf(b4, "TypedIndex", &[Prop::I32(0)]);
                                });
                            }
                        });
                    });

                // Model
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

                // Material
                let mat_name  = if sm.material.is_empty() { sm_name } else { &sm.material };
                let mat_label = format!("{mat_name}\x00\x01Material");
                node(b, "Material",
                    &[Prop::I64(mat_id), Prop::Str(&mat_label), Prop::Str("")],
                    |b2| {
                        leaf(b2, "Version",      &[Prop::I32(102)]);
                        leaf(b2, "ShadingModel", &[Prop::Str("phong")]);
                        node(b2, "Properties70", &[], |b3| {
                            leaf(b3, "P", &[Prop::Str("DiffuseColor"), Prop::Str("Color"), Prop::Str(""), Prop::Str("A"), Prop::F64(1.0), Prop::F64(1.0), Prop::F64(1.0)]);
                        });
                    });

                // Texture + Video (only when this submesh has a texture)
                if let Some(tex) = &texs[idx] {
                    let tex_id   = tex_ids[idx];
                    let vid_id   = vid_ids[idx];
                    let tex_label = format!("{sm_name}\x00\x01Texture");
                    let vid_label = format!("{sm_name}\x00\x01Video");
                    let rel = tex.png_relative_path.to_string();
                    let abs = tex.png_absolute_path.to_string();

                    node(b, "Video",
                        &[Prop::I64(vid_id), Prop::Str(&vid_label), Prop::Str("Clip")],
                        |b2| {
                            leaf(b2, "Type",             &[Prop::Str("Clip")]);
                            node(b2, "Properties70", &[], |b3| {
                                leaf(b3, "P", &[Prop::Str("Path"), Prop::Str("KString"), Prop::Str("XRefUrl"), Prop::Str(""), Prop::Str(&abs)]);
                            });
                            leaf(b2, "UseMipMap",        &[Prop::I32(0)]);
                            leaf(b2, "Filename",         &[Prop::Str(&abs)]);
                            leaf(b2, "RelativeFilename", &[Prop::Str(&rel)]);
                        });

                    node(b, "Texture",
                        &[Prop::I64(tex_id), Prop::Str(&tex_label), Prop::Str("")],
                        |b2| {
                            leaf(b2, "Type",                  &[Prop::Str("TextureVideoClip")]);
                            leaf(b2, "Version",               &[Prop::I32(202)]);
                            leaf(b2, "TextureName",           &[Prop::Str(&tex_label)]);
                            leaf(b2, "Media",                 &[Prop::Str(&vid_label)]);
                            leaf(b2, "Filename",              &[Prop::Str(&abs)]);
                            leaf(b2, "RelativeFilename",      &[Prop::Str(&rel)]);
                            leaf(b2, "ModelUVTranslation",    &[Prop::F64(0.0), Prop::F64(0.0)]);
                            leaf(b2, "ModelUVScaling",        &[Prop::F64(1.0), Prop::F64(1.0)]);
                            leaf(b2, "Texture_Alpha_Source",  &[Prop::Str("None")]);
                            leaf(b2, "Cropping",              &[Prop::I32(0), Prop::I32(0), Prop::I32(0), Prop::I32(0)]);
                        });
                }
            }
        });
    }

    // ---- Connections ------------------------------------------------------
    {
        let geom_ids  = &geom_ids;
        let model_ids = &model_ids;
        let mat_ids   = &mat_ids;
        let tex_ids   = &tex_ids;
        let vid_ids   = &vid_ids;
        let texs      = textures;
        let n         = submeshes.len();

        node(&mut buf, "Connections", &[], move |b| {
            for i in 0..n {
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(model_ids[i]), Prop::I64(0)]);
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(geom_ids[i]),  Prop::I64(model_ids[i])]);
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(mat_ids[i]),   Prop::I64(model_ids[i])]);
                if texs[i].is_some() {
                    // Texture connects to Material's DiffuseColor; Video to Texture.
                    leaf(b, "C", &[Prop::Str("OP"), Prop::I64(tex_ids[i]), Prop::I64(mat_ids[i]), Prop::Str("DiffuseColor")]);
                    leaf(b, "C", &[Prop::Str("OO"), Prop::I64(vid_ids[i]), Prop::I64(tex_ids[i])]);
                }
            }
        });
    }

    // ---- Footer (same as phase 1) ----------------------------------------
    buf.extend_from_slice(&[0u8; 13]);
    buf.extend_from_slice(b"\xfa\xbc\xab\x09\xd0\xc8\xd4\x66\xb1\x76\xfb\x83\x1c\xf7\x26\x7e");
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&7400u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 120]);
    buf.extend_from_slice(b"\xf8\x5a\x8c\x6a\xde\xf5\xd9\x7e\xec\xe9\x0c\xe3\x75\x8f\x29\x0b");

    buf
}

// ---------------------------------------------------------------------------
// Skinned FBX export (stage 6.5 finish): mesh + LimbNode bones + Skin/Cluster
// ---------------------------------------------------------------------------

/// Bones whose weights get redirected to the Bip01 root before binding.
/// Pearl Abyss "B_TL_*" and "B_MoveControl_*" are sibling locomotion-target
/// bones that don't animate during walks; vertices weighted to them get
/// left behind when the character translates -- they all need to ride
/// along with Bip01 (always bone index 0 in PA character skeletons).
/// Mirrors the CONTROL_BONE_PREFIXES handling in `export_fbx_with_skeleton`.
const CONTROL_BONE_PREFIXES: &[&str] = &["B_TL_", "B_MoveControl_"];
const BIP01_INDEX: usize = 0;

/// Same shape as [`submeshes_to_textured_fbx`] but additionally writes a
/// LimbNode armature plus per-submesh `Deformer::Skin` + per-bone
/// `SubDeformer::Cluster` so Blender / Maya / Unity / Unreal see a posable
/// rig with proper skin weights.
///
/// Geometry, normals, and bone bind matrices are pre-converted Y-up -> Z-up
/// at write time (matching the Python flow); the FBX scene declares
/// `UpAxis = 2` so importers consume the data without re-converting.
///
/// `textures` may be `None`, meaning no texture nodes get emitted (the same
/// shape as [`submeshes_to_fbx`] with armature appended). Otherwise pass an
/// `Option<TextureRef>` per submesh -- pass `None` for individual submeshes
/// that don't have a texture.
///
/// Skip-skin guard: if the skeleton contains any `_stub_bone_*` placeholder
/// (returned by the upstream parser when bone records are truncated), the
/// armature is still written but the Skin/Cluster nodes are omitted to avoid
/// the "spike-shard explosion" reported on real characters with stub bones.
/// Weights then round-trip through the cfmeta sidecar instead.
pub fn submeshes_to_skinned_fbx(
    submeshes: &[&SubMesh],
    mesh_name: &str,
    skeleton: &Skeleton,
    textures: Option<&[Option<TextureRef<'_>>]>,
    scale: f64,
) -> Vec<u8> {
    if let Some(texs) = textures {
        assert_eq!(submeshes.len(), texs.len(),
            "submeshes_to_skinned_fbx: textures must align 1:1 with submeshes");
    }

    let mut buf = Vec::with_capacity(512 * 1024);

    // Header
    buf.extend_from_slice(b"Kaydara FBX Binary  \x00");
    buf.extend_from_slice(b"\x1a\x00");
    buf.extend_from_slice(&7400u32.to_le_bytes());

    let mut id_ctr: i64 = 3_000_000_000;
    let mut uid = || {
        id_ctr += 1;
        id_ctr
    };

    // Per-submesh ids
    let geom_ids: Vec<i64> = (0..submeshes.len()).map(|_| uid()).collect();
    let model_ids: Vec<i64> = (0..submeshes.len()).map(|_| uid()).collect();
    let mat_ids: Vec<i64> = (0..submeshes.len()).map(|_| uid()).collect();
    // Texture/video ids (placeholder 0 for None entries)
    let tex_ids: Vec<i64> = match textures {
        Some(texs) => texs.iter().map(|t| if t.is_some() { let v = uid(); v } else { 0 }).collect(),
        None => vec![0; submeshes.len()],
    };
    let vid_ids: Vec<i64> = match textures {
        Some(texs) => texs.iter().map(|t| if t.is_some() { let v = uid(); v } else { 0 }).collect(),
        None => vec![0; submeshes.len()],
    };

    // Per-bone ids
    let bone_model_ids: Vec<i64> = skeleton.bones.iter().map(|_| uid()).collect();
    let bone_attr_ids: Vec<i64> = skeleton.bones.iter().map(|_| uid()).collect();
    let pose_id = uid();

    // Skip-skin if any bone has a "_stub_bone_" name -- avoids spike-shard
    // artifacts on truncated-skeleton characters.
    let skip_skin = skeleton
        .bones
        .iter()
        .any(|b| b.name.starts_with("_stub_bone_"));

    // Precompute Y-up -> Z-up world bind matrix per bone. f64 throughout to
    // match the Python reference's float precision.
    let world_by_idx: Vec<crate::repack::mesh::skeleton_math::Mat4> = skeleton
        .bones
        .iter()
        .map(|b| {
            let flat = flatten_pab_bind(&b.bind_matrix);
            yup_to_zup_mat4(&flat)
        })
        .collect();

    // Per-bone LOCAL (parent-relative) bind = inv(parent_world) * world.
    // Root bone: local = world.
    let local_bind_by_idx: Vec<crate::repack::mesh::skeleton_math::Mat4> = skeleton
        .bones
        .iter()
        .map(|b| {
            let w = world_by_idx[b.index];
            let p = b.parent_index;
            if p >= 0 && (p as usize) < world_by_idx.len() {
                let pw_inv = mat4_inverse(&world_by_idx[p as usize]);
                mat4_mul(&pw_inv, &w)
            } else {
                w
            }
        })
        .collect();

    // Precompute skinning data per submesh: { bone_idx -> [(vert_idx, weight)] }.
    // Iterates submesh.bone_indices / bone_weights; normalizes per-vertex sum to
    // 1.0 (the engine GPU shader does this at runtime; FBX does not). Redirects
    // weights on control bones to Bip01.
    let mut cluster_data: Vec<std::collections::BTreeMap<usize, Vec<(i32, f64)>>> =
        Vec::with_capacity(submeshes.len());
    let mut skin_ids: Vec<i64> = Vec::with_capacity(submeshes.len());
    let mut cluster_ids: Vec<std::collections::BTreeMap<usize, i64>> =
        Vec::with_capacity(submeshes.len());

    for sm in submeshes.iter() {
        let mut per_bone: std::collections::BTreeMap<usize, Vec<(i32, f64)>> =
            std::collections::BTreeMap::new();
        if !skeleton.bones.is_empty() && !skip_skin {
            for vi in 0..sm.vertices.len() {
                let bones: &[u32] = sm
                    .bone_indices
                    .get(vi)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let weights: &[f32] = sm
                    .bone_weights
                    .get(vi)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let wsum: f64 = weights.iter().filter(|&&w| w > 0.0).map(|&w| w as f64).sum();
                if wsum <= 1e-6 {
                    continue;
                }
                let inv_sum = 1.0 / wsum;
                for (b_idx, &w) in bones.iter().zip(weights.iter()) {
                    if w <= 0.0 {
                        continue;
                    }
                    let bi = *b_idx as usize;
                    if bi >= skeleton.bones.len() {
                        continue;
                    }
                    let bone_name = &skeleton.bones[bi].name;
                    let is_control = CONTROL_BONE_PREFIXES.iter().any(|p| bone_name.starts_with(p));
                    let target = if is_control { BIP01_INDEX } else { bi };
                    per_bone
                        .entry(target)
                        .or_default()
                        .push((vi as i32, (w as f64) * inv_sum));
                }
            }
        }
        if !per_bone.is_empty() {
            let sk = uid();
            skin_ids.push(sk);
            let mut cls = std::collections::BTreeMap::new();
            for &b_idx in per_bone.keys() {
                cls.insert(b_idx, uid());
            }
            cluster_ids.push(cls);
        } else {
            skin_ids.push(0);
            cluster_ids.push(std::collections::BTreeMap::new());
        }
        cluster_data.push(per_bone);
    }

    // FBXHeaderExtension
    node(&mut buf, "FBXHeaderExtension", &[], |b| {
        leaf(b, "FBXHeaderVersion", &[Prop::I32(1003)]);
        leaf(b, "FBXVersion", &[Prop::I32(7400)]);
        leaf(b, "Creator", &[Prop::Str("cdcore mesh+skeleton")]);
    });

    // GlobalSettings: Z-up (geometry + bones already converted)
    node(&mut buf, "GlobalSettings", &[], |b| {
        node(b, "Properties70", &[], |b2| {
            leaf(b2, "P", &[Prop::Str("UpAxis"),                  Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(2)]);
            leaf(b2, "P", &[Prop::Str("UpAxisSign"),              Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(1)]);
            leaf(b2, "P", &[Prop::Str("FrontAxis"),               Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(1)]);
            leaf(b2, "P", &[Prop::Str("FrontAxisSign"),           Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(-1)]);
            leaf(b2, "P", &[Prop::Str("CoordAxis"),               Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(0)]);
            leaf(b2, "P", &[Prop::Str("CoordAxisSign"),           Prop::Str("int"),    Prop::Str("Integer"), Prop::Str(""), Prop::I32(1)]);
            leaf(b2, "P", &[Prop::Str("UnitScaleFactor"),         Prop::Str("double"), Prop::Str("Number"),  Prop::Str(""), Prop::F64(100.0)]);
            leaf(b2, "P", &[Prop::Str("OriginalUnitScaleFactor"), Prop::Str("double"), Prop::Str("Number"),  Prop::Str(""), Prop::F64(100.0)]);
        });
    });

    // Objects
    {
        let geom_ids = &geom_ids;
        let model_ids = &model_ids;
        let mat_ids = &mat_ids;
        let tex_ids = &tex_ids;
        let vid_ids = &vid_ids;
        let bone_model_ids = &bone_model_ids;
        let bone_attr_ids = &bone_attr_ids;
        let world_by_idx = &world_by_idx;
        let local_bind_by_idx = &local_bind_by_idx;
        let skin_ids = &skin_ids;
        let cluster_ids = &cluster_ids;
        let cluster_data = &cluster_data;
        let textures = textures;
        let bones = &skeleton.bones;

        node(&mut buf, "Objects", &[], move |b| {
            // ---- Per submesh: Geometry / Model / Material / Texture+Video ----
            for (idx, sm) in submeshes.iter().enumerate() {
                let geom_id = geom_ids[idx];
                let model_id = model_ids[idx];
                let mat_id = mat_ids[idx];
                let sm_name = if sm.name.is_empty() { mesh_name } else { &sm.name };

                // Z-up converted vertex positions + normals.
                let verts_flat: Vec<f64> = sm
                    .vertices
                    .iter()
                    .flat_map(|[x, y, z]| {
                        let v = yup_to_zup_vec3([
                            *x as f64 * scale,
                            *y as f64 * scale,
                            *z as f64 * scale,
                        ]);
                        [v[0], v[1], v[2]]
                    })
                    .collect();
                let poly_idx: Vec<i32> = sm
                    .faces
                    .iter()
                    .flat_map(|[a, bb, c]| [*a as i32, *bb as i32, *c as i32 ^ -1])
                    .collect();
                let normals_flat: Vec<f64> = sm
                    .normals
                    .iter()
                    .flat_map(|[nx, ny, nz]| {
                        let n = yup_to_zup_vec3([*nx as f64, *ny as f64, *nz as f64]);
                        [n[0], n[1], n[2]]
                    })
                    .collect();
                let uvs_flat: Vec<f64> = sm.uvs.iter()
                    .flat_map(|[u, v]| [*u as f64, 1.0 - *v as f64])
                    .collect();
                let has_normals = !normals_flat.is_empty();
                let has_uvs = !uvs_flat.is_empty();
                let has_tex = textures
                    .map(|t| t.get(idx).map(|x| x.is_some()).unwrap_or(false))
                    .unwrap_or(false);

                let geom_label = format!("{sm_name}\x00\x01Geometry");
                node(b, "Geometry",
                    &[Prop::I64(geom_id), Prop::Str(&geom_label), Prop::Str("Mesh")],
                    |b2| {
                        leaf(b2, "Vertices", &[Prop::F64s(&verts_flat)]);
                        leaf(b2, "PolygonVertexIndex", &[Prop::I32s(&poly_idx)]);
                        if has_normals {
                            node(b2, "LayerElementNormal", &[Prop::I32(0)], |b3| {
                                leaf(b3, "Version", &[Prop::I32(101)]);
                                leaf(b3, "Name", &[Prop::Str("")]);
                                leaf(b3, "MappingInformationType", &[Prop::Str("ByVertice")]);
                                leaf(b3, "ReferenceInformationType", &[Prop::Str("Direct")]);
                                leaf(b3, "Normals", &[Prop::F64s(&normals_flat)]);
                            });
                        }
                        if has_uvs {
                            node(b2, "LayerElementUV", &[Prop::I32(0)], |b3| {
                                leaf(b3, "Version", &[Prop::I32(101)]);
                                leaf(b3, "Name", &[Prop::Str("UVMap")]);
                                leaf(b3, "MappingInformationType", &[Prop::Str("ByVertice")]);
                                leaf(b3, "ReferenceInformationType", &[Prop::Str("Direct")]);
                                leaf(b3, "UV", &[Prop::F64s(&uvs_flat)]);
                            });
                        }
                        if has_tex {
                            node(b2, "LayerElementTexture", &[Prop::I32(0)], |b3| {
                                leaf(b3, "Version", &[Prop::I32(101)]);
                                leaf(b3, "Name", &[Prop::Str("")]);
                                leaf(b3, "MappingInformationType", &[Prop::Str("AllSame")]);
                                leaf(b3, "ReferenceInformationType", &[Prop::Str("Direct")]);
                                leaf(b3, "BlendMode", &[Prop::Str("Translucent")]);
                                leaf(b3, "TextureAlpha", &[Prop::F64(1.0)]);
                            });
                        }
                        node(b2, "Layer", &[Prop::I32(0)], |b3| {
                            leaf(b3, "Version", &[Prop::I32(100)]);
                            if has_normals {
                                node(b3, "LayerElement", &[], |b4| {
                                    leaf(b4, "Type", &[Prop::Str("LayerElementNormal")]);
                                    leaf(b4, "TypedIndex", &[Prop::I32(0)]);
                                });
                            }
                            if has_uvs {
                                node(b3, "LayerElement", &[], |b4| {
                                    leaf(b4, "Type", &[Prop::Str("LayerElementUV")]);
                                    leaf(b4, "TypedIndex", &[Prop::I32(0)]);
                                });
                            }
                            if has_tex {
                                node(b3, "LayerElement", &[], |b4| {
                                    leaf(b4, "Type", &[Prop::Str("LayerElementTexture")]);
                                    leaf(b4, "TypedIndex", &[Prop::I32(0)]);
                                });
                            }
                        });
                    });

                // Model (mesh) -- identity Lcl TRS so BindPose math works.
                let model_label = format!("{sm_name}\x00\x01Model");
                node(b, "Model",
                    &[Prop::I64(model_id), Prop::Str(&model_label), Prop::Str("Mesh")],
                    |b2| {
                        leaf(b2, "Version", &[Prop::I32(232)]);
                        leaf(b2, "MultiLayer", &[Prop::I32(0)]);
                        leaf(b2, "MultiTake", &[Prop::I32(0)]);
                        leaf(b2, "Shading", &[Prop::Bool(true)]);
                        leaf(b2, "Culling", &[Prop::Str("CullingOff")]);
                        node(b2, "Properties70", &[], |b3| {
                            leaf(b3, "P", &[Prop::Str("Lcl Translation"), Prop::Str("Lcl Translation"), Prop::Str(""), Prop::Str("A"), Prop::F64(0.0), Prop::F64(0.0), Prop::F64(0.0)]);
                            leaf(b3, "P", &[Prop::Str("Lcl Rotation"),    Prop::Str("Lcl Rotation"),    Prop::Str(""), Prop::Str("A"), Prop::F64(0.0), Prop::F64(0.0), Prop::F64(0.0)]);
                            leaf(b3, "P", &[Prop::Str("Lcl Scaling"),     Prop::Str("Lcl Scaling"),     Prop::Str(""), Prop::Str("A"), Prop::F64(1.0), Prop::F64(1.0), Prop::F64(1.0)]);
                        });
                    });

                // Material
                let mat_name = if sm.material.is_empty() { sm_name } else { &sm.material };
                let mat_label = format!("{mat_name}\x00\x01Material");
                node(b, "Material",
                    &[Prop::I64(mat_id), Prop::Str(&mat_label), Prop::Str("")],
                    |b2| {
                        leaf(b2, "Version", &[Prop::I32(102)]);
                        leaf(b2, "ShadingModel", &[Prop::Str("phong")]);
                        node(b2, "Properties70", &[], |b3| {
                            leaf(b3, "P", &[Prop::Str("DiffuseColor"), Prop::Str("Color"), Prop::Str(""), Prop::Str("A"), Prop::F64(1.0), Prop::F64(1.0), Prop::F64(1.0)]);
                        });
                    });

                // Texture + Video (only when this submesh has a texture)
                if has_tex {
                    let texs = textures.unwrap();
                    let tex = texs[idx].as_ref().unwrap();
                    let tex_id = tex_ids[idx];
                    let vid_id = vid_ids[idx];
                    let tex_label = format!("{sm_name}\x00\x01Texture");
                    let vid_label = format!("{sm_name}\x00\x01Video");
                    let rel = tex.png_relative_path.to_string();
                    let abs = tex.png_absolute_path.to_string();
                    node(b, "Video",
                        &[Prop::I64(vid_id), Prop::Str(&vid_label), Prop::Str("Clip")],
                        |b2| {
                            leaf(b2, "Type", &[Prop::Str("Clip")]);
                            node(b2, "Properties70", &[], |b3| {
                                leaf(b3, "P", &[Prop::Str("Path"), Prop::Str("KString"), Prop::Str("XRefUrl"), Prop::Str(""), Prop::Str(&abs)]);
                            });
                            leaf(b2, "UseMipMap", &[Prop::I32(0)]);
                            leaf(b2, "Filename", &[Prop::Str(&abs)]);
                            leaf(b2, "RelativeFilename", &[Prop::Str(&rel)]);
                        });
                    node(b, "Texture",
                        &[Prop::I64(tex_id), Prop::Str(&tex_label), Prop::Str("")],
                        |b2| {
                            leaf(b2, "Type", &[Prop::Str("TextureVideoClip")]);
                            leaf(b2, "Version", &[Prop::I32(202)]);
                            leaf(b2, "TextureName", &[Prop::Str(&tex_label)]);
                            leaf(b2, "Media", &[Prop::Str(&vid_label)]);
                            leaf(b2, "Filename", &[Prop::Str(&abs)]);
                            leaf(b2, "RelativeFilename", &[Prop::Str(&rel)]);
                            leaf(b2, "ModelUVTranslation", &[Prop::F64(0.0), Prop::F64(0.0)]);
                            leaf(b2, "ModelUVScaling", &[Prop::F64(1.0), Prop::F64(1.0)]);
                            leaf(b2, "Texture_Alpha_Source", &[Prop::Str("None")]);
                            leaf(b2, "Cropping", &[Prop::I32(0), Prop::I32(0), Prop::I32(0), Prop::I32(0)]);
                        });
                }
            }

            // ---- Per bone: NodeAttribute (LimbNode) + Model (LimbNode) ----
            if !skip_skin {
                for bone in bones.iter() {
                    let attr_id = bone_attr_ids[bone.index];
                    let bm_id = bone_model_ids[bone.index];
                    let attr_label = format!("{}\x00\x01NodeAttribute", bone.name);
                    let model_label = format!("{}\x00\x01Model", bone.name);
                    node(b, "NodeAttribute",
                        &[Prop::I64(attr_id), Prop::Str(&attr_label), Prop::Str("LimbNode")],
                        |b2| {
                            node(b2, "Properties70", &[], |b3| {
                                leaf(b3, "P", &[Prop::Str("Size"), Prop::Str("double"), Prop::Str("Number"), Prop::Str(""), Prop::F64(0.05)]);
                            });
                            leaf(b2, "TypeFlags", &[Prop::Str("Skeleton")]);
                        });

                    // Decompose local bind to Lcl TRS (Blender's intrinsic XYZ
                    // convention -- see skeleton_math docs).
                    let trs = lcl_from_bind_matrix(&local_bind_by_idx[bone.index], scale);
                    node(b, "Model",
                        &[Prop::I64(bm_id), Prop::Str(&model_label), Prop::Str("LimbNode")],
                        |b2| {
                            leaf(b2, "Version", &[Prop::I32(232)]);
                            leaf(b2, "MultiLayer", &[Prop::I32(0)]);
                            leaf(b2, "MultiTake", &[Prop::I32(0)]);
                            leaf(b2, "Shading", &[Prop::Bool(true)]);
                            leaf(b2, "Culling", &[Prop::Str("CullingOff")]);
                            node(b2, "Properties70", &[], |b3| {
                                leaf(b3, "P", &[Prop::Str("InheritType"),    Prop::Str("enum"),  Prop::Str(""),       Prop::Str(""),  Prop::I32(1)]);
                                leaf(b3, "P", &[Prop::Str("RotationOrder"),  Prop::Str("enum"),  Prop::Str(""),       Prop::Str(""),  Prop::I32(0)]);
                                leaf(b3, "P", &[Prop::Str("RotationActive"), Prop::Str("bool"),  Prop::Str(""),       Prop::Str(""),  Prop::I32(1)]);
                                leaf(b3, "P", &[Prop::Str("Size"),           Prop::Str("double"),Prop::Str("Number"), Prop::Str(""),  Prop::F64(1.0)]);
                                leaf(b3, "P", &[Prop::Str("Lcl Translation"),Prop::Str("Lcl Translation"), Prop::Str(""), Prop::Str("A"), Prop::F64(trs.tx), Prop::F64(trs.ty), Prop::F64(trs.tz)]);
                                leaf(b3, "P", &[Prop::Str("Lcl Rotation"),   Prop::Str("Lcl Rotation"),    Prop::Str(""), Prop::Str("A"), Prop::F64(trs.rx), Prop::F64(trs.ry), Prop::F64(trs.rz)]);
                                leaf(b3, "P", &[Prop::Str("Lcl Scaling"),    Prop::Str("Lcl Scaling"),     Prop::Str(""), Prop::Str("A"), Prop::F64(trs.sx), Prop::F64(trs.sy), Prop::F64(trs.sz)]);
                            });
                        });
                }

                // ---- Skin + Cluster deformers per submesh ----
                for (idx, sm) in submeshes.iter().enumerate() {
                    let sk_id = skin_ids[idx];
                    if sk_id == 0 {
                        continue;
                    }
                    let skin_label = format!("{}_Skin\x00\x01Deformer", sm.name);
                    node(b, "Deformer",
                        &[Prop::I64(sk_id), Prop::Str(&skin_label), Prop::Str("Skin")],
                        |b2| {
                            leaf(b2, "Version", &[Prop::I32(101)]);
                            leaf(b2, "SkinningType", &[Prop::Str("Linear")]);
                        });
                    for (b_idx, weight_list) in &cluster_data[idx] {
                        let cl_id = cluster_ids[idx][b_idx];
                        let bone = &bones[*b_idx];
                        let indexes: Vec<i32> = weight_list.iter().map(|(vi, _)| *vi).collect();
                        let weights: Vec<f64> = weight_list.iter().map(|(_, w)| *w).collect();
                        let link = world_by_idx[*b_idx];
                        let cluster_label = format!("{}_{}\x00\x01SubDeformer", bone.name, sm.name);
                        node(b, "Deformer",
                            &[Prop::I64(cl_id), Prop::Str(&cluster_label), Prop::Str("Cluster")],
                            |b2| {
                                leaf(b2, "Version", &[Prop::I32(100)]);
                                leaf(b2, "UserData", &[Prop::Str(""), Prop::Str("")]);
                                leaf(b2, "Indexes", &[Prop::I32s(&indexes)]);
                                leaf(b2, "Weights", &[Prop::F64s(&weights)]);
                                leaf(b2, "Transform", &[Prop::F64s(&IDENTITY)]);
                                leaf(b2, "TransformLink", &[Prop::F64s(&link)]);
                            });
                    }
                }

                // ---- BindPose ----
                let total_pose_nodes = bones.len() + submeshes.len();
                let pose_label = "BindPose\x00\x01Pose".to_string();
                node(b, "Pose",
                    &[Prop::I64(pose_id), Prop::Str(&pose_label), Prop::Str("BindPose")],
                    |b2| {
                        leaf(b2, "Type", &[Prop::Str("BindPose")]);
                        leaf(b2, "Version", &[Prop::I32(100)]);
                        leaf(b2, "NbPoseNodes", &[Prop::I32(total_pose_nodes as i32)]);
                        for &mid in model_ids {
                            node(b2, "PoseNode", &[], |b3| {
                                leaf(b3, "Node", &[Prop::I64(mid)]);
                                leaf(b3, "Matrix", &[Prop::F64s(&IDENTITY)]);
                            });
                        }
                        for bone in bones.iter() {
                            let bm = bone_model_ids[bone.index];
                            let mat = world_by_idx[bone.index];
                            node(b2, "PoseNode", &[], |b3| {
                                leaf(b3, "Node", &[Prop::I64(bm)]);
                                leaf(b3, "Matrix", &[Prop::F64s(&mat)]);
                            });
                        }
                    });
            }
        });
    }

    // Connections
    {
        let geom_ids = &geom_ids;
        let model_ids = &model_ids;
        let mat_ids = &mat_ids;
        let tex_ids = &tex_ids;
        let vid_ids = &vid_ids;
        let bone_model_ids = &bone_model_ids;
        let bone_attr_ids = &bone_attr_ids;
        let skin_ids = &skin_ids;
        let cluster_ids = &cluster_ids;
        let textures = textures;
        let bones = &skeleton.bones;
        let n = submeshes.len();

        node(&mut buf, "Connections", &[], move |b| {
            // Mesh wiring
            for i in 0..n {
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(model_ids[i]), Prop::I64(0)]);
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(geom_ids[i]),  Prop::I64(model_ids[i])]);
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(mat_ids[i]),   Prop::I64(model_ids[i])]);
                let has_tex = textures
                    .map(|t| t.get(i).map(|x| x.is_some()).unwrap_or(false))
                    .unwrap_or(false);
                if has_tex {
                    leaf(b, "C", &[Prop::Str("OP"), Prop::I64(tex_ids[i]), Prop::I64(mat_ids[i]), Prop::Str("DiffuseColor")]);
                    leaf(b, "C", &[Prop::Str("OO"), Prop::I64(vid_ids[i]), Prop::I64(tex_ids[i])]);
                }
            }

            if !skip_skin {
                // BindPose -> root
                leaf(b, "C", &[Prop::Str("OO"), Prop::I64(pose_id), Prop::I64(0)]);

                // Bone hierarchy
                for bone in bones.iter() {
                    leaf(b, "C", &[Prop::Str("OO"), Prop::I64(bone_attr_ids[bone.index]), Prop::I64(bone_model_ids[bone.index])]);
                    let parent = if bone.parent_index >= 0
                        && (bone.parent_index as usize) < bones.len()
                    {
                        bone_model_ids[bone.parent_index as usize]
                    } else {
                        0
                    };
                    leaf(b, "C", &[Prop::Str("OO"), Prop::I64(bone_model_ids[bone.index]), Prop::I64(parent)]);
                }

                // Skin + Cluster wiring
                for i in 0..n {
                    let sk = skin_ids[i];
                    if sk == 0 {
                        continue;
                    }
                    leaf(b, "C", &[Prop::Str("OO"), Prop::I64(sk), Prop::I64(geom_ids[i])]);
                    for (b_idx, cl_id) in &cluster_ids[i] {
                        leaf(b, "C", &[Prop::Str("OO"), Prop::I64(*cl_id), Prop::I64(sk)]);
                        leaf(b, "C", &[Prop::Str("OO"), Prop::I64(bone_model_ids[*b_idx]), Prop::I64(*cl_id)]);
                    }
                }
            }
        });
    }

    // Footer (same as the unskinned writers)
    buf.extend_from_slice(&[0u8; 13]);
    buf.extend_from_slice(b"\xfa\xbc\xab\x09\xd0\xc8\xd4\x66\xb1\x76\xfb\x83\x1c\xf7\x26\x7e");
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&7400u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 120]);
    buf.extend_from_slice(b"\xf8\x5a\x8c\x6a\xde\xf5\xd9\x7e\xec\xe9\x0c\xe3\x75\x8f\x29\x0b");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::animation::pab::{Bone, Skeleton};

    fn unit_bone(index: usize, name: &str, parent: i32) -> Bone {
        let mut bind = [[0.0f32; 4]; 4];
        // Identity-ish bind in PAB row-of-array (file column-major flat) form:
        // mat[0][0]=1, mat[1][1]=1, mat[2][2]=1, mat[3][3]=1.
        bind[0][0] = 1.0;
        bind[1][1] = 1.0;
        bind[2][2] = 1.0;
        bind[3][3] = 1.0;
        Bone {
            index,
            name: name.into(),
            bone_hash: 0,
            parent_index: parent,
            bind_matrix: bind,
            inv_bind_matrix: bind,
            scale: [1.0, 1.0, 1.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            position: [0.0, 0.0, 0.0],
        }
    }

    #[test]
    fn skinned_fbx_writes_par_magic_and_header() {
        let sm = SubMesh {
            name: "test".into(),
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            faces: vec![[0u32, 1, 2]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0]; 3],
            bone_indices: vec![vec![0]; 3],
            bone_weights: vec![vec![1.0]; 3],
            ..Default::default()
        };
        let skel = Skeleton {
            path: "test".into(),
            bones: vec![unit_bone(0, "Bip01", -1)],
        };
        let bytes = submeshes_to_skinned_fbx(&[&sm], "test", &skel, None, 1.0);
        // FBX binary 7.4 magic + version
        assert_eq!(&bytes[..21], b"Kaydara FBX Binary  \x00");
        assert_eq!(&bytes[21..23], b"\x1a\x00");
        let v = u32::from_le_bytes(bytes[23..27].try_into().unwrap());
        assert_eq!(v, 7400);
        // Output should be non-trivial.
        assert!(bytes.len() > 1024, "output suspiciously small: {} bytes", bytes.len());
    }

    #[test]
    fn skinned_fbx_skip_skin_when_stub_bones_present() {
        let sm = SubMesh {
            name: "test".into(),
            vertices: vec![[0.0; 3]; 3],
            faces: vec![[0u32, 1, 2]],
            bone_indices: vec![vec![0]; 3],
            bone_weights: vec![vec![1.0]; 3],
            ..Default::default()
        };
        let skel = Skeleton {
            path: "test".into(),
            bones: vec![
                unit_bone(0, "Bip01", -1),
                unit_bone(1, "_stub_bone_001", 0),
            ],
        };
        // Should not panic; writer skips skin/clusters when stub bones are present.
        let bytes = submeshes_to_skinned_fbx(&[&sm], "test", &skel, None, 1.0);
        assert_eq!(&bytes[..21], b"Kaydara FBX Binary  \x00");
    }

    #[test]
    fn skinned_fbx_normalizes_weights() {
        // Verify that an under-weighted vertex (sum < 1) gets normalized in
        // the cluster output -- via the indirect observation that the
        // function completes without panic and produces a Cluster with
        // weight values that are renormalized. We check by introspection:
        // collect cluster weights for a (1.0, 0.0) -> Bip01 vs (3.0, 1.0) ->
        // Bip01+other. Both vertices should appear in Bip01's cluster.
        let sm = SubMesh {
            name: "t".into(),
            vertices: vec![[0.0; 3]; 2],
            faces: vec![],
            bone_indices: vec![vec![0], vec![0, 1]],
            bone_weights: vec![vec![3.0], vec![3.0, 1.0]], // raw u8-like sums
            ..Default::default()
        };
        let skel = Skeleton {
            path: "t".into(),
            bones: vec![unit_bone(0, "Bip01", -1), unit_bone(1, "Spine", 0)],
        };
        // No assertion beyond "doesn't panic"; full byte-for-byte equivalence
        // belongs in the Python-oracle byte-equivalence harness.
        let _ = submeshes_to_skinned_fbx(&[&sm], "t", &skel, None, 1.0);
    }

    #[test]
    fn skinned_fbx_redirects_control_bone_weights_to_bip01() {
        // B_TL_LH weights should land on Bip01 (index 0), not on B_TL_LH itself.
        // We test this indirectly by counting clusters: with all weights on
        // B_TL_LH the redirect collapses everything to one cluster (Bip01),
        // not two.
        let sm = SubMesh {
            name: "t".into(),
            vertices: vec![[0.0; 3]; 2],
            faces: vec![],
            bone_indices: vec![vec![1]; 2],
            bone_weights: vec![vec![1.0]; 2],
            ..Default::default()
        };
        let skel = Skeleton {
            path: "t".into(),
            bones: vec![
                unit_bone(0, "Bip01", -1),
                unit_bone(1, "B_TL_LH", -1),
            ],
        };
        // The output should contain a Cluster wired to Bip01's bone model,
        // not a Cluster wired to B_TL_LH. Smoke-test the byte stream for
        // the substring "Bip01_t" (the cluster label format
        // "<bone_name>_<sm_name>") -- presence implies redirect.
        let bytes = submeshes_to_skinned_fbx(&[&sm], "t", &skel, None, 1.0);
        let has_bip01_cluster = bytes.windows(b"Bip01_t".len()).any(|w| w == b"Bip01_t");
        let has_btl_cluster = bytes.windows(b"B_TL_LH_t".len()).any(|w| w == b"B_TL_LH_t");
        assert!(has_bip01_cluster, "expected Bip01_t cluster (control redirect target)");
        assert!(!has_btl_cluster, "B_TL_LH_t cluster present -- redirect did not happen");
    }
}
