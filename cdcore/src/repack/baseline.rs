//! PAC mesh baseline snapshot system.
//!
//! Before a PAC mesh is first modified, the original bytes are snapshotted
//! at ~/.crimsonforge/mesh_baselines/<sha1_hex>.bin. All subsequent repacks
//! use the snapshot as the base to prevent double-patch accumulation.

use std::path::PathBuf;
use sha1::{Sha1, Digest};
use crate::error::Result;

fn baseline_dir() -> PathBuf {
    dirs_or_home().join(".crimsonforge").join("mesh_baselines")
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Compute the SHA-1 hex digest of data.
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// Check if a baseline exists for the given original bytes.
pub fn has_baseline(original_data: &[u8]) -> bool {
    let key = sha1_hex(original_data);
    baseline_dir().join(format!("{key}.bin")).exists()
}

/// Read the baseline for the given original bytes, or return the original bytes.
pub fn get_or_create(original_data: &[u8]) -> Result<Vec<u8>> {
    let key = sha1_hex(original_data);
    let path = baseline_dir().join(format!("{key}.bin"));

    if path.exists() {
        return Ok(std::fs::read(&path)?);
    }

    // Create baseline directory and write snapshot
    std::fs::create_dir_all(baseline_dir())?;
    std::fs::write(&path, original_data)?;

    Ok(original_data.to_vec())
}

/// Explicitly save a baseline for a PAC mesh.
pub fn save(original_data: &[u8]) -> Result<String> {
    let key = sha1_hex(original_data);
    let path = baseline_dir().join(format!("{key}.bin"));
    std::fs::create_dir_all(baseline_dir())?;
    std::fs::write(&path, original_data)?;
    Ok(key)
}
