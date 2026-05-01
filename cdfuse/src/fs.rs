//! Parallel FUSE filesystem — `impl Filesystem for CdFs` where CdFs wraps Arc<SharedFs>.
//!
//! Concurrency model
//! ─────────────────
//! The FUSE session loop calls callbacks with `&mut CdFs` — single-threaded.
//! Slow operations (cold dir build, full decode) are offloaded to rayon workers.
//! Reply objects are Send and are consumed on the worker thread.
//!
//! Key insight: workers must not write to any structure that the session thread
//! reads on the hot path.  This was the root of the freeze:
//!
//!   DashMap  → crossbeam_epoch sched_yield (31% CPU wasted)
//!   RwLock   → bulk writes from N workers starved the session thread's reads
//!
//! Solution: session thread owns a private `paths` HashMap (no lock at all).
//! Workers push new (ino, path, is_dir) tuples onto a Mutex<Vec> queue.
//! Session thread drains the queue into `paths` at the top of each callback —
//! a single Mutex acquire/release, then all reads are unsynchronised.
//!
//! Workers touch only:
//!   dir_cache   RwLock — one brief write per dir (insert Arc<OnceLock>)
//!   in_flight   RwLock — one brief write per cold decode
//!   decode_cache Mutex — brief for cache probe/insert
//!   paz_maps     Mutex — rare (one per PAZ file)
//!   path_queue   Mutex — one push per child entry (append to Vec)
//!   vfs          internally Arc<RwLock<BTreeMap>>, read-only after load

use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEntry, Request,
};
use libc::{ENOENT, EIO};
use log::{debug, info, warn};
use lru::LruCache;
use memmap2::Mmap;

use crimsonforge_core::{VfsManager, crypto, compression};

const TTL:               Duration = Duration::from_secs(60);
const ROOT_INO:          u64     = 1;
const MAX_CACHE_ENTRIES: usize   = 2048;
const MAX_CACHED_BYTES:  usize   = 512 * 1024 * 1024;

// ── Inode helpers ─────────────────────────────────────────────────────────────

fn ino_for(path: &str) -> u64 {
    if path.is_empty() { return ROOT_INO; }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish().wrapping_mul(0x9e3779b97f4a7c15).max(2)
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

// ── DirEntry ──────────────────────────────────────────────────────────────────

struct DirEntry {
    ino:    u64,
    attr:   FileAttr,
    name:   String,
    path:   Box<str>,   // full virtual path — used by path_queue drain
    is_dir: bool,
}

// ── SharedFs — state accessed by BOTH session thread and rayon workers ────────

pub struct SharedFs {
    vfs:          VfsManager,
    path_queue:   Mutex<Vec<Vec<(u64, Box<str>, bool)>>>,
    dir_cache:    RwLock<HashMap<u64, Arc<OnceLock<Vec<DirEntry>>>>>,
    decode_cache: Mutex<LruCache<u64, Arc<[u8]>>>,
    cached_bytes: AtomicUsize,
    in_flight:    Mutex<HashMap<u64, Arc<OnceLock<Option<Arc<[u8]>>>>>>,
    paz_maps:     Mutex<HashMap<String, Arc<Mmap>>>,
    /// Dedicated thread pool for file decodes — separate from the rayon global
    /// pool used by dir builds so decodes are never queued behind dir builds.
    /// Fixed size: avoids the 292K × pthread_create overhead of std::thread::spawn.
    decode_pool:  rayon::ThreadPool,
    uid:          u32,
    gid:          u32,
}

impl SharedFs {
    fn new_inner(vfs: VfsManager) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let decode_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(16)
            .thread_name(|i| format!("cdfuse-decode-{i}"))
            .build()
            .expect("failed to build decode thread pool");
        SharedFs {
            vfs,
            path_queue:   Mutex::new(Vec::new()),
            dir_cache:    RwLock::new(HashMap::new()),
            decode_cache: Mutex::new(LruCache::new(NonZeroUsize::new(MAX_CACHE_ENTRIES).unwrap())),
            cached_bytes: AtomicUsize::new(0),
            in_flight:    Mutex::new(HashMap::new()),
            paz_maps:     Mutex::new(HashMap::new()),
            decode_pool,
            uid,
            gid,
        }
    }

    // ── Attr builders ─────────────────────────────────────────────────────────

    fn file_attr(&self, ino: u64, size: u64) -> FileAttr {
        FileAttr {
            ino, size, blocks: (size + 511) / 512,
            atime: UNIX_EPOCH, mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH, crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o444, nlink: 1,
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
            // Avoid format! overhead (string parsing + multiple allocations).
            let mut s = String::with_capacity(parent.len() + 1 + n.len());
            s.push_str(parent);
            s.push('/');
            s.push_str(&n);
            s
        }
    }

    // ── mmap pool ─────────────────────────────────────────────────────────────

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

    // ── Decode cache ──────────────────────────────────────────────────────────

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

    // ── Full decode (rayon worker) ─────────────────────────────────────────────

    fn decode(&self, ino: u64, path: &str) -> Option<Arc<[u8]>> {
        if let Some(d) = self.cache_get(ino) {
            return Some(d);
        }

        let slot = {
            let mut map = self.in_flight.lock().unwrap();
            Arc::clone(map.entry(ino).or_insert_with(|| Arc::new(OnceLock::new())))
        };

        let result = slot.get_or_init(|| {
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
                let mut f = std::fs::File::open(&entry.paz_file).ok()?;
                f.seek(SeekFrom::Start(entry.offset)).ok()?;
                let mut buf = vec![0u8; entry.comp_size as usize];
                f.read_exact(&mut buf).ok()?;
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

    // ── Probe read (mmap slice) ────────────────────────────────────────────────

    fn probe(&self, ino: u64, offset: i64, size: u32, path: &str) -> Option<Vec<u8>> {
        // Serve raw PAZ bytes only for *partial* offset-0 reads (size < orig_size).
        // This covers MIME detection on large files (Thunar reads 32KB of a 500KB file).
        // For small files where size >= orig_size (whole-file reads), we must serve
        // decoded content — removing this guard breaks encrypted files (raw bytes ≠
        // decrypted content, causing incorrect checksums and garbled data).
        if offset != 0 { return None; }
        if self.cache_get(ino).is_some() { return None; }
        let entry = self.vfs.lookup(path)?;
        if size >= entry.orig_size { return None; }  // whole-file read — must decode
        let mmap  = self.get_mmap(&entry.paz_file)?;
        let start = entry.offset as usize;
        let raw   = (size as usize).min(entry.comp_size as usize);
        let end   = (start + raw).min(mmap.len());
        if start >= mmap.len() { return None; }
        Some(mmap[start..end].to_vec())
    }

    // ── Dir cache ─────────────────────────────────────────────────────────────

    fn dir_slot(&self, ino: u64) -> Arc<OnceLock<Vec<DirEntry>>> {
        if let Some(s) = self.dir_cache.read().unwrap().get(&ino) {
            return Arc::clone(s);
        }
        let s = Arc::new(OnceLock::new());
        self.dir_cache.write().unwrap().entry(ino).or_insert_with(|| Arc::clone(&s));
        s
    }

    fn build_dir_entries(&self, ino: u64, path: &str) -> Vec<DirEntry> {
        let parent_ino = if ino == ROOT_INO { ROOT_INO } else { ino_for(parent_path(path)) };
        let mut entries = vec![
            DirEntry { ino, attr: self.dir_attr(ino), name: ".".into(),
                       path: path.into(), is_dir: true },
            DirEntry { ino: parent_ino, attr: self.dir_attr(parent_ino),
                       name: "..".into(), path: parent_path(path).into(), is_dir: true },
        ];

        let children = self.vfs.list_dir_with_sizes_unsorted(path);
        let mut queue_batch: Vec<(u64, Box<str>, bool)> = Vec::with_capacity(children.len());

        for (name, is_dir, orig_size) in &children {
            let child_path = Self::child_path(path, OsStr::new(name));
            let child_ino  = ino_for(&child_path);
            let attr = if *is_dir {
                self.dir_attr(child_ino)
            } else {
                self.file_attr(child_ino, *orig_size as u64)
            };
            queue_batch.push((child_ino, child_path.clone().into(), *is_dir));
            entries.push(DirEntry { ino: child_ino, attr, name: name.clone(),
                                    path: child_path.into(), is_dir: *is_dir });
        }

        // Push the whole batch as one Vec — O(1) pointer move under the lock.
        // extend() would hold the lock while moving 329K items; push() does not.
        self.path_queue.lock().unwrap().push(queue_batch);

        let n = entries.len().saturating_sub(2);
        info!("readdir {path:?} → {n} entries");
        entries
    }
}

// ── CdFs — session-thread-owned wrapper ──────────────────────────────────────

pub struct CdFs {
    shared: Arc<SharedFs>,
    /// Private path map — only the session thread reads/writes this.
    /// No lock needed. Populated by draining shared.path_queue each callback.
    paths: HashMap<u64, (Box<str>, bool)>,  // ino → (path, is_dir)
}

impl CdFs {
    pub fn new(vfs: VfsManager) -> Self {
        let shared = Arc::new(SharedFs::new_inner(vfs));
        let mut paths = HashMap::new();
        paths.insert(ROOT_INO, (Box::from(""), true));
        CdFs { shared, paths }
    }

    /// Drain any new inode batches that workers deposited since the last callback.
    /// Lock is held only for mem::take (O(1)); processing happens without the lock.
    fn drain(&mut self) {
        let batches = std::mem::take(&mut *self.shared.path_queue.lock().unwrap());
        if !batches.is_empty() {
            let total: usize = batches.iter().map(|b| b.len()).sum();
            debug!("drain: {} batches, {} inodes", batches.len(), total);
            for batch in batches {
                for (ino, path, is_dir) in batch {
                    self.paths.entry(ino).or_insert((path, is_dir));
                }
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

// ── Filesystem impl ───────────────────────────────────────────────────────────

impl Filesystem for CdFs {
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        // Advertise READDIRPLUS capability so the kernel sends READDIRPLUS
        // instead of READDIR + N×LOOKUP. Without this the kernel never calls
        // our readdirplus() handler, causing 329K individual lookup round-trips.
        // Requires abi-7-21. Without this flag the kernel never calls our
        // readdirplus() handler — it falls back to READDIR + N×LOOKUP instead.
        let _ = config.add_capabilities(fuser::consts::FUSE_DO_READDIRPLUS);
        info!("filesystem mounted (readdirplus enabled)");
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!("lookup parent={parent} name={name:?}");
        self.drain();
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        let child = SharedFs::child_path(&parent_path, name);

        if let Some(entry) = self.shared.vfs.lookup(&child) {
            let ino  = self.ensure_path(&child, false);
            let attr = self.shared.file_attr(ino, entry.orig_size as u64);
            debug!("lookup {child:?} → file ino={ino}");
            reply.entry(&TTL, &attr, 0);
            return;
        }
        if !self.shared.vfs.list_dir(&child).is_empty() || self.paths.contains_key(&ino_for(&child)) {
            let ino  = self.ensure_path(&child, true);
            let attr = self.shared.dir_attr(ino);
            debug!("lookup {child:?} → dir ino={ino}");
            reply.entry(&TTL, &attr, 0);
            return;
        }
        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        debug!("getattr ino={ino}");
        self.drain();
        if self.is_dir(ino) {
            reply.attr(&TTL, &self.shared.dir_attr(ino));
            return;
        }
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => { reply.error(ENOENT); return; }
        };
        match self.shared.vfs.lookup(&path) {
            Some(e) => reply.attr(&TTL, &self.shared.file_attr(ino, e.orig_size as u64)),
            None    => reply.error(ENOENT),
        }
    }

    fn readdirplus(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64,
                   reply: ReplyDirectoryPlus) {
        self.drain();
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => {
                warn!("readdirplus ino={ino} offset={offset} → ENOENT (unknown ino)");
                reply.error(ENOENT); return;
            }
        };
        let slot = self.shared.dir_slot(ino);
        if slot.get().is_some() {
            debug!("readdirplus {path:?} offset={offset} → cache hit");
            serve_readdirplus(slot.get().unwrap(), offset, reply);
            return;
        }
        info!("readdirplus {path:?} offset={offset} → cold, spawning build");
        let shared = Arc::clone(&self.shared);
        rayon::spawn(move || {
            info!("readdirplus worker: building {path:?}");
            slot.get_or_init(|| shared.build_dir_entries(ino, &path));
            info!("readdirplus worker: done {path:?}, serving offset={offset}");
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
        } else {
            reply.opened(0, fuser::consts::FOPEN_DIRECT_IO);
        }
    }

    fn read(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64,
            size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        self.drain();
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None    => {
                warn!("read ino={ino} offset={offset} → ENOENT (unknown ino)");
                reply.error(ENOENT); return;
            }
        };

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

        // Dedicated decode pool — separate from rayon global pool (dir builds).
        // Fixed thread count avoids 292K×pthread_create overhead.
        let shared = Arc::clone(&self.shared);
        // Borrow the pool before moving `shared` into the closure.
        let pool = &shared.decode_pool as *const rayon::ThreadPool;
        // SAFETY: pool points into `shared` which is kept alive by the Arc clone
        // passed into the closure; the pool outlives the closure execution.
        let pool = unsafe { &*pool };
        pool.spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                shared.decode(ino, &path)
            }));
            match result {
                Ok(Some(data)) => {
                    let s = (offset as usize).min(data.len());
                    let e = (s + size as usize).min(data.len());
                    info!("read {path:?} → decoded {}B [{s}..{e}]", data.len());
                    reply.data(&data[s..e]);
                }
                Ok(None) => {
                    warn!("read {path:?} → decode returned None");
                    reply.error(EIO);
                }
                Err(e) => {
                    let msg = e.downcast_ref::<&str>().copied()
                        .or_else(|| e.downcast_ref::<String>().map(|s| s.as_str()))
                        .unwrap_or("unknown panic");
                    warn!("read {path:?} → decode panicked: {msg}");
                    reply.error(EIO);
                }
            }
        });
    }
}

fn serve_readdirplus(entries: &[DirEntry], offset: i64, mut reply: ReplyDirectoryPlus) {
    for (i, e) in entries.iter().enumerate().skip(offset as usize) {
        if reply.add(e.ino, (i + 1) as i64, &e.name, &TTL, &e.attr, 0) { break; }
    }
    reply.ok();
}
