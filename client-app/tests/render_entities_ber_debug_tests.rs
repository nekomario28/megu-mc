// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Coverage for the additive overlay renderers in `megu_client::render`:
//! articulated entity rigs, vanilla BER meshes, billboard particles, and the
//! F3+G chunk-border / F3+B hitbox debug wireframes — all composited into
//! deterministic software-framebuffer PNG captures.

use std::collections::HashSet;

use megu_client::render::{
    render_debug_lines, render_entity_rig, render_entity_rig_facing, render_vanilla_ber,
    render_vanilla_ber_state, render_vanilla_particles, FrameBuffer, OverlayCamera,
};
use megu_client_mesh::ber_instancing::{
    VanillaBerFacing, VanillaBerKind, VanillaBerState, VanillaParticleKind, VanillaParticleWorld,
};
use megu_client_mesh::entity_rig::EntityPose;
use megu_kernel::debug_chords::{build_chunk_border_lines, build_entity_hitbox_lines, DebugLine};
use megu_kernel::Aabb;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

/// Eye at [8, 5, -8] looking down +Z: chunk (0,0) borders, a zombie and a
/// chest all land inside the frustum.
fn camera() -> OverlayCamera {
    OverlayCamera {
        eye: [8.0, 5.0, -8.0],
        yaw: 0.0,
        pitch: 0.0,
    }
}

fn empty_framebuffer() -> FrameBuffer {
    FrameBuffer::new(WIDTH, HEIGHT, 70.0)
}

/// Pixels that differ from an untouched sky-gradient framebuffer.
fn diff_pixels(frame: &FrameBuffer, baseline: &FrameBuffer) -> usize {
    frame
        .rgb()
        .as_chunks::<3>()
        .0
        .iter()
        .zip(baseline.rgb().as_chunks::<3>().0.iter())
        .filter(|(a, b)| a != b)
        .count()
}

fn distinct_diff_colors(frame: &FrameBuffer, baseline: &FrameBuffer) -> usize {
    frame
        .rgb()
        .as_chunks::<3>()
        .0
        .iter()
        .zip(baseline.rgb().as_chunks::<3>().0.iter())
        .filter(|(a, b)| a != b)
        .map(|(a, _)| *a)
        .collect::<HashSet<_>>()
        .len()
}

fn has_exact_pixel(frame: &FrameBuffer, rgb: [u8; 3]) -> bool {
    frame.rgb().as_chunks::<3>().0.contains(&rgb)
}

/// Zombie (protocol id 124, `Biped`) left of center, a chest right of
/// center, the F3+G chunk-border grid, and an enlarged F3+B hitbox around
/// the zombie — one deterministic capture exercising every overlay kind.
fn scene_framebuffer() -> FrameBuffer {
    let mut fb = empty_framebuffer();
    let cam = camera();

    render_entity_rig(&mut fb, cam, 124, [10.0, 4.0, 6.0], &EntityPose::default());
    render_vanilla_ber(&mut fb, cam, &VanillaBerKind::Chest, [6.0, 4.0, 9.0]);

    let borders = build_chunk_border_lines(0, 0, 0, 8);
    render_debug_lines(&mut fb, cam, &borders);

    // Deliberately larger than the 0.6x1.95 collision box so the wireframe
    // edges land on sky instead of inside the mob silhouette.
    let hitbox = build_entity_hitbox_lines(
        Aabb {
            min: [9.4, 3.8, 5.4],
            max: [10.6, 6.2, 6.6],
        },
        5.62,
        0.0,
        0.0,
        true,
    );
    render_debug_lines(&mut fb, cam, &hitbox);
    fb
}

#[test]
fn framebuffer_renders_mob_ber_and_debug_wireframes() {
    let fb = scene_framebuffer();
    let baseline = empty_framebuffer();

    // Every overlay family contributed pixels.
    let changed = diff_pixels(&fb, &baseline);
    assert!(
        changed > 500,
        "expected hundreds of overlay pixels, got {changed}"
    );
    assert!(
        distinct_diff_colors(&fb, &baseline) >= 8,
        "expected many shaded/tinted colors, got {}",
        distinct_diff_colors(&fb, &baseline)
    );

    // Crisp wireframe proof: opaque debug colors land byte-exact.
    // YELLOW subdivision pillars (F3+G), WHITE hitbox edges and RED
    // living-entity eye line (F3+B).
    for color in [[255, 255, 0], [255, 255, 255], [255, 0, 0]] {
        assert!(
            has_exact_pixel(&fb, color),
            "missing exact debug color {color:?}"
        );
    }

    // Lit geometry actually shaded: triangles and shaded pixels counted.
    let stats = fb.stats();
    assert!(stats.triangles_drawn > 0, "no overlay triangles rasterized");
    assert!(stats.pixels_shaded > 0, "no overlay pixels shaded");

    // The PNG path accepts the framebuffer output unchanged.
    let png = fb.encode_png();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn overlay_frame_is_byte_identical_across_repeated_renders() {
    let a = scene_framebuffer();
    let b = scene_framebuffer();
    assert_eq!(a.rgb(), b.rgb(), "rgb buffers must be deterministic");
    assert_eq!(a.encode_png(), b.encode_png(), "PNG bytes must match");

    // Facing/yaw variants are deterministic too.
    let mut c = empty_framebuffer();
    let mut d = empty_framebuffer();
    let cam = camera();
    let pose = EntityPose {
        limb_swing: 0.7,
        limb_swing_amount: 0.9,
        head_yaw_deg: 35.0,
        ..EntityPose::default()
    };
    render_entity_rig_facing(&mut c, cam, 124, [10.0, 4.0, 6.0], &pose, 1.25);
    render_entity_rig_facing(&mut d, cam, 124, [10.0, 4.0, 6.0], &pose, 1.25);
    assert_eq!(c.rgb(), d.rgb());
    assert_ne!(c.rgb(), empty_framebuffer().rgb());
}

#[test]
fn framebuffer_renders_vanilla_particles() {
    let mut particles = VanillaParticleWorld::new();
    for i in 0..12 {
        assert!(particles.spawn(
            VanillaParticleKind::Explosion,
            [
                9.0 + (i % 3) as f32 * 0.4,
                5.0 + (i / 3) as f32 * 0.3,
                7.0,
            ],
            [0.0, 0.5, 0.0],
            [1.0, 0.45, 0.1, 0.9],
            4.0,
        ));
    }

    let mut fb = empty_framebuffer();
    render_vanilla_particles(&mut fb, camera(), &particles);
    let baseline = empty_framebuffer();
    assert!(
        diff_pixels(&fb, &baseline) > 20,
        "expected translucent particle pixels, got {}",
        diff_pixels(&fb, &baseline)
    );

    // Deterministic: same pool, same frame, same bytes.
    let mut again = empty_framebuffer();
    render_vanilla_particles(&mut again, camera(), &particles);
    assert_eq!(fb.rgb(), again.rgb());
}

#[test]
fn overlay_renderers_fail_closed_on_invalid_input() {
    let cam = camera();
    let mut fb = empty_framebuffer();
    let before = fb.rgb().to_vec();

    // Unknown entity id, NaN entity position, NaN BER position.
    render_entity_rig(&mut fb, cam, u16::MAX, [8.0, 4.0, 4.0], &EntityPose::default());
    render_entity_rig(
        &mut fb,
        cam,
        124,
        [f32::NAN, 4.0, 4.0],
        &EntityPose::default(),
    );
    render_vanilla_ber(&mut fb, cam, &VanillaBerKind::Chest, [f32::NAN; 3]);

    // Degenerate debug lines: NaN endpoints, zero alpha, fully behind the
    // camera (camera looks +Z from z=-8).
    render_debug_lines(
        &mut fb,
        cam,
        &[
            DebugLine {
                start: [f32::NAN; 3],
                end: [1.0; 3],
                rgba: [255, 0, 0, 255],
            },
            DebugLine {
                start: [0.0; 3],
                end: [0.0; 3],
                rgba: [0, 0, 0, 0],
            },
            DebugLine {
                start: [8.0, 5.0, -20.0],
                end: [9.0, 5.0, -20.0],
                rgba: [0, 255, 0, 255],
            },
        ],
    );
    assert_eq!(fb.rgb(), before.as_slice(), "invalid input drew pixels");

    // A non-finite pose is sanitized by the rig builder and still renders.
    render_entity_rig(
        &mut fb,
        cam,
        124,
        [10.0, 4.0, 6.0],
        &EntityPose {
            limb_swing: f32::NAN,
            head_yaw_deg: f32::INFINITY,
            age_ticks: f32::NAN,
            ..EntityPose::default()
        },
    );
    assert_ne!(fb.rgb(), before.as_slice(), "NaN pose should still draw");

    // A segment crossing the near plane clips cleanly and draws its
    // surviving front portion.
    let mut clipped = empty_framebuffer();
    render_debug_lines(
        &mut clipped,
        cam,
        &[DebugLine {
            start: [10.0, 5.0, -20.0],
            end: [10.0, 5.0, 20.0],
            rgba: [255, 255, 0, 255],
        }],
    );
    assert!(
        diff_pixels(&clipped, &empty_framebuffer()) > 0,
        "near-plane-clipped line drew nothing"
    );
    assert!(has_exact_pixel(&clipped, [255, 255, 0]));

    // BER state variant and a second BER family also draw.
    let mut shulker = empty_framebuffer();
    render_vanilla_ber_state(
        &mut shulker,
        cam,
        VanillaBerState::ShulkerBox {
            open_progress: 0.6,
            facing: VanillaBerFacing::North,
        },
        [5.0, 4.0, 10.0],
        [120, 200, 90],
    );
    assert!(
        diff_pixels(&shulker, &empty_framebuffer()) > 0,
        "shulker box state drew nothing"
    );

    let mut ber = empty_framebuffer();
    render_vanilla_ber(&mut ber, cam, &VanillaBerKind::Lectern, [12.0, 4.0, 8.0]);
    render_vanilla_ber(&mut ber, cam, &VanillaBerKind::Chest, [6.0, 4.0, 9.0]);
    assert!(
        diff_pixels(&ber, &empty_framebuffer()) > 50,
        "BER family meshes drew too few pixels: {}",
        diff_pixels(&ber, &empty_framebuffer())
    );

    // Degenerate framebuffer dimensions cannot panic.
    let tiny = FrameBuffer::new(0, 0, f64::NAN);
    assert_eq!((tiny.width(), tiny.height()), (1, 1));
    render_entity_rig(&mut FrameBuffer::new(1, 1, 70.0), cam, 124, [10.0, 4.0, 6.0], &EntityPose::default());
}
