// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu-client-ui`: the complete zero-dependency pure-Rust client UI.
//!
//! Three layers, all `std`-only:
//!
//! * [`draw`] — the deterministic [`draw::UiDrawList`] command buffer, an
//!   original built-in bitmap font (ASCII plus the [`font_cjk`] CJK/kana/
//!   Hangul/extended-Latin coverage), GPU quad tessellation with a
//!   companion WGSL shader ([`draw::UI_QUAD_WGSL`]), a software RGBA
//!   rasterizer ([`draw::rasterize_draw_list`]) and a dependency-free PNG
//!   encoder ([`draw::encode_png_rgba`]).
//! * [`font_cjk`] — the extended glyph table and the pure-`std` external
//!   GNU Unifont `.hex` font pack loader ([`font_cjk::GlyphAtlas`]).
//! * [`state`] — the screen state machine: title, tabbed settings
//!   (Video/Audio/QoL/Fair-Play/Language), friends invite codes, mod-sync
//!   consent, in-game HUD, pause, and the NotEnoughCrashes-style crash
//!   recovery screen — plus the unified keyboard/mouse/gamepad input
//!   router that maps [`state::UiInputEvent`]s to [`state::UiAction`]s.
//! * [`layout`] — responsive builders that lay every screen out across
//!   `640×360` through `3840×2160` (`ui_scale` aware), producing each
//!   frame's `(UiDrawList, Vec<HitRegion>)`.
//!
//! Typical frame loop for an embedder:
//!
//! ```no_run
//! use megu_client_ui::state::UiState;
//! use megu_client_ui::draw::rasterize_draw_list;
//!
//! let mut ui = UiState::new();
//! let list = ui.render(1280, 720);          // layout + hit-region cache
//! let rgba = rasterize_draw_list(1280, 720, &list); // or tessellate for GPU
//! # assert_eq!(rgba.len(), 1280 * 720 * 4);
//! ```

pub mod container_screen;
pub mod death_screen;
pub mod draw;
pub mod font_cjk;
pub mod hud_widgets;
pub mod layout;
pub mod magic_hud;
pub mod mod_i18n;
pub mod mod_menu;
pub mod recipe_browser;
pub mod shader_screen;
pub mod state;
pub mod subtitle_hud;
