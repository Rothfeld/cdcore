// Compress THIRD_PARTY_LICENSES.md at build time so include_bytes! embeds
// ~25 KB instead of ~228 KB. Decompressed at runtime only on --licenses.
use std::io::Write;

fn main() {
    // Emit /DELAYLOAD:winfsp-x64.dll so the binary doesn't fail to load when
    // WinFSP's bin dir isn't on PATH at process start. Without this, the
    // registry-PATH workaround in main.rs runs too late — Windows has already
    // failed with STATUS_DLL_NOT_FOUND before main is reached.
    winfsp::build::winfsp_link_delayload();

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // cdwinfs has its own THIRD_PARTY_LICENSES.md (includes winfsp deps).
    let src = std::path::Path::new(&manifest).join("THIRD_PARTY_LICENSES.md");
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap())
        .join("licenses.deflate");

    let text = std::fs::read(&src)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", src.display()));

    let mut encoder = flate2::write::DeflateEncoder::new(
        Vec::new(), flate2::Compression::best()
    );
    encoder.write_all(&text).unwrap();
    let compressed = encoder.finish().unwrap();

    std::fs::write(&out, &compressed).unwrap();
    println!("cargo:rerun-if-changed={}", src.display());
    eprintln!(
        "licenses: {} bytes -> {} bytes ({:.0}%)",
        text.len(), compressed.len(),
        compressed.len() as f64 / text.len() as f64 * 100.0
    );
}
