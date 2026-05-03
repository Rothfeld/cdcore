//! Auto-detection helpers for first-run defaults on Linux.

use std::path::{Path, PathBuf};

// -- Optional tool detection --------------------------------------------------

/// Find vgmstream-cli in PATH or the CrimsonForge managed tools directory.
pub fn detect_vgmstream() -> Option<PathBuf> {
    find_tool(&["vgmstream-cli", "vgmstream_cli"])
}

/// Find ffmpeg in PATH or the CrimsonForge managed tools directory.
pub fn detect_ffmpeg() -> Option<PathBuf> {
    find_tool(&["ffmpeg"])
}

fn find_tool(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for &name in names {
            let p = dir.join(name);
            if p.exists() { return Some(p); }
        }
    }
    // Check managed install location used by CrimsonForge
    let home = std::env::var("HOME").unwrap_or_default();
    for &name in names {
        let p = PathBuf::from(&home).join(".crimsonforge/tools").join(name);
        if p.exists() { return Some(p); }
    }
    None
}

/// Try to locate the Crimson Desert packages directory.
pub fn detect_game_dir() -> Option<PathBuf> {
    // 1. /cd — devcontainer mount
    let cd = PathBuf::from("/cd");
    if is_packages_dir(&cd) { return Some(cd); }

    // 2. Steam library paths
    let home = std::env::var("HOME").unwrap_or_default();
    let steam_roots = [
        format!("{home}/.steam/steam/steamapps/common"),
        format!("{home}/.local/share/Steam/steamapps/common"),
        "/run/media".to_string(),
    ];
    for root in &steam_roots {
        let base = PathBuf::from(root);
        for name in ["Crimson Desert", "CrimsonDesert"] {
            if let Some(p) = resolve_packages(&base.join(name)) { return Some(p); }
        }
    }

    // 3. Common manual install paths
    for root in ["/opt", "/games", "/home"] {
        let base = PathBuf::from(root);
        if !base.exists() { continue; }
        for name in ["crimson-desert", "CrimsonDesert", "Crimson Desert",
                     "pearl-abyss/crimson-desert"] {
            if let Some(p) = resolve_packages(&base.join(name)) { return Some(p); }
        }
    }

    None
}

/// Suggest a default mount point under /media/<user>/cd.
pub fn detect_default_mount() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "user".to_string());
    format!("/media/{user}/cd")
}

fn resolve_packages(root: &Path) -> Option<PathBuf> {
    if is_packages_dir(root)          { return Some(root.to_path_buf()); }
    let sub = root.join("packages");
    if is_packages_dir(&sub)          { return Some(sub); }
    None
}

fn is_packages_dir(dir: &Path) -> bool {
    dir.join("meta").is_dir()
}
