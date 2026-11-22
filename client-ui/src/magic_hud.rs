// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Optional magic overlays, driven by plain server-synced state.
//!
//! Render the returned draw list over the in-game HUD. An unused state has no
//! drawing cost or visual footprint. All coordinates are physical pixels.

use crate::draw::{palette, UiColor, UiDrawList, UiRect};

/// A spell currently being cast or channeled.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CastBarState {
    pub spell_label: String,
    /// Fraction completed; invalid/out-of-range values are normalized on draw.
    pub progress: f32,
    pub channeled: bool,
    /// While positive, the cast bar shows its interrupted state.
    pub interrupted_flash_ticks: u8,
}

/// A spell waiting to become available again (20 ticks per second).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CooldownSlotState {
    pub spell_label: String,
    pub remaining_ticks: u32,
    pub total_ticks: u32,
}

/// A magic-specific temporary status. Amplifier 0 means level I.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusBadgeState {
    pub label: String,
    pub remaining_ticks: u32,
    pub amplifier: u8,
    pub is_harmful: bool,
    pub shield_amount: Option<f32>,
}

/// Snapshot suitable for feeding from a magic-mod sync packet.
///
/// No mana bar is drawn without a finite positive `max_mana`. Active casts,
/// cooldowns, and custom statuses can still be shown for non-mana spells.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MagicHudState {
    pub mana: f32,
    pub max_mana: f32,
    pub active_cast: Option<CastBarState>,
    pub cooldowns: Vec<CooldownSlotState>,
    pub custom_statuses: Vec<StatusBadgeState>,
}

impl MagicHudState {
    /// Whether there is anything magic-specific to show this frame.
    pub fn is_empty(&self) -> bool {
        !has_mana_pool(self.max_mana)
            && self.active_cast.is_none()
            && !self.cooldowns.iter().any(|slot| slot.remaining_ticks > 0)
            && !self
                .custom_statuses
                .iter()
                .any(|status| status.remaining_ticks > 0)
    }
}

/// Maximum extra tooltip lines the looking-at overlay renders.
pub const LOOKING_AT_MAX_EXTRA_LINES: usize = 4;

/// Jade/WAILA-style "what am I looking at" overlay state
/// (`hud.jade_looking_at`).
///
/// Pure data: the game fills it from the block/entity under the crosshair
/// (plus mod-provided tooltip lines) and
/// [`crate::layout::layout_looking_at`] draws the panel while
/// [`is_visible`](Self::is_visible) holds. `enabled = false` — the
/// default — draws nothing even when populated, so the vanilla HUD is
/// untouched until a host opts in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LookingAtBlockState {
    /// Master switch; `false` suppresses the overlay entirely.
    pub enabled: bool,
    /// Registry id of the targeted block, e.g. `"minecraft:oak_log"`.
    pub block_id: String,
    /// Localized display name; [`name`](Self::name) falls back to
    /// `block_id` when this is empty.
    pub display_name: String,
    /// Target block position `[x, y, z]`; `None` omits the coordinate
    /// line.
    pub position: Option<[i32; 3]>,
    /// Owning mod id/name shown on the dim info line.
    pub mod_name: Option<String>,
    /// Required-tool hint, e.g. `"Needs diamond pickaxe"`.
    pub tool_hint: Option<String>,
    /// Extra mod-provided tooltip lines (rendered dim, capped at
    /// [`LOOKING_AT_MAX_EXTRA_LINES`]).
    pub extra_lines: Vec<String>,
}

impl LookingAtBlockState {
    /// `true` while enabled and a target (id or display name) exists.
    pub fn is_visible(&self) -> bool {
        self.enabled && !(self.block_id.is_empty() && self.display_name.is_empty())
    }

    /// The name line to render: `display_name`, else `block_id`.
    pub fn name(&self) -> &str {
        if self.display_name.is_empty() {
            &self.block_id
        } else {
            &self.display_name
        }
    }
}

const MARGIN: i32 = 8;
const PANEL_WIDTH: i32 = 194;
const BAR_HEIGHT: i32 = 25;
const COOLDOWN_HEIGHT: i32 = 29;
const STATUS_WIDTH: i32 = 156;
const STATUS_HEIGHT: i32 = 16;
const STATUS_GAP: i32 = 3;
const MAX_COOLDOWNS: usize = 8;
const MAX_STATUSES: usize = 6;

const MANA_COLOR: UiColor = UiColor::rgb(80, 139, 232);
const CAST_COLOR: UiColor = UiColor::rgb(173, 110, 231);
const CAST_INTERRUPTED: UiColor = UiColor::rgb(221, 78, 72);
const READY_COLOR: UiColor = UiColor::rgb(105, 176, 187);
const BAR_BACK: UiColor = UiColor::rgba(12, 17, 28, 225);

/// Return a finite fraction, even for corrupted or incomplete sync values.
fn unit_fraction(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn has_mana_pool(max_mana: f32) -> bool {
    max_mana.is_finite() && max_mana > 0.0
}

fn mana_fraction(mana: f32, max_mana: f32) -> f32 {
    if !has_mana_pool(max_mana) {
        return 0.0;
    }
    unit_fraction(mana / max_mana)
}

fn ticks_label(ticks: u32) -> String {
    let seconds = ticks.div_ceil(20);
    format!("{}s", seconds.min(999))
}

fn slot_time_label(ticks: u32, max_cells: usize) -> String {
    let seconds = ticks.div_ceil(20);
    if max_cells < 4 && seconds >= 100 {
        format!("{}m", seconds.div_ceil(60).min(99))
    } else {
        ticks_label(ticks)
    }
}

/// Keep labels inside their cells and prevent embedded newlines from moving text.
fn clipped_label(label: &str, max_cells: usize) -> String {
    let mut chars = label
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch });
    let visible: String = chars.by_ref().take(max_cells).collect();
    if chars.next().is_some() && max_cells >= 3 {
        let mut clipped: String = visible.chars().take(max_cells - 3).collect();
        clipped.push_str("...");
        clipped
    } else {
        visible
    }
}

/// Small logical-pixel wrapper over the shared draw-list API.
struct Canvas {
    list: UiDrawList,
    scale: u32,
}

impl Canvas {
    fn rect(&self, x: i32, y: i32, w: i32, h: i32) -> UiRect {
        let s = self.scale as i32;
        UiRect::new(x * s, y * s, (w * s) as u32, (h * s) as u32)
    }

    fn panel(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let rect = self.rect(x, y, w, h);
        self.list.fill_rect(rect, palette::PANEL);
        self.list
            .border_rect(rect, palette::PANEL_BORDER, self.scale);
    }

    fn text(&mut self, x: i32, y: i32, color: UiColor, text: &str) {
        self.list.text_str(
            x * self.scale as i32,
            y * self.scale as i32,
            self.scale,
            color,
            text,
        );
    }

    fn progress(&mut self, x: i32, y: i32, w: i32, fraction: f32, color: UiColor) {
        let rect = self.rect(x, y, w, 7);
        self.list
            .progress_bar(rect, fraction, color, BAR_BACK, palette::BUTTON_BORDER);
    }
}

/// Build an overlay that can be appended to any `UiDrawList` after the vanilla HUD.
///
/// The centered bars sit above the vanilla hotbar and vitals, while temporary
/// statuses occupy the upper-right corner. Cooldown slots and status badges
/// are capped to keep even small screens uncluttered.
pub fn build_magic_hud_draw_list(
    state: &MagicHudState,
    viewport_w: u32,
    viewport_h: u32,
) -> UiDrawList {
    if state.is_empty() || viewport_w < 80 || viewport_h < 140 {
        return UiDrawList::new();
    }

    let scale = (viewport_h / 360)
        .clamp(1, 4)
        .min((viewport_w / 320).max(1));
    let (w, h) = ((viewport_w / scale) as i32, (viewport_h / scale) as i32);
    let mut canvas = Canvas {
        list: UiDrawList::new(),
        scale,
    };
    let panel_w = PANEL_WIDTH.min(w - 2 * MARGIN);
    let panel_x = (w - panel_w) / 2;
    let mana_y = h - 68;
    let cast_y = h - 98;
    let cooldown_y = h - 128;
    let show_mana = has_mana_pool(state.max_mana);

    if show_mana {
        let fraction = mana_fraction(state.mana, state.max_mana);
        let label = if state.max_mana <= 9999.0 {
            let amount = if state.mana.is_nan() {
                0.0
            } else {
                state.mana.clamp(0.0, state.max_mana)
            };
            format!("Mana {amount:.0}/{:.0}", state.max_mana)
        } else {
            format!("Mana {:.0}%", fraction * 100.0)
        };
        canvas.panel(panel_x, mana_y, panel_w, BAR_HEIGHT);
        canvas.text(
            panel_x + 5,
            mana_y + 3,
            palette::TEXT,
            &clipped_label(&label, ((panel_w - 10) / 8) as usize),
        );
        canvas.progress(panel_x + 4, mana_y + 16, panel_w - 8, fraction, MANA_COLOR);
    }

    if let Some(cast) = &state.active_cast {
        let interrupted = cast.interrupted_flash_ticks > 0;
        let prefix = if interrupted {
            "Interrupted: "
        } else if cast.channeled {
            "Channeling: "
        } else {
            "Casting: "
        };
        canvas.panel(panel_x, cast_y, panel_w, BAR_HEIGHT);
        canvas.text(
            panel_x + 5,
            cast_y + 3,
            if interrupted {
                CAST_INTERRUPTED
            } else {
                palette::TEXT
            },
            &clipped_label(
                &format!("{prefix}{}", cast.spell_label),
                ((panel_w - 10) / 8) as usize,
            ),
        );
        canvas.progress(
            panel_x + 4,
            cast_y + 16,
            panel_w - 8,
            unit_fraction(cast.progress),
            if interrupted {
                CAST_INTERRUPTED
            } else {
                CAST_COLOR
            },
        );
    }

    let cooldowns: Vec<_> = state
        .cooldowns
        .iter()
        .filter(|slot| slot.remaining_ticks > 0)
        .take(MAX_COOLDOWNS)
        .collect();
    if !cooldowns.is_empty() {
        let slot_w = if w >= 560 {
            50
        } else if w >= 400 {
            42
        } else {
            34
        };
        let slot_gap = 3;
        let slots_per_row = ((w - 2 * MARGIN + slot_gap) / (slot_w + slot_gap)) as usize;
        let count = cooldowns.len().min(slots_per_row);
        let row_w = count as i32 * (slot_w + slot_gap) - slot_gap;
        let start_x = (w - row_w) / 2;
        for (i, cooldown) in cooldowns.iter().take(count).enumerate() {
            let x = start_x + i as i32 * (slot_w + slot_gap);
            let fraction = if cooldown.total_ticks == 0 {
                0.0
            } else {
                unit_fraction(cooldown.remaining_ticks as f32 / cooldown.total_ticks as f32)
            };
            canvas.panel(x, cooldown_y, slot_w, COOLDOWN_HEIGHT);
            let label_cells = ((slot_w - 8) / 8) as usize;
            canvas.text(
                x + 4,
                cooldown_y + 2,
                palette::TEXT,
                &clipped_label(&cooldown.spell_label, label_cells),
            );
            canvas.progress(x + 2, cooldown_y + 11, slot_w - 4, fraction, READY_COLOR);
            canvas.text(
                x + 4,
                cooldown_y + 18,
                palette::TEXT_DIM,
                &slot_time_label(cooldown.remaining_ticks, label_cells),
            );
        }
    }

    let top_boundary = if !cooldowns.is_empty() {
        cooldown_y
    } else if state.active_cast.is_some() {
        cast_y
    } else if show_mana {
        mana_y
    } else {
        h - 38
    };
    let available_rows =
        ((top_boundary - MARGIN - 5 + STATUS_GAP) / (STATUS_HEIGHT + STATUS_GAP)).max(0) as usize;
    let status_w = STATUS_WIDTH.min(w - 2 * MARGIN);
    let status_x = w - MARGIN - status_w;
    for (i, status) in state
        .custom_statuses
        .iter()
        .filter(|status| status.remaining_ticks > 0)
        .take(MAX_STATUSES.min(available_rows))
        .enumerate()
    {
        let y = MARGIN + i as i32 * (STATUS_HEIGHT + STATUS_GAP);
        let tint = if status.is_harmful {
            CAST_INTERRUPTED
        } else {
            READY_COLOR
        };
        canvas.panel(status_x, y, status_w, STATUS_HEIGHT);
        let accent_rect = canvas.rect(status_x + 1, y + 1, 3, STATUS_HEIGHT - 2);
        canvas.list.fill_rect(accent_rect, tint);
        let mut name = status.label.clone();
        if status.amplifier > 0 {
            name.push_str(&format!(" {}", u16::from(status.amplifier) + 1));
        }
        if let Some(shield) = status
            .shield_amount
            .filter(|value| value.is_finite() && *value > 0.0)
        {
            name.push_str(&format!(" +{:.0}", shield.min(9999.0)));
        }
        canvas.text(
            status_x + 7,
            y + 4,
            palette::TEXT,
            &clipped_label(&name, ((status_w - 50) / 8) as usize),
        );
        canvas.text(
            status_x + status_w - 39,
            y + 4,
            palette::TEXT_DIM,
            &ticks_label(status.remaining_ticks),
        );
    }

    canvas.list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{rasterize_draw_list, text_width, UiCommand};

    fn populated() -> MagicHudState {
        MagicHudState {
            mana: 74.0,
            max_mana: 100.0,
            active_cast: Some(CastBarState {
                spell_label: "Arcane Bolt".into(),
                progress: 0.6,
                channeled: true,
                interrupted_flash_ticks: 0,
            }),
            cooldowns: vec![
                CooldownSlotState {
                    spell_label: "Fireball".into(),
                    remaining_ticks: 40,
                    total_ticks: 100,
                },
                CooldownSlotState {
                    spell_label: "Blink".into(),
                    remaining_ticks: 120,
                    total_ticks: 160,
                },
            ],
            custom_statuses: vec![
                StatusBadgeState {
                    label: "Ward".into(),
                    remaining_ticks: 400,
                    amplifier: 1,
                    is_harmful: false,
                    shield_amount: Some(12.0),
                },
                StatusBadgeState {
                    label: "Curse".into(),
                    remaining_ticks: 80,
                    amplifier: 0,
                    is_harmful: true,
                    shield_amount: None,
                },
            ],
        }
    }

    fn assert_within_viewport(list: &UiDrawList, width: u32, height: u32) {
        for cmd in &list.commands {
            match cmd {
                UiCommand::FillRect { rect, .. }
                | UiCommand::BorderRect { rect, .. }
                | UiCommand::ProgressBar { rect, .. } => {
                    assert!(rect.x >= 0 && rect.y >= 0, "{cmd:?}");
                    assert!(rect.right() <= width as i32, "{cmd:?}");
                    assert!(rect.bottom() <= height as i32, "{cmd:?}");
                }
                UiCommand::Text {
                    x, y, scale, text, ..
                } => {
                    assert!(*x >= 0 && *y >= 0, "{cmd:?}");
                    assert!(
                        *x + text_width(text, *scale) as i32 <= width as i32,
                        "{cmd:?}"
                    );
                    assert!(*y + 8 * *scale as i32 <= height as i32, "{cmd:?}");
                    assert!(!text.chars().any(char::is_control), "{cmd:?}");
                }
            }
            if let UiCommand::ProgressBar { fraction, .. } = cmd {
                assert!(fraction.is_finite() && (0.0..=1.0).contains(fraction));
            }
        }
    }

    #[test]
    fn empty_state_emits_nothing_even_with_invalid_mana() {
        let mut state = MagicHudState::default();
        assert!(state.is_empty());
        assert!(build_magic_hud_draw_list(&state, 1280, 720).is_empty());
        for value in [0.0, -2.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            state.max_mana = value;
            assert!(state.is_empty());
            assert!(build_magic_hud_draw_list(&state, 320, 240).is_empty());
        }
        state.cooldowns.push(CooldownSlotState::default());
        state.custom_statuses.push(StatusBadgeState::default());
        assert!(state.is_empty());
    }

    #[test]
    fn complete_overlay_rasterizes_deterministically() {
        let state = populated();
        let first = build_magic_hud_draw_list(&state, 320, 240);
        let second = build_magic_hud_draw_list(&state, 320, 240);
        assert_eq!(first, second);
        assert_within_viewport(&first, 320, 240);
        assert!(
            first
                .commands
                .iter()
                .filter(|cmd| matches!(cmd, UiCommand::ProgressBar { .. }))
                .count()
                >= 4
        );
        assert!(first
            .commands
            .iter()
            .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.contains("Mana"))));
        assert!(first
            .commands
            .iter()
            .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.contains("Channel"))));
        let pixels = rasterize_draw_list(320, 240, &first);
        assert_eq!(pixels.len(), 320 * 240 * 4);
        let (rgba, remaining) = pixels.as_chunks::<4>();
        assert!(remaining.is_empty());
        assert!(rgba.iter().filter(|pixel| pixel[3] != 0).count() > 500);
        assert!(rgba
            .iter()
            .any(|pixel| pixel[..3] == [MANA_COLOR.r, MANA_COLOR.g, MANA_COLOR.b]));
        assert!(rgba
            .iter()
            .any(|pixel| pixel[..3] == [CAST_COLOR.r, CAST_COLOR.g, CAST_COLOR.b]));
    }

    #[test]
    fn invalid_numbers_are_clamped_and_statuses_remain_visible_without_mana() {
        let mut state = populated();
        state.mana = f32::INFINITY;
        state.max_mana = f32::NAN;
        state.active_cast.as_mut().unwrap().progress = f32::NAN;
        state.custom_statuses[0].shield_amount = Some(f32::INFINITY);
        state.custom_statuses[1].shield_amount = Some(-10.0);
        assert!(!state.is_empty());
        let list = build_magic_hud_draw_list(&state, 640, 360);
        assert_within_viewport(&list, 640, 360);
        assert!(!list
            .commands
            .iter()
            .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.starts_with("Mana"))));
        assert!(list
            .commands
            .iter()
            .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.contains("Ward"))));
        let state = MagicHudState {
            mana: -999.0,
            max_mana: 100.0,
            ..Default::default()
        };
        let list = build_magic_hud_draw_list(&state, 320, 240);
        assert!(list
            .commands
            .iter()
            .any(|cmd| matches!(cmd, UiCommand::ProgressBar { fraction, .. } if *fraction == 0.0)));
    }

    #[test]
    fn many_long_labels_and_small_viewports_never_overflow() {
        let mut state = populated();
        state.cooldowns = (0..32)
            .map(|_| CooldownSlotState {
                spell_label: "Very long\nspell name".repeat(8),
                remaining_ticks: u32::MAX,
                total_ticks: 0,
            })
            .collect();
        state.custom_statuses = (0..24)
            .map(|_| StatusBadgeState {
                label: "Very long\nstatus name".repeat(8),
                remaining_ticks: u32::MAX,
                ..Default::default()
            })
            .collect();
        state.active_cast.as_mut().unwrap().spell_label = "Channel\nname".repeat(12);
        for (w, h) in [
            (0, 0),
            (1, 1),
            (79, 139),
            (80, 140),
            (160, 180),
            (319, 239),
            (320, 240),
            (321, 240),
            (640, 360),
            (1280, 720),
            (1920, 1080),
            (3840, 2160),
        ] {
            let list = build_magic_hud_draw_list(&state, w, h);
            assert_within_viewport(&list, w, h);
            if w >= 320 && h >= 240 {
                assert!(!list.is_empty());
                let pixels = rasterize_draw_list(w, h, &list);
                assert!(pixels.iter().any(|pixel| *pixel != 0));
            }
        }
    }

    #[test]
    fn interrupted_cast_and_cooldown_limits() {
        let mut state = populated();
        state.active_cast.as_mut().unwrap().interrupted_flash_ticks = 4;
        state.active_cast.as_mut().unwrap().progress = f32::INFINITY;
        state.cooldowns.extend((0..12).map(|_| CooldownSlotState {
            spell_label: "Spell".into(),
            remaining_ticks: 20,
            total_ticks: 200,
        }));
        let list = build_magic_hud_draw_list(&state, 320, 240);
        assert_within_viewport(&list, 320, 240);
        assert!(list.commands.iter().any(
            |cmd| matches!(cmd, UiCommand::ProgressBar { fill, .. } if *fill == CAST_INTERRUPTED)
        ));
        let cooldown_panels = list
            .commands
            .iter()
            .filter(
                |cmd| matches!(cmd, UiCommand::ProgressBar { fill, .. } if *fill == READY_COLOR),
            )
            .count();
        assert_eq!(cooldown_panels, MAX_COOLDOWNS);
        assert_eq!(slot_time_label(u32::MAX, 3), "99m");
        assert_eq!(slot_time_label(40, 3), "2s");
    }
}
