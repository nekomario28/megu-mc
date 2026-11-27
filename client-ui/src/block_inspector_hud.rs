// SPDX-License-Identifier: LGPL-3.0-or-later
//! Generic mod-agnostic target block telemetry overlay.
//!
//! Renders a compact HUD card beside the crosshair for any inspected block
//! exposing rotary/kinetic, processing, fluid, or custom key-value metrics.

use crate::draw::{glyph_is_covered, palette as pal, text_width, UiColor, UiDrawList, UiRect};
use crate::mod_i18n::UiModStrings;
use megu_core::ui_contract::compute_gui_scale;

/// One generic key-value or progress metric attached to an inspected block.
#[derive(Clone, Debug, PartialEq)]
pub struct InspectorMetric {
    pub label: String,
    pub value: String,
    /// Optional normalized bar fill in `0.0..=1.0`.
    pub progress: Option<f32>,
    pub color: Option<UiColor>,
}

/// Live telemetry for the block currently under the crosshair.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockInspectorTelemetry {
    pub block_id: String,
    pub display_name: Option<String>,
    /// Signed rotations per minute; the sign preserves rotation direction.
    pub rpm: f32,
    pub stress_impact: f32,
    pub network_stress_used: f32,
    pub network_stress_capacity: f32,
    pub overstressed: bool,
    pub processing_item: Option<String>,
    /// Processing completion in the inclusive range `0.0..=1.0`.
    pub processing_progress: Option<f32>,
    /// Fluid name, current millibuckets, and capacity in millibuckets.
    pub fluid_mb: Option<(String, u32, u32)>,
    /// Additional mod-agnostic metrics (energy, mana, temperature, pressure, etc.).
    pub extra_metrics: Vec<InspectorMetric>,
}

impl BlockInspectorTelemetry {
    fn has_rotary_metrics(&self) -> bool {
        self.rpm != 0.0
            || self.stress_impact > 0.0
            || self.network_stress_used > 0.0
            || self.network_stress_capacity > 0.0
            || self.overstressed
    }
}

struct InspectorRow {
    text: String,
    color: UiColor,
    bar: Option<(f32, UiColor)>,
}

/// Stateless builder for the generic block-inspector HUD card.
pub struct BlockInspectorHudLayout;

impl BlockInspectorHudLayout {
    /// Build a compact panel beside the screen center, keeping emitted
    /// geometry inside the viewport and clear of the crosshair and hotbar.
    pub fn build(
        telemetry: &BlockInspectorTelemetry,
        viewport_w: u32,
        viewport_h: u32,
        lang: &str,
    ) -> UiDrawList {
        let mut list = UiDrawList::new();
        let scale = compute_gui_scale(viewport_w, viewport_h, 0);
        if viewport_w < 96 || viewport_h < 96 {
            return list;
        }

        let width = (viewport_w / scale) as i32;
        let height = (viewport_h / scale) as i32;
        let ja = is_japanese_locale(lang);
        let title = telemetry
            .display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| block_title(&telemetry.block_id, lang));
        let title = covered_text(&title);

        let mut rows: Vec<InspectorRow> = Vec::with_capacity(8);
        rows.push(InspectorRow {
            text: title,
            color: pal::TEXT,
            bar: None,
        });

        if telemetry.has_rotary_metrics() {
            let rpm = finite_or_zero(telemetry.rpm);
            let direction = if rpm < 0.0 { "CCW" } else { "CW" };
            let speed_line = if ja {
                format!("スピード: {:.1} RPM {direction}", rpm.abs())
            } else {
                format!("Speed: {:.1} RPM {direction}", rpm.abs())
            };
            let used = finite_nonnegative(telemetry.network_stress_used);
            let capacity = finite_nonnegative(telemetry.network_stress_capacity);
            let ratio = if capacity > 0.0 { used / capacity } else { 0.0 };
            let percent = (ratio * 100.0).round() as u32;
            let stress_line = if ja {
                format!("ストレス: {:.0} / {:.0} SU ({percent}%)", used, capacity)
            } else {
                format!("Stress: {:.0} / {:.0} SU ({percent}%)", used, capacity)
            };
            let impact = finite_nonnegative(telemetry.stress_impact);
            let impact_line = if ja {
                format!("インパクト: {impact:.0} SU")
            } else {
                format!("Impact: {impact:.0} SU")
            };
            let stress_color = if telemetry.overstressed || ratio >= 1.0 {
                pal::DANGER
            } else if ratio >= 0.75 {
                pal::WARNING
            } else {
                pal::ACCENT
            };
            rows.push(InspectorRow {
                text: speed_line,
                color: pal::TEXT,
                bar: None,
            });
            rows.push(InspectorRow {
                text: impact_line,
                color: pal::TEXT_DIM,
                bar: None,
            });
            rows.push(InspectorRow {
                text: stress_line,
                color: stress_color,
                bar: Some((unit_fraction(ratio), stress_color)),
            });
            if telemetry.overstressed {
                let warn = if ja {
                    "カドフカ!".to_owned()
                } else {
                    "OVERSTRESSED!".to_owned()
                };
                rows.push(InspectorRow {
                    text: warn,
                    color: pal::DANGER,
                    bar: None,
                });
            }
        }

        if let Some((item, progress)) = telemetry
            .processing_item
            .as_deref()
            .zip(telemetry.processing_progress)
        {
            let item = covered_text(item);
            let progress = unit_fraction(progress);
            let label = if ja {
                format!("カコウ: {item} {:.0}%", progress * 100.0)
            } else {
                format!("Processing: {item} {:.0}%", progress * 100.0)
            };
            rows.push(InspectorRow {
                text: label,
                color: pal::TEXT_DIM,
                bar: Some((progress, UiColor::rgb(85, 170, 220))),
            });
        }

        if let Some((name, current, max)) = telemetry.fluid_mb.as_ref() {
            let name = covered_text(name);
            rows.push(InspectorRow {
                text: format!("{name}: {current} / {max} mB"),
                color: pal::TEXT_DIM,
                bar: None,
            });
        }

        for metric in &telemetry.extra_metrics {
            let label = covered_text(&metric.label);
            let value = covered_text(&metric.value);
            let line = if label.is_empty() {
                value
            } else {
                format!("{label}: {value}")
            };
            let color = metric.color.unwrap_or(pal::TEXT_DIM);
            let bar = metric
                .progress
                .map(|p| (unit_fraction(p), metric.color.unwrap_or(pal::ACCENT)));
            rows.push(InspectorRow {
                text: line,
                color,
                bar,
            });
        }

        let row_h = 10_i32;
        let padding_x = 6_i32;
        let padding_y = 5_i32;
        let max_panel_w = (width - 16).clamp(0, 288);
        if max_panel_w < 64 {
            return list;
        }
        let measured_w = rows
            .iter()
            .map(|row| text_width(&row.text, 1) as i32)
            .max()
            .unwrap_or(0)
            + padding_x * 2;
        let panel_w = measured_w.clamp(96, max_panel_w);
        let panel_h = rows.len() as i32 * row_h + padding_y * 2;
        if panel_h > height - 8 {
            return list;
        }

        // Keep a small gap from the center crosshair. If the right side is
        // too narrow, put the card to its left instead.
        let center_x = width / 2;
        let center_y = height / 2;
        let gap = 12_i32;
        let margin = 4_i32;
        let right_space = width - (center_x + gap) - margin;
        let (panel_x, panel_w) = if right_space >= 96 {
            (center_x + gap, panel_w.min(right_space))
        } else {
            (margin, panel_w.min((center_x - gap - margin).max(0)))
        };
        if panel_w < 64 {
            return list;
        }
        let mut panel_y = center_y - panel_h / 2;
        let hotbar_top = height - 28;
        if panel_y + panel_h > hotbar_top {
            panel_y = hotbar_top - panel_h;
        }
        panel_y = panel_y.clamp(margin, (height - margin - panel_h).max(margin));

        let s = scale as i32;
        let panel = UiRect::new(
            panel_x * s,
            panel_y * s,
            (panel_w * s) as u32,
            (panel_h * s) as u32,
        );
        list.fill_rect(panel, UiColor::rgba(14, 16, 22, 224));
        list.border_rect(panel, UiColor::rgba(105, 117, 139, 240), scale);

        let max_px = (panel_w - padding_x * 2).max(0) as usize;
        let bar_h = 6_i32;
        let bar_w = (panel_w - padding_x * 2).max(0);
        for (index, row) in rows.iter().enumerate() {
            let clipped = clip_text(&row.text, max_px);
            list.text_str(
                (panel_x + padding_x) * s,
                (panel_y + padding_y + index as i32 * row_h) * s,
                scale,
                row.color,
                &clipped,
            );
            if let Some((fraction, bar_color)) = row.bar {
                let y = panel_y + padding_y + (index as i32 + 1) * row_h - 1;
                list.progress_bar(
                    UiRect::new(
                        (panel_x + padding_x) * s,
                        y * s,
                        (bar_w * s) as u32,
                        (bar_h * s) as u32,
                    ),
                    fraction,
                    bar_color,
                    UiColor::rgba(38, 41, 48, 230),
                    UiColor::rgba(3, 4, 7, 255),
                );
            }
        }
        list
    }
}

fn is_japanese_locale(lang: &str) -> bool {
    let trimmed = lang.trim();
    trimmed.eq_ignore_ascii_case("ja")
        || trimmed.eq_ignore_ascii_case("ja_jp")
        || trimmed.eq_ignore_ascii_case("ja-jp")
}

fn block_title(block_id: &str, lang: &str) -> String {
    UiModStrings::default().resolve(block_id, lang)
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn finite_nonnegative(value: f32) -> f32 {
    finite_or_zero(value).max(0.0)
}

fn unit_fraction(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn covered_text(text: &str) -> String {
    text.chars()
        .map(|c| if glyph_is_covered(c) { c } else { '?' })
        .collect()
}

fn clip_text(text: &str, px: usize) -> String {
    crate::draw::first_px(text, px).to_string()
}
