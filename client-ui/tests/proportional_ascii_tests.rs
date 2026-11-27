// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_ui::draw::{
    glyph_advance, rasterize_draw_list, tessellate_draw_list, text_width, UiColor, UiDrawList,
};
use megu_client_ui::font_cjk::{install_external_font, GlyphAtlas};

#[test]
fn active_ascii_face_shares_proportional_metrics_with_both_renderers() {
    assert_eq!(glyph_advance('i'), 2);
    assert_eq!(glyph_advance('t'), 4);
    assert_eq!(glyph_advance('M'), 6);
    assert_eq!(text_width("Options...", 1), 42);
    assert_eq!(text_width("Options...", 2), 84);

    let mut list = UiDrawList::new();
    list.text(2, 1, 2, UiColor::rgb(255, 255, 255), "i t");
    let builtin_mesh = tessellate_draw_list(32, 24, &list);
    assert_eq!(builtin_mesh.vertices[0].position[0], 2.0);
    assert_eq!(builtin_mesh.vertices[4].position[0], 14.0);

    // Synthetic left-padded ASCII glyphs exercise the same path as a
    // launcher-installed bitmap font.
    let mut bitmap = Box::new([[0; 8]; 128]);
    bitmap['i' as usize][0] = 0b0001_1000;
    bitmap['t' as usize][0] = 0b0000_1110;
    let mut atlas = GlyphAtlas::default();
    atlas.set_ascii_bitmap(bitmap);
    install_external_font(atlas);

    assert_eq!(glyph_advance('i'), 3);
    assert_eq!(text_width("i t", 2), 22);
    let mesh = tessellate_draw_list(32, 24, &list);
    let painted_x: Vec<_> = mesh
        .vertices
        .as_chunks::<4>()
        .0
        .iter()
        .map(|quad| quad[0].position[0])
        .collect();
    assert_eq!(painted_x, [2.0, 4.0, 16.0, 18.0, 20.0]);
    let pixels = rasterize_draw_list(32, 24, &list);
    let ink: Vec<_> = (0..32).filter(|&x| pixels[(32 + x) * 4 + 3] != 0).collect();
    assert_eq!(ink, [2, 3, 4, 5, 16, 17, 18, 19, 20, 21]);
}
