// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
use std::{env, fs, path::PathBuf};

fn source(path: &str) -> String {
    let path = format!("../src/{path}.rs");
    println!("cargo:rerun-if-changed={path}");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
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
    let protocol = source("play_protocol");
    let varint = between(&protocol, "\nfn write_varint(", "\nfn write_str(");
    let palette_src = source("paletted");
    let palette = split(&palette_src, "    pub(crate) fn write_wire(").0;
    let palette_helpers = between(
        &palette_src,
        "\n/// VarInt byte count",
        "\npub(crate) fn run_paletted_bench(",
    );
    emit("palette.rs", format!("{varint}\n{palette}}}\n{palette_helpers}\n"));

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
        "\nfn entity_collide_world(",
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

    let fluid = source("entity_fluid");
    let fluid_types = between(&fluid, "\n// `Entity.bn()`", "\n/// Maps a block state ID");
    let fluid_height = between(
        &fluid,
        "\n/// 1.21.1 `FlowingFluid.getOwnHeight",
        "\n/// 1.21.1 `FlowingFluid.getHeight",
    );
    let fluid_vec = between(
        &fluid,
        "\n/// 1.21.1 `Vec3.normalize()",
        "\nfn fluid_blocks_motion(",
    );
    let fluid_flow = between(
        &fluid,
        "\n/// 1.21.1 `FlowingFluid.getFlow",
        "\n/// 1.21.1 `Entity.updateFluidOnEyes",
    );
    let scale = between(
        &fluid,
        "\n/// `Entity.bn()`",
        "\n/// `Entity.getFluidJumpThreshold",
    );
    let solid_face = between(
        &fluid,
        "\n/// 1.21.1 `FlowingFluid.isSolidFace",
        "    if let Some(reg) = crate::block_registry::breg()",
    );
    // Preserve the same-fluid and upward-face rules, and read the remaining
    // BReg facts directly from the query cell.
    let solid_face =
        format!("{solid_face}    !st.is_ice && (st.sturdy_faces & (1 << dir_idx)) != 0\n}}\n");
    // Switch the lookup value from a BReg state ID to the caller's per-cell facts.
    let fluid_query = format!("{fluid_height}{solid_face}{fluid_flow}");
    let block_lookup = "get_block: &impl Fn(i32, i32, i32) -> u32";
    assert_eq!(fluid_query.matches(block_lookup).count(), 5);
    let fluid_query = fluid_query.replace(
        block_lookup,
        "get_block: &impl Fn(i32, i32, i32) -> FluidCell",
    );
    emit(
        "physics.rs",
        format!(
            "{aabb_struct}\nimpl EntityAabb {{\n{inflate}}}\n{overlaps}\n{collision_types}\nimpl EntityAabb {{\n{aabb_impl}\n{sweep}\n{fluid_types}\n{fluid_vec}\n{fluid_query}\n{scale}"
        ),
    );
}
