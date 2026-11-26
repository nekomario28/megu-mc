// SPDX-License-Identifier: LGPL-3.0-or-later
//! Focused scissor/clipping regressions for the shared UI draw list.

use megu_client_ui::draw::{
    rasterize_draw_list, tessellate_draw_list, GuiSpriteAtlas, GuiSpriteUv, ItemIconAtlas, UiColor,
    UiDrawList, UiRect,
};
use megu_client_ui::gui_bridge::{
    lower_bridge_batch_to_draw_list, GuiBridgeBatch, GuiBridgeDrawOp,
};
use megu_client_ui::retained_cache::UiRetainedCache;
use megu_core::rev::UiLayer;

fn assert_vertices_inside(mesh: &megu_client_ui::draw::UiMesh, clip: UiRect) {
    for vertex in &mesh.vertices {
        assert!(
            vertex.position[0] >= clip.x as f32
                && vertex.position[0] <= clip.right() as f32
                && vertex.position[1] >= clip.y as f32
                && vertex.position[1] <= clip.bottom() as f32,
            "vertex {:?} escaped clip {clip:?}",
            vertex.position
        );
    }
}

#[test]
fn fill_rect_and_text_vertices_never_escape_scissor() {
    let clip = UiRect::new(10, 8, 12, 9);
    let mut list = UiDrawList::new();
    list.fill_rect(UiRect::new(12, 9, 1, 1), UiColor::rgb(0, 0, 0));
    list.push_scissor(clip);
    list.fill_rect(UiRect::new(0, 0, 32, 32), UiColor::rgb(255, 0, 0));
    list.text(8, 8, 1, UiColor::rgb(255, 255, 255), "A");
    assert_eq!(list.command_clip(0), None);
    assert_eq!(list.command_clip(1), Some(clip));
    let mesh = tessellate_draw_list(64, 64, &list);
    assert!(!mesh.vertices.is_empty());
    assert_vertices_inside(&mesh, clip);

    // The built-in ASCII glyph is a single quad; clipping x=8..16 to
    // x=10..22 keeps the whole glyph width inside this scissor, but the
    // first fill rect proves partial quad geometry rather than whole-command
    // dropping: it must occupy exactly the scissor intersection.
    let fill_widths: Vec<f32> = mesh
        .vertices
        .iter()
        .filter(|v| v.color[..3] == [1.0, 0.0, 0.0])
        .map(|v| v.position[0])
        .collect();
    assert!(fill_widths.contains(&10.0));
    assert!(fill_widths.contains(&22.0));
}

#[test]
fn fully_clipped_commands_emit_no_vertices() {
    let mut list = UiDrawList::new();
    list.push_scissor(UiRect::new(10, 10, 4, 4));
    list.fill_rect(UiRect::new(30, 30, 8, 8), UiColor::rgb(255, 0, 0));
    list.text(30, 30, 1, UiColor::rgb(255, 255, 255), "A");
    assert!(tessellate_draw_list(64, 64, &list).vertices.is_empty());
}

#[test]
fn textured_quad_clipping_crops_uvs_proportionally() {
    let mut list = UiDrawList::new();
    list.push_scissor(UiRect::new(10, 0, 10, 20));
    list.texture_uv(
        UiRect::new(5, 0, 20, 10),
        GuiSpriteUv {
            uv_min: [0.2, 0.1],
            uv_max: [0.8, 0.9],
            width: 20,
            height: 10,
            nine_slice_border: [0; 4],
        },
    );
    let mesh = tessellate_draw_list(64, 64, &list);
    assert_eq!(mesh.vertices.len(), 4);
    let min_x = mesh
        .vertices
        .iter()
        .map(|v| v.position[0])
        .fold(f32::INFINITY, f32::min);
    let max_x = mesh
        .vertices
        .iter()
        .map(|v| v.position[0])
        .fold(f32::NEG_INFINITY, f32::max);
    assert_eq!((min_x, max_x), (10.0, 20.0));

    // Geometry removed the left 25% and right 25% of the source quad, so
    // u crops 0.2..0.8 to 0.35..0.65. The sprite-atlas sentinel biases u by
    // -4.0 in the mesh stream.
    let mut us: Vec<f32> = mesh.vertices.iter().map(|v| v.uv[0]).collect();
    us.sort_by(f32::total_cmp);
    assert_eq!(us.first().copied().unwrap(), 0.35 - 4.0);
    assert_eq!(us.last().copied().unwrap(), 0.65 - 4.0);
    let mut vs: Vec<f32> = mesh.vertices.iter().map(|v| v.uv[1]).collect();
    vs.sort_by(f32::total_cmp);
    assert_eq!(vs.first().copied().unwrap(), 0.1);
    assert_eq!(vs.last().copied().unwrap(), 0.9);
}

#[test]
fn nested_disjoint_scissor_stays_empty_after_child_push() {
    let mut list = UiDrawList::new();
    list.push_scissor(UiRect::new(0, 0, 4, 4));
    list.push_scissor(UiRect::new(20, 20, 4, 4));
    list.fill_rect(UiRect::new(0, 0, 64, 64), UiColor::rgb(255, 0, 0));
    assert!(tessellate_draw_list(64, 64, &list).vertices.is_empty());
}

#[test]
fn software_rasterizer_clips_fill_and_text_pixels() {
    let red = UiColor::rgba(255, 0, 0, 255);
    let white = UiColor::rgba(255, 255, 255, 255);
    let mut list = UiDrawList::new();
    list.push_scissor(UiRect::new(4, 0, 2, 8));
    list.fill_rect(UiRect::new(0, 0, 8, 2), red);
    list.text(0, 2, 1, white, "A");
    let px = rasterize_draw_list(8, 12, &list);
    let alpha_at = |x: usize, y: usize| px[(y * 8 + x) * 4 + 3];
    for y in 0..2 {
        assert_eq!(alpha_at(3, y), 0);
        assert_eq!(alpha_at(4, y), 255);
        assert_eq!(alpha_at(5, y), 255);
        assert_eq!(alpha_at(6, y), 0);
    }
    // Glyph pixels may be sparse, but no pixel outside x=4..6 may be lit.
    for y in 2..10 {
        for x in 0..8 {
            if !(4..6).contains(&x) {
                assert_eq!(alpha_at(x, y), 0, "pixel {x},{y} escaped text clip");
            }
        }
    }
}

#[test]
fn gui_bridge_lowering_preserves_per_command_scissor_metadata() {
    let batch = GuiBridgeBatch::new(vec![
        GuiBridgeDrawOp::PushScissor {
            x: 10,
            y: 0,
            w: 10,
            h: 20,
        },
        GuiBridgeDrawOp::DrawText {
            x: 8,
            y: 0,
            scale: 1,
            rgba: [255, 255, 255, 255],
            text: "A".to_owned(),
        },
        GuiBridgeDrawOp::BlitAtlas {
            x: 5,
            y: 0,
            w: 20,
            h: 10,
            texture_id: "test:atlas".to_owned(),
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: [200, 100, 50, 255],
        },
        GuiBridgeDrawOp::PopScissor,
    ]);
    let list = lower_bridge_batch_to_draw_list(&batch, 64, 64);
    assert_eq!(list.commands.len(), 2);
    assert_eq!(
        list.command_clip(0),
        Some(UiRect::new(10, 0, 10, 20)),
        "text must keep partial-glyph clip metadata"
    );
    assert_eq!(list.command_clip(1), Some(UiRect::new(10, 0, 10, 20)));

    let mesh = tessellate_draw_list(64, 64, &list);
    assert!(!mesh.vertices.is_empty());
    assert_vertices_inside(&mesh, UiRect::new(10, 0, 10, 20));
    assert!(
        mesh.vertices.iter().any(|v| {
            (v.color[0] - 200.0 / 255.0).abs() < 1e-6
                && (v.color[1] - 100.0 / 255.0).abs() < 1e-6
                && (v.color[2] - 50.0 / 255.0).abs() < 1e-6
        }),
        "BlitAtlas tint must survive the native UV encoding"
    );
}

#[test]
fn gui_bridge_non_unit_atlas_uvs_keep_tinted_footprint_fallback() {
    let batch = GuiBridgeBatch::new(vec![GuiBridgeDrawOp::BlitAtlas {
        x: 1,
        y: 2,
        w: 8,
        h: 8,
        texture_id: "test:tiled".to_owned(),
        u0: 0.0,
        v0: 0.0,
        u1: 2.0,
        v1: 1.0,
        tint: [9, 8, 7, 255],
    }]);
    let list = lower_bridge_batch_to_draw_list(&batch, 64, 64);
    assert_eq!(list.commands.len(), 1);
    assert!(matches!(
        list.commands[0],
        megu_client_ui::draw::UiCommand::FillRect { .. }
    ));
}

#[test]
fn retained_cache_tessellation_preserves_clip_metadata() {
    let clip = UiRect::new(3, 2, 5, 5);
    let mut list = UiDrawList::new();
    list.push_scissor(clip);
    list.fill_rect(UiRect::new(0, 0, 16, 16), UiColor::rgb(255, 0, 0));
    let mut cache = UiRetainedCache::new();
    cache.update_layer(
        UiLayer::Overlay,
        &list,
        32,
        32,
        &ItemIconAtlas::default(),
        &GuiSpriteAtlas::default(),
    );
    let (vertices, _) = cache.layer_mesh(UiLayer::Overlay);
    assert_eq!(vertices.len(), 4);
    for vertex in vertices {
        assert!(vertex.position[0] >= clip.x as f32);
        assert!(vertex.position[0] <= clip.right() as f32);
        assert!(vertex.position[1] >= clip.y as f32);
        assert!(vertex.position[1] <= clip.bottom() as f32);
    }
}

#[test]
fn draw_list_equality_includes_render_visible_scissor_metadata() {
    let mut unclipped = UiDrawList::new();
    unclipped.fill_rect(UiRect::new(0, 0, 8, 8), UiColor::rgb(255, 0, 0));
    let mut clipped = UiDrawList::new();
    clipped.push_scissor(UiRect::new(2, 2, 4, 4));
    clipped.fill_rect(UiRect::new(0, 0, 8, 8), UiColor::rgb(255, 0, 0));
    assert_eq!(unclipped.commands, clipped.commands);
    assert_ne!(
        unclipped, clipped,
        "identical command streams with different scissors render differently"
    );
}

#[test]
fn trademark_hygiene_audit_accepts_clean_tree_and_rejects_synthetic_branding() {
    let root = std::env::temp_dir().join(format!(
        "megu-trademark-audit-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("client-ui/src")).unwrap();
    std::fs::create_dir_all(root.join("core/src")).unwrap();
    std::fs::write(root.join("core/src/sys_metrics.rs"), "// clean fixture\n").unwrap();
    let mod_manager = root.join("client-ui/src/mod_manager.rs");
    std::fs::write(
        &mod_manager,
        "pub fn builtin_label() -> &'static str { \"Mod manager\" }\n",
    )
    .unwrap();

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("tools/trademark_hygiene_audit.py");
    let run = || {
        std::process::Command::new("python3")
            .arg(&script)
            .arg("--strict")
            .arg("--root")
            .arg(&root)
            .output()
            .expect("run trademark hygiene audit")
    };
    let clean = run();
    assert!(
        clean.status.success(),
        "clean synthetic tree must pass: {}",
        String::from_utf8_lossy(&clean.stdout)
    );

    std::fs::write(
        &mod_manager,
        "pub fn builtin_label() -> &'static str { \"MiniHUD overlay\" }\n",
    )
    .unwrap();
    let branded = run();
    assert!(
        !branded.status.success(),
        "a synthetic third-party branded built-in label must fail the strict audit"
    );
    assert!(
        String::from_utf8_lossy(&branded.stdout).contains("MiniHUD"),
        "finding must name the rejected mark: {}",
        String::from_utf8_lossy(&branded.stdout)
    );

    std::fs::write(
        &mod_manager,
        "pub fn builtin_label() -> &'static str { \"MiniHUD overlay\" } // trademark-hygiene:allow\n",
    )
    .unwrap();
    let reasonless = run();
    assert!(
        !reasonless.status.success(),
        "an allow marker without a human-readable reason must not suppress findings"
    );

    std::fs::write(
        &mod_manager,
        "pub fn builtin_label() -> &'static str { \"Mod manager\" }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("core/src/sys_metrics.rs"),
        "pub struct MiniHudConfig;\n",
    )
    .unwrap();
    let core_api = run();
    assert!(
        !core_api.status.success(),
        "core sys_metrics public API names must be scanned after the HUD rename"
    );
    assert!(
        String::from_utf8_lossy(&core_api.stdout).contains("public-api-name"),
        "core finding must be the public API checker: {}",
        String::from_utf8_lossy(&core_api.stdout)
    );

    let _ = std::fs::remove_dir_all(&root);
}
