// SPDX-License-Identifier: LGPL-3.0-or-later
//! Focused CPU/naga contract tests for the standalone crack overlay pass.

use megu_client_gpu::crack_overlay_pass::{
    crack_uniform, pack_destroy_stage_strip, procedural_destroy_stage_rgba, CrackUniforms,
    CRACK_CUBE_INDICES, CRACK_CUBE_INFLATION, CRACK_CUBE_MAX, CRACK_CUBE_MIN, CRACK_CUBE_VERTICES,
    CRACK_OVERLAY_WGSL,
};
use megu_client_gpu::vanilla_world_assets::{DecodedRgbaTexture, VanillaWorldAssets};
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};
use wgpu::naga::ShaderStage;

fn patterned_stage(stage: u8, width: u32, height: u32) -> DecodedRgbaTexture {
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[stage * 10 + x as u8, y as u8, 100 + stage, 200 + stage]);
        }
    }
    DecodedRgbaTexture {
        width,
        height,
        rgba,
        from_jar: true,
    }
}

fn patterned_assets(width: u32, height: u32) -> VanillaWorldAssets {
    let mut assets = VanillaWorldAssets::fallback();
    assets.destroy_stages =
        std::array::from_fn(|stage| patterned_stage(stage as u8, width, height));
    assets
}

fn strip_pixel(strip: &[u8], strip_width: u32, x: u32, y: u32) -> [u8; 4] {
    let at = (y as usize * strip_width as usize + x as usize) * 4;
    strip[at..at + 4].try_into().expect("RGBA pixel")
}

fn alpha_coverage(rgba: &[u8]) -> usize {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[3] != 0)
        .count()
}

#[test]
fn crack_overlay_wgsl_validates_with_real_naga() {
    let module = wgsl::parse_str(CRACK_OVERLAY_WGSL).unwrap_or_else(|error| {
        panic!(
            "crack overlay WGSL parse failed:\n{}",
            error.emit_to_string(CRACK_OVERLAY_WGSL)
        )
    });
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|error| panic!("crack overlay WGSL validation failed:\n{error}"));

    assert!(module
        .entry_points
        .iter()
        .any(|entry| entry.stage == ShaderStage::Vertex && entry.name == "vs_crack_overlay"));
    assert!(module
        .entry_points
        .iter()
        .any(|entry| entry.stage == ShaderStage::Fragment && entry.name == "fs_crack_overlay"));
}

#[test]
fn destroy_stage_strip_packs_all_ten_stages_left_to_right() {
    let assets = patterned_assets(4, 2);
    let strip = pack_destroy_stage_strip(&assets);

    assert_eq!(strip.tile_width, 4);
    assert_eq!(strip.width, 40);
    assert_eq!(strip.height, 2);
    assert_eq!(strip.rgba.len(), 40 * 2 * 4);
    assert_eq!(strip.fallback_stages, [false; 10]);

    for stage in 0..10u32 {
        for y in 0..2u32 {
            for x in 0..4u32 {
                assert_eq!(
                    strip_pixel(&strip.rgba, strip.width, stage * 4 + x, y),
                    [
                        stage as u8 * 10 + x as u8,
                        y as u8,
                        100 + stage as u8,
                        200 + stage as u8,
                    ],
                    "stage {stage} pixel ({x}, {y})"
                );
            }
        }
    }
}

#[test]
fn blank_destroy_stage_uses_visible_procedural_fallback_in_strip() {
    let mut assets = patterned_assets(4, 2);
    assets.destroy_stages[3] = DecodedRgbaTexture {
        width: 4,
        height: 2,
        rgba: vec![0; 4 * 2 * 4],
        from_jar: false,
    };

    let strip = pack_destroy_stage_strip(&assets);
    assert_eq!(strip.width, 40);
    assert!(strip.fallback_stages[3]);
    assert_eq!(
        strip.fallback_stages.iter().filter(|used| **used).count(),
        1
    );
    let fallback_alpha = (0..2u32)
        .flat_map(|y| (12..16u32).map(move |x| (x, y)))
        .filter(|(x, y)| strip_pixel(&strip.rgba, strip.width, *x, *y)[3] != 0)
        .count();
    assert!(fallback_alpha > 0, "blank stage must become visible");
    assert_eq!(
        strip_pixel(&strip.rgba, strip.width, 2 * 4 + 1, 1),
        [21, 1, 102, 202],
        "neighbouring real stages must remain exact"
    );
}

#[test]
fn procedural_fallback_cracks_increase_in_nested_coverage() {
    let mut previous_count = 0;
    let mut previous_active = vec![false; 16 * 16];
    for stage in 0..10u8 {
        let rgba = procedural_destroy_stage_rgba(stage, 16, 16);
        assert_eq!(rgba.len(), 16 * 16 * 4);
        let count = alpha_coverage(&rgba);
        assert!(count > 0, "stage {stage} must not be empty");
        assert!(
            count > previous_count,
            "stage {stage} coverage {count} must exceed previous {previous_count}"
        );
        for (index, was_active) in previous_active.iter().copied().enumerate() {
            assert!(
                !was_active || rgba[index * 4 + 3] != 0,
                "stage {stage} must retain every crack pixel from earlier stages"
            );
        }
        previous_count = count;
        previous_active = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[3] != 0)
            .collect();
    }

    let stage9 = procedural_destroy_stage_rgba(9, 16, 16);
    for y in 0..16usize {
        let row_coverage = (0..16usize)
            .filter(|x| stage9[(y * 16 + x) * 4 + 3] != 0)
            .count();
        assert!(
            row_coverage < 16,
            "fracture stage 9 must not fill scanline row {y}"
        );
    }
}

#[test]
fn loader_fallback_assets_are_replaced_with_increasing_fracture_fallback() {
    let assets = VanillaWorldAssets::fallback();
    let strip = pack_destroy_stage_strip(&assets);
    assert_eq!(strip.width, 160);
    assert_eq!(strip.height, 16);
    assert_eq!(strip.fallback_stages, [true; 10]);

    let mut previous = 0;
    for stage in 0..10u32 {
        let count = (0..16u32)
            .flat_map(|y| (stage * 16..stage * 16 + 16).map(move |x| (x, y)))
            .filter(|(x, y)| strip_pixel(&strip.rgba, strip.width, *x, *y)[3] != 0)
            .count();
        assert!(count > previous, "stage {stage} coverage must increase");
        previous = count;
    }
}

#[test]
fn uniform_writer_clamps_stage_and_stays_exactly_80_bytes() {
    assert_eq!(std::mem::size_of::<CrackUniforms>(), 80);
    let identity = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let uniforms = crack_uniform(identity, ([-12, 64, 130], 255));
    assert_eq!(uniforms.view_proj, identity);
    assert_eq!(uniforms.block_origin_stage, [-12.0, 64.0, 130.0, 9.0]);
    assert_eq!(
        crack_uniform(identity, ([0, 0, 0], 10)).block_origin_stage[3],
        9.0
    );
    assert_eq!(
        crack_uniform(identity, ([0, 0, 0], 9)).block_origin_stage[3],
        9.0
    );
}

#[test]
fn crack_cube_is_six_full_faces_inflated_to_vanilla_overlay_bounds() {
    assert_eq!(CRACK_CUBE_VERTICES.len(), 24);
    assert_eq!(CRACK_CUBE_INDICES.len(), 36);
    assert_eq!(CRACK_CUBE_INFLATION, 0.003);
    assert_eq!(CRACK_CUBE_MIN, -0.003);
    assert_eq!(CRACK_CUBE_MAX, 1.003);

    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for vertex in CRACK_CUBE_VERTICES {
        for axis in 0..3 {
            min[axis] = min[axis].min(vertex.position[axis]);
            max[axis] = max[axis].max(vertex.position[axis]);
        }
        assert!(vertex.uv[0] >= 0.0 && vertex.uv[0] <= 1.0);
        assert!(vertex.uv[1] >= 0.0 && vertex.uv[1] <= 1.0);
    }
    assert_eq!(min, [-0.003; 3]);
    assert_eq!(max, [1.003; 3]);
    assert!(CRACK_CUBE_INDICES.iter().all(|index| *index < 24));

    for (face, (axis, outward_sign)) in [
        (1, -1.0),
        (1, 1.0),
        (2, -1.0),
        (2, 1.0),
        (0, -1.0),
        (0, 1.0),
    ]
    .into_iter()
    .enumerate()
    {
        let base = face * 6;
        let triangle = [
            CRACK_CUBE_INDICES[base] as usize,
            CRACK_CUBE_INDICES[base + 1] as usize,
            CRACK_CUBE_INDICES[base + 2] as usize,
        ];
        let a = CRACK_CUBE_VERTICES[triangle[0]].position;
        let b = CRACK_CUBE_VERTICES[triangle[1]].position;
        let c = CRACK_CUBE_VERTICES[triangle[2]].position;
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let normal = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        assert!(
            normal[axis] * outward_sign > 0.0,
            "face {face} must wind outward for back-face culling: {normal:?}"
        );
    }
}
