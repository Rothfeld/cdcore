//! Synthetic per-prefab subtree at `/_prefabs/<stem>/...`.
//!
//! Each `.prefab` in the VFS gets a virtual directory with:
//!   prefab.prefab       -- pass-through to the real prefab bytes
//!   manifest.json       -- synth: parsed FileRef list with ext-based classification
//!   assets/<basename>   -- pass-through to each referenced VFS file
//!
//! All synthesis is lazy. The prefab stem list is cached on first access and
//! invalidated only on game patch (cdfuse already drops dir caches in that case).

use std::sync::RwLock;

use cdcore::formats::mesh::{
    parse_pac, parse_pam, parse_pamlod, submeshes_to_skinned_fbx, submeshes_to_textured_fbx,
    SubMesh, TextureRef,
};
use cdcore::formats::animation::pab::parse as parse_pab;
use cdcore::formats::scene::{parse_prefab, PrefabStringKind};
use cdcore::VfsManager;

use crate::virtual_files::{self, VirtualKind};

pub const PREFAB_ROOT_NAME: &str = "_prefabs";
pub const ASSETS_DIR_NAME: &str  = "assets";
pub const MANIFEST_NAME:   &str  = "manifest.json";
pub const MESH_FBX_NAME:   &str  = "mesh.fbx";
pub const MESH_FBM_DIR:    &str  = "mesh.fbm";

#[derive(Default)]
pub struct PrefabIndex {
    /// Map prefab stem -> full VFS path of the .prefab file.
    /// Built once on first use.
    stems: RwLock<Option<Vec<(String, String)>>>,
}

impl PrefabIndex {
    pub fn new() -> Self { Self::default() }

    /// List every prefab stem (the `<x>` in `<x>.prefab`). Lazy build.
    pub fn stems<'a>(&'a self, vfs: &VfsManager) -> Vec<String> {
        {
            if let Some(list) = self.stems.read().unwrap().as_ref() {
                return list.iter().map(|(stem, _)| stem.clone()).collect();
            }
        }
        let mut entries: Vec<(String, String)> = vfs
            .search(".prefab")
            .into_iter()
            .filter(|e| e.path.ends_with(".prefab"))
            .map(|e| (stem_of(&e.path), e.path))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        let stems: Vec<String> = entries.iter().map(|(s, _)| s.clone()).collect();
        *self.stems.write().unwrap() = Some(entries);
        stems
    }

    /// Resolve a prefab stem to its full VFS path. Caches via [`stems`].
    pub fn full_path_of(&self, vfs: &VfsManager, stem: &str) -> Option<String> {
        if self.stems.read().unwrap().is_none() {
            let _ = self.stems(vfs);
        }
        let guard = self.stems.read().unwrap();
        let list = guard.as_ref()?;
        list.binary_search_by(|(s, _)| s.as_str().cmp(stem))
            .ok()
            .map(|i| list[i].1.clone())
    }

    pub fn invalidate(&self) {
        *self.stems.write().unwrap() = None;
    }
}

/// Strip the directory prefix and `.prefab` extension to recover the stem.
fn stem_of(vfs_path: &str) -> String {
    let basename = vfs_path.rsplit_once('/').map(|(_, b)| b).unwrap_or(vfs_path);
    basename.strip_suffix(".prefab").unwrap_or(basename).to_string()
}

// -- Path classification -------------------------------------------------------

#[derive(Debug, Clone)]
pub enum PrefabPath<'a> {
    /// `_prefabs`
    Root,
    /// `_prefabs/<stem>`
    BundleDir { stem: &'a str },
    /// `_prefabs/<stem>/manifest.json`
    Manifest { stem: &'a str },
    /// `_prefabs/<stem>/prefab.prefab`
    PrefabFile { stem: &'a str },
    /// `_prefabs/<stem>/assets`
    AssetsDir { stem: &'a str },
    /// `_prefabs/<stem>/assets/<relpath>`. May be a file (exact match against
    /// an asset entry) or an intermediate directory (prefix of one). The
    /// caller resolves which against the prefab contents.
    AssetsEntry { stem: &'a str, relpath: &'a str },
    /// `_prefabs/<stem>/mesh.fbx`
    MeshFbx { stem: &'a str },
    /// `_prefabs/<stem>/mesh.fbm`
    FbmDir { stem: &'a str },
    /// `_prefabs/<stem>/mesh.fbm/<relpath>`. Same file-or-dir ambiguity as
    /// `AssetsEntry`.
    FbmEntry { stem: &'a str, relpath: &'a str },
}

/// Classify a path relative to the mount root. Returns None for paths outside
/// the `_prefabs` virtual root.
pub fn classify(path: &str) -> Option<PrefabPath<'_>> {
    if path == PREFAB_ROOT_NAME {
        return Some(PrefabPath::Root);
    }
    let rest = path.strip_prefix(PREFAB_ROOT_NAME)?.strip_prefix('/')?;
    let mut parts = rest.splitn(3, '/');
    let stem = parts.next()?;
    if stem.is_empty() {
        return None;
    }
    let leaf = match parts.next() {
        None => return Some(PrefabPath::BundleDir { stem }),
        Some(s) => s,
    };
    // For `assets/...` and `mesh.fbm/...` the remainder may include slashes
    // (we mirror VFS subdir structure); keep the entire tail as `relpath`.
    match (leaf, parts.next()) {
        (MANIFEST_NAME, None)             => Some(PrefabPath::Manifest { stem }),
        ("prefab.prefab", None)           => Some(PrefabPath::PrefabFile { stem }),
        (ASSETS_DIR_NAME, None)           => Some(PrefabPath::AssetsDir { stem }),
        (ASSETS_DIR_NAME, Some(rel))      => Some(PrefabPath::AssetsEntry { stem, relpath: rel }),
        (MESH_FBX_NAME, None)             => Some(PrefabPath::MeshFbx { stem }),
        (MESH_FBM_DIR, None)              => Some(PrefabPath::FbmDir { stem }),
        (MESH_FBM_DIR, Some(rel))         => Some(PrefabPath::FbmEntry { stem, relpath: rel }),
        _ => None,
    }
}

// -- Bundle resolution ---------------------------------------------------------

/// Classified file reference inside a prefab.
#[derive(Debug, Clone)]
pub struct AssetRef {
    pub vfs_path: String,
    pub kind:     AssetKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    Mesh,       // .pac / .pam / .pamlod
    Skeleton,   // .pab
    Animation,  // .paa
    Texture,    // .dds
    Material,   // .mat / inline
    Effect,     // .pafx
    Audio,      // .wem / .wmm
    Other,
}

fn classify_ext(path: &str) -> AssetKind {
    let lower = path.to_lowercase();
    if lower.ends_with(".pac") || lower.ends_with(".pam") || lower.ends_with(".pamlod") {
        AssetKind::Mesh
    } else if lower.ends_with(".pab") {
        AssetKind::Skeleton
    } else if lower.ends_with(".paa") {
        AssetKind::Animation
    } else if lower.ends_with(".dds") {
        AssetKind::Texture
    } else if lower.ends_with(".mat") {
        AssetKind::Material
    } else if lower.ends_with(".pafx") {
        AssetKind::Effect
    } else if lower.ends_with(".wem") || lower.ends_with(".wmm") {
        AssetKind::Audio
    } else {
        AssetKind::Other
    }
}

/// Parse the prefab and return the list of asset references it points at.
/// Returns an empty vec on parse error (callers see an empty `assets/` dir).
pub fn resolve_assets(vfs: &VfsManager, prefab_vfs_path: &str) -> Vec<AssetRef> {
    let entry = match vfs.lookup(prefab_vfs_path) {
        Some(e) => e,
        None    => return vec![],
    };
    let bytes = match vfs.read_entry(&entry) {
        Ok(b) => b,
        Err(e) => { log::warn!("prefab read failed for {prefab_vfs_path}: {e}"); return vec![]; }
    };
    let parsed = match parse_prefab(&bytes, prefab_vfs_path) {
        Ok(p) => p,
        Err(e) => { log::warn!("prefab parse failed for {prefab_vfs_path}: {e}"); return vec![]; }
    };

    let mut out: Vec<AssetRef> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for s in &parsed.strings {
        if !matches!(s.kind, PrefabStringKind::FileRef) {
            continue;
        }
        // Some prefab FileRefs are stored with a leading slash (`/object/...`);
        // the VFS keys without one. Normalize before lookup.
        let v = s.value.replace('\\', "/")
            .trim_start_matches('/')
            .to_string();
        if v.is_empty() || !v.contains('.') {
            continue;
        }
        if !seen.insert(v.clone()) {
            continue;
        }
        out.push(AssetRef {
            kind:     classify_ext(&v),
            vfs_path: v,
        });
    }
    out
}

/// Synthesize the manifest.json bytes for a single prefab bundle.
pub fn synth_manifest(vfs: &VfsManager, prefab_vfs_path: &str) -> Vec<u8> {
    let assets = resolve_assets(vfs, prefab_vfs_path);
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"prefab\": {},\n", json_str(prefab_vfs_path)));
    out.push_str("  \"assets\": [\n");
    for (i, a) in assets.iter().enumerate() {
        out.push_str("    {\"path\": ");
        out.push_str(&json_str(&a.vfs_path));
        out.push_str(", \"kind\": \"");
        out.push_str(match a.kind {
            AssetKind::Mesh      => "mesh",
            AssetKind::Skeleton  => "skeleton",
            AssetKind::Animation => "animation",
            AssetKind::Texture   => "texture",
            AssetKind::Material  => "material",
            AssetKind::Effect    => "effect",
            AssetKind::Audio     => "audio",
            AssetKind::Other     => "other",
        });
        out.push_str("\", \"present\": ");
        out.push_str(if vfs.lookup(&a.vfs_path).is_some() { "true" } else { "false" });
        out.push('}');
        if i + 1 < assets.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]\n");
    out.push_str("}\n");
    out.into_bytes()
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"'  => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What the assets/ dir actually exposes for one referenced asset.
/// Source files whose extension has a virtual mapping (`.dds` -> `.dds.png`,
/// `.pam`/`.pamlod`/`.pac` -> `.fbx`, `.wem` -> `.ogg`, `.paloc` -> `.jsonl`)
/// get a sibling synth alongside the raw passthrough so file managers and
/// content tools can open them directly.
///
/// `relpath` mirrors the original VFS path verbatim (subdirectories preserved),
/// so listing the assets/ tree reproduces the game's directory layout.
pub enum AssetEntry {
    /// Pass-through to `vfs_path` (real bytes after VFS decode).
    Passthrough { vfs_path: String, relpath: String, kind: AssetKind },
    /// Synthetic alias: `<vfs_path><suffix>`, rendered from `vfs_path` via
    /// the matching `virtual_files::render_*` for `kind`.
    Synth { vfs_path: String, relpath: String, kind: VirtualKind },
}

impl AssetEntry {
    pub fn relpath(&self) -> &str {
        match self {
            AssetEntry::Passthrough { relpath, .. } | AssetEntry::Synth { relpath, .. } => relpath,
        }
    }
}

/// Expand the bundle's asset list into the actual file paths inside
/// `_prefabs/<stem>/assets/`. Each source file produces a passthrough; if its
/// extension has a virtual mapping (DDS/PAM/PAMLOD/PAC/WEM/PALOC) it also
/// produces a sibling synth at `<vfs_path><suffix>`.
pub fn list_asset_entries(vfs: &VfsManager, prefab_vfs_path: &str) -> Vec<AssetEntry> {
    let mut out: Vec<AssetEntry> = Vec::new();
    for a in resolve_assets(vfs, prefab_vfs_path) {
        // Only emit a synth alias when the source actually resolves -- a
        // sibling entry that EIOs on read just confuses file managers.
        if vfs.lookup(&a.vfs_path).is_some() {
            if let Some((kind, suffix)) = virtual_files::synth_for_source(&a.vfs_path) {
                out.push(AssetEntry::Synth {
                    vfs_path: a.vfs_path.clone(),
                    relpath: format!("{}{suffix}", a.vfs_path),
                    kind,
                });
            }
        }
        out.push(AssetEntry::Passthrough {
            relpath: a.vfs_path.clone(),
            vfs_path: a.vfs_path,
            kind: a.kind,
        });
    }
    out
}

/// Resolve a path-relative-to-`assets/` to its underlying VFS source plus the
/// `VirtualKind` that should render it (`None` for raw passthroughs).
/// Returns `None` outright if no asset matches.
pub fn resolve_asset_relpath(
    vfs: &VfsManager,
    prefab_vfs_path: &str,
    relpath: &str,
) -> Option<(String, Option<VirtualKind>)> {
    for entry in list_asset_entries(vfs, prefab_vfs_path) {
        if entry.relpath() == relpath {
            return Some(match entry {
                AssetEntry::Passthrough { vfs_path, .. } => (vfs_path, None),
                AssetEntry::Synth { vfs_path, kind, .. } => (vfs_path, Some(kind)),
            });
        }
    }
    None
}

/// Convenience: just the VFS source path for `relpath`.
pub fn vfs_path_for_asset(
    vfs: &VfsManager,
    prefab_vfs_path: &str,
    relpath: &str,
) -> Option<String> {
    resolve_asset_relpath(vfs, prefab_vfs_path, relpath).map(|(p, _)| p)
}

/// One directory entry (file or subdir) at a given level inside the assets
/// tree. Used by the FUSE layer to enumerate `_prefabs/<stem>/assets/<dir>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetsTreeChild {
    /// A real asset entry whose relpath has no further slashes after `prefix`.
    /// `synth_kind` is `Some` for virtual-format aliases (rendered via
    /// `virtual_files::render_*`), `None` for raw passthroughs.
    File { relpath: String, name: String, synth_kind: Option<VirtualKind> },
    /// An intermediate subdirectory that contains at least one asset.
    Dir { name: String },
}

/// Direct children of `_prefabs/<stem>/assets/<prefix>` (or of `assets/`
/// itself when `prefix.is_empty()`). Files match assets whose relpath equals
/// `prefix + name`; subdirs match assets whose relpath starts with
/// `prefix + name + '/'`.
pub fn assets_dir_children(
    vfs: &VfsManager,
    prefab_vfs_path: &str,
    prefix: &str,
) -> Vec<AssetsTreeChild> {
    let entries = list_asset_entries(vfs, prefab_vfs_path);
    children_at_prefix(
        entries.iter().map(|e| (e.relpath(), match e {
            AssetEntry::Synth { kind, .. } => Some(*kind),
            AssetEntry::Passthrough { .. } => None,
        })),
        prefix,
    )
}

/// Whether `relpath` is a known intermediate subdirectory inside `assets/`.
/// Returns true if at least one asset's relpath starts with `relpath + "/"`.
pub fn is_assets_subdir(vfs: &VfsManager, prefab_vfs_path: &str, relpath: &str) -> bool {
    let prefix = format!("{relpath}/");
    list_asset_entries(vfs, prefab_vfs_path)
        .iter()
        .any(|e| e.relpath().starts_with(&prefix))
}

/// Direct children of `_prefabs/<stem>/mesh.fbm/<prefix>`. The `mesh.fbm`
/// tree mirrors the VFS path of each texture but with `.dds` rewritten to
/// `.png`.
pub fn fbm_dir_children(
    vfs: &VfsManager,
    prefab_vfs_path: &str,
    prefix: &str,
) -> Vec<AssetsTreeChild> {
    let textures = texture_paths(vfs, prefab_vfs_path);
    let pngs: Vec<String> = textures.iter().map(|p| png_path_for_dds(p)).collect();
    children_at_prefix(pngs.iter().map(|p| (p.as_str(), Some(VirtualKind::DdsPng))), prefix)
}

/// Whether `relpath` is a known intermediate subdirectory inside `mesh.fbm/`.
pub fn is_fbm_subdir(vfs: &VfsManager, prefab_vfs_path: &str, relpath: &str) -> bool {
    let prefix = format!("{relpath}/");
    texture_paths(vfs, prefab_vfs_path)
        .iter()
        .any(|p| png_path_for_dds(p).starts_with(&prefix))
}

/// Compute the unique direct children at a given prefix from an iterator of
/// (relpath, synth_kind) tuples. Files at the prefix surface as `File`;
/// any longer relpath surfaces its first remaining segment as a `Dir`.
fn children_at_prefix<'a, I>(entries: I, prefix: &str) -> Vec<AssetsTreeChild>
where
    I: IntoIterator<Item = (&'a str, Option<VirtualKind>)>,
{
    let normalized_prefix = if prefix.is_empty() {
        String::new()
    } else if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    };

    let mut files: Vec<AssetsTreeChild> = Vec::new();
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (rel, synth_kind) in entries {
        let tail = match rel.strip_prefix(&normalized_prefix) {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        if let Some(slash) = tail.find('/') {
            dirs.insert(tail[..slash].to_string());
        } else {
            files.push(AssetsTreeChild::File {
                relpath: rel.to_string(),
                name: tail.to_string(),
                synth_kind,
            });
        }
    }
    let mut out: Vec<AssetsTreeChild> = dirs.into_iter().map(|name| AssetsTreeChild::Dir { name }).collect();
    out.append(&mut files);
    out
}

/// First mesh asset (PAC/PAM/PAMLOD) referenced by this prefab, or None.
///
/// `.pami` (StaticMeshInstance XML) refs are followed one hop to their
/// `<StaticMesh Path="..."/>` target. Direct `.pac/.pam/.pamlod` refs win
/// when both forms are present.
pub fn primary_mesh_path(vfs: &VfsManager, prefab_vfs_path: &str) -> Option<String> {
    let assets = resolve_assets(vfs, prefab_vfs_path);
    // First try direct mesh refs.
    if let Some(direct) = assets.iter()
        .find(|a| matches!(a.kind, AssetKind::Mesh) && vfs.lookup(&a.vfs_path).is_some())
    {
        return Some(direct.vfs_path.clone());
    }
    // Fall back: follow .pami -> StaticMesh Path. The pami XML can use a
    // logical path with extra components (typically `effect/mesh/foo.pam`)
    // that don't appear in the VFS literally; the asset is at `effect/foo.pam`.
    // Try the literal path first, then a small set of common rewrites.
    for a in &assets {
        if !a.vfs_path.to_lowercase().ends_with(".pami") {
            continue;
        }
        let entry = match vfs.lookup(&a.vfs_path) { Some(e) => e, None => continue };
        let bytes = match vfs.read_entry(&entry) { Ok(b) => b, Err(_) => continue };
        let raw = match pami_static_mesh_path(&bytes) { Some(p) => p, None => continue };
        for cand in pami_path_candidates(&raw) {
            if vfs.lookup(&cand).is_some() {
                return Some(cand);
            }
        }
    }
    None
}

/// Generate VFS-path candidates from a .pami StaticMesh logical path. The
/// logical paths embed extra dir components (e.g. `effect/mesh/foo.pam`)
/// that the runtime drops when looking up in the PAMT (`effect/foo.pam`).
fn pami_path_candidates(raw: &str) -> Vec<String> {
    let raw = raw.replace('\\', "/").trim_start_matches('/').to_string();
    let mut out = vec![raw.clone()];
    // Common rewrite: drop "/mesh/" segment.
    if let Some(stripped) = raw.replace("/mesh/", "/").strip_prefix("") {
        if stripped != raw {
            out.push(stripped.to_string());
        }
    }
    // Fallback: also try just the basename in the parent dir of the .pami
    // (e.g. logical "object/X/foo.pam" might actually be "X/foo.pam").
    if let Some((_, basename)) = raw.rsplit_once('/') {
        out.push(basename.to_string());
    }
    out
}

/// Pull the `<StaticMesh Path="..."/>` value out of a .pami XML blob.
/// Returns None if the tag is missing or malformed.
fn pami_static_mesh_path(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes).ok()?;
    let needle = "<StaticMesh Path=\"";
    let i = s.find(needle)?;
    let rest = &s[i + needle.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// First-listed PAB skeleton path for this prefab, if any. Used to drive the
/// skinned FBX export so Blender opens `mesh.fbx` with a posable rig instead
/// of a static mesh.
pub fn primary_skeleton_path(vfs: &VfsManager, prefab_vfs_path: &str) -> Option<String> {
    resolve_assets(vfs, prefab_vfs_path)
        .into_iter()
        .find(|a| matches!(a.kind, AssetKind::Skeleton) && vfs.lookup(&a.vfs_path).is_some())
        .map(|a| a.vfs_path)
}

/// Texture asset paths for this prefab, in deterministic order.
pub fn texture_paths(vfs: &VfsManager, prefab_vfs_path: &str) -> Vec<String> {
    resolve_assets(vfs, prefab_vfs_path)
        .into_iter()
        .filter(|a| matches!(a.kind, AssetKind::Texture) && vfs.lookup(&a.vfs_path).is_some())
        .map(|a| a.vfs_path)
        .collect()
}

/// VFS-relative PNG path inside `mesh.fbm/` for a given DDS VFS path.
/// Mirrors the DDS path with `.dds` rewritten to `.png`, preserving directory
/// structure so the FBX `RelativeFilename` references survive cleanly.
pub fn png_path_for_dds(dds_vfs_path: &str) -> String {
    if let Some(stem) = dds_vfs_path.strip_suffix(".dds") {
        format!("{stem}.png")
    } else {
        format!("{dds_vfs_path}.png")
    }
}

/// Reverse: which DDS path corresponds to a `.fbm/<relpath>.png` synth?
pub fn dds_path_for_fbm_png(
    vfs: &VfsManager,
    prefab_vfs_path: &str,
    relpath: &str,
) -> Option<String> {
    for tex in texture_paths(vfs, prefab_vfs_path) {
        if png_path_for_dds(&tex) == relpath {
            return Some(tex);
        }
    }
    None
}

/// Synthesize the textured `mesh.fbx` bytes for a prefab. Returns None when
/// the prefab references no decodable mesh.
pub fn synth_mesh_fbx(vfs: &VfsManager, prefab_vfs_path: &str) -> Option<Vec<u8>> {
    let mesh_path = primary_mesh_path(vfs, prefab_vfs_path)?;
    let entry = vfs.lookup(&mesh_path)?;
    let bytes = vfs.read_entry(&entry).ok()?;
    let stem = mesh_path
        .rsplit_once('/').map(|(_, b)| b).unwrap_or(&mesh_path)
        .to_string();

    // Pull SubMesh slices out of whichever PAR variant this is.
    let submeshes_owned: Vec<SubMesh> = if mesh_path.ends_with(".pac") {
        parse_pac(&bytes, &mesh_path).ok()?
            .submeshes.into_iter().map(|p| p.base).collect()
    } else if mesh_path.ends_with(".pamlod") {
        parse_pamlod(&bytes, &mesh_path).ok()?.submeshes
    } else if mesh_path.ends_with(".pam") {
        parse_pam(&bytes, &mesh_path).ok()?.submeshes
    } else {
        return None;
    };
    if submeshes_owned.is_empty() { return None; }

    // For each submesh, look up the matching texture by `submesh.texture` field.
    // Fallback: assign textures positionally (textures[i] for submesh[i]).
    let textures = texture_paths(vfs, prefab_vfs_path);
    let png_paths: Vec<String> = textures.iter().map(|p| png_path_for_dds(p)).collect();

    // Assemble TextureRef per submesh. None when no texture lines up.
    let tex_refs: Vec<Option<TextureRef<'_>>> = (0..submeshes_owned.len())
        .map(|i| {
            png_paths.get(i).map(|rel| TextureRef {
                png_relative_path: rel.as_str(),
                png_absolute_path: rel.as_str(),
            })
        })
        .collect();

    let sm_refs: Vec<&SubMesh> = submeshes_owned.iter().collect();

    // When the prefab references a PAB skeleton, emit a skinned FBX with
    // LimbNode bones + Skin/Cluster deformers. Falls back to the static
    // textured export when the PAB is absent or fails to parse.
    if let Some(skel_path) = primary_skeleton_path(vfs, prefab_vfs_path) {
        if let Some(skel_entry) = vfs.lookup(&skel_path) {
            if let Ok(skel_bytes) = vfs.read_entry(&skel_entry) {
                match parse_pab(&skel_bytes, &skel_path) {
                    Ok(skel) if !skel.bones.is_empty() => {
                        return Some(submeshes_to_skinned_fbx(
                            &sm_refs,
                            &stem,
                            &skel,
                            Some(&tex_refs),
                            1.0,
                        ));
                    }
                    Ok(_) => {
                        log::info!("prefab_view: PAB {skel_path} has 0 bones, falling back to static FBX");
                    }
                    Err(e) => {
                        log::warn!("prefab_view: PAB parse failed for {skel_path}: {e}; falling back to static FBX");
                    }
                }
            }
        }
    }
    Some(submeshes_to_textured_fbx(&sm_refs, &stem, &tex_refs))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_handles_each_path_shape() {
        assert!(matches!(classify("_prefabs"), Some(PrefabPath::Root)));
        assert!(matches!(classify("_prefabs/foo"), Some(PrefabPath::BundleDir { stem: "foo" })));
        assert!(matches!(classify("_prefabs/foo/manifest.json"), Some(PrefabPath::Manifest { stem: "foo" })));
        assert!(matches!(classify("_prefabs/foo/prefab.prefab"),  Some(PrefabPath::PrefabFile { stem: "foo" })));
        assert!(matches!(classify("_prefabs/foo/assets"),         Some(PrefabPath::AssetsDir { stem: "foo" })));
        // Flat file under assets/
        assert!(matches!(
            classify("_prefabs/foo/assets/bar.dds"),
            Some(PrefabPath::AssetsEntry { stem: "foo", relpath: "bar.dds" })
        ));
        // Nested file under assets/ now allowed -- mirrors VFS structure.
        assert!(matches!(
            classify("_prefabs/foo/assets/character/sub/x.dds"),
            Some(PrefabPath::AssetsEntry { stem: "foo", relpath: "character/sub/x.dds" })
        ));
        // mesh.fbm tree: same nesting.
        assert!(matches!(
            classify("_prefabs/foo/mesh.fbm/character/x.png"),
            Some(PrefabPath::FbmEntry { stem: "foo", relpath: "character/x.png" })
        ));
        // non-prefab paths
        assert!(classify("character/foo.pac").is_none());
        // bogus children
        assert!(classify("_prefabs/foo/garbage").is_none());
    }

    #[test]
    fn png_path_for_dds_preserves_directory_structure() {
        assert_eq!(png_path_for_dds("character/foo/bar.dds"), "character/foo/bar.png");
        assert_eq!(png_path_for_dds("texture.dds"), "texture.png");
        // Non-.dds path: append `.png` so the alias is still resolvable.
        assert_eq!(png_path_for_dds("weird"), "weird.png");
    }

    #[test]
    fn children_at_prefix_groups_subdirs() {
        let entries = vec![
            ("character/cha/foo.pac", None),
            ("character/cha/bar.dds", None),
            ("character/cha/bar.dds.png", Some(VirtualKind::DdsPng)),
            ("character/skel.pab", None),
            ("misc/x.wem", None),
        ];
        // Root prefix: two subdirs, no direct files.
        let root = children_at_prefix(entries.iter().copied(), "");
        assert_eq!(root, vec![
            AssetsTreeChild::Dir { name: "character".into() },
            AssetsTreeChild::Dir { name: "misc".into() },
        ]);
        // Inside character/: one file (skel.pab) + one subdir (cha).
        let chr = children_at_prefix(entries.iter().copied(), "character");
        assert!(chr.contains(&AssetsTreeChild::Dir { name: "cha".into() }));
        assert!(chr.iter().any(|c| matches!(c, AssetsTreeChild::File { name, .. } if name == "skel.pab")));
        // Deepest: three files, no subdirs.
        let cha = children_at_prefix(entries.iter().copied(), "character/cha");
        let names: Vec<&str> = cha.iter().filter_map(|c| match c {
            AssetsTreeChild::File { name, .. } => Some(name.as_str()),
            _ => None,
        }).collect();
        assert!(names.contains(&"foo.pac"));
        assert!(names.contains(&"bar.dds"));
        assert!(names.contains(&"bar.dds.png"));
    }
}
