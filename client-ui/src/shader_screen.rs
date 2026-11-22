// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Dependency-free shaderpack option screen for the client UI.
//!
//! Hosts supply plain option/profile/screen metadata parsed by the pack
//! loader. Applying returns a snapshot compatible with option overrides;
//! no client-port types or GPU resources are used here.

use std::collections::BTreeMap;

use crate::draw::{palette, UiColor, UiDrawList, UiRect};

/// A toggle, bounded slider, wrapping choice, or link to a named screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShaderOptionKind {
    Toggle { default: bool },
    Slider { values: Vec<String>, default: usize },
    Cycle { values: Vec<String>, default: usize },
    SubScreen { screen: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderOptionRow {
    pub key: String,
    pub kind: ShaderOptionKind,
}

/// Pack metadata supplied by a host's shaderpack loader.
///
/// Screen names omit the screen. prefix. A screen's ordered entries are option
/// keys or [screenName] links. The root screen may be stored under the empty
/// name; when absent, all option rows appear in declaration order.
/// Profile values use the same Some(value)/None conventions as option
/// overrides: None disables a define, Some("") enables a bare define.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShaderPackScreenDefinition {
    pub id: String,
    pub title: String,
    pub options: Vec<ShaderOptionRow>,
    pub screens: BTreeMap<String, Vec<String>>,
    pub profiles: BTreeMap<String, BTreeMap<String, Option<String>>>,
    pub falls_back_to_builtin: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShaderPackSelection {
    pub pack_id: String,
    pub profile: Option<String>,
    /// User-changed values applied after the selected profile.
    pub options: BTreeMap<String, Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShaderScreenAction {
    SelectPack(usize),
    SwitchProfile(Option<String>),
    CycleProfile(i32),
    ToggleOption(String),
    StepOption { key: String, direction: i32 },
    EnterScreen(String),
    ExitScreen,
    MoveFocus(i32),
    ActivateFocused,
    Apply,
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderScreenKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Apply,
    Reset,
    PreviousPack,
    NextPack,
    PreviousProfile,
    NextProfile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShaderScreenHitTarget {
    Pack(usize),
    PreviousProfile,
    NextProfile,
    Toggle(String),
    Step(String, i32),
    SubScreen(String),
    Back,
    Apply,
    Reset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderScreenHitRegion {
    pub rect: UiRect,
    pub target: ShaderScreenHitTarget,
}

/// Localized entries follow the ShaderPackLang key conventions:
/// option.KEY, value.KEY.VALUE, profile.NAME, screen.NAME.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShaderPackScreenState {
    pub packs: Vec<ShaderPackScreenDefinition>,
    pub selected_pack: Option<usize>,
    pub profile: Option<String>,
    pub options: BTreeMap<String, Option<String>>,
    pub screen_stack: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub focused_row: usize,
    applied: Option<ShaderPackSelection>,
}

impl ShaderPackScreenState {
    pub fn new(packs: Vec<ShaderPackScreenDefinition>) -> Self {
        let selected_pack = (!packs.is_empty()).then_some(0);
        Self {
            packs,
            selected_pack,
            ..Self::default()
        }
    }

    pub fn selected(&self) -> Option<&ShaderPackScreenDefinition> {
        self.selected_pack.and_then(|i| self.packs.get(i))
    }

    pub fn pending(&self) -> Option<ShaderPackSelection> {
        Some(ShaderPackSelection {
            pack_id: self.selected()?.id.clone(),
            profile: self.profile.clone(),
            options: self.options.clone(),
        })
    }

    pub fn applied(&self) -> Option<&ShaderPackSelection> {
        self.applied.as_ref()
    }

    pub fn is_dirty(&self) -> bool {
        self.pending() != self.applied
    }

    pub fn label<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        self.labels.get(key).map(String::as_str).unwrap_or(fallback)
    }

    pub fn active_screen(&self) -> Option<&str> {
        self.screen_stack.last().map(String::as_str)
    }

    pub fn current_rows(&self) -> Vec<ShaderOptionRow> {
        let Some(pack) = self.selected() else {
            return Vec::new();
        };
        let screen = self.active_screen().unwrap_or("");
        let Some(ordered) = pack.screens.get(screen) else {
            return if screen.is_empty() {
                pack.options.clone()
            } else {
                Vec::new()
            };
        };
        ordered
            .iter()
            .filter_map(|name| {
                if let Some(screen) = name.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                    if pack.screens.contains_key(screen) {
                        return Some(ShaderOptionRow {
                            key: screen.to_owned(),
                            kind: ShaderOptionKind::SubScreen {
                                screen: screen.to_owned(),
                            },
                        });
                    }
                    return None;
                }
                pack.options.iter().find(|row| row.key == *name).cloned()
            })
            .collect()
    }

    fn profile_value(&self, key: &str) -> Option<Option<String>> {
        self.selected()?
            .profiles
            .get(self.profile.as_deref()?)?
            .get(key)
            .cloned()
    }

    /// Effective value for a toggle, slider, or choice in pack syntax.
    pub fn option_value(&self, key: &str) -> Option<Option<String>> {
        let row = self.selected()?.options.iter().find(|row| row.key == key)?;
        if let Some(value) = self.options.get(key) {
            return Some(value.clone());
        }
        if let Some(value) = self.profile_value(key) {
            return Some(value);
        }
        match &row.kind {
            ShaderOptionKind::Toggle { default } => Some(default.then(String::new)),
            ShaderOptionKind::Slider { values, default }
            | ShaderOptionKind::Cycle { values, default } => Some(values.get(*default).cloned()),
            ShaderOptionKind::SubScreen { .. } => None,
        }
    }

    fn option_text(&self, row: &ShaderOptionRow) -> String {
        match &row.kind {
            ShaderOptionKind::Toggle { .. } => {
                if self.option_value(&row.key).flatten().is_some() {
                    self.label("shader.on", "On").to_owned()
                } else {
                    self.label("shader.off", "Off").to_owned()
                }
            }
            ShaderOptionKind::Slider { .. } | ShaderOptionKind::Cycle { .. } => {
                let raw = self.option_value(&row.key).flatten().unwrap_or_default();
                self.label(&format!("value.{}.{}", row.key, raw), &raw)
                    .to_owned()
            }
            ShaderOptionKind::SubScreen { .. } => ">".to_owned(),
        }
    }

    fn move_profile(&mut self, direction: i32) {
        let Some(pack) = self.selected() else {
            return;
        };
        let profiles: Vec<String> = pack.profiles.keys().cloned().collect();
        if profiles.is_empty() {
            return;
        }
        let position = self
            .profile
            .as_ref()
            .and_then(|name| profiles.iter().position(|p| p == name))
            .map(|p| p + 1)
            .unwrap_or(0);
        let count = profiles.len() + 1;
        let next = (position as i64 + i64::from(direction)).rem_euclid(count as i64) as usize;
        self.profile = next.checked_sub(1).and_then(|i| profiles.get(i).cloned());
        self.options.clear();
    }

    fn step_option(&mut self, key: &str, direction: i32) {
        let Some(row) = self
            .selected()
            .and_then(|p| p.options.iter().find(|row| row.key == key))
        else {
            return;
        };
        let (values, wrapping) = match &row.kind {
            ShaderOptionKind::Slider { values, .. } => (values.clone(), false),
            ShaderOptionKind::Cycle { values, .. } => (values.clone(), true),
            _ => return,
        };
        if values.is_empty() {
            return;
        }
        let current = self.option_value(key).flatten().unwrap_or_default();
        let position = values.iter().position(|v| *v == current).unwrap_or(0);
        let next = if wrapping {
            (position as i64 + i64::from(direction)).rem_euclid(values.len() as i64) as usize
        } else {
            (position as i64 + i64::from(direction)).clamp(0, (values.len() - 1) as i64) as usize
        };
        self.options
            .insert(key.to_owned(), Some(values[next].clone()));
    }

    /// Execute an option action. Apply returns a committed snapshot for the
    /// host to pass to its shaderpack planner; other actions return None.
    pub fn apply_action(&mut self, action: ShaderScreenAction) -> Option<ShaderPackSelection> {
        match action {
            ShaderScreenAction::SelectPack(index) => {
                if index < self.packs.len() {
                    self.selected_pack = Some(index);
                    self.profile = None;
                    self.options.clear();
                    self.screen_stack.clear();
                    self.focused_row = 0;
                }
            }
            ShaderScreenAction::SwitchProfile(profile) => {
                if self.selected().is_some_and(|pack| {
                    profile
                        .as_ref()
                        .is_none_or(|name| pack.profiles.contains_key(name))
                }) {
                    self.profile = profile;
                    self.options.clear();
                }
            }
            ShaderScreenAction::CycleProfile(direction) => self.move_profile(direction),
            ShaderScreenAction::ToggleOption(key) => {
                if self.selected().is_some_and(|pack| {
                    pack.options.iter().any(|row| {
                        row.key == key && matches!(row.kind, ShaderOptionKind::Toggle { .. })
                    })
                }) {
                    let new_value = if self.option_value(&key).flatten().is_some() {
                        None
                    } else {
                        Some(String::new())
                    };
                    self.options.insert(key, new_value);
                }
            }
            ShaderScreenAction::StepOption { key, direction } => {
                self.step_option(&key, direction);
            }
            ShaderScreenAction::EnterScreen(screen) => {
                if self
                    .selected()
                    .is_some_and(|p| p.screens.contains_key(&screen))
                {
                    self.screen_stack.push(screen);
                    self.focused_row = 0;
                }
            }
            ShaderScreenAction::ExitScreen => {
                self.screen_stack.pop();
                self.focused_row = 0;
            }
            ShaderScreenAction::MoveFocus(offset) => {
                let len = self.current_rows().len();
                if len != 0 {
                    self.focused_row = (self.focused_row as i64 + i64::from(offset))
                        .rem_euclid(len as i64) as usize;
                }
            }
            ShaderScreenAction::ActivateFocused => {
                if let Some(row) = self.current_rows().get(self.focused_row) {
                    match &row.kind {
                        ShaderOptionKind::Toggle { .. } => {
                            return self
                                .apply_action(ShaderScreenAction::ToggleOption(row.key.clone()));
                        }
                        ShaderOptionKind::Slider { .. } | ShaderOptionKind::Cycle { .. } => {
                            return self.apply_action(ShaderScreenAction::StepOption {
                                key: row.key.clone(),
                                direction: 1,
                            });
                        }
                        ShaderOptionKind::SubScreen { screen } => {
                            return self
                                .apply_action(ShaderScreenAction::EnterScreen(screen.clone()));
                        }
                    }
                }
            }
            ShaderScreenAction::Apply => {
                self.applied = self.pending();
                return self.applied.clone();
            }
            ShaderScreenAction::Reset => {
                if let Some(applied) = self.applied.clone() {
                    self.selected_pack = self.packs.iter().position(|p| p.id == applied.pack_id);
                    self.profile = applied.profile;
                    self.options = applied.options;
                } else {
                    self.selected_pack = (!self.packs.is_empty()).then_some(0);
                    self.profile = None;
                    self.options.clear();
                }
                self.screen_stack.clear();
                self.focused_row = 0;
            }
        }
        None
    }

    pub fn handle_key(&mut self, key: ShaderScreenKey) -> Option<ShaderPackSelection> {
        let action = match key {
            ShaderScreenKey::Up => ShaderScreenAction::MoveFocus(-1),
            ShaderScreenKey::Down => ShaderScreenAction::MoveFocus(1),
            ShaderScreenKey::Left | ShaderScreenKey::Right => {
                let row = self.current_rows().get(self.focused_row).cloned()?;
                let direction = if key == ShaderScreenKey::Left { -1 } else { 1 };
                ShaderScreenAction::StepOption {
                    key: row.key,
                    direction,
                }
            }
            ShaderScreenKey::Enter => ShaderScreenAction::ActivateFocused,
            ShaderScreenKey::Escape => ShaderScreenAction::ExitScreen,
            ShaderScreenKey::Apply => ShaderScreenAction::Apply,
            ShaderScreenKey::Reset => ShaderScreenAction::Reset,
            ShaderScreenKey::PreviousPack | ShaderScreenKey::NextPack => {
                let count = self.packs.len();
                if count == 0 {
                    return None;
                }
                let selected = self.selected_pack.unwrap_or(0);
                let next = if key == ShaderScreenKey::PreviousPack {
                    (selected + count - 1) % count
                } else {
                    (selected + 1) % count
                };
                ShaderScreenAction::SelectPack(next)
            }
            ShaderScreenKey::PreviousProfile => ShaderScreenAction::CycleProfile(-1),
            ShaderScreenKey::NextProfile => ShaderScreenAction::CycleProfile(1),
        };
        self.apply_action(action)
    }

    pub fn pointer_action(
        &self,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
    ) -> Option<ShaderScreenAction> {
        let (_, hit_regions) = self.render(width, height);
        let target = &hit_regions
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y))?
            .target;
        Some(match target {
            ShaderScreenHitTarget::Pack(index) => ShaderScreenAction::SelectPack(*index),
            ShaderScreenHitTarget::PreviousProfile => ShaderScreenAction::CycleProfile(-1),
            ShaderScreenHitTarget::NextProfile => ShaderScreenAction::CycleProfile(1),
            ShaderScreenHitTarget::Toggle(key) => ShaderScreenAction::ToggleOption(key.clone()),
            ShaderScreenHitTarget::Step(key, direction) => ShaderScreenAction::StepOption {
                key: key.clone(),
                direction: *direction,
            },
            ShaderScreenHitTarget::SubScreen(name) => ShaderScreenAction::EnterScreen(name.clone()),
            ShaderScreenHitTarget::Back => ShaderScreenAction::ExitScreen,
            ShaderScreenHitTarget::Apply => ShaderScreenAction::Apply,
            ShaderScreenHitTarget::Reset => ShaderScreenAction::Reset,
        })
    }

    pub fn handle_pointer(
        &mut self,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
    ) -> Option<ShaderPackSelection> {
        let action = self.pointer_action(width, height, x, y)?;
        self.apply_action(action)
    }

    /// Render screen content as the existing draw-list primitives, together
    /// with hit regions. Rows scroll automatically to the keyboard focus.
    pub fn render(&self, width: u32, height: u32) -> (UiDrawList, Vec<ShaderScreenHitRegion>) {
        let mut draw = UiDrawList::new();
        let mut hits = Vec::new();
        if width < 80 || height < 120 {
            return (draw, hits);
        }
        let w = width.min(i32::MAX as u32) as i32;
        let h = height.min(i32::MAX as u32) as i32;
        let left = 20;
        let right = w - 20;
        let panel_w = (right - left).max(1) as u32;
        draw.fill_rect(UiRect::new(0, 0, width, height), palette::BACKGROUND_TOP);
        draw.fill_rect(
            UiRect::new(left, 12, panel_w, (h - 24) as u32),
            palette::PANEL,
        );
        draw.border_rect(
            UiRect::new(left, 12, panel_w, (h - 24) as u32),
            palette::PANEL_BORDER,
            1,
        );
        draw.text(
            left + 12,
            24,
            2,
            palette::TEXT,
            fit_text(
                self.label("shader.title", "Shader Packs"),
                (panel_w.saturating_sub(24) / 16) as usize,
            ),
        );

        let Some(pack) = self.selected() else {
            draw.text(
                left + 12,
                65,
                1,
                palette::TEXT_DIM,
                fit_text(
                    self.label("shader.empty", "No shader packs available"),
                    (panel_w.saturating_sub(24) / 8) as usize,
                ),
            );
            return (draw, hits);
        };

        let strip = UiRect::new(left + 12, 55, panel_w.saturating_sub(24), 32);
        draw.fill_rect(strip, palette::BUTTON);
        // Cycle arrows need 68px inside the strip; below that they would
        // overlap each other or fall outside the panel.
        let show_arrows = strip.w >= 80;
        if show_arrows {
            let prev = UiRect::new(strip.x, strip.y, 34, strip.h);
            let next = UiRect::new(strip.right() - 34, strip.y, 34, strip.h);
            draw.text(prev.x + 10, prev.y + 11, 1, palette::TEXT, "<");
            draw.text(next.x + 10, next.y + 11, 1, palette::TEXT, ">");
            if self.packs.len() > 1 {
                let selected = self.selected_pack.unwrap_or(0);
                hits.push(ShaderScreenHitRegion {
                    rect: prev,
                    target: ShaderScreenHitTarget::Pack(
                        (selected + self.packs.len() - 1) % self.packs.len(),
                    ),
                });
                hits.push(ShaderScreenHitRegion {
                    rect: next,
                    target: ShaderScreenHitTarget::Pack((selected + 1) % self.packs.len()),
                });
            }
        }
        let title_x = strip.x + if show_arrows { 46 } else { 10 };
        draw.text(
            title_x,
            strip.y + 11,
            1,
            palette::TEXT,
            fit_text(
                &pack.title,
                ((strip.right() - title_x - 12).max(0) as u32 / 8) as usize,
            ),
        );

        // Optional blocks must end above the footer; when the window is
        // short they collapse top-down instead of painting over it.
        let footer_y = h - 50;
        let mut y = 97;
        if !pack.profiles.is_empty() && y + 30 <= footer_y - 6 {
            let title = self
                .profile
                .as_ref()
                .map(|name| self.label(&format!("profile.{name}"), name))
                .unwrap_or_else(|| self.label("shader.profile.default", "Default"));
            let profile_rect = UiRect::new(left + 12, y, panel_w.saturating_sub(24), 30);
            draw.fill_rect(profile_rect, palette::BUTTON);
            let profile_arrows = profile_rect.w >= 92;
            draw.text(
                profile_rect.x + 10,
                y + 9,
                1,
                palette::TEXT,
                fit_text(
                    &format!("{}: {}", self.label("shader.profile", "Profile"), title),
                    (profile_rect.w.saturating_sub(if profile_arrows { 78 } else { 20 }) / 8)
                        as usize,
                ),
            );
            if profile_arrows {
                let prev = UiRect::new(profile_rect.right() - 68, y, 34, 30);
                let next = UiRect::new(profile_rect.right() - 34, y, 34, 30);
                draw.text(prev.x + 11, y + 9, 1, palette::TEXT, "<");
                draw.text(next.x + 11, y + 9, 1, palette::TEXT, ">");
                hits.push(ShaderScreenHitRegion {
                    rect: prev,
                    target: ShaderScreenHitTarget::PreviousProfile,
                });
                hits.push(ShaderScreenHitRegion {
                    rect: next,
                    target: ShaderScreenHitTarget::NextProfile,
                });
            }
            y += 38;
        }
        if pack.falls_back_to_builtin && y + 42 <= footer_y - 6 {
            let banner = UiRect::new(left + 12, y, panel_w.saturating_sub(24), 42);
            draw.fill_rect(banner, UiColor::rgb(85, 60, 23));
            draw.text(
                banner.x + 8,
                banner.y + 8,
                1,
                palette::TEXT,
                fit_text(
                    self.label(
                        "shader.fallback",
                        "This pack cannot run on your graphics device.",
                    ),
                    (banner.w.saturating_sub(16) / 8) as usize,
                ),
            );
            draw.text(
                banner.x + 8,
                banner.y + 23,
                1,
                palette::TEXT,
                fit_text(
                    self.label("shader.fallback.detail", "Using built-in graphics instead."),
                    (banner.w.saturating_sub(16) / 8) as usize,
                ),
            );
            y += 48;
        }
        if self.active_screen().is_some() && y + 26 <= footer_y - 6 && right - 68 >= left + 12 {
            let screen = self.active_screen().unwrap_or_default();
            draw.text(
                left + 12,
                y + 6,
                1,
                palette::TEXT,
                fit_text(
                    self.label(&format!("screen.{screen}"), screen),
                    (panel_w.saturating_sub(100) / 8) as usize,
                ),
            );
            let rect = UiRect::new(right - 68, y, 56, 26);
            draw.fill_rect(rect, palette::BUTTON);
            draw.text(
                rect.x + 8,
                y + 9,
                1,
                palette::TEXT,
                fit_text(self.label("shader.back", "Back"), 5),
            );
            hits.push(ShaderScreenHitRegion {
                rect,
                target: ShaderScreenHitTarget::Back,
            });
            y += 34;
        }

        let rows = self.current_rows();
        let bottom = h - 64;
        let visible_count = ((bottom - y).max(0) / 38) as usize;
        let offset = self
            .focused_row
            .saturating_sub(visible_count.saturating_sub(1));
        for (index, row) in rows.iter().enumerate().skip(offset).take(visible_count) {
            let rect = UiRect::new(left + 12, y, panel_w.saturating_sub(24), 32);
            draw.fill_rect(
                rect,
                if index == self.focused_row {
                    palette::BUTTON_HOVER
                } else {
                    palette::BUTTON
                },
            );
            let label_key = if matches!(row.kind, ShaderOptionKind::SubScreen { .. }) {
                format!("screen.{}", row.key)
            } else {
                format!("option.{}", row.key)
            };
            let label = self.label(&label_key, &row.key);
            // The right-side value+stepper chrome needs ~140px; on
            // narrow panels it collapses so nothing clips outside.
            let chrome = rect.w >= 160;
            draw.text(
                rect.x + 10,
                y + 12,
                1,
                palette::TEXT,
                fit_text(
                    label,
                    (rect.w.saturating_sub(if chrome { 138 } else { 20 }) / 8) as usize,
                ),
            );
            if chrome {
                let value = self.option_text(row);
                draw.text(
                    rect.right() - 112,
                    y + 12,
                    1,
                    palette::TEXT,
                    fit_text(&value, 12),
                );
            }
            let target = match &row.kind {
                ShaderOptionKind::Toggle { .. } => {
                    hits.push(ShaderScreenHitRegion {
                        rect,
                        target: ShaderScreenHitTarget::Toggle(row.key.clone()),
                    });
                    None
                }
                ShaderOptionKind::Slider { .. } | ShaderOptionKind::Cycle { .. } => {
                    if chrome {
                        let step_left = UiRect::new(rect.right() - 128, y, 20, 32);
                        let step_right = UiRect::new(rect.right() - 22, y, 22, 32);
                        draw.text(step_left.x + 6, y + 12, 1, palette::TEXT, "<");
                        draw.text(step_right.x + 6, y + 12, 1, palette::TEXT, ">");
                        hits.push(ShaderScreenHitRegion {
                            rect: step_left,
                            target: ShaderScreenHitTarget::Step(row.key.clone(), -1),
                        });
                        hits.push(ShaderScreenHitRegion {
                            rect: step_right,
                            target: ShaderScreenHitTarget::Step(row.key.clone(), 1),
                        });
                    } else {
                        // No room for steppers: the row still steps via
                        // pointer through the whole-row target.
                        hits.push(ShaderScreenHitRegion {
                            rect,
                            target: ShaderScreenHitTarget::Step(row.key.clone(), 1),
                        });
                    }
                    None
                }
                ShaderOptionKind::SubScreen { screen } => {
                    Some(ShaderScreenHitTarget::SubScreen(screen.clone()))
                }
            };
            if let Some(target) = target {
                hits.push(ShaderScreenHitRegion { rect, target });
            }
            y += 38;
        }

        let half = panel_w.saturating_sub(36) / 2;
        let apply = UiRect::new(left + 12, footer_y, half, 32);
        let reset = UiRect::new(apply.right() + 12, footer_y, half, 32);
        draw.fill_rect(apply, palette::ACCENT);
        draw.fill_rect(reset, palette::BUTTON);
        draw.text(
            apply.x + 12,
            footer_y + 11,
            1,
            palette::TEXT,
            fit_text(
                self.label("shader.apply", "Apply"),
                (half.saturating_sub(16) / 8) as usize,
            ),
        );
        draw.text(
            reset.x + 12,
            footer_y + 11,
            1,
            palette::TEXT,
            fit_text(
                self.label("shader.reset", "Reset"),
                (half.saturating_sub(16) / 8) as usize,
            ),
        );
        hits.push(ShaderScreenHitRegion {
            rect: apply,
            target: ShaderScreenHitTarget::Apply,
        });
        hits.push(ShaderScreenHitRegion {
            rect: reset,
            target: ShaderScreenHitTarget::Reset,
        });
        (draw, hits)
    }
}

// Built-in bitmap font uses fixed 8px cells. Elide overlong labels rather
// than painting text over an adjacent control or outside a panel.
fn fit_text(label: &str, cells: usize) -> String {
    if crate::draw::text_cells(label) <= cells {
        return label.to_owned();
    }
    if cells <= 3 {
        return ".".repeat(cells);
    }
    let mut clipped = crate::draw::first_cells(label, cells - 3).to_owned();
    clipped.push_str("...");
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::UiCommand;

    fn fixture() -> ShaderPackScreenState {
        let mut pack = ShaderPackScreenDefinition {
            id: "example".to_owned(),
            title: "Example Pack".to_owned(),
            ..ShaderPackScreenDefinition::default()
        };
        pack.options = vec![
            ShaderOptionRow {
                key: "SHADOWS".into(),
                kind: ShaderOptionKind::Toggle { default: true },
            },
            ShaderOptionRow {
                key: "QUALITY".into(),
                kind: ShaderOptionKind::Slider {
                    values: vec!["LOW".into(), "MED".into(), "HIGH".into()],
                    default: 1,
                },
            },
            ShaderOptionRow {
                key: "CLOUDS".into(),
                kind: ShaderOptionKind::Cycle {
                    values: vec!["OFF".into(), "FAST".into(), "FANCY".into()],
                    default: 0,
                },
            },
        ];
        pack.screens
            .insert("".into(), vec!["SHADOWS".into(), "[detail]".into()]);
        pack.screens
            .insert("detail".into(), vec!["QUALITY".into(), "CLOUDS".into()]);
        pack.profiles.insert(
            "Fast".into(),
            [
                ("SHADOWS".into(), None),
                ("QUALITY".into(), Some("LOW".into())),
            ]
            .into(),
        );
        pack.profiles.insert(
            "Fancy".into(),
            [("CLOUDS".into(), Some("FANCY".into()))].into(),
        );
        ShaderPackScreenState::new(vec![pack])
    }

    #[test]
    fn empty_screen_and_unsupported_dimensions_are_safe() {
        let state = ShaderPackScreenState::new(Vec::new());
        assert!(state.pending().is_none());
        assert!(state.render(640, 360).1.is_empty());
        assert!(state.render(0, 0).0.commands.is_empty());
        assert!(state.pointer_action(640, 360, 5, 5).is_none());
    }

    #[test]
    fn toggle_slider_cycle_and_profiles() {
        let mut state = fixture();
        assert_eq!(state.option_value("SHADOWS"), Some(Some(String::new())));
        state.apply_action(ShaderScreenAction::ToggleOption("SHADOWS".into()));
        assert_eq!(state.option_value("SHADOWS"), Some(None));
        state.apply_action(ShaderScreenAction::StepOption {
            key: "QUALITY".into(),
            direction: 10,
        });
        assert_eq!(state.option_value("QUALITY"), Some(Some("HIGH".into())));
        state.apply_action(ShaderScreenAction::StepOption {
            key: "CLOUDS".into(),
            direction: -1,
        });
        assert_eq!(state.option_value("CLOUDS"), Some(Some("FANCY".into())));
        state.apply_action(ShaderScreenAction::SwitchProfile(Some("Fancy".into())));
        assert_eq!(state.option_value("SHADOWS"), Some(Some(String::new())));
        assert_eq!(state.option_value("QUALITY"), Some(Some("MED".into())));
        assert_eq!(state.option_value("CLOUDS"), Some(Some("FANCY".into())));
        state.apply_action(ShaderScreenAction::SwitchProfile(Some("Fast".into())));
        assert_eq!(state.option_value("SHADOWS"), Some(None));
        assert_eq!(state.option_value("QUALITY"), Some(Some("LOW".into())));
        assert!(state.options.is_empty());
        state.apply_action(ShaderScreenAction::CycleProfile(1));
        assert_eq!(state.profile, None);
        state.apply_action(ShaderScreenAction::CycleProfile(-1));
        assert_eq!(state.profile.as_deref(), Some("Fast"));
        state.apply_action(ShaderScreenAction::SwitchProfile(Some("Missing".into())));
        assert_eq!(state.profile.as_deref(), Some("Fast"));
    }

    #[test]
    fn screen_navigation_and_keyboard() {
        let mut state = fixture();
        assert_eq!(state.current_rows().len(), 2);
        state.handle_key(ShaderScreenKey::Down);
        state.handle_key(ShaderScreenKey::Enter);
        assert_eq!(state.active_screen(), Some("detail"));
        assert_eq!(state.current_rows().len(), 2);
        state.handle_key(ShaderScreenKey::Right);
        assert_eq!(state.option_value("QUALITY"), Some(Some("HIGH".into())));
        state.handle_key(ShaderScreenKey::Down);
        state.handle_key(ShaderScreenKey::Left);
        assert_eq!(state.option_value("CLOUDS"), Some(Some("FANCY".into())));
        state.handle_key(ShaderScreenKey::Escape);
        assert_eq!(state.active_screen(), None);
    }

    #[test]
    fn apply_and_reset_preserve_snapshot_across_pack_changes() {
        let mut state = fixture();
        state.packs.push(ShaderPackScreenDefinition {
            id: "second".into(),
            title: "Second".into(),
            ..Default::default()
        });
        state.apply_action(ShaderScreenAction::SwitchProfile(Some("Fancy".into())));
        let snapshot = state.handle_key(ShaderScreenKey::Apply).unwrap();
        assert_eq!(snapshot.profile.as_deref(), Some("Fancy"));
        assert!(!state.is_dirty());
        state.handle_key(ShaderScreenKey::NextPack);
        assert!(state.is_dirty());
        state.handle_key(ShaderScreenKey::Reset);
        assert_eq!(state.selected().unwrap().id, "example");
        assert_eq!(state.profile.as_deref(), Some("Fancy"));
        assert!(!state.is_dirty());
    }

    #[test]
    fn localized_labels_and_fallback_banner() {
        let mut state = fixture();
        state.packs[0].falls_back_to_builtin = true;
        state
            .labels
            .insert("shader.fallback".into(), "Graphics unavailable".into());
        state
            .labels
            .insert("option.SHADOWS".into(), "Nice Shadows".into());
        state
            .labels
            .insert("screen.detail".into(), "Detail Settings".into());
        assert_eq!(state.label("option.SHADOWS", "SHADOWS"), "Nice Shadows");
        assert_eq!(state.label("missing", "raw key"), "raw key");
        let (draw, _) = state.render(800, 600);
        let text: Vec<String> = draw
            .commands
            .iter()
            .filter_map(|c| match c {
                UiCommand::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(text.iter().any(|s| s.contains("Graphics unavailable")));
        assert!(text.iter().any(|s| s.contains("Using built-in graphics")));
        assert!(text.iter().any(|s| s.contains("Nice Shadows")));
    }

    #[test]
    fn pointer_hits_and_apply_snapshot() {
        let mut state = fixture();
        let (_draw, hits) = state.render(800, 600);
        let toggle = hits
            .iter()
            .find(|h| matches!(h.target, ShaderScreenHitTarget::Toggle(_)))
            .unwrap();
        let (x, y) = (toggle.rect.x + 4, toggle.rect.y + 4);
        state.handle_pointer(800, 600, x, y);
        assert_eq!(state.option_value("SHADOWS"), Some(None));
        let (_, hits) = state.render(800, 600);
        let apply = hits
            .iter()
            .find(|h| h.target == ShaderScreenHitTarget::Apply)
            .unwrap();
        let committed = state
            .handle_pointer(800, 600, apply.rect.x + 4, apply.rect.y + 4)
            .unwrap();
        assert_eq!(committed.options.get("SHADOWS"), Some(&None));
    }

    #[test]
    fn malformed_metadata_does_not_create_controls() {
        let mut state = fixture();
        state.apply_action(ShaderScreenAction::EnterScreen("does-not-exist".into()));
        state.apply_action(ShaderScreenAction::ToggleOption("UNKNOWN".into()));
        state.apply_action(ShaderScreenAction::StepOption {
            key: "CLOUDS".into(),
            direction: 1,
        });
        assert!(state.active_screen().is_none());
        assert!(!state.options.contains_key("UNKNOWN"));
        assert_eq!(state.option_value("CLOUDS"), Some(Some("FAST".into())));
    }
}
