//! Write-side mesh module: OBJ/FBX import + PAC/PAM/PAMLOD binary build.
//!
//! Mirrors the public surface of `core/mesh_importer.py`:
//!   - `import_obj(path) -> ParsedMesh`              (stage 3, TODO)
//!   - `import_fbx(path) -> ParsedMesh`              (stage 7, TODO)
//!   - `build_pam(mesh, original) -> Vec<u8>`        (stage 4, TODO)
//!   - `build_pamlod(mesh, original) -> Vec<u8>`     (stage 5, TODO)
//!   - `build_pac(mesh, original) -> Vec<u8>`        (stage 6, TODO)
//!   - `build_mesh(mesh, original) -> Vec<u8>`       dispatch on `mesh.format`
//!
//! ParsedMesh / SubMesh / MeshVertex are re-exported from the read-side
//! `cdcore::formats::mesh::pam` module so reader and writer share one struct
//! definition (matches the Python convention where mesh_importer.py uses
//! mesh_parser.ParsedMesh directly).

pub mod cfmeta;
pub mod donor;
pub mod fbx_import;
pub mod layout;
pub mod obj_import;
pub mod pac_builder;
pub mod pam_builder;
pub mod pam_local;
pub mod pamlod_builder;
pub mod quant;
pub mod skeleton_math;
pub mod spatial_hash;

pub use fbx_import::{import_fbx, parse_fbx, FbxNode, FbxProp};
pub use obj_import::import_obj;
pub use pac_builder::build_pac;
pub use pam_builder::{build_pam, PamBuildError};
pub use pamlod_builder::build_pamlod;

pub use crate::formats::mesh::pam::{ParsedMesh, SubMesh};
