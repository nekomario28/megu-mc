// Emit `megu_vanilla_data` when the generated 1.21.1 fixtures the
// vanilla-parity test modules need are checked out. Strict-public exports
// omit them on purpose; modules gated on this cfg are compiled out there
// instead of failing to build or panicking, while every clean-room test
// still runs unconditionally.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(megu_vanilla_data)");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if root.join("assets/block_states_1211.json").is_file()
        && root.join("datapacks/vanilla").is_dir()
    {
        println!("cargo:rustc-cfg=megu_vanilla_data");
    }
    println!("cargo:rerun-if-changed=assets/block_states_1211.json");
    println!("cargo:rerun-if-changed=datapacks/vanilla");
}
