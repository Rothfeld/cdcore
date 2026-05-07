//! WinFSP filesystem -- `impl FileSystemContext for CdWinFs` where CdWinFs wraps Arc<SharedFs>.
//!
//! Concurrency model
//! -----------------
//! WinFSP dispatches from its own thread pool (OperationGuardStrategy::Fine):
//!   EXCL: set_volume_label, flush(volume), create, cleanup(delete), rename
//!   SHRD: get_volume_info, open, set_delete, read_directory
//!   NONE: everything else (read, write, get_file_info, ...)
//!
//! All shared state is protected by Mutex / AtomicXxx.
//! DirBuffer provides its own interior-mutability (per open FileCtx).
//!
//! Key differences from cdfuse/fs.rs:
//!   - No single-threaded session loop; no path_queue / drain pattern.
//!   - WinFSP always supplies the path via FileCtx; no ino->path HashMap needed.
//!   - write_overlay / write_mtimes keyed by path string instead of ino.
//!   - decode() called synchronously (WinFSP uses multiple dispatcher threads).
//!   - dir_cache removed; each open FileCtx owns its DirBuffer.
//!   - rayon decode_pool removed; not needed without async FUSE replies.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

use log::{info, warn};
use lru::LruCache;
use memmap2::Mmap;
use winfsp::filesystem::{
    DirInfo, DirMarker, FileSecurity, FileInfo,
    FileSystemContext, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, Result, U16CStr};

use cdcore::{VfsManager, crypto, compression};
use cdcore::repack::{RepackEngine, ModifiedFile};
use crate::prefab_view;
use crate::virtual_files;

// ---- constants ---------------------------------------------------------------

const ROOT_INO:          u64   = 1;
const MAX_CACHE_ENTRIES: usize = 131_072;
const MAX_CACHED_BYTES:  usize = 512 * 1024 * 1024;
const SLOW_MS:           u128  = 200;

const FILE_ATTRIBUTE_READONLY:  u32 = 0x01;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE:   u32 = 0x20;

const FSP_CLEANUP_DELETE: u32 = 0x01;

// NTSTATUS codes (raw i32 -- avoids pulling windows crate into this file)
const NTSTATUS_NOT_FOUND:  i32 = 0xC0000034u32 as i32; // STATUS_OBJECT_NAME_NOT_FOUND
const NTSTATUS_WRITE_PROT: i32 = 0xC00000A2u32 as i32; // STATUS_MEDIA_WRITE_PROTECTED
const NTSTATUS_IO_ERR:     i32 = 0xC0000185u32 as i32; // STATUS_IO_DEVICE_ERROR
const NTSTATUS_ACCESS:     i32 = 0xC0000022u32 as i32; // STATUS_ACCESS_DENIED
const NTSTATUS_OBJ_EXISTS: i32 = 0xC0000035u32 as i32; // STATUS_OBJECT_NAME_COLLISION

// ---- helpers -----------------------------------------------------------------

fn ino_for(path: &str) -> u64 {
    if path.is_empty() { return ROOT_INO; }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish().wrapping_mul(0x9e3779b97f4a7c15).max(2)
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() }
    else { format!("{parent}/{name}") }
}

// Convert WinFSP path (\foo\bar) to VFS path (foo/bar).
fn vfs_path(p: &U16CStr) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix('\\').unwrap_or(&s);
    s.replace('\\', "/")
}

// `[HH:MM:SS]` UTC for TUI event log lines.  Matches the timestamp style
// env_logger writes into cdwinfs.log so a user comparing the two can line
// events up at a glance.  UTC keeps things free of TZ deps; the TUI label
// is short enough that local-vs-UTC ambiguity isn't a concern.
fn event_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = secs % 86_400;
    let h = day_secs / 3600;
    let m = (day_secs / 60) % 60;
    let s = day_secs % 60;
    format!("[{h:02}:{m:02}:{s:02}]")
}

// Convert SystemTime to Windows FILETIME (100-ns intervals since 1601-01-01).
fn to_filetime(t: SystemTime) -> u64 {
    const OFFSET: u64 = 116_444_736_000_000_000;
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d)  => OFFSET + d.as_nanos() as u64 / 100,
        Err(_) => 0,
    }
}

/// True for any path whose first segment matches one of our synthetic root
/// directory names (`.dds.png`, `.paloc.json`, `_prefabs`, ...). Used to keep
/// editor temp files (Paint, etc.) from leaking into the user package group
/// under a virtual prefix -- where they'd both pollute root listings and
/// shadow the read-only synth view with stale data.
fn is_under_virtual_root(path: &str) -> bool {
    let head = path.split('/').next().unwrap_or("");
    if head == prefab_view::PREFAB_ROOT_NAME { return true; }
    virtual_files::virtual_root_dirs().any(|v| v == head)
}

// -- Snapshots -- archive the original bytes before any overwrite -------------

/// `<exe_parent>/snapshots`, or None if `current_exe()` can't be resolved.
fn snapshot_root() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("snapshots"))
}

/// `YYYY-MM-DDTHH-MM-SS.sssZ`. Hinnant's civil-from-days algorithm; no deps.
fn snapshot_stamp() -> String {
    let d = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let ms   = d.subsec_millis();
    let day_secs = secs % 86_400;
    let hh = day_secs / 3600;
    let mm = (day_secs / 60) % 60;
    let ss = day_secs % 60;
    let days = (secs / 86_400) as i64;
    let z   = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y   = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let dd  = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo  = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let yr  = if mo <= 2 { y + 1 } else { y };
    format!("{yr:04}-{mo:02}-{dd:02}T{hh:02}-{mm:02}-{ss:02}.{ms:03}Z")
}

/// Persist `data` (the bytes about to be overwritten) under
/// `<exe>/snapshots/<utc-stamp>/<vfs_path>`. Best-effort: errors are logged and
/// swallowed so a snapshot failure never aborts a flush.
fn save_snapshot(vfs_path: &str, data: &[u8]) {
    let Some(root) = snapshot_root() else {
        warn!("snapshot {vfs_path}: cannot resolve exe directory");
        return;
    };
    let mut dest = root.join(snapshot_stamp());
    for part in vfs_path.split('/').filter(|s| !s.is_empty()) {
        dest.push(part);
    }
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("snapshot {vfs_path}: mkdir {parent:?}: {e}");
            return;
        }
    }
    match std::fs::write(&dest, data) {
        Ok(_)  => info!("snapshot {vfs_path} -> {dest:?} ({} bytes)", data.len()),
        Err(e) => warn!("snapshot {vfs_path}: write {dest:?}: {e}"),
    }
}

// ---- per-open-handle context -------------------------------------------------

pub struct FileCtx {
    pub path:       String,
    pub is_dir:     bool,
    delete_pending: AtomicBool,
    /// Set when this specific handle has performed at least one mutation
    /// (write / overwrite / set_file_size). close() only spawns the flush
    /// thread for handles where this is true -- otherwise a concurrent
    /// read-only handle (typical: Windows thumbnail provider opening the
    /// file mid-save) racing past the writer's close would trigger a flush
    /// of the half-written overlay and emit corrupted bytes downstream.
    had_writes: AtomicBool,
    /// Bytes of the file at the moment this handle was opened (after any
    /// virtual-file rendering). Reads from this handle serve directly from
    /// here as long as the handle hasn't written, so two paginated reads
    /// always come from the same Arc -- without this, the underlying cache
    /// can be invalidated between read 1 and read 2 (post-save, when the
    /// flush thread calls invalidate_virtual_path), and Windows stitches
    /// "old chunk 1" + "new chunk 2" into a corrupted thumbnail.
    snapshot: OnceLock<Arc<[u8]>>,
    /// Sorted child list, populated on first read_directory call.
    /// We bypass winfsp's DirBuffer because for very large dirs (200K+ entries)
    /// it returns the marker entry itself once enumeration reaches the last
    /// entry, causing the kernel to loop forever instead of seeing EOF.
    dir_entries: OnceLock<Vec<DirEntry>>,
}

/// One direct child of a directory: name + cached FileInfo.  Sorted by `name`
/// (UTF-8 byte order, which matches WinFSP's case-sensitive wide compare for
/// the ASCII filenames used in the game data).
struct DirEntry {
    name:      String,
    file_info: FileInfo,
}

impl FileCtx {
    fn new(path: String, is_dir: bool) -> Self {
        FileCtx {
            path, is_dir,
            delete_pending: AtomicBool::new(false),
            had_writes: AtomicBool::new(false),
            snapshot: OnceLock::new(),
            dir_entries: OnceLock::new(),
        }
    }

    /// First-mutation hook: flips `had_writes` from false to true exactly
    /// once. Subsequent calls are no-ops; callers can invoke this from
    /// every overwrite/write/set_file_size without bookkeeping.
    fn note_writing(&self) {
        self.had_writes.store(true, Ordering::Relaxed);
    }

    /// Reported file size for THIS handle. Read-only handles return their
    /// open-time snapshot length, which is exactly what subsequent reads
    /// will deliver. Without this, `file_size_for(path)` can fall back to
    /// the source DDS size (when the rendered cache is invalidated mid-
    /// thumbnail-fetch) while reads still serve the smaller PNG snapshot,
    /// giving Windows a truncated thumbnail.
    fn reported_size(&self, shared: &SharedFs) -> u64 {
        if !self.had_writes.load(Ordering::Relaxed) {
            if let Some(s) = self.snapshot.get() { return s.len() as u64; }
        }
        shared.file_size_for(&self.path)
    }
}

// ---- SharedFs ----------------------------------------------------------------
// Identical role to cdfuse's SharedFs; callers hold Arc<SharedFs>.

pub struct SharedFs {
    vfs:           VfsManager,
    /// Lazy index of every `*.prefab` in the VFS. Powers the synthetic
    /// `/_prefabs/<stem>/...` subtree (manifest + asset pass-through).
    prefab_index:  prefab_view::PrefabIndex,
    decode_cache:  Mutex<LruCache<u64, Arc<[u8]>>>,
    cached_bytes:  AtomicUsize,
    in_flight:     Mutex<HashMap<u64, Arc<OnceLock<Option<Arc<[u8]>>>>>>,
    paz_maps:      Mutex<HashMap<String, Arc<Mmap>>>,
    // keyed by VFS path string (not ino -- no session-thread path map here)
    write_overlay: Mutex<HashMap<String, Vec<u8>>>,
    write_mtimes:  Mutex<HashMap<String, SystemTime>>,
    pending_paths: Mutex<HashSet<String>>,
    repack_engine:  RepackEngine,
    papgt_path:     String,
    readonly:       bool,
    auto_repack:    bool,
    recent_events:  Mutex<VecDeque<String>>,
}

impl SharedFs {
    fn new_inner(vfs: VfsManager, readonly: bool, auto_repack: bool) -> Self {
        let packages_path = vfs.packages_path().to_string();
        let papgt_path    = format!("{packages_path}/meta/0.papgt");
        let repack_engine = RepackEngine::new(&packages_path);

        // Bootstrap (or open) the user package group so create/unlink land in
        // a real on-disk PAMT the game also sees. Default group "9000" sorts
        // after every shipped group so its PAPGT slot is always "the next
        // free position". Errors are demoted to a warning -- a read-only
        // user group still permits browsing, just no writes.
        if !readonly {
            if let Err(e) = vfs.init_user_group("9000", std::path::Path::new(&papgt_path)) {
                warn!("user-group bootstrap failed: {e}; create/unlink will return WRITE_PROT");
            }
            // Older builds let editor temp files leak into 9000/0.pamt under
            // virtual-root prefixes (`.dds.png/...`). Prune those on mount so
            // they stop shadowing the read-only synth view and don't keep
            // reappearing if the user tries to delete them.
            for p in vfs.user_group_paths() {
                if is_under_virtual_root(&p) {
                    warn!("user_group: pruning stale entry {p:?} (under virtual root)");
                    if let Err(e) = vfs.remove_user_file(&p) {
                        warn!("user_group: failed to prune {p:?}: {e}");
                    }
                }
            }
        }
        SharedFs {
            vfs,
            prefab_index: prefab_view::PrefabIndex::new(),
            decode_cache:  Mutex::new(LruCache::new(NonZeroUsize::new(MAX_CACHE_ENTRIES).unwrap())),
            cached_bytes:  AtomicUsize::new(0),
            in_flight:     Mutex::new(HashMap::new()),
            paz_maps:      Mutex::new(HashMap::new()),
            write_overlay: Mutex::new(HashMap::new()),
            write_mtimes:  Mutex::new(HashMap::new()),
            pending_paths: Mutex::new(HashSet::new()),
            repack_engine,
            papgt_path,
            readonly,
            auto_repack,
            recent_events: Mutex::new(VecDeque::new()),
        }
    }


    pub fn is_readonly(&self) -> bool { self.readonly }

    pub fn push_event(&self, msg: String) {
        let mut q = self.recent_events.lock().unwrap();
        if q.len() >= 10 { q.pop_front(); }
        q.push_back(format!("{} {msg}", event_timestamp()));
    }

    pub fn recent_events(&self) -> Vec<String> {
        self.recent_events.lock().unwrap().iter().cloned().collect()
    }

    pub fn discard_pending(&self) {
        self.write_overlay.lock().unwrap().clear();
        self.write_mtimes.lock().unwrap().clear();
        self.pending_paths.lock().unwrap().clear();
    }

    pub fn flush_all_pending(&self) {
        let overlay = std::mem::take(&mut *self.write_overlay.lock().unwrap());
        let pending = std::mem::take(&mut *self.pending_paths.lock().unwrap());
        if pending.is_empty() { return; }
        info!("flush_all_pending: flushing {} write(s) to PAZ", pending.len());
        for path in &pending {
            if let Some(data) = overlay.get(path).cloned() {
                self.flush_path_sync(path, data);
            }
        }
    }

    pub fn pending_write_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.pending_paths.lock().unwrap().iter().cloned().collect();
        v.sort();
        v
    }

    // -- attr builders ---------------------------------------------------------

    fn file_info(&self, path: &str, size: u64) -> FileInfo {
        let mtime = self.write_mtimes.lock().unwrap()
            .get(path).copied()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let ft = to_filetime(mtime);
        let attrs = if self.readonly {
            FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE
        } else {
            FILE_ATTRIBUTE_ARCHIVE
        };
        FileInfo {
            file_attributes: attrs,
            reparse_tag:      0,
            allocation_size:  (size + 511) / 512 * 512,
            file_size:        size,
            creation_time:    ft,
            last_access_time: ft,
            last_write_time:  ft,
            change_time:      ft,
            index_number:     ino_for(path),
            hard_links:       0,
            ea_size:          0,
        }
    }

    fn dir_info(&self, path: &str) -> FileInfo {
        FileInfo {
            file_attributes: FILE_ATTRIBUTE_DIRECTORY,
            reparse_tag:      0,
            allocation_size:  0,
            file_size:        0,
            creation_time:    0,
            last_access_time: 0,
            last_write_time:  0,
            change_time:      0,
            index_number:     ino_for(path),
            hard_links:       0,
            ea_size:          0,
        }
    }

    fn file_size_for(&self, path: &str) -> u64 {
        if let Some(d) = self.write_overlay.lock().unwrap().get(path) {
            return d.len() as u64;
        }
        // Decoded/rendered size wins over source size: virtual files (.dds.png,
        // .wem.ogg, etc.) are populated into the cache by `open`, so reads see
        // the rendered length rather than the source's orig_size.
        if let Some(d) = self.cache_get(ino_for(path)) {
            return d.len() as u64;
        }
        // Prefab synth files: report content size (manifest) or pass-through
        // VFS entry size for prefab.prefab and assets/*.
        if let Some(pp) = prefab_view::classify(path) {
            use prefab_view::PrefabPath;
            return match pp {
                PrefabPath::Manifest { stem } => self.prefab_index
                    .full_path_of(&self.vfs, stem)
                    .map(|full| prefab_view::synth_manifest(&self.vfs, &full).len() as u64)
                    .unwrap_or(0),
                PrefabPath::PrefabFile { stem } => self.prefab_index
                    .full_path_of(&self.vfs, stem)
                    .and_then(|full| self.vfs.lookup(&full).map(|e| e.orig_size as u64))
                    .unwrap_or(0),
                PrefabPath::AssetsEntry { stem, relpath } => self.prefab_index
                    .full_path_of(&self.vfs, stem)
                    .and_then(|full| prefab_view::vfs_path_for_asset(&self.vfs, &full, relpath))
                    .and_then(|p| self.vfs.lookup(&p).map(|e| e.orig_size as u64))
                    .unwrap_or(0),
                _ => 0,
            };
        }
        if let Some(e) = self.vfs.lookup(path) {
            return e.orig_size as u64;
        }
        // Virtual file whose source exists but we haven't decoded yet: report
        // the source size as a placeholder.  `open` triggers a decode, so by
        // the time the kernel actually reads, the cache_get above hits.
        if let Some(vf) = virtual_files::resolve(path) {
            if let Some(e) = self.vfs.lookup(&vf.source_path) {
                return e.orig_size as u64;
            }
        }
        0
    }

    // -- mmap pool -------------------------------------------------------------

    fn get_mmap(&self, paz_path: &str) -> Option<Arc<Mmap>> {
        {
            let maps = self.paz_maps.lock().unwrap();
            if let Some(m) = maps.get(paz_path) { return Some(Arc::clone(m)); }
        }
        let f = std::fs::File::open(paz_path).ok()?;
        let m = Arc::new(unsafe { Mmap::map(&f).ok()? });
        self.paz_maps.lock().unwrap()
            .entry(paz_path.to_string()).or_insert_with(|| Arc::clone(&m));
        Some(m)
    }

    // -- decode cache ----------------------------------------------------------

    fn cache_get(&self, ino: u64) -> Option<Arc<[u8]>> {
        self.decode_cache.lock().unwrap().get(&ino).map(Arc::clone)
    }

    fn cache_put(&self, ino: u64, data: Arc<[u8]>) {
        let len = data.len();
        let mut cache = self.decode_cache.lock().unwrap();
        while self.cached_bytes.load(Ordering::Relaxed) + len > MAX_CACHED_BYTES {
            match cache.pop_lru() {
                Some((_, e)) => { self.cached_bytes.fetch_sub(e.len(), Ordering::Relaxed); }
                None => break,
            }
        }
        cache.put(ino, Arc::clone(&data));
        self.cached_bytes.fetch_add(len, Ordering::Relaxed);
    }

    // -- full decode -----------------------------------------------------------
    // Called synchronously from WinFSP dispatcher threads.
    // OnceLock deduplicates concurrent cold-decode requests for the same ino.

    pub fn decode(&self, ino: u64, path: &str) -> Option<Arc<[u8]>> {
        if let Some(d) = self.cache_get(ino) { return Some(d); }

        let slot = {
            let mut map = self.in_flight.lock().unwrap();
            Arc::clone(map.entry(ino).or_insert_with(|| Arc::new(OnceLock::new())))
        };

        let result = slot.get_or_init(|| {
            let t = Instant::now();
            let r = self.decode_inner(ino, path);
            let ms = t.elapsed().as_millis();
            if ms >= SLOW_MS { warn!("SLOW decode {path:?} {ms}ms"); }
            r
        });

        self.in_flight.lock().unwrap().remove(&ino);

        if let Some(data) = result {
            self.cache_put(ino, Arc::clone(data));
            Some(Arc::clone(data))
        } else {
            None
        }
    }

    fn decode_inner(&self, _ino: u64, path: &str) -> Option<Arc<[u8]>> {
        // Prefab-view synth: manifest.json (built from parsed prefab),
        // prefab.prefab and assets/* (delegated to underlying VFS path).
        {
            use prefab_view::PrefabPath;
            if let Some(pp) = prefab_view::classify(path) {
                match pp {
                    PrefabPath::Manifest { stem } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        return Some(Arc::from(prefab_view::synth_manifest(&self.vfs, &full)));
                    }
                    PrefabPath::PrefabFile { stem } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        let src_ino = ino_for(&full);
                        return self.decode(src_ino, &full);
                    }
                    PrefabPath::AssetsEntry { stem, relpath } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        let asset_path = prefab_view::vfs_path_for_asset(&self.vfs, &full, relpath)?;
                        if prefab_view::is_dds_png_relpath(&self.vfs, &full, relpath) {
                            let src_ino  = ino_for(&asset_path);
                            let src_data = self.decode(src_ino, &asset_path)?;
                            let png      = virtual_files::render_dds_png(&src_data, &asset_path)?;
                            return Some(Arc::from(png));
                        }
                        let src_ino = ino_for(&asset_path);
                        return self.decode(src_ino, &asset_path);
                    }
                    PrefabPath::MeshFbx { stem } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        return prefab_view::synth_mesh_fbx(&self.vfs, &full).map(Arc::from);
                    }
                    PrefabPath::FbmEntry { stem, relpath } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        let dds_path = prefab_view::dds_path_for_fbm_png(&self.vfs, &full, relpath)?;
                        let src_ino  = ino_for(&dds_path);
                        let src_data = self.decode(src_ino, &dds_path)?;
                        let png      = virtual_files::render_dds_png(&src_data, &dds_path)?;
                        return Some(Arc::from(png));
                    }
                    _ => return None, // Root / BundleDir / AssetsDir / FbmDir are directories.
                }
            }
        }

        // Virtual files: render on-the-fly from source.
        if let Some(vf) = virtual_files::resolve(path) {
            let src_ino  = ino_for(&vf.source_path);
            let src_data = self.decode(src_ino, &vf.source_path)?;
            let bytes = match vf.kind {
                virtual_files::VirtualKind::PalocJson => {
                    virtual_files::render_paloc(&src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::PabgbJson => {
                    let pabgh = vf.source_path.strip_suffix(".pabgb")
                        .map(|b| format!("{b}.pabgh"))?;
                    let pabgh_data = self.decode(ino_for(&pabgh), &pabgh)?;
                    virtual_files::render_pabgb(&pabgh_data, &src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::PrefabJsonl => {
                    virtual_files::render_prefab(&src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::PaaMetabinJsonl => {
                    virtual_files::render_paa_metabin(&src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::NavJsonl => {
                    virtual_files::render_nav(&src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::DdsPng => {
                    virtual_files::render_dds_png(&src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::PamFbx => {
                    virtual_files::render_pam_fbx(&src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::PamlodFbx => {
                    virtual_files::render_pamlod_fbx(&src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::PacFbx => {
                    virtual_files::render_pac_fbx(&src_data, &vf.source_path)?
                }
                virtual_files::VirtualKind::WemOgg => {
                    virtual_files::render_wem_ogg(&src_data, &vf.source_path)?
                }
            };
            return Some(Arc::from(bytes));
        }

        // Real VFS file.
        let entry = self.vfs.lookup(path)?;
        let raw: Vec<u8> = if let Some(mmap) = self.get_mmap(&entry.paz_file) {
            let start = entry.offset as usize;
            if start >= mmap.len() {
                warn!("decode {path}: offset {start} >= mmap len {}", mmap.len());
                return None;
            }
            let end = (start + entry.comp_size as usize).min(mmap.len());
            mmap[start..end].to_vec()
        } else {
            // Use cdcore::archive::paz::read_bytes -- it opens with full
            // read/write/delete sharing on Windows, so a concurrent repack
            // append on the same archive does not trigger `os error 32`.
            match cdcore::archive::paz::read_bytes(
                &entry.paz_file, entry.offset, entry.comp_size as usize,
            ) {
                Ok(buf) => buf,
                Err(e)  => { warn!("decode {path}: read: {e}"); return None; }
            }
        };

        let mut data = raw;
        if entry.encrypted() {
            let bn = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
            crypto::decrypt_inplace(&mut data, bn);
        }
        if entry.compressed() && entry.compression_type() != 0 {
            data = compression::decompress(
                &data, entry.orig_size as usize, entry.compression_type()).ok()?;
        }
        Some(Arc::from(data))
    }

    // -- write overlay helpers -------------------------------------------------

    fn seed_overlay(&self, path: &str, ino: u64) {
        if self.write_overlay.lock().unwrap().contains_key(path) { return; }
        let seed = self.cache_get(ino)
            .map(|d| d.to_vec())
            .unwrap_or_else(|| self.decode(ino, path).map(|d| d.to_vec()).unwrap_or_default());
        self.write_overlay.lock().unwrap().entry(path.to_string()).or_insert(seed);
    }

    /// Finalise a virtual-file save: drop the in-flight overlay and put the
    /// exact bytes the user just saved into the rendered-output cache.
    /// Called once the recursive flush_path_sync(source) returns successfully.
    ///
    /// We deliberately do NOT clear the cache and let it lazy-rerender from
    /// the new source: that left a window where `file_size_for` fell back
    /// to the source DDS size while subsequent reads still served PNG
    /// bytes. Windows latches the source-size as the file size and then
    /// truncates the read -- visible as a missing strip at the bottom of
    /// the thumbnail. Caching the saved bytes directly keeps file_size_for
    /// consistent with what reads return, and lets the user see the exact
    /// image they just saved without a lossy DDS round-trip.
    fn commit_virtual_path(&self, virtual_path: &str, saved_bytes: Vec<u8>) {
        self.write_overlay.lock().unwrap().remove(virtual_path);
        self.cache_put(ino_for(virtual_path), Arc::from(saved_bytes));
    }

    // -- flush to PAZ ----------------------------------------------------------

    pub fn flush_path_sync(&self, path: &str, data: Vec<u8>) {
        self.pending_paths.lock().unwrap().remove(path);
        // Keep `write_mtimes[path]` populated past flush -- without it
        // file_info reverts to UNIX_EPOCH, and Explorer's thumbnail cache
        // (keyed on path+size+mtime) never invalidates between saves.
        self.write_mtimes.lock().unwrap().insert(path.to_string(), SystemTime::now());

        // Prefab-view synth `mesh.fbx`: write FBX bytes to a temp file, import
        // back into a ParsedMesh, build new mesh bytes via the appropriate
        // builder for the underlying format, then recurse to flush the real path.
        if let Some(prefab_view::PrefabPath::MeshFbx { stem }) = prefab_view::classify(path) {
            let full = match self.prefab_index.full_path_of(&self.vfs, stem) {
                Some(s) => s,
                None    => { warn!("flush mesh.fbx {path}: prefab stem not found"); return; }
            };
            let mesh_path = match prefab_view::primary_mesh_path(&self.vfs, &full) {
                Some(p) => p,
                None    => { warn!("flush mesh.fbx {path}: no resolvable mesh"); return; }
            };
            let tmp = std::env::temp_dir().join(format!("cdwinfs_save_{}.fbx", std::process::id()));
            if let Err(e) = std::fs::write(&tmp, &data) {
                warn!("flush mesh.fbx {path}: temp write {tmp:?}: {e}"); return;
            }
            let mesh = match cdcore::repack::mesh::import_fbx(&tmp) {
                Ok(m) => m,
                Err(e) => {
                    let msg = format!("import_fbx {path}: {e}");
                    warn!("{msg}");
                    self.push_event(format!("[err] {msg}"));
                    let _ = std::fs::remove_file(&tmp);
                    return;
                }
            };
            let _ = std::fs::remove_file(&tmp);

            let entry = match self.vfs.lookup(&mesh_path) {
                Some(e) => e,
                None    => { warn!("flush {path}: target mesh {mesh_path} not in VFS"); return; }
            };
            let original = match self.vfs.read_entry(&entry) {
                Ok(b) => b,
                Err(e) => { warn!("flush {path}: read {mesh_path}: {e}"); return; }
            };

            let lower = mesh_path.to_lowercase();
            let result = if lower.ends_with(".pac") {
                cdcore::repack::mesh::build_pac(&mesh, &original)
            } else if lower.ends_with(".pamlod") {
                cdcore::repack::mesh::build_pamlod(&mesh, &original)
            } else if lower.ends_with(".pam") {
                cdcore::repack::mesh::build_pam(&mesh, &original)
            } else {
                warn!("flush {path}: unsupported mesh extension {mesh_path}");
                return;
            };
            match result {
                Ok(new_bytes) => {
                    info!("flush mesh.fbx {path}: built {}B for {mesh_path}", new_bytes.len());
                    self.flush_path_sync(&mesh_path, new_bytes);
                    self.commit_virtual_path(path, data);
                }
                Err(e) => {
                    let msg = format!("build_mesh {path}: {e}");
                    warn!("{msg}");
                    self.push_event(format!("[err] {msg}"));
                }
            }
            return;
        }

        if let Some(vf) = virtual_files::resolve(path) {
            match vf.kind {
                virtual_files::VirtualKind::PalocJson => {
                    match virtual_files::parse_paloc_jsonl(&data) {
                        Some(binary) => {
                            info!("flush {path}: paloc -> {}B, repacking {}", binary.len(), vf.source_path);
                            self.flush_path_sync(&vf.source_path, binary);
                            self.commit_virtual_path(path, data);
                        }
                        None => {
                            let msg = format!("paloc parse failed: {path}");
                            warn!("{msg}");
                            self.push_event(format!("[err] {msg}"));
                        }
                    }
                }
                virtual_files::VirtualKind::DdsPng => {
                    let src_entry = match self.vfs.lookup(&vf.source_path) {
                        Some(e) => e,
                        None    => { warn!("flush {path}: source {} not in VFS", vf.source_path); return; }
                    };
                    let orig_dds = match self.vfs.read_entry(&src_entry) {
                        Ok(d)  => d,
                        Err(e) => { warn!("flush {path}: read source DDS: {e}"); return; }
                    };
                    match virtual_files::parse_png_to_dds(&data, &orig_dds, &vf.source_path) {
                        Some(dds) => {
                            info!("flush {path}: PNG -> {}B DDS", dds.len());
                            self.flush_path_sync(&vf.source_path, dds);
                            self.commit_virtual_path(path, data);
                        }
                        None => {
                            let msg = format!("PNG->DDS failed: {path}");
                            warn!("{msg}");
                            self.push_event(format!("[err] {msg}"));
                        }
                    }
                }
                _ => warn!("flush {path}: write-back not implemented for this virtual format"),
            }
            return;
        }

        // User-group paths bypass the shared repack engine: their PAMT is
        // rewritten from scratch on every change, and they own a single
        // private PAZ. Forward the bytes to UserGroup::add via VfsManager.
        if self.vfs.is_user_path(path) || self.vfs.user_group_ready() && self.vfs.lookup(path).is_none() {
            // Defensive: never persist a user-group entry that lives under a
            // synth root prefix. create() already rejects such paths, but
            // an in-flight overlay from before the create-guard existed could
            // still reach us; drop it instead of polluting 9000/0.pamt.
            if is_under_virtual_root(path) {
                info!("flush {path}: refused user_group write -- under virtual root");
                self.write_overlay.lock().unwrap().remove(path);
                return;
            }
            if let Some(prev) = self.vfs.read_user_file(path) {
                save_snapshot(path, &prev);
            }
            match self.vfs.create_user_file(path, &data) {
                Ok(entry) => {
                    info!("user_group: persisted {path} ({} bytes -> orig_size={})",
                          data.len(), entry.orig_size);
                    self.push_event(format!("[ok]  user_group {path}"));
                    self.cache_put(ino_for(path), Arc::from(data));
                }
                Err(e) => {
                    let msg = format!("user_group write failed for {path}: {e}");
                    warn!("{msg}");
                    self.push_event(format!("[err] {msg}"));
                }
            }
            return;
        }

        let entry = match self.vfs.lookup(path) {
            Some(e) => e,
            None    => { warn!("flush {path}: not in VFS"); return; }
        };
        // Snapshot the original shipped bytes before the repack rewrites them.
        match self.vfs.read_entry(&entry) {
            Ok(prev) => save_snapshot(path, &prev),
            Err(e)   => warn!("snapshot {path}: read source for snapshot: {e}"),
        }
        let group_dir = Path::new(&entry.paz_file)
            .parent().and_then(|p| p.file_name()).and_then(|n| n.to_str())
            .unwrap_or("").to_string();
        let pamt_data = match self.vfs.get_pamt(&group_dir) {
            Some(p) => p,
            None    => { warn!("flush {path}: no pamt for group {group_dir}"); return; }
        };
        self.cache_put(ino_for(path), Arc::from(data.clone()));
        let mf = ModifiedFile { data, entry: entry.clone(), pamt_data, package_group: group_dir.clone() };
        // Drop our mmap of this PAZ before the repack appends to it.  On
        // Windows, even with FILE_SHARE_WRITE on the writer side, holding an
        // active mapping of a file we are about to extend is a recipe for
        // edge-case races; releasing it makes the append unambiguously safe.
        self.paz_maps.lock().unwrap().remove(&entry.paz_file);
        match self.repack_engine.repack(vec![mf], &self.papgt_path) {
            Ok(r) if r.success => {
                info!("repack {path}: ok");
                self.push_event(format!("[ok]  repacked {path}"));
                if let Err(e) = self.vfs.reload_group(&group_dir) {
                    warn!("repack {path}: reload_group failed: {e}");
                }
            }
            Ok(r) => {
                let msg = format!("repack errors: {path}: {:?}", r.errors);
                warn!("{msg}");
                self.push_event(format!("[err] {msg}"));
            }
            Err(e) => {
                let msg = format!("repack failed: {path}: {e}");
                warn!("{msg}");
                self.push_event(format!("[err] {msg}"));
            }
        }
    }

    // -- directory enumeration -------------------------------------------------
    // Build a sorted Vec<DirEntry> for a directory.  Called once per open handle,
    // cached in FileCtx.dir_entries; subsequent paginated reads index into it.

    fn build_dir_entries(&self, path: &str) -> Vec<DirEntry> {
        let mut out: Vec<DirEntry> = Vec::new();
        out.push(DirEntry { name: ".".into(),  file_info: self.dir_info(path) });
        out.push(DirEntry { name: "..".into(), file_info: self.dir_info(parent_path(path)) });

        // Prefab synth tree handled separately so paths under `_prefabs/`
        // resolve even though they don't exist in the game files.
        if prefab_view::classify(path).is_some() {
            self.append_prefab_dir(&mut out, path);
            out.sort_by(|a, b| a.name.cmp(&b.name));
            return out;
        }

        // Virtual root directories appear only at the filesystem root.
        if path.is_empty() {
            for vdir_name in virtual_files::virtual_root_dirs() {
                if virtual_files::root_requires_vgmstream(vdir_name) {
                    continue;
                }
                out.push(DirEntry {
                    name:      vdir_name.to_string(),
                    file_info: self.dir_info(vdir_name),
                });
            }
            // Surface the synthetic `_prefabs` root alongside the other virtual roots.
            out.push(DirEntry {
                name:      prefab_view::PREFAB_ROOT_NAME.to_string(),
                file_info: self.dir_info(prefab_view::PREFAB_ROOT_NAME),
            });
        }

        if let Some(vdir) = virtual_files::resolve_virtual_dir(path) {
            self.append_virtual_dir(&mut out, path, &vdir);
        } else {
            // Regular VFS children.
            let children = self.vfs.list_dir_with_sizes_unsorted(path);
            for (name, is_dir, orig_size) in children {
                // At root, hide stale user-group entries whose first path
                // segment collides with one of our synthetic root names --
                // we already inserted the canonical synth root above, and
                // letting the VFS variant through too produces a duplicate
                // entry like the second `.dds.png` folder.
                if path.is_empty() && is_under_virtual_root(&name) {
                    continue;
                }
                let cpath = child_path(path, &name);
                let fi = if is_dir {
                    self.dir_info(&cpath)
                } else {
                    let size = self.write_overlay.lock().unwrap()
                        .get(&cpath).map(|d| d.len() as u64)
                        .unwrap_or(orig_size as u64);
                    self.file_info(&cpath, size)
                };
                out.push(DirEntry { name, file_info: fi });
            }
        }

        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn append_prefab_dir(&self, out: &mut Vec<DirEntry>, path: &str) {
        use prefab_view::PrefabPath;
        let pp = match prefab_view::classify(path) {
            Some(p) => p,
            None    => return,
        };
        match pp {
            PrefabPath::Root => {
                for stem in self.prefab_index.stems(&self.vfs) {
                    let cp = format!("{}/{stem}", prefab_view::PREFAB_ROOT_NAME);
                    out.push(DirEntry { name: stem, file_info: self.dir_info(&cp) });
                }
            }
            PrefabPath::BundleDir { stem } => {
                let mp = format!("{}/{stem}/{}", prefab_view::PREFAB_ROOT_NAME, prefab_view::MANIFEST_NAME);
                let mlen = self.prefab_index.full_path_of(&self.vfs, stem)
                    .map(|full| prefab_view::synth_manifest(&self.vfs, &full).len() as u64)
                    .unwrap_or(0);
                out.push(DirEntry {
                    name: prefab_view::MANIFEST_NAME.to_string(),
                    file_info: self.file_info(&mp, mlen),
                });
                if let Some(full) = self.prefab_index.full_path_of(&self.vfs, stem) {
                    if let Some(e) = self.vfs.lookup(&full) {
                        let pp = format!("{}/{stem}/prefab.prefab", prefab_view::PREFAB_ROOT_NAME);
                        out.push(DirEntry {
                            name: "prefab.prefab".to_string(),
                            file_info: self.file_info(&pp, e.orig_size as u64),
                        });
                    }
                    if prefab_view::primary_mesh_path(&self.vfs, &full).is_some() {
                        let fp = format!("{}/{stem}/{}", prefab_view::PREFAB_ROOT_NAME, prefab_view::MESH_FBX_NAME);
                        out.push(DirEntry {
                            name: prefab_view::MESH_FBX_NAME.to_string(),
                            file_info: self.file_info(&fp, 0),
                        });
                        let dp = format!("{}/{stem}/{}", prefab_view::PREFAB_ROOT_NAME, prefab_view::MESH_FBM_DIR);
                        out.push(DirEntry {
                            name: prefab_view::MESH_FBM_DIR.to_string(),
                            file_info: self.dir_info(&dp),
                        });
                    }
                }
                let ap = format!("{}/{stem}/{}", prefab_view::PREFAB_ROOT_NAME, prefab_view::ASSETS_DIR_NAME);
                out.push(DirEntry {
                    name: prefab_view::ASSETS_DIR_NAME.to_string(),
                    file_info: self.dir_info(&ap),
                });
            }
            PrefabPath::AssetsDir { stem } => {
                if let Some(full) = self.prefab_index.full_path_of(&self.vfs, stem) {
                    self.append_assets_tree(
                        out, &prefab_view::assets_dir_children(&self.vfs, &full, ""),
                        stem, prefab_view::ASSETS_DIR_NAME, "", true);
                }
            }
            PrefabPath::FbmDir { stem } => {
                if let Some(full) = self.prefab_index.full_path_of(&self.vfs, stem) {
                    self.append_assets_tree(
                        out, &prefab_view::fbm_dir_children(&self.vfs, &full, ""),
                        stem, prefab_view::MESH_FBM_DIR, "", false);
                }
            }
            PrefabPath::AssetsEntry { stem, relpath } => {
                if let Some(full) = self.prefab_index.full_path_of(&self.vfs, stem) {
                    self.append_assets_tree(
                        out, &prefab_view::assets_dir_children(&self.vfs, &full, relpath),
                        stem, prefab_view::ASSETS_DIR_NAME, relpath, true);
                }
            }
            PrefabPath::FbmEntry { stem, relpath } => {
                if let Some(full) = self.prefab_index.full_path_of(&self.vfs, stem) {
                    self.append_assets_tree(
                        out, &prefab_view::fbm_dir_children(&self.vfs, &full, relpath),
                        stem, prefab_view::MESH_FBM_DIR, relpath, false);
                }
            }
            PrefabPath::Manifest { .. }
            | PrefabPath::PrefabFile { .. }
            | PrefabPath::MeshFbx { .. } => {}
        }
    }

    fn append_assets_tree(
        &self,
        out: &mut Vec<DirEntry>,
        children: &[prefab_view::AssetsTreeChild],
        stem: &str,
        tree_root: &str,
        prefix: &str,
        size_lookup_vfs: bool,
    ) {
        let dir_base = if prefix.is_empty() {
            format!("{}/{stem}/{tree_root}", prefab_view::PREFAB_ROOT_NAME)
        } else {
            format!("{}/{stem}/{tree_root}/{prefix}", prefab_view::PREFAB_ROOT_NAME)
        };
        for child in children {
            match child {
                prefab_view::AssetsTreeChild::Dir { name } => {
                    let cp = format!("{dir_base}/{name}");
                    out.push(DirEntry { name: name.clone(), file_info: self.dir_info(&cp) });
                }
                prefab_view::AssetsTreeChild::File { relpath, name, is_dds_png } => {
                    let cp = format!("{dir_base}/{name}");
                    let size = if *is_dds_png {
                        0
                    } else if size_lookup_vfs {
                        self.vfs.lookup(relpath).map(|e| e.orig_size as u64).unwrap_or(0)
                    } else {
                        0
                    };
                    out.push(DirEntry { name: name.clone(), file_info: self.file_info(&cp, size) });
                }
            }
        }
    }

    fn append_virtual_dir(
        &self,
        out:   &mut Vec<DirEntry>,
        vpath: &str,
        vdir:  &virtual_files::VirtualDirInfo,
    ) {
        let children = self.vfs.list_dir_with_sizes_unsorted(&vdir.real_path);
        for (name, is_dir, orig_size) in children {
            if is_dir {
                let real_child = child_path(&vdir.real_path, &name);
                if !self.vfs.subtree_has_ext(&real_child, vdir.filter_ext) { continue; }
                let cvpath = child_path(vpath, &name);
                out.push(DirEntry { name, file_info: self.dir_info(&cvpath) });
            } else if name.ends_with(vdir.filter_ext) {
                let should_add = if vdir.filter_ext == ".pabgb" {
                    name.strip_suffix(".pabgb").is_some_and(|base| {
                        let sibling = child_path(&vdir.real_path, &format!("{base}.pabgh"));
                        self.vfs.lookup(&sibling).is_some()
                    })
                } else {
                    true
                };
                if !should_add { continue; }
                let virt_name = if vdir.suffix.is_empty() {
                    name
                } else {
                    format!("{name}{}", vdir.suffix)
                };
                let cvpath = child_path(vpath, &virt_name);
                let fi = self.file_info(&cvpath, orig_size as u64);
                out.push(DirEntry { name: virt_name, file_info: fi });
            }
        }
    }

}

// ---- CdWinFs -- thin wrapper, implements FileSystemContext -------------------

pub struct CdWinFs(Arc<SharedFs>);

impl CdWinFs {
    pub fn new(vfs: VfsManager, readonly: bool, auto_repack: bool) -> Self {
        CdWinFs(Arc::new(SharedFs::new_inner(vfs, readonly, auto_repack)))
    }

    pub fn shared(&self) -> Arc<SharedFs> { Arc::clone(&self.0) }

    fn lookup(&self, path: &str) -> Option<(FileInfo, bool)> {
        if path.is_empty() {
            return Some((self.0.dir_info(""), true));
        }
        // Prefab synth tree: classify-then-validate before falling through to
        // the real VFS, so paths under `_prefabs/` resolve.
        if let Some(pp) = prefab_view::classify(path) {
            use prefab_view::PrefabPath;
            let (valid, is_dir) = match &pp {
                PrefabPath::Root => (true, true),
                PrefabPath::BundleDir { stem }
                | PrefabPath::Manifest { stem }
                | PrefabPath::PrefabFile { stem }
                | PrefabPath::AssetsDir { stem } => {
                    let exists = self.0.prefab_index.full_path_of(&self.0.vfs, stem).is_some();
                    let is_dir = matches!(pp,
                        PrefabPath::Root | PrefabPath::BundleDir { .. } | PrefabPath::AssetsDir { .. });
                    (exists, is_dir)
                }
                PrefabPath::AssetsEntry { stem, relpath } => {
                    if let Some(full) = self.0.prefab_index.full_path_of(&self.0.vfs, stem) {
                        if prefab_view::vfs_path_for_asset(&self.0.vfs, &full, relpath).is_some() {
                            (true, false)
                        } else if prefab_view::is_assets_subdir(&self.0.vfs, &full, relpath) {
                            (true, true)
                        } else {
                            (false, false)
                        }
                    } else {
                        (false, false)
                    }
                }
                PrefabPath::MeshFbx { stem }
                | PrefabPath::FbmDir { stem } => {
                    let has_mesh = self.0.prefab_index.full_path_of(&self.0.vfs, stem)
                        .and_then(|full| prefab_view::primary_mesh_path(&self.0.vfs, &full))
                        .is_some();
                    let is_dir = matches!(pp, PrefabPath::FbmDir { .. });
                    (has_mesh, is_dir)
                }
                PrefabPath::FbmEntry { stem, relpath } => {
                    if let Some(full) = self.0.prefab_index.full_path_of(&self.0.vfs, stem) {
                        if prefab_view::dds_path_for_fbm_png(&self.0.vfs, &full, relpath).is_some() {
                            (true, false)
                        } else if prefab_view::is_fbm_subdir(&self.0.vfs, &full, relpath) {
                            (true, true)
                        } else {
                            (false, false)
                        }
                    } else {
                        (false, false)
                    }
                }
            };
            if !valid { return None; }
            if is_dir {
                return Some((self.0.dir_info(path), true));
            }
            return Some((self.0.file_info(path, self.0.file_size_for(path)), false));
        }
        if let Some(entry) = self.0.vfs.lookup(path) {
            let size = self.0.write_overlay.lock().unwrap()
                .get(path).map(|d| d.len() as u64)
                .unwrap_or(entry.orig_size as u64);
            return Some((self.0.file_info(path, size), false));
        }
        if self.0.vfs.dir_exists(path) {
            return Some((self.0.dir_info(path), true));
        }
        {
            let ov = self.0.write_overlay.lock().unwrap();
            if let Some(data) = ov.get(path) {
                return Some((self.0.file_info(path, data.len() as u64), false));
            }
        }
        if let Some(vf) = virtual_files::resolve(path) {
            if self.0.vfs.lookup(&vf.source_path).is_some() {
                let ino  = ino_for(path);
                let size = self.0.cache_get(ino)
                    .map(|d| d.len() as u64)
                    .or_else(|| self.0.vfs.lookup(&vf.source_path).map(|e| e.orig_size as u64))
                    .unwrap_or(0);
                return Some((self.0.file_info(path, size), false));
            }
        }
        if let Some(vdir) = virtual_files::resolve_virtual_dir(path) {
            let exists = vdir.real_path.is_empty()
                || self.0.vfs.subtree_has_ext(&vdir.real_path, vdir.filter_ext);
            if exists { return Some((self.0.dir_info(path), true)); }
        }
        None
    }
}

// ---- FileSystemContext impl --------------------------------------------------

impl FileSystemContext for CdWinFs {
    type FileContext = FileCtx;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [std::ffi::c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Result<FileSecurity> {
        let path = vfs_path(file_name);
        match self.lookup(&path) {
            Some((fi, _)) => Ok(FileSecurity {
                reparse: false,
                sz_security_descriptor: 0,
                attributes: fi.file_attributes,
            }),
            None => Err(FspError::NTSTATUS(NTSTATUS_NOT_FOUND)),
        }
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let path = vfs_path(file_name);
        let (mut fi, is_dir) = match self.lookup(&path) {
            Some(x)  => x,
            None     => return Err(FspError::NTSTATUS(NTSTATUS_NOT_FOUND)),
        };

        // Decode (and pin) the bytes at open time for non-directory handles.
        // Two reasons:
        //  1. For virtual files (.dds.png/foo.dds.png, etc.) the rendered
        //     output size differs from the source DDS/WEM/PAM size lookup()
        //     falls back to. Without an early decode, OpenFileInfo carries
        //     the placeholder size and Explorer reads a truncated PNG.
        //  2. We snapshot the resulting Arc<[u8]> into the FileCtx so all
        //     reads on this handle return slices of the same buffer. Without
        //     that, paginated reads can straddle a cache invalidation
        //     (post-save invalidate_virtual_path) -- chunk 1 from the old
        //     rendering, chunk 2 from the new rendering -- producing a
        //     corrupted thumbnail in the shell thumbcache.
        let mut snapshot: Option<Arc<[u8]>> = None;
        if !is_dir {
            let ino = ino_for(&path);
            if let Some(data) = self.0.decode(ino, &path) {
                fi = self.0.file_info(&path, data.len() as u64);
                snapshot = Some(data);
            }
        }

        *file_info.as_mut() = fi;
        let ctx = FileCtx::new(path, is_dir);
        if let Some(s) = snapshot { let _ = ctx.snapshot.set(s); }
        Ok(ctx)
    }

    fn close(&self, context: Self::FileContext) {
        // Only flush from handles that actually wrote. A read-only handle
        // (e.g. shell thumbnail provider) closing during another handle's
        // in-progress save would otherwise claim pending_paths and flush a
        // partially-filled overlay, producing corrupted output.
        if !context.had_writes.load(Ordering::Relaxed) { return; }
        // Atomically claim the pending flush so concurrent writer closes can't
        // both spawn a flush thread for the same path.
        let claimed = self.0.pending_paths.lock().unwrap().remove(&context.path);
        if claimed && self.0.auto_repack {
            if let Some(data) = self.0.write_overlay.lock().unwrap().get(&context.path).cloned() {
                let shared = Arc::clone(&self.0);
                let path   = context.path.clone();
                std::thread::spawn(move || {
                    shared.flush_path_sync(&path, data);
                });
            }
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        if self.0.readonly { return Err(FspError::NTSTATUS(NTSTATUS_WRITE_PROT)); }
        const FILE_DIRECTORY_FILE: u32 = 0x1;
        let path = vfs_path(file_name);
        let now  = SystemTime::now();

        // mkdir: register a synthetic empty directory that survives until
        // rmdir or until a child file pins it implicitly via PAMT.
        if create_options & FILE_DIRECTORY_FILE != 0 {
            if self.0.vfs.lookup(&path).is_some() || self.0.vfs.dir_exists(&path) {
                return Err(FspError::NTSTATUS(NTSTATUS_OBJ_EXISTS));
            }
            self.0.vfs.add_synth_dir(&path);
            *file_info.as_mut() = self.0.dir_info(&path);
            return Ok(FileCtx::new(path, true));
        }

        // File create: STATUS_OBJECT_NAME_COLLISION when anything already
        // lives at this path. Per WinFSP API contract, create() is invoked
        // either for FILE_CREATE or as a fallback when open() reported
        // NOT_FOUND -- in both cases an existing file is a collision.
        // Without this, Explorer's drag-and-drop overwrite prompt is bypassed:
        // Explorer pre-checks via GetFileAttributesEx, sees the file, and
        // shows the prompt only if create() also reports the collision.
        if self.0.vfs.lookup(&path).is_some()
            || self.0.vfs.dir_exists(&path)
            || self.0.write_overlay.lock().unwrap().contains_key(&path)
        {
            return Err(FspError::NTSTATUS(NTSTATUS_OBJ_EXISTS));
        }

        // Refuse fresh files under our synthetic root prefixes -- those are
        // read-only render-on-demand views. Editors (Paint, etc.) sometimes
        // try to drop temp files alongside the file they opened; without
        // this guard those temp files end up persisted into user_group with
        // a `.dds.png/...` path, which shows up as a duplicate root entry
        // on remount and shadows the real virtual view.
        if is_under_virtual_root(&path) {
            info!("create {path}: refused -- path under read-only virtual root");
            return Err(FspError::NTSTATUS(NTSTATUS_ACCESS));
        }
        self.0.write_overlay.lock().unwrap().entry(path.clone()).or_insert_with(Vec::new);
        self.0.pending_paths.lock().unwrap().insert(path.clone());
        self.0.write_mtimes.lock().unwrap().insert(path.clone(), now);
        let ft = to_filetime(now);
        *file_info.as_mut() = FileInfo {
            file_attributes: FILE_ATTRIBUTE_ARCHIVE,
            reparse_tag:      0,
            allocation_size,
            file_size:        0,
            creation_time:    ft,
            last_access_time: ft,
            last_write_time:  ft,
            change_time:      ft,
            index_number:     ino_for(&path),
            hard_links:       0,
            ea_size:          0,
        };
        Ok(FileCtx::new(path, false))
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        if self.0.readonly { return Err(FspError::NTSTATUS(NTSTATUS_WRITE_PROT)); }
        let path = &context.path;
        context.note_writing();
        self.0.write_overlay.lock().unwrap().insert(path.clone(), Vec::new());
        self.0.pending_paths.lock().unwrap().insert(path.clone());
        self.0.write_mtimes.lock().unwrap().insert(path.clone(), SystemTime::now());
        *file_info = self.0.file_info(path, context.reported_size(&self.0));
        Ok(())
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let delete_pending = context.delete_pending.load(Ordering::Relaxed);
        let delete_flag    = flags & FSP_CLEANUP_DELETE != 0;
        if delete_flag || delete_pending {
            info!("cleanup {:?}: flags=0x{flags:08x} delete_flag={delete_flag} \
                   delete_pending={delete_pending} is_dir={}",
                  context.path, context.is_dir);
        }
        if delete_flag && delete_pending {
            let path = &context.path;
            // rmdir: only synthetic empty user dirs are removable.
            if context.is_dir {
                let prefix = format!("{}/", path);
                let has_children = self.0.vfs.list_dir(path).iter().any(|_| true)
                    || self.0.write_overlay.lock().unwrap().keys().any(|p| p.starts_with(&prefix));
                if has_children {
                    info!("cleanup rmdir {path}: refused -- not empty");
                    return;
                }
                let removed = self.0.vfs.remove_synth_dir(path);
                info!("cleanup rmdir {path}: synth_dir_removed={removed}");
                return;
            }
            // Delete is only permitted for files in the user package group.
            // Shipped paths are immutable; pure-overlay (pending) paths drop
            // the overlay only.
            let is_user = self.0.vfs.is_user_path(path);
            if !is_user {
                if self.0.vfs.lookup(path).is_some() {
                    info!("cleanup unlink {path}: refused -- shipped file");
                    return;
                }
                info!("cleanup unlink {path}: dropping overlay-only entry");
                self.0.write_overlay.lock().unwrap().remove(path);
                self.0.write_mtimes.lock().unwrap().remove(path);
                self.0.pending_paths.lock().unwrap().remove(path);
                return;
            }
            match self.0.vfs.remove_user_file(path) {
                Ok(true)  => info!("cleanup unlink {path}: user_group entry removed + PAMT rewritten"),
                Ok(false) => warn!("cleanup unlink {path}: user_group reported not-present"),
                Err(e)    => {
                    let msg = format!("user_group unlink {path}: {e}");
                    warn!("{msg}");
                    self.0.push_event(format!("[err] {msg}"));
                    return;
                }
            }
            self.0.decode_cache.lock().unwrap().pop(&ino_for(path));
            self.0.write_overlay.lock().unwrap().remove(path);
            self.0.write_mtimes.lock().unwrap().remove(path);
            self.0.pending_paths.lock().unwrap().remove(path);
        }
    }

    fn flush(&self, context: Option<&Self::FileContext>, file_info: &mut FileInfo) -> Result<()> {
        // FlushFileBuffers semantics: ensure cached data hits backing store.
        // Our backing store from the kernel's perspective IS the in-memory
        // write_overlay -- it's already "flushed" the moment write() returns.
        // Triggering a PAZ repack here used to take 7+ minutes per save: the
        // I/O manager + cache manager issue IRP_MJ_FLUSH_BUFFERS many times
        // during a single Save (per-chunk and on close), and each one was
        // running the full DDS encode + repack. The actual repack now runs
        // exactly once, on close().
        if let Some(ctx) = context {
            *file_info = self.0.file_info(&ctx.path, ctx.reported_size(&self.0));
        }
        Ok(())
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        if context.is_dir {
            *file_info = self.0.dir_info(&context.path);
        } else {
            *file_info = self.0.file_info(&context.path, context.reported_size(&self.0));
        }
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        // Timestamps are not persisted; echo back current state.
        self.get_file_info(context, file_info)
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<()> {
        info!("set_delete {:?} -> {delete_file}", context.path);
        context.delete_pending.store(delete_file, Ordering::Relaxed);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        if self.0.readonly { return Err(FspError::NTSTATUS(NTSTATUS_WRITE_PROT)); }
        if !set_allocation_size {
            let path = &context.path;
            context.note_writing();
            self.0.seed_overlay(path, ino_for(path));
            let mut ov = self.0.write_overlay.lock().unwrap();
            if let Some(buf) = ov.get_mut(path) { buf.resize(new_size as usize, 0); }
            self.0.write_mtimes.lock().unwrap().insert(path.clone(), SystemTime::now());
        }
        *file_info = self.0.file_info(&context.path, context.reported_size(&self.0));
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        let path = &context.path;
        let ino  = ino_for(path);
        let own_writes = context.had_writes.load(Ordering::Relaxed);

        // Read-only handle: serve from the open-time snapshot if we have one.
        // Two paginated reads from this handle then come from the same Arc,
        // so they can't straddle a cache invalidation and produce a Frankenstein
        // PNG in the shell thumbcache.
        if !own_writes {
            if let Some(snap) = context.snapshot.get() {
                let s = (offset as usize).min(snap.len());
                let e = (s + buffer.len()).min(snap.len());
                buffer[..e - s].copy_from_slice(&snap[s..e]);
                return Ok((e - s) as u32);
            }
        }

        // Writer reading its own bytes -- always serve from the overlay.
        if own_writes {
            let ov = self.0.write_overlay.lock().unwrap();
            if let Some(data) = ov.get(path) {
                let s = (offset as usize).min(data.len());
                let e = (s + buffer.len()).min(data.len());
                buffer[..e - s].copy_from_slice(&data[s..e]);
                return Ok((e - s) as u32);
            }
        }

        // Fallback (no snapshot taken at open, or post-write read with no
        // overlay yet): decode synchronously.
        match self.0.decode(ino, path) {
            Some(data) => {
                let s = (offset as usize).min(data.len());
                let e = (s + buffer.len()).min(data.len());
                buffer[..e - s].copy_from_slice(&data[s..e]);
                Ok((e - s) as u32)
            }
            None => Err(FspError::NTSTATUS(NTSTATUS_IO_ERR)),
        }
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32> {
        if self.0.readonly { return Err(FspError::NTSTATUS(NTSTATUS_WRITE_PROT)); }
        let path = &context.path;

        if let Some(vf) = virtual_files::resolve(path) {
            match vf.kind {
                virtual_files::VirtualKind::PalocJson => {}
                virtual_files::VirtualKind::DdsPng    => {}
                _ => return Err(FspError::NTSTATUS(NTSTATUS_WRITE_PROT)),
            }
        }

        context.note_writing();
        self.0.seed_overlay(path, ino_for(path));

        let n = {
            let mut ov = self.0.write_overlay.lock().unwrap();
            let buf = ov.entry(path.clone()).or_insert_with(Vec::new);
            // WinFSP sets `offset` to the current EOF position before calling us
            // when write_to_eof=true, so offset is always the correct write position.
            // Using write_to_eof to derive `len` was wrong: it produced len=0 and
            // silently dropped every write issued with WriteToEndOfFile=true.
            let start = if write_to_eof { buf.len() } else { offset as usize };
            if constrained_io && start > buf.len() {
                0
            } else {
                let end = start + buffer.len();
                if end > buf.len() { buf.resize(end, 0); }
                buf[start..end].copy_from_slice(buffer);
                buffer.len()
            }
        };

        self.0.write_mtimes.lock().unwrap().insert(path.clone(), SystemTime::now());
        self.0.pending_paths.lock().unwrap().insert(path.clone());
        *file_info = self.0.file_info(path, context.reported_size(&self.0));
        Ok(n as u32)
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> Result<()> {
        if self.0.readonly { return Err(FspError::NTSTATUS(NTSTATUS_WRITE_PROT)); }
        let old = context.path.clone();
        let new = vfs_path(new_file_name);

        // Refuse renames that would create or remove a shipped file. Only
        // user-group paths can be renamed; shipped paths are immutable.
        let src_is_user    = self.0.vfs.is_user_path(&old);
        let src_is_shipped = !src_is_user && self.0.vfs.lookup(&old).is_some();
        let dst_is_shipped = self.0.vfs.lookup(&new).is_some()
            && !self.0.vfs.is_user_path(&new);
        if src_is_shipped || dst_is_shipped {
            return Err(FspError::NTSTATUS(NTSTATUS_ACCESS));
        }

        // User-group rename: read on-disk bytes, drop the old PAMT entry,
        // write the new entry under `new`. The PAZ blob for the old entry
        // becomes orphaned (no compaction yet); same as add-then-remove.
        if src_is_user {
            let data = self.0.write_overlay.lock().unwrap().get(&old).cloned()
                .or_else(|| self.0.vfs.read_user_file(&old));
            let Some(data) = data else {
                return Err(FspError::NTSTATUS(NTSTATUS_NOT_FOUND));
            };
            if let Err(e) = self.0.vfs.create_user_file(&new, &data) {
                let msg = format!("rename {old} -> {new}: {e}");
                warn!("{msg}");
                self.0.push_event(format!("[err] {msg}"));
                return Err(FspError::NTSTATUS(NTSTATUS_IO_ERR));
            }
            if let Err(e) = self.0.vfs.remove_user_file(&old) {
                let msg = format!("rename cleanup of {old}: {e}");
                warn!("{msg}");
                self.0.push_event(format!("[err] {msg}"));
            }
            self.0.cache_put(ino_for(&old), Arc::from(data.clone()));
            self.0.cache_put(ino_for(&new), Arc::from(data.clone()));
            self.0.write_overlay.lock().unwrap().remove(&old);
            self.0.write_overlay.lock().unwrap().insert(new.clone(), data);
            self.0.pending_paths.lock().unwrap().remove(&old);
            self.0.write_mtimes.lock().unwrap().remove(&old);
            self.0.write_mtimes.lock().unwrap().insert(new, SystemTime::now());
            return Ok(());
        }

        // Pure-overlay rename (source isn't yet persisted to user_group):
        // just shift the in-memory state. The next flush lands at `new`.
        if let Some(data) = self.0.write_overlay.lock().unwrap().remove(&old) {
            self.0.write_overlay.lock().unwrap().insert(new.clone(), data);
        }
        self.0.pending_paths.lock().unwrap().remove(&old);
        self.0.pending_paths.lock().unwrap().insert(new.clone());
        if let Some(t) = self.0.write_mtimes.lock().unwrap().remove(&old) {
            self.0.write_mtimes.lock().unwrap().insert(new, t);
        }
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> Result<u32> {
        // Build the sorted entry list once per open handle.  Subsequent paginated
        // calls index into the cached Vec.  We deliberately do NOT use winfsp's
        // DirBuffer: for >50K entries it returns the marker entry itself once
        // the kernel reaches the end, which makes Windows loop forever instead
        // of seeing EOF.  Going through `append_to_buffer` + `finalize_buffer`
        // keeps us in full control of marker/EOF semantics.
        let entries = context.dir_entries.get_or_init(|| {
            self.0.build_dir_entries(&context.path)
        });

        // Find the first index strictly past the marker.  Marker is the last
        // filename returned in the previous chunk, so we resume right after it.
        let start = match marker.inner() {
            None => 0,
            Some(slice) => {
                let mut bytes = slice;
                while bytes.last() == Some(&0) { bytes = &bytes[..bytes.len() - 1]; }
                let needle = String::from_utf16_lossy(bytes);
                entries.partition_point(|e| e.name.as_str() <= needle.as_str())
            }
        };

        let mut cursor = 0u32;
        for entry in &entries[start..] {
            let mut di: DirInfo = DirInfo::new();
            if di.set_name(entry.name.as_str()).is_err() { continue; }
            *di.file_info_mut() = entry.file_info.clone();
            if !di.append_to_buffer(buffer, &mut cursor) {
                // Buffer full -- kernel will call back with the last name as marker.
                return Ok(cursor);
            }
        }
        // All remaining entries fit; write the EOF terminator.
        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> Result<()> {
        out.total_size = 1024u64 * 1024 * 1024 * 1024; // 1 TiB nominal
        out.free_size  = if self.0.readonly { 0 } else { 1024u64 * 1024 * 1024 };
        out.set_volume_label("CrimsonDesert");
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        _security_descriptor: Option<&mut [std::ffi::c_void]>,
    ) -> Result<u64> {
        Ok(0) // persistent_acls = false; no security descriptor needed
    }
}
