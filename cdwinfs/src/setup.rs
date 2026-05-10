//! Auto-detection helpers for first-run defaults.
//! No TUI code lives here -- path selection is handled inside tui.rs.

use std::path::{Path, PathBuf};

/// Try to locate the Crimson Desert packages directory automatically.
/// Returns the directory that directly contains 0000\, meta\, etc.
pub fn detect_game_dir() -> Option<PathBuf> {
    // 1. Registry: standard Windows uninstall entries
    if let Some(p) = from_uninstall_registry() { return Some(p); }

    // 2. Common paths on every present drive
    for c in 'A'..='Z' {
        let drive = PathBuf::from(format!("{c}:\\"));
        if !drive.exists() { continue; }
        if is_packages_dir(&drive) { return Some(drive.clone()); }
        for sub in ["Pearl Abyss\\Crimson Desert",
                    "Pearl Abyss\\CrimsonDesert",
                    "Crimson Desert", "CrimsonDesert"] {
            if let Some(p) = resolve_packages(&drive.join(sub)) { return Some(p); }
        }
    }

    // 3. Program Files variants
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(var) {
            for sub in ["Pearl Abyss\\Crimson Desert", "Crimson Desert"] {
                if let Some(p) = resolve_packages(&PathBuf::from(&pf).join(sub)) {
                    return Some(p);
                }
            }
        }
    }

    None
}

/// Return the first unused drive letter scanning Z -> D (single char, no colon).
pub fn detect_free_drive() -> Option<String> {
    for c in ('D'..='Z').rev() {
        if !PathBuf::from(format!("{c}:\\")).exists() {
            return Some(c.to_string());
        }
    }
    None
}

/// Scan the three standard Uninstall hives in-process for a "Crimson Desert"
/// entry and resolve its `InstallLocation`.
///
/// Done via the Win32 registry API (winreg crate), not by shelling out to
/// `powershell`.  A spawned `powershell.exe` triggers Sysmon process_creation
/// events that match noisy Sigma rules in VirusTotal sandbox telemetry.
fn from_uninstall_registry() -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;
    let hives = [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_CURRENT_USER,  r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    for (hive, path) in hives {
        let Ok(root) = RegKey::predef(hive).open_subkey_with_flags(path, KEY_READ)
            else { continue };
        for subkey in root.enum_keys().flatten() {
            let Ok(sub) = root.open_subkey_with_flags(&subkey, KEY_READ) else { continue };
            let Ok(name): Result<String, _> = sub.get_value("DisplayName") else { continue };
            if !name.contains("Crimson Desert") { continue; }
            let Ok(loc): Result<String, _> = sub.get_value("InstallLocation") else { continue };
            let loc = loc.trim_end_matches('\\');
            if loc.is_empty() { continue; }
            if let Some(p) = resolve_packages(&PathBuf::from(loc)) { return Some(p); }
        }
    }
    None
}

fn resolve_packages(root: &Path) -> Option<PathBuf> {
    if is_packages_dir(root)           { return Some(root.to_path_buf()); }
    let sub = root.join("packages");
    if is_packages_dir(&sub)           { return Some(sub); }
    None
}

fn is_packages_dir(dir: &Path) -> bool {
    dir.join("meta").is_dir()
}
