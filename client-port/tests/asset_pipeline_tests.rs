// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Integration tests for `asset_pipeline`: ordered pack layers compile into
//! one [`CompiledAssetBundle`] — merged blockstates, overlay model stack,
//! packed atlas + linear mip chain — and `bake_block_with_uvs` yields quads
//! whose UVs are already remapped into the atlas.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use megu_client_mesh::asset_pipeline::{
    AssetPipelineError, CompiledAssetBundle, ResourcePackLayer, MAX_ASSET_FILE_BYTES,
};
use megu_client_mesh::atlas::AtlasTileRect;

/// RGBA8 sprite payload: `(width, height, pixels)`.
type Sprite = (u32, u32, Vec<u8>);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// An opaque `size`x`size` RGBA8 sprite with a flat colour.
fn opaque_sprite(size: u32, rgb: [u8; 3]) -> Sprite {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for _ in 0..size * size {
        rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], u8::MAX]);
    }
    (size, size, rgba)
}

/// A `size`x`size` cutout sprite: checkerboard alpha (0/255 only), so the
/// pipeline's non-opaque -> `ClampEdge` padding path is exercised.
fn cutout_sprite(size: u32) -> Sprite {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let alpha = if (x + y) % 2 == 0 { u8::MAX } else { 0 };
            rgba.extend_from_slice(&[200, 120, 40, alpha]);
        }
    }
    (size, size, rgba)
}

/// Builds a [`ResourcePackLayer`] from `(id, json)` / `(id, sprite)` slices.
fn layer(
    name: &str,
    models: &[(&str, &str)],
    blockstates: &[(&str, &str)],
    sprites: &[(&str, Sprite)],
) -> ResourcePackLayer {
    ResourcePackLayer {
        name: name.to_owned(),
        models: models
            .iter()
            .map(|(id, json)| (id.to_string(), json.to_string()))
            .collect(),
        blockstates: blockstates
            .iter()
            .map(|(id, json)| (id.to_string(), json.to_string()))
            .collect(),
        sprites: sprites
            .iter()
            .map(|(id, sprite)| (id.to_string(), sprite.clone()))
            .collect(),
        animations: BTreeMap::new(),
    }
}

/// Every vertex UV must land strictly inside the sprite's tile rect —
/// proving the remap targeted this sprite's sub-rect, not a neighbour's.
fn assert_uvs_inside(quad_uv: &[[f32; 2]; 4], tile: &AtlasTileRect, what: &str) {
    for (corner, uv) in quad_uv.iter().enumerate() {
        assert!(
            uv[0] > tile.u0 + 1e-6 && uv[0] < tile.u0 + tile.u_scale - 1e-6,
            "{what} corner {corner}: u {} outside ({}, {})",
            uv[0],
            tile.u0,
            tile.u0 + tile.u_scale
        );
        assert!(
            uv[1] > tile.v0 + 1e-6 && uv[1] < tile.v0 + tile.v_scale - 1e-6,
            "{what} corner {corner}: v {} outside ({}, {})",
            uv[1],
            tile.v0,
            tile.v0 + tile.v_scale
        );
    }
}

// ---------------------------------------------------------------------------
// End-to-end: overlay + rotated variant + multipart + atlas UVs + mips
// ---------------------------------------------------------------------------

/// Base pack's `block/test_block`: a single up face on the base texture.
/// Never baked — the user pack's same-id model shadows it — but it must
/// still parse so the overlay stack is exercised honestly.
const BASE_TEST_BLOCK: &str = r##"{"textures":{"t":"minecraft:block/base_stone"},
"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{
"up":{"texture":"#t","uv":[2,2,14,14]}}}]}"##;

/// User pack's 3D override of `block/test_block`: a single north face on
/// its own cutout sprite, inset UVs so remapped UVs stay strictly inside.
const OVERRIDE_TEST_BLOCK: &str = r##"{"textures":{"t":"mypack:block/ore"},
"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{
"north":{"texture":"#t","uv":[2,2,14,14]}}}]}"##;

/// Fence post: full cube on the post texture, all faces inset-UV'd.
const FENCE_POST_MODEL: &str = r##"{"textures":{"t":"minecraft:block/fence_post_tex"},
"elements":[{"from":[6,0,6],"to":[10,16,10],"faces":{
"down":{"texture":"#t","uv":[2,2,14,14]},"up":{"texture":"#t","uv":[2,2,14,14]},
"north":{"texture":"#t","uv":[2,2,14,14]},"south":{"texture":"#t","uv":[2,2,14,14]},
"west":{"texture":"#t","uv":[2,2,14,14]},"east":{"texture":"#t","uv":[2,2,14,14]}}}]}"##;

/// Fence side rail: thin plate, four side faces on the side texture.
const FENCE_SIDE_MODEL: &str = r##"{"textures":{"t":"minecraft:block/fence_side_tex"},
"elements":[{"from":[0,5,7],"to":[16,11,9],"faces":{
"north":{"texture":"#t","uv":[2,2,14,14]},"south":{"texture":"#t","uv":[2,2,14,14]},
"west":{"texture":"#t","uv":[2,2,14,14]},"east":{"texture":"#t","uv":[2,2,14,14]}}}]}"##;

/// `testblock` blockstate: `facing=north` -> y:90 + uvlock, `facing=east`
/// -> y:270 + uvlock. Keys are written namespace-less on purpose so key
/// normalization is covered too.
const TESTBLOCK_STATE: &str = r##"{"variants":{
"facing=north":{"model":"block/test_block","y":90,"uvlock":true},
"facing=east":{"model":"block/test_block","y":270,"uvlock":true}}}"##;

/// `fence` blockstate: multipart — a side rail when `north`/`east` is
/// true (east rotated y:90) plus an unconditional post.
const FENCE_STATE: &str = r##"{"multipart":[
{"when":{"north":"true"},"apply":{"model":"block/fence_side"}},
{"when":{"east":"true"},"apply":{"model":"block/fence_side","y":90}},
{"apply":{"model":"block/fence_post"}}]}"##;

#[test]
fn overlay_bundle_bakes_atlas_uv_quads_for_variants_and_multipart() {
    let base = layer(
        "vanilla",
        &[
            ("block/test_block", BASE_TEST_BLOCK),
            ("block/fence_post", FENCE_POST_MODEL),
            ("block/fence_side", FENCE_SIDE_MODEL),
        ],
        &[("testblock", TESTBLOCK_STATE), ("fence", FENCE_STATE)],
        &[
            ("minecraft:block/base_stone", opaque_sprite(16, [120, 120, 120])),
            (
                "minecraft:block/fence_post_tex",
                opaque_sprite(16, [140, 90, 40]),
            ),
            (
                "minecraft:block/fence_side_tex",
                opaque_sprite(16, [90, 60, 30]),
            ),
        ],
    );
    // Higher-priority 3D pack: shadows `block/test_block` and supplies the
    // cutout sprite its face references.
    let user = layer(
        "user-3d",
        &[("block/test_block", OVERRIDE_TEST_BLOCK)],
        &[],
        &[("mypack:block/ore", cutout_sprite(16))],
    );

    let bundle = CompiledAssetBundle::build(&[base, user]).expect("bundle builds");
    assert!(
        bundle.skipped_errors.is_empty(),
        "unexpected skipped entries: {:?}",
        bundle.skipped_errors
    );
    // Namespace-less blockstate key was normalized on merge.
    assert!(bundle.blockstates.contains_key("minecraft:testblock"));
    assert!(bundle.blockstates.contains_key("minecraft:fence"));
    // A linear mip chain down to 1x1 exists below the atlas base image.
    assert!(!bundle.mipmaps.is_empty());
    assert_eq!(bundle.mipmaps[0].level, 1);
    assert_eq!(bundle.mipmaps[0].width, bundle.atlas.width / 2);
    assert_eq!(bundle.mipmaps[0].height, bundle.atlas.height / 2);
    // The chain runs the atlas's full mip depth: the last level has a
    // 1-texel axis (the atlas need not be square).
    assert_eq!(
        bundle.mipmaps.len(),
        bundle.atlas.max_mip_levels() as usize
    );
    let last = bundle.mipmaps.last().expect("mip chain non-empty");
    assert_eq!(last.width.min(last.height), 1);

    // Rotated variant: y=90 + uvlock must bake through the *override* model.
    let quads = bundle
        .bake_block_with_uvs("testblock", &[("facing", "north")], [3, 64, -2])
        .expect("y=90 uvlock variant bakes");
    assert_eq!(quads.len(), 1);
    let quad = &quads[0];
    assert_eq!(
        quad.texture, "mypack:block/ore",
        "higher-priority pack's model must win the overlay"
    );
    // Blockstate y=90 maps the north-face normal onto east.
    assert_eq!(quad.normal, [1.0, 0.0, 0.0]);
    assert_eq!(quad.tintindex, -1);
    assert!(quad.shade);
    assert!(quad.cullface.is_none());
    let ore_tile = bundle
        .atlas
        .tile_rect("mypack:block/ore")
        .expect("override sprite packed");
    assert_eq!(quad.tile, ore_tile);
    assert_uvs_inside(&quad.uv, &ore_tile, "rotated variant");

    // y=270 variant resolves too and counter-rotates UVs via uvlock.
    let quads_east = bundle
        .bake_block_with_uvs("testblock", &[("facing", "east")], [0, 0, 0])
        .expect("y=270 uvlock variant bakes");
    assert_eq!(quads_east.len(), 1);
    assert_eq!(quads_east[0].texture, "mypack:block/ore");
    assert_uvs_inside(&quads_east[0].uv, &ore_tile, "y=270 variant");

    // Multipart fence: north=true selects the side rail + unconditional
    // post -> 4 + 6 quads, every UV inside its own sprite's tile rect.
    let fence = bundle
        .bake_block_with_uvs(
            "fence",
            &[("north", "true"), ("east", "false")],
            [0, 0, 0],
        )
        .expect("multipart fence bakes");
    assert_eq!(fence.len(), 10);
    let side_tile = bundle
        .atlas
        .tile_rect("minecraft:block/fence_side_tex")
        .expect("side sprite packed");
    let post_tile = bundle
        .atlas
        .tile_rect("minecraft:block/fence_post_tex")
        .expect("post sprite packed");
    for quad in &fence {
        let tile = if quad.texture == "minecraft:block/fence_side_tex" {
            side_tile
        } else {
            post_tile
        };
        assert_eq!(quad.tile, tile);
        assert_uvs_inside(&quad.uv, &tile, "fence quad");
    }
    assert_eq!(
        fence
            .iter()
            .filter(|q| q.texture == "minecraft:block/fence_side_tex")
            .count(),
        4
    );
    assert_eq!(
        fence
            .iter()
            .filter(|q| q.texture == "minecraft:block/fence_post_tex")
            .count(),
        6
    );

    // east=true instead selects the y:90-rotated side rail + post.
    let fence_east = bundle
        .bake_block_with_uvs(
            "fence",
            &[("north", "false"), ("east", "true")],
            [0, 0, 0],
        )
        .expect("east multipart state bakes");
    assert_eq!(fence_east.len(), 10);
    assert!(fence_east.iter().all(|q| {
        q.texture == "minecraft:block/fence_side_tex" || q.texture == "minecraft:block/fence_post_tex"
    }));

    // Neither flag: only the unconditional post part matches.
    let post_only = bundle
        .bake_block_with_uvs(
            "fence",
            &[("north", "false"), ("east", "false")],
            [0, 0, 0],
        )
        .expect("post-only state bakes");
    assert_eq!(post_only.len(), 6);
    assert!(
        post_only
            .iter()
            .all(|q| q.texture == "minecraft:block/fence_post_tex")
    );
}

// ---------------------------------------------------------------------------
// Gamma-correct mip chain
// ---------------------------------------------------------------------------

#[test]
fn mipmaps_are_built_in_linear_srgb() {
    // 2x2 sprite: black, white, white, black — all alpha 255. With padding
    // 2 the cell is 6x6 so the atlas is 8x8; mip level 1 is 4x4 and texel
    // (1,1) covers exactly the sprite's four texels.
    let mut rgba = Vec::with_capacity(16);
    for px in [
        [0u8, 0, 0, 255],
        [255, 255, 255, 255],
        [255, 255, 255, 255],
        [0, 0, 0, 255],
    ] {
        rgba.extend_from_slice(&px);
    }
    let pack = layer(
        "tiny",
        &[],
        &[],
        &[("minecraft:block/tiny", (2, 2, rgba))],
    );
    let bundle = CompiledAssetBundle::build(&[pack]).expect("bundle builds");
    assert_eq!((bundle.atlas.width, bundle.atlas.height), (8, 8));
    // 8 -> 4 -> 2 -> 1: the full chain down to 1x1.
    assert_eq!(bundle.mipmaps.len(), 3);
    let mip1 = &bundle.mipmaps[0];
    assert_eq!((mip1.width, mip1.height), (4, 4));
    // Linear-light mean of sRGB 0 and 255 is 0.5, which encodes back to
    // ~188 — the naive sRGB-space average would be ~128. Pinning ~188
    // proves `build_mipmaps_linear` ran, not `build_mipmaps`.
    let texel = (mip1.width as usize + 1) * 4;
    let red = mip1.rgba[texel];
    assert!(
        (185..=190).contains(&red),
        "linear mip average should be ~188, got {red}"
    );
    assert_eq!(mip1.rgba[texel + 3], 255, "alpha stays fully opaque");
}

// ---------------------------------------------------------------------------
// Fail-closed per-entry handling
// ---------------------------------------------------------------------------

const GOOD_MODEL: &str = r##"{"textures":{"t":"minecraft:block/tex"},
"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{
"up":{"texture":"#t","uv":[2,2,14,14]}}}]}"##;
const GOOD_STATE: &str = r##"{"variants":{"":{"model":"block/good"}}}"##;

#[test]
fn corrupt_entries_fail_closed_into_skipped_errors() {
    let pack = layer(
        "mixed",
        &[
            ("block/good", GOOD_MODEL),
            ("block/broken", "{ this is not json"),
        ],
        &[
            ("good", GOOD_STATE),
            ("broken", "{ also not json"),
            (
                "uses_broken",
                r##"{"variants":{"":{"model":"block/broken"}}}"##,
            ),
        ],
        &[("minecraft:block/tex", opaque_sprite(16, [1, 2, 3]))],
    );
    let bundle = CompiledAssetBundle::build(&[pack]).expect("bundle still builds");
    // Exactly the two corrupt documents are recorded, with reasons.
    assert_eq!(bundle.skipped_errors.len(), 2, "{:?}", bundle.skipped_errors);
    assert!(bundle
        .skipped_errors
        .iter()
        .any(|(key, reason)| key == "block/broken" && !reason.is_empty()));
    assert!(bundle
        .skipped_errors
        .iter()
        .any(|(key, reason)| key == "broken" && !reason.is_empty()));

    // The healthy entries still work end to end.
    let quads = bundle
        .bake_block_with_uvs("good", &[], [0, 0, 0])
        .expect("good blockstate bakes");
    assert!(!quads.is_empty());
    let tex_tile = bundle
        .atlas
        .tile_rect("minecraft:block/tex")
        .expect("tex sprite packed");
    for quad in &quads {
        assert_uvs_inside(&quad.uv, &tex_tile, "good quad");
    }

    // The corrupt blockstate never entered the merged map.
    assert!(matches!(
        bundle.bake_block_with_uvs("broken", &[], [0, 0, 0]),
        Err(AssetPipelineError::MissingBlockState(_))
    ));
    // A valid blockstate pointing at the corrupt model fails at bake time.
    assert!(matches!(
        bundle.bake_block_with_uvs("uses_broken", &[], [0, 0, 0]),
        Err(AssetPipelineError::Model(_))
    ));
}

// ---------------------------------------------------------------------------
// from_dir: pack layout, animations, traversal and size fail-closed
// ---------------------------------------------------------------------------

/// Unique temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "asset-pipeline-test-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp dir creates");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `.rgba` encoding matching [`RGBA8_SPRITE_FORMAT`][megu_client_mesh::asset_pipeline::RGBA8_SPRITE_FORMAT].
fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + rgba.len());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(rgba);
    out
}

#[test]
fn from_dir_loads_blockstates_models_sprites_and_animations() {
    let tmp = TempDir::new("load");
    let root = tmp.path().join("my-pack");
    for dir in [
        "assets/minecraft/blockstates",
        "assets/minecraft/models/block",
        "assets/minecraft/textures/block",
        "assets/mypack/models/item",
    ] {
        fs::create_dir_all(root.join(dir)).expect("pack dirs create");
    }
    fs::write(
        root.join("assets/minecraft/blockstates/ore.json"),
        r##"{"variants":{"":{"model":"block/ore"}}}"##,
    )
    .unwrap();
    fs::write(
        root.join("assets/minecraft/models/block/ore.json"),
        r##"{"textures":{"t":"minecraft:block/ore"},
"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{
"up":{"texture":"#t","uv":[2,2,14,14]}}}]}"##,
    )
    .unwrap();
    // A 2x4 strip: two 2x2 frames of flat colour.
    let mut strip = Vec::with_capacity(8 * 4);
    for i in 0..8u8 {
        strip.extend_from_slice(&[i * 20, 200, 30, 255]);
    }
    fs::write(
        root.join("assets/minecraft/textures/block/ore.rgba"),
        encode_rgba(2, 4, &strip),
    )
    .unwrap();
    fs::write(
        root.join("assets/minecraft/textures/block/ore.rgba.mcmeta"),
        r##"{"animation":{"frametime":3,"interpolate":true}}"##,
    )
    .unwrap();
    // Other namespaces load too; unrelated files (.png, pack.mcmeta) are ignored.
    fs::write(
        root.join("assets/mypack/models/item/thing.json"),
        r##"{"textures":{}}"##,
    )
    .unwrap();
    fs::write(root.join("assets/minecraft/textures/block/ignore.png"), b"not a sprite")
        .unwrap();
    fs::write(root.join("pack.mcmeta"), b"{}").unwrap();

    let layer = ResourcePackLayer::from_dir(&root).expect("pack dir loads");
    assert_eq!(layer.name, "my-pack");
    assert!(layer.blockstates.contains_key("minecraft:ore"));
    assert!(layer.models.contains_key("minecraft:block/ore"));
    assert!(layer.models.contains_key("mypack:item/thing"));
    let (width, height, _) = layer
        .sprites
        .get("minecraft:block/ore")
        .expect("ore sprite loaded");
    assert_eq!((*width, *height), (2, 4));
    let animation = layer
        .animations
        .get("minecraft:block/ore")
        .expect("ore animation loaded");
    assert_eq!(animation.frame_count(), 2);
    assert_eq!(animation.frames()[0].ticks, 3);
    assert!(animation.interpolate());

    // The disk-loaded layer feeds the whole pipeline end to end.
    let bundle = CompiledAssetBundle::build(&[layer]).expect("bundle builds");
    let quads = bundle
        .bake_block_with_uvs("ore", &[], [0, 0, 0])
        .expect("disk-loaded block bakes");
    assert_eq!(quads.len(), 1);
    assert_eq!(quads[0].texture, "minecraft:block/ore");
    let tile = bundle
        .atlas
        .tile_rect("minecraft:block/ore")
        .expect("ore sprite packed");
    assert_uvs_inside(&quads[0].uv, &tile, "disk-loaded quad");
    // The animation table is merged and resolvable by texture id.
    assert!(bundle.animation_for("minecraft:block/ore").is_some());
}

#[test]
#[cfg(unix)]
fn from_dir_rejects_symlink_escapes() {
    let tmp = TempDir::new("escape");
    let root = tmp.path().join("pack");
    fs::create_dir_all(root.join("assets/minecraft/textures/block")).unwrap();
    // A real sprite outside the pack root, symlinked in as a texture.
    let outside = tmp.path().join("outside.rgba");
    fs::write(&outside, encode_rgba(1, 1, &[9, 9, 9, 255])).unwrap();
    std::os::unix::fs::symlink(
        &outside,
        root.join("assets/minecraft/textures/block/evil.rgba"),
    )
    .unwrap();
    let err = ResourcePackLayer::from_dir(&root).expect_err("escape must fail");
    assert!(
        matches!(err, AssetPipelineError::UnsafePath { .. }),
        "expected UnsafePath, got {err:?}"
    );
}

#[test]
fn from_dir_rejects_oversized_and_malformed_files() {
    // Oversized file: sparse file over the 8 MiB cap.
    let tmp = TempDir::new("big");
    let root = tmp.path().join("pack");
    fs::create_dir_all(root.join("assets/minecraft/models/block")).unwrap();
    let big = root.join("assets/minecraft/models/block/big.json");
    let file = fs::File::create(&big).unwrap();
    file.set_len(MAX_ASSET_FILE_BYTES + 1).unwrap();
    drop(file);
    let err = ResourcePackLayer::from_dir(&root).expect_err("oversize must fail");
    assert!(
        matches!(err, AssetPipelineError::FileTooLarge { .. }),
        "expected FileTooLarge, got {err:?}"
    );

    // Malformed .rgba: header claims 4x4 but the payload is far short.
    let tmp = TempDir::new("badrgba");
    let root = tmp.path().join("pack");
    fs::create_dir_all(root.join("assets/minecraft/textures/block")).unwrap();
    let mut bad = encode_rgba(4, 4, &[0; 8]);
    bad.truncate(16); // header + 8 bytes, needs 8 + 64
    fs::write(root.join("assets/minecraft/textures/block/bad.rgba"), bad).unwrap();
    let err = ResourcePackLayer::from_dir(&root).expect_err("bad sprite must fail");
    assert!(
        matches!(err, AssetPipelineError::InvalidPackFile { .. }),
        "expected InvalidPackFile, got {err:?}"
    );

    // Malformed .mcmeta: animation section is not an object.
    let tmp = TempDir::new("badmeta");
    let root = tmp.path().join("pack");
    fs::create_dir_all(root.join("assets/minecraft/textures/block")).unwrap();
    let mut sprite_px = Vec::with_capacity(16);
    for _ in 0..4 {
        sprite_px.extend_from_slice(&[1, 2, 3, 255]);
    }
    fs::write(
        root.join("assets/minecraft/textures/block/s.rgba"),
        encode_rgba(2, 2, &sprite_px),
    )
    .unwrap();
    fs::write(
        root.join("assets/minecraft/textures/block/s.rgba.mcmeta"),
        r##"{"animation":"nope"}"##,
    )
    .unwrap();
    let err = ResourcePackLayer::from_dir(&root).expect_err("bad mcmeta must fail");
    assert!(
        matches!(err, AssetPipelineError::InvalidPackFile { .. }),
        "expected InvalidPackFile, got {err:?}"
    );

    // Not a directory at all.
    let file_path = tmp.path().join("plain-file");
    fs::write(&file_path, b"x").unwrap();
    assert!(matches!(
        ResourcePackLayer::from_dir(&file_path),
        Err(AssetPipelineError::NotADirectory { .. })
    ));
}
