// intel_tex_2 -> ispc_rt requires the C++ exception personality symbol.
// lib_fix/ contains a symlink to the versioned libstdc++.so in the devcontainer.
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search={manifest}/lib_fix");
    println!("cargo:rustc-link-lib=stdc++");
}
