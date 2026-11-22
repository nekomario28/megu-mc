// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Quality-contract tests for atlas padding fills, gamma-correct mipmaps,
//! cutout alpha-coverage preservation and tile-safe UV rects.

use std::collections::BTreeMap;

use mc_spine_client_mesh::atlas::*;

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

/// A 4x4 sprite whose every texel has a unique colour, so gutter fills can
/// be checked texel-for-texel against their source.
fn indexed_rgba() -> Vec<u8> {
    let mut rgba = Vec::with_capacity(4 * 4 * 4);
    for i in 0..16u8 {
        rgba.extend_from_slice(&[i.wrapping_mul(16), 255u8.wrapping_sub(i * 16), i, 255]);
    }
    rgba
}

fn atlas_px(atlas: &PackedAtlas, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * atlas.width + x) * 4) as usize;
    atlas.rgba[i..i + 4].try_into().unwrap()
}

fn buffer_px(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    rgba[i..i + 4].try_into().unwrap()
}

fn mip_px(mip: &AtlasMip, x: u32, y: u32) -> [u8; 4] {
    buffer_px(&mip.rgba, mip.width, x, y)
}

/// Fraction of texels in a `width * height` RGBA buffer with `a >= cutoff`.
fn coverage_fraction(rgba: &[u8], width: u32, height: u32, cutoff: u8) -> f64 {
    let mut passing = 0u64;
    for px in rgba.as_chunks::<4>().0 {
        if px[3] >= cutoff {
            passing += 1;
        }
    }
    passing as f64 / (u64::from(width) * u64::from(height)) as f64
}

// ---------------------------------------------------------------------------
// Padding gutter fills
// ---------------------------------------------------------------------------

/// Builds the canonical 8x8 test atlas: one 4x4 indexed sprite with
/// `padding = 2` and the given mode, so the sprite lands at atlas `(2, 2)`
/// inside an 8x8 cell covering the whole atlas.
fn padded_atlas(mode: PaddingMode) -> PackedAtlas {
    let mut builder = AtlasBuilder::new();
    builder.set_padding(2).unwrap();
    builder.set_padding_mode(mode);
    builder.add("sprite", 4, 4, indexed_rgba()).unwrap();
    builder.build().unwrap()
}

#[test]
fn clamp_edge_and_wrap_tile_fill_all_sides_and_corners() {
    // ClampEdge: every gutter texel replicates the nearest sprite edge
    // texel; the whole 8x8 cell stays opaque through mip 1 and 2.
    let clamped = padded_atlas(PaddingMode::ClampEdge);
    assert_eq!((clamped.width, clamped.height), (8, 8));
    let p = clamped.placement("sprite").unwrap();
    assert_eq!((p.x, p.y), (2, 2));

    // Sprite interior untouched.
    let src = indexed_rgba();
    for y in 0..4u32 {
        for x in 0..4u32 {
            assert_eq!(atlas_px(&clamped, 2 + x, 2 + y), buffer_px(&src, 4, x, y));
        }
    }
    // Every one of the 64 texels is opaque: the full 2-texel border on all
    // four sides plus all four corners was filled, not left transparent.
    for y in 0..8u32 {
        for x in 0..8u32 {
            assert_eq!(atlas_px(&clamped, x, y)[3], 255, "texel ({x}, {y})");
        }
    }
    // Edge middles replicate the clamped edge column/row.
    assert_eq!(atlas_px(&clamped, 1, 4), atlas_px(&clamped, 2, 4)); // left -> x=0
    assert_eq!(atlas_px(&clamped, 0, 4), atlas_px(&clamped, 2, 4)); // deeper left too
    assert_eq!(atlas_px(&clamped, 6, 4), atlas_px(&clamped, 5, 4)); // right -> x=3
    assert_eq!(atlas_px(&clamped, 4, 1), atlas_px(&clamped, 4, 2)); // top -> y=0
    assert_eq!(atlas_px(&clamped, 4, 6), atlas_px(&clamped, 4, 5)); // bottom -> y=3
    // Corners clamp both axes to the sprite's corner texels.
    assert_eq!(atlas_px(&clamped, 0, 0), atlas_px(&clamped, 2, 2)); // (0,0)
    assert_eq!(atlas_px(&clamped, 7, 0), atlas_px(&clamped, 5, 2)); // (3,0)
    assert_eq!(atlas_px(&clamped, 0, 7), atlas_px(&clamped, 2, 5)); // (0,3)
    assert_eq!(atlas_px(&clamped, 7, 7), atlas_px(&clamped, 5, 5)); // (3,3)

    // WrapTile: gutter texels wrap from the opposite sprite edge.
    let wrapped = padded_atlas(PaddingMode::WrapTile);
    assert_eq!(wrapped.placements, clamped.placements);
    // Sprite interior identical.
    for y in 0..4u32 {
        for x in 0..4u32 {
            assert_eq!(atlas_px(&wrapped, 2 + x, 2 + y), atlas_px(&clamped, 2 + x, 2 + y));
        }
    }
    // Left gutter wraps to the sprite's right edge (x = 3).
    assert_eq!(atlas_px(&wrapped, 1, 4), atlas_px(&wrapped, 5, 4)); // sx=-1 -> tx=3
    assert_eq!(atlas_px(&wrapped, 0, 4), atlas_px(&wrapped, 4, 4)); // sx=-2 -> tx=2
    // Right gutter wraps to the sprite's left edge (x = 0).
    assert_eq!(atlas_px(&wrapped, 6, 4), atlas_px(&wrapped, 2, 4)); // sx=4 -> tx=0
    assert_eq!(atlas_px(&wrapped, 7, 4), atlas_px(&wrapped, 3, 4)); // sx=5 -> tx=1
    // Top gutter wraps to the sprite's bottom edge (y = 3).
    assert_eq!(atlas_px(&wrapped, 4, 1), atlas_px(&wrapped, 4, 5)); // sy=-1 -> ty=3
    assert_eq!(atlas_px(&wrapped, 4, 0), atlas_px(&wrapped, 4, 4)); // sy=-2 -> ty=2
    // Bottom gutter wraps to the sprite's top edge (y = 0).
    assert_eq!(atlas_px(&wrapped, 4, 6), atlas_px(&wrapped, 4, 2)); // sy=4 -> ty=0
    assert_eq!(atlas_px(&wrapped, 4, 7), atlas_px(&wrapped, 4, 3)); // sy=5 -> ty=1
    // Corners wrap both axes.
    assert_eq!(atlas_px(&wrapped, 0, 0), atlas_px(&wrapped, 4, 4)); // (-2,-2)->(2,2)
    assert_eq!(atlas_px(&wrapped, 7, 0), atlas_px(&wrapped, 3, 4)); // (5,-2)->(1,2)
    assert_eq!(atlas_px(&wrapped, 0, 7), atlas_px(&wrapped, 4, 3)); // (-2,5)->(2,1)
    assert_eq!(atlas_px(&wrapped, 7, 7), atlas_px(&wrapped, 3, 3)); // (5,5)->(1,1)
    // The whole wrapped cell is opaque too.
    for y in 0..8u32 {
        for x in 0..8u32 {
            assert_eq!(atlas_px(&wrapped, x, y)[3], 255, "texel ({x}, {y})");
        }
    }

    // An opaque block keeps alpha == 255 through mip levels 1 and 2.
    for atlas in [&clamped, &wrapped] {
        let mips = build_mipmaps(atlas, 2).unwrap();
        assert_eq!(mips.len(), 2);
        for mip in &mips {
            for px in mip.rgba.as_chunks::<4>().0 {
                assert_eq!(px[3], 255, "mip {} lost opacity", mip.level);
            }
        }
    }
}

#[test]
fn transparent_stays_default_and_per_sprite_mode_overrides() {
    // Default builder keeps the historical (0, 0, 0, 0) gutter.
    let mut plain = AtlasBuilder::new();
    assert_eq!(plain.padding_mode(), PaddingMode::Transparent);
    plain.set_padding(2).unwrap();
    plain.add("sprite", 4, 4, indexed_rgba()).unwrap();
    let atlas = plain.build().unwrap();
    let p = atlas.placement("sprite").unwrap();
    assert_eq!(atlas_px(&atlas, p.x, p.y - 1), [0, 0, 0, 0]);
    assert_eq!(atlas_px(&atlas, p.x - 1, p.y - 1), [0, 0, 0, 0]);

    // Per-sprite override: one WrapTile sprite next to a default
    // (Transparent) sprite in the same atlas.
    let mut mixed = AtlasBuilder::new();
    mixed.set_padding(1).unwrap();
    mixed
        .add_sprite_with_mode("wrapped", 4, 4, indexed_rgba(), PaddingMode::WrapTile)
        .unwrap();
    mixed
        .add("plain", 4, 4, solid_rgba(4, 4, [9, 9, 9, 255]))
        .unwrap();
    let atlas = mixed.build().unwrap();

    // Pack order is name-sorted for equal sizes: "plain" first at cell
    // (0, 0) -> sprite (1, 1); "wrapped" second at cell (6, 0) -> (7, 1).
    let plain_p = atlas.placement("plain").unwrap();
    let wrap_p = atlas.placement("wrapped").unwrap();
    assert_eq!((plain_p.x, plain_p.y), (1, 1));
    assert_eq!((wrap_p.x, wrap_p.y), (7, 1));

    // The wrapped sprite's left gutter (sx = -1) samples sprite texel
    // (3, 0) -> atlas (10, 1).
    assert_eq!(atlas_px(&atlas, 6, 1), atlas_px(&atlas, 10, 1));
    // Its top-left corner gutter (sx = -1, sy = -1) samples (3, 3) ->
    // atlas (10, 4).
    assert_eq!(atlas_px(&atlas, 6, 0), atlas_px(&atlas, 10, 4));
    // The plain sprite's gutter stays fully transparent.
    assert_eq!(atlas_px(&atlas, 0, 0), [0, 0, 0, 0]);
    assert_eq!(atlas_px(&atlas, 0, 3), [0, 0, 0, 0]);
    assert_eq!(atlas_px(&atlas, 3, 0), [0, 0, 0, 0]);

    // for_blocks presets WrapTile.
    let mut blocks = AtlasBuilder::for_blocks();
    assert_eq!(blocks.padding_mode(), PaddingMode::WrapTile);
    blocks.set_padding(1).unwrap();
    blocks
        .add("block", 4, 4, solid_rgba(4, 4, [200, 50, 50, 255]))
        .unwrap();
    let atlas = blocks.build().unwrap();
    let p = atlas.placement("block").unwrap();
    assert_eq!((p.x, p.y), (1, 1));
    // Corner gutter wraps to the opposite corner texel: opaque red.
    assert_eq!(atlas_px(&atlas, 0, 0), [200, 50, 50, 255]);
    assert_eq!(atlas_px(&atlas, 0, 3)[3], 255);
    // Texels outside the sprite's cell remain uncovered.
    assert_eq!(atlas_px(&atlas, atlas.width - 1, atlas.height - 1), [0, 0, 0, 0]);
}

// ---------------------------------------------------------------------------
// Gamma-correct mipmaps
// ---------------------------------------------------------------------------

#[test]
fn linear_mipmaps_average_srgb_in_linear_space() {
    // 2x2 atlas: left column sRGB black, right column sRGB white, all
    // opaque — the classic 0/255 average case.
    let mut rgba = vec![0u8; 2 * 2 * 4];
    for px in rgba.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    for y in 0..2u32 {
        for x in 1..2u32 {
            let i = ((y * 2 + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    let atlas = PackedAtlas {
        width: 2,
        height: 2,
        padding: 0,
        rgba,
        placements: BTreeMap::new(),
    };

    let linear = build_mipmaps_linear(&atlas, 1, None).unwrap();
    assert_eq!(linear.len(), 1);
    assert_eq!((linear[0].level, linear[0].width, linear[0].height), (1, 1, 1));
    let px = mip_px(&linear[0], 0, 0);
    // Linear mean of sRGB 0 and 1 is 0.5 -> encoded back to ~188, not the
    // naive sRGB-space ~128.
    for &channel in &px[..3] {
        assert!(
            (180..=200).contains(&channel),
            "linear mip channel {channel} out of expected ~188 range"
        );
        assert!(channel > 140, "channel {channel} looks like naive sRGB averaging");
    }
    assert_eq!(px[3], 255);

    // The raw sRGB-space path is unchanged and still averages to ~128.
    let raw = build_mipmaps(&atlas, 1).unwrap();
    let raw_px = mip_px(&raw[0], 0, 0);
    for &channel in &raw_px[..3] {
        assert!((126..=130).contains(&channel), "raw channel {channel}");
    }
    // Alpha handling is identical between the two paths.
    assert_eq!(px[3], raw_px[3]);
}

// ---------------------------------------------------------------------------
// Cutout alpha-coverage preservation
// ---------------------------------------------------------------------------

#[test]
fn cutout_coverage_preservation_keeps_half_coverage() {
    // 8x8 atlas, 50% coverage checkerboard of 4-texel squares: opaque
    // green where (x / 4 + y / 4) is even.
    let mut rgba = vec![0u8; 8 * 8 * 4];
    for y in 0..8u32 {
        for x in 0..8u32 {
            if (x / 4 + y / 4) % 2 == 0 {
                let i = ((y * 8 + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[0, 200, 0, 255]);
            }
        }
    }
    let atlas = PackedAtlas {
        width: 8,
        height: 8,
        padding: 0,
        rgba,
        placements: BTreeMap::new(),
    };

    let base = coverage_fraction(&atlas.rgba, 8, 8, 128);
    assert!((base - 0.5).abs() < 1e-9, "base coverage {base}");

    let mips = build_mipmaps_linear(&atlas, 2, Some(128)).unwrap();
    assert_eq!(mips.len(), 2);
    for mip in &mips {
        let frac = coverage_fraction(&mip.rgba, mip.width, mip.height, 128);
        assert!(
            (frac - 0.5).abs() < 0.06,
            "mip {} coverage {frac} drifted from the 50% base",
            mip.level
        );
        assert!(frac >= base, "mip {} coverage {frac} below base", mip.level);
    }
}

#[test]
fn cutout_coverage_boost_rescues_sparse_foliage() {
    // Sparse cutout: one opaque texel per 4x4 cell on an 8x8 atlas —
    // 4/64 = 6.25% base coverage. Naive mipmapping dilutes every mip texel
    // below the alpha cutoff and the foliage vanishes entirely.
    let mut rgba = vec![0u8; 8 * 8 * 4];
    for &(x, y) in &[(0u32, 0u32), (4, 0), (0, 4), (4, 4)] {
        let i = ((y * 8 + x) * 4) as usize;
        rgba[i..i + 4].copy_from_slice(&[0, 200, 0, 255]);
    }
    let atlas = PackedAtlas {
        width: 8,
        height: 8,
        padding: 0,
        rgba,
        placements: BTreeMap::new(),
    };
    let base = coverage_fraction(&atlas.rgba, 8, 8, 128);
    assert!((base - 0.0625).abs() < 1e-9, "base coverage {base}");

    // Without coverage preservation the mip chain collapses to zero
    // passing texels; with it the silhouette is rescued.
    let mips = build_mipmaps_linear(&atlas, 2, Some(128)).unwrap();
    let mip1 = coverage_fraction(&mips[0].rgba, mips[0].width, mips[0].height, 128);
    let mip2 = coverage_fraction(&mips[1].rgba, mips[1].width, mips[1].height, 128);
    assert!(mip1 > 0.0, "mip 1 lost all coverage");
    assert!(mip2 > 0.0, "mip 2 lost all coverage");
    assert!(mip1 >= base && mip2 >= base, "coverage fell below base");
    // The boost restores coverage without inflating it to everything:
    // mip 1 keeps exactly the four texels that each held one opaque
    // source texel.
    assert!((mip1 - 0.25).abs() < 0.01, "mip 1 coverage {mip1}");
}

// ---------------------------------------------------------------------------
// Tile-safe UV sub-rects
// ---------------------------------------------------------------------------

#[test]
fn tile_rect_maps_wrapped_uvs_inside_sprite_sub_rect() {
    let mut builder = AtlasBuilder::new();
    builder
        .add("block/stone", 16, 16, solid_rgba(16, 16, [100, 100, 100, 255]))
        .unwrap();
    builder
        .add("block/dirt", 16, 16, solid_rgba(16, 16, [120, 85, 60, 255]))
        .unwrap();
    let atlas = builder.build().unwrap();

    // Namespaced ids resolve through the path fallback, same as the
    // baked-quad remappers.
    let rect = atlas
        .tile_rect("minecraft:block/stone")
        .expect("stone tile rect");
    // Deterministic: identical queries return identical rects.
    assert_eq!(atlas.tile_rect("minecraft:block/stone"), Some(rect));

    // The rect agrees with uv_rect: u0/v0 are the sprite's top-left and
    // u_scale/v_scale its normalized size.
    let uv = atlas.uv_rect("block/stone").unwrap();
    assert!((rect.u0 - uv[0]).abs() < 1e-6);
    assert!((rect.v0 - uv[1]).abs() < 1e-6);
    assert!((rect.u_scale - (uv[2] - uv[0])).abs() < 1e-6);
    assert!((rect.v_scale - (uv[3] - uv[1])).abs() < 1e-6);
    assert!((rect.u0 + rect.u_scale - uv[2]).abs() < 1e-6);
    assert!((rect.v0 + rect.v_scale - uv[3]).abs() < 1e-6);
    assert!((rect.texel_u - 1.0 / atlas.width as f32).abs() < 1e-6);
    assert!((rect.texel_v - 1.0 / atlas.height as f32).abs() < 1e-6);

    // Shader contract: f = uv - floor(uv) (GLSL fract), then
    // au = u0 + f_u * u_scale, av = v0 + f_v * v_scale. Every wrapped UV —
    // including values far outside 0..1 — lands strictly inside
    // [u0, u0 + u_scale) x [v0, v0 + v_scale).
    for &uv_u in &[0.0f32, 0.25, 0.5, 0.999, 1.0, 1.7, 2.3, 13.75] {
        for &uv_v in &[0.0f32, 0.5, 0.999, 3.2] {
            let fu = uv_u - uv_u.floor();
            let fv = uv_v - uv_v.floor();
            let au = rect.u0 + fu * rect.u_scale;
            let av = rect.v0 + fv * rect.v_scale;
            assert!(
                au >= rect.u0 && au < rect.u0 + rect.u_scale,
                "u {uv_u}: atlas u {au} escaped [{}, {})",
                rect.u0,
                rect.u0 + rect.u_scale
            );
            assert!(
                av >= rect.v0 && av < rect.v0 + rect.v_scale,
                "v {uv_v}: atlas v {av} escaped [{}, {})",
                rect.v0,
                rect.v0 + rect.v_scale
            );
        }
    }

    // Unknown sprites fail closed.
    assert!(atlas.tile_rect("minecraft:block/missing").is_none());
}
