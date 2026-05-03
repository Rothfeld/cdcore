//! Auto-detection helpers for first-run defaults.
//! No TUI code lives here — path selection is handled inside tui.rs.

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

/// Return the first unused drive letter scanning Z → D.
pub fn detect_free_drive() -> Option<String> {
    for c in ('D'..='Z').rev() {
        if !PathBuf::from(format!("{c}:\\")).exists() {
            return Some(format!("{c}:"));
        }
    }
    None
}

fn from_uninstall_registry() -> Option<PathBuf> {
    let ps = r#"
$keys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
foreach ($k in $keys) {
    $hit = Get-ItemProperty $k -ErrorAction SilentlyContinue |
           Where-Object { $_.DisplayName -like '*Crimson Desert*' } |
           Select-Object -First 1
    if ($hit -and $hit.InstallLocation) { $hit.InstallLocation.TrimEnd('\'); break }
}
"#;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps])
        .output().ok()?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let s = raw.trim();
    if s.is_empty() { return None; }
    resolve_packages(&PathBuf::from(s))
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
