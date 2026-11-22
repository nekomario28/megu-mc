// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use mc_spine_client_mesh::vanilla_plus_3d::{ExtrudedMesh, TEXEL};
use mc_spine_client_mesh::vanilla_plus_visuals::*;
use mc_spine_client_mesh::Direction;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cfg() -> VanillaPlusVisualsConfig {
    VanillaPlusVisualsConfig::default()
}

fn neighbors(n: bool, e: bool, s: bool, w: bool) -> CtmNeighbors {
    CtmNeighbors {
        n,
        e,
        s,
        w,
        ..CtmNeighbors::NONE
    }
}

fn assert_unit_normals(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        let len = (q.normal[0] * q.normal[0]
            + q.normal[1] * q.normal[1]
            + q.normal[2] * q.normal[2])
            .sqrt();
        assert!(
            (len - 1.0).abs() < 1e-4,
            "quad {i} normal {:?} is not unit (len {len})",
            q.normal
        );
    }
}

fn assert_block_local(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        for p in &q.positions {
            for c in p {
                assert!(
                    (-1e-5..=1.0 + 1e-5).contains(c),
                    "quad {i} position {p:?} outside 0..=1"
                );
                assert!(c.is_finite(), "quad {i} non-finite position {p:?}");
            }
        }
    }
}

fn axis_of(d: Direction) -> usize {
    match d {
        Direction::West | Direction::East => 0,
        Direction::Down | Direction::Up => 1,
        Direction::North | Direction::South => 2,
    }
}

fn outward(d: Direction) -> [f32; 3] {
    let mut v = [0.0; 3];
    v[axis_of(d)] = match d {
        Direction::Up | Direction::South | Direction::East => 1.0,
        _ => -1.0,
    };
    v
}

/// Whether a quad's outward-facing positions are all recessed at least
/// `depth` inside the surface plane of `dir`.
fn is_recessed_on(q: &mc_spine_client_mesh::vanilla_plus_3d::ExtrudedQuad, dir: Direction, depth: f32) {
    let axis = axis_of(dir);
    let out = outward(dir)[axis];
    for p in &q.positions {
        let c = p[axis];
        if out > 0.0 {
            assert!(c <= 1.0 - depth + 1e-5, "position {p:?} not recessed");
        } else {
            assert!(c >= depth - 1e-5, "position {p:?} not recessed");
        }
    }
}

fn skin_with(pixels: &[(usize, usize)]) -> Vec<u8> {
    let mut skin = vec![0u8; SKIN_RGBA_BYTES];
    for &(x, y) in pixels {
        skin[(y * SKIN_DIM + x) * 4 + 3] = 255;
    }
    skin
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn default_config_is_fully_vanilla() {
    let c = cfg();
    assert!(!c.connected_textures);
    assert!(!c.better_grass);
    assert!(!c.better_snow);
    assert!(!c.block_relief_3d);
    assert!(!c.skin_3d_layers);
    assert_eq!(c.relief_lod_cutoff_blocks, 20.0);
    assert_eq!(c.skin_3d_lod_cutoff_blocks, 16.0);
    assert!(c.respect_custom_resourcepack);
}

#[test]
fn lod_cutoffs_clamp() {
    let mut c = cfg();
    c.relief_lod_cutoff_blocks = 1.0;
    c.skin_3d_lod_cutoff_blocks = 999.0;
    assert_eq!(c.effective_relief_lod_cutoff(), 6.0);
    assert_eq!(c.effective_skin_3d_lod_cutoff(), 32.0);
    c.relief_lod_cutoff_blocks = 500.0;
    c.skin_3d_lod_cutoff_blocks = -3.0;
    assert_eq!(c.effective_relief_lod_cutoff(), 48.0);
    assert_eq!(c.effective_skin_3d_lod_cutoff(), 6.0);
}

// ---------------------------------------------------------------------------
// CTM classification
// ---------------------------------------------------------------------------

#[test]
fn ctm_classification() {
    for id in [
        "glass",
        "glass_pane",
        "tinted_glass",
        "white_stained_glass",
        "black_stained_glass_pane",
        "minecraft:orange_stained_glass",
        "mymod:fancy_glass",
        "mymod:fancy_glass_pane",
    ] {
        assert_eq!(classify_ctm_block(id), Some(CtmMode::Full47), "{id}");
    }
    for id in ["bookshelf", "chiseled_bookshelf", "mymod:dark_bookshelf"] {
        assert_eq!(classify_ctm_block(id), Some(CtmMode::Horizontal), "{id}");
    }
    for id in ["sandstone", "cut_sandstone", "red_sandstone", "cut_red_sandstone"] {
        assert_eq!(classify_ctm_block(id), Some(CtmMode::Vertical), "{id}");
    }
    assert_eq!(classify_ctm_block("stone"), None);
    assert_eq!(classify_ctm_block("minecraft:grass_block"), None);
    assert_eq!(classify_ctm_block("hourglass"), None);
}

// ---------------------------------------------------------------------------
// CTM Full47
// ---------------------------------------------------------------------------

#[test]
fn ctm_full47_isolated_shows_all_borders() {
    let tile = resolve_ctm_tile(CtmMode::Full47, &CtmNeighbors::NONE);
    let CtmTile::Full47 { index, suppress } = tile else {
        panic!("expected Full47, got {tile:?}");
    };
    assert_eq!(index, 0);
    assert_eq!(suppress, CtmBorderSuppress::NONE);
}

#[test]
fn ctm_full47_2x2_suppresses_interior_borders() {
    // Bottom-left cell of a 2x2 glass sheet: neighbours right, below, and the
    // shared diagonal — interior edges hidden, outer edges shown.
    let n = CtmNeighbors {
        e: true,
        s: true,
        se: true,
        ..CtmNeighbors::NONE
    };
    let tile = resolve_ctm_tile(CtmMode::Full47, &n);
    let CtmTile::Full47 { index, suppress } = tile else {
        panic!("expected Full47, got {tile:?}");
    };
    assert_eq!(index, 8); // adjacent pair E|S at 7 + shared corner
    assert!(suppress.hide_right);
    assert!(suppress.hide_bottom);
    assert!(!suppress.hide_top);
    assert!(!suppress.hide_left);
}

#[test]
fn ctm_full47_diagonal_needs_both_cardinals() {
    // Diagonal alone is not a connection.
    let n = CtmNeighbors {
        se: true,
        ..CtmNeighbors::NONE
    };
    let CtmTile::Full47 { index, suppress } = resolve_ctm_tile(CtmMode::Full47, &n) else {
        panic!("expected Full47");
    };
    assert_eq!(index, 0);
    assert_eq!(suppress, CtmBorderSuppress::NONE);

    // NW corner present but only N connects — corner does not count: plain
    // N|S column tile.
    let n = CtmNeighbors {
        n: true,
        s: true,
        nw: true,
        ..CtmNeighbors::NONE
    };
    let CtmTile::Full47 { index, .. } = resolve_ctm_tile(CtmMode::Full47, &n) else {
        panic!("expected Full47");
    };
    assert_eq!(index, 13);

    // Fully surrounded glass: innermost tile, every border hidden.
    let CtmTile::Full47 { index, suppress } =
        resolve_ctm_tile(CtmMode::Full47, &CtmNeighbors::ALL)
    else {
        panic!("expected Full47");
    };
    assert_eq!(index, 46);
    assert!(suppress.hide_top && suppress.hide_right && suppress.hide_bottom && suppress.hide_left);
}

#[test]
fn ctm_horizontal_and_vertical_tiles() {
    assert_eq!(
        resolve_ctm_tile(CtmMode::Horizontal, &CtmNeighbors::NONE),
        CtmTile::Horizontal(CtmHorizontalTile::Single)
    );
    assert_eq!(
        resolve_ctm_tile(CtmMode::Horizontal, &neighbors(false, true, false, false)),
        CtmTile::Horizontal(CtmHorizontalTile::Left)
    );
    assert_eq!(
        resolve_ctm_tile(CtmMode::Horizontal, &neighbors(false, true, false, true)),
        CtmTile::Horizontal(CtmHorizontalTile::Middle)
    );
    assert_eq!(
        resolve_ctm_tile(CtmMode::Horizontal, &neighbors(false, false, false, true)),
        CtmTile::Horizontal(CtmHorizontalTile::Right)
    );

    assert_eq!(
        resolve_ctm_tile(CtmMode::Vertical, &CtmNeighbors::NONE),
        CtmTile::Vertical(CtmVerticalTile::Single)
    );
    assert_eq!(
        resolve_ctm_tile(CtmMode::Vertical, &neighbors(false, false, true, false)),
        CtmTile::Vertical(CtmVerticalTile::Top)
    );
    assert_eq!(
        resolve_ctm_tile(CtmMode::Vertical, &neighbors(true, false, false, false)),
        CtmTile::Vertical(CtmVerticalTile::Bottom)
    );
    assert_eq!(
        resolve_ctm_tile(CtmMode::Vertical, &neighbors(true, false, true, false)),
        CtmTile::Vertical(CtmVerticalTile::Middle)
    );
}

// ---------------------------------------------------------------------------
// Better Grass / Better Snow
// ---------------------------------------------------------------------------

#[test]
fn better_grass_evaluates_slopes() {
    let mut c = cfg();
    c.better_grass = true;

    // Flat plain: no lower diagonal step -> vanilla side.
    assert_eq!(
        evaluate_better_grass(&c, "grass_block", Direction::North, false, false, false, false),
        SideFaceStyle::VanillaSide
    );
    // One-block step: lower diagonal same grass, same level not opaque.
    assert_eq!(
        evaluate_better_grass(&c, "grass_block", Direction::East, true, false, false, false),
        SideFaceStyle::FullTopTexture
    );
    // Buried face: same-level neighbour opaque -> stays vanilla.
    assert_eq!(
        evaluate_better_grass(&c, "grass_block", Direction::East, true, true, false, false),
        SideFaceStyle::VanillaSide
    );
    // Snowy grass beside snow promotes the side face.
    assert_eq!(
        evaluate_better_grass(&c, "grass_block", Direction::South, false, false, true, true),
        SideFaceStyle::SnowySide
    );
    // Snowy flag without adjacent snow falls through to the slope check.
    assert_eq!(
        evaluate_better_grass(&c, "grass_block", Direction::South, true, false, true, false),
        SideFaceStyle::FullTopTexture
    );
    // Top/bottom faces and unlisted blocks never change.
    assert_eq!(
        evaluate_better_grass(&c, "grass_block", Direction::Up, true, false, false, false),
        SideFaceStyle::VanillaSide
    );
    assert_eq!(
        evaluate_better_grass(&c, "stone", Direction::North, true, false, false, false),
        SideFaceStyle::VanillaSide
    );
    // Nylium variants are covered; feature off is pure passthrough.
    assert_eq!(
        evaluate_better_grass(&c, "warped_nylium", Direction::West, true, false, false, false),
        SideFaceStyle::FullTopTexture
    );
    assert_eq!(
        evaluate_better_grass(&cfg(), "grass_block", Direction::East, true, false, false, false),
        SideFaceStyle::VanillaSide
    );
}

#[test]
fn better_snow_fills_partial_blocks() {
    let mut c = cfg();
    c.better_snow = true;

    let on = evaluate_better_snow(&c, "oak_fence", false, true, 3);
    assert!(on.emit_snow_layer);
    assert_eq!(on.layer_height, 0.125);

    let on = evaluate_better_snow(&c, "poppy", false, true, 2);
    assert!(on.emit_snow_layer);
    let on = evaluate_better_snow(&c, "cobblestone_wall", false, true, 4);
    assert!(on.emit_snow_layer);
    let on = evaluate_better_snow(&c, "torch", false, true, 2);
    assert!(on.emit_snow_layer);
    // Bottom slab counts as a partial block too.
    let on = evaluate_better_snow(&c, "oak_slab", true, true, 2);
    assert!(on.emit_snow_layer);

    // Not enough snow around -> bare.
    assert!(!evaluate_better_snow(&c, "oak_fence", false, true, 1).emit_snow_layer);
    // Non-snowy plains -> bare.
    assert!(!evaluate_better_snow(&c, "oak_fence", false, true, 0).emit_snow_layer);
    // Floating in the air -> no layer.
    assert!(!evaluate_better_snow(&c, "oak_fence", false, false, 4).emit_snow_layer);
    // Full blocks and disabled feature -> nothing.
    assert!(!evaluate_better_snow(&c, "stone", false, true, 4).emit_snow_layer);
    assert!(!evaluate_better_snow(&cfg(), "oak_fence", false, true, 4).emit_snow_layer);
}

// ---------------------------------------------------------------------------
// 3D block relief
// ---------------------------------------------------------------------------

fn relief(c: &VanillaPlusVisualsConfig, subject: ReliefSubject) -> Option<ExtrudedMesh> {
    build_3d_relief_mesh(c, subject, false, 0.0)
}

#[test]
fn relief_disabled_or_deferred_returns_none() {
    // Feature off.
    assert!(relief(&cfg(), ReliefSubject::Bookshelf).is_none());
    // LOD cutoff.
    let mut c = cfg();
    c.block_relief_3d = true;
    assert!(build_3d_relief_mesh(&c, ReliefSubject::Bookshelf, false, 21.0).is_none());
    assert!(build_3d_relief_mesh(&c, ReliefSubject::Bookshelf, false, 20.0).is_some());
    // Custom resource-pack model wins while respect flag is on.
    assert!(build_3d_relief_mesh(&c, ReliefSubject::Bookshelf, true, 0.0).is_none());
    c.respect_custom_resourcepack = false;
    assert!(build_3d_relief_mesh(&c, ReliefSubject::Bookshelf, true, 0.0).is_some());
}

#[test]
fn furnace_relief_lit_vs_unlit() {
    let mut c = cfg();
    c.block_relief_3d = true;

    let unlit = relief(
        &c,
        ReliefSubject::Furnace {
            lit: false,
            facing: Direction::South,
        },
    )
    .expect("unlit furnace relief");
    assert_block_local(&unlit);
    assert_unit_normals(&unlit);
    assert!(unlit.quads.iter().all(|q| !is_emissive_relief_quad(q)));
    // Two recesses: 5 quads each.
    assert_eq!(unlit.quads.len(), 10);
    // Every firebox/slot quad is recessed inside the South face.
    for q in &unlit.quads {
        if (q.normal[2] - 1.0).abs() < 1e-4 {
            is_recessed_on(q, Direction::South, TEXEL);
        }
    }

    let lit = relief(
        &c,
        ReliefSubject::Furnace {
            lit: true,
            facing: Direction::South,
        },
    )
    .expect("lit furnace relief");
    assert_block_local(&lit);
    let emissive: Vec<_> = lit.quads.iter().filter(|q| is_emissive_relief_quad(q)).collect();
    assert_eq!(emissive.len(), 1, "exactly one tagged ember quad");
    // Ember sits inside the firebox pocket, in front of its back panel.
    let ember = emissive[0];
    assert!((ember.normal[2] - 1.0).abs() < 1e-4);
    for p in &ember.positions {
        assert!(p[2] <= 1.0 - TEXEL + 1e-5 && p[2] > 0.5);
    }
}

#[test]
fn furnace_relief_rotates_all_cardinals() {
    let mut c = cfg();
    c.block_relief_3d = true;
    for facing in [
        Direction::South,
        Direction::East,
        Direction::North,
        Direction::West,
    ] {
        let mesh = relief(
            &c,
            ReliefSubject::Furnace {
                lit: false,
                facing,
            },
        )
        .expect("furnace relief");
        assert_block_local(&mesh);
        let want = outward(facing);
        assert!(
            mesh.quads.iter().any(|q| {
                (q.normal[0] - want[0]).abs() < 1e-4
                    && (q.normal[1] - want[1]).abs() < 1e-4
                    && (q.normal[2] - want[2]).abs() < 1e-4
            }),
            "facing {facing:?}: no outward-facing recessed panel"
        );
    }
}

#[test]
fn bookshelf_relief_recesses_all_four_sides() {
    let mut c = cfg();
    c.block_relief_3d = true;
    let mesh = relief(&c, ReliefSubject::Bookshelf).expect("bookshelf relief");
    assert_block_local(&mesh);
    assert_unit_normals(&mesh);
    assert_eq!(mesh.quads.len(), 40); // 4 sides x 2 bands x 5 quads
    for dir in [
        Direction::South,
        Direction::East,
        Direction::North,
        Direction::West,
    ] {
        let want = outward(dir);
        let axis = axis_of(dir);
        let depth = if want[axis] > 0.0 { 1.0 - TEXEL } else { TEXEL };
        // Back panels face outward AND sit flat at exactly the recess depth;
        // pocket walls on neighbouring faces share the normal but span the
        // depth range instead.
        let panels: Vec<_> = mesh
            .quads
            .iter()
            .filter(|q| {
                (q.normal[0] - want[0]).abs() < 1e-4
                    && (q.normal[1] - want[1]).abs() < 1e-4
                    && (q.normal[2] - want[2]).abs() < 1e-4
                    && q.positions.iter().all(|p| (p[axis] - depth).abs() < 1e-5)
            })
            .collect();
        assert_eq!(panels.len(), 2, "{dir:?} should have 2 recessed book panels");
        for q in panels {
            is_recessed_on(q, dir, TEXEL);
        }
    }
}

#[test]
fn jukebox_relief_recesses_top_slot() {
    let mut c = cfg();
    c.block_relief_3d = true;
    let mesh = relief(&c, ReliefSubject::JukeboxTop).expect("jukebox relief");
    assert_block_local(&mesh);
    assert_eq!(mesh.quads.len(), 5);
    let back = mesh
        .quads
        .iter()
        .find(|q| (q.normal[1] - 1.0).abs() < 1e-4)
        .expect("top-facing back panel");
    for p in &back.positions {
        assert!((p[1] - (1.0 - TEXEL)).abs() < 1e-5, "slot depth {p:?}");
    }
}

#[test]
fn door_and_trapdoor_window_frames() {
    let mut c = cfg();
    c.block_relief_3d = true;
    let door = relief(
        &c,
        ReliefSubject::DoorWindow {
            facing: Direction::East,
        },
    )
    .expect("door window relief");
    assert_block_local(&door);
    assert_unit_normals(&door);
    assert_eq!(door.quads.len(), 30); // 5 bars x 6 faces

    let trapdoor = relief(
        &c,
        ReliefSubject::TrapdoorWindow {
            facing: Direction::North,
        },
    )
    .expect("trapdoor window relief");
    assert_block_local(&trapdoor);
    assert_eq!(trapdoor.quads.len(), 30);
    // Bars are 2 texels thick and stay near the slab plane.
    for q in &trapdoor.quads {
        for p in &q.positions {
            assert!(p[1] <= 2.0 * TEXEL + 1e-5);
        }
    }
}

// ---------------------------------------------------------------------------
// 3D skin layers
// ---------------------------------------------------------------------------

#[test]
fn skin_layer_disabled_or_distant_is_flat() {
    let skin = skin_with(&[(40, 8)]);
    assert_eq!(
        build_3d_skin_layer_part(&cfg(), &skin, SkinOverlayPart::Hat, false, 0.0),
        SkinLayerResult::UseFlatOverlay
    );
    let mut c = cfg();
    c.skin_3d_layers = true;
    assert_eq!(
        build_3d_skin_layer_part(&c, &skin, SkinOverlayPart::Hat, false, 17.0),
        SkinLayerResult::UseFlatOverlay
    );
    assert!(matches!(
        build_3d_skin_layer_part(&c, &skin, SkinOverlayPart::Hat, false, 16.0),
        SkinLayerResult::Extruded(_)
    ));
}

#[test]
fn skin_layer_invalid_buffer_fails_closed() {
    let mut c = cfg();
    c.skin_3d_layers = true;
    for len in [0usize, 64, SKIN_RGBA_BYTES - 1, SKIN_RGBA_BYTES + 4] {
        let buf = vec![0u8; len];
        assert_eq!(
            build_3d_skin_layer_part(&c, &buf, SkinOverlayPart::Hat, false, 0.0),
            SkinLayerResult::InvalidSkin,
            "len {len}"
        );
    }
}

#[test]
fn skin_layer_transparent_overlay_emits_nothing() {
    let mut c = cfg();
    c.skin_3d_layers = true;
    let skin = vec![0u8; SKIN_RGBA_BYTES];
    let SkinLayerResult::Extruded(mesh) =
        build_3d_skin_layer_part(&c, &skin, SkinOverlayPart::Hat, false, 0.0)
    else {
        panic!("expected Extruded");
    };
    assert_eq!(mesh.quad_count(), 0);
}

#[test]
fn skin_layer_opaque_pixel_extrudes_cleanly() {
    let mut c = cfg();
    c.skin_3d_layers = true;
    // One opaque texel on the hat front region (40,8)-(48,16).
    let skin = skin_with(&[(40, 8)]);
    let SkinLayerResult::Extruded(mesh) =
        build_3d_skin_layer_part(&c, &skin, SkinOverlayPart::Hat, false, 0.0)
    else {
        panic!("expected Extruded");
    };
    // Front + back + four perimeter walls, nothing hidden.
    assert_eq!(mesh.quad_count(), 6);
    assert_unit_normals(&mesh);
    let max_z = mesh
        .quads
        .iter()
        .flat_map(|q| q.positions.iter().map(|p| p[2]))
        .fold(f32::MIN, f32::max);
    assert_eq!(max_z, 8.5, "extruded half a texel past the 8-deep head");
    let normals: Vec<[f32; 3]> = mesh.quads.iter().map(|q| q.normal).collect();
    assert!(normals.iter().any(|n| (n[2] - 1.0).abs() < 1e-4));
    assert!(normals.iter().any(|n| (n[2] + 1.0).abs() < 1e-4));
}

#[test]
fn skin_layer_slim_vs_classic_arm_width() {
    let mut c = cfg();
    c.skin_3d_layers = true;
    // One opaque texel on the right sleeve front region (44,36)-(48,48).
    let skin = skin_with(&[(44, 36)]);

    let SkinLayerResult::Extruded(classic) =
        build_3d_skin_layer_part(&c, &skin, SkinOverlayPart::RightSleeve, false, 0.0)
    else {
        panic!("expected Extruded");
    };
    let SkinLayerResult::Extruded(slim) =
        build_3d_skin_layer_part(&c, &skin, SkinOverlayPart::RightSleeve, true, 0.0)
    else {
        panic!("expected Extruded");
    };
    let max_x = |m: &ExtrudedMesh| {
        m.quads
            .iter()
            .flat_map(|q| q.positions.iter().map(|p| p[0]))
            .fold(f32::MIN, f32::max)
    };
    assert_eq!(max_x(&classic), 4.0, "classic Steve arm is 4 texels wide");
    assert_eq!(max_x(&slim), 3.0, "slim Alex arm is 3 texels wide");
    assert_unit_normals(&classic);
    assert_unit_normals(&slim);
}

#[test]
fn skin_layer_jacket_extrudes_when_drawn() {
    let mut c = cfg();
    c.skin_3d_layers = true;
    // Fill the whole jacket front region (20,36)-(28,48).
    let pixels: Vec<(usize, usize)> = (20..28).flat_map(|x| (36..48).map(move |y| (x, y))).collect();
    let skin = skin_with(&pixels);
    let SkinLayerResult::Extruded(mesh) =
        build_3d_skin_layer_part(&c, &skin, SkinOverlayPart::Jacket, false, 0.0)
    else {
        panic!("expected Extruded");
    };
    // A solid 8x12 front panel merges to 1 front + 1 back + 4 walls = 6 quads.
    assert_eq!(mesh.quad_count(), 6);
    assert_unit_normals(&mesh);
}
