// SPDX-License-Identifier: LGPL-3.0-or-later
//! `gamepad`: a Controllable-class native gamepad engine.
//!
//! Zero-external-dependency, `#![forbid(unsafe_code)]` controller support for
//! Xbox, DualSense/PlayStation, Switch Pro and Steam Deck pads — clean-room,
//! pure `std` logic that a host feeds with raw device state each frame:
//!
//! * [`GamepadBrand`] / [`GamepadButton`] / [`GamepadAxis`] — device profiles,
//!   per-brand prompt labels (`A/B/X/Y`, `Cross/Circle/Square/Triangle`,
//!   Nintendo `B/A/Y/X`) and bilingual EN/JA action names
//!   ([`GamepadAction::name`]).
//! * [`StickConfig`] / [`process_radial_stick`] — scaled radial deadzone,
//!   outer deadzone, exponential response curve and Y-invert with fail-closed
//!   NaN/Inf handling. Right stick becomes camera look
//!   ([`GamepadEvent::Look`]), left stick movement ([`GamepadEvent::Move`]).
//! * [`GamepadMapper`] — the standard console bindings (jump, inventory,
//!   hotbar bumpers, analog-trigger use/attack with hysteresis, L3 sprint,
//!   R3 sneak, D-pad camera/zoom/recipe/debug) plus a full UI/container mode:
//!   magnetic virtual cursor that slows over [`HitRegion`] centers, D-pad
//!   spatial focus jumps, confirm/cancel/quick-move/split-stack and bumper
//!   tab switching — with the Nintendo confirm-swap option.
//! * [`RadialWheelState`] — an 8-slice action wheel driven by the right
//!   stick while Select is held, plus [`RadialWheelState::layout`]'s
//!   pure-geometry render model for `client-gpu`.
//! * [`RumbleCommand`] — a bounded haptic queue honoring `rumble_enabled`
//!   and `rumble_strength`.
//! * [`build_context_hints`] — Controllable-style per-screen button hint
//!   bars ([`HintContext`] / [`ConsoleHintItem`]) with clean-room
//!   geometric [`ButtonBadgeSpec`]s.
//! * [`InputModeTracker`] — seamless keyboard/mouse ⇄ gamepad prompt
//!   switching; [`AnalogMovementMode::EightWayVanillaCompat`] — 8-way
//!   movement snap for strict third-party anti-cheat servers.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use crate::state::{tr, HitRegion, UiLang};

// ---------------------------------------------------------------------------
// Hardware identity
// ---------------------------------------------------------------------------

/// Controller family, for prompt glyphs and the confirm-swap option.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GamepadBrand {
    /// Xbox 360/One/Series layout (`A` south, `B` east).
    #[default]
    Xbox,
    /// DualShock/DualSense (`Cross` south, `Circle` east).
    PlayStation,
    /// Switch Pro / Joy-Con grip (`B` south, `A` east — swapped vs Xbox).
    Switch,
    /// Steam Deck integrated controller (Xbox lettering, Steam buttons).
    SteamDeck,
}

impl GamepadBrand {
    /// Best-effort brand detection from USB vendor/product ids and the
    /// OS-reported device name. Unknown devices default to the Xbox layout —
    /// the de-facto standard mapping.
    pub fn detect(vendor_id: u16, _product_id: u16, name: &str) -> Self {
        match vendor_id {
            0x045E => return Self::Xbox,      // Microsoft
            0x054C => return Self::PlayStation, // Sony
            0x057E => return Self::Switch,    // Nintendo
            0x28DE => return Self::SteamDeck, // Valve
            _ => {}
        }
        let n = name.to_ascii_lowercase();
        if n.contains("dualsense") || n.contains("dualshock") || n.contains("playstation") {
            Self::PlayStation
        } else if n.contains("switch") || n.contains("nintendo") || n.contains("pro controller") {
            Self::Switch
        } else if n.contains("steam") || n.contains("valve") {
            Self::SteamDeck
        } else {
            Self::Xbox
        }
    }

    /// Human-readable marketing name for settings screens.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Xbox => "Xbox",
            Self::PlayStation => "PlayStation",
            Self::Switch => "Nintendo Switch",
            Self::SteamDeck => "Steam Deck",
        }
    }
}

/// Every digital input the engine understands, in canonical SDL-style
/// positions so prompts stay correct across brands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadButton {
    /// Bottom face button (Xbox `A`, PS `Cross`, Switch `B`).
    South,
    /// Right face button (Xbox `B`, PS `Circle`, Switch `A`).
    East,
    /// Left face button (Xbox `X`, PS `Square`, Switch `Y`).
    West,
    /// Top face button (Xbox `Y`, PS `Triangle`, Switch `X`).
    North,
    /// Left bumper.
    LeftShoulder,
    /// Right bumper.
    RightShoulder,
    /// Digital left trigger (for pads that report it as a button).
    LeftTrigger,
    /// Digital right trigger.
    RightTrigger,
    /// Left stick click (`L3`).
    LeftStick,
    /// Right stick click (`R3`).
    RightStick,
    /// Menu / Options / `+`.
    Start,
    /// View / Create / `−`.
    Select,
    /// Xbox / PS / Home / Steam guide button.
    Guide,
    /// D-pad up.
    DpadUp,
    /// D-pad down.
    DpadDown,
    /// D-pad left.
    DpadLeft,
    /// D-pad right.
    DpadRight,
}

const _: () = assert!(
    GamepadButton::DpadRight.index() + 1 == GAMEPAD_BUTTON_COUNT,
    "GAMEPAD_BUTTON_COUNT must cover every GamepadButton variant"
);

/// Number of [`GamepadButton`] variants — the size of [`GamepadState::buttons`].
pub const GAMEPAD_BUTTON_COUNT: usize = 17;

/// Ordered button table; index order matches [`GamepadButton::index`].
pub const GAMEPAD_BUTTONS: [GamepadButton; GAMEPAD_BUTTON_COUNT] = [
    GamepadButton::South,
    GamepadButton::East,
    GamepadButton::West,
    GamepadButton::North,
    GamepadButton::LeftShoulder,
    GamepadButton::RightShoulder,
    GamepadButton::LeftTrigger,
    GamepadButton::RightTrigger,
    GamepadButton::LeftStick,
    GamepadButton::RightStick,
    GamepadButton::Start,
    GamepadButton::Select,
    GamepadButton::Guide,
    GamepadButton::DpadUp,
    GamepadButton::DpadDown,
    GamepadButton::DpadLeft,
    GamepadButton::DpadRight,
];

impl GamepadButton {
    /// Stable index into [`GamepadState::buttons`].
    pub const fn index(self) -> usize {
        match self {
            Self::South => 0,
            Self::East => 1,
            Self::West => 2,
            Self::North => 3,
            Self::LeftShoulder => 4,
            Self::RightShoulder => 5,
            Self::LeftTrigger => 6,
            Self::RightTrigger => 7,
            Self::LeftStick => 8,
            Self::RightStick => 9,
            Self::Start => 10,
            Self::Select => 11,
            Self::Guide => 12,
            Self::DpadUp => 13,
            Self::DpadDown => 14,
            Self::DpadLeft => 15,
            Self::DpadRight => 16,
        }
    }

    /// On-screen prompt label for this button on `brand`.
    ///
    /// Face buttons follow the physical position, so Nintendo pads
    /// correctly read `B/A/Y/X` (confirm sits on the *east* label `A`).
    /// Symbols (`✕○□△`, arrows, `☰`, `⧉`) are drawn by the built-in
    /// extended glyph table — no external brand assets anywhere.
    pub const fn prompt_label(self, brand: GamepadBrand) -> &'static str {
        match brand {
            GamepadBrand::Xbox => match self {
                Self::South => "A",
                Self::East => "B",
                Self::West => "X",
                Self::North => "Y",
                Self::LeftShoulder => "LB",
                Self::RightShoulder => "RB",
                Self::LeftTrigger => "LT",
                Self::RightTrigger => "RT",
                Self::LeftStick => "LS",
                Self::RightStick => "RS",
                Self::Start => "☰",
                Self::Select => "⧉",
                Self::Guide => "Xbox",
                Self::DpadUp => "↑",
                Self::DpadDown => "↓",
                Self::DpadLeft => "←",
                Self::DpadRight => "→",
            },
            GamepadBrand::PlayStation => match self {
                Self::South => "✕",
                Self::East => "○",
                Self::West => "□",
                Self::North => "△",
                Self::LeftShoulder => "L1",
                Self::RightShoulder => "R1",
                Self::LeftTrigger => "L2",
                Self::RightTrigger => "R2",
                Self::LeftStick => "L3",
                Self::RightStick => "R3",
                Self::Start => "Options",
                Self::Select => "Create",
                Self::Guide => "PS",
                Self::DpadUp => "↑",
                Self::DpadDown => "↓",
                Self::DpadLeft => "←",
                Self::DpadRight => "→",
            },
            GamepadBrand::Switch => match self {
                Self::South => "B",
                Self::East => "A",
                Self::West => "Y",
                Self::North => "X",
                Self::LeftShoulder => "L",
                Self::RightShoulder => "R",
                Self::LeftTrigger => "ZL",
                Self::RightTrigger => "ZR",
                Self::LeftStick => "LS",
                Self::RightStick => "RS",
                Self::Start => "+",
                Self::Select => "−",
                Self::Guide => "HOME",
                Self::DpadUp => "↑",
                Self::DpadDown => "↓",
                Self::DpadLeft => "←",
                Self::DpadRight => "→",
            },
            GamepadBrand::SteamDeck => match self {
                Self::South => "A",
                Self::East => "B",
                Self::West => "X",
                Self::North => "Y",
                Self::LeftShoulder => "L1",
                Self::RightShoulder => "R1",
                Self::LeftTrigger => "L2",
                Self::RightTrigger => "R2",
                Self::LeftStick => "L3",
                Self::RightStick => "R3",
                Self::Start => "☰",
                Self::Select => "⧉",
                Self::Guide => "STEAM",
                Self::DpadUp => "↑",
                Self::DpadDown => "↓",
                Self::DpadLeft => "←",
                Self::DpadRight => "→",
            },
        }
    }

    /// Geometric badge spec for drawing this button's prompt without any
    /// external brand assets — shape, symbol and flat RGBA colors a
    /// rasterizer can draw as primitives.
    pub const fn badge_spec(self, brand: GamepadBrand) -> ButtonBadgeSpec {
        let shape = match self {
            Self::South | Self::East | Self::West | Self::North => BadgeShape::Circle,
            Self::LeftStick | Self::RightStick | Self::Guide => BadgeShape::Circle,
            Self::LeftShoulder | Self::RightShoulder => BadgeShape::Bumper,
            Self::LeftTrigger | Self::RightTrigger => BadgeShape::Trigger,
            Self::Start | Self::Select => BadgeShape::Pill,
            Self::DpadUp | Self::DpadDown | Self::DpadLeft | Self::DpadRight => {
                BadgeShape::DpadDir
            }
        };
        let fill_rgba = match brand {
            GamepadBrand::Xbox => match self {
                Self::South => XBOX_FACE_A,
                Self::East => XBOX_FACE_B,
                Self::West => XBOX_FACE_X,
                Self::North => XBOX_FACE_Y,
                _ => XBOX_BASE_FILL,
            },
            GamepadBrand::PlayStation => match self {
                Self::South => PS_FACE_CROSS,
                Self::East => PS_FACE_CIRCLE,
                Self::West => PS_FACE_SQUARE,
                Self::North => PS_FACE_TRIANGLE,
                _ => PS_BASE_FILL,
            },
            GamepadBrand::Switch => SWITCH_FILL,
            GamepadBrand::SteamDeck => STEAMDECK_FILL,
        };
        ButtonBadgeSpec {
            shape,
            symbol: self.prompt_label(brand),
            fill_rgba,
            fg_rgba: BADGE_FG,
            outline_rgba: BADGE_OUTLINE,
        }
    }
}

/// The primitive a renderer should draw behind a prompt symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeShape {
    /// Round face-button / stick-click / guide badge.
    Circle,
    /// Oblong menu button badge (Start/Select).
    Pill,
    /// Shoulder bumper badge.
    Bumper,
    /// Analog trigger badge.
    Trigger,
    /// Directional-pad arrow badge.
    DpadDir,
}

/// Everything needed to paint one clean-room button prompt badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ButtonBadgeSpec {
    /// Badge silhouette.
    pub shape: BadgeShape,
    /// Prompt symbol drawn in `fg_rgba` (same string as
    /// [`GamepadButton::prompt_label`]).
    pub symbol: &'static str,
    /// Badge fill color.
    pub fill_rgba: [u8; 4],
    /// Symbol foreground color.
    pub fg_rgba: [u8; 4],
    /// 1px outline color.
    pub outline_rgba: [u8; 4],
}

const BADGE_FG: [u8; 4] = [245, 245, 247, 255];
const BADGE_OUTLINE: [u8; 4] = [12, 12, 14, 255];
// Clean-room flat approximations of the familiar console button
// colors — geometry only, never brand artwork.
const XBOX_FACE_A: [u8; 4] = [82, 176, 67, 255];
const XBOX_FACE_B: [u8; 4] = [194, 62, 62, 255];
const XBOX_FACE_X: [u8; 4] = [54, 117, 204, 255];
const XBOX_FACE_Y: [u8; 4] = [233, 178, 43, 255];
const XBOX_BASE_FILL: [u8; 4] = [45, 45, 48, 255];
const PS_FACE_CROSS: [u8; 4] = [82, 123, 207, 255];
const PS_FACE_CIRCLE: [u8; 4] = [209, 79, 89, 255];
const PS_FACE_SQUARE: [u8; 4] = [211, 98, 163, 255];
const PS_FACE_TRIANGLE: [u8; 4] = [66, 174, 151, 255];
const PS_BASE_FILL: [u8; 4] = [40, 40, 46, 255];
const SWITCH_FILL: [u8; 4] = [36, 36, 40, 255];
const STEAMDECK_FILL: [u8; 4] = [44, 52, 62, 255];

/// Analog axes (sticks in `-1.0..=1.0`, triggers `0.0..=1.0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadAxis {
    /// Left stick horizontal; `+1` right.
    LeftX,
    /// Left stick vertical; `+1` down (SDL convention).
    LeftY,
    /// Right stick horizontal; `+1` right.
    RightX,
    /// Right stick vertical; `+1` down.
    RightY,
    /// Left trigger analog travel, `0` released → `1` fully pulled.
    LeftTrigger,
    /// Right trigger analog travel.
    RightTrigger,
}

/// Number of [`GamepadAxis`] variants — the size of [`GamepadState::axes`].
pub const GAMEPAD_AXIS_COUNT: usize = 6;

impl GamepadAxis {
    /// Stable index into [`GamepadState::axes`].
    pub const fn index(self) -> usize {
        match self {
            Self::LeftX => 0,
            Self::LeftY => 1,
            Self::RightX => 2,
            Self::RightY => 3,
            Self::LeftTrigger => 4,
            Self::RightTrigger => 5,
        }
    }
}

// ---------------------------------------------------------------------------
// Actions and bilingual names
// ---------------------------------------------------------------------------

/// Semantic actions the mapper can emit — one enum shared by gameplay and
/// UI/container navigation so prompt rendering has a single vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadAction {
    /// South (or East on swapped Nintendo): jump / confirm.
    Jump,
    /// East (or South on swapped Nintendo): drop held item / go back.
    DropOrBack,
    /// West: open/close the inventory.
    OpenInventory,
    /// North: swap hands in-world, craft in containers.
    SwapOrCraft,
    /// Left bumper: previous hotbar slot.
    HotbarPrev,
    /// Right bumper: next hotbar slot.
    HotbarNext,
    /// Left trigger: use item / place block.
    UseItem,
    /// Right trigger: attack / mine.
    AttackMine,
    /// Left stick click: sprint toggle/hold.
    ToggleSprint,
    /// Right stick click: sneak toggle.
    ToggleSneak,
    /// Start: pause menu.
    Pause,
    /// D-pad up: cycle camera perspective (F5).
    PerspectiveCamera,
    /// D-pad left: toggle the debug overlay (F3).
    DebugOverlay,
    /// D-pad right: hold-to-zoom.
    Zoom,
    /// D-pad down: toggle the recipe overlay.
    RecipeOverlay,
    /// UI mode: primary confirm (shift-aware click).
    UiConfirm,
    /// UI mode: cancel / back.
    UiCancel,
    /// UI mode: quick-move — the shift-click equivalent.
    UiQuickMove,
    /// UI mode: split stack — the right-click equivalent.
    UiSplitStack,
    /// UI mode: previous settings/inventory tab.
    UiTabPrev,
    /// UI mode: next settings/inventory tab.
    UiTabNext,
}

impl GamepadAction {
    /// Bilingual display name for prompts and the controls screen.
    pub fn name(self, lang: UiLang) -> &'static str {
        match self {
            Self::Jump => tr(lang, "Jump", "ジャンプ"),
            Self::DropOrBack => tr(lang, "Drop / Back", "ドロップ / 戻る"),
            Self::OpenInventory => tr(lang, "Inventory", "インベントリ"),
            Self::SwapOrCraft => tr(lang, "Swap Hands / Craft", "持ち替え / クラフト"),
            Self::HotbarPrev => tr(lang, "Previous Hotbar Slot", "前のホットバー"),
            Self::HotbarNext => tr(lang, "Next Hotbar Slot", "次のホットバー"),
            Self::UseItem => tr(lang, "Use / Place", "使う / 置く"),
            Self::AttackMine => tr(lang, "Attack / Mine", "攻撃 / 採掘"),
            Self::ToggleSprint => tr(lang, "Sprint", "ダッシュ"),
            Self::ToggleSneak => tr(lang, "Sneak", "スニーク"),
            Self::Pause => tr(lang, "Pause", "一時停止"),
            Self::PerspectiveCamera => tr(lang, "Camera (F5)", "視点切替 (F5)"),
            Self::DebugOverlay => tr(lang, "Debug Overlay (F3)", "デバッグ表示 (F3)"),
            Self::Zoom => tr(lang, "Zoom", "ズーム"),
            Self::RecipeOverlay => tr(lang, "Recipe Overlay", "レシピ表示"),
            Self::UiConfirm => tr(lang, "Confirm", "決定"),
            Self::UiCancel => tr(lang, "Cancel", "キャンセル"),
            Self::UiQuickMove => tr(lang, "Quick Move", "クイック移動"),
            Self::UiSplitStack => tr(lang, "Split Stack", "スタック分割"),
            Self::UiTabPrev => tr(lang, "Previous Tab", "前のタブ"),
            Self::UiTabNext => tr(lang, "Next Tab", "次のタブ"),
        }
    }
}

// ---------------------------------------------------------------------------
// Analog stick processing
// ---------------------------------------------------------------------------

/// Tunable response shaping for one analog stick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StickConfig {
    /// Radial inner deadzone: inputs with magnitude below this read as
    /// exactly `(0, 0)` — kills stick drift without axis snapping.
    pub inner_deadzone: f32,
    /// Outer deadzone: magnitudes past `1.0 - outer_deadzone` saturate to
    /// full deflection, so worn sticks can still hit 100%.
    pub outer_deadzone: f32,
    /// Response curve exponent, clamped to `1.0..=3.0`. `1.0` is linear;
    /// higher values soften the low end for precise aiming.
    pub exponent: f32,
    /// Mirror the Y axis (for players who invert stick/look directions).
    pub invert_y: bool,
}

impl Default for StickConfig {
    fn default() -> Self {
        Self {
            inner_deadzone: 0.18,
            outer_deadzone: 0.08,
            exponent: 1.4,
            invert_y: false,
        }
    }
}

impl StickConfig {
    /// Field values clamped into their legal ranges.
    fn normalized(self) -> Self {
        let inner = self.inner_deadzone.clamp(0.0, 0.9);
        let outer = self.outer_deadzone.clamp(0.0, (0.95 - inner).max(0.0));
        Self {
            inner_deadzone: inner,
            outer_deadzone: outer,
            exponent: self.exponent.clamp(1.0, 3.0),
            invert_y: self.invert_y,
        }
    }
}

/// Process one raw stick sample through `cfg`.
///
/// The deadzone is *radial* (applied to magnitude, not per-axis), so a
/// diagonal push near the deadzone edge never snaps onto a cardinal.
/// Output magnitude ramps smoothly from 0 at the inner deadzone edge to 1
/// at the outer deadzone edge, shaped by `cfg.exponent`. Non-finite inputs
/// (NaN/Inf from a flaky device) fail closed to `(0.0, 0.0)`.
pub fn process_radial_stick(x: f32, y: f32, cfg: &StickConfig) -> (f32, f32) {
    if !x.is_finite() || !y.is_finite() {
        return (0.0, 0.0);
    }
    let cfg = cfg.normalized();
    let mag = x.hypot(y);
    if mag <= cfg.inner_deadzone || mag <= f32::EPSILON {
        return (0.0, 0.0);
    }
    let span = (1.0 - cfg.outer_deadzone - cfg.inner_deadzone).max(f32::EPSILON);
    let t = ((mag.min(1.0) - cfg.inner_deadzone) / span).clamp(0.0, 1.0);
    let shaped = t.powf(cfg.exponent) / mag;
    let mut out_x = (x * shaped).clamp(-1.0, 1.0);
    let mut out_y = (y * shaped).clamp(-1.0, 1.0);
    if cfg.invert_y {
        out_y = -out_y;
    }
    if !out_x.is_finite() {
        out_x = 0.0;
    }
    if !out_y.is_finite() {
        out_y = 0.0;
    }
    (out_x, out_y)
}

// ---------------------------------------------------------------------------
// Device frame state
// ---------------------------------------------------------------------------

/// One raw snapshot of the physical pad, supplied by the host each frame.
/// Non-finite axis values are treated as `0` everywhere downstream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GamepadState {
    /// Digital inputs indexed by [`GamepadButton::index`].
    pub buttons: [bool; GAMEPAD_BUTTON_COUNT],
    /// Analog inputs indexed by [`GamepadAxis::index`].
    pub axes: [f32; GAMEPAD_AXIS_COUNT],
}

impl GamepadState {
    pub fn set_button(&mut self, button: GamepadButton, pressed: bool) {
        self.buttons[button.index()] = pressed;
    }

    pub fn button(&self, button: GamepadButton) -> bool {
        self.buttons[button.index()]
    }

    pub fn set_axis(&mut self, axis: GamepadAxis, value: f32) {
        self.axes[axis.index()] = value;
    }

    /// Axis value with fail-closed finite filtering.
    pub fn axis(&self, axis: GamepadAxis) -> f32 {
        let v = self.axes[axis.index()];
        if v.is_finite() {
            v
        } else {
            0.0
        }
    }
}

// ---------------------------------------------------------------------------
// Rumble / haptics
// ---------------------------------------------------------------------------

/// One bounded haptic pulse, in normalized motor units (`0.0..=1.0`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RumbleCommand {
    /// Low-frequency (heavy) motor amplitude.
    pub low_freq: f32,
    /// High-frequency (light) motor amplitude.
    pub high_freq: f32,
    /// Pulse length in milliseconds.
    pub duration_ms: u32,
}

/// Game events that produce haptic feedback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RumbleKind {
    /// Player took damage — heavy, mid-length thump.
    Damage,
    /// A block finished breaking — short sharp tick.
    BlockBreak,
    /// Bow draw tension — light sustained hum.
    BowDraw,
    /// Critical hit landed — strong both-motor burst.
    CriticalHit,
}

impl RumbleKind {
    const fn base(self) -> RumbleCommand {
        match self {
            Self::Damage => RumbleCommand {
                low_freq: 0.8,
                high_freq: 0.4,
                duration_ms: 250,
            },
            Self::BlockBreak => RumbleCommand {
                low_freq: 0.4,
                high_freq: 0.9,
                duration_ms: 80,
            },
            Self::BowDraw => RumbleCommand {
                low_freq: 0.2,
                high_freq: 0.5,
                duration_ms: 400,
            },
            Self::CriticalHit => RumbleCommand {
                low_freq: 1.0,
                high_freq: 1.0,
                duration_ms: 200,
            },
        }
    }
}

/// Maximum queued rumble commands; the oldest are dropped beyond this so a
/// misbehaving game loop can never grow memory unboundedly.
pub const RUMBLE_QUEUE_MAX: usize = 16;

// ---------------------------------------------------------------------------
// Radial wheel
// ---------------------------------------------------------------------------

/// The eight wheel slices, in slice order (index 0 = east, counting
/// counter-clockwise in 45° sectors).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadialAction {
    /// Toggle the debug overlay (F3).
    DebugOverlay,
    /// Cycle camera perspective (F5).
    PerspectiveCamera,
    /// Take a screenshot (F2).
    Screenshot,
    /// Toggle zoom.
    Zoom,
    /// Toggle the recipe book overlay.
    RecipeOverlay,
    /// Open the inventory.
    Inventory,
    /// Open chat.
    Chat,
    /// Open the advancements screen.
    Advancements,
}

/// Number of slices on the radial wheel.
pub const RADIAL_SLICE_COUNT: usize = 8;

/// Slice order matching [`radial_slice_for_angle`].
pub const RADIAL_ACTIONS: [RadialAction; RADIAL_SLICE_COUNT] = [
    RadialAction::DebugOverlay,
    RadialAction::PerspectiveCamera,
    RadialAction::Screenshot,
    RadialAction::Zoom,
    RadialAction::RecipeOverlay,
    RadialAction::Inventory,
    RadialAction::Chat,
    RadialAction::Advancements,
];

impl RadialAction {
    /// Bilingual wheel label.
    pub fn name(self, lang: UiLang) -> &'static str {
        match self {
            Self::DebugOverlay => tr(lang, "Debug (F3)", "デバッグ (F3)"),
            Self::PerspectiveCamera => tr(lang, "Camera (F5)", "視点 (F5)"),
            Self::Screenshot => tr(lang, "Screenshot", "スクリーンショット"),
            Self::Zoom => tr(lang, "Zoom", "ズーム"),
            Self::RecipeOverlay => tr(lang, "Recipes", "レシピ"),
            Self::Inventory => tr(lang, "Inventory", "インベントリ"),
            Self::Chat => tr(lang, "Chat", "チャット"),
            Self::Advancements => tr(lang, "Advancements", "進捗"),
        }
    }
}

/// Map a wheel angle in degrees to a slice index.
///
/// Angle `0` is east (stick right) and increases counter-clockwise
/// (screen-up is `90`). Slice `i` covers `[i*45 - 22.5, i*45 + 22.5)`;
/// non-finite angles return slice `0`.
pub fn radial_slice_for_angle(angle_deg: f32) -> usize {
    if !angle_deg.is_finite() {
        return 0;
    }
    let deg = angle_deg.rem_euclid(360.0);
    (((deg + 22.5) / 45.0) as usize) % RADIAL_SLICE_COUNT
}

/// State of the Select-held quick wheel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RadialWheelState {
    /// `true` while the wheel is on screen.
    pub open: bool,
    /// Currently highlighted slice, `None` when the stick rests in the
    /// deadzone (neutral selection — releasing Select chooses nothing).
    pub selected: Option<usize>,
}

impl RadialWheelState {
    /// Update the highlight from a processed right-stick sample.
    ///
    /// `x`/`y` are raw stick values (this method applies the deadzone
    /// itself); `+y` is stick-down. Angles are converted so stick-up
    /// selects the top slice.
    pub fn update(&mut self, x: f32, y: f32, deadzone: f32) {
        if !x.is_finite() || !y.is_finite() || x.hypot(y) <= deadzone {
            self.selected = None;
            return;
        }
        // `+y` is down on the device; negate so screen-up is positive
        // before measuring counter-clockwise from east.
        self.selected = Some(radial_slice_for_angle((-y).atan2(x).to_degrees()));
    }

    /// The highlighted action, if any.
    pub fn selected_action(&self) -> Option<RadialAction> {
        self.selected.map(|i| RADIAL_ACTIONS[i % RADIAL_SLICE_COUNT])
    }

    /// Pure-geometry render model: the 8 slice angle spans, label anchor
    /// points, localized labels and highlight state — everything
    /// `client-gpu` needs to draw the wheel without knowing wheel logic.
    ///
    /// `radius <= 0` or non-finite inputs are sanitized to `0`.
    pub fn layout(
        &self,
        center_x: f32,
        center_y: f32,
        radius: f32,
        lang: UiLang,
    ) -> RadialWheelLayout {
        let clean = |v: f32| if v.is_finite() { v } else { 0.0 };
        let (cx, cy, r) = (clean(center_x), clean(center_y), clean(radius).max(0.0));
        let label_r = r * 0.62;
        RadialWheelLayout {
            center_x: cx,
            center_y: cy,
            radius: r,
            open: self.open,
            slices: std::array::from_fn(|i| {
                let center = i as f32 * 45.0;
                let rad = center.to_radians();
                RadialWheelSliceLayout {
                    index: i,
                    action: RADIAL_ACTIONS[i],
                    label: RADIAL_ACTIONS[i].name(lang),
                    angle_start_deg: center - 22.5,
                    angle_center_deg: center,
                    angle_end_deg: center + 22.5,
                    anchor_x: cx + rad.cos() * label_r,
                    // Screen y grows downward; wheel angles run
                    // counter-clockwise, so subtract the sine.
                    anchor_y: cy - rad.sin() * label_r,
                    highlighted: self.selected == Some(i),
                }
            }),
        }
    }
}

/// Draw-ready geometry for one radial-wheel slice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadialWheelSliceLayout {
    /// Slice index `0..8` (0 = east, counter-clockwise).
    pub index: usize,
    /// The action this slice activates.
    pub action: RadialAction,
    /// Localized label for the slice.
    pub label: &'static str,
    /// Sector start angle in degrees (center − 22.5°).
    pub angle_start_deg: f32,
    /// Sector center angle in degrees (`index * 45`).
    pub angle_center_deg: f32,
    /// Sector end angle in degrees (center + 22.5°).
    pub angle_end_deg: f32,
    /// Label anchor X in physical pixels.
    pub anchor_x: f32,
    /// Label anchor Y in physical pixels.
    pub anchor_y: f32,
    /// `true` when the stick currently selects this slice.
    pub highlighted: bool,
}

/// Draw-ready geometry for the whole radial wheel.
#[derive(Clone, Debug, PartialEq)]
pub struct RadialWheelLayout {
    /// Wheel center X in physical pixels.
    pub center_x: f32,
    /// Wheel center Y in physical pixels.
    pub center_y: f32,
    /// Outer wheel radius in physical pixels.
    pub radius: f32,
    /// Mirrors [`RadialWheelState::open`].
    pub open: bool,
    /// All eight slices, in index order.
    pub slices: [RadialWheelSliceLayout; RADIAL_SLICE_COUNT],
}

// ---------------------------------------------------------------------------
// UI frame context and events
// ---------------------------------------------------------------------------

/// Per-frame data the UI navigation mode needs from the layout pass.
#[derive(Clone, Copy, Debug)]
pub struct UiContext<'a> {
    /// Focusable regions produced by the current layout.
    pub hit_regions: &'a [HitRegion],
    /// Virtual-cursor bounds, in physical pixels.
    pub screen_w: f32,
    /// Virtual-cursor bounds, in physical pixels.
    pub screen_h: f32,
}

/// Everything the host may need to act on after [`GamepadMapper::update`].
#[derive(Clone, Debug, PartialEq)]
pub enum GamepadEvent {
    /// A bound action's button/trigger crossed to pressed.
    ActionPressed(GamepadAction),
    /// A bound action was released.
    ActionReleased(GamepadAction),
    /// Camera look delta this frame (degrees; respects `dt`, deadzone,
    /// curve and invert).
    Look {
        /// Horizontal turn rate applied this frame.
        yaw_delta_deg: f32,
        /// Vertical turn rate applied this frame.
        pitch_delta_deg: f32,
    },
    /// Left-stick locomotion, emitted every gameplay frame — `forward`
    /// `+1` means stick pushed up, `strafe` `+1` means right.
    Move {
        /// Forward/back axis (`-1.0..=1.0`).
        forward: f32,
        /// Strafe axis (`-1.0..=1.0`).
        strafe: f32,
        /// `L3` held this frame.
        sprint: bool,
    },
    /// Hotbar selection changed (wrap-around applied); payload is the new
    /// slot index `0..9`.
    HotbarChanged(usize),
    /// Virtual cursor moved (UI mode).
    CursorMoved {
        /// New cursor X in physical pixels.
        x: f32,
        /// New cursor Y in physical pixels.
        y: f32,
    },
    /// D-pad focus jump landed on `hit_regions[region_index]` (UI mode).
    /// Emitted on press edges and by the hold auto-repeater.
    FocusJumped {
        /// Index into the frame's [`UiContext::hit_regions`].
        region_index: usize,
    },
    /// Right-stick scroll for lists, recipes and chests (UI mode).
    /// Values are in scroll ticks, already `dt`-scaled after deadzone.
    UiScroll {
        /// Horizontal scroll ticks.
        dx: f32,
        /// Vertical scroll ticks (`+` = stick down / scroll down).
        dy: f32,
    },
    /// UI mode: the confirm press landed on a text field — the host
    /// should open the virtual keyboard instead of a click.
    RequestVirtualKeyboard,
    /// The radial wheel opened (Select pressed).
    RadialOpened,
    /// Highlighted slice changed while the wheel is open.
    RadialSelectionChanged(Option<usize>),
    /// The radial wheel closed; `Some(slice)` means confirm the action.
    RadialClosed {
        /// Selected slice index, or `None` for neutral release.
        selected: Option<usize>,
    },
}

// ---------------------------------------------------------------------------
// Mapper
// ---------------------------------------------------------------------------

/// Hotbar slot count (vanilla).
pub const HOTBAR_SLOTS: usize = 9;
/// Full-deflection camera speed.
pub const LOOK_SPEED_DEG_PER_SEC: f32 = 180.0;
/// Free-roam virtual cursor speed.
pub const CURSOR_SPEED_PX_PER_SEC: f32 = 480.0;
/// Distance from a region center where magnetic slowdown starts.
pub const SNAP_RADIUS_PX: f32 = 32.0;
/// Cursor speed multiplier at the exact center of a snap target.
pub const SNAP_SLOWDOWN: f32 = 0.25;
/// How hard the cursor is pulled toward a snap target's center (1/s).
pub const SNAP_PULL_PER_SEC: f32 = 6.0;
/// Analog trigger travel that counts as a press.
pub const TRIGGER_PRESS_THRESHOLD: f32 = 0.7;
/// Travel a pressed trigger must fall to before it releases — the
/// hysteresis band between this and [`TRIGGER_PRESS_THRESHOLD`] prevents
/// flicker on noisy analog inputs.
pub const TRIGGER_RELEASE_THRESHOLD: f32 = 0.55;
/// Largest `dt` the mapper integrates per frame; bigger gaps (loading
/// hitches) are clamped so look/cursor never teleport.
pub const MAX_FRAME_DT_SECS: f32 = 0.1;
/// UI mode: how long a D-pad direction must be held before focus jumps
/// start auto-repeating.
pub const DPAD_REPEAT_DELAY_SECS: f32 = 0.35;
/// UI mode: interval between auto-repeat focus jumps once the initial
/// [`DPAD_REPEAT_DELAY_SECS`] has elapsed.
pub const DPAD_REPEAT_INTERVAL_SECS: f32 = 0.09;
/// UI mode: right-stick scroll speed in list ticks per second at full
/// deflection.
pub const UI_SCROLL_TICKS_PER_SEC: f32 = 6.0;
/// Mouse motion magnitude (px) that flips the input source back to
/// keyboard/mouse in [`InputModeTracker`].
pub const MOUSE_SWITCH_THRESHOLD_PX: f32 = 4.0;
/// Minimum axis deflection counted as gamepad activity by
/// [`InputModeTracker`] — comfortably above stick drift.
pub const GAMEPAD_ACTIVITY_THRESHOLD: f32 = 0.1;

// ---------------------------------------------------------------------------
// Input source switching & movement modes
// ---------------------------------------------------------------------------

/// Which device family currently owns the UI — drives prompt glyph and
/// hint-bar flavor. Switching is seamless: whichever device produced
/// activity last wins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputSourceMode {
    /// Keyboard and mouse.
    #[default]
    KeyboardMouse,
    /// Any controller handled by this engine.
    Gamepad,
}

/// Tracks the active [`InputSourceMode`]. Deterministic: driven purely by
/// the events fed to it, never by wall-clock time.
#[derive(Clone, Debug, Default)]
pub struct InputModeTracker {
    mode: InputSourceMode,
}

impl InputModeTracker {
    pub fn mode(&self) -> InputSourceMode {
        self.mode
    }

    /// Any button press or stick/trigger deflection beyond
    /// [`GAMEPAD_ACTIVITY_THRESHOLD`] immediately selects
    /// [`InputSourceMode::Gamepad`].
    pub fn on_gamepad_state(&mut self, state: &GamepadState) -> InputSourceMode {
        let active = state.buttons.iter().any(|&b| b)
            || state
                .axes
                .iter()
                .any(|&v| v.is_finite() && v.abs() >= GAMEPAD_ACTIVITY_THRESHOLD);
        if active {
            self.mode = InputSourceMode::Gamepad;
        }
        self.mode
    }

    /// Any keyboard input selects keyboard/mouse.
    pub fn on_keyboard_input(&mut self) -> InputSourceMode {
        self.mode = InputSourceMode::KeyboardMouse;
        self.mode
    }

    /// Mouse motion flips to keyboard/mouse once the frame delta reaches
    /// [`MOUSE_SWITCH_THRESHOLD_PX`] — small jitter never causes a
    /// prompt-flavor flicker.
    pub fn on_mouse_motion(&mut self, dx: f32, dy: f32) -> InputSourceMode {
        if dx.is_finite()
            && dy.is_finite()
            && dx.hypot(dy) >= MOUSE_SWITCH_THRESHOLD_PX
        {
            self.mode = InputSourceMode::KeyboardMouse;
        }
        self.mode
    }

    /// Any mouse button click selects keyboard/mouse.
    pub fn on_mouse_button(&mut self) -> InputSourceMode {
        self.mode = InputSourceMode::KeyboardMouse;
        self.mode
    }
}

/// How the left stick maps onto movement input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnalogMovementMode {
    /// Full 360° analog — default for local worlds and Megu servers.
    #[default]
    Full360,
    /// Snap stick direction to the 8 vanilla cardinal/diagonal vectors.
    /// For strict third-party servers whose anti-cheat rejects
    /// non-keyboard movement angles.
    EightWayVanillaCompat,
}

/// The physical button meaning "confirm / jump / primary" for `brand`
/// under the Nintendo confirm-swap setting.
const fn confirm_button_for(brand: GamepadBrand, nintendo_confirm_swap: bool) -> GamepadButton {
    if matches!(brand, GamepadBrand::Switch) && nintendo_confirm_swap {
        GamepadButton::East
    } else {
        GamepadButton::South
    }
}

/// The physical button meaning "cancel / drop / back" for `brand`.
const fn cancel_button_for(brand: GamepadBrand, nintendo_confirm_swap: bool) -> GamepadButton {
    if matches!(brand, GamepadBrand::Switch) && nintendo_confirm_swap {
        GamepadButton::South
    } else {
        GamepadButton::East
    }
}

/// Snap a direction vector to the nearest of the 8 vanilla movement
/// directions (45° sectors), preserving magnitude.
fn snap_to_eight_way(x: f32, y: f32) -> (f32, f32) {
    let mag = x.hypot(y);
    if !mag.is_finite() || mag <= f32::EPSILON {
        return (0.0, 0.0);
    }
    let snapped = (y.atan2(x) / std::f32::consts::FRAC_PI_4).round()
        * std::f32::consts::FRAC_PI_4;
    (snapped.cos() * mag, snapped.sin() * mag)
}

/// The stateful mapper: feed it [`GamepadState`] frames, get
/// [`GamepadEvent`]s, [`RumbleCommand`]s and UI focus/cursor state back.
#[derive(Clone, Debug)]
pub struct GamepadMapper {
    /// Device family for prompts and the confirm-swap option.
    pub brand: GamepadBrand,
    /// Left stick (movement / virtual cursor) shaping.
    pub left_stick: StickConfig,
    /// Right stick (camera / radial wheel) shaping.
    pub right_stick: StickConfig,
    /// Analog trigger press threshold.
    pub trigger_press: f32,
    /// Analog trigger release threshold (hysteresis).
    pub trigger_release: f32,
    /// When `brand` is [`GamepadBrand::Switch`], treat the east (`A`)
    /// button as confirm/jump and south (`B`) as cancel/back — matching
    /// Nintendo system conventions. No effect on other brands.
    pub nintendo_confirm_swap: bool,
    /// Master haptics switch.
    pub rumble_enabled: bool,
    /// Haptic intensity scale, clamped to `0.0..=1.0` per command.
    pub rumble_strength: f32,
    /// UI/container navigation mode: the left stick drives the virtual
    /// cursor, the D-pad performs focus jumps and the face buttons become
    /// confirm/cancel/quick-move/split-stack instead of gameplay binds.
    pub ui_mode: bool,
    /// Left-stick movement semantics (full analog vs 8-way snap).
    pub movement_mode: AnalogMovementMode,
    prev_buttons: [bool; GAMEPAD_BUTTON_COUNT],
    dpad_held_dir: Option<(i32, i32)>,
    dpad_held_secs: f32,
    dpad_next_repeat: f32,
    lt_pressed: bool,
    rt_pressed: bool,
    hotbar_slot: usize,
    cursor_x: f32,
    cursor_y: f32,
    focused_region: Option<usize>,
    radial: RadialWheelState,
    rumble_queue: VecDeque<RumbleCommand>,
}

impl Default for GamepadMapper {
    fn default() -> Self {
        Self::new(GamepadBrand::Xbox)
    }
}

impl GamepadMapper {
    pub fn new(brand: GamepadBrand) -> Self {
        Self {
            brand,
            left_stick: StickConfig::default(),
            right_stick: StickConfig::default(),
            trigger_press: TRIGGER_PRESS_THRESHOLD,
            trigger_release: TRIGGER_RELEASE_THRESHOLD,
            nintendo_confirm_swap: true,
            rumble_enabled: true,
            rumble_strength: 1.0,
            ui_mode: false,
            movement_mode: AnalogMovementMode::Full360,
            prev_buttons: [false; GAMEPAD_BUTTON_COUNT],
            dpad_held_dir: None,
            dpad_held_secs: 0.0,
            dpad_next_repeat: DPAD_REPEAT_DELAY_SECS,
            lt_pressed: false,
            rt_pressed: false,
            hotbar_slot: 0,
            cursor_x: 0.0,
            cursor_y: 0.0,
            focused_region: None,
            radial: RadialWheelState::default(),
            rumble_queue: VecDeque::new(),
        }
    }

    /// The physical button that means "confirm / jump / primary" right
    /// now — `East` (`A`) on Switch when [`Self::nintendo_confirm_swap`]
    /// is set, `South` everywhere else.
    pub fn confirm_button(&self) -> GamepadButton {
        confirm_button_for(self.brand, self.nintendo_confirm_swap)
    }

    /// The physical button that means "cancel / drop / back".
    pub fn cancel_button(&self) -> GamepadButton {
        cancel_button_for(self.brand, self.nintendo_confirm_swap)
    }

    /// Current hotbar slot (`0..9`).
    pub fn hotbar_slot(&self) -> usize {
        self.hotbar_slot
    }

    /// Virtual cursor position in physical pixels.
    pub fn cursor(&self) -> (f32, f32) {
        (self.cursor_x, self.cursor_y)
    }

    /// Region index the D-pad last focused, if still meaningful.
    pub fn focused_region(&self) -> Option<usize> {
        self.focused_region
    }

    /// The radial wheel's live state.
    pub fn radial(&self) -> &RadialWheelState {
        &self.radial
    }

    /// Queue haptics for `kind`, honoring `rumble_enabled`,
    /// `rumble_strength` and [`RUMBLE_QUEUE_MAX`].
    pub fn emit_rumble(&mut self, kind: RumbleKind) {
        let strength = if self.rumble_strength.is_finite() {
            self.rumble_strength.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if !self.rumble_enabled || strength <= 0.0 {
            return;
        }
        if self.rumble_queue.len() >= RUMBLE_QUEUE_MAX {
            self.rumble_queue.pop_front();
        }
        let base = kind.base();
        self.rumble_queue.push_back(RumbleCommand {
            low_freq: (base.low_freq * strength).clamp(0.0, 1.0),
            high_freq: (base.high_freq * strength).clamp(0.0, 1.0),
            duration_ms: base.duration_ms,
        });
    }

    /// Pop the oldest pending haptic command, if any.
    pub fn poll_rumble(&mut self) -> Option<RumbleCommand> {
        self.rumble_queue.pop_front()
    }

    /// Number of haptic commands waiting for the device.
    pub fn pending_rumble(&self) -> usize {
        self.rumble_queue.len()
    }

    /// Advance one frame. `ui` is required for cursor/focus behavior in UI
    /// mode; gameplay binds work without it.
    pub fn update(
        &mut self,
        raw: &GamepadState,
        dt_secs: f32,
        ui: Option<UiContext<'_>>,
    ) -> Vec<GamepadEvent> {
        let dt = if dt_secs.is_finite() {
            dt_secs.clamp(0.0, MAX_FRAME_DT_SECS)
        } else {
            0.0
        };
        let mut events = Vec::new();

        self.update_triggers(raw, &mut events);
        self.update_buttons(raw, ui, &mut events);

        if self.ui_mode {
            self.update_dpad_repeat(raw, dt, ui, &mut events);
            if let Some(ctx) = ui {
                self.update_cursor(raw, dt, ctx, &mut events);
            }
            self.update_ui_scroll(raw, dt, &mut events);
        } else {
            self.update_radial(raw, &mut events);
            self.update_sticks(raw, dt, &mut events);
        }

        self.prev_buttons = raw.buttons;
        events
    }

    // -- triggers --------------------------------------------------------

    fn update_triggers(&mut self, raw: &GamepadState, events: &mut Vec<GamepadEvent>) {
        for (axis, pressed, action) in [
            (
                GamepadAxis::LeftTrigger,
                &mut self.lt_pressed,
                GamepadAction::UseItem,
            ),
            (
                GamepadAxis::RightTrigger,
                &mut self.rt_pressed,
                GamepadAction::AttackMine,
            ),
        ] {
            let v = raw.axis(axis);
            let (press, release) = (self.trigger_press, self.trigger_release);
            let was = *pressed;
            if !was && v >= press {
                *pressed = true;
                events.push(GamepadEvent::ActionPressed(action));
            } else if was && v <= release {
                *pressed = false;
                events.push(GamepadEvent::ActionReleased(action));
            }
        }
    }

    // -- buttons ---------------------------------------------------------

    fn update_buttons(
        &mut self,
        raw: &GamepadState,
        ui: Option<UiContext<'_>>,
        events: &mut Vec<GamepadEvent>,
    ) {
        for (i, &now) in raw.buttons.iter().enumerate() {
            let was = self.prev_buttons[i];
            if now == was {
                continue;
            }
            let btn = GAMEPAD_BUTTONS[i];
            if now {
                self.on_button_press(btn, ui, events);
            } else {
                self.on_button_release(btn, events);
            }
        }
    }

    fn gameplay_action(&self, btn: GamepadButton) -> Option<GamepadAction> {
        if btn == self.confirm_button() {
            return Some(GamepadAction::Jump);
        }
        if btn == self.cancel_button() {
            return Some(GamepadAction::DropOrBack);
        }
        match btn {
            GamepadButton::West => Some(GamepadAction::OpenInventory),
            GamepadButton::North => Some(GamepadAction::SwapOrCraft),
            GamepadButton::LeftShoulder => Some(GamepadAction::HotbarPrev),
            GamepadButton::RightShoulder => Some(GamepadAction::HotbarNext),
            GamepadButton::LeftTrigger => Some(GamepadAction::UseItem),
            GamepadButton::RightTrigger => Some(GamepadAction::AttackMine),
            GamepadButton::LeftStick => Some(GamepadAction::ToggleSprint),
            GamepadButton::RightStick => Some(GamepadAction::ToggleSneak),
            GamepadButton::Start => Some(GamepadAction::Pause),
            GamepadButton::DpadUp => Some(GamepadAction::PerspectiveCamera),
            GamepadButton::DpadDown => Some(GamepadAction::RecipeOverlay),
            GamepadButton::DpadLeft => Some(GamepadAction::DebugOverlay),
            GamepadButton::DpadRight => Some(GamepadAction::Zoom),
            _ => None,
        }
    }

    fn ui_action(&self, btn: GamepadButton) -> Option<GamepadAction> {
        if btn == self.confirm_button() {
            return Some(GamepadAction::UiConfirm);
        }
        if btn == self.cancel_button() {
            return Some(GamepadAction::UiCancel);
        }
        match btn {
            GamepadButton::North => Some(GamepadAction::UiQuickMove),
            GamepadButton::West => Some(GamepadAction::UiSplitStack),
            GamepadButton::LeftShoulder => Some(GamepadAction::UiTabPrev),
            GamepadButton::RightShoulder => Some(GamepadAction::UiTabNext),
            GamepadButton::Start => Some(GamepadAction::Pause),
            _ => None,
        }
    }

    fn dpad_dir(btn: GamepadButton) -> Option<(i32, i32)> {
        match btn {
            GamepadButton::DpadUp => Some((0, -1)),
            GamepadButton::DpadDown => Some((0, 1)),
            GamepadButton::DpadLeft => Some((-1, 0)),
            GamepadButton::DpadRight => Some((1, 0)),
            _ => None,
        }
    }

    fn on_button_press(
        &mut self,
        btn: GamepadButton,
        ui: Option<UiContext<'_>>,
        events: &mut Vec<GamepadEvent>,
    ) {
        if self.ui_mode {
            if let (Some((dx, dy)), Some(ctx)) = (Self::dpad_dir(btn), ui) {
                if let Some(i) = self.focus_jump(dx, dy, ctx.hit_regions) {
                    events.push(GamepadEvent::FocusJumped { region_index: i });
                }
                return;
            }
            if btn == self.confirm_button() && self.confirm_targets_text_field(ui) {
                events.push(GamepadEvent::RequestVirtualKeyboard);
                return;
            }
            if let Some(action) = self.ui_action(btn) {
                events.push(GamepadEvent::ActionPressed(action));
            }
            return;
        }

        if btn == GamepadButton::Select {
            self.radial.open = true;
            self.radial.selected = None;
            events.push(GamepadEvent::RadialOpened);
            return;
        }
        match btn {
            GamepadButton::LeftShoulder => {
                self.hotbar_slot = (self.hotbar_slot + HOTBAR_SLOTS - 1) % HOTBAR_SLOTS;
                events.push(GamepadEvent::HotbarChanged(self.hotbar_slot));
            }
            GamepadButton::RightShoulder => {
                self.hotbar_slot = (self.hotbar_slot + 1) % HOTBAR_SLOTS;
                events.push(GamepadEvent::HotbarChanged(self.hotbar_slot));
            }
            _ => {}
        }
        if let Some(action) = self.gameplay_action(btn) {
            events.push(GamepadEvent::ActionPressed(action));
        }
    }

    fn on_button_release(&mut self, btn: GamepadButton, events: &mut Vec<GamepadEvent>) {
        if btn == GamepadButton::Select && self.radial.open {
            self.radial.open = false;
            events.push(GamepadEvent::RadialClosed {
                selected: self.radial.selected,
            });
            self.radial.selected = None;
            return;
        }
        let action = if self.ui_mode {
            self.ui_action(btn)
        } else {
            self.gameplay_action(btn)
        };
        if let Some(action) = action {
            events.push(GamepadEvent::ActionReleased(action));
        }
    }

    // -- sticks ----------------------------------------------------------

    fn update_sticks(&mut self, raw: &GamepadState, dt: f32, events: &mut Vec<GamepadEvent>) {
        let (lx, ly) = process_radial_stick(
            raw.axis(GamepadAxis::LeftX),
            raw.axis(GamepadAxis::LeftY),
            &self.left_stick,
        );
        // Anti-cheat compatibility: collapse the analog direction to the
        // 8 vanilla vectors when the strict movement mode is selected.
        let (lx, ly) = match self.movement_mode {
            AnalogMovementMode::Full360 => (lx, ly),
            AnalogMovementMode::EightWayVanillaCompat => snap_to_eight_way(lx, ly),
        };
        // `+y` is stick-down on the device; forward is up.
        events.push(GamepadEvent::Move {
            forward: (-ly).clamp(-1.0, 1.0),
            strafe: lx.clamp(-1.0, 1.0),
            sprint: raw.button(GamepadButton::LeftStick),
        });

        // While the wheel is open the right stick selects slices instead
        // of steering the camera.
        if self.radial.open {
            return;
        }
        let (rx, ry) = process_radial_stick(
            raw.axis(GamepadAxis::RightX),
            raw.axis(GamepadAxis::RightY),
            &self.right_stick,
        );
        let yaw = rx * LOOK_SPEED_DEG_PER_SEC * dt;
        // Stick-up (`ry < 0`) should pitch the camera up.
        let pitch = -ry * LOOK_SPEED_DEG_PER_SEC * dt;
        if yaw != 0.0 || pitch != 0.0 {
            events.push(GamepadEvent::Look {
                yaw_delta_deg: yaw,
                pitch_delta_deg: pitch,
            });
        }
    }

    fn update_radial(&mut self, raw: &GamepadState, events: &mut Vec<GamepadEvent>) {
        if !self.radial.open {
            return;
        }
        let before = self.radial.selected;
        self.radial.update(
            raw.axis(GamepadAxis::RightX),
            raw.axis(GamepadAxis::RightY),
            self.right_stick.normalized().inner_deadzone,
        );
        if self.radial.selected != before {
            events.push(GamepadEvent::RadialSelectionChanged(self.radial.selected));
        }
    }

    // -- UI navigation ---------------------------------------------------

    /// D-pad hold auto-repeat: after [`DPAD_REPEAT_DELAY_SECS`] the focus
    /// jump refires every [`DPAD_REPEAT_INTERVAL_SECS`] until release or
    /// a direction change. Fully deterministic — driven by `dt`, never
    /// the wall clock.
    fn update_dpad_repeat(
        &mut self,
        raw: &GamepadState,
        dt: f32,
        ui: Option<UiContext<'_>>,
        events: &mut Vec<GamepadEvent>,
    ) {
        let held = if raw.button(GamepadButton::DpadUp) {
            Some((0, -1))
        } else if raw.button(GamepadButton::DpadDown) {
            Some((0, 1))
        } else if raw.button(GamepadButton::DpadLeft) {
            Some((-1, 0))
        } else if raw.button(GamepadButton::DpadRight) {
            Some((1, 0))
        } else {
            None
        };
        if held != self.dpad_held_dir {
            self.dpad_held_dir = held;
            self.dpad_held_secs = 0.0;
            self.dpad_next_repeat = DPAD_REPEAT_DELAY_SECS;
        }
        let (Some((dx, dy)), Some(ctx)) = (held, ui) else {
            return;
        };
        self.dpad_held_secs += dt;
        while self.dpad_held_secs >= self.dpad_next_repeat {
            self.dpad_next_repeat += DPAD_REPEAT_INTERVAL_SECS;
            if let Some(i) = self.focus_jump(dx, dy, ctx.hit_regions) {
                events.push(GamepadEvent::FocusJumped { region_index: i });
            }
        }
    }

    /// Right-stick UI scroll for recipe lists, settings and chests.
    fn update_ui_scroll(&mut self, raw: &GamepadState, dt: f32, events: &mut Vec<GamepadEvent>) {
        let (rx, ry) = process_radial_stick(
            raw.axis(GamepadAxis::RightX),
            raw.axis(GamepadAxis::RightY),
            &self.right_stick,
        );
        if rx != 0.0 || ry != 0.0 {
            events.push(GamepadEvent::UiScroll {
                dx: rx * UI_SCROLL_TICKS_PER_SEC * dt,
                dy: ry * UI_SCROLL_TICKS_PER_SEC * dt,
            });
        }
    }

    /// Whether a confirm press would land on a text field (focused
    /// region first, else the region under the virtual cursor).
    fn confirm_targets_text_field(&self, ui: Option<UiContext<'_>>) -> bool {
        let Some(ctx) = ui else {
            return false;
        };
        if let Some(r) = self
            .focused_region
            .and_then(|i| ctx.hit_regions.get(i))
        {
            return r.id.is_text_field();
        }
        ctx.hit_regions.iter().any(|r| {
            r.id.is_text_field()
                && r.contains(self.cursor_x as i32, self.cursor_y as i32)
        })
    }

    fn update_cursor(
        &mut self,
        raw: &GamepadState,
        dt: f32,
        ctx: UiContext<'_>,
        events: &mut Vec<GamepadEvent>,
    ) {
        let (sx, sy) = process_radial_stick(
            raw.axis(GamepadAxis::LeftX),
            raw.axis(GamepadAxis::LeftY),
            &self.left_stick,
        );
        if sx == 0.0 && sy == 0.0 {
            return;
        }

        // Magnetic snap: inside SNAP_RADIUS_PX of a region center, speed
        // scales down toward SNAP_SLOWDOWN and the cursor is gently pulled
        // toward the center — Controllable's snap-to-slot feel.
        let mut speed = CURSOR_SPEED_PX_PER_SEC;
        if let Some((cx, cy, dist)) = nearest_region(self.cursor_x, self.cursor_y, ctx.hit_regions)
        {
            if dist < SNAP_RADIUS_PX {
                let t = 1.0 - dist / SNAP_RADIUS_PX;
                speed *= 1.0 - t * (1.0 - SNAP_SLOWDOWN);
                let pull = (SNAP_PULL_PER_SEC * t * dt).min(1.0);
                self.cursor_x += (cx - self.cursor_x) * pull;
                self.cursor_y += (cy - self.cursor_y) * pull;
            }
        }

        let old = (self.cursor_x, self.cursor_y);
        self.cursor_x = (self.cursor_x + sx * speed * dt).clamp(0.0, ctx.screen_w.max(0.0));
        self.cursor_y = (self.cursor_y + sy * speed * dt).clamp(0.0, ctx.screen_h.max(0.0));
        if (self.cursor_x, self.cursor_y) != old {
            events.push(GamepadEvent::CursorMoved {
                x: self.cursor_x,
                y: self.cursor_y,
            });
        }
    }

    /// D-pad spatial focus jump: land on the nearest region whose center
    /// lies inside the 90° cone around `(dx, dy)` from the current focus
    /// origin (last focused region, else the cursor). Also snaps the
    /// cursor onto the target's center.
    pub fn focus_jump(
        &mut self,
        dx: i32,
        dy: i32,
        regions: &[HitRegion],
    ) -> Option<usize> {
        let (dx, dy) = (dx.signum() as f32, dy.signum() as f32);
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        let (ox, oy) = self
            .focused_region
            .and_then(|i| regions.get(i))
            .map(region_center)
            .unwrap_or((self.cursor_x, self.cursor_y));

        let mut best: Option<(usize, f32)> = None;
        for (i, r) in regions.iter().enumerate() {
            let (cx, cy) = region_center(r);
            let offx = cx - ox;
            let offy = cy - oy;
            let proj = offx * dx + offy * dy;
            let perp = (offx * dy - offy * dx).abs();
            if proj > 0.5 && perp <= proj {
                let dist2 = offx * offx + offy * offy;
                if best.is_none_or(|(_, d)| dist2 < d) {
                    best = Some((i, dist2));
                }
            }
        }
        let (i, _) = best?;
        let (cx, cy) = region_center(&regions[i]);
        self.cursor_x = cx;
        self.cursor_y = cy;
        self.focused_region = Some(i);
        Some(i)
    }
}

/// Center of a hit region in physical pixels.
fn region_center(r: &HitRegion) -> (f32, f32) {
    (
        r.rect.x as f32 + r.rect.w as f32 * 0.5,
        r.rect.y as f32 + r.rect.h as f32 * 0.5,
    )
}

/// Nearest region center to `(x, y)` as `(cx, cy, distance)`.
fn nearest_region(x: f32, y: f32, regions: &[HitRegion]) -> Option<(f32, f32, f32)> {
    let mut best: Option<(f32, f32, f32)> = None;
    for r in regions {
        let (cx, cy) = region_center(r);
        let d = (cx - x).hypot(cy - y);
        if best.is_none_or(|(_, _, bd)| d < bd) {
            best = Some((cx, cy, d));
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Context button-hint bar
// ---------------------------------------------------------------------------

/// Which screen family the hint bar describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintContext {
    /// In-world gameplay (HUD).
    Gameplay,
    /// Player inventory.
    Inventory,
    /// Chest / container screens.
    Container,
    /// Crafting table / recipe screens.
    Crafting,
    /// Pause menu.
    PauseMenu,
    /// Settings screens.
    Settings,
    /// Text input (sign, chat, search fields).
    TextInput,
}

/// One hint-bar entry: a clean-room button badge plus a localized label.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConsoleHintItem {
    /// Geometric badge for the bound button on the current brand.
    pub badge: ButtonBadgeSpec,
    /// Bilingual action label.
    pub label: &'static str,
}

/// Controllable-style context hint bar: the short row of
/// "button = action" prompts shown at the bottom of every screen.
///
/// Badges follow `brand`; the confirm/cancel pair honors
/// `nintendo_confirm_swap` exactly like [`GamepadMapper`].
pub fn build_context_hints(
    context: HintContext,
    brand: GamepadBrand,
    lang: UiLang,
    nintendo_confirm_swap: bool,
) -> Vec<ConsoleHintItem> {
    let confirm = confirm_button_for(brand, nintendo_confirm_swap);
    let cancel = cancel_button_for(brand, nintendo_confirm_swap);
    let item = |btn: GamepadButton, label: &'static str| ConsoleHintItem {
        badge: btn.badge_spec(brand),
        label,
    };
    match context {
        HintContext::Gameplay => vec![
            item(confirm, tr(lang, "Jump", "ジャンプ")),
            item(cancel, tr(lang, "Drop", "ドロップ")),
            item(GamepadButton::West, tr(lang, "Inventory", "インベントリ")),
            item(
                GamepadButton::LeftShoulder,
                tr(lang, "Prev Slot", "前のスロット"),
            ),
            item(
                GamepadButton::RightShoulder,
                tr(lang, "Next Slot", "次のスロット"),
            ),
            item(GamepadButton::LeftTrigger, tr(lang, "Use / Place", "使う / 置く")),
            item(
                GamepadButton::RightTrigger,
                tr(lang, "Attack / Mine", "攻撃 / 採掘"),
            ),
            item(GamepadButton::Start, tr(lang, "Pause", "一時停止")),
        ],
        HintContext::Inventory | HintContext::Container => vec![
            item(confirm, tr(lang, "Confirm", "決定")),
            item(
                GamepadButton::North,
                tr(lang, "Quick Move", "クイック移動"),
            ),
            item(
                GamepadButton::West,
                tr(lang, "Split Stack", "半分に分ける"),
            ),
            item(
                GamepadButton::RightStick,
                tr(lang, "Scroll", "スクロール"),
            ),
            item(cancel, tr(lang, "Back", "戻る")),
        ],
        HintContext::Crafting => vec![
            item(confirm, tr(lang, "Craft", "クラフト")),
            item(
                GamepadButton::North,
                tr(lang, "Quick Move", "クイック移動"),
            ),
            item(
                GamepadButton::DpadDown,
                tr(lang, "Recipes", "レシピ"),
            ),
            item(
                GamepadButton::RightStick,
                tr(lang, "Scroll", "スクロール"),
            ),
            item(cancel, tr(lang, "Back", "戻る")),
        ],
        HintContext::PauseMenu => vec![
            item(confirm, tr(lang, "Confirm", "決定")),
            item(cancel, tr(lang, "Back", "戻る")),
        ],
        HintContext::Settings => vec![
            item(confirm, tr(lang, "Confirm", "決定")),
            item(
                GamepadButton::LeftShoulder,
                tr(lang, "Prev Tab", "前のタブ"),
            ),
            item(
                GamepadButton::RightShoulder,
                tr(lang, "Next Tab", "次のタブ"),
            ),
            item(
                GamepadButton::RightStick,
                tr(lang, "Scroll", "スクロール"),
            ),
            item(cancel, tr(lang, "Close", "閉じる")),
        ],
        HintContext::TextInput => vec![
            item(confirm, tr(lang, "Submit", "決定")),
            item(cancel, tr(lang, "Close", "閉じる")),
        ],
    }
}
