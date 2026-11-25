// SPDX-License-Identifier: LGPL-3.0-or-later

use std::path::PathBuf;

use megu_client_gpu::renderer::{
    block_selection_outline_lines, screenshot_output_path, write_rgba_png,
};
use megu_client_gpu::vanilla_assets_bridge::VanillaRuntimeAssets;

#[test]
fn selection_outline_is_a_twelve_edge_inflated_box() {
    let lines = block_selection_outline_lines([2, 3, 4]);
    assert_eq!(lines.len(), 12);
    assert!(lines.iter().all(|line| line.rgba == [0, 0, 0, 115]));
    assert!(lines.iter().any(|line| line.start[0] < 2.0));
    assert!(lines.iter().any(|line| line.end[1] > 4.0));
}

#[test]
fn screenshot_path_is_stable_and_scoped_to_game_dir() {
    assert_eq!(
        screenshot_output_path(std::path::Path::new("/game"), 42),
        PathBuf::from("/game/screenshots/Megu_42.png")
    );
}

#[test]
fn png_helper_encodes_rgba_and_rejects_wrong_buffer_size() {
    let path = std::env::temp_dir().join(format!("megu-visuals-{}.png", std::process::id()));
    let pixels = [255, 0, 0, 255, 0, 255, 0, 128];
    write_rgba_png(&path, 2, 1, &pixels).unwrap();
    let encoded = std::fs::read(&path).unwrap();
    let decoded = megu_client_mesh::png_decode::decode_png_rgba(&encoded).unwrap();
    assert_eq!((decoded.0, decoded.1), (2, 1));
    assert_eq!(decoded.2, pixels);
    std::fs::remove_file(path).unwrap();
    assert!(write_rgba_png(std::path::Path::new("unused.png"), 2, 1, &[0; 4]).is_err());
}

#[test]
fn missing_local_assets_build_a_diagnostic_fallback_bundle() {
    let bundle = VanillaRuntimeAssets::load_runtime_assets(None, None, None, &[], "ja_jp");
    let summary = bundle.summary();
    assert_eq!(summary.locale, "ja_jp");
    assert_eq!(summary.world_textures, 27);
    assert!(!bundle.mob_effect_uvs.is_empty());
    assert!(bundle.gui_textures.width >= 256);
    assert!(summary.notes > 0);
}
