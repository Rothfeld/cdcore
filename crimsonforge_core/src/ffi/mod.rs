//! C-ABI exports for Python ctypes integration.
//!
//! All pointers returned must be freed with the corresponding `_free` function.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::slice;

use crate::crypto::{pa_checksum, decrypt};
use crate::compression;
use crate::vfs::VfsManager;

// ── Checksum ──────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cf_pa_checksum(data: *const u8, len: usize) -> u32 {
    if data.is_null() || len == 0 { return 0; }
    let bytes = unsafe { slice::from_raw_parts(data, len) };
    pa_checksum(bytes)
}

// ── Crypto ────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cf_decrypt(
    data: *const u8, len: usize,
    filename: *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    if data.is_null() || filename.is_null() { return std::ptr::null_mut(); }
    let bytes = unsafe { slice::from_raw_parts(data, len) };
    let name = unsafe { CStr::from_ptr(filename) }.to_str().unwrap_or("");
    let result = decrypt(bytes, name);
    let boxed = result.into_boxed_slice();
    if !out_len.is_null() {
        unsafe { *out_len = boxed.len(); }
    }
    Box::into_raw(boxed) as *mut u8
}

#[no_mangle]
pub extern "C" fn cf_encrypt(
    data: *const u8, len: usize,
    filename: *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    cf_decrypt(data, len, filename, out_len) // symmetric
}

#[no_mangle]
pub extern "C" fn cf_free_bytes(ptr: *mut u8, len: usize) {
    if ptr.is_null() { return; }
    unsafe {
        let _ = Box::from_raw(slice::from_raw_parts_mut(ptr, len));
    }
}

// ── Compression ───────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cf_lz4_decompress(
    data: *const u8, len: usize,
    orig_size: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if data.is_null() { return std::ptr::null_mut(); }
    let bytes = unsafe { slice::from_raw_parts(data, len) };
    match compression::lz4::decompress(bytes, orig_size) {
        Ok(v) => {
            let boxed = v.into_boxed_slice();
            if !out_len.is_null() { unsafe { *out_len = boxed.len(); } }
            Box::into_raw(boxed) as *mut u8
        }
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn cf_lz4_compress(
    data: *const u8, len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if data.is_null() { return std::ptr::null_mut(); }
    let bytes = unsafe { slice::from_raw_parts(data, len) };
    let v = compression::lz4::compress(bytes);
    let boxed = v.into_boxed_slice();
    if !out_len.is_null() { unsafe { *out_len = boxed.len(); } }
    Box::into_raw(boxed) as *mut u8
}

// ── VFS ───────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cf_vfs_new(packages_path: *const c_char) -> *mut VfsManager {
    let path = unsafe { CStr::from_ptr(packages_path) }.to_str().unwrap_or("");
    match VfsManager::new(path) {
        Ok(vfs) => Box::into_raw(Box::new(vfs)),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn cf_vfs_free(vfs: *mut VfsManager) {
    if !vfs.is_null() {
        unsafe { drop(Box::from_raw(vfs)); }
    }
}

#[no_mangle]
pub extern "C" fn cf_vfs_load_all(vfs: *mut VfsManager) -> c_int {
    if vfs.is_null() { return -1; }
    let vfs = unsafe { &*vfs };
    match vfs.load_all_groups() {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// Extract a file by virtual path. Caller must free with cf_free_bytes.
#[no_mangle]
pub extern "C" fn cf_vfs_extract(
    vfs: *mut VfsManager,
    path: *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    if vfs.is_null() || path.is_null() { return std::ptr::null_mut(); }
    let vfs  = unsafe { &*vfs };
    let path = unsafe { CStr::from_ptr(path) }.to_str().unwrap_or("");

    let entry = match vfs.lookup(path) {
        Some(e) => e,
        None    => return std::ptr::null_mut(),
    };
    match vfs.read_entry(&entry) {
        Ok(data) => {
            let boxed = data.into_boxed_slice();
            if !out_len.is_null() { unsafe { *out_len = boxed.len(); } }
            Box::into_raw(boxed) as *mut u8
        }
        Err(_) => std::ptr::null_mut(),
    }
}
