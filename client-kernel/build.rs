// SPDX-License-Identifier: LGPL-3.0-or-later
// Projects the server sources the kernel still shares byte-for-byte.
//
// A family that has moved to a typed `megu_core` module is no longer spliced
// here. `paletted` was the first: the kernel now depends on
// `megu_core::paletted` directly and nothing in this file touches it. `nbt`
// followed: the codec lives in `megu_core::nbt` and the kernel calls it, so no
// projection is cut for it either. `entity_collision` (plus the `EntityAabb`
// pieces out of `src/main.rs`) is the remaining string splice; it disappears
// the same way, by moving behind a core API instead of being re-sliced at
// build time.
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
