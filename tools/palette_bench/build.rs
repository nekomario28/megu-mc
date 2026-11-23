use std::{env, fs, path::PathBuf};

fn main() {
    // Import the exact production type and methods, without including the unrelated
    // worldgen benchmark or its repository-only integration tests.
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src/paletted.rs");
    println!("cargo:rerun-if-changed={}", source.display());
    let content = fs::read_to_string(&source)
        .expect("read current production paletted.rs")
        .replace("\r\n", "\n");
    let marker = "\npub(crate) fn run_paletted_bench() {";
    let (implementation, _) = content
        .split_once(marker)
        .expect("production palette benchmark boundary changed; update adapter");
    assert!(implementation.contains("pub(crate) struct PalettedSection"));
    assert!(implementation.contains("pub(crate) fn write_wire"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::write(out.join("production_palette.rs"), implementation).expect("write linked adapter");
}
