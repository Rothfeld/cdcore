//! Parallel FUSE filesystem -- `impl Filesystem for CdFs` where CdFs wraps Arc<SharedFs>.
//!
//! Concurrency model
//! -----------------
//! The FUSE session loop calls callbacks with `&mut CdFs` -- single-threaded.
//! Slow operations (cold dir build, full decode) are offloaded to rayon workers.
//! Reply objects are Send and are consumed on the worker thread.
//!
//! Key insight: workers must not write to any structure that the session thread
//! reads on the hot path.  This was the root of the freeze:
//!
//!   DashMap  -> crossbeam_epoch sched_yield (31% CPU wasted)
//!   RwLock   -> bulk writes from N workers starved the session thread's reads
//!
//! Solution: session thread owns a private `paths` HashMap (no lock at all).
//! Workers push new (ino, path, is_dir) tuples onto a Mutex<Vec> queue.
//! Session thread drains the queue into `paths` at the top of each callback --
//! a single Mutex acquire/release, then all reads are unsynchronised.
//!
//! Workers touch only:
//!   dir_cache   RwLock -- one brief write per dir (insert Arc<OnceLock>)
//!   in_flight   RwLock -- one brief write per cold decode
//!   decode_cache Mutex -- brief for cache probe/insert
//!   paz_maps     Mutex -- rare (one per PAZ file)
//!   path_queue   Mutex -- one push per child entry (append to Vec)
//!   vfs          internally Arc<RwLock<BTreeMap>>, read-only after load

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEntry, Request,
};
use libc::{ENOENT, EIO};
use log::{debug, info, warn};
use lru::LruCache;
use memmap2::Mmap;

use cdcore::{VfsManager, crypto, compression};
use cdcore::repack::{RepackEngine, ModifiedFile};
use crate::virtual_files;

const TTL:               Duration = Duration::from_secs(60);
const ROOT_INO:          u64     = 1;
const MAX_CACHE_ENTRIES: usize   = 131_072;
const MAX_CACHED_BYTES:  usize   = 512 * 1024 * 1024;
const SLOW_MS:           u128    = 200;  // log warning if a callback takes longer than this

// Returned for absent paths instead of reply.error(ENOENT).
// nodeid=0 tells the kernel to cache the "not found" result for TTL seconds
// (FUSE negative dentry caching), eliminating repeated lookups for the same
// absent name (e.g. .Trash, .sh_thumbnails) that otherwise saturate the
// session thread.
const ABSENT_ATTR: FileAttr = FileAttr {
    ino: 0, size: 0, blocks: 0,
    atime: UNIX_EPOCH, mtime: UNIX_EPOCH, ctime: UNIX_EPOCH, crtime: UNIX_EPOCH,
    kind: FileType::RegularFile,
    perm: 0, nlink: 0, uid: 0, gid: 0, rdev: 0, blksize: 0, flags: 0,
};


// -- Stub headers for MIME-type sniff reads ------------------------------------

/// 27-byte FBX binary magic so file managers identify the type without
/// triggering a full mesh parse + FBX conversion on every directory listing.
fn fbx_magic_stub() -> Vec<u8> {
    let mut v = Vec::with_capacity(27);
    v.extend_from_slice(b"Kaydara FBX Binary  \x00");
    v.extend_from_slice(b"\x1a\x00");
    v.extend_from_slice(&7400u32.to_le_bytes());
    v
}

/// Minimal OGG page-capture pattern for MIME sniff reads on .wem.ogg/ virtual files.
fn ogg_magic_stub() -> &'static [u8] {
    // OggS + stream_structure_version(0) + header_type(0x02=first page) + granule(0) + serial
    b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00"
}

// -- PNG header builder -------------------------------------------------------

fn build_png_header(width: u32, height: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(33);
    v.extend_from_slice(b"\x89PNG\r\n\x1a\n");          // 8-byte sig
    v.extend_from_slice(&13u32.to_be_bytes());           // IHDR data length
    let ihdr_start = v.len();
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);              // depth=8 type=6(RGBA)
    let crc = png_crc32(&v[ihdr_start..]);
    v.extend_from_slice(&crc.to_be_bytes());
    v
}

fn png_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

// -- Inode helpers -------------------------------------------------------------

// `[HH:MM:SS]` UTC for TUI event log lines.  Matches cdwinfs so the two side-by-side
// look uniform; UTC keeps things free of TZ deps and the label is short enough that
// local-vs-UTC ambiguity isn't a concern.
fn event_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = secs % 86_400;
    let h = day_secs / 3600;
    let m = (day_secs / 60) % 60;
    let s = day_secs % 60;
    format!("[{h:02}:{m:02}:{s:02}]")
}

/// True for any path whose first segment matches one of our synthetic root
/// directory names (`.dds.png`, `.paloc.json`, `_prefabs`, ...). Used to keep
/// editor temp files (Paint, etc.) from leaking into the user package group
/// under a virtual prefix.
fn is_under_virtual_root(path: &str) -> bool {
    let head = path.split('/').next().unwrap_or("");
    if head == crate::prefab_view::PREFAB_ROOT_NAME { return true; }
    virtual_files::virtual_root_dirs().any(|v| v == head)
}

// -- Snapshots -- archive the original bytes before any overwrite -------------

/// `<exe_parent>/snapshots`, or None if `current_exe()` can't be resolved.
fn snapshot_root() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("snapshots"))
}

/// `YYYY-MM-DDTHH-MM-SS.sssZ`. Hinnant's civil-from-days algorithm; no deps.
fn snapshot_stamp() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
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

fn ino_for(path: &str) -> u64 {
    if path.is_empty() { return ROOT_INO; }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish().wrapping_mul(0x9e3779b97f4a7c15).max(2)
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

// -- DirEntry ------------------------------------------------------------------

struct DirEntry {
    ino:      u64,
    attr:     FileAttr,
    name:     String,
    attr_ttl: Duration,
}

// -- SharedFs -- state accessed by BOTH session thread and rayon workers --------

pub struct SharedFs {
    vfs:           VfsManager,
    /// Lazy index of every `*.prefab` in the VFS. Powers the synthetic
    /// `/_prefabs/<stem>/...` subtree (manifest + asset pass-through).
    prefab_index:  crate::prefab_view::PrefabIndex,
    path_queue:    Mutex<Vec<Vec<(u64, Box<str>, bool)>>>,
    dir_cache:     RwLock<HashMap<u64, Arc<OnceLock<Vec<DirEntry>>>>>,
    decode_cache:  Mutex<LruCache<u64, Arc<[u8]>>>,
    cached_bytes:  AtomicUsize,
    in_flight:     Mutex<HashMap<u64, Arc<OnceLock<Option<Arc<[u8]>>>>>>,
    paz_maps:      Mutex<HashMap<String, Arc<Mmap>>>,
    write_overlay:  Mutex<HashMap<String, Vec<u8>>>,
    write_mtimes:   Mutex<HashMap<String, SystemTime>>,
    pending_paths:  Mutex<HashSet<String>>,
    repack_engine:  RepackEngine,
    papgt_path:     String,
    /// Dedicated thread pool for file decodes -- separate from the rayon global
    /// pool used by dir builds so decodes are never queued behind dir builds.
    /// Fixed size: avoids the 292K x pthread_create overhead of std::thread::spawn.
    decode_pool:   rayon::ThreadPool,
    uid:           u32,
    gid:           u32,
    readonly:      bool,
    auto_repack:   bool,
    recent_events: Mutex<VecDeque<String>>,
}

impl SharedFs {
    fn new_inner(vfs: VfsManager, readonly: bool, auto_repack: bool) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
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
                warn!("user-group bootstrap failed: {e}; create/unlink will return EROFS");
            }
            // Prune any user-group entries whose first segment collides with a
            // synthetic root name -- editor temp-file leakage from older builds.
            for p in vfs.user_group_paths() {
                if is_under_virtual_root(&p) {
                    warn!("user_group: pruning stale entry {p:?} (under virtual root)");
                    if let Err(e) = vfs.remove_user_file(&p) {
                        warn!("user_group: failed to prune {p:?}: {e}");
                    }
                }
            }
        }
        let decode_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
            .thread_name(|i| format!("cdfuse-decode-{i}"))
            .build()
            .expect("failed to build decode thread pool");
        SharedFs {
            vfs,
            prefab_index: crate::prefab_view::PrefabIndex::new(),
            path_queue:    Mutex::new(Vec::new()),
            dir_cache:     RwLock::new(HashMap::new()),
            decode_cache:  Mutex::new(LruCache::new(NonZeroUsize::new(MAX_CACHE_ENTRIES).unwrap())),
            cached_bytes:  AtomicUsize::new(0),
            in_flight:     Mutex::new(HashMap::new()),
            paz_maps:      Mutex::new(HashMap::new()),
            write_overlay:  Mutex::new(HashMap::new()),
            write_mtimes:   Mutex::new(HashMap::new()),
            pending_paths:  Mutex::new(HashSet::new()),
            repack_engine,
            papgt_path,
            decode_pool,
            uid,
            gid,
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
        std::mem::take(&mut *self.write_overlay.lock().unwrap());
        std::mem::take(&mut *self.pending_paths.lock().unwrap());
        std::mem::take(&mut *self.write_mtimes.lock().unwrap());
    }

    pub fn flush_all_pending(&self) {
        let pending = std::mem::take(&mut *self.pending_paths.lock().unwrap());
        let overlay = std::mem::take(&mut *self.write_overlay.lock().unwrap());
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

    // -- Attr builders ---------------------------------------------------------

    fn file_attr(&self, ino: u64, path: &str, size: u64) -> FileAttr {
        let mtime = self.write_mtimes.lock().unwrap()
            .get(path).copied()
            .unwrap_or(UNIX_EPOCH);
        FileAttr {
            ino, size, blocks: (size + 511) / 512,
            atime: mtime, mtime,
            ctime: mtime, crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: if self.readonly { 0o444 } else { 0o644 }, nlink: 1,
            uid: self.uid, gid: self.gid,
            rdev: 0, blksize: 4096, flags: 0,
        }
    }

    fn dir_attr(&self, ino: u64) -> FileAttr {
        FileAttr {
            ino, size: 0, blocks: 0,
            atime: UNIX_EPOCH, mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH, crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o555, nlink: 2,
            uid: self.uid, gid: self.gid,
            rdev: 0, blksize: 4096, flags: 0,
        }
    }

    fn child_path(parent: &str, name: &OsStr) -> String {
        let n = name.to_string_lossy();
        if parent.is_empty() {
            n.into_owned()
        } else {
            let mut s = String::with_capacity(parent.len() + 1 + n.len());
            s.push_str(parent);
            s.push('/');
            s.push_str(&n);
            s
        }
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

    // -- Decode cache ----------------------------------------------------------

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

    // -- Full decode (rayon worker) ---------------------------------------------

    fn decode(&self, ino: u64, path: &str) -> Option<Arc<[u8]>> {
        if let Some(d) = self.cache_get(ino) {
            return Some(d);
        }

        let slot = {
            let mut map = self.in_flight.lock().unwrap();
            Arc::clone(map.entry(ino).or_insert_with(|| Arc::new(OnceLock::new())))
        };

        let result = slot.get_or_init(|| {
            // Prefab-view synth: manifest.json (built from parsed prefab),
            // prefab.prefab and assets/* (delegated to underlying VFS path).
            use crate::prefab_view::{self as pv, PrefabPath};
            if let Some(pp) = pv::classify(path) {
                match pp {
                    PrefabPath::Manifest { stem } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        return Some(Arc::from(pv::synth_manifest(&self.vfs, &full)));
                    }
                    PrefabPath::PrefabFile { stem } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        let src_ino = ino_for(&full);
                        return self.decode(src_ino, &full);
                    }
                    PrefabPath::AssetsEntry { stem, relpath } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        // Could resolve to a file or to an intermediate dir;
                        // only files have decoded bytes. Dirs return None here
                        // (callers don't read directories via decode()).
                        let asset_path = pv::vfs_path_for_asset(&self.vfs, &full, relpath)?;
                        // Synth `.dds.png`: decode the source DDS then PNG-encode.
                        if pv::is_dds_png_relpath(&self.vfs, &full, relpath) {
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
                        return pv::synth_mesh_fbx(&self.vfs, &full).map(Arc::from);
                    }
                    PrefabPath::FbmEntry { stem, relpath } => {
                        let full = self.prefab_index.full_path_of(&self.vfs, stem)?;
                        let dds_path = pv::dds_path_for_fbm_png(&self.vfs, &full, relpath)?;
                        let src_ino  = ino_for(&dds_path);
                        let src_data = self.decode(src_ino, &dds_path)?;
                        let png      = virtual_files::render_dds_png(&src_data, &dds_path)?;
                        return Some(Arc::from(png));
                    }
                    _ => return None, // Root / BundleDir / AssetsDir / FbmDir are directories.
                }
            }

            if let Some(vf) = virtual_files::resolve(path) {
                let src_ino  = ino_for(&vf.source_path);
                let src_data = self.decode(src_ino, &vf.source_path)?;
                let bytes = match vf.kind {
                    virtual_files::VirtualKind::PalocJson => {
                        virtual_files::render_paloc(&src_data, &vf.source_path)?
                    }
                    virtual_files::VirtualKind::PabgbJson => {
                        let pabgh_path = vf.source_path.strip_suffix(".pabgb")
                            .map(|b| format!("{b}.pabgh"))?;
                        let pabgh_ino  = ino_for(&pabgh_path);
                        let pabgh_data = self.decode(pabgh_ino, &pabgh_path)?;
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

            let entry = self.vfs.lookup(path)?;
            let raw: Vec<u8> = if let Some(mmap) = self.get_mmap(&entry.paz_file) {
                let start = entry.offset as usize;
                if start >= mmap.len() {
                    warn!("decode {path}: offset {start} >= mmap len {} (paz: {})",
                          mmap.len(), &entry.paz_file);
                    return None;
                }
                let end = (start + entry.comp_size as usize).min(mmap.len());
                mmap[start..end].to_vec()
            } else {
                use std::io::{Read, Seek, SeekFrom};
                let mut f = match std::fs::File::open(&entry.paz_file) {
                    Ok(f)  => f,
                    Err(e) => { warn!("decode {path}: open {}: {e}", entry.paz_file); return None; }
                };
                if let Err(e) = f.seek(SeekFrom::Start(entry.offset)) {
                    warn!("decode {path}: seek to {}: {e}", entry.offset); return None;
                }
                let mut buf = vec![0u8; entry.comp_size as usize];
                if let Err(e) = f.read_exact(&mut buf) {
                    warn!("decode {path}: read {} bytes: {e}", entry.comp_size); return None;
                }
                buf
            };
            let mut data = raw;
            if entry.encrypted() {
                let bn = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
                crypto::decrypt_inplace(&mut data, bn);
            }
            if entry.compressed() && entry.compression_type() != 0 {
                data = compression::decompress(&data, entry.orig_size as usize, entry.compression_type()).ok()?;
            }
            Some(Arc::from(data))
        });

        self.in_flight.lock().unwrap().remove(&ino);

        if let Some(data) = result {
            self.cache_put(ino, Arc::clone(data));
            Some(Arc::clone(data))
        } else {
            None
        }
    }

    // -- Probe read (mmap slice) ------------------------------------------------
    // Fast path for reads at offset 0 that request fewer bytes than the full file.
    // Returns raw compressed bytes directly from the mmap -- callers that only want
    // magic bytes (e.g. `file(1)`, thumbnail generators) get a response without a
    // full decrypt+decompress cycle.  The bytes are NOT decrypted or decompressed,
    // which is intentional: the probe is only useful for compressed/encrypted blobs
    // where the caller accepts raw data.  Any caller that needs real content will
    // miss the probe conditions (offset != 0, size >= orig_size, cached, overlay
    // present) and fall through to the full decode path.

    fn probe(&self, ino: u64, offset: i64, size: u32, path: &str) -> Option<Vec<u8>> {
        if offset == 0 {
            if let Some(vf) = virtual_files::resolve(path) {
                let not_cached = self.cache_get(ino).is_none()
                    && !self.write_overlay.lock().unwrap().contains_key(path);
                if not_cached {
                    match vf.kind {
                        virtual_files::VirtualKind::DdsPng => {
                            let hdr = self.dds_png_stub_header(&vf.source_path);
                            let n   = (size as usize).min(hdr.len());
                            return Some(hdr[..n].to_vec());
                        }
                        virtual_files::VirtualKind::PamFbx
                        | virtual_files::VirtualKind::PamlodFbx
                        | virtual_files::VirtualKind::PacFbx => {
                            // Return just the FBX magic bytes so file managers
                            // can identify the MIME type without triggering a
                            // full mesh parse+FBX conversion for every file in
                            // the directory listing (same pattern as DdsPng stub).
                            let hdr = fbx_magic_stub();
                            let n   = (size as usize).min(hdr.len());
                            return Some(hdr[..n].to_vec());
                        }
                        virtual_files::VirtualKind::WemOgg => {
                            let hdr = ogg_magic_stub();
                            let n   = (size as usize).min(hdr.len());
                            return Some(hdr[..n].to_vec());
                        }
                        _ => {}
                    }
                }
            }
        }
        if virtual_files::resolve(path).is_some() { return None; }
        if self.write_overlay.lock().unwrap().contains_key(path) { return None; }
        if offset != 0 { return None; }
        if self.cache_get(ino).is_some() { return None; }
        let entry = self.vfs.lookup(path)?;
        if size >= entry.orig_size { return None; }
        let mmap  = self.get_mmap(&entry.paz_file)?;
        let start = entry.offset as usize;
        let raw   = (size as usize).min(entry.comp_size as usize);
        let end   = (start + raw).min(mmap.len());
        if start >= mmap.len() { return None; }
        Some(mmap[start..end].to_vec())
    }

    // -- Dir cache -------------------------------------------------------------

    fn dir_slot(&self, ino: u64) -> Arc<OnceLock<Vec<DirEntry>>> {
        if let Some(s) = self.dir_cache.read().unwrap().get(&ino) {
            return Arc::clone(s);
        }
        let s = Arc::new(OnceLock::new());
        self.dir_cache.write().unwrap().entry(ino).or_insert_with(|| Arc::clone(&s));
        s
    }

    fn build_dir_entries(&self, ino: u64, path: &str) -> Vec<DirEntry> {
        if let Some(vdir) = virtual_files::resolve_virtual_dir(path) {
            return self.build_virtual_dir_entries(ino, path, &vdir);
        }
        if crate::prefab_view::classify(path).is_some() {
            return self.build_prefab_dir_entries(ino, path);
        }

        let parent_ino = if ino == ROOT_INO { ROOT_INO } else { ino_for(parent_path(path)) };
        let mut entries = vec![
            DirEntry { ino, attr: self.dir_attr(ino), name: ".".into(), attr_ttl: TTL },
            DirEntry { ino: parent_ino, attr: self.dir_attr(parent_ino), name: "..".into(), attr_ttl: TTL },
        ];

        let children = self.vfs.list_dir_with_sizes_unsorted(path);
        let mut queue_batch: Vec<(u64, Box<str>, bool)> = Vec::with_capacity(
            children.len() + virtual_files::virtual_root_dirs().count() + 1
        );

        if path.is_empty() {
            for vdir_name in virtual_files::virtual_root_dirs() {
                if virtual_files::root_requires_vgmstream(vdir_name) {
                    continue;
                }
                let vino = ino_for(vdir_name);
                queue_batch.push((vino, Box::from(vdir_name), true));
                entries.push(DirEntry {
                    ino: vino, attr: self.dir_attr(vino),
                    name: vdir_name.to_string(), attr_ttl: TTL,
                });
            }
            // Surface the synthetic `_prefabs` root alongside the other virtual roots.
            let prefab_root = crate::prefab_view::PREFAB_ROOT_NAME;
            let pino = ino_for(prefab_root);
            queue_batch.push((pino, Box::from(prefab_root), true));
            entries.push(DirEntry {
                ino: pino, attr: self.dir_attr(pino),
                name: prefab_root.to_string(), attr_ttl: TTL,
            });
        }

        for (name, is_dir, orig_size) in &children {
            let child_path = Self::child_path(path, OsStr::new(name));
            let child_ino  = ino_for(&child_path);
            let attr = if *is_dir {
                self.dir_attr(child_ino)
            } else {
                self.file_attr(child_ino, &child_path, *orig_size as u64)
            };
            queue_batch.push((child_ino, child_path.clone().into(), *is_dir));
            entries.push(DirEntry { ino: child_ino, attr, name: name.clone(), attr_ttl: TTL });
        }

        self.path_queue.lock().unwrap().push(queue_batch);

        let n = entries.len().saturating_sub(2);
        info!("readdir {path:?} -> {n} entries");
        entries
    }

    fn build_prefab_dir_entries(&self, ino: u64, path: &str) -> Vec<DirEntry> {
        use crate::prefab_view::{self as pv, PrefabPath};
        let parent_ino = if ino == ROOT_INO { ROOT_INO } else { ino_for(parent_path(path)) };
        let mut entries = vec![
            DirEntry { ino, attr: self.dir_attr(ino), name: ".".into(), attr_ttl: TTL },
            DirEntry { ino: parent_ino, attr: self.dir_attr(parent_ino), name: "..".into(), attr_ttl: TTL },
        ];
        let mut queue_batch: Vec<(u64, Box<str>, bool)> = Vec::new();

        match pv::classify(path).expect("classified above") {
            PrefabPath::Root => {
                for stem in self.prefab_index.stems(&self.vfs) {
                    let cp = format!("{}/{stem}", pv::PREFAB_ROOT_NAME);
                    let cino = ino_for(&cp);
                    queue_batch.push((cino, cp.clone().into(), true));
                    entries.push(DirEntry {
                        ino: cino, attr: self.dir_attr(cino),
                        name: stem, attr_ttl: TTL,
                    });
                }
            }
            PrefabPath::BundleDir { stem } => {
                // manifest.json
                let mp = format!("{}/{stem}/{}", pv::PREFAB_ROOT_NAME, pv::MANIFEST_NAME);
                let mino = ino_for(&mp);
                queue_batch.push((mino, mp.clone().into(), false));
                let mlen = pv::synth_manifest(&self.vfs, &self.prefab_full_path(stem).unwrap_or_default()).len() as u64;
                entries.push(DirEntry {
                    ino: mino, attr: self.file_attr(mino, &mp, mlen),
                    name: pv::MANIFEST_NAME.to_string(), attr_ttl: Duration::ZERO,
                });
                // prefab.prefab (real bytes pass-through)
                if let Some(full) = self.prefab_full_path(stem) {
                    if let Some(e) = self.vfs.lookup(&full) {
                        let pp = format!("{}/{stem}/prefab.prefab", pv::PREFAB_ROOT_NAME);
                        let pino = ino_for(&pp);
                        queue_batch.push((pino, pp.clone().into(), false));
                        entries.push(DirEntry {
                            ino: pino, attr: self.file_attr(pino, &pp, e.orig_size as u64),
                            name: "prefab.prefab".to_string(), attr_ttl: Duration::ZERO,
                        });
                    }
                }
                // assets/ subdir
                let ap = format!("{}/{stem}/{}", pv::PREFAB_ROOT_NAME, pv::ASSETS_DIR_NAME);
                let aino = ino_for(&ap);
                queue_batch.push((aino, ap.clone().into(), true));
                entries.push(DirEntry {
                    ino: aino, attr: self.dir_attr(aino),
                    name: pv::ASSETS_DIR_NAME.to_string(), attr_ttl: TTL,
                });
                // mesh.fbx + mesh.fbm/ when the prefab references a decodable mesh.
                if let Some(full) = self.prefab_full_path(stem) {
                    if pv::primary_mesh_path(&self.vfs, &full).is_some() {
                        let fp = format!("{}/{stem}/{}", pv::PREFAB_ROOT_NAME, pv::MESH_FBX_NAME);
                        let fino = ino_for(&fp);
                        queue_batch.push((fino, fp.clone().into(), false));
                        entries.push(DirEntry {
                            ino: fino, attr: self.file_attr(fino, &fp, 0),
                            name: pv::MESH_FBX_NAME.to_string(), attr_ttl: Duration::ZERO,
                        });
                        let dp = format!("{}/{stem}/{}", pv::PREFAB_ROOT_NAME, pv::MESH_FBM_DIR);
                        let dino = ino_for(&dp);
                        queue_batch.push((dino, dp.clone().into(), true));
                        entries.push(DirEntry {
                            ino: dino, attr: self.dir_attr(dino),
                            name: pv::MESH_FBM_DIR.to_string(), attr_ttl: TTL,
                        });
                    }
                }
            }
            PrefabPath::FbmDir { stem } => {
                if let Some(full) = self.prefab_full_path(stem) {
                    self.push_assets_tree_children(
                        &mut entries, &mut queue_batch,
                        &pv::fbm_dir_children(&self.vfs, &full, ""),
                        stem, pv::MESH_FBM_DIR, "",
                        None,  // size unknown; surface 0
                    );
                }
            }
            PrefabPath::AssetsDir { stem } => {
                if let Some(full) = self.prefab_full_path(stem) {
                    self.push_assets_tree_children(
                        &mut entries, &mut queue_batch,
                        &pv::assets_dir_children(&self.vfs, &full, ""),
                        stem, pv::ASSETS_DIR_NAME, "",
                        Some(&self.vfs),
                    );
                }
            }
            // Intermediate subdirs under assets/ or mesh.fbm/ -- list direct
            // children only (one VFS-mirrored level deep at a time).
            PrefabPath::AssetsEntry { stem, relpath } => {
                if let Some(full) = self.prefab_full_path(stem) {
                    self.push_assets_tree_children(
                        &mut entries, &mut queue_batch,
                        &pv::assets_dir_children(&self.vfs, &full, relpath),
                        stem, pv::ASSETS_DIR_NAME, relpath,
                        Some(&self.vfs),
                    );
                }
            }
            PrefabPath::FbmEntry { stem, relpath } => {
                if let Some(full) = self.prefab_full_path(stem) {
                    self.push_assets_tree_children(
                        &mut entries, &mut queue_batch,
                        &pv::fbm_dir_children(&self.vfs, &full, relpath),
                        stem, pv::MESH_FBM_DIR, relpath,
                        None,
                    );
                }
            }
            // Files don't have entries themselves.
            PrefabPath::Manifest { .. }
            | PrefabPath::PrefabFile { .. }
            | PrefabPath::MeshFbx { .. } => {}
        }

        self.path_queue.lock().unwrap().push(queue_batch);
        let n = entries.len().saturating_sub(2);
        info!("readdir (prefab) {path:?} -> {n} entries");
        entries
    }

    /// Resolve a prefab stem to its full VFS path via the cached index.
    fn prefab_full_path(&self, stem: &str) -> Option<String> {
        self.prefab_index.full_path_of(&self.vfs, stem)
    }

    /// Append a single level of children (files + immediate subdirs) into
    /// the readdir buffer. `tree_root` is the synth root within the prefab
    /// (e.g. `assets` or `mesh.fbm`); `prefix` is the relpath of the current
    /// dir under that root (empty when listing the root itself).
    /// `size_lookup_vfs`: when Some, file sizes for non-DDS-PNG entries come
    /// from the underlying VFS entry. Otherwise sizes report 0 (file managers
    /// tolerate that and the data appears at decode time).
    #[allow(clippy::too_many_arguments)]
    fn push_assets_tree_children(
        &self,
        entries: &mut Vec<DirEntry>,
        queue_batch: &mut Vec<(u64, Box<str>, bool)>,
        children: &[crate::prefab_view::AssetsTreeChild],
        stem: &str,
        tree_root: &str,
        prefix: &str,
        size_lookup_vfs: Option<&cdcore::VfsManager>,
    ) {
        use crate::prefab_view as pv;
        let dir_base = if prefix.is_empty() {
            format!("{}/{stem}/{tree_root}", pv::PREFAB_ROOT_NAME)
        } else {
            format!("{}/{stem}/{tree_root}/{prefix}", pv::PREFAB_ROOT_NAME)
        };
        for child in children {
            match child {
                pv::AssetsTreeChild::Dir { name } => {
                    let cp = format!("{dir_base}/{name}");
                    let cino = ino_for(&cp);
                    queue_batch.push((cino, cp.clone().into(), true));
                    entries.push(DirEntry {
                        ino: cino, attr: self.dir_attr(cino),
                        name: name.clone(), attr_ttl: TTL,
                    });
                }
                pv::AssetsTreeChild::File { relpath, name, is_dds_png } => {
                    let cp = format!("{dir_base}/{name}");
                    let cino = ino_for(&cp);
                    let size = if *is_dds_png {
                        0
                    } else if let Some(vfs) = size_lookup_vfs {
                        vfs.lookup(relpath).map(|e| e.orig_size as u64).unwrap_or(0)
                    } else {
                        0
                    };
                    queue_batch.push((cino, cp.clone().into(), false));
                    entries.push(DirEntry {
                        ino: cino, attr: self.file_attr(cino, &cp, size),
                        name: name.clone(), attr_ttl: Duration::ZERO,
                    });
                }
            }
        }
    }

    fn build_virtual_dir_entries(&self, ino: u64, path: &str,
                                  vdir: &virtual_files::VirtualDirInfo) -> Vec<DirEntry> {
        let parent_ino = if ino == ROOT_INO { ROOT_INO } else { ino_for(parent_path(path)) };
        let mut entries = vec![
            DirEntry { ino, attr: self.dir_attr(ino), name: ".".into(), attr_ttl: TTL },
            DirEntry { ino: parent_ino, attr: self.dir_attr(parent_ino), name: "..".into(), attr_ttl: TTL },
        ];

        let children = self.vfs.list_dir_with_sizes_unsorted(&vdir.real_path);
        let mut queue_batch: Vec<(u64, Box<str>, bool)> = Vec::with_capacity(children.len());

        for (name, is_dir, orig_size) in &children {
            let child_vpath = Self::child_path(path, OsStr::new(name));
            let child_vino  = ino_for(&child_vpath);

            if *is_dir {
                let real_child = if vdir.real_path.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{name}", vdir.real_path)
                };
                if !self.vfs.subtree_has_ext(&real_child, vdir.filter_ext) {
                    continue;
                }
                queue_batch.push((child_vino, child_vpath.clone().into(), true));
                entries.push(DirEntry {
                    ino: child_vino, attr: self.dir_attr(child_vino),
                    name: name.clone(), attr_ttl: TTL,
                });
            } else if name.ends_with(vdir.filter_ext) {
                let should_add = if vdir.filter_ext == ".pabgb" {
                    name.strip_suffix(".pabgb").is_some_and(|base| {
                        let real_sibling = if vdir.real_path.is_empty() {
                            format!("{base}.pabgh")
                        } else {
                            format!("{}/{base}.pabgh", vdir.real_path)
                        };
                        self.vfs.lookup(&real_sibling).is_some()
                    })
                } else {
                    true
                };
                if should_add {
                    let virt_name = if vdir.suffix.is_empty() {
                        name.clone()
                    } else {
                        format!("{name}{}", vdir.suffix)
                    };
                    let vpath = Self::child_path(path, OsStr::new(&virt_name));
                    let vino  = ino_for(&vpath);
                    queue_batch.push((vino, vpath.clone().into(), false));
                    entries.push(DirEntry {
                        ino: vino, attr: self.file_attr(vino, &vpath, *orig_size as u64),
                        name: virt_name,
                        attr_ttl: Duration::ZERO,
                    });
                }
            }
        }

        self.path_queue.lock().unwrap().push(queue_batch);

        let n = entries.len().saturating_sub(2);
        info!("readdir (virtual) {path:?} -> {n} entries");
        entries
    }

    fn dds_png_stub_header(&self, dds_path: &str) -> Vec<u8> {
        let entry = match self.vfs.lookup(dds_path) {
            Some(e) => e,
            None    => return build_png_header(1, 1),
        };
        let mmap = match self.get_mmap(&entry.paz_file) {
            Some(m) => m,
            None    => return build_png_header(1, 1),
        };
        let start = entry.offset as usize;
        let end   = (start + 200).min(mmap.len());
        if start + 20 > mmap.len() { return build_png_header(1, 1); }

        let mut buf = mmap[start..end].to_vec();
        if entry.encrypted() {
            let bn = Path::new(dds_path).file_name()
                .and_then(|n| n.to_str()).unwrap_or(dds_path);
            crypto::decrypt_inplace(&mut buf, bn);
        }
        if buf.len() < 20 || &buf[..4] != b"DDS " {
            return build_png_header(1, 1);
        }
        let h = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        let w = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        build_png_header(w, h)
    }

    /// Drop the user's input bytes and the rendered output cache for a virtual
    /// path after its underlying source has been rewritten. Called once the
    /// recursive flush_path_sync(source) returns successfully -- subsequent
    /// reads then re-render from the new source instead of returning the
    /// pre-flush user input or the stale rendered cache.
    fn invalidate_virtual_path(&self, virtual_path: &str) {
        self.write_overlay.lock().unwrap().remove(virtual_path);
        self.decode_cache.lock().unwrap().pop(&ino_for(virtual_path));
    }

    fn flush_path_sync(&self, path: &str, data: Vec<u8>) {
        self.pending_paths.lock().unwrap().remove(path);
        // Keep `write_mtimes[path]` populated past flush -- without it
        // file_attr reverts to UNIX_EPOCH, and file-manager thumbnail caches
        // (keyed on mtime) never invalidate between saves.
        self.write_mtimes.lock().unwrap().insert(path.to_string(), SystemTime::now());

        // Prefab-view synth `mesh.fbx`: write FBX bytes to a temp file, import
        // back into a ParsedMesh, build new mesh bytes via the appropriate
        // builder for the underlying format, then recurse to flush the real path.
        if let Some(crate::prefab_view::PrefabPath::MeshFbx { stem }) = crate::prefab_view::classify(path) {
            let full = match self.prefab_index.full_path_of(&self.vfs, stem) {
                Some(s) => s,
                None    => { warn!("flush mesh.fbx {path}: prefab stem not found"); return; }
            };
            let mesh_path = match crate::prefab_view::primary_mesh_path(&self.vfs, &full) {
                Some(p) => p,
                None    => { warn!("flush mesh.fbx {path}: no resolvable mesh"); return; }
            };
            let tmp = std::env::temp_dir().join(format!("cdfuse_save_{}.fbx", std::process::id()));
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

            // Read original bytes from VFS to feed the builders.
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
                    self.invalidate_virtual_path(path);
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
                            info!("flush {path}: paloc JSONL -> {}B binary, repacking {}",
                                  binary.len(), vf.source_path);
                            self.flush_path_sync(&vf.source_path, binary);
                            self.invalidate_virtual_path(path);
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
                            info!("flush {path}: PNG -> {}B DDS, repacking {}",
                                  dds.len(), vf.source_path);
                            self.flush_path_sync(&vf.source_path, dds);
                            self.invalidate_virtual_path(path);
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
            // Either an existing user file (replace) or a brand-new path the
            // current `create()` registered without writing yet (the lookup
            // miss tells us the file isn't backed by any shipped PAMT).
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
            None    => return,
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
            None => { warn!("flush_sync {path}: no pamt for group {group_dir}"); return; }
        };
        self.cache_put(ino_for(path), Arc::from(data.clone()));
        let mf = ModifiedFile { data, entry: entry.clone(), pamt_data, package_group: group_dir.clone() };
        match self.repack_engine.repack(vec![mf], &self.papgt_path) {
            Ok(r) if r.success => {
                info!("repack {path}: ok");
                self.push_event(format!("[ok]  repacked {path}"));
                self.paz_maps.lock().unwrap().remove(&entry.paz_file);
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
}

// -- CdFs -- session-thread-owned wrapper --------------------------------------

pub struct CdFs {
    shared: Arc<SharedFs>,
    paths: HashMap<u64, (Box<str>, bool)>,
}

impl CdFs {
    pub fn shared(&self) -> Arc<SharedFs> { Arc::clone(&self.shared) }

    pub fn new(vfs: VfsManager, readonly: bool, auto_repack: bool) -> Self {
        let shared = Arc::new(SharedFs::new_inner(vfs, readonly, auto_repack));
        let mut paths = HashMap::new();
        paths.insert(ROOT_INO, (Box::from(""), true));
        CdFs { shared, paths }
    }

    fn drain(&mut self) {
        let batches = std::mem::take(&mut *self.shared.path_queue.lock().unwrap());
        if !batches.is_empty() {
            let total: usize = batches.iter().map(|b| b.len()).sum();
            let t = Instant::now();
            for batch in batches {
                for (ino, path, is_dir) in batch {
                    self.paths.entry(ino).or_insert((path, is_dir));
                }
            }
            let ms = t.elapsed().as_millis();
            if ms >= SLOW_MS {
                warn!("SLOW drain: {} inodes took {}ms", total, ms);
            } else {
                debug!("drain: {} inodes in {}ms", total, ms);
            }
        }
    }

    fn path_of(&self, ino: u64) -> Option<&str> {
        self.paths.get(&ino).map(|(p, _)| p.as_ref())
    }

    fn is_dir(&self, ino: u64) -> bool {
        self.paths.get(&ino).map(|(_, d)| *d).unwrap_or(false)
    }

    fn ensure_path(&mut self, path: &str, is_dir: bool) -> u64 {
        let ino = ino_for(path);
        self.paths.entry(ino).or_insert_with(|| (path.into(), is_dir));
        ino
    }
}

// -- Filesystem impl -----------------------------------------------------------

impl Filesystem for CdFs {
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        let _ = config.add_capabilities(fuser::consts::FUSE_DO_READDIRPLUS);
        info!("filesystem mounted (readdirplus enabled)");
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let _t = Instant::now();
        debug!(">> lookup parent={parent} name={name:?}");
        self.drain();
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let child = SharedFs::child_path(&parent_path, name);

        // Prefab-view synth tree handled before the real-VFS check so paths
        // under `_prefabs/` resolve even though they don't exist in the game files.
        if let Some(pp) = crate::prefab_view::classify(&child) {
            use crate::prefab_view::PrefabPath;
            // Resolve (valid, is_dir) by inspecting the bundle. AssetsEntry /
            // FbmEntry can be either a real file or an intermediate subdir;
            // they pivot on whether the relpath matches an asset entry exactly
            // (file) or appears as a prefix (dir).
            let (valid, is_dir) = match &pp {
                PrefabPath::Root => (true, true),
                PrefabPath::BundleDir { stem }
                | PrefabPath::Manifest { stem }
                | PrefabPath::PrefabFile { stem }
                | PrefabPath::AssetsDir { stem } => {
                    let exists = self.shared.prefab_index.full_path_of(&self.shared.vfs, stem).is_some();
                    let is_dir = matches!(pp, PrefabPath::BundleDir { .. } | PrefabPath::AssetsDir { .. });
                    (exists, is_dir)
                }
                PrefabPath::AssetsEntry { stem, relpath } => {
                    if let Some(full) = self.shared.prefab_index.full_path_of(&self.shared.vfs, stem) {
                        if crate::prefab_view::vfs_path_for_asset(&self.shared.vfs, &full, relpath).is_some() {
                            (true, false)
                        } else if crate::prefab_view::is_assets_subdir(&self.shared.vfs, &full, relpath) {
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
                    let has_mesh = self.shared.prefab_index.full_path_of(&self.shared.vfs, stem)
                        .and_then(|full| crate::prefab_view::primary_mesh_path(&self.shared.vfs, &full))
                        .is_some();
                    let is_dir = matches!(pp, PrefabPath::FbmDir { .. });
                    (has_mesh, is_dir)
                }
                PrefabPath::FbmEntry { stem, relpath } => {
                    if let Some(full) = self.shared.prefab_index.full_path_of(&self.shared.vfs, stem) {
                        if crate::prefab_view::dds_path_for_fbm_png(&self.shared.vfs, &full, relpath).is_some() {
                            (true, false)
                        } else if crate::prefab_view::is_fbm_subdir(&self.shared.vfs, &full, relpath) {
                            (true, true)
                        } else {
                            (false, false)
                        }
                    } else {
                        (false, false)
                    }
                }
            };
            if !valid {
                reply.entry(&TTL, &ABSENT_ATTR, 0);
                return;
            }
            let ino = self.ensure_path(&child, is_dir);
            let attr = if is_dir {
                self.shared.dir_attr(ino)
            } else {
                // Size of synth manifest is its content len; pass-through files
                // borrow the underlying VFS entry's orig_size.
                let size = match &pp {
                    PrefabPath::Manifest { stem } => self.shared.prefab_index
                        .full_path_of(&self.shared.vfs, stem)
                        .map(|full| crate::prefab_view::synth_manifest(&self.shared.vfs, &full).len() as u64)
                        .unwrap_or(0),
                    PrefabPath::PrefabFile { stem } => self.shared.prefab_index
                        .full_path_of(&self.shared.vfs, stem)
                        .and_then(|full| self.shared.vfs.lookup(&full).map(|e| e.orig_size as u64))
                        .unwrap_or(0),
                    PrefabPath::AssetsEntry { stem, relpath } => self.shared.prefab_index
                        .full_path_of(&self.shared.vfs, stem)
                        .and_then(|full| crate::prefab_view::vfs_path_for_asset(&self.shared.vfs, &full, relpath))
                        .and_then(|p| self.shared.vfs.lookup(&p).map(|e| e.orig_size as u64))
                        .unwrap_or(0),
                    _ => 0,
                };
                self.shared.file_attr(ino, &child, size)
            };
            reply.entry(&Duration::ZERO, &attr, 0);
            return;
        }

        if let Some(entry) = self.shared.vfs.lookup(&child) {
            let ino  = self.ensure_path(&child, false);
            let attr = self.shared.file_attr(ino, &child, entry.orig_size as u64);
            reply.entry(&TTL, &attr, 0);
            return;
        }
        if self.shared.vfs.dir_exists(&child)
            || self.paths.get(&ino_for(&child)).is_some_and(|(_, d)| *d)
        {
            let ino  = self.ensure_path(&child, true);
            let attr = self.shared.dir_attr(ino);
            reply.entry(&TTL, &attr, 0);
            return;
        }
        if let Some(vf) = virtual_files::resolve(&child) {
            if self.shared.vfs.lookup(&vf.source_path).is_some() {
                let ino  = self.ensure_path(&child, false);
                let size = self.shared.cache_get(ino)
                    .map(|d| d.len() as u64)
                    .or_else(|| self.shared.vfs.lookup(&vf.source_path).map(|e| e.orig_size as u64))
                    .unwrap_or(0);
                let attr = self.shared.file_attr(ino, &child, size);
                reply.entry(&Duration::ZERO, &attr, 0);
                return;
            }
        }
        if let Some(vdir) = virtual_files::resolve_virtual_dir(&child) {
            let real_exists = vdir.real_path.is_empty()
                || self.shared.vfs.subtree_has_ext(&vdir.real_path, vdir.filter_ext);
            if real_exists {
                let ino  = self.ensure_path(&child, true);
                let attr = self.shared.dir_attr(ino);
                reply.entry(&TTL, &attr, 0);
                return;
            }
        }
        // Last-chance: a path registered via `create()` that hasn't flushed
        // yet (no PAMT entry, just an overlay). Reject when the overlay is
        // also absent so renamed-away paths don't ghost-resolve.
        if self.paths.get(&ino_for(&child)).is_some_and(|(_, d)| !*d) {
            let has_overlay = self.shared.write_overlay.lock().unwrap().contains_key(&child);
            if has_overlay {
                let ino  = self.ensure_path(&child, false);
                let size = self.shared.write_overlay.lock().unwrap()
                    .get(&child).map(|d| d.len() as u64).unwrap_or(0);
                let attr = self.shared.file_attr(ino, &child, size);
                reply.entry(&Duration::ZERO, &attr, 0);
                return;
            }
        }
        reply.entry(&TTL, &ABSENT_ATTR, 0);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        debug!(">> getattr ino={ino}");
        self.drain();
        if self.is_dir(ino) {
            reply.attr(&TTL, &self.shared.dir_attr(ino));
            return;
        }
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        // Prefab-view synth files: size from cache if present, else compute on the fly.
        if let Some(pp) = crate::prefab_view::classify(&path) {
            use crate::prefab_view::PrefabPath;
            let size = match self.shared.cache_get(ino) {
                Some(d) => d.len() as u64,
                None    => match &pp {
                    PrefabPath::Manifest { stem } => self.shared.prefab_index
                        .full_path_of(&self.shared.vfs, stem)
                        .map(|full| crate::prefab_view::synth_manifest(&self.shared.vfs, &full).len() as u64)
                        .unwrap_or(0),
                    PrefabPath::PrefabFile { stem } => self.shared.prefab_index
                        .full_path_of(&self.shared.vfs, stem)
                        .and_then(|full| self.shared.vfs.lookup(&full).map(|e| e.orig_size as u64))
                        .unwrap_or(0),
                    PrefabPath::AssetsEntry { stem, relpath } => self.shared.prefab_index
                        .full_path_of(&self.shared.vfs, stem)
                        .and_then(|full| crate::prefab_view::vfs_path_for_asset(&self.shared.vfs, &full, relpath))
                        .and_then(|p| self.shared.vfs.lookup(&p).map(|e| e.orig_size as u64))
                        .unwrap_or(0),
                    _ => 0,
                },
            };
            reply.attr(&Duration::ZERO, &self.shared.file_attr(ino, &path, size));
            return;
        }
        if let Some(e) = self.shared.vfs.lookup(&path) {
            let size = self.shared.write_overlay.lock().unwrap()
                .get(&path).map(|d| d.len() as u64)
                .unwrap_or(e.orig_size as u64);
            reply.attr(&TTL, &self.shared.file_attr(ino, &path, size));
        } else if let Some(vf) = virtual_files::resolve(&path) {
            let (size, ttl) = match self.shared.cache_get(ino) {
                Some(d) => (d.len() as u64, TTL),
                None    => {
                    let est = self.shared.vfs.lookup(&vf.source_path)
                        .map(|e| e.orig_size as u64)
                        .unwrap_or(4096);
                    (est, Duration::ZERO)
                }
            };
            reply.attr(&ttl, &self.shared.file_attr(ino, &path, size));
        } else if self.paths.get(&ino).is_some_and(|(_, d)| !*d) {
            let size = self.shared.write_overlay.lock().unwrap()
                .get(&path).map(|d| d.len() as u64).unwrap_or(0);
            reply.attr(&Duration::ZERO, &self.shared.file_attr(ino, &path, size));
        } else {
            reply.error(ENOENT);
        }
    }

    fn setattr(&mut self, _req: &Request<'_>, ino: u64, _mode: Option<u32>,
               _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>,
               _atime: Option<fuser::TimeOrNow>, _mtime: Option<fuser::TimeOrNow>,
               _ctime: Option<std::time::SystemTime>, _fh: Option<u64>,
               _crtime: Option<std::time::SystemTime>, _chgtime: Option<std::time::SystemTime>,
               _bkuptime: Option<std::time::SystemTime>, _flags: Option<u32>,
               reply: ReplyAttr) {
        self.drain();
        if self.is_dir(ino) { reply.error(libc::EISDIR); return; }
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        if let Some(new_size) = size {
            let needs_seed = !self.shared.write_overlay.lock().unwrap().contains_key(&path);
            if needs_seed {
                let seed = self.shared.cache_get(ino)
                    .map(|d| d.to_vec())
                    .unwrap_or_else(|| {
                        self.shared.decode(ino, &path)
                            .map(|d| d.to_vec())
                            .unwrap_or_default()
                    });
                self.shared.write_overlay.lock().unwrap().entry(path.clone()).or_insert(seed);
            }
            {
                let mut overlay = self.shared.write_overlay.lock().unwrap();
                let buf = overlay.get_mut(&path).unwrap();
                buf.resize(new_size as usize, 0);
            }
            self.shared.write_mtimes.lock().unwrap().insert(path.clone(), SystemTime::now());
            let attr = self.shared.file_attr(ino, &path, new_size);
            reply.attr(&Duration::ZERO, &attr);
        } else {
            let size = self.shared.write_overlay.lock().unwrap()
                .get(&path).map(|d| d.len() as u64)
                .or_else(|| self.shared.vfs.lookup(&path).map(|e| e.orig_size as u64))
                .unwrap_or(0);
            reply.attr(&TTL, &self.shared.file_attr(ino, &path, size));
        }
    }

    fn write(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64,
             data: &[u8], _write_flags: u32, _flags: i32, _lock_owner: Option<u64>,
             reply: fuser::ReplyWrite) {
        self.drain();
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        if let Some(vf) = virtual_files::resolve(&path) {
            match vf.kind {
                virtual_files::VirtualKind::PalocJson => {}
                virtual_files::VirtualKind::DdsPng    => {}
                _ => { reply.error(libc::EROFS); return; }
            }
        }
        let known = self.shared.vfs.lookup(&path).is_some()
            || self.paths.get(&ino).is_some_and(|(_, d)| !*d);
        if !known {
            reply.error(ENOENT);
            return;
        }
        let needs_seed = !self.shared.write_overlay.lock().unwrap().contains_key(&path);
        if needs_seed {
            let seed = self.shared.cache_get(ino)
                .map(|d| d.to_vec())
                .unwrap_or_else(|| {
                    self.shared.decode(ino, &path)
                        .map(|d| d.to_vec())
                        .unwrap_or_default()
                });
            self.shared.write_overlay.lock().unwrap().entry(path.clone()).or_insert(seed);
        }
        self.shared.pending_paths.lock().unwrap().insert(path.clone());
        self.shared.write_mtimes.lock().unwrap().insert(path.clone(), SystemTime::now());
        let offset = offset as usize;
        let mut overlay = self.shared.write_overlay.lock().unwrap();
        let buf = overlay.get_mut(&path).unwrap();
        let end = offset + data.len();
        if end > buf.len() { buf.resize(end, 0); }
        buf[offset..end].copy_from_slice(data);
        reply.written(data.len() as u32);
    }

    fn create(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr,
              _mode: u32, _umask: u32, _flags: i32, reply: fuser::ReplyCreate) {
        self.drain();
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let child = SharedFs::child_path(&parent_path, name);
        let ino = self.ensure_path(&child, false);
        let now = SystemTime::now();
        self.shared.write_overlay.lock().unwrap().entry(child.clone()).or_insert_with(Vec::new);
        self.shared.pending_paths.lock().unwrap().insert(child.clone());
        self.shared.write_mtimes.lock().unwrap().insert(child.clone(), now);
        // Invalidate parent dir cache so the new file shows up in the next readdir.
        self.shared.dir_cache.write().unwrap().remove(&ino_for(&parent_path));
        let attr = self.shared.file_attr(ino, &child, 0);
        reply.created(&Duration::ZERO, &attr, 0, 0, fuser::consts::FOPEN_DIRECT_IO);
    }

    fn rename(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr,
              newparent: u64, newname: &OsStr, _flags: u32, reply: fuser::ReplyEmpty) {
        self.drain();
        let src_parent = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let dst_parent = match self.path_of(newparent) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let src = SharedFs::child_path(&src_parent, name);
        let dst = SharedFs::child_path(&dst_parent, newname);

        // Refuse renames that would create or remove a shipped file. Only
        // user-group paths can be renamed; shipped paths are immutable.
        let src_is_user = self.shared.vfs.is_user_path(&src);
        let src_is_shipped = !src_is_user && self.shared.vfs.lookup(&src).is_some();
        let dst_is_shipped = self.shared.vfs.lookup(&dst).is_some()
            && !self.shared.vfs.is_user_path(&dst);
        if src_is_shipped || dst_is_shipped {
            reply.error(libc::EACCES);
            return;
        }

        let dst_ino = self.ensure_path(&dst, false);

        // User-group rename: read the on-disk bytes, drop the old PAMT entry,
        // write the new entry under `dst`. The PAZ blob for the old entry
        // becomes orphaned (no compaction yet); same as add-then-remove.
        if src_is_user {
            // Prefer freshest data: pending overlay > user_group on-disk.
            let data = self.shared.write_overlay.lock().unwrap().get(&src).cloned()
                .or_else(|| self.shared.vfs.read_user_file(&src));
            let Some(data) = data else {
                // No data anywhere -- treat as ENOENT.
                reply.error(ENOENT);
                return;
            };
            if let Err(e) = self.shared.vfs.create_user_file(&dst, &data) {
                let msg = format!("rename {src} -> {dst}: {e}");
                warn!("{msg}");
                self.shared.push_event(format!("[err] {msg}"));
                reply.error(libc::EIO);
                return;
            }
            // Remove the old entry only after the new one has been persisted.
            if let Err(e) = self.shared.vfs.remove_user_file(&src) {
                let msg = format!("rename cleanup of {src}: {e}");
                warn!("{msg}");
                self.shared.push_event(format!("[err] {msg}"));
                // Don't fail the rename: dst exists, src is stale; the next
                // remount will see whichever is in PAMT.
            }
            // The kernel keeps the source's inode after a successful rename
            // and uses it to read the new name. Repoint the src ino at dst,
            // and mirror the same mapping under dst's hash for fresh
            // lookups. Cache the data under both so reads in either form
            // hit fast.
            let src_ino = ino_for(&src);
            self.paths.insert(src_ino, (Box::from(dst.as_str()), false));
            self.paths.insert(dst_ino, (Box::from(dst.as_str()), false));
            self.shared.cache_put(src_ino, Arc::from(data.clone()));
            self.shared.cache_put(dst_ino, Arc::from(data.clone()));
            self.shared.write_overlay.lock().unwrap().remove(&src);
            self.shared.write_overlay.lock().unwrap().insert(dst.clone(), data);
            self.shared.pending_paths.lock().unwrap().remove(&src);
            self.shared.write_mtimes.lock().unwrap().remove(&src);
            self.shared.write_mtimes.lock().unwrap().insert(dst.clone(), SystemTime::now());
            self.shared.dir_cache.write().unwrap().remove(&ino_for(&src_parent));
            self.shared.dir_cache.write().unwrap().remove(&ino_for(&dst_parent));
            reply.ok();
            return;
        }

        // Pure-overlay rename (source isn't yet persisted to user_group):
        // just shift the in-memory state. The next flush will land at `dst`.
        let src_ino = ino_for(&src);
        let moved = self.shared.write_overlay.lock().unwrap().remove(&src);
        if let Some(data) = moved {
            self.paths.insert(src_ino, (Box::from(dst.as_str()), false));
            self.paths.insert(dst_ino, (Box::from(dst.as_str()), false));
            self.shared.cache_put(src_ino, Arc::from(data.clone()));
            self.shared.cache_put(dst_ino, Arc::from(data.clone()));
            self.shared.write_overlay.lock().unwrap().insert(dst.clone(), data);
        }
        self.shared.pending_paths.lock().unwrap().remove(&src);
        self.shared.pending_paths.lock().unwrap().insert(dst.clone());
        self.shared.dir_cache.write().unwrap().remove(&ino_for(&src_parent));
        self.shared.dir_cache.write().unwrap().remove(&ino_for(&dst_parent));
        reply.ok();
    }

    fn release(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, _flags: i32,
               _lock_owner: Option<u64>, _flush: bool, reply: fuser::ReplyEmpty) {
        self.drain();
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => { reply.ok(); return; }
        };
        let pending = self.shared.pending_paths.lock().unwrap().contains(&path);
        if pending && self.shared.auto_repack {
            if let Some(data) = self.shared.write_overlay.lock().unwrap().get(&path).cloned() {
                let shared = Arc::clone(&self.shared);
                self.shared.decode_pool.spawn(move || {
                    shared.flush_path_sync(&path, data);
                });
                reply.ok();
                return;
            }
        }
        if let Some(data) = self.shared.write_overlay.lock().unwrap().get(&path) {
            self.shared.cache_put(ino, Arc::from(data.as_slice()));
        }
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request<'_>, _ino: u64, _fh: u64, _datasync: bool,
             reply: fuser::ReplyEmpty) {
        reply.ok();
    }

    fn destroy(&mut self) {
        let pending = std::mem::take(&mut *self.shared.pending_paths.lock().unwrap());
        let overlay = std::mem::take(&mut *self.shared.write_overlay.lock().unwrap());
        if pending.is_empty() { return; }
        warn!("destroy: flushing {} pending write(s) to PAZ", pending.len());
        for path in &pending {
            if let Some(data) = overlay.get(path).cloned() {
                self.shared.flush_path_sync(path, data);
            }
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        self.drain();
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let child = SharedFs::child_path(&parent_path, name);
        let ino = ino_for(&child);

        if virtual_files::resolve(&child).is_some() {
            self.shared.write_overlay.lock().unwrap().remove(&child);
            self.shared.write_mtimes.lock().unwrap().remove(&child);
            self.shared.pending_paths.lock().unwrap().remove(&child);
            self.shared.dir_cache.write().unwrap().remove(&ino_for(&parent_path));
            reply.ok();
            return;
        }
        // Delete is only permitted for files in the user package group.
        // Shipped paths come back EACCES so users can't accidentally remove
        // game data by typoing rm.
        let is_user = self.shared.vfs.is_user_path(&child);
        if !is_user {
            if self.shared.vfs.lookup(&child).is_some() {
                reply.error(libc::EACCES);
                return;
            }
            // Path isn't shipped, isn't user-owned. If it's just a registered
            // pending-write file with no on-disk backing, drop the overlay.
            if self.paths.get(&ino).is_some_and(|(_, d)| !*d) {
                self.shared.write_overlay.lock().unwrap().remove(&child);
                self.shared.write_mtimes.lock().unwrap().remove(&child);
                self.shared.pending_paths.lock().unwrap().remove(&child);
                self.shared.dir_cache.write().unwrap().remove(&ino_for(&parent_path));
                reply.ok();
                return;
            }
            reply.error(ENOENT);
            return;
        }
        if let Err(e) = self.shared.vfs.remove_user_file(&child) {
            let msg = format!("user_group unlink {child}: {e}");
            warn!("{msg}");
            self.shared.push_event(format!("[err] {msg}"));
            reply.error(libc::EIO);
            return;
        }
        self.shared.decode_cache.lock().unwrap().pop(&ino);
        self.shared.write_overlay.lock().unwrap().remove(&child);
        self.shared.write_mtimes.lock().unwrap().remove(&child);
        self.shared.pending_paths.lock().unwrap().remove(&child);
        self.shared.dir_cache.write().unwrap().remove(&ino_for(&parent_path));
        reply.ok();
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr,
             _mode: u32, _umask: u32, reply: ReplyEntry) {
        self.drain();
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let child = SharedFs::child_path(&parent_path, name);
        // Refuse if a shipped/user path or another synth-dir already lives there.
        if self.shared.vfs.lookup(&child).is_some() || self.shared.vfs.dir_exists(&child) {
            reply.error(libc::EEXIST);
            return;
        }
        self.shared.vfs.add_synth_dir(&child);
        let ino = self.ensure_path(&child, true);
        self.shared.dir_cache.write().unwrap().remove(&ino_for(&parent_path));
        let attr = self.shared.dir_attr(ino);
        reply.entry(&Duration::ZERO, &attr, 0);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        self.drain();
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let child = SharedFs::child_path(&parent_path, name);
        // Reject for any directory still backed by real entries -- only
        // synthetic empty user dirs are removable.
        let prefix = format!("{}/", child);
        let has_children = self.shared.vfs.list_dir(&child).iter().any(|_| true)
            || self.shared.write_overlay.lock().unwrap().keys().any(|p| p.starts_with(&prefix));
        if has_children {
            reply.error(libc::ENOTEMPTY);
            return;
        }
        if !self.shared.vfs.remove_synth_dir(&child) {
            reply.error(libc::EACCES);
            return;
        }
        self.shared.dir_cache.write().unwrap().remove(&ino_for(&parent_path));
        reply.ok();
    }

    fn readdirplus(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64,
                   reply: ReplyDirectoryPlus) {
        self.drain();
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let slot = self.shared.dir_slot(ino);
        if slot.get().is_some() {
            serve_readdirplus(slot.get().unwrap(), offset, reply);
            return;
        }
        let shared = Arc::clone(&self.shared);
        rayon::spawn(move || {
            slot.get_or_init(|| shared.build_dir_entries(ino, &path));
            serve_readdirplus(slot.get().unwrap(), offset, reply);
        });
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64,
               mut reply: ReplyDirectory) {
        self.drain();
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let slot = self.shared.dir_slot(ino);
        slot.get_or_init(|| self.shared.build_dir_entries(ino, &path));
        let entries = slot.get().unwrap();
        for (i, e) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(e.ino, (i + 1) as i64, e.attr.kind, &e.name) { break; }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        self.drain();
        if self.is_dir(ino) {
            reply.error(libc::EISDIR);
            return;
        }
        let is_write = (_flags & libc::O_WRONLY != 0) || (_flags & libc::O_RDWR != 0);
        if is_write {
            if let Some(path) = self.path_of(ino) {
                if virtual_files::resolve(path).is_some() && self.shared.cache_get(ino).is_none() {
                    let path   = path.to_string();
                    let shared = Arc::clone(&self.shared);
                    let pool   = &shared.decode_pool as *const rayon::ThreadPool;
                    let pool   = unsafe { &*pool };
                    pool.spawn(move || {
                        shared.decode(ino, &path);
                        reply.opened(0, fuser::consts::FOPEN_DIRECT_IO);
                    });
                    return;
                }
            }
        }
        reply.opened(0, fuser::consts::FOPEN_DIRECT_IO);
    }

    fn read(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64,
            size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        self.drain();
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        {
            let overlay = self.shared.write_overlay.lock().unwrap();
            if let Some(data) = overlay.get(&path) {
                let s = (offset as usize).min(data.len());
                let e = (s + size as usize).min(data.len());
                reply.data(&data[s..e]);
                return;
            }
        }
        if let Some(raw) = self.shared.probe(ino, offset, size, &path) {
            reply.data(&raw);
            return;
        }
        if let Some(data) = self.shared.cache_get(ino) {
            let s = (offset as usize).min(data.len());
            let e = (s + size as usize).min(data.len());
            reply.data(&data[s..e]);
            return;
        }
        let shared = Arc::clone(&self.shared);
        let pool = &shared.decode_pool as *const rayon::ThreadPool;
        let pool = unsafe { &*pool };
        pool.spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                shared.decode(ino, &path)
            }));
            match result {
                Ok(Some(data)) => {
                    let s = (offset as usize).min(data.len());
                    let e = (s + size as usize).min(data.len());
                    reply.data(&data[s..e]);
                }
                Ok(None) => { reply.error(EIO); }
                Err(e) => {
                    let msg = e.downcast_ref::<&str>().copied()
                        .or_else(|| e.downcast_ref::<String>().map(|s| s.as_str()))
                        .unwrap_or("unknown panic");
                    warn!("read {path:?} -> decode panicked: {msg}");
                    reply.error(EIO);
                }
            }
        });
    }
}

fn serve_readdirplus(entries: &[DirEntry], offset: i64, mut reply: ReplyDirectoryPlus) {
    for (i, e) in entries.iter().enumerate().skip(offset as usize) {
        if reply.add(e.ino, (i + 1) as i64, &e.name, &e.attr_ttl, &e.attr, 0) { break; }
    }
    reply.ok();
}
