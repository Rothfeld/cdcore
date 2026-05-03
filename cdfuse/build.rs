fn main() {
    // On Windows: resolve winfsp-x64.dll at runtime via the WinFsp registry key
    // (HKLM\SOFTWARE\WinFsp\InstallDir) rather than requiring it on PATH.
    // WinFsp must be installed: https://winfsp.dev/rel/
    #[cfg(windows)]
    winfsp_build::winfsp_link_delayload();
}
