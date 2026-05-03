fn main() {
    winfsp::build::winfsp_link_delayload();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

fn embed_windows_resources() {
    // Re-run if the git HEAD moves (local builds) or the CI commit env changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let hash    = commit_hash();
    let file_version_str = format!("{version}+{hash}");

    let mut res = winres::WindowsResource::new();
    res.set("FileDescription",  "Crimson Desert archive filesystem (WinFSP)");
    res.set("ProductName",      "cdwinfs");
    res.set("OriginalFilename", "cdwinfs.exe");
    res.set("InternalName",     "cdwinfs");
    res.set("LegalCopyright",   env!("CARGO_PKG_REPOSITORY"));
    // Explorer's "File version" row shows the numeric FILEVERSION (semver only).
    // Explorer's "Product version" and "Comments" rows show the string table
    // values — put the commit hash there so it's visible in file properties.
    res.set("FileVersion",      &file_version_str);
    res.set("ProductVersion",   &file_version_str);
    res.set("Comments",         &format!("commit {hash}"));
    res.set_manifest_file("manifest.xml");
    res.compile().expect("winres: failed to compile Windows resources");
}

/// Returns a short commit hash from $GITHUB_SHA (CI) or `git rev-parse` (local).
fn commit_hash() -> String {
    // GitHub Actions provides the full SHA.
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        return sha.chars().take(7).collect();
    }
    // Local: ask git directly.
    std::process::Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
