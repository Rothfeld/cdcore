fn main() {
    winfsp::build::winfsp_link_delayload();

    // Embed VERSIONINFO resource and application manifest into the .exe.
    // Only meaningful when targeting Windows; skip silently on other targets.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();
        res.set("FileDescription", "Crimson Desert archive filesystem (WinFSP)");
        res.set("ProductName",     "cdwinfs");
        res.set("OriginalFilename","cdwinfs.exe");
        res.set("InternalName",    "cdwinfs");
        // FileVersion and ProductVersion are set automatically from CARGO_PKG_VERSION.
        res.set_manifest_file("manifest.xml");
        res.compile().expect("winres: failed to compile Windows resources");
    }
}
