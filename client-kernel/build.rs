// SPDX-License-Identifier: LGPL-3.0-or-later
use std::{env, fs, path::PathBuf};

fn source(path: &str) -> String {
    let path = format!("../src/{path}.rs");
    println!("cargo:rerun-if-changed={path}");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {path}: {e}"))
        .replace("\r\n", "\n")
}

fn split<'a>(source: &'a str, marker: &str) -> (&'a str, &'a str) {
    let (before, after) = source
        .split_once(marker)
        .unwrap_or_else(|| panic!("server source boundary changed: {marker}"));
    assert!(
        !after.contains(marker),
        "ambiguous source boundary: {marker}"
    );
    (before, &source[before.len()..])
}

fn between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    split(split(source, start).1, end).0
}

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let emit = |name, code: String| {
        fs::write(out.join(name), code).expect("write server source projection");
    };

    let main = source("main");
    let palette_src = source("paletted");
    let palette = split(&palette_src, "    pub(crate) fn write_wire(").0;
    let palette_helpers = between(
        &palette_src,
        "\n/// Append `words` big-endian",
        "\npub(crate) fn run_paletted_bench(",
    );
    let varint_len = r#"
#[inline]
fn varint_len_u64(value: u64) -> usize {
    megu_core::varint::varint_i32_len(value as i32)
}
"#;
    let varint = r#"
fn write_varint(out: &mut Vec<u8>, value: i64) {
    megu_core::varint::write_varint_i32(value as i32, out);
}
"#;
    emit(
        "palette.rs",
        format!("{varint}\n{palette}}}\n{varint_len}\n{palette_helpers}\n"),
    );

    let nbt = source("nbt");
    let (read, gzip) = split(&nbt, "\npub(crate) fn read_gzip(");
    let write = between(gzip, "\nfn write_string(", "\n#[cfg(test)]");
    // The module-level documentation is an inner comment, which include! cannot import.
    let (_, read) = read.split_once('\n').expect("NBT module documentation");
    emit("nbt.rs", format!("{read}{write}"));

    let collision = source("entity_collision");
    let (collision_types, _) = split(&collision, "\nimpl EntityAabb {");
    let aabb_impl = between(
        &collision,
        "    fn min(self)",
        "\n/// The 1.21.1 `BlockCollisions`",
    );
    let sweep = between(
        &collision,
        "\n/// 1.21.1 `Shapes.collide`",
        // `entity_collide_world` itself is not spliced (the kernel supplies its
        // own shape source), so stop at its doc comment: a trailing doc comment
        // with no item after it does not compile.
        "\n/// `entity_collide_world_dim`",
    );
    let aabb_struct = between(
        &main,
        "#[derive(Clone, Copy, Debug, PartialEq)]\nstruct EntityAabb",
        "\nimpl EntityAabb {",
    );
    let inflate = between(
        &main,
        "    /// 1.21.1 `AABB.inflate",
        "    /// 1.21.1 `AABB.intersects",
    );
    let overlaps = between(&main, "\nfn aabb_overlaps(", "\nfn aabb_collides_blocks(");

    emit(
        "physics.rs",
        format!(
            "{aabb_struct}\nimpl EntityAabb {{\n{inflate}}}\n{overlaps}\n{collision_types}\nimpl EntityAabb {{\n{aabb_impl}\n{sweep}"
        ),
    );
}
