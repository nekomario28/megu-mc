// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real `naga 30.0.1` WGSL frontend + validator coverage for every shader the
//! builtin Megu shadow pack ships. These tests compile through the same naga
//! crate `wgpu` re-exports, so a syntax or type error that would reject a
//! `wgpu::ShaderModule` at runtime is caught here.

use megu_client::shaders::{TERRAIN_OPAQUE_WGSL, TERRAIN_TRANSLUCENT_WGSL};
use megu_client_gpu::renderer::{
    compact_caster_wgsl, compact_pack_terrain_wgsl, compact_terrain_wgsl, composed_shadow_wgsl,
    region_opaque_wgsl, terrain_wgsl,
};
use megu_client_gpu::subworld_render::{COMPOSITE_LIT_WGSL, COMPOSITE_TERRAIN_WGSL};
use megu_client_mesh::builtin_shadow_wgsl::{
    BUILTIN_SHADOW_SHADERS, MEGU_SHADOW_CASTERS_WGSL, MEGU_SHADOW_CUTOUT_DEPTH_WGSL,
    MEGU_SHADOW_DEPTH_WGSL, MEGU_SHADOW_SAMPLING_WGSL, MEGU_SKY_ATMOSPHERE_WGSL,
    MEGU_SPATIAL_SHARPEN_WGSL, MEGU_TERRAIN_LIT_WGSL, MEGU_WATER_SURFACE_WGSL,
};
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};
use wgpu::naga::ShaderStage;

/// Parse a WGSL module with the real frontend and run the full validator.
/// Both the parse diagnostics and the validation diagnostics are emitted
/// into the panic message so a failure points at the offending line.
fn compile_and_validate(name: &str, source: &str) -> wgpu::naga::Module {
    let module = wgsl::parse_str(source).unwrap_or_else(|err| {
        panic!(
            "{name}: naga WGSL parse failed:\n{}",
            err.emit_to_string(source)
        )
    });
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|err| {
            panic!(
                "{name}: naga validation failed:\n{}",
                err.emit_to_string(source)
            )
        });
    module
}

fn has_entry_point(module: &wgpu::naga::Module, stage: ShaderStage, name: &str) -> bool {
    module
        .entry_points
        .iter()
        .any(|ep| ep.stage == stage && ep.name == name)
}

#[test]
fn every_builtin_megu_shader_compiles_with_real_naga() {
    for (name, source) in [
        ("MEGU_SHADOW_DEPTH_WGSL", MEGU_SHADOW_DEPTH_WGSL),
        ("MEGU_TERRAIN_LIT_WGSL", MEGU_TERRAIN_LIT_WGSL),
        ("MEGU_SKY_ATMOSPHERE_WGSL", MEGU_SKY_ATMOSPHERE_WGSL),
        ("MEGU_WATER_SURFACE_WGSL", MEGU_WATER_SURFACE_WGSL),
        ("MEGU_SPATIAL_SHARPEN_WGSL", MEGU_SPATIAL_SHARPEN_WGSL),
    ] {
        let module = compile_and_validate(name, source);
        assert!(
            has_entry_point(&module, ShaderStage::Vertex, "vs_main"),
            "{name} lost its vertex entry point"
        );
        assert!(
            has_entry_point(&module, ShaderStage::Fragment, "fs_main"),
            "{name} lost its fragment entry point"
        );
    }
}

#[test]
fn shader_table_matches_published_constants() {
    // The loader table must ship exactly the five modules, under the file
    // names the manifest references.
    let table: Vec<(&str, &str)> = BUILTIN_SHADOW_SHADERS.to_vec();
    assert_eq!(table.len(), 5);
    for (path, source) in &table {
        assert!(path.ends_with(".wgsl"));
        let module = compile_and_validate(path, source);
        assert!(!module.entry_points.is_empty());
    }
}

#[test]
fn every_production_shadow_shader_compiles_with_naga() {
    let depth = format!("{MEGU_SHADOW_DEPTH_WGSL}\n{MEGU_SHADOW_CASTERS_WGSL}");
    let module = compile_and_validate("shadow caster pipeline", &depth);
    for entry in ["vs_terrain", "vs_lit", "vs_inst"] {
        assert!(has_entry_point(&module, ShaderStage::Vertex, entry));
    }
    assert!(MEGU_SHADOW_SAMPLING_WGSL.contains("textureSampleCompareLevel"));

    for (name, source, terrain, vs, fs) in [
        (
            "shadowed terrain",
            COMPOSITE_TERRAIN_WGSL,
            true,
            "terrain_vs",
            "terrain_fs",
        ),
        (
            "shadowed lit",
            COMPOSITE_LIT_WGSL,
            false,
            "lit_vs",
            "lit_fs",
        ),
    ] {
        let compiled = composed_shadow_wgsl(source, terrain);
        assert!(
            compiled.contains("shadow_visibility(in.world, n)"),
            "{name} must sample shadows"
        );
        let module = compile_and_validate(name, &compiled);
        assert!(has_entry_point(&module, ShaderStage::Vertex, vs));
        assert!(has_entry_point(&module, ShaderStage::Fragment, fs));
    }
}

#[test]
fn naga_frontend_actually_rejects_broken_wgsl() {
    // Guard against a silently vacuous pass: a module with a real syntax
    // error must fail to parse, and a module that parses but declares an
    // entry point with no output must fail validation checks.
    assert!(wgsl::parse_str("fn oops( {").is_err());
    assert!(wgsl::parse_str("struct A { x: f32").is_err());
    let module = wgsl::parse_str(
        "@vertex fn vs_main() -> @builtin(position) vec4<f32> { return missing_name; }",
    );
    if let Ok(m) = module {
        assert!(
            Validator::new(ValidationFlags::all(), Capabilities::all())
                .validate(&m)
                .is_err(),
            "undeclared identifier must fail validation"
        );
    } // A parse rejection is an equally correct outcome.
}

/// Every terrain pipeline now consumes the 12-byte `CompactVoxelVertex`
/// stream — validate each WGSL variant the renderer builds from the
/// packed sources so a transform regression fails here, not at
/// `create_shader_module` inside a frame.
#[test]
fn every_compact_vertex_shader_variant_validates_with_naga() {
    let cases: Vec<(String, &[&str], &[&str])> = vec![
        (
            compact_terrain_wgsl(&terrain_wgsl(false), None),
            &["terrain_vs", "terrain_translucent_vs"],
            &["terrain_fs", "terrain_translucent_fs"],
        ),
        (
            compact_terrain_wgsl(&terrain_wgsl(true), None),
            &["terrain_vs", "terrain_translucent_vs"],
            &["terrain_fs", "terrain_translucent_fs"],
        ),
        (
            region_opaque_wgsl(&compact_terrain_wgsl(
                &terrain_wgsl(false),
                Some("terrain_vs"),
            )),
            &["terrain_vs", "terrain_translucent_vs"],
            &["terrain_fs", "terrain_translucent_fs"],
        ),
        (
            region_opaque_wgsl(&compact_terrain_wgsl(
                &terrain_wgsl(true),
                Some("terrain_vs"),
            )),
            &["terrain_vs", "terrain_translucent_vs"],
            &["terrain_fs", "terrain_translucent_fs"],
        ),
        (
            composed_shadow_wgsl(&compact_terrain_wgsl(&terrain_wgsl(false), None), true),
            &["terrain_vs", "terrain_translucent_vs"],
            &["terrain_fs", "terrain_translucent_fs"],
        ),
        (
            composed_shadow_wgsl(
                &region_opaque_wgsl(&compact_terrain_wgsl(
                    &terrain_wgsl(false),
                    Some("terrain_vs"),
                )),
                true,
            ),
            &["terrain_vs", "terrain_translucent_vs"],
            &["terrain_fs", "terrain_translucent_fs"],
        ),
        (
            compact_terrain_wgsl(TERRAIN_OPAQUE_WGSL, None),
            &["terrain_opaque_vs"],
            &["terrain_opaque_fs"],
        ),
        (
            region_opaque_wgsl(&compact_terrain_wgsl(
                TERRAIN_OPAQUE_WGSL,
                Some("terrain_opaque_vs"),
            )),
            &["terrain_opaque_vs"],
            &["terrain_opaque_fs"],
        ),
        (
            compact_terrain_wgsl(TERRAIN_TRANSLUCENT_WGSL, None),
            &["terrain_translucent_vs"],
            &["terrain_translucent_fs"],
        ),
        (
            compact_terrain_wgsl(MEGU_SHADOW_CUTOUT_DEPTH_WGSL, None),
            &["vs_cutout"],
            &["fs_cutout"],
        ),
        (
            compact_caster_wgsl(false),
            &["vs_terrain", "vs_lit", "vs_inst"],
            &[],
        ),
        (
            compact_caster_wgsl(true),
            &["vs_terrain", "vs_lit", "vs_inst"],
            &[],
        ),
    ];
    for (source, vertex_entries, fragment_entries) in &cases {
        let probe = source
            .lines()
            .take(40)
            .find(|line| line.contains("word0"))
            .unwrap_or("<none>");
        let name = format!("compact variant ({probe})");
        // The compact stream declares three u32 inputs; the old four-word
        // packed inputs must be gone entirely.
        assert!(
            !source.contains("@location(0) packed_pos: u32"),
            "{name}: 16-byte vertex input survived"
        );
        assert!(
            !source.contains("in.packed_pos") && !source.contains("input.packed_pos"),
            "{name}: stale packed field access survived"
        );
        let module = compile_and_validate(&name, source);
        for entry in *vertex_entries {
            assert!(
                has_entry_point(&module, ShaderStage::Vertex, entry),
                "{name}: lost vertex entry {entry}"
            );
        }
        for entry in *fragment_entries {
            assert!(
                has_entry_point(&module, ShaderStage::Fragment, entry),
                "{name}: lost fragment entry {entry}"
            );
        }
    }
}

/// Pack-authored `gbuffers_terrain` modules are written against the
/// 16-byte four-word contract under arbitrary struct/field naming;
/// `compact_pack_terrain_wgsl` must rewrite any matching vertex input to
/// the 12-byte stream while leaving fragment inputs and non-terrain
/// modules untouched.
#[test]
fn compact_pack_transform_rewrites_terrain_inputs_under_any_names() {
    // Struct input with pack-chosen names, plus a fragment struct whose
    // `in` parameter reuses the vertex body's parameter name — rebinding
    // must stay inside the vertex body (a leaked `cv_packed` reference
    // in `pack_fs` fails validation).
    let struct_pack = r#"
struct PackVertex {
    @location(0) packed_pos: u32,
    @location(1) state_id: u32,
    @location(2) attributes: u32,
    @location(3) tint: u32,
};
struct FragIn {
    @builtin(position) clip: vec4<f32>,
    @location(0) tint: vec4<f32>,
};
@vertex
fn pack_vs(in: PackVertex) -> FragIn {
    let lx = f32(in.packed_pos & 0x1fu);
    var out: FragIn;
    out.clip = vec4<f32>(lx, f32(in.tint & 0xffu), 0.0, 1.0);
    out.tint = vec4<f32>(0.0);
    return out;
}
@fragment
fn pack_fs(in: FragIn) -> @location(0) vec4<f32> {
    return in.tint;
}
"#;
    let rewritten = compact_pack_terrain_wgsl(struct_pack);
    assert!(
        !rewritten.contains("@location(3) tint: u32"),
        "16-byte pack input survived:\n{rewritten}"
    );
    assert!(
        rewritten.contains("@location(0) word0: u32"),
        "compact words were not injected:\n{rewritten}"
    );
    let module = compile_and_validate("pack struct terrain", &rewritten);
    assert!(has_entry_point(&module, ShaderStage::Vertex, "pack_vs"));
    assert!(has_entry_point(&module, ShaderStage::Fragment, "pack_fs"));

    // Free `@location` params (the caster-style signature).
    let free_pack = r#"
@vertex
fn free_vs(@location(0) packed_pos: u32, @location(1) state_id: u32,
    @location(2) attributes: u32, @location(3) tint: u32,
    @builtin(vertex_index) vid: u32) -> @builtin(position) vec4<f32> {
    let lx = f32(packed_pos & 0x1fu) + f32(vid) * 0.0;
    let w = f32(tint & 0xffu) + f32(state_id) + f32(attributes);
    return vec4<f32>(lx, w, 0.0, 1.0);
}
@fragment
fn free_fs() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0);
}
"#;
    let rewritten = compact_pack_terrain_wgsl(free_pack);
    assert!(
        !rewritten.contains("@location(3) tint: u32"),
        "free 16-byte input survived:\n{rewritten}"
    );
    let module = compile_and_validate("pack free-param terrain", &rewritten);
    assert!(has_entry_point(&module, ShaderStage::Vertex, "free_vs"));

    // A module with no packed-terrain vertex input passes through
    // untouched — the device rejection then drives the same fallback the
    // pre-compact path had.
    let untouchable = r#"
@vertex
fn other_vs(@builtin(vertex_index) vid: u32) -> @builtin(position) vec4<f32> {
    return vec4<f32>(f32(vid), 0.0, 0.0, 1.0);
}
@fragment
fn other_fs() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0);
}
"#;
    assert_eq!(compact_pack_terrain_wgsl(untouchable), untouchable);
}
