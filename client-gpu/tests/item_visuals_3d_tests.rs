// SPDX-License-Identifier: LGPL-3.0-or-later
use std::collections::BTreeMap;
use std::path::PathBuf;

use megu_client_gpu::item_visuals_3d::{
    build_winit_window_icon, composite_enchanted_glint_icon, discover_official_builtin_packs,
    enchanted_glint_uv_transform, merged_end_credits_lines, render_isometric_block_icon_rgba,
    upgrade_item_icon_atlas_with_3d_blocks, SpriteRgba,
};
use megu_client_gpu::vanilla_i18n_data::VanillaTextsAndFonts;
use megu_client_gpu::vanilla_official_extras::{
    BuiltinResourcePack, CreditDiscipline, CreditSection, VanillaOfficialExtras, WindowIconRgba,
};

fn solid_rgba(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    for _ in 0..width * height {
        out.extend_from_slice(&rgba);
    }
    out
}

fn pixel(image: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
    let index = (y as usize * size as usize + x as usize) * 4;
    image[index..index + 4].try_into().unwrap()
}

#[test]
fn isometric_cube_has_hexagonal_mask_and_directional_face_lighting() {
    let gray = solid_rgba(4, 4, [200, 200, 200, 255]);
    let sprite = SpriteRgba::new(4, 4, &gray);
    let icon = render_isometric_block_icon_rgba(&sprite, &sprite, &sprite, 32, None);

    assert_eq!(icon.len(), 32 * 32 * 4);
    for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31)] {
        assert_eq!(
            pixel(&icon, 32, x, y)[3],
            0,
            "corner {x},{y} must be transparent"
        );
    }
    assert!(pixel(&icon, 32, 16, 16)[3] != 0);
    let opaque = icon
        .iter()
        .skip(3)
        .step_by(4)
        .filter(|alpha| **alpha != 0)
        .count();
    assert!(
        opaque > 600,
        "hexagonal cube silhouette should cover most of the icon"
    );

    assert_eq!(pixel(&icon, 32, 16, 8), [200, 200, 200, 255]);
    assert_eq!(pixel(&icon, 32, 8, 19), [160, 160, 160, 255]);
    assert_eq!(pixel(&icon, 32, 24, 19), [120, 120, 120, 255]);
}

#[test]
fn isometric_cube_applies_grass_top_tint_only_to_top_face() {
    let gray = solid_rgba(4, 4, [200, 200, 200, 255]);
    let sprite = SpriteRgba::new(4, 4, &gray);
    let icon =
        render_isometric_block_icon_rgba(&sprite, &sprite, &sprite, 32, Some([100, 200, 50]));

    assert_eq!(pixel(&icon, 32, 16, 8), [78, 157, 39, 255]);
    assert_eq!(pixel(&icon, 32, 8, 19), [160, 160, 160, 255]);
    assert_eq!(pixel(&icon, 32, 24, 19), [120, 120, 120, 255]);
}

#[test]
fn atlas_upgrade_generates_icons_for_cube_block_entries() {
    let gray = solid_rgba(4, 4, [200, 200, 200, 255]);
    let sprite = SpriteRgba::new(4, 4, &gray);
    let mut blocks = BTreeMap::new();
    blocks.insert(
        "grass_block".to_string(),
        (sprite, sprite, Some([100, 200, 50])),
    );
    blocks.insert("stone".to_string(), (sprite, sprite, None));

    let upgraded = upgrade_item_icon_atlas_with_3d_blocks(&blocks, 16);
    assert_eq!(upgraded.len(), 2);
    assert_eq!(upgraded["grass_block"].len(), 16 * 16 * 4);
    assert_eq!(upgraded["stone"].len(), 16 * 16 * 4);
    assert!(upgraded["grass_block"]
        .iter()
        .skip(3)
        .step_by(4)
        .any(|alpha| *alpha != 0));
}

#[test]
fn enchanted_glint_scroll_is_monotonic_before_wrap_and_uses_item_tilt() {
    let (uv0, angle0) = enchanted_glint_uv_transform(0.10);
    let (uv1, angle1) = enchanted_glint_uv_transform(0.20);
    assert_eq!(angle0, -50.0);
    assert_eq!(angle1, -50.0);
    assert!(uv1[0] < uv0[0], "vanilla U glint scroll moves negative");
    assert!(uv1[1] > uv0[1], "vanilla V glint scroll moves positive");
    assert!(uv0.iter().chain(uv1.iter()).all(|v| v.is_finite()));
}

#[test]
fn enchanted_glint_composite_only_touches_nontransparent_base_pixels() {
    let mut base = Vec::new();
    for _y in 0..8 {
        for x in 0..8 {
            if x < 4 {
                base.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                base.extend_from_slice(&[10, 10, 10, 255]);
            }
        }
    }
    let glint = solid_rgba(2, 2, [120, 40, 220, 255]);
    let out = composite_enchanted_glint_icon(&base, 8, 8, Some((&glint, 2, 2)), 0.25);

    assert_eq!(out.len(), base.len());
    for y in 0..8 {
        for x in 0..8 {
            let before = pixel(&base, 8, x, y);
            let after = pixel(&out, 8, x, y);
            if x < 4 {
                assert_eq!(
                    after, before,
                    "transparent pixel {x},{y} must remain untouched"
                );
            } else {
                assert!(after[0] > before[0] || after[1] > before[1] || after[2] > before[2]);
                assert_eq!(after[3], before[3]);
            }
        }
    }
}

fn window_icon(size: u32, fill: u8) -> WindowIconRgba {
    WindowIconRgba {
        width: size,
        height: size,
        rgba: solid_rgba(size, size, [fill, fill, fill, 255]),
    }
}

#[test]
fn winit_window_icon_accepts_synthetic_32x32_rgba() {
    let mut extras = VanillaOfficialExtras::default();
    extras.window_icons.push(window_icon(48, 48));
    extras.window_icons.push(window_icon(32, 32));
    extras.window_icons.push(window_icon(128, 128));

    assert!(build_winit_window_icon(&extras).is_some());
}

#[test]
fn official_builtin_pack_discovery_is_canonical_and_complete() {
    let mut extras = VanillaOfficialExtras::default();
    extras.builtin_resource_packs.push(BuiltinResourcePack {
        id: "high_contrast".into(),
        display_name: "High Contrast".into(),
        object_path: PathBuf::from("/assets/objects/hc/hash"),
    });
    extras.builtin_resource_packs.push(BuiltinResourcePack {
        id: "programmer_art".into(),
        display_name: "Programmer Art".into(),
        object_path: PathBuf::from("/assets/objects/pa/hash"),
    });

    let packs = discover_official_builtin_packs(&extras);
    assert_eq!(
        packs,
        vec![
            (
                "programmer_art".to_string(),
                "Programmer Art".to_string(),
                PathBuf::from("/assets/objects/pa/hash")
            ),
            (
                "high_contrast".to_string(),
                "High Contrast".to_string(),
                PathBuf::from("/assets/objects/hc/hash")
            ),
        ]
    );
}

#[test]
fn merged_end_credits_combine_poem_structured_credits_and_postcredits() {
    let texts = VanillaTextsAndFonts {
        end_text: Some("end one\n\nend two\n".into()),
        postcredits_text: Some("\npostcredits\n".into()),
        ..Default::default()
    };
    let mut extras = VanillaOfficialExtras::default();
    extras.credits.push(CreditSection {
        section: "Mojang Studios".into(),
        disciplines: vec![CreditDiscipline {
            name: "Programming".into(),
            credits: vec!["Ada".into()],
        }],
    });

    assert_eq!(
        merged_end_credits_lines(&texts, &extras),
        vec![
            "end one",
            "",
            "end two",
            "",
            "Mojang Studios",
            "Programming",
            "Ada",
            "",
            "postcredits",
        ]
    );
}
