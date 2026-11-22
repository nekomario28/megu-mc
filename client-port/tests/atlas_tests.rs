// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::collections::BTreeMap;

use megu_client_mesh::atlas::*;
use megu_client_mesh::model_baker::BakedModelQuad;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A solid-colour RGBA8 sprite buffer, `width * height * 4` bytes.
fn solid_rgba(width: u32, height: u32, px: [u8; 4]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for _ in 0..width * height {
        rgba.extend_from_slice(&px);
    }
    rgba
}

/// `(x, y, w, h)` rectangle overlap test.
fn overlaps(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

fn placement_rect(p: &SpritePlacement) -> (u32, u32, u32, u32) {
    (p.x, p.y, p.width, p.height)
}

fn atlas_px(atlas: &PackedAtlas, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * atlas.width + x) * 4) as usize;
    atlas.rgba[i..i + 4].try_into().unwrap()
}

fn assert_f32_eq(actual: f32, expected: f32) {
    assert!((actual - expected).abs() <= 1e-6, "{actual} != {expected}");
}

fn assert_rect_eq(actual: [f32; 4], expected: [f32; 4]) {
    for (a, e) in actual.iter().zip(expected.iter()) {
        assert_f32_eq(*a, *e);
    }
}

fn assert_uv_eq(actual: [f32; 2], expected: [f32; 2]) {
    assert_f32_eq(actual[0], expected[0]);
    assert_f32_eq(actual[1], expected[1]);
}

// ---------------------------------------------------------------------------
// Atlas packing
// ---------------------------------------------------------------------------

#[test]
fn packs_mixed_sizes_with_padding_deterministically() {
    let sprites: [(&str, u32, u32, [u8; 4]); 5] = [
        ("tiny_8", 8, 8, [10, 20, 30, 255]),
        ("block_16", 16, 16, [40, 50, 60, 255]),
        ("wide_32x16", 32, 16, [70, 80, 90, 128]),
        ("big_64", 64, 64, [200, 210, 220, 255]),
        ("tall_16x32", 16, 32, [1, 2, 3, 4]),
    ];

    let mut forward = AtlasBuilder::new();
    forward.set_padding(2).unwrap();
    for (name, w, h, px) in sprites {
        forward.add(name, w, h, solid_rgba(w, h, px)).unwrap();
    }
    let atlas_a = forward.build().unwrap();

    // Same sprites, different insertion order: the deterministic sort must
    // produce a byte-identical atlas.
    let mut reverse = AtlasBuilder::new();
    reverse.set_padding(2).unwrap();
    for &(name, w, h, px) in sprites.iter().rev() {
        reverse.add(name, w, h, solid_rgba(w, h, px)).unwrap();
    }
    let atlas_b = reverse.build().unwrap();

    assert_eq!(atlas_a.width, atlas_b.width);
    assert_eq!(atlas_a.height, atlas_b.height);
    assert_eq!(atlas_a.rgba, atlas_b.rgba);
    assert_eq!(atlas_a.placements, atlas_b.placements);

    // Power-of-two bounds within the configured cap.
    assert!(atlas_a.width.is_power_of_two());
    assert!(atlas_a.height.is_power_of_two());
    assert!(atlas_a.width <= DEFAULT_ATLAS_MAX_DIM);
    assert!(atlas_a.height <= DEFAULT_ATLAS_MAX_DIM);
    assert_eq!(
        atlas_a.rgba.len(),
        atlas_a.width as usize * atlas_a.height as usize * 4
    );
    assert_eq!(atlas_a.padding, 2);

    // Every placement inside bounds; zero pairwise pixel-rect overlaps.
    let placements: Vec<&SpritePlacement> = atlas_a.placements.values().collect();
    assert_eq!(placements.len(), sprites.len());
    let pixel_rects: Vec<_> = placements.iter().map(|p| placement_rect(p)).collect();
    for (i, a) in pixel_rects.iter().enumerate() {
        assert!(a.0 + a.2 <= atlas_a.width && a.1 + a.3 <= atlas_a.height);
        for b in &pixel_rects[i + 1..] {
            assert!(!overlaps(*a, *b), "placements {a:?} and {b:?} overlap");
        }
    }

    // The packer's cells are the placements inflated by `padding`; those
    // must be disjoint too, which is what stops mipmap bleed.
    let pad = atlas_a.padding;
    let cells: Vec<_> = pixel_rects
        .iter()
        .map(|&(x, y, w, h)| (x - pad, y - pad, w + 2 * pad, h + 2 * pad))
        .collect();
    for (i, a) in cells.iter().enumerate() {
        for b in &cells[i + 1..] {
            assert!(!overlaps(*a, *b), "padded cells {a:?} and {b:?} overlap");
        }
    }

    // Pixel-exact copy checks.
    let big = atlas_a.placement("big_64").unwrap();
    assert_eq!(atlas_px(&atlas_a, big.x, big.y), [200, 210, 220, 255]);
    assert_eq!(
        atlas_px(&atlas_a, big.x + 63, big.y + 63),
        [200, 210, 220, 255]
    );
    // The texel inside the sprite's padding border stays transparent.
    assert_eq!(atlas_px(&atlas_a, big.x, big.y - 1), [0, 0, 0, 0]);
    // Atlas corner is uncovered padding/background.
    assert_eq!(atlas_px(&atlas_a, 0, 0), [0, 0, 0, 0]);

    let tall = atlas_a.placement("tall_16x32").unwrap();
    assert_eq!(atlas_px(&atlas_a, tall.x + 5, tall.y + 17), [1, 2, 3, 4]);

    // uv_rect matches the placement in normalized atlas space.
    let rect = atlas_a.uv_rect("block_16").unwrap();
    let p = atlas_a.placement("block_16").unwrap();
    assert_f32_eq(rect[0], p.x as f32 / atlas_a.width as f32);
    assert_f32_eq(rect[2], (p.x + p.width) as f32 / atlas_a.width as f32);
}

// ---------------------------------------------------------------------------
// Alpha-weighted mipmaps
// ---------------------------------------------------------------------------

#[test]
fn mipmaps_weight_color_by_alpha_without_black_bleed() {
    // 4x4 atlas: a column of opaque pure-green texels on the left edge,
    // fully transparent texels on the right — the cutout-leaf situation.
    let mut rgba = vec![0u8; 4 * 4 * 4];
    for y in 0..4u32 {
        let i = ((y * 4 + 1) * 4) as usize;
        rgba[i..i + 4].copy_from_slice(&[0, 255, 0, 255]);
    }
    let atlas = PackedAtlas {
        width: 4,
        height: 4,
        padding: 0,
        rgba,
        placements: BTreeMap::new(),
    };

    let mips = build_mipmaps(&atlas, 2).unwrap();
    assert_eq!(mips.len(), 2);
    assert_eq!((mips[0].level, mips[0].width, mips[0].height), (1, 2, 2));
    assert_eq!((mips[1].level, mips[1].width, mips[1].height), (2, 1, 1));

    // Every texel that kept any alpha must be pure green. A naive box
    // filter would dilute the green to ~64; alpha weighting preserves 255.
    for mip in &mips {
        for px in mip.rgba.as_chunks::<4>().0 {
            if px[3] > 0 {
                assert_eq!(
                    &px[..3],
                    &[0, 255, 0],
                    "mip level {} texel picked up a bleed colour",
                    mip.level
                );
            }
        }
    }

    // Level 1: the two left-column texels each saw two opaque green source
    // texels; the two right-column texels saw only transparency and must
    // stay exactly (0, 0, 0, 0) — no green may bleed sideways.
    let level1 = &mips[0].rgba;
    let lit = level1
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px[3] > 0)
        .count();
    assert_eq!(lit, 2);
    assert_eq!(&level1[4..8], &[0, 0, 0, 0]);
    assert_eq!(&level1[12..16], &[0, 0, 0, 0]);

    // Level 2: the single texel still resolves to pure green with partial
    // alpha — no dark fringe anywhere in the chain.
    let top = &mips[1].rgba[0..4];
    assert_eq!(&top[..3], &[0, 255, 0]);
    assert!(top[3] > 0 && top[3] < 255);
}

#[test]
fn build_mipmaps_rejects_overlong_chains_and_malformed_atlases() {
    let atlas = PackedAtlas {
        width: 4,
        height: 4,
        padding: 0,
        rgba: vec![0u8; 4 * 4 * 4],
        placements: BTreeMap::new(),
    };
    // 4x4 supports exactly two halvings (4 -> 2 -> 1).
    assert!(matches!(
        build_mipmaps(&atlas, 3),
        Err(AtlasError::TooManyMipLevels {
            requested: 3,
            max: 2
        })
    ));
    assert!(build_mipmaps(&atlas, 2).is_ok());
    assert!(build_mipmaps(&atlas, 0).unwrap().is_empty());

    let malformed = PackedAtlas {
        width: 4,
        height: 4,
        padding: 0,
        rgba: vec![0u8; 10],
        placements: BTreeMap::new(),
    };
    assert!(matches!(
        build_mipmaps(&malformed, 0),
        Err(AtlasError::MalformedAtlas { .. })
    ));

    // Absurd hand-built dimensions must fail closed, never overflow.
    let absurd = PackedAtlas {
        width: u32::MAX,
        height: u32::MAX,
        padding: 0,
        rgba: Vec::new(),
        placements: BTreeMap::new(),
    };
    assert!(matches!(
        build_mipmaps(&absurd, 0),
        Err(AtlasError::MalformedAtlas { .. })
    ));
}

// ---------------------------------------------------------------------------
// `.mcmeta` animation tables
// ---------------------------------------------------------------------------

#[test]
fn animation_strip_resolves_frames_and_blends_across_ticks() {
    // 16px square frames, four-high strip, 2 ticks per frame, interpolated.
    let anim = SpriteAnimation::strip(16, 64, 2, true).unwrap();
    assert_eq!(anim.frame_count(), 4);
    assert_eq!(anim.strip_frames(), 4);
    assert_eq!(anim.frame_size(), 16);
    assert_eq!(anim.strip_height(), 64);
    assert_eq!(anim.total_ticks(), 8);
    assert!(anim.interpolate());

    let cases: [(u64, usize, usize, f32); 7] = [
        (0, 0, 1, 0.0),
        (1, 0, 1, 0.5),
        (2, 1, 2, 0.0),
        (3, 1, 2, 0.5),
        (7, 3, 0, 0.5), // last frame blends cyclically back into the first
        (8, 0, 1, 0.0), // wraps
        (15, 3, 0, 0.5),
    ];
    for (tick, cur, next, alpha) in cases {
        let (c, n, a) = anim.active_frame_at_tick(tick);
        assert_eq!((c, n), (cur, next), "tick {tick}");
        assert_f32_eq(a, alpha);
    }

    // Non-interpolated animations always report zero blend.
    let still = SpriteAnimation::strip(16, 32, 5, false).unwrap();
    let (c, n, a) = still.active_frame_at_tick(6);
    assert_eq!((c, n), (1, 0));
    assert_f32_eq(a, 0.0);
}

#[test]
fn animation_custom_frame_table_and_slot_rects() {
    let custom = SpriteAnimation::with_frames(
        16,
        64,
        vec![
            AnimFrame { index: 2, ticks: 3 },
            AnimFrame { index: 0, ticks: 1 },
            AnimFrame { index: 2, ticks: 2 },
        ],
        true,
    )
    .unwrap();
    assert_eq!(custom.total_ticks(), 6);
    assert_eq!(custom.strip_frames(), 4);

    // Sequence indices resolve the playback table; `.index` names the slot.
    let (c, n, a) = custom.active_frame_at_tick(3);
    assert_eq!((c, n), (1, 2));
    assert_f32_eq(a, 0.0);
    assert_eq!(custom.frames()[c].index, 0);

    let (c, _, a) = custom.active_frame_at_tick(5);
    assert_eq!(c, 2);
    assert_f32_eq(a, 0.5);
    assert_eq!(custom.frames()[c].index, 2);

    // Strip slot UV rects inside the sprite image.
    assert_rect_eq(custom.slot_rect(0).unwrap(), [0.0, 0.0, 1.0, 0.25]);
    assert_rect_eq(custom.slot_rect(3).unwrap(), [0.0, 0.75, 1.0, 1.0]);
    assert!(custom.slot_rect(4).is_none());
}

#[test]
fn animation_tables_fail_closed() {
    assert!(matches!(
        SpriteAnimation::strip(0, 16, 1, false),
        Err(AnimationError::ZeroFrameSize)
    ));
    assert!(matches!(
        SpriteAnimation::strip(16, 20, 1, false),
        Err(AnimationError::InvalidStrip { .. })
    ));
    assert!(matches!(
        SpriteAnimation::strip(16, 16, 0, false),
        Err(AnimationError::ZeroFrameDuration { .. })
    ));
    assert!(matches!(
        SpriteAnimation::with_frames(16, 16, Vec::new(), false),
        Err(AnimationError::EmptyFrameTable)
    ));
    // A 16/16 strip holds exactly one slot; index 1 is out of range.
    assert!(matches!(
        SpriteAnimation::with_frames(16, 16, vec![AnimFrame { index: 1, ticks: 1 }], false),
        Err(AnimationError::FrameIndexOutOfRange { .. })
    ));
    assert!(matches!(
        SpriteAnimation::with_frames(16, 32, vec![AnimFrame { index: 0, ticks: 0 }], false),
        Err(AnimationError::ZeroFrameDuration { .. })
    ));
    // Overlong implicit table from a huge strip.
    assert!(matches!(
        SpriteAnimation::strip(1, (MAX_ANIMATION_FRAMES as u32) + 2, 1, false),
        Err(AnimationError::TooManyFrames(_))
    ));
    // Explicit table over the cap.
    assert!(matches!(
        SpriteAnimation::with_frames(
            16,
            16,
            vec![AnimFrame { index: 0, ticks: 1 }; MAX_ANIMATION_FRAMES + 1],
            false
        ),
        Err(AnimationError::TooManyFrames(_))
    ));
}

// ---------------------------------------------------------------------------
// Baked-quad UV remapping + biome tint
// ---------------------------------------------------------------------------

fn stone_atlas() -> PackedAtlas {
    let mut builder = AtlasBuilder::new();
    builder
        .add("block/stone", 16, 16, solid_rgba(16, 16, [90, 90, 90, 255]))
        .unwrap();
    builder
        .add("block/dirt", 16, 16, solid_rgba(16, 16, [120, 85, 60, 255]))
        .unwrap();
    builder.build().unwrap()
}

fn full_uv_quad(texture: &str) -> BakedModelQuad {
    BakedModelQuad {
        positions: [[0.0; 3]; 4],
        normal: [0.0, 1.0, 0.0],
        texture: texture.to_string(),
        uv: [[0.0, 0.0], [16.0, 0.0], [16.0, 16.0], [0.0, 16.0]],
        cullface: None,
        tintindex: -1,
        shade: true,
    }
}

#[test]
fn remaps_baked_quad_uvs_into_atlas_sub_rect() {
    let atlas = stone_atlas();
    // Sorted by name: dirt packs first at x=0, stone second at x=16.
    let stone = atlas.placement("block/stone").unwrap();
    assert_eq!((stone.x, stone.y), (16, 0));
    let rect = atlas.uv_rect("block/stone").unwrap();
    assert_rect_eq(rect, [0.5, 0.0, 1.0, 1.0]);

    // The quad uses a namespaced texture id while the sprite is registered
    // by path only — the remapper must fall back to the path part.
    let quad = full_uv_quad("minecraft:block/stone");
    let uvs = remap_baked_quad_uvs(&atlas, &quad).unwrap();
    assert_uv_eq(uvs[0], [rect[0], rect[1]]);
    assert_uv_eq(uvs[1], [rect[2], rect[1]]);
    assert_uv_eq(uvs[2], [rect[2], rect[3]]);
    assert_uv_eq(uvs[3], [rect[0], rect[3]]);

    // Partial UVs (4..12 of 16) map to the matching fraction of the rect.
    let mut partial = quad.clone();
    partial.uv = [[4.0, 4.0], [12.0, 4.0], [12.0, 12.0], [4.0, 12.0]];
    let puv = remap_baked_quad_uvs(&atlas, &partial).unwrap();
    let eu0 = rect[0] + 0.25 * (rect[2] - rect[0]);
    let ev0 = rect[1] + 0.25 * (rect[3] - rect[1]);
    let eu1 = rect[0] + 0.75 * (rect[2] - rect[0]);
    let ev1 = rect[1] + 0.75 * (rect[3] - rect[1]);
    assert_uv_eq(puv[0], [eu0, ev0]);
    assert_uv_eq(puv[2], [eu1, ev1]);

    // Half-texel inset lands strictly inside the sub-rect on every corner.
    let inset = remap_baked_quad_uvs_inset(&atlas, &quad).unwrap();
    for uv in inset {
        assert!(uv[0] > rect[0] && uv[0] < rect[2]);
        assert!(uv[1] > rect[1] && uv[1] < rect[3]);
    }

    // Unknown texture fails closed.
    let missing = full_uv_quad("minecraft:block/missing");
    assert!(matches!(
        remap_baked_quad_uvs(&atlas, &missing),
        Err(AtlasError::TextureNotInAtlas(_))
    ));
}

#[test]
fn biome_tint_multiplies_rgb_and_preserves_alpha() {
    // Identity tint.
    assert_eq!(
        apply_biome_tint([100, 150, 200, 77], [255, 255, 255]),
        [100, 150, 200, 77]
    );
    // channel = round(c * t / 255), alpha untouched.
    assert_eq!(
        apply_biome_tint([200, 100, 50, 33], [128, 64, 255]),
        [100, 25, 50, 33]
    );
    // Black stays black; alpha survives even a zero tint.
    assert_eq!(
        apply_biome_tint([0, 0, 0, 200], [255, 0, 0]),
        [0, 0, 0, 200]
    );
}
