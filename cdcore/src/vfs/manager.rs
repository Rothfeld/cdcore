//! Item 7 -- Flat BTreeMap index replacing the HashMap trie.
//!
//! All file entries live in a single `BTreeMap<String, (PamtFileEntry, String)>`
//! sorted by virtual path. This gives:
//!   * lookup:       O(log n) binary search vs O(depth) pointer-chased HashMap
//!   * list_dir:     O(k log n) range scan -- one contiguous memory region
//!   * memory:       ~300 MB less than the recursive HashMap tree for 1.4 M files
//!   * parallelism:  PAMT parsing is fully parallel; one batch write lock at merge

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use dashmap::DashMap;
use rayon::prelude::*;

use crate::archive::pamt::{parse_pamt, PamtData, PamtFileEntry};
use crate::archive::paz;
use crate::archive::user_group::UserGroup;
use crate::compression;
use crate::crypto;
use crate::error::{ParseError, Result};

/// (PamtFileEntry, group_dir)
type Entry = (PamtFileEntry, String);

pub struct VfsManager {
    packages_path: PathBuf,
    /// All file entries sorted by virtual path.
    tree: Arc<RwLock<BTreeMap<String, Entry>>>,
    /// Which groups are loaded (used for dedup / invalidation).
    loaded: DashMap<String, ()>,
    /// Raw PAMT data kept for repack / checksum operations.
    pamt_cache: DashMap<String, PamtData>,
    /// User-created files via a dedicated package group (default `9000`).
    /// `None` until [`VfsManager::init_user_group`] is called.
    user_group: Mutex<Option<UserGroup>>,
    /// Synthetic empty directories (mkdir-only). Persisted across mount
    /// sessions when at least one user file lives under them; otherwise
    /// they vanish on unmount. Stored as the directory path without trailing
    /// slash. Read by `dir_exists` and `list_dir_*` to decide visibility.
    synth_dirs: RwLock<std::collections::HashSet<String>>,
}

impl VfsManager {
    pub fn new(packages_path: &str) -> Result<Self> {
        let path = PathBuf::from(packages_path);
        if !path.is_dir() {
            return Err(ParseError::Other(format!(
                "packages directory not found: {packages_path}"
            )));
        }
        Ok(VfsManager {
            packages_path: path,
            tree: Arc::new(RwLock::new(BTreeMap::new())),
            loaded: DashMap::new(),
            pamt_cache: DashMap::new(),
            user_group: Mutex::new(None),
            synth_dirs: RwLock::new(std::collections::HashSet::new()),
        })
    }

    // -- User-created files (group 9000 by default) --------------------------

    /// Bootstrap or open the user package group (default `9000`). Idempotent;
    /// safe to call once per mount. Inserts any pre-existing user files into
    /// the in-memory tree so they show up in `lookup` and `list_dir_*`.
    pub fn init_user_group(&self, group_id: &str, papgt_path: &Path) -> Result<()> {
        let mut slot = self.user_group.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        let ug = UserGroup::open_or_create(&self.packages_path, papgt_path, group_id)?;
        // Inject existing entries into the tree.
        let entries = ug.all_entries();
        let group_id_owned = ug.group_id.clone();
        let mut tree = self.tree.write().unwrap();
        for e in entries {
            tree.insert(e.path.clone(), (e, group_id_owned.clone()));
        }
        *slot = Some(ug);
        Ok(())
    }

    /// Whether the user group has been initialised on this manager.
    pub fn user_group_ready(&self) -> bool {
        self.user_group.lock().unwrap().is_some()
    }

    /// Whether `path` is currently owned by the user group (i.e. created via
    /// [`Self::create_user_file`]). Returns false for shipped paths.
    pub fn is_user_path(&self, path: &str) -> bool {
        let norm = path.replace('\\', "/");
        let slot = self.user_group.lock().unwrap();
        slot.as_ref().is_some_and(|g| g.contains(&norm))
    }

    /// Create or replace a user file. Rejects paths that already exist as
    /// shipped entries (the user group never shadows shipped data). Returns
    /// the new PamtFileEntry.
    pub fn create_user_file(&self, path: &str, data: &[u8]) -> Result<PamtFileEntry> {
        let norm = path.replace('\\', "/");
        // Reject if a shipped entry already lives here.
        {
            let tree = self.tree.read().unwrap();
            if let Some((_, g)) = tree.get(&norm) {
                let user_g = self.user_group.lock().unwrap()
                    .as_ref().map(|u| u.group_id.clone())
                    .unwrap_or_default();
                if g != &user_g {
                    return Err(ParseError::Other(format!(
                        "shipped file already exists at {norm}; user group will not shadow it"
                    )));
                }
            }
        }

        let mut slot = self.user_group.lock().unwrap();
        let ug = slot.as_mut().ok_or_else(|| {
            ParseError::Other("user group not initialised; call init_user_group first".into())
        })?;
        ug.add(&norm, data)?;
        let entry = ug.entry_for(&norm).ok_or_else(|| {
            ParseError::Other("user_group::add succeeded but entry_for missed".into())
        })?;
        let group_id = ug.group_id.clone();
        drop(slot);

        let mut tree = self.tree.write().unwrap();
        tree.insert(norm.clone(), (entry.clone(), group_id));
        Ok(entry)
    }

    /// Remove a user file. Returns true if it existed.
    pub fn remove_user_file(&self, path: &str) -> Result<bool> {
        let norm = path.replace('\\', "/");
        let mut slot = self.user_group.lock().unwrap();
        let ug = slot.as_mut().ok_or_else(|| {
            ParseError::Other("user group not initialised".into())
        })?;
        let removed = ug.remove(&norm)?;
        drop(slot);
        if removed {
            self.tree.write().unwrap().remove(&norm);
        }
        Ok(removed)
    }

    /// Read a user file's bytes (decrypted + decompressed). Returns None for
    /// non-user paths.
    pub fn read_user_file(&self, path: &str) -> Option<Vec<u8>> {
        let norm = path.replace('\\', "/");
        self.user_group.lock().unwrap().as_ref().and_then(|g| g.read(&norm))
    }

    /// Mark `dir` as a synthetic empty directory so `mkdir` survives until
    /// either an `rmdir` removes it or a child file pins it implicitly.
    pub fn add_synth_dir(&self, dir: &str) {
        let norm = dir.trim_end_matches('/').replace('\\', "/");
        if !norm.is_empty() {
            self.synth_dirs.write().unwrap().insert(norm);
        }
    }

    /// Drop a synthetic empty directory. No-op if not registered.
    pub fn remove_synth_dir(&self, dir: &str) -> bool {
        let norm = dir.trim_end_matches('/').replace('\\', "/");
        self.synth_dirs.write().unwrap().remove(&norm)
    }

    /// Whether `dir` is a registered synthetic empty directory.
    pub fn is_synth_dir(&self, dir: &str) -> bool {
        let norm = dir.trim_end_matches('/').replace('\\', "/");
        self.synth_dirs.read().unwrap().contains(&norm)
    }

    /// Load one package group (idempotent).
    pub fn load_group(&self, group_dir: &str) -> Result<()> {
        if self.loaded.contains_key(group_dir) {
            return Ok(());
        }

        let pamt_path = self.packages_path.join(group_dir).join("0.pamt");
        let paz_dir   = self.packages_path.join(group_dir);
        let pamt = parse_pamt(
            pamt_path.to_str().unwrap(),
            Some(paz_dir.to_str().unwrap()),
        )?;

        let mut tree = self.tree.write().unwrap();
        for entry in &pamt.file_entries {
            tree.insert(entry.path.clone(), (entry.clone(), group_dir.to_string()));
        }
        drop(tree);

        self.pamt_cache.insert(group_dir.to_string(), pamt);
        self.loaded.insert(group_dir.to_string(), ());
        Ok(())
    }

    /// Parse all PAMTs in parallel, then insert in one batch write lock.
    pub fn load_all_groups(&self) -> Result<()> {
        let groups = self.list_groups()?;
        let new_groups: Vec<&String> = groups
            .iter()
            .filter(|g| !self.loaded.contains_key(*g))
            .collect();

        if new_groups.is_empty() {
            return Ok(());
        }

        // Parse in parallel (no lock held)
        let parsed: Vec<(String, PamtData)> = new_groups
            .par_iter()
            .map(|g| -> Result<(String, PamtData)> {
                let pamt_path = self.packages_path.join(*g).join("0.pamt");
                let paz_dir   = self.packages_path.join(*g);
                let pamt = parse_pamt(
                    pamt_path.to_str().unwrap(),
                    Some(paz_dir.to_str().unwrap()),
                )?;
                Ok(((*g).clone(), pamt))
            })
            .collect::<Result<_>>()?;

        // Single write lock for the batch insert
        let mut tree = self.tree.write().unwrap();
        for (group, pamt) in &parsed {
            for entry in &pamt.file_entries {
                tree.insert(entry.path.clone(), (entry.clone(), group.clone()));
            }
        }
        drop(tree);

        for (group, pamt) in parsed {
            self.pamt_cache.insert(group.clone(), pamt);
            self.loaded.insert(group, ());
        }
        self.expose_multi_package_dirs();
        Ok(())
    }

    pub fn list_groups(&self) -> Result<Vec<String>> {
        let mut groups: Vec<String> = std::fs::read_dir(&self.packages_path)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir() && e.path().join("0.pamt").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        groups.sort();
        Ok(groups)
    }

    /// O(log n) file lookup.
    pub fn lookup(&self, path: &str) -> Option<PamtFileEntry> {
        let norm = path.replace('\\', "/");
        self.tree.read().unwrap().get(&norm).map(|(e, _)| e.clone())
    }

    /// Direct children of `dir`, sorted (dirs first then files).
    pub fn list_dir(&self, dir: &str) -> Vec<String> {
        self.list_dir_typed(dir).into_iter().map(|(n, _)| n).collect()
    }

    /// Direct children of `dir` with `is_dir` flag -- O(k log n) range scan.
    pub fn list_dir_typed(&self, dir: &str) -> Vec<(String, bool)> {
        self.list_dir_with_sizes(dir)
            .into_iter()
            .map(|(n, d, _)| (n, d))
            .collect()
    }

    /// Direct children of `dir` with `(name, is_dir, orig_size)`.
    ///
    /// Single BTreeMap read lock, single range scan -- use this in
    /// `build_dir_cache` instead of calling `list_dir_typed` + Nx`lookup`.
    pub fn list_dir_with_sizes(&self, dir: &str) -> Vec<(String, bool, u32)> {
        let tree = self.tree.read().unwrap();
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", dir.replace('\\', "/"))
        };

        // `seen`: name -> (is_dir, orig_size). For directory children, size=0.
        let mut seen: std::collections::HashMap<String, (bool, u32)> =
            std::collections::HashMap::new();

        for (path, (entry, _)) in tree.range(prefix.clone()..) {
            if !prefix.is_empty() && !path.starts_with(&prefix) {
                break;
            }
            let rest = if prefix.is_empty() { path.as_str() } else { &path[prefix.len()..] };
            if let Some(slash) = rest.find('/') {
                seen.entry(rest[..slash].to_string()).or_insert((true, 0));
            } else {
                seen.entry(rest.to_string()).or_insert((false, entry.orig_size));
            }
        }

        let mut result: Vec<(String, bool, u32)> = seen
            .into_iter()
            .map(|(n, (d, s))| (n, d, s))
            .collect();
        result.sort_by(|(an, ad, _), (bn, bd, _)| bd.cmp(ad).then(an.cmp(bn)));
        result
    }

    /// Like `list_dir_with_sizes` but skips the sort -- for FUSE readdirplus
    /// where kernel ordering doesn't matter and 329K-entry sorts add ~7% CPU.
    pub fn list_dir_with_sizes_unsorted(&self, dir: &str) -> Vec<(String, bool, u32)> {
        let tree = self.tree.read().unwrap();
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", dir.replace('\\', "/"))
        };
        let mut seen: std::collections::HashMap<String, (bool, u32)> =
            std::collections::HashMap::new();
        for (path, (entry, _)) in tree.range(prefix.clone()..) {
            if !prefix.is_empty() && !path.starts_with(&prefix) { break; }
            let rest = if prefix.is_empty() { path.as_str() } else { &path[prefix.len()..] };
            if let Some(slash) = rest.find('/') {
                seen.entry(rest[..slash].to_string()).or_insert((true, 0));
            } else {
                seen.entry(rest.to_string()).or_insert((false, entry.orig_size));
            }
        }
        seen.into_iter().map(|(n, (d, s))| (n, d, s)).collect()
    }

    /// Returns `true` if `path` is a non-empty directory in the VFS.
    /// O(log n) -- seeks to the first entry under the prefix, checks one node.
    /// Also matches synthetic empty user dirs created via `add_synth_dir`.
    pub fn dir_exists(&self, path: &str) -> bool {
        let norm = path.replace('\\', "/");
        if self.is_synth_dir(&norm) {
            return true;
        }
        let prefix = format!("{}/", norm);
        let tree = self.tree.read().unwrap();
        tree.range(prefix.clone()..)
            .next()
            .map(|(k, _)| k.starts_with(&prefix))
            .unwrap_or(false)
    }

    /// Returns `true` if any file under `dir` (recursively) has a path ending
    /// with `ext`.  Uses a single BTreeMap range scan and exits on first match.
    pub fn subtree_has_ext(&self, dir: &str, ext: &str) -> bool {
        let tree = self.tree.read().unwrap();
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", dir.replace('\\', "/"))
        };
        for path in tree.range(prefix.clone()..).map(|(k, _)| k) {
            if !prefix.is_empty() && !path.starts_with(&prefix) {
                break;
            }
            if path.ends_with(ext) {
                return true;
            }
        }
        false
    }

    pub fn search(&self, query: &str) -> Vec<PamtFileEntry> {
        let q = query.to_lowercase();
        self.tree
            .read()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.to_lowercase().contains(&q))
            .map(|(_, (e, _))| e.clone())
            .collect()
    }

    /// Full decrypt + decompress pipeline for a file entry.
    /// Routes user-group entries through the UserGroup helper since their
    /// PAMT layout doesn't carry a flags byte for paz_index.
    pub fn read_entry(&self, entry: &PamtFileEntry) -> Result<Vec<u8>> {
        if let Some(ug) = self.user_group.lock().unwrap().as_ref() {
            if ug.contains(&entry.path) {
                return ug.read(&entry.path).ok_or_else(|| ParseError::Other(format!(
                    "user_group read failed for {}", entry.path
                )));
            }
        }
        self.read_entry_inner(entry)
    }

    fn read_entry_inner(&self, entry: &PamtFileEntry) -> Result<Vec<u8>> {
        let read_size = if entry.compressed() {
            entry.comp_size as usize
        } else {
            entry.orig_size as usize
        };

        let mut data = paz::read_bytes(&entry.paz_file, entry.offset, read_size)?;

        if entry.encrypted() {
            let basename = Path::new(&entry.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&entry.path);
            crypto::decrypt_inplace(&mut data, basename);
        }

        if entry.compressed() && entry.compression_type() != 0 {
            data = compression::decompress(
                &data,
                entry.orig_size as usize,
                entry.compression_type(),
            )?;
        }

        Ok(data)
    }

    /// Reload one package group from disk, replacing all its in-memory entries.
    ///
    /// Parses the PAMT without holding the tree lock, then swaps entries in a
    /// single write-lock so there is no window where the group is absent.
    /// Call this after repacking a file in the group so reads that miss the
    /// decode cache see the updated offsets rather than the stale ones.
    pub fn reload_group(&self, group_dir: &str) -> Result<()> {
        let pamt_path = self.packages_path.join(group_dir).join("0.pamt");
        let paz_dir   = self.packages_path.join(group_dir);
        let pamt = parse_pamt(
            pamt_path.to_str().unwrap(),
            Some(paz_dir.to_str().unwrap()),
        )?;

        let mut tree = self.tree.write().unwrap();
        tree.retain(|_, (_, g)| g != group_dir);
        for entry in &pamt.file_entries {
            tree.insert(entry.path.clone(), (entry.clone(), group_dir.to_string()));
        }
        drop(tree);

        self.pamt_cache.insert(group_dir.to_string(), pamt);
        self.loaded.insert(group_dir.to_string(), ());
        Ok(())
    }

    /// Remove a single file entry from the in-memory index.
    /// The PAZ archive is not modified; the file simply becomes invisible.
    pub fn remove_entry(&self, path: &str) -> bool {
        let norm = path.replace('\\', "/");
        self.tree.write().unwrap().remove(&norm).is_some()
    }

    /// Remove all entries belonging to `group_dir` and clear its PAMT cache.
    pub fn invalidate_group(&self, group_dir: &str) {
        let mut tree = self.tree.write().unwrap();
        tree.retain(|_, (_, g)| g != group_dir);
        drop(tree);
        self.pamt_cache.remove(group_dir);
        self.loaded.remove(group_dir);
    }

    /// Expose multi-package top-level directories as `dir@group/` aliases.
    ///
    /// Scans the tree for top-level directory names that have entries from
    /// more than one package group.  For each such directory, inserts alias
    /// entries `dir@group/rest/of/path` alongside the default `dir/rest/of/path`
    /// so callers can request a specific language/variant explicitly.
    ///
    /// Example: `sound/` appears in groups 0005, 0006, 0035 ->
    ///   `sound@0005/nhm_adult_noble_1_hello.wem` (Korean)
    ///   `sound@0006/nhm_adult_noble_1_hello.wem` (English)
    ///   `sound@0035/nhm_adult_noble_1_hello.wem` (Japanese)
    ///
    /// Called automatically after every `load_group` and `load_all_groups`.
    /// Idempotent: existing `@` aliases are removed and rebuilt on each call.
    pub fn expose_multi_package_dirs(&self) {
        let tree_r = self.tree.read().unwrap();

        // Find top-level dirs that appear in more than one group.
        let mut dir_groups: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
            std::collections::HashMap::new();
        for (path, (_, group)) in tree_r.iter() {
            // Skip existing aliases.
            if path.contains('@') { continue; }
            if let Some(slash) = path.find('/') {
                dir_groups.entry(&path[..slash])
                    .or_default()
                    .insert(group.as_str());
            }
        }

        let multi_dirs: std::collections::HashSet<String> = dir_groups
            .into_iter()
            .filter(|(_, groups)| groups.len() > 1)
            .map(|(dir, _)| dir.to_string())
            .collect();

        if multi_dirs.is_empty() { return; }

        // Collect aliases to insert.
        let mut new_entries: Vec<(String, Entry)> = Vec::new();
        for (path, (entry, group)) in tree_r.iter() {
            if path.contains('@') { continue; }
            if let Some(slash) = path.find('/') {
                let top = &path[..slash];
                if multi_dirs.contains(top) {
                    let alias = format!("{}@{}{}", top, group, &path[slash..]);
                    new_entries.push((alias, (entry.clone(), group.clone())));
                }
            }
        }
        drop(tree_r);

        let mut tree_w = self.tree.write().unwrap();
        // Remove stale aliases before reinserting.
        tree_w.retain(|k, _| !k.contains('@'));
        for (path, entry) in new_entries {
            tree_w.insert(path, entry);
        }
    }

    pub fn reload(&mut self) -> Result<()> {
        if !self.packages_path.is_dir() {
            return Err(ParseError::Other(
                "packages directory disappeared during reload".into(),
            ));
        }
        self.tree.write().unwrap().clear();
        self.pamt_cache.clear();
        self.loaded.clear();
        Ok(())
    }

    pub fn packages_path(&self) -> &str {
        self.packages_path.to_str().unwrap_or("")
    }

    /// Expose PAMT data for checksum / repack operations.
    pub fn get_pamt(&self, group_dir: &str) -> Option<PamtData> {
        self.pamt_cache.get(group_dir).map(|r| r.clone())
    }
}
