//! Windows filesystem implementation using WinFsp.
//!
//! WinFsp differs from FUSE in two key ways:
//!
//!   1. No inodes — each open handle carries its own FileContext (path + is_dir).
//!      No global ino→path map; no path-queue/drain pattern needed.
//!
//!   2. Multi-threaded by default — FileSystemContext methods are called from any
//!      of WinFsp's worker threads concurrently.  SharedFs is already safe for this
//!      (all mutable state behind Mutex/RwLock).
//!
//! Mount:
//!   cdfuse.exe Z: C:\path\to\crimson_desert\packages
//!
//! WinFsp runtime (winfsp-x64.dll) must be installed on the machine.

use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use cdcore::VfsManager;
use log::{debug, info, warn};
use winfsp::filesystem::{
    DirBuffer, DirBufferLock, DirInfo, DirMarker, FileInfo, FileSecurity,
    ModificationDescriptor, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::filesystem::FileSystemContext;
use winfsp::Result as FspResult;
use winfsp::U16CStr;
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_IO_DEVICE_ERROR, STATUS_MEDIA_WRITE_PROTECTED,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_NOT_FOUND,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
};

use crate::fs::{ino_for, SharedFs};
use crate::virtual_files;

// FILETIME offset: 100ns intervals from 1601-01-01 to 1970-01-01
const EPOCH_OFFSET_100NS: u64 = 116_444_736_000_000_000;

fn now_filetime() -> u64 {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secs * 10_000_000 + EPOCH_OFFSET_100NS
}

fn systemtime_to_filetime(t: SystemTime) -> u64 {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    secs * 10_000_000 + EPOCH_OFFSET_100NS
}

// -- Pattern matching (for read_directory filter) ------------------------------

fn glob_match(pattern: &[char], text: &[char]) -> bool {
    if pattern.is_empty() { return text.is_empty(); }
    if pattern[0] == '*' {
        for i in 0..=text.len() {
            if glob_match(&pattern[1..], &text[i..]) { return true; }
        }
        return false;
    }
    if text.is_empty() { return false; }
    if pattern[0] == '<' {
        // WinFsp uses < as a DOS-mode wildcard matching * but stopping at dots
        // Treat as * for simplicity
        for i in 0..=text.len() {
            if glob_match(&pattern[1..], &text[i..]) { return true; }
        }
        return false;
    }
    if pattern[0] == '>' || pattern[0] == '?' || pattern[0].to_lowercase().next() == text[0].to_lowercase().next() {
        glob_match(&pattern[1..], &text[1..])
    } else {
        false
    }
}

fn pattern_matches(pattern: Option<&U16CStr>, name: &str) -> bool {
    let p = match pattern {
        None => return true,
        Some(p) => p.to_string_lossy().to_string(),
    };
    if p == "*" || p == "*.*" { return true; }
    let pc: Vec<char> = p.to_lowercase().chars().collect();
    let nc: Vec<char> = name.to_lowercase().chars().collect();
    glob_match(&pc, &nc)
}

// -- FileContext ---------------------------------------------------------------

pub struct FileContext {
    pub vfs_path: String,
    pub is_dir: bool,
    /// Pending delete flag; set by set_delete(), acted on in cleanup().
    pub pending_delete: AtomicBool,
    /// Directory read buffer; populated lazily on first read_directory call.
    pub dir_buffer: DirBuffer,
}

impl FileContext {
    fn new(vfs_path: String, is_dir: bool) -> Self {
        FileContext {
            vfs_path,
            is_dir,
            pending_delete: AtomicBool::new(false),
            dir_buffer: DirBuffer::new(),
        }
    }
}

// -- CdFsWin ------------------------------------------------------------------

pub struct CdFsWin {
    shared: Arc<SharedFs>,
}

impl CdFsWin {
    pub fn new(vfs: VfsManager, readonly: bool) -> Self {
        CdFsWin { shared: Arc::new(SharedFs::new_inner(vfs, readonly)) }
    }

    pub fn shared(&self) -> Arc<SharedFs> { Arc::clone(&self.shared) }

    fn normalize_path(winfsp_path: &U16CStr) -> String {
        let s = winfsp_path.to_string_lossy().to_string();
        let s = s.trim_start_matches('\\');
        s.replace('\\', "/")
    }

    fn file_info_for(&self, path: &str, ino: u64) -> FileInfo {
        let mtime = self.shared.write_mtimes.lock().unwrap()
            .get(&ino).copied()
            .map(systemtime_to_filetime)
            .unwrap_or(EPOCH_OFFSET_100NS);

        let attrs = if self.shared.readonly {
            FILE_ATTRIBUTE_NORMAL.0 | FILE_ATTRIBUTE_READONLY.0
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        };

        let size = self.shared.write_overlay.lock().unwrap()
            .get(&ino).map(|d| d.len() as u64)
            .or_else(|| self.shared.vfs.lookup(path).map(|e| e.orig_size as u64))
            .or_else(|| {
                virtual_files::resolve(path).and_then(|vf| {
                    self.shared.vfs.lookup(&vf.source_path).map(|e| e.orig_size as u64)
                })
            })
            .unwrap_or(0);

        let mut fi = FileInfo::default();
        fi.file_attributes = attrs;
        fi.file_size = size;
        fi.allocation_size = (size + 511) & !511;
        fi.creation_time = EPOCH_OFFSET_100NS;
        fi.last_access_time = mtime;
        fi.last_write_time = mtime;
        fi.change_time = mtime;
        fi
    }

    fn dir_file_info(&self) -> FileInfo {
        let attrs = FILE_ATTRIBUTE_DIRECTORY.0
            | if self.shared.readonly { FILE_ATTRIBUTE_READONLY.0 } else { 0 };
        let mut fi = FileInfo::default();
        fi.file_attributes = attrs;
        fi.creation_time = EPOCH_OFFSET_100NS;
        fi.last_access_time = EPOCH_OFFSET_100NS;
        fi.last_write_time = EPOCH_OFFSET_100NS;
        fi.change_time = EPOCH_OFFSET_100NS;
        fi
    }

    fn path_is_dir(&self, path: &str) -> bool {
        if path.is_empty() { return true; }
        if self.shared.vfs.dir_exists(path) { return true; }
        if virtual_files::resolve_virtual_dir(path).is_some() { return true; }
        false
    }

    fn path_exists(&self, path: &str) -> bool {
        if path.is_empty() { return true; }
        if self.shared.vfs.lookup(path).is_some() { return true; }
        if self.path_is_dir(path) { return true; }
        if let Some(vf) = virtual_files::resolve(path) {
            return self.shared.vfs.lookup(&vf.source_path).is_some();
        }
        let ino = ino_for(path);
        self.shared.write_overlay.lock().unwrap().contains_key(&ino)
    }

    fn fill_dir_entries(
        &self,
        lock: &DirBufferLock<'_>,
        path: &str,
        pattern: Option<&U16CStr>,
    ) -> FspResult<()> {
        // . and ..
        for dot in [".", ".."] {
            if !pattern_matches(pattern, dot) { continue; }
            let mut di = DirInfo::<255>::new();
            *di.file_info_mut() = self.dir_file_info();
            di.set_name(OsStr::new(dot));
            lock.write(&mut di)?;
        }

        // Inject virtual root dirs into VFS root
        if path.is_empty() {
            for vdir_name in virtual_files::virtual_root_dirs() {
                if !pattern_matches(pattern, vdir_name) { continue; }
                let mut di = DirInfo::<255>::new();
                *di.file_info_mut() = self.dir_file_info();
                di.set_name(OsStr::new(vdir_name));
                lock.write(&mut di)?;
            }
        }

        // Virtual directory mirror (e.g. .paloc.jsonl/game/text)
        if let Some(vdir) = virtual_files::resolve_virtual_dir(path) {
            let children = self.shared.vfs.list_dir_with_sizes_unsorted(&vdir.real_path);
            for (name, is_dir, orig_size) in &children {
                if *is_dir {
                    // Only include subdirs that contain files of the target extension
                    let real_child = if vdir.real_path.is_empty() {
                        name.clone()
                    } else {
                        format!("{}/{name}", vdir.real_path)
                    };
                    if !self.shared.vfs.subtree_has_ext(&real_child, vdir.filter_ext) {
                        continue;
                    }
                    if !pattern_matches(pattern, name) { continue; }
                    let mut di = DirInfo::<255>::new();
                    *di.file_info_mut() = self.dir_file_info();
                    di.set_name(OsStr::new(name));
                    lock.write(&mut di)?;
                } else if name.ends_with(vdir.filter_ext) {
                    let should_add = if vdir.filter_ext == ".pabgb" {
                        name.strip_suffix(".pabgb").is_some_and(|base| {
                            let sibling = if vdir.real_path.is_empty() {
                                format!("{base}.pabgh")
                            } else {
                                format!("{}/{base}.pabgh", vdir.real_path)
                            };
                            self.shared.vfs.lookup(&sibling).is_some()
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
                    if !pattern_matches(pattern, &virt_name) { continue; }
                    let mut di = DirInfo::<255>::new();
                    let mut fi = FileInfo::default();
                    fi.file_attributes = FILE_ATTRIBUTE_NORMAL.0;
                    fi.file_size = *orig_size as u64;
                    fi.allocation_size = (*orig_size as u64 + 511) & !511;
                    fi.creation_time = EPOCH_OFFSET_100NS;
                    fi.last_access_time = EPOCH_OFFSET_100NS;
                    fi.last_write_time = EPOCH_OFFSET_100NS;
                    fi.change_time = EPOCH_OFFSET_100NS;
                    *di.file_info_mut() = fi;
                    di.set_name(OsStr::new(&virt_name));
                    lock.write(&mut di)?;
                }
            }
            return Ok(());
        }

        // Regular VFS directory
        let children = self.shared.vfs.list_dir_with_sizes_unsorted(path);
        for (name, is_dir, orig_size) in &children {
            if !pattern_matches(pattern, name) { continue; }
            let mut di = DirInfo::<255>::new();
            if *is_dir {
                *di.file_info_mut() = self.dir_file_info();
            } else {
                let child_path = SharedFs::child_path(path, name);
                let child_ino  = ino_for(&child_path);
                // Actual size may differ if there's an overlay (e.g. pending write)
                let size = self.shared.write_overlay.lock().unwrap()
                    .get(&child_ino).map(|d| d.len() as u64)
                    .unwrap_or(*orig_size as u64);
                let mtime = self.shared.write_mtimes.lock().unwrap()
                    .get(&child_ino).copied()
                    .map(systemtime_to_filetime)
                    .unwrap_or(EPOCH_OFFSET_100NS);
                let attrs = FILE_ATTRIBUTE_NORMAL.0
                    | if self.shared.readonly { FILE_ATTRIBUTE_READONLY.0 } else { 0 };
                let mut fi = FileInfo::default();
                fi.file_attributes = attrs;
                fi.file_size = size;
                fi.allocation_size = (size + 511) & !511;
                fi.creation_time = EPOCH_OFFSET_100NS;
                fi.last_access_time = mtime;
                fi.last_write_time = mtime;
                fi.change_time = mtime;
                *di.file_info_mut() = fi;
            }
            di.set_name(OsStr::new(name));
            lock.write(&mut di)?;
        }

        Ok(())
    }
}

// -- FileSystemContext impl ----------------------------------------------------

impl FileSystemContext for CdFsWin {
    type FileContext = FileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [std::ffi::c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> FspResult<FileSecurity> {
        let path = Self::normalize_path(file_name);
        debug!("get_security_by_name: {path:?}");

        let attrs = if self.path_is_dir(&path) {
            FILE_ATTRIBUTE_DIRECTORY.0
                | if self.shared.readonly { FILE_ATTRIBUTE_READONLY.0 } else { 0 }
        } else if self.path_exists(&path) {
            FILE_ATTRIBUTE_NORMAL.0
                | if self.shared.readonly { FILE_ATTRIBUTE_READONLY.0 } else { 0 }
        } else {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND.into());
        };

        Ok(FileSecurity { reparse: false, sz_security_descriptor: 0, attributes: attrs })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        let path = Self::normalize_path(file_name);
        debug!("open: {path:?}");

        if self.path_is_dir(&path) {
            *file_info.as_mut() = self.dir_file_info();
            return Ok(FileContext::new(path, true));
        }

        if self.path_exists(&path) {
            let ino = ino_for(&path);
            *file_info.as_mut() = self.file_info_for(&path, ino);
            return Ok(FileContext::new(path, false));
        }

        Err(STATUS_OBJECT_NAME_NOT_FOUND.into())
    }

    fn close(&self, context: Self::FileContext) {
        // Update decode cache with final overlay content so re-opens see it.
        let ino = ino_for(&context.vfs_path);
        if let Some(data) = self.shared.write_overlay.lock().unwrap().get(&ino) {
            self.shared.cache_put(ino, Arc::from(data.as_slice()));
        }
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> FspResult<()> {
        if context.is_dir {
            *file_info = self.dir_file_info();
        } else {
            *file_info = self.file_info_for(&context.vfs_path, ino_for(&context.vfs_path));
        }
        Ok(())
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> FspResult<()> {
        out.total_size = 100 * 1024 * 1024 * 1024;
        out.free_size  = 50  * 1024 * 1024 * 1024;
        out.set_volume_label("cdfuse");
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        _security_descriptor: Option<&mut [std::ffi::c_void]>,
    ) -> FspResult<u64> {
        // Return 0 — no security descriptor; permissive default.
        Ok(0)
    }

    fn set_security(
        &self,
        _context: &Self::FileContext,
        _security_information: u32,
        _modification_descriptor: ModificationDescriptor,
    ) -> FspResult<()> {
        // Accept silently; we don't persist security descriptors.
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> FspResult<u32> {
        let ino = ino_for(&context.vfs_path);
        debug!("read: {} offset={offset} len={}", context.vfs_path, buffer.len());

        // Serve from overlay first (pending writes)
        {
            let overlay = self.shared.write_overlay.lock().unwrap();
            if let Some(data) = overlay.get(&ino) {
                let s = (offset as usize).min(data.len());
                let e = (s + buffer.len()).min(data.len());
                let n = e - s;
                buffer[..n].copy_from_slice(&data[s..e]);
                return Ok(n as u32);
            }
        }

        // Serve from decode cache
        if let Some(data) = self.shared.cache_get(ino) {
            let s = (offset as usize).min(data.len());
            let e = (s + buffer.len()).min(data.len());
            let n = e - s;
            buffer[..n].copy_from_slice(&data[s..e]);
            return Ok(n as u32);
        }

        // Decode (blocking on WinFsp worker thread)
        match self.shared.decode(ino, &context.vfs_path) {
            Some(data) => {
                let s = (offset as usize).min(data.len());
                let e = (s + buffer.len()).min(data.len());
                let n = e - s;
                buffer[..n].copy_from_slice(&data[s..e]);
                info!("read {}: decoded {}B [{s}..{e}]", context.vfs_path, data.len());
                Ok(n as u32)
            }
            None => {
                warn!("read {}: decode failed", context.vfs_path);
                Err(STATUS_IO_DEVICE_ERROR.into())
            }
        }
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<u32> {
        if self.shared.readonly {
            return Err(STATUS_MEDIA_WRITE_PROTECTED.into());
        }
        if context.is_dir {
            return Err(STATUS_NOT_A_DIRECTORY.into());
        }
        let ino = ino_for(&context.vfs_path);

        // Reject writes to read-only virtual formats
        if let Some(vf) = virtual_files::resolve(&context.vfs_path) {
            match vf.kind {
                virtual_files::VirtualKind::PalocJson => {}
                virtual_files::VirtualKind::DdsPng    => {}
                _ => return Err(STATUS_ACCESS_DENIED.into()),
            }
        }

        let known = self.shared.vfs.lookup(&context.vfs_path).is_some()
            || self.shared.write_overlay.lock().unwrap().contains_key(&ino);
        if !known {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND.into());
        }

        // Seed overlay on first write
        let needs_seed = !self.shared.write_overlay.lock().unwrap().contains_key(&ino);
        if needs_seed {
            let seed = self.shared.cache_get(ino)
                .map(|d| d.to_vec())
                .unwrap_or_else(|| {
                    self.shared.decode(ino, &context.vfs_path)
                        .map(|d| d.to_vec())
                        .unwrap_or_default()
                });
            self.shared.write_overlay.lock().unwrap().entry(ino).or_insert(seed);
        }

        self.shared.pending_paths.lock().unwrap()
            .entry(ino).or_insert_with(|| context.vfs_path.clone());
        self.shared.write_mtimes.lock().unwrap().insert(ino, SystemTime::now());

        let mut overlay = self.shared.write_overlay.lock().unwrap();
        let buf = overlay.get_mut(&ino).unwrap();
        let off = if write_to_eof { buf.len() } else { offset as usize };
        let end = off + buffer.len();
        if end > buf.len() { buf.resize(end, 0); }
        buf[off..end].copy_from_slice(buffer);
        drop(overlay);

        *file_info = self.file_info_for(&context.vfs_path, ino);
        Ok(buffer.len() as u32)
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let ino = ino_for(&context.vfs_path);
        if set_allocation_size {
            // Allocation hint — ignore, return current info.
            *file_info = self.file_info_for(&context.vfs_path, ino);
            return Ok(());
        }
        let needs_seed = !self.shared.write_overlay.lock().unwrap().contains_key(&ino);
        if needs_seed {
            let seed = self.shared.cache_get(ino)
                .map(|d| d.to_vec())
                .unwrap_or_else(|| {
                    self.shared.decode(ino, &context.vfs_path)
                        .map(|d| d.to_vec())
                        .unwrap_or_default()
                });
            self.shared.write_overlay.lock().unwrap().entry(ino).or_insert(seed);
        }
        if let Some(buf) = self.shared.write_overlay.lock().unwrap().get_mut(&ino) {
            buf.resize(new_size as usize, 0);
        }
        *file_info = self.file_info_for(&context.vfs_path, ino);
        Ok(())
    }

    fn flush(&self, _context: Option<&Self::FileContext>, file_info: &mut FileInfo) -> FspResult<()> {
        // Writes live in the overlay until explicit commit via TUI or unmount.
        // Return a zeroed FileInfo — WinFsp accepts this for flush no-ops.
        *file_info = FileInfo::default();
        Ok(())
    }

    fn create(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        if self.shared.readonly {
            return Err(STATUS_MEDIA_WRITE_PROTECTED.into());
        }
        let path = Self::normalize_path(file_name);
        let ino = ino_for(&path);
        self.shared.write_overlay.lock().unwrap().entry(ino).or_insert_with(Vec::new);
        *file_info.as_mut() = self.file_info_for(&path, ino);
        Ok(FileContext::new(path, false))
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let ino = ino_for(&context.vfs_path);
        // Truncate to zero and seed overlay.
        self.shared.write_overlay.lock().unwrap().insert(ino, Vec::new());
        self.shared.pending_paths.lock().unwrap()
            .entry(ino).or_insert_with(|| context.vfs_path.clone());
        self.shared.write_mtimes.lock().unwrap().insert(ino, SystemTime::now());
        *file_info = self.file_info_for(&context.vfs_path, ino);
        Ok(())
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        // FspCleanupDelete = 0x01
        if flags & 0x01 != 0 && context.pending_delete.load(Ordering::Relaxed) {
            let ino = ino_for(&context.vfs_path);
            if virtual_files::resolve(&context.vfs_path).is_some() {
                self.shared.write_overlay.lock().unwrap().remove(&ino);
                self.shared.write_mtimes.lock().unwrap().remove(&ino);
                self.shared.pending_paths.lock().unwrap().remove(&ino);
                info!("cleanup (delete) {} (virtual, overlay discarded)", context.vfs_path);
            } else {
                self.shared.vfs.remove_entry(&context.vfs_path);
                self.shared.decode_cache.lock().unwrap().pop(&ino);
                self.shared.write_overlay.lock().unwrap().remove(&ino);
                self.shared.write_mtimes.lock().unwrap().remove(&ino);
                self.shared.pending_paths.lock().unwrap().remove(&ino);
                info!("cleanup (delete) {} (removed from VFS index)", context.vfs_path);
            }
        }
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> FspResult<()> {
        context.pending_delete.store(delete_file, Ordering::Relaxed);
        Ok(())
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> FspResult<()> {
        let src = &context.vfs_path;
        let dst = Self::normalize_path(new_file_name);
        let src_ino = ino_for(src);
        let dst_ino = ino_for(&dst);

        let moved = self.shared.write_overlay.lock().unwrap().remove(&src_ino);
        if let Some(data) = moved {
            self.shared.cache_put(dst_ino, Arc::from(data.clone()));
            self.shared.write_overlay.lock().unwrap().insert(dst_ino, data);
        }
        self.shared.pending_paths.lock().unwrap().remove(&src_ino);
        self.shared.pending_paths.lock().unwrap()
            .entry(dst_ino).or_insert_with(|| dst.clone());
        debug!("rename {src:?} -> {dst:?}");
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        // Accept silently; we track write time ourselves.
        if context.is_dir {
            *file_info = self.dir_file_info();
        } else {
            *file_info = self.file_info_for(&context.vfs_path, ino_for(&context.vfs_path));
        }
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> FspResult<u32> {
        if !context.is_dir {
            return Err(STATUS_NOT_A_DIRECTORY.into());
        }
        debug!("read_directory: {} pattern={:?}", context.vfs_path, pattern.map(|p| p.to_string_lossy().to_string()));

        // acquire() returns Ok(lock) if we need to fill the buffer (first call or reset).
        // Returns Err if the buffer is already filled and we just need to read.
        let reset = marker.is_none();
        if let Ok(lock) = context.dir_buffer.acquire(reset, None) {
            self.fill_dir_entries(&lock, &context.vfs_path, pattern)?;
        }

        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn dispatcher_stopped(&self, _normally: bool) {
        // Final flush: repack all pending writes to PAZ on unmount.
        let overlay = std::mem::take(&mut *self.shared.write_overlay.lock().unwrap());
        if overlay.is_empty() { return; }
        warn!("dispatcher_stopped: flushing {} write(s) to PAZ", overlay.len());
        let paths = self.shared.pending_paths.lock().unwrap().clone();
        for (ino, data) in overlay {
            if let Some(path) = paths.get(&ino) {
                self.shared.flush_ino_sync(ino, path, data);
            } else {
                warn!("dispatcher_stopped: no path for ino {ino}, skipping");
            }
        }
    }
}
