# Virtual FBX mesh views

Read-only virtual views that expose .pam / .pamlod / .pac mesh files as FBX
inside the FUSE / WinFSP filesystem.  Write-back is out of scope for now.

## Goal

After this work, a user can open any mesh directly in Blender, Maya, etc.:

```
cp /mnt/cd/.pam.fbx/character/cd_phm_basic_00_body.pam.fbx /tmp/body.fbx
blender --python import_fbx.py -- /tmp/body.fbx
```

or drag the virtual file straight from the mount into the DCC tool.

---

## What we have

`cdcore` already parses all three mesh formats into clean Rust structs:

| Format | Parser | Struct | Contains |
|--------|--------|--------|----------|
| `.pam` | `parse_pam()` | `ParsedMesh { submeshes: Vec<SubMesh> }` | vertices, UVs, normals, face indices |
| `.pamlod` | `parse_pamlod()` | same as PAM, LOD 0 only | same |
| `.pac` | `parse_pac()` | `ParsedPac { submeshes: Vec<PacSubMesh> }` | above + bone indices/weights per vertex |

No skeleton (bone hierarchy, bind poses) is available from the mesh file alone;
that lives in a companion `.paa_metabin` file.  The first version will export
geometry and skinning weights without a proper skeleton — imported meshes will
have a flat bone list, not a hierarchy.  Good enough for texture/shape work.

---

## FBX format choice: ASCII FBX 7.4

Binary FBX is undocumented; ASCII FBX is stable since FBX SDK 2013.
Blender, Maya, and 3ds Max all import ASCII FBX reliably.

No external Rust crate is needed.  We write a minimal ASCII FBX serialiser
(~300 lines) covering:

- `FBXHeaderExtension` — version, creator string
- `Geometry` node — vertices, normals, UVs, polygon vertex index
- `Model` node — mesh node linking to Geometry
- `Connections` — Model → RootNode
- For PAC: `Deformer` / `SubDeformer` nodes for skin clusters (weights only,
  identity bind poses since we have no skeleton)

This is sufficient for geometry import.  Animation and a proper skeleton are
future work contingent on parsing `.paa_metabin`.

---

## Implementation plan

### Step 1 — FBX serialiser in cdcore  (`cdcore/src/formats/mesh/fbx.rs`)

New file, pure Rust, no new dependencies.

```rust
pub fn pam_to_fbx(mesh: &ParsedMesh,  name: &str) -> Vec<u8>
pub fn pac_to_fbx(mesh: &ParsedPac,   name: &str) -> Vec<u8>
```

Both emit ASCII FBX bytes.  `pamlod` reuses `pam_to_fbx` (it parses to the
same `ParsedMesh` struct).

Internal helpers:
- `write_geometry(w, submeshes)` — flattens all submeshes into one Geometry node
- `write_skin(w, submeshes)` — emits Deformer/SubDeformer for PAC bone weights
- `unique_bone_names(submeshes)` — derives a flat bone name list from indices

Expose via `cdcore::formats::mesh`:
```rust
pub use fbx::{pam_to_fbx, pac_to_fbx};
```

### Step 2 — New virtual roots in `virtual_files.rs` (both cdfuse and cdwinfs)

Add three entries to `VIRTUAL_ROOTS`:

```rust
(".pam.fbx",    ".pam",    ".fbx"),
(".pamlod.fbx", ".pamlod", ".fbx"),
(".pac.fbx",    ".pac",    ".fbx"),
```

Add three `VirtualKind` variants:
```rust
PamFbx,
PamlodFbx,
PacFbx,
```

### Step 3 — Render functions in `virtual_files.rs`

```rust
pub fn render_pam_fbx(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let name = stem(path);
    let mesh = cdcore::formats::mesh::parse_pam(data, path).ok()?;
    Some(cdcore::formats::mesh::pam_to_fbx(&mesh, &name))
}

pub fn render_pamlod_fbx(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let name = stem(path);
    let mesh = cdcore::formats::mesh::parse_pamlod(data, path).ok()?;
    Some(cdcore::formats::mesh::pam_to_fbx(&mesh, &name))
}

pub fn render_pac_fbx(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let name = stem(path);
    let mesh = cdcore::formats::mesh::parse_pac(data, path).ok()?;
    Some(cdcore::formats::mesh::pac_to_fbx(&mesh, &name))
}
```

Wire these into `decode_inner` in `fs.rs` (both crates) alongside the existing
`PalocJson`, `DdsPng`, etc. match arms.

### Step 4 — File size estimation for getattr / lookup

FBX files are larger than the source binary.  A rough factor of 4–6× the
decompressed mesh size works.  The virtual file size will be estimated as
`orig_size * 5`; once decoded the real size is cached.  Same pattern as `.dds.png`.

### Step 5 — Update README

Add `.pam.fbx/`, `.pamlod.fbx/`, `.pac.fbx/` to the virtual views section.

---

## Out of scope (future)

- Write-back (FBX → PAM/PAC repack)
- Proper skeleton hierarchy (needs `.paa_metabin` correlation)
- Animation curves
- Multiple LODs in a single FBX (currently LOD 0 only)
- glTF output (alternative format, simpler Rust ecosystem support)
