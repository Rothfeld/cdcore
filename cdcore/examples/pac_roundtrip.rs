//! Identity round-trip of a real /cd PAC: parse, build_pac, compare bytes.
//! Run with: cargo run --release --example pac_roundtrip -- character/...pac

use cdcore::formats::mesh::pac::parse as parse_pac;
use cdcore::repack::mesh::{build_pac, ParsedMesh};
use cdcore::VfsManager;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "character/cd_0081_doorstatue.pac".into());
    let v = VfsManager::new("/cd").expect("/cd open");
    v.load_all_groups().expect("load groups");
    let entry = match v.lookup(&path) {
        Some(e) => e,
        None => { eprintln!("not found: {path}"); return; }
    };
    let bytes = v.read_entry(&entry).expect("read");
    println!("source: {} ({} bytes)", path, bytes.len());

    let pac = parse_pac(&bytes, &path).expect("parse_pac");
    println!("submeshes: {} verts={} faces={}", pac.submeshes.len(), pac.total_vertices, pac.total_faces);
    let mesh = ParsedMesh {
        path: pac.path.clone(),
        format: "pac".into(),
        bbox_min: pac.bbox_min,
        bbox_max: pac.bbox_max,
        submeshes: pac.submeshes.iter().map(|p| p.base.clone()).collect(),
        total_vertices: pac.total_vertices,
        total_faces: pac.total_faces,
        has_uvs: pac.has_uvs,
        has_bones: pac.has_bones,
        ..Default::default()
    };
    match build_pac(&mesh, &bytes) {
        Ok(out) => {
            let diff = out.iter().zip(bytes.iter()).filter(|(a, b)| a != b).count();
            println!("identity: bytes_eq={} (out={} src={}) diffs={}",
                out == bytes, out.len(), bytes.len(), diff);
        }
        Err(e) => println!("build_pac error: {e}"),
    }
}
