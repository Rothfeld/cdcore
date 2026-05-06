//! `<file>.cfmeta.json` sidecar reader.
//!
//! The sidecar travels alongside `.obj` and `.fbx` exports and carries data
//! that those formats can't natively represent (skin bindings, source-vertex
//! provenance, spike-vertex preservation list, original VFS path).
//!
//! Schema versions:
//!   v1 -- per-submesh: name, vertex_count, bone_indices, bone_weights.
//!         No source_vertex_map (identity assumed).
//!   v2 -- adds source_vertex_map (each export-vertex's original PAC slot)
//!         and filtered_vertices (spike donor records preserved verbatim).
//!
//! Both versions deserialize to the same Rust struct; absent v2 fields stay
//! empty. Returns `None` (not an error) when the sidecar is missing or
//! malformed -- matches the Python convention that callers must tolerate
//! the no-sidecar case.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CfmetaSubmesh {
    pub name: String,
    pub vertex_count: usize,
    /// Per-vertex bone indices. Outer length matches `vertex_count`; each
    /// inner slice is the bone-id list for that vertex (variable arity).
    pub bone_indices: Vec<Vec<u32>>,
    /// Per-vertex bone weights, parallel to `bone_indices`.
    pub bone_weights: Vec<Vec<f32>>,
    /// v2 only: each export-vertex's original PAC vertex slot, or -1 when
    /// the vertex was added after export (e.g. user inserted geometry).
    pub source_vertex_map: Vec<i64>,
    /// v2 only: spike donor records preserved verbatim across the round-trip.
    /// Opaque to the importer; passed through to the PAC builder.
    pub filtered_vertices: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Cfmeta {
    pub schema_version: u32,
    pub source_path: String,
    pub source_format: String,
    pub submeshes: Vec<CfmetaSubmesh>,
}

/// Read `<obj_path>.cfmeta.json` if it exists. Returns `None` on any error
/// (missing file, invalid JSON, unsupported schema). Callers MUST treat the
/// no-sidecar case as "no skin info available".
pub fn load_sidecar(obj_path: &Path) -> Option<Cfmeta> {
    let mut sidecar_path = obj_path.as_os_str().to_owned();
    sidecar_path.push(".cfmeta.json");
    let sidecar_path = std::path::PathBuf::from(sidecar_path);
    if !sidecar_path.is_file() {
        return None;
    }
    let bytes = match std::fs::read(&sidecar_path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("failed to read cfmeta sidecar {}: {e}", sidecar_path.display());
            return None;
        }
    };
    let parsed: Cfmeta = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("invalid JSON in cfmeta sidecar {}: {e}", sidecar_path.display());
            return None;
        }
    };
    if parsed.schema_version != 1 && parsed.schema_version != 2 {
        log::warn!(
            "cfmeta sidecar {} has unsupported schema_version {}",
            sidecar_path.display(),
            parsed.schema_version
        );
        return None;
    }
    Some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cdml_cfmeta_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn missing_sidecar_returns_none() {
        let dir = std::env::temp_dir().join(format!("cdml_cfmeta_missing_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let obj = dir.join("nope.obj");
        assert!(load_sidecar(&obj).is_none());
    }

    #[test]
    fn v1_sidecar_round_trips() {
        let obj = write_tmp(
            "x.obj",
            "# this is the obj, the sidecar lives at x.obj.cfmeta.json",
        );
        let _ = write_tmp(
            "x.obj.cfmeta.json",
            r#"{
                "schema_version": 1,
                "source_path": "character/cha00100/cha00100.pac",
                "source_format": "pac",
                "submeshes": [
                    {
                        "name": "body",
                        "vertex_count": 2,
                        "bone_indices": [[0, 1], [2, 3]],
                        "bone_weights": [[1.0, 0.0], [0.5, 0.5]]
                    }
                ]
            }"#,
        );
        let c = load_sidecar(&obj).expect("should parse");
        assert_eq!(c.schema_version, 1);
        assert_eq!(c.source_path, "character/cha00100/cha00100.pac");
        assert_eq!(c.source_format, "pac");
        assert_eq!(c.submeshes.len(), 1);
        let sm = &c.submeshes[0];
        assert_eq!(sm.name, "body");
        assert_eq!(sm.vertex_count, 2);
        assert_eq!(sm.bone_indices, vec![vec![0u32, 1], vec![2, 3]]);
        assert_eq!(sm.bone_weights, vec![vec![1.0f32, 0.0], vec![0.5, 0.5]]);
        // v1 -> v2 fields stay empty, not Err.
        assert!(sm.source_vertex_map.is_empty());
    }

    #[test]
    fn v2_sidecar_with_source_map() {
        let obj = write_tmp(
            "y.obj",
            "# obj for v2 sidecar test",
        );
        let _ = write_tmp(
            "y.obj.cfmeta.json",
            r#"{
                "schema_version": 2,
                "source_path": "weapon/sword_001.pac",
                "source_format": "pac",
                "submeshes": [
                    {
                        "name": "blade",
                        "vertex_count": 3,
                        "bone_indices": [[], [], []],
                        "bone_weights": [[], [], []],
                        "source_vertex_map": [0, 1, -1],
                        "filtered_vertices": []
                    }
                ]
            }"#,
        );
        let c = load_sidecar(&obj).expect("should parse v2");
        assert_eq!(c.schema_version, 2);
        let sm = &c.submeshes[0];
        assert_eq!(sm.source_vertex_map, vec![0, 1, -1]);
    }

    #[test]
    fn unsupported_schema_returns_none() {
        let obj = write_tmp("z.obj", "");
        let _ = write_tmp(
            "z.obj.cfmeta.json",
            r#"{ "schema_version": 99, "submeshes": [] }"#,
        );
        assert!(load_sidecar(&obj).is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        let obj = write_tmp("w.obj", "");
        let _ = write_tmp("w.obj.cfmeta.json", "not json {{{");
        assert!(load_sidecar(&obj).is_none());
    }
}
