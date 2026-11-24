// SPDX-License-Identifier: LGPL-3.0-or-later
//! Regression coverage for the composed color+depth pass draw order.
//! Translucent terrain water must be encoded strictly after every
//! depth-writing opaque stream — opaque 6-DOF sub-worlds, GPU-instanced
//! Create mechanical parts and opaque entities/BER — and strictly
//! before the alpha-blended sub-world entries and particles. Encoding
//! water first starves submerged water wheels, cogwheels, shafts and
//! contraptions of the surface blend (the Create × wgpu × shaderpack
//! coexistence ordering bug).

use std::path::Path;

/// Slice `encode_composed_world`'s body out of `renderer.rs` — the one
/// render pass that owns the whole world draw sequence for both the
/// offscreen-composed and live-window paths.
fn composed_world_body() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/renderer.rs");
    let source = std::fs::read_to_string(&path).expect("read renderer.rs");
    let start = source
        .find("fn encode_composed_world(")
        .expect("encode_composed_world must exist");
    let end = source[start..]
        .find("pub fn render_offscreen_composed_frame(")
        .map(|i| start + i)
        .expect("render_offscreen_composed_frame follows encode_composed_world");
    source[start..end].to_string()
}

fn position(body: &str, marker: &str) -> usize {
    body.find(marker)
        .unwrap_or_else(|| panic!("draw-phase marker missing from encode_composed_world: {marker:?}"))
}

#[test]
fn composed_pass_draw_order_contract() {
    let body = composed_world_body();
    // One unique anchor line per draw phase, in source order.
    let opaque_terrain = position(&body, "encode_opaque_terrain(");
    let opaque_subworld = position(&body, "pass.set_pipeline(&lit_opaque_pipeline);");
    let instanced_parts = position(&body, "for batch in subworld_gpu.batches() {");
    let entity_ber = position(&body, "if let Some(mesh) = entity_ber {");
    let translucent_terrain = position(&body, "pass.set_pipeline(&gpu.terrain_alpha_pipeline);");
    let water = position(&body, "pass.set_pipeline(&water.pipeline);");
    let alpha_subworld = position(&body, "pass.set_pipeline(&lit_alpha_pipeline);");
    let particles = position(&body, "if let Some(mesh) = particles {");
    let debug_lines = position(&body, "if let Some((buffer, count)) = debug_lines {");

    // 1. Opaque & cutout terrain (and region arenas) opens the pass.
    assert!(
        opaque_terrain < opaque_subworld,
        "opaque terrain must precede opaque sub-worlds"
    );
    // 2-4. Opaque sub-world entries, instanced mechanical parts and
    // opaque entities/BER all land before the first translucent draw.
    assert!(
        opaque_subworld < instanced_parts,
        "opaque sub-world entries must precede instanced mechanical parts"
    );
    assert!(
        instanced_parts < entity_ber,
        "instanced mechanical parts must precede opaque entities/BER"
    );
    // 5. Translucent terrain and the dedicated water pass draw over the
    // now-complete opaque depth buffer — submerged Create geometry
    // blends correctly.
    assert!(
        entity_ber < translucent_terrain,
        "translucent terrain must follow opaque entities/BER"
    );
    assert!(
        translucent_terrain < water,
        "translucent terrain must precede the water pass"
    );
    // 6. Alpha-blended sub-world entries and particles strictly follow
    // translucent water.
    assert!(
        water < alpha_subworld,
        "alpha-blended sub-world entries must follow translucent water"
    );
    assert!(
        water < particles,
        "alpha particles must follow translucent water"
    );
    assert!(
        alpha_subworld < particles,
        "alpha-blended sub-world entries must precede particles"
    );
    // 7. Debug lines close the pass.
    assert!(particles < debug_lines, "debug lines must draw last");
}
