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
    DirBuffer, DirInfo, DirMarker, FileSecurity, FileInfo,
    FileSystemContext, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, Result, U16CStr};

use cdcore::{VfsManager, crypto, compression};
use cdcore::repack::{RepackEngine, ModifiedFile};
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

// NTSTATUS codes (raw i32 — avoids pulling windows crate into this file)
const NTSTATUS_NOT_FOUND:  i32 = 0xC0000034u32 as i32; // STATUS_OBJECT_NAME_NOT_FOUND
const NTSTATUS_WRITE_PROT: i32 = 0xC00000A2u32 as i32; // STATUS_MEDIA_WRITE_PROTECTED
const NTSTATUS_INSUF_RES:  i32 = 0xC000009Au32 as i32; // STATUS_INSUFFICIENT_RESOURCES
const NTSTATUS_IO_ERR:     i32 = 0xC0000185u32 as i32; // STATUS_IO_DEVICE_ERROR

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

// Convert SystemTime to Windows FILETIME (100-ns intervals since 1601-01-01).
fn to_filetime(t: SystemTime) -> u64 {
    const OFFSET: u64 = 116_444_736_000_000_000;
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d)  => OFFSET + d.as_nanos() as u64 / 100,
        Err(_) => 0,
    }
}

// ---- per-open-handle context -------------------------------------------------

pub struct FileCtx {
    pub path:       String,
    pub is_dir:     bool,
    delete_pending: AtomicBool,
    pub dir_buffer: DirBuffer,
}

impl FileCtx {
    fn new(path: String, is_dir: bool) -> Self {
        FileCtx { path, is_dir, delete_pending: AtomicBool::new(false), dir_buffer: DirBuffer::new() }
    }
}

// ---- SharedFs ----------------------------------------------------------------
// Identical role to cdfuse's SharedFs; callers hold Arc<SharedFs>.

pub struct SharedFs {
    vfs:           VfsManager,
    decode_cache:  Mutex<LruCache<u64, Arc<[u8]>>>,
    cached_bytes:  AtomicUsize,
    in_flight:     Mutex<HashMap<u64, Arc<OnceLock<Option<Arc<[u8]>>>>>>,
    paz_maps:      Mutex<HashMap<String, Arc<Mmap>>>,
    // keyed by VFS path string (not ino — no session-thread path map here)
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
        let repack_engine = RepackEngine::new(&packages_path, None);
        SharedFs {
            vfs,
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
        q.push_back(msg);
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
        self.write_overlay.lock().unwrap().get(path).map(|d| d.len() as u64)
            .or_else(|| self.vfs.lookup(path).map(|e| e.orig_size as u64))
            .unwrap_or(0)
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
            use std::io::{Read, Seek, SeekFrom};
            let mut f = match std::fs::File::open(&entry.paz_file) {
                Ok(f)  => f,
                Err(e) => { warn!("decode {path}: open {}: {e}", entry.paz_file); return None; }
            };
            if let Err(e) = f.seek(SeekFrom::Start(entry.offset)) {
                warn!("decode {path}: seek: {e}"); return None;
            }
            let mut buf = vec![0u8; entry.comp_size as usize];
            if let Err(e) = f.read_exact(&mut buf) {
                warn!("decode {path}: read: {e}"); return None;
            }
            buf
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

    // -- flush to PAZ ----------------------------------------------------------

    pub fn flush_path_sync(&self, path: &str, data: Vec<u8>) {
        self.pending_paths.lock().unwrap().remove(path);
        self.write_mtimes.lock().unwrap().remove(path);

        if let Some(vf) = virtual_files::resolve(path) {
            match vf.kind {
                virtual_files::VirtualKind::PalocJson => {
                    match virtual_files::parse_paloc_jsonl(&data) {
                        Some(binary) => {
                            info!("flush {path}: paloc -> {}B, repacking {}", binary.len(), vf.source_path);
                            self.flush_path_sync(&vf.source_path, binary);
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

        let entry = match self.vfs.lookup(path) {
            Some(e) => e,
            None    => { warn!("flush {path}: not in VFS"); return; }
        };
        let group_dir = Path::new(&entry.paz_file)
            .parent().and_then(|p| p.file_name()).and_then(|n| n.to_str())
            .unwrap_or("").to_string();
        let pamt_data = match self.vfs.get_pamt(&group_dir) {
            Some(p) => p,
            None    => { warn!("flush {path}: no pamt for group {group_dir}"); return; }
        };
        self.cache_put(ino_for(path), Arc::from(data.clone()));
        let mf = ModifiedFile { data, entry: entry.clone(), pamt_data, package_group: group_dir.clone() };
        match self.repack_engine.repack(vec![mf], &self.papgt_path, true) {
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

    // -- directory population --------------------------------------------------
    // Fills a DirBufferLock with entries.  Called with reset=true once per
    // "readdir session"; DirBuffer caches entries for subsequent paginated calls.

    fn populate_dir(&self, lock: &winfsp::filesystem::DirBufferLock<'_>, path: &str) {
        macro_rules! add {
            ($name:expr, $fi:expr) => {{
                let mut di: DirInfo = DirInfo::new();
                if di.set_name($name).is_ok() {
                    *di.file_info_mut() = $fi;
                    if lock.write(&mut di).is_err() { return; }
                }
            }};
        }

        add!(".",  self.dir_info(path));
        add!("..", self.dir_info(parent_path(path)));

        // Virtual root directories appear only at the filesystem root.
        if path.is_empty() {
            for vdir_name in virtual_files::virtual_root_dirs() {
                if virtual_files::root_requires_vgmstream(vdir_name) {
                    continue;
                }
                add!(vdir_name, self.dir_info(vdir_name));
            }
        }

        // If this is a virtual directory, list its contents and return.
        if let Some(vdir) = virtual_files::resolve_virtual_dir(path) {
            self.populate_virtual_dir(lock, path, &vdir);
            return;
        }

        // Regular VFS children.
        let children = self.vfs.list_dir_with_sizes_unsorted(path);
        for (name, is_dir, orig_size) in &children {
            let cpath = child_path(path, name);
            let fi = if *is_dir {
                self.dir_info(&cpath)
            } else {
                let size = self.write_overlay.lock().unwrap()
                    .get(&cpath).map(|d| d.len() as u64)
                    .unwrap_or(*orig_size as u64);
                self.file_info(&cpath, size)
            };
            add!(name.as_str(), fi);
        }
    }

    fn populate_virtual_dir(
        &self,
        lock: &winfsp::filesystem::DirBufferLock<'_>,
        vpath: &str,
        vdir: &virtual_files::VirtualDirInfo,
    ) {
        let children = self.vfs.list_dir_with_sizes_unsorted(&vdir.real_path);
        for (name, is_dir, orig_size) in &children {
            if *is_dir {
                let real_child = child_path(&vdir.real_path, name);
                if !self.vfs.subtree_has_ext(&real_child, vdir.filter_ext) { continue; }
                let cvpath = child_path(vpath, name);
                let mut di: DirInfo = DirInfo::new();
                if di.set_name(name.as_str()).is_ok() {
                    *di.file_info_mut() = self.dir_info(&cvpath);
                    if lock.write(&mut di).is_err() { return; }
                }
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
                    name.clone()
                } else {
                    format!("{name}{}", vdir.suffix)
                };
                let cvpath = child_path(vpath, &virt_name);
                let mut di: DirInfo = DirInfo::new();
                if di.set_name(virt_name.as_str()).is_ok() {
                    *di.file_info_mut() = self.file_info(&cvpath, *orig_size as u64);
                    if lock.write(&mut di).is_err() { return; }
                }
            }
        }
    }
}

// ---- CdWinFs — thin wrapper, implements FileSystemContext -------------------

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
        match self.lookup(&path) {
            Some((fi, is_dir)) => {
                *file_info.as_mut() = fi;
                Ok(FileCtx::new(path, is_dir))
            }
            None => Err(FspError::NTSTATUS(NTSTATUS_NOT_FOUND)),
        }
    }

    fn close(&self, context: Self::FileContext) {
        let pending = self.0.pending_paths.lock().unwrap().contains(&context.path);
        if pending && self.0.auto_repack {
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
        if create_options & FILE_DIRECTORY_FILE != 0 {
            return Err(FspError::NTSTATUS(NTSTATUS_WRITE_PROT)); // no mkdir in VFS
        }
        let path = vfs_path(file_name);
        let now  = SystemTime::now();
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
        self.0.write_overlay.lock().unwrap().insert(path.clone(), Vec::new());
        self.0.pending_paths.lock().unwrap().insert(path.clone());
        self.0.write_mtimes.lock().unwrap().insert(path.clone(), SystemTime::now());
        *file_info = self.0.file_info(path, 0);
        Ok(())
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        if flags & FSP_CLEANUP_DELETE != 0 && context.delete_pending.load(Ordering::Relaxed) {
            let path = &context.path;
            self.0.vfs.remove_entry(path);
            self.0.decode_cache.lock().unwrap().pop(&ino_for(path));
            self.0.write_overlay.lock().unwrap().remove(path);
            self.0.write_mtimes.lock().unwrap().remove(path);
            self.0.pending_paths.lock().unwrap().remove(path);
        }
    }

    fn flush(&self, context: Option<&Self::FileContext>, file_info: &mut FileInfo) -> Result<()> {
        if let Some(ctx) = context {
            let data = self.0.write_overlay.lock().unwrap().get(&ctx.path).cloned();
            if let Some(data) = data {
                self.0.flush_path_sync(&ctx.path, data);
            }
            let size = self.0.file_size_for(&ctx.path);
            *file_info = self.0.file_info(&ctx.path, size);
        }
        Ok(())
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        if context.is_dir {
            *file_info = self.0.dir_info(&context.path);
        } else {
            *file_info = self.0.file_info(&context.path, self.0.file_size_for(&context.path));
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
            self.0.seed_overlay(path, ino_for(path));
            let mut ov = self.0.write_overlay.lock().unwrap();
            if let Some(buf) = ov.get_mut(path) { buf.resize(new_size as usize, 0); }
            self.0.write_mtimes.lock().unwrap().insert(path.clone(), SystemTime::now());
        }
        *file_info = self.0.file_info(&context.path, self.0.file_size_for(&context.path));
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        let path = &context.path;
        let ino  = ino_for(path);

        // Write overlay takes priority over cache/decode.
        {
            let ov = self.0.write_overlay.lock().unwrap();
            if let Some(data) = ov.get(path) {
                let s = (offset as usize).min(data.len());
                let e = (s + buffer.len()).min(data.len());
                buffer[..e - s].copy_from_slice(&data[s..e]);
                return Ok((e - s) as u32);
            }
        }

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
        *file_info = self.0.file_info(path, self.0.file_size_for(path));
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
        let reset = marker.is_none();
        {
            let lock = context.dir_buffer.acquire(reset, None)
                .map_err(|_| FspError::NTSTATUS(NTSTATUS_INSUF_RES))?;
            if reset {
                self.0.populate_dir(&lock, &context.path);
            }
            // lock released here
        }
        Ok(context.dir_buffer.read(marker, buffer))
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
