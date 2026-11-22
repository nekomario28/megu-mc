// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real `naga 30.0.1` WGSL frontend + validator coverage for every shader the
//! builtin Megu shadow pack ships. These tests compile through the same naga
//! crate `wgpu` re-exports, so a syntax or type error that would reject a
//! `wgpu::ShaderModule` at runtime is caught here.

use mc_spine_client_gpu::renderer::composed_shadow_wgsl;
use mc_spine_client_gpu::subworld_render::{COMPOSITE_LIT_WGSL, COMPOSITE_TERRAIN_WGSL};
use mc_spine_client_mesh::builtin_shadow_wgsl::{
    BUILTIN_SHADOW_SHADERS, MEGU_SHADOW_CASTERS_WGSL, MEGU_SHADOW_DEPTH_WGSL,
    MEGU_SHADOW_SAMPLING_WGSL, MEGU_SKY_ATMOSPHERE_WGSL, MEGU_SPATIAL_SHARPEN_WGSL,
    MEGU_TERRAIN_LIT_WGSL, MEGU_WATER_SURFACE_WGSL,
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
