// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `winit` 0.30 window + input loop: `ApplicationHandler`, FPS camera
//! controls, pause/settings via `megu_ui`, background-FPS throttling,
//! and `FrameBudgetGovernor` pacing — all driving the real `wgpu` renderer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::live_session::LiveHudFeed;
use megu_client::audio_qol::{ZoomState, DEFAULT_ZOOM_SMOOTHING_RATE};
use megu_client::friend_client::{FriendGuestBridge, FriendHostSession};
use megu_client::integrated;
use megu_client::protocol::encode_slot_stack;
use megu_client::render::{FrameBudgetGovernor, RenderConfig, ThrottleStage};
use megu_client::spatial_audio::SpatialAudioPreset;
use megu_client::zsh_command_bar::ZshCommandBar;
use megu_client_mesh::builtin_shadow_wgsl::{BuiltinShadowQuality, RenderScalePreset};
use megu_client_mesh::tactile_effects::{TactileEffectsConfig, TactilePreset};
use megu_client_mesh::ChunkUploadBudget;
use megu_kernel::debug_chords::{
    finish_gamemode_switcher, handle_f3_chord, DebugChordAction, DebugChordContext,
    DebugChordState, DebugTarget, F3Chord, PlayerDebugInfo,
};
use megu_ui::container_screen::{
    menu_schema, ContainerScreenState, DragState, InventorySortState, MenuScreenSchema, SlotDef,
    SlotKind, UiItemStack,
};
use megu_ui::draw::tessellate_draw_list;
use megu_ui::draw::{text_width, UiColor, UiCommand, UiDrawList, UiRect};
use megu_ui::hud_widgets::ScoreboardEntry;
use megu_ui::recipe_browser::{
    BrowserTab, DetailMode, ItemEntry, RecipeBrowserAction, RecipeBrowserState,
};
use megu_ui::magic_hud::build_magic_hud_draw_list;
use megu_ui::magic_hud::LookingAtBlockState;
use megu_ui::state::{invite_code_display, ScoreboardState, TitleOverlayState};
use megu_ui::state::{
    Screen, SettingsTab, UiAction, UiInputEvent, UiKey, UiSettings, UiState, VideoPreset,
    VisualStylePreset,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};

use crate::live_session::{LiveSession, PlayerMovement, PlayerPose, SessionPaths, SessionStatus};
use crate::renderer::{request_gpu_device, GpuRenderer};
use crate::title_branding::{title_branding_sources, TitleBrandingAssets};
use crate::{
    build_demo_scene, detect_display_server_kind, fingerprint_draw_list, preferred_alpha_mode,
    present_mode_for, select_display_presentation_policy, Camera, DisplayPresentationPolicy,
    GpuCliConfig, MoveInput,
};

/// Window title.
pub const WINDOW_TITLE: &str = "megu";

/// Event-loop wake interval while the surface is occluded: slow enough for
/// zero-GPU sleep, fast enough to keep a live session's network pump alive.
const OCCLUDED_TICK: Duration = Duration::from_millis(50);

/// F3 diagnostic line refresh interval: the overlay reads smoothly at
/// 10 Hz, and a stable actionbar keeps the draw-list fingerprint — and
/// therefore the UI mesh upload — quiet on steady frames.
const F3_REFRESH: Duration = Duration::from_millis(100);

const THIRD_PERSON_DISTANCE: f32 = 4.0;
const SPRINT_FOV_BOOST: f32 = 0.10;
const SPRINT_FOV_SMOOTHING_RATE: f32 = 12.0;

/// Friend-relay session timeout (register, join, revoke round-trips).
const FRIEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Non-registry menu id for the client-side window-0 player inventory —
/// `menu_schema` owns `0..=24` and this sentinel can never collide.
pub const PLAYER_INVENTORY_MENU_ID: u32 = u32::MAX;

/// Vanilla window-0 player inventory layout (46 slots): crafting output +
/// 2x2 grid + 4 armor + offhand as "container" slots, then the standard
/// 27+9 region `inventory_start = 9` describes.
static PLAYER_INVENTORY_SLOTS: [SlotDef; 10] = [
    SlotDef::new(0, 144, 36, SlotKind::Output),
    SlotDef::new(1, 98, 18, SlotKind::Input),
    SlotDef::new(2, 116, 18, SlotKind::Input),
    SlotDef::new(3, 98, 36, SlotKind::Input),
    SlotDef::new(4, 116, 36, SlotKind::Input),
    SlotDef::new(5, 8, 8, SlotKind::Armor),
    SlotDef::new(6, 8, 26, SlotKind::Armor),
    SlotDef::new(7, 8, 44, SlotKind::Armor),
    SlotDef::new(8, 8, 62, SlotKind::Armor),
    SlotDef::new(45, 77, 62, SlotKind::Offhand),
];

/// The `E`-key screen's schema: vanilla's own 46-slot player inventory,
/// rendered by the same [`ContainerScreenState`] every server menu uses.
pub static PLAYER_INVENTORY_SCHEMA: MenuScreenSchema = MenuScreenSchema {
    menu_id: PLAYER_INVENTORY_MENU_ID,
    name: "minecraft:inventory",
    panel_w: 176.0,
    panel_h: 166.0,
    container_slots: &PLAYER_INVENTORY_SLOTS,
    has_player_inventory: true,
    inventory_start: 9,
    slot_count: 46,
    inventory_origin_x: 8,
    inventory_origin_y: 84,
    buttons: &[],
    property_bars: &[],
};

/// Which JEI-style focus the R/U/W hotkeys request on the hovered stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeFocus {
    /// `R` — "recipes producing this item" detail view.
    Recipes,
    /// `U` — "uses consuming this item" detail view.
    Uses,
    /// `W` — Create-Ponder-style 3D scene for the item.
    Ponder,
}

/// Display-independent output of the Minecraft keyboard bindings.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowInputEvent {
    DebugChord(DebugChordAction),
    SetDebugOverlayVisible(bool),
    SelectHotbar(u8),
    SwapOffhand,
    DropItem { entire_stack: bool },
    OpenInventory,
    OpenChat { slash_prefill: bool },
    SetTabListVisible(bool),
    SortInventory,
    OpenAdvancements,
    /// JEI-style hover hotkey resolved from `R`/`U`/`W` while a container
    /// surface is up — opens the recipe browser focused on the hovered
    /// stack (no-op when nothing resolvable is under the cursor).
    RecipeBrowserFocus(RecipeFocus),
}

/// Pure input router shared by the window loop and display-free tests.
#[derive(Clone, Debug, Default)]
pub struct WindowInputRouter {
    pub debug_chords: DebugChordState,
    pub f3_held: bool,
    pub f3_chord_consumed: bool,
    pub active_hotbar_slot: u8,
    pub tab_list_held: bool,
}

impl WindowInputRouter {
    /// Process a physical key transition. `ctrl` is the current modifier state.
    pub fn route_key(
        &mut self,
        key: KeyCode,
        state: ElementState,
        ctrl: bool,
    ) -> Vec<WindowInputEvent> {
        self.route_key_with_context(key, state, ctrl, None, None, false)
    }

    /// Supply authoritative player/target data when available for F3+C/I/N/F4.
    pub fn route_key_with_context(
        &mut self,
        key: KeyCode,
        state: ElementState,
        ctrl: bool,
        player: Option<PlayerDebugInfo<'_>>,
        target: Option<DebugTarget<'_>>,
        has_permission_level_2: bool,
    ) -> Vec<WindowInputEvent> {
        let pressed = state == ElementState::Pressed;
        if key == KeyCode::F3 {
            if pressed {
                if !self.f3_held {
                    self.f3_chord_consumed = false;
                }
                self.f3_held = true;
                return Vec::new();
            }
            if !self.f3_held {
                return Vec::new();
            }
            self.f3_held = false;
            let consumed = std::mem::take(&mut self.f3_chord_consumed);
            let mut events = Vec::new();
            if let Some(mode) = player.map(|info| info.gamemode) {
                if let Some(action) = finish_gamemode_switcher(&mut self.debug_chords, mode) {
                    events.push(WindowInputEvent::DebugChord(action));
                }
            } else {
                self.debug_chords.gamemode_switcher = None;
            }
            if !consumed {
                self.debug_chords.toggle_debug_overlay();
                events.push(WindowInputEvent::SetDebugOverlayVisible(
                    self.debug_chords.show_debug_overlay,
                ));
            }
            return events;
        }

        if self.f3_held && pressed {
            if let Some(chord) = f3_chord_for_key(key) {
                self.f3_chord_consumed = true;
                let mut context = DebugChordContext::new(&mut self.debug_chords);
                context.player = player;
                context.target = target;
                context.has_permission_level_2 = has_permission_level_2;
                let action = handle_f3_chord(chord, &mut context);
                return vec![WindowInputEvent::DebugChord(action)];
            }
        }

        if key == KeyCode::Tab {
            if self.tab_list_held != pressed {
                self.tab_list_held = pressed;
                return vec![WindowInputEvent::SetTabListVisible(pressed)];
            }
            return Vec::new();
        }
        if !pressed {
            return Vec::new();
        }

        let event = match key {
            KeyCode::Digit1 => WindowInputEvent::SelectHotbar(0),
            KeyCode::Digit2 => WindowInputEvent::SelectHotbar(1),
            KeyCode::Digit3 => WindowInputEvent::SelectHotbar(2),
            KeyCode::Digit4 => WindowInputEvent::SelectHotbar(3),
            KeyCode::Digit5 => WindowInputEvent::SelectHotbar(4),
            KeyCode::Digit6 => WindowInputEvent::SelectHotbar(5),
            KeyCode::Digit7 => WindowInputEvent::SelectHotbar(6),
            KeyCode::Digit8 => WindowInputEvent::SelectHotbar(7),
            KeyCode::Digit9 => WindowInputEvent::SelectHotbar(8),
            KeyCode::KeyF => WindowInputEvent::SwapOffhand,
            KeyCode::KeyQ => WindowInputEvent::DropItem { entire_stack: ctrl },
            KeyCode::KeyE => WindowInputEvent::OpenInventory,
            KeyCode::KeyT => WindowInputEvent::OpenChat {
                slash_prefill: false,
            },
            KeyCode::Slash => WindowInputEvent::OpenChat {
                slash_prefill: true,
            },
            KeyCode::KeyR => WindowInputEvent::SortInventory,
            KeyCode::KeyL => WindowInputEvent::OpenAdvancements,
            _ => return Vec::new(),
        };
        if let WindowInputEvent::SelectHotbar(slot) = event {
            self.active_hotbar_slot = slot;
        }
        vec![event]
    }

    /// A lost focus must not leave F3 or Tab logically held.
    pub fn release_held_keys(&mut self) -> Option<WindowInputEvent> {
        self.f3_held = false;
        self.f3_chord_consumed = false;
        self.debug_chords.gamemode_switcher = None;
        let tab_was_held = std::mem::take(&mut self.tab_list_held);
        tab_was_held.then_some(WindowInputEvent::SetTabListVisible(false))
    }
}

fn f3_chord_for_key(key: KeyCode) -> Option<F3Chord> {
    Some(match key {
        KeyCode::KeyA => F3Chord::A,
        KeyCode::KeyB => F3Chord::B,
        KeyCode::KeyC => F3Chord::C,
        KeyCode::KeyD => F3Chord::D,
        KeyCode::F4 => F3Chord::F4,
        KeyCode::KeyG => F3Chord::G,
        KeyCode::KeyH => F3Chord::H,
        KeyCode::KeyI => F3Chord::I,
        KeyCode::KeyN => F3Chord::N,
        KeyCode::KeyP => F3Chord::P,
        KeyCode::KeyQ => F3Chord::Q,
        KeyCode::KeyT => F3Chord::T,
        KeyCode::Digit1 => F3Chord::Key1,
        KeyCode::Digit2 => F3Chord::Key2,
        KeyCode::Digit3 => F3Chord::Key3,
        _ => return None,
    })
}

/// The three Minecraft camera views in F5 cycling order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CameraMode {
    #[default]
    FirstPerson,
    ThirdPersonBack,
    ThirdPersonFront,
}

/// Window-independent input and projection state. The player's camera stays
/// at the eye position for movement/network updates in every view mode.
#[derive(Clone, Debug)]
pub struct CameraController {
    mode: CameraMode,
    zoom: ZoomState,
    base_fov_deg: f32,
    base_sensitivity: f32,
    sprint_blend: f32,
}

impl Default for CameraController {
    fn default() -> Self {
        Self::new(70.0, 1.0, 4.0)
    }
}

impl CameraController {
    /// `sensitivity` multiplies the shared camera mouse-look speed.
    pub fn new(fov_deg: f32, sensitivity: f32, zoom_divisor: f32) -> Self {
        let mut controller = Self {
            mode: CameraMode::FirstPerson,
            // Keep ZoomState's base constant so changing FOV mid-zoom does
            // not reset the current interpolated zoom factor.
            zoom: ZoomState::with_zoom_divisor(1.0, zoom_divisor),
            base_fov_deg: 70.0,
            base_sensitivity: 1.0,
            sprint_blend: 0.0,
        };
        controller.apply_settings(fov_deg, sensitivity, zoom_divisor);
        controller
    }

    pub fn mode(&self) -> CameraMode {
        self.mode
    }

    pub fn cycle_mode(&mut self) -> CameraMode {
        self.mode = match self.mode {
            CameraMode::FirstPerson => CameraMode::ThirdPersonBack,
            CameraMode::ThirdPersonBack => CameraMode::ThirdPersonFront,
            CameraMode::ThirdPersonFront => CameraMode::FirstPerson,
        };
        self.mode
    }

    /// Apply live settings while retaining the current zoom transition.
    pub fn apply_settings(&mut self, fov_deg: f32, sensitivity: f32, zoom_divisor: f32) {
        self.base_fov_deg = finite_clamp(fov_deg, 70.0, 10.0, 120.0);
        self.base_sensitivity = finite_clamp(sensitivity, 1.0, 0.01, 10.0);
        self.zoom.set_zoom_divisor(zoom_divisor);
    }

    pub fn base_fov_deg(&self) -> f32 {
        self.base_fov_deg
    }

    pub fn base_sensitivity(&self) -> f32 {
        self.base_sensitivity
    }

    pub fn zoom_divisor(&self) -> f32 {
        self.zoom.zoom_divisor()
    }

    pub fn set_zoomed(&mut self, held: bool) {
        self.zoom.set_zoomed(held);
    }

    /// Disabling smoothing makes zoom snap at the next frame update.
    pub fn set_smooth_zoom(&mut self, enabled: bool) {
        self.zoom.set_smoothing_rate(if enabled {
            DEFAULT_ZOOM_SMOOTHING_RATE
        } else {
            10_000.0
        });
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoom.is_zoomed()
    }

    /// Advance smooth zoom and the 10% sprint FOV transition using frame dt.
    pub fn tick(&mut self, dt: f32, sprinting: bool) -> f32 {
        self.zoom.tick(dt);
        let dt = finite_clamp(dt, 0.0, 0.0, 0.25);
        let target = if sprinting { 1.0 } else { 0.0 };
        let blend = 1.0 - (-SPRINT_FOV_SMOOTHING_RATE * dt).exp();
        self.sprint_blend += (target - self.sprint_blend) * blend;
        self.effective_fov_deg()
    }

    /// Effective vertical projection FOV, always finite and within bounds.
    pub fn effective_fov_deg(&self) -> f32 {
        finite_clamp(
            self.base_fov_deg / self.zoom.zoom_factor()
                * (1.0 + SPRINT_FOV_BOOST * self.sprint_blend),
            self.base_fov_deg,
            10.0,
            120.0,
        )
    }

    pub fn effective_sensitivity(&self) -> f32 {
        self.zoom.scaled_sensitivity(self.base_sensitivity)
    }

    /// Apply raw device mouse motion to the player's orientation.
    pub fn apply_mouse_look(&self, camera: &mut Camera, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        camera.apply_mouse_look(
            dx * f64::from(self.effective_sensitivity()),
            dy * f64::from(self.effective_sensitivity()),
        );
    }

    /// Derived render camera. Does not change player movement or network pose.
    pub fn view_camera(&self, player: &Camera) -> Camera {
        let mut camera = *player;
        let forward = player.forward_vector();
        let distance = match self.mode {
            CameraMode::FirstPerson => 0.0,
            CameraMode::ThirdPersonBack => -THIRD_PERSON_DISTANCE,
            CameraMode::ThirdPersonFront => THIRD_PERSON_DISTANCE,
        };
        for (eye, direction) in camera.position.iter_mut().zip(forward) {
            *eye += direction * distance;
        }
        if self.mode == CameraMode::ThirdPersonFront {
            camera.yaw_deg = (camera.yaw_deg + 180.0).rem_euclid(360.0);
            camera.pitch_deg = -camera.pitch_deg;
        }
        camera.fov_deg = self.effective_fov_deg();
        camera
    }
}

fn finite_clamp(value: f32, fallback: f32, low: f32, high: f32) -> f32 {
    if value.is_finite() {
        value.clamp(low, high)
    } else {
        fallback
    }
}

/// Dirty-only gate for the software-rasterized UI framebuffer: remembers
/// the `(width, height, fingerprint)` of the last uploaded frame so
/// `render_frame` skips `rasterize_draw_list` + the PCIe texture upload on
/// frames where the HUD did not change (the common case at 240+ FPS). The
/// counters let tests prove zero redundant uploads.
#[derive(Default)]
pub struct UiUploadCache {
    last_upload: Option<(u32, u32, u64)>,
    /// Frames that rasterized and uploaded (first frame, HUD change, resize).
    pub ui_uploads_performed: u64,
    /// Frames skipped because `(w, h, fingerprint)` matched the last upload.
    pub ui_uploads_skipped: u64,
}

impl UiUploadCache {
    /// `true` when this `(width, height, fingerprint)` differs from the
    /// last upload (or none has happened yet); records the key and counts
    /// the decision either way.
    pub fn should_upload(&mut self, width: u32, height: u32, fingerprint: u64) -> bool {
        let key = (width, height, fingerprint);
        if self.last_upload == Some(key) {
            self.ui_uploads_skipped += 1;
            return false;
        }
        self.last_upload = Some(key);
        self.ui_uploads_performed += 1;
        true
    }
}

/// GPU-side state that exists only once the window + surface are up.
struct ActiveWindow {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    renderer: GpuRenderer,
    surface_format: wgpu::TextureFormat,
    size: PhysicalSize<u32>,
}

/// Key set for FPS movement.
#[derive(Default)]
struct Keys {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    sprint: bool,
}

impl Keys {
    fn move_input(&self) -> MoveInput {
        let axis = |pos: bool, neg: bool| match (pos, neg) {
            (true, false) => 1.0,
            (false, true) => -1.0,
            _ => 0.0,
        };
        MoveInput {
            forward: axis(self.forward, self.back),
            strafe: axis(self.right, self.left),
            vertical: axis(self.up, self.down),
            sprint: self.sprint,
        }
    }
}

/// The `ApplicationHandler` `EventLoop::run_app` drives.
pub struct GpuApp {
    config: GpuCliConfig,
    live: Option<LiveSession>,
    active: Option<ActiveWindow>,
    camera: Camera,
    camera_controller: CameraController,
    keys: Keys,
    input_router: WindowInputRouter,
    debug_messages: Vec<String>,
    focused: bool,
    occluded: bool,
    scale_factor: f64,
    cursor_grabbed: bool,
    paused: bool,
    show_diagnostics: bool,
    mouse_pos: (i32, i32),
    left_mouse_held: bool,
    container_screen: Option<ContainerScreenState>,
    /// `container_screen` currently holds the client-side window-0 player
    /// inventory opened by `E` (not a server `open_screen`).
    own_inventory_open: bool,
    /// `L`-key advancements overlay; opening announces the tab via
    /// `seen_advancements`, closing retracts it.
    advancements_open: bool,
    chat_bar: Option<ZshCommandBar>,
    /// Latest `complete_suggestions` hit for the open chat bar's `/`-input.
    chat_suggestions: Vec<String>,
    /// QoL container sorter armed by the `SortInventory` keybind.
    inventory_sort: InventorySortState,
    /// Launcher filesystem paths handed to every `LiveSession` this app
    /// spawns (`--gameDir`/`--assetsDir`/`--assetIndex`/`--version`/
    /// `--resourcePackDir`).
    session_paths: SessionPaths,
    /// Boot-time recipe-browser ingest report (mod JARs + vanilla client
    /// JAR + datapacks); empty when no `--gameDir` was supplied.
    recipe_ingest: crate::recipe_ingest::IngestReport,
    /// Clean-room runtime `MEGU EDITION` title branding (wordmark +
    /// patched edition strip, merged splash pool) loaded once at boot —
    /// the built-in splash set with no banner when no local client JAR
    /// is discoverable.
    title_branding: TitleBrandingAssets,
    /// Session-deterministic splash pick seed (FNV-1a over the launch
    /// config — never `SystemTime`).
    splash_seed: u64,
    /// Integrated `megu` server guard while a singleplayer world runs.
    singleplayer_server: Option<integrated::SingleplayerServer>,
    /// Friend-relay host session while an invite code is live.
    friend_host: Option<FriendHostSession>,
    /// Friend-relay guest bridge while joined through an invite code.
    friend_bridge: Option<FriendGuestBridge>,
    /// Last live `title_revision` applied to `ui.hud.title_overlay`.
    title_seen_rev: u64,
    /// Fractional-tick accumulator driving `TitleOverlayState::tick` at 20/s.
    title_tick_accum: f32,
    ui: UiState,
    ui_cache: UiUploadCache,
    render_config: RenderConfig,
    policy: DisplayPresentationPolicy,
    governor: FrameBudgetGovernor,
    /// User-requested shadow quality / render scale, restored when the
    /// governor steps back to `ThrottleStage::Full`.
    user_shadow_quality: BuiltinShadowQuality,
    user_render_scale: RenderScalePreset,
    /// Renderer upload budget captured at init; `TrimRebuilds` swaps in a
    /// 1-section / 32 KiB cap and `Full` restores this baseline.
    baseline_upload_budget: Option<ChunkUploadBudget>,
    /// Latest F3 diagnostic string and when it was built — refreshed at
    /// most every `F3_REFRESH` so steady frames never re-format it.
    f3_text: Option<(String, Instant)>,
    /// Mouse deltas accumulated while the loop slept, applied once
    /// immediately before `render_frame` derives the view camera — the
    /// late-latch: input sampled as close to the draw as possible.
    pending_mouse_delta: (f64, f64),
    last_frame: Instant,
    frame_start: Instant,
    fatal: Option<String>,
}

impl GpuApp {
    /// Build the app state (no GPU or window yet — `resumed` does that).
    pub fn new(config: GpuCliConfig) -> Self {
        let session_paths = SessionPaths {
            game_dir: config.game_dir.clone(),
            assets_dir: config.assets_dir.clone(),
            asset_index: config.asset_index.clone(),
            version_id: config.version_id.clone(),
            resource_pack_dir: config.resource_pack_dir.clone(),
        };
        // `--quickPlaySingleplayer`: launch the integrated server first so
        // the session can bind its loopback endpoint. Failures surface as a
        // title-screen status line, never a silent no-op.
        let mut singleplayer_server = None;
        let mut startup_status = Vec::new();
        if !config.offline_demo {
            if let Some(world) = &config.quick_play_singleplayer {
                match Self::launch_singleplayer(&config, world) {
                    Ok(server) => singleplayer_server = Some(server),
                    Err(err) => {
                        startup_status.push(format!("Singleplayer {world:?}: {err}"));
                    }
                }
            }
        }
        let live = if config.offline_demo {
            None
        } else if let Some(server) = &singleplayer_server {
            let addr = server.addr();
            Some(LiveSession::connect_with_paths_and_auth(
                addr.ip().to_string(),
                addr.port(),
                config.username.clone(),
                config.mc_version.clone(),
                session_paths.clone(),
                config.access_token.clone(),
                config.uuid.clone(),
            ))
        } else {
            config.connect.as_ref().map(|(host, port)| {
                LiveSession::connect_with_paths_and_auth(
                    host.clone(),
                    *port,
                    config.username.clone(),
                    config.mc_version.clone(),
                    session_paths.clone(),
                    config.access_token.clone(),
                    config.uuid.clone(),
                )
            })
        };
        let render_config = RenderConfig {
            vsync: config.vsync,
            gpu_backend: config.gpu_backend,
            shader_pack: config.shader_pack.as_ref().map(|p| p.display().to_string()),
            ..RenderConfig::default()
        };
        let policy = select_display_presentation_policy(
            detect_display_server_kind(),
            render_config.gpu_backend,
        );
        let governor = FrameBudgetGovernor::new(render_config.max_fps);
        let camera = Camera {
            position: [8.0, 18.0, 26.0],
            yaw_deg: 180.0,
            pitch_deg: -18.0,
            ..Camera::default()
        };
        let splash_seed = splash_seed_for(&config);
        let mut ui = UiState::new();
        apply_megu_default_links(&mut ui);
        ui.settings.video.vsync = render_config.vsync;
        // Straight into gameplay when a world/server is already targeted;
        // the title screen otherwise — like a vanilla launch with no
        // quick-play argument.
        ui.screen = if live.is_some() || config.offline_demo {
            Screen::InGameHud
        } else {
            Screen::Title
        };
        if ui.screen == Screen::InGameHud {
            ui.hud = megu_ui::state::HudState::demo();
        }
        let mut app = Self {
            config,
            live,
            active: None,
            camera,
            camera_controller: CameraController::new(render_config.fov_deg as f32, 1.0, 4.0),
            keys: Keys::default(),
            input_router: WindowInputRouter::default(),
            debug_messages: Vec::new(),
            focused: true,
            occluded: false,
            scale_factor: 1.0,
            cursor_grabbed: false,
            paused: false,
            show_diagnostics: false,
            mouse_pos: (0, 0),
            left_mouse_held: false,
            container_screen: None,
            own_inventory_open: false,
            advancements_open: false,
            chat_bar: None,
            chat_suggestions: Vec::new(),
            inventory_sort: InventorySortState::default(),
            session_paths,
            recipe_ingest: crate::recipe_ingest::IngestReport::default(),
            title_branding: TitleBrandingAssets::fallback(),
            splash_seed,
            singleplayer_server,
            friend_host: None,
            friend_bridge: None,
            title_seen_rev: 0,
            title_tick_accum: 0.0,
            ui,
            ui_cache: UiUploadCache::default(),
            render_config,
            policy,
            governor,
            user_shadow_quality: BuiltinShadowQuality::Off,
            user_render_scale: RenderScalePreset::Native100,
            baseline_upload_budget: None,
            f3_text: None,
            pending_mouse_delta: (0.0, 0.0),
            last_frame: Instant::now(),
            frame_start: Instant::now(),
            fatal: None,
        };
        for message in startup_status {
            app.status_line(message);
        }
        // Lane GPU-JEI-PONDER-WIRE: feed `ui.recipe_browser.index` real
        // datapack content — `<gameDir>/mods/*.jar`, the vanilla version
        // JAR, and datapack dirs — all through the lazy ZIP index with
        // zero extraction. The report rides `status_line` exactly like the
        // texture/sound discovery summaries: a loaded count when sources
        // exist, silence when no game dir was ever given.
        app.recipe_ingest = crate::recipe_ingest::ingest_mod_recipes_into_ui(
            &app.session_paths,
            &mut app.ui.recipe_browser,
        );
        if app.recipe_ingest.any_source() {
            let summary = app.recipe_ingest.summary_line();
            app.status_line(summary);
        }
        // Runtime `MEGU EDITION` title branding: when the launcher paths
        // resolve a local client JAR, its `minecraft.png`/`edition.png`/
        // `splashes.txt` are read in memory through the lazy ZIP index —
        // the edition strip patched to `MEGU` — and mod JARs contribute
        // their own splash pools. Zero temp files, graceful fallback.
        let (jar, mod_jars) = title_branding_sources(&app.session_paths);
        app.title_branding = match jar {
            Some(jar) => crate::title_branding::load_title_branding_from_jar(&jar, &mod_jars),
            None => TitleBrandingAssets::fallback(),
        };
        app
    }

    /// The fatal setup error, if `resumed` hit one (`run` reports it).
    pub fn fatal(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    /// The top-level UI screen (Title, Pause, InGameHud, ...).
    pub fn screen(&self) -> Screen {
        self.ui.screen
    }

    /// The active live session, when one was spawned.
    pub fn live(&self) -> Option<&LiveSession> {
        self.live.as_ref()
    }

    /// `true` while `container_screen` is the window-0 player inventory.
    pub fn own_inventory_open(&self) -> bool {
        self.own_inventory_open
    }

    /// `true` while the `L`-key advancements overlay is up.
    pub fn advancements_open(&self) -> bool {
        self.advancements_open
    }

    /// Slot count of the open container surface (own inventory or server
    /// screen), `None` when no surface is up.
    pub fn container_slot_count(&self) -> Option<usize> {
        self.container_screen
            .as_ref()
            .map(ContainerScreenState::slot_count)
    }

    /// One slot of the open container surface.
    pub fn container_slot(&self, index: usize) -> Option<UiItemStack> {
        self.container_screen.as_ref()?.slot(index).cloned()
    }

    /// Status lines surfaced by dispatched actions (join failures, invite
    /// results, statistics summaries) — never silently dropped.
    pub fn status_messages(&self) -> &[String] {
        &self.debug_messages
    }

    /// The recipe/resource browser state (JEI/EMI/JER/Ponder index, tabs,
    /// focused detail) — read-only view for host code and tests.
    pub fn recipe_browser(&self) -> &RecipeBrowserState {
        &self.ui.recipe_browser
    }

    /// Runtime `MEGU EDITION` title branding assets loaded at boot —
    /// [`TitleBrandingAssets::fallback`] state (built-in splashes, no
    /// banner) when no local client JAR was discoverable.
    pub fn title_branding(&self) -> &TitleBrandingAssets {
        &self.title_branding
    }

    /// The session-deterministic title splash line from the merged
    /// pool (local JAR + mod JARs + built-in Megu set).
    pub fn active_splash(&self) -> &str {
        self.title_branding.select_splash(self.splash_seed)
    }

    /// Boot-time mod/vanilla/datapack ingest report (see
    /// [`crate::recipe_ingest`]); `None` counters when no `--gameDir`.
    pub fn recipe_ingest_report(&self) -> &crate::recipe_ingest::IngestReport {
        &self.recipe_ingest
    }

    /// Set the hovered container slot — the host-side mirror of
    /// [`ContainerScreenState::hover`] (cursor hover for mouse, and the
    /// path gamepad-style focus movement uses).
    pub fn set_container_hover(&mut self, slot: Option<usize>) {
        if let Some(screen) = &mut self.container_screen {
            screen.hover(slot);
        }
    }

    /// Inject a session without a real socket — tests use the
    /// `offline_with_*` fixtures to drive the same window-0/advancements
    /// mirrors the network path fills.
    pub fn attach_live(&mut self, live: LiveSession) {
        self.live = Some(live);
    }

    /// Drive one [`WindowInputEvent`] through the same handler `handle_key`
    /// uses — the display-free way tests exercise the keybind path.
    pub fn dispatch_input(&mut self, event: WindowInputEvent) {
        self.apply_window_input(event);
    }

    /// Apply a [`UiAction`] that needs no event loop. `QuitGame` is handled
    /// inside `apply_ui_action` instead — it owns `event_loop.exit()`.
    pub fn dispatch_ui_action(&mut self, action: &UiAction) {
        match action {
            UiAction::OpenScreen(screen) => self.ui.navigate(*screen),
            UiAction::Back => {
                self.ui
                    .handle_input(UiInputEvent::KeyPressed(UiKey::Escape));
            }
            UiAction::SettingsChanged => {
                let settings = self.ui.settings.clone();
                self.apply_settings(&settings);
            }
            UiAction::StartSingleplayer => {
                if let Err(err) = self.start_singleplayer() {
                    self.status_line(format!("Singleplayer: {err}"));
                }
            }
            UiAction::StartMultiplayer => {
                if let Err(err) = self.join_multiplayer() {
                    self.status_line(format!("Multiplayer: {err}"));
                }
            }
            UiAction::JoinWithCode(code) => {
                if let Err(err) = self.join_with_code(*code) {
                    self.status_line(format!("Join with code: {err}"));
                }
            }
            UiAction::GenerateInvite => match self.generate_invite() {
                Ok(()) => {
                    self.status_line(format!("Invite ready: {}", self.ui.invite_display()));
                }
                Err(err) => self.status_line(format!("Invite: {err}")),
            },
            UiAction::CopyInvite(code) => {
                // No clipboard backend is linked into this binary; the code
                // stays visible on the Friends screen and the status line
                // reports the dispatch instead of dropping it.
                self.status_line(format!("Invite code {code} — share it with a friend"));
            }
            UiAction::RevokeInvite => {
                self.ui.active_invite = None;
                match self.friend_host.as_mut().map(|host| host.revoke()) {
                    None => self.status_line("Invite revoked (no live relay session)"),
                    Some(Ok(())) => self.status_line("Invite revoked"),
                    Some(Err(err)) => self.status_line(format!("Revoke invite: {err}")),
                }
                self.friend_host = None;
            }
            UiAction::ShowStatistics => {
                let stats = self.live.as_ref().map_or_else(
                    || "Statistics: no live session".to_owned(),
                    |live| {
                        format!(
                            "Statistics: {} chunks, {} subworlds, {} advancements",
                            live.world.loaded_chunks(),
                            live.subworlds.len(),
                            live.advancements.len(),
                        )
                    },
                );
                self.status_line(stats);
            }
            UiAction::OpenShaderOptions => {
                self.ui.navigate(Screen::Settings(SettingsTab::Shaders));
            }
            UiAction::OpenUrl(url) => {
                if let Err(err) = open_url(url) {
                    self.status_line(format!("Open link: {err}"));
                }
            }
            UiAction::SubmitCrashReport => self.handle_submit_crash_report(),
            UiAction::RecipeBrowser(action) => self.dispatch_recipe_browser_action(action),
            _ => {}
        }
    }

    /// Persist a redacted local crash report on explicit user confirmation from
    /// the CrashRecovery screen, strictly honoring `settings.qol.crash_report_mode`.
    fn handle_submit_crash_report(&mut self) {
        if self.ui.settings.qol.crash_report_mode == megu_ui::state::CrashReportMode::Off {
            self.status_line("Crash reporting is off (nothing saved or sent)");
            return;
        }
        let (kind_label, summary) = self
            .ui
            .crash
            .as_ref()
            .map(|c| (c.error_kind.clone(), c.detail_message.clone()))
            .unwrap_or_else(|| ("crash_recovery".to_string(), "client crash".to_string()));
        let crash_dir = self
            .config
            .game_dir
            .as_deref()
            .map(|dir| dir.join("crash-reports"))
            .unwrap_or_else(|| std::path::PathBuf::from("crash-reports"));
        let mut env = megu_client::crash_reporter::ReportEnv::detect();
        env.mc_version = self.config.mc_version.clone();
        env.engine_version = format!("{} {}", crate::CLIENT_GPU_NAME, env!("CARGO_PKG_VERSION"));
        env.subsystem = Some("client-gpu".to_string());
        let report = megu_client::crash_reporter::capture_panic_context(
            &env,
            megu_client::crash_reporter::ReportKind::Crash,
            &kind_label,
            &summary,
            None,
        );
        match megu_client::crash_reporter::write_local_report(&crash_dir, &report) {
            Ok(path) => self.status_line(format!(
                "Redacted crash report saved: {} (fp {})",
                path.display(),
                report.fingerprint
            )),
            Err(err) => self.status_line(format!("Crash report save failed: {err}")),
        }
    }

    /// The recipe browser's only host-facing actions today are the JEI `+`
    /// transfer and `Close` (bookmark/toggle mutations are state-internal,
    /// and Ponder opens through the `W` hotkey or the browser's own `P`
    /// key — both feed `browser.ponder`, no host action is emitted).
    /// `TransferToCraftingGrid` sends the serverbound `place_recipe`
    /// packet on a live session or surfaces an explicit status when
    /// offline — it is never dropped on the floor.
    fn dispatch_recipe_browser_action(&mut self, action: &RecipeBrowserAction) {
        match action {
            RecipeBrowserAction::TransferToCraftingGrid {
                recipe_id,
                max_craft,
            } => {
                let window_id = self
                    .container_screen
                    .as_ref()
                    .map(|screen| screen.window_id)
                    .or_else(|| {
                        self.live.as_ref().and_then(|live| {
                            live.open_screen
                                .as_ref()
                                .map(|s| s.window_id.clamp(0, 255) as u8)
                        })
                    })
                    .unwrap_or(0);
                // `make_all` is vanilla's shift-click fill-all flag: request
                // it whenever the inventory could run the recipe more than
                // once, so `+` fills the grid like a shift-click.
                let make_all = *max_craft > 1;
                match &mut self.live {
                    Some(live) => match live.status {
                        SessionStatus::Playing => {
                            if live.send_place_recipe(window_id, recipe_id, make_all) {
                                self.status_line(format!("Placing {recipe_id} in the grid"));
                            } else {
                                self.status_line(format!(
                                    "Place recipe {recipe_id}: send queue busy"
                                ));
                            }
                        }
                        SessionStatus::Connecting => {
                            self.status_line(format!("Place recipe {recipe_id}: still connecting"))
                        }
                        SessionStatus::Disconnected(_) => self
                            .status_line(format!("Place recipe {recipe_id}: not connected")),
                    },
                    None => self
                        .status_line(format!("Place recipe {recipe_id}: not connected")),
                }
            }
            RecipeBrowserAction::Close => {
                self.ui
                    .handle_input(UiInputEvent::KeyPressed(UiKey::Escape));
            }
        }
    }

    pub fn camera_controller(&self) -> &CameraController {
        &self.camera_controller
    }

    pub fn render_config(&self) -> &RenderConfig {
        &self.render_config
    }

    /// Camera values can also be updated by a settings source with FOV,
    /// sensitivity and zoom-divisor controls.
    pub fn apply_camera_settings(&mut self, fov_deg: f32, sensitivity: f32, zoom_divisor: f32) {
        self.camera_controller
            .apply_settings(fov_deg, sensitivity, zoom_divisor);
        self.render_config.fov_deg = f64::from(self.camera_controller.base_fov_deg());
    }

    /// Apply settings immediately, including the active swapchain and GPU.
    /// `UiSettings` currently offers video/visual presets and a zoom toggle;
    /// camera numerical values are accepted through `apply_camera_settings`.
    pub fn apply_settings(&mut self, settings: &UiSettings) {
        let video = &settings.video;
        self.camera_controller
            .set_smooth_zoom(settings.qol.smooth_zoom);
        let vsync_changed = self.render_config.vsync != video.vsync;
        let fps_cap = (video.fps_cap > 0).then_some(video.fps_cap);
        if self.render_config.max_fps != fps_cap {
            self.governor = FrameBudgetGovernor::new(fps_cap);
        }
        self.render_config.max_fps = fps_cap;
        self.render_config.background_fps_limit = video.background_fps;
        self.render_config.vsync = video.vsync;
        self.render_config.smooth_lighting = video.smooth_lighting;
        self.render_config.view_distance = Some(i32::from(video.view_distance));

        if let Some(active) = &mut self.active {
            let shadow_quality = match settings.visual.preset {
                VisualStylePreset::Vanilla | VisualStylePreset::MeguNaturalLight => {
                    BuiltinShadowQuality::Off
                }
                VisualStylePreset::MeguNatural => BuiltinShadowQuality::Natural,
            };
            self.user_shadow_quality = shadow_quality;
            active.renderer.set_shadow_quality(shadow_quality);
            let scale = match video.preset {
                VideoPreset::Fast => RenderScalePreset::Performance67,
                VideoPreset::Balanced | VideoPreset::High | VideoPreset::Ultra => {
                    RenderScalePreset::Native100
                }
                VideoPreset::Custom => active.renderer.render_scale,
            };
            self.user_render_scale = scale;
            active.renderer.set_render_scale(scale);
        }
        if vsync_changed {
            if let Some(size) = self.active.as_ref().map(|active| active.size) {
                self.reconfigure(size);
            }
        }

        // Live-session surfaces (QUAL-4): the 3D blocks/skins and nature
        // packs, the tactile weather/touch sim, and the spatial-audio
        // mixer preset all take the same toggles the settings screen
        // exposes.
        if let Some(live) = &mut self.live {
            live.vanilla_plus_cfg.three_d.enabled = settings.visual.blocks_and_skins_3d;
            live.vanilla_plus_cfg.nature.enabled = settings.visual.world_effects;
            let detail = &settings.visual.world_effects_detail;
            let tactile = live.tactile_config().clone();
            live.set_tactile_config(TactileEffectsConfig {
                preset: if settings.visual.world_effects {
                    TactilePreset::Fancy
                } else {
                    TactilePreset::Off
                },
                water_and_lava_reactions: detail.water_splash || detail.ripples,
                tree_shake_and_footsteps: detail.cherry_leaves || detail.autumn_leaves,
                cold_breath_and_embers: detail.cold_breath,
                shooting_stars_and_aurora: detail.night_sky,
                subtle_camera_motion: settings.visual.camera_micro_motion,
                ..tactile
            });
            let preset = if settings.visual.spatial_audio_3d {
                SpatialAudioPreset::Realistic
            } else {
                SpatialAudioPreset::Off
            };
            live.set_spatial_audio_preset(preset);
        }
    }

    fn set_cursor_grab(&mut self, grab: bool) {
        let Some(active) = &self.active else {
            return;
        };
        if grab && !self.cursor_grabbed {
            // The display policy picks the grab order: `Locked` first on
            // Wayland (`zwp_locked_pointer_v1` + `zwp_relative_pointer_v1`
            // unaccelerated raw deltas, so the invisible cursor can never
            // hit the window borders), macOS and Windows; `Confined` first
            // on X11, where XI2 `DeviceEvent::MouseMotion` already delivers
            // unaccelerated raw deltas and `Locked` hits X11/RDP cursor
            // lock quirks. The other mode is always the fallback.
            let (first, second) = if self.policy.prefer_locked_pointer_first {
                (CursorGrabMode::Locked, CursorGrabMode::Confined)
            } else {
                (CursorGrabMode::Confined, CursorGrabMode::Locked)
            };
            active
                .window
                .set_cursor_grab(first)
                .or_else(|_| active.window.set_cursor_grab(second))
                .ok();
            active.window.set_cursor_visible(false);
            self.cursor_grabbed = true;
        } else if !grab && self.cursor_grabbed {
            active.window.set_cursor_grab(CursorGrabMode::None).ok();
            active.window.set_cursor_visible(true);
            self.cursor_grabbed = false;
        }
    }

    /// True when the pointer should be captured for mouselook: in-game,
    /// unpaused, no chat bar, no container/inventory screen, no
    /// advancements overlay.
    fn wants_cursor_grab(&self) -> bool {
        self.ui.screen == Screen::InGameHud
            && !self.paused
            && self.focused
            && self.chat_bar.is_none()
            && self.container_screen.is_none()
            && !self.advancements_open
    }

    /// Re-apply the grab decision after an overlay/screen transition.
    fn maybe_regrab(&mut self) {
        let grab = self.wants_cursor_grab();
        self.set_cursor_grab(grab);
    }

    /// True when key/mouse input belongs to the UI layer (pause menus and
    /// every non-HUD screen) rather than gameplay movement.
    fn ui_wants_input(&self) -> bool {
        self.paused || self.ui.screen != Screen::InGameHud
    }

    /// A user-visible status line for actions that can't silently no-op:
    /// the message lands in chat both offline and live.
    fn status_line(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.debug_messages.push(message.clone());
        if self.debug_messages.len() > 100 {
            let excess = self.debug_messages.len() - 100;
            self.debug_messages.drain(..excess);
        }
        self.ui.hud.chat_lines.push(message.clone());
        if self.ui.hud.chat_lines.len() > 100 {
            let excess = self.ui.hud.chat_lines.len() - 100;
            self.ui.hud.chat_lines.drain(..excess);
        }
        if let Some(live) = &mut self.live {
            live.hud.chat_lines.push(message);
            live.hud_revision += 1;
        }
    }

    /// Spawn a session against `host:port` and switch the UI into gameplay.
    fn connect_to(&mut self, host: String, port: u16) {
        let live = LiveSession::connect_with_paths_and_auth(
            host,
            port,
            self.config.username.clone(),
            self.config.mc_version.clone(),
            self.session_paths.clone(),
            self.config.access_token.clone(),
            self.config.uuid.clone(),
        );
        // Windowed path: the dedicated real-time `AudioMixThread` owns the
        // cpal stream; gameplay code enqueues `AudioCmd`s non-blocking.
        // (`attach_live` test fixtures intentionally keep the synchronous
        // `pump()` fallback.)
        // Windowed sessions hand their sink to the real-time mix thread
        // lazily in `render_frame` (first live frame): `AudioMixCore`
        // construction is heavy enough that running it on small test
        // threads overflows their stack, and headless/test sessions keep
        // the synchronous `pump()` path by design.
        self.live = Some(live);
        self.ui.screen = Screen::InGameHud;
        self.ui.hud = megu_ui::state::HudState::default();
        self.paused = false;
        self.maybe_regrab();
    }

    /// Launch the integrated server for world `world` (validated by
    /// `integrated::launch`, which also resolves the server binary and
    /// runtime assets) and return the running guard.
    fn launch_singleplayer(
        config: &GpuCliConfig,
        world: &str,
    ) -> Result<integrated::SingleplayerServer, String> {
        let mut cli = megu_client::cli::CliConfig::default();
        cli.quick_play_singleplayer = Some(world.to_owned());
        cli.server_bin = config.server_bin.clone();
        cli.game_dir = config.game_dir.clone();
        integrated::launch(&cli)
    }

    /// Title Singleplayer: launch the integrated loopback world named by
    /// `--quickPlaySingleplayer` (or `New World`), then connect to it.
    fn start_singleplayer(&mut self) -> Result<(), String> {
        let world = self
            .config
            .quick_play_singleplayer
            .clone()
            .unwrap_or_else(|| "New World".to_owned());
        let server = Self::launch_singleplayer(&self.config, &world)?;
        let addr = server.addr();
        self.singleplayer_server = Some(server);
        self.connect_to(addr.ip().to_string(), addr.port());
        Ok(())
    }

    /// Title Multiplayer: connect to the configured server target.
    fn join_multiplayer(&mut self) -> Result<(), String> {
        let (host, port) = self.config.connect.clone().ok_or_else(|| {
            "no server configured — pass --connect host:port or \
             --quickPlayMultiplayer host:port"
                .to_owned()
        })?;
        self.connect_to(host, port);
        Ok(())
    }

    /// Friends "join with code": bind a [`FriendGuestBridge`] to the
    /// configured relay, then run the normal session over its loopback
    /// endpoint — exactly the headless client's relay path.
    fn join_with_code(&mut self, code_value: u64) -> Result<(), String> {
        let relay = self
            .config
            .relay
            .clone()
            .ok_or_else(|| "friend invites need --relay host:port".to_owned())?;
        let display = invite_code_display(code_value);
        let code = megu_proto::InviteCode::parse(&display)
            .map_err(|e| format!("invite code {display}: {e}"))?;
        let bridge =
            FriendGuestBridge::start(&relay, &code, &self.config.username, FRIEND_TIMEOUT)?;
        let addr = bridge.local_addr();
        self.friend_bridge = Some(bridge);
        self.connect_to(addr.ip().to_string(), addr.port());
        Ok(())
    }

    /// Host side of "Play with Friends": register a live invite code on
    /// the relay while the integrated world is running.
    fn generate_invite(&mut self) -> Result<(), String> {
        if self.singleplayer_server.is_none() {
            return Err("friend invites need a hosted world — start singleplayer first".to_owned());
        }
        let relay = self
            .config
            .relay
            .clone()
            .ok_or_else(|| "friend invites need --relay host:port".to_owned())?;
        let protocol = self.config.mc_version.parse::<i32>().unwrap_or(767);
        let world = self
            .config
            .quick_play_singleplayer
            .clone()
            .unwrap_or_else(|| "New World".to_owned());
        let session = FriendHostSession::register(
            &relay,
            protocol,
            &world,
            8,
            host_secret_bytes(),
            FRIEND_TIMEOUT,
        )?;
        let code = session.invite_code();
        let value = invite_code_value(&code);
        self.friend_host = Some(session);
        self.ui.active_invite = Some(value);
        Ok(())
    }

    /// `E` opens the window-0 player inventory: a 46-slot
    /// [`ContainerScreenState`] fed by `live.player_inventory`, the mirror
    /// the server's window-0 `container_set_content`/`set_slot` packets
    /// keep current even with no `open_screen`.
    fn open_own_inventory(&mut self) {
        let mut screen = ContainerScreenState::new(&PLAYER_INVENTORY_SCHEMA, 0, 0, "Inventory");
        if let Some(live) = &self.live {
            screen.state_id = live.player_inventory_state_id;
            for (index, stack) in &live.player_inventory {
                if let Ok(index) = usize::try_from(*index) {
                    screen.set_slot(
                        index,
                        UiItemStack::new(
                            stack.item_id.max(0) as u32,
                            stack.count.clamp(0, 255) as u8,
                        ),
                    );
                }
            }
            screen.set_cursor(live.player_carried.as_ref().map(|carried| {
                UiItemStack::new(
                    carried.item_id.max(0) as u32,
                    carried.count.clamp(0, 255) as u8,
                )
            }));
        }
        self.container_screen = Some(screen);
        self.own_inventory_open = true;
        self.set_cursor_grab(false);
    }

    /// Close the window-0 inventory: `E` or `Escape`, with the vanilla
    /// `container_close(0)` notification when a session is live.
    fn close_own_inventory(&mut self) {
        self.own_inventory_open = false;
        self.container_screen = None;
        if let Some(live) = &mut self.live {
            live.container_close(0);
        }
        self.maybe_regrab();
    }

    /// Close whatever container surface is up: the own inventory, or the
    /// server-driven screen (sending its `container_close`).
    fn close_container_surface(&mut self) -> bool {
        if self.own_inventory_open {
            self.close_own_inventory();
            return true;
        }
        if let Some(screen) = self.container_screen.take() {
            if let Some(live) = &mut self.live {
                live.container_close(screen.window_id);
            }
            self.maybe_regrab();
            return true;
        }
        false
    }

    /// `L` toggles the advancements overlay; vanilla announces the screen
    /// through `seen_advancements` on open (`Some(tab)`) and close (`None`).
    fn toggle_advancements(&mut self) {
        if self.advancements_open {
            self.advancements_open = false;
            if let Some(live) = &mut self.live {
                live.seen_advancements(None);
            }
            self.maybe_regrab();
        } else {
            self.advancements_open = true;
            let tab = self.live.as_ref().and_then(|live| {
                live.advancements_tab
                    .clone()
                    .or_else(|| live.advancements.keys().next().cloned())
            });
            if let Some(live) = &mut self.live {
                live.seen_advancements(tab.as_deref());
            }
            self.set_cursor_grab(false);
        }
    }

    /// The `L` overlay: a parchment-dark panel listing the mirrored
    /// advancement tree with progress, drawn through the normal UI draw
    /// list so it shares HUD compositing.
    fn render_advancements_overlay(
        live: Option<&LiveSession>,
        w: i32,
        h: i32,
        out: &mut megu_ui::draw::UiDrawList,
    ) {
        let panel_w = w.saturating_sub(80).clamp(200, 520);
        let rows = live.map_or(0, |live| live.advancements.len()) as i32;
        let panel_h = (60 + rows * 14).min(h.saturating_sub(80)).max(80);
        let x0 = ((w - panel_w) / 2).max(8);
        let y0 = ((h - panel_h) / 2).max(8);
        let rect = UiRect::new(x0, y0, panel_w as u32, panel_h as u32);
        out.fill_rect(rect, UiColor::rgba(12, 12, 14, 222));
        out.border_rect(rect, UiColor::rgba(84, 84, 96, 255), 2);
        out.text(
            x0 + 12,
            y0 + 10,
            1,
            UiColor::rgba(255, 213, 79, 255),
            "Advancements",
        );
        let Some(live) = live else {
            out.text_str(
                x0 + 12,
                y0 + 30,
                1,
                UiColor::rgba(170, 170, 178, 255),
                "no live session",
            );
            return;
        };
        if live.advancements.is_empty() {
            out.text_str(
                x0 + 12,
                y0 + 30,
                1,
                UiColor::rgba(170, 170, 178, 255),
                "no advancements synced yet",
            );
            return;
        }
        let mut y = y0 + 28;
        let max_y = y0 + panel_h - 12;
        for (key, advancement) in &live.advancements {
            if y >= max_y {
                out.text_str(x0 + 12, y, 1, UiColor::rgba(170, 170, 178, 255), "...");
                break;
            }
            let progress = live.advancement_progress.get(key);
            let done = progress.map_or(0, |p| p.done);
            let total = advancement.criteria_total.max(1);
            let complete = done >= total;
            let color = if complete {
                UiColor::rgba(85, 255, 85, 255)
            } else {
                UiColor::rgba(220, 220, 226, 255)
            };
            let title = if advancement.title.is_empty() {
                key.as_str()
            } else {
                advancement.title.as_str()
            };
            out.text(x0 + 12, y, 1, color, format!("{title}  {done}/{total}"));
            y += 14;
        }
    }

    fn init_gpu(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let size = PhysicalSize::new(
            self.config.width.unwrap_or(1280).max(1),
            self.config.height.unwrap_or(720).max(1),
        );
        let mut attrs = WindowAttributes::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size(size)
            .with_resizable(true);
        if self.config.fullscreen {
            attrs = attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .map_err(|e| format!("create_window: {e}"))?,
        );
        // Instance bound to this event loop's display, then adapter + device,
        // then a surface the selected adapter can present to.
        let ctx = request_gpu_device(
            self.render_config.gpu_backend,
            Some(event_loop.owned_display_handle()),
            None,
        )
        .map_err(|e| e.to_string())?;
        let surface = ctx
            .instance
            .create_surface(window.clone())
            .map_err(|e| format!("create_surface: {e}"))?;
        let caps = surface.get_capabilities(&ctx.adapter);
        if caps.formats.is_empty() {
            return Err(format!(
                "adapter {:?} cannot present to this surface",
                ctx.report.backend
            ));
        }
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        let mut active = ActiveWindow {
            window,
            surface,
            renderer: {
                GpuRenderer::new(ctx, surface_format)
            },
            surface_format,
            size,
        };
        let config = surface_config_for(
            self.render_config.vsync,
            self.render_config.effective_fps_cap(self.focused),
            &self.policy,
            active.surface_format,
            active.size,
            &caps,
        );
        active
            .surface
            .configure(&active.renderer.gpu().device, &config);
        self.baseline_upload_budget = Some(active.renderer.upload_budget().clone());
        // One-time PCIe upload of the runtime `MEGU EDITION` banner
        // textures (wordmark + patched edition strip); the CPU copies
        // stay resident in `title_branding` for a later surface loss.
        if self.title_branding.has_banner() {
            active.renderer.set_title_branding(
                self.title_branding.logo.as_ref(),
                self.title_branding.edition.as_ref(),
            );
        }
        if self.live.is_none() {
            let scene = build_demo_scene(self.camera.position);
            active
                .renderer
                .upload_scene(&scene)
                .map_err(|e| e.to_string())?;
        }
        if let Some(path) = &self.config.shader_pack {
            active.renderer.load_shaderpack(path);
        }
        self.active = Some(active);
        let settings = self.ui.settings.clone();
        self.apply_settings(&settings);
        self.maybe_regrab();
        Ok(())
    }

    /// Mirror the live session's server-driven `open_screen` into a
    /// `ContainerScreenState`: the schema comes from `menu_schema` (all 25
    /// vanilla `0x33` menus), slots/carried refresh when `state_id` bumps,
    /// and the pointer is released while a screen is up (vanilla mouse
    /// behavior).
    fn sync_container_screen(&mut self) {
        // Phase 1: resolve the schema + snapshot the shared state under an
        // immutable borrow of `self.live`.
        struct Snapshot {
            schema: &'static megu_ui::container_screen::MenuScreenSchema,
            window_id: u8,
            state_id: i32,
        }
        let snap = self
            .live
            .as_ref()
            .and_then(|live| live.open_screen.as_ref())
            .and_then(|open| {
                let schema = open.menu_type.try_into().ok().and_then(menu_schema)?;
                let window_id = u8::try_from(open.window_id).ok()?;
                Some(Snapshot {
                    schema,
                    window_id,
                    state_id: open.state_id,
                })
            });
        // Phase 2: mutate `self.container_screen` / grab state freely.
        match snap {
            Some(Snapshot {
                schema,
                window_id,
                state_id,
            }) => {
                // A server-opened window always replaces the local
                // inventory surface.
                self.own_inventory_open = false;
                let opened_now = !matches!(
                    &self.container_screen,
                    Some(screen) if screen.window_id == window_id
                );
                if opened_now {
                    self.container_screen = Some(ContainerScreenState::new(
                        schema,
                        window_id,
                        state_id,
                        "container",
                    ));
                    self.set_cursor_grab(false);
                }
                let screen = self.container_screen.as_mut().unwrap();
                if opened_now || screen.state_id != state_id {
                    screen.state_id = state_id;
                    if let Some(open) = self
                        .live
                        .as_ref()
                        .and_then(|live| live.open_screen.as_ref())
                    {
                        for (index, stack) in &open.slots {
                            if let Ok(index) = usize::try_from(*index) {
                                screen.set_slot(
                                    index,
                                    UiItemStack::new(
                                        stack.item_id.max(0) as u32,
                                        stack.count.clamp(0, 255) as u8,
                                    ),
                                );
                            }
                        }
                        screen.set_cursor(open.carried.as_ref().map(|carried| {
                            UiItemStack::new(
                                carried.item_id.max(0) as u32,
                                carried.count.clamp(0, 255) as u8,
                            )
                        }));
                    }
                }
            }
            None => {
                if self.own_inventory_open {
                    // Window-0 sync without an `open_screen`: refresh the
                    // local inventory's slots when `state_id` bumps.
                    if let (Some(live), Some(screen)) = (&self.live, &mut self.container_screen) {
                        if screen.state_id != live.player_inventory_state_id {
                            screen.state_id = live.player_inventory_state_id;
                            for (index, stack) in &live.player_inventory {
                                if let Ok(index) = usize::try_from(*index) {
                                    screen.set_slot(
                                        index,
                                        UiItemStack::new(
                                            stack.item_id.max(0) as u32,
                                            stack.count.clamp(0, 255) as u8,
                                        ),
                                    );
                                }
                            }
                            screen.set_cursor(live.player_carried.as_ref().map(|carried| {
                                UiItemStack::new(
                                    carried.item_id.max(0) as u32,
                                    carried.count.clamp(0, 255) as u8,
                                )
                            }));
                        }
                    }
                } else if self.container_screen.take().is_some() {
                    self.maybe_regrab();
                }
            }
        }
    }

    /// Forward one UI click action as the matching `container_click` /
    /// `container_button_click` serverbound packet on the worker.
    fn forward_click_action(
        live: &mut LiveSession,
        action: megu_ui::container_screen::ClickWindowAction,
    ) {
        let changed: Vec<(i16, Vec<u8>)> = action
            .changed_slots
            .iter()
            .map(|change| {
                (
                    change.slot,
                    encode_slot_stack(change.stack.item_id as i32, i32::from(change.stack.count)),
                )
            })
            .collect();
        let carried = action
            .carried_item
            .as_ref()
            .map(|stack| encode_slot_stack(stack.item_id as i32, i32::from(stack.count)))
            .unwrap_or_else(|| encode_slot_stack(0, 0));
        live.container_click(
            action.window_id,
            action.state_id,
            action.slot,
            action.button,
            i32::from(action.mode),
            changed,
            carried,
        );
    }

    /// Route a press/release to the open container screen when one is up.
    /// Returns `true` when the event was consumed by the screen.
    fn container_mouse_input(&mut self, state: ElementState, button: MouseButton) -> bool {
        let Some(screen) = &mut self.container_screen else {
            return false;
        };
        let (mx, my) = self.mouse_pos;
        let (w, h) = self
            .active
            .as_ref()
            .map(|a| (a.size.width as f32, a.size.height as f32))
            .unwrap_or((0.0, 0.0));
        let button_id: i8 = match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            _ => return true,
        };
        match state {
            ElementState::Pressed => {
                let layout = screen.layout(w, h);
                if let Some(hit) = layout.slots.iter().find(|slot| slot.rect.contains(mx, my)) {
                    let slot_index = hit.index;
                    if let Some(action) = screen.click(slot_index, button_id) {
                        if let Some(live) = &mut self.live {
                            Self::forward_click_action(live, action);
                        }
                    }
                } else if let Some(hit) = layout.buttons.iter().find(|b| b.rect.contains(mx, my)) {
                    let id = hit.id;
                    if let Some(action) = screen.button_click(id) {
                        if let Some(live) = &mut self.live {
                            live.container_button_click(action.window_id, action.button_id);
                        }
                    }
                }
            }
            ElementState::Released => {
                if let Some(action) = screen.drag_end() {
                    if let Some(live) = &mut self.live {
                        Self::forward_click_action(live, action);
                    }
                }
            }
        }
        true
    }

    /// The six view-frustum planes of the current render camera, in the
    /// `box_in_frustum` form `LiveSession::poll` orders mesh rebuilds
    /// with. `None` while no active surface exists (poll then degrades to
    /// pure distance ordering).
    fn view_frustum_planes(&self) -> Option<[[f32; 4]; 6]> {
        Self::frustum_planes_for(
            &self.camera_controller,
            &self.camera,
            self.active.as_ref().map(|a| a.size),
        )
    }

    /// [`Self::view_frustum_planes`] over disjoint field borrows so it can
    /// run inside `&mut self.live` scopes.
    fn frustum_planes_for(
        camera_controller: &CameraController,
        camera: &Camera,
        size: Option<PhysicalSize<u32>>,
    ) -> Option<[[f32; 4]; 6]> {
        let size = size?;
        let aspect = size.width.max(1) as f32 / size.height.max(1) as f32;
        let camera = camera_controller.view_camera(camera);
        Some(crate::renderer::frustum_planes_from_view_proj(
            &camera.view_projection(aspect),
        ))
    }

    /// The hovered container stack's namespaced item id — the numeric
    /// `minecraft:item` registry id resolved through the built-in protocol
    /// table; empty (`0`) slots return `None`, and unknown ids degrade to
    /// `unknown:item_<n>` so a modded stack still opens a deterministic
    /// focus target.
    fn hovered_item_name(&self) -> Option<String> {
        let screen = self.container_screen.as_ref()?;
        let stack = screen.slot(screen.hovered_slot?)?;
        (stack.item_id != 0).then(|| crate::recipe_ingest::item_name_for_id(stack.item_id))
    }

    /// JEI-style `R`/`U`/`W` open: navigate to the recipe browser focused
    /// on `item` (recipes/uses detail, or a Ponder scene on the Ponder
    /// tab). A hovered stack is ground truth, so the id lands in the
    /// catalog even when no JAR declared it.
    fn open_recipe_browser_focused(&mut self, item: &str, focus: RecipeFocus) {
        if self.ui.recipe_browser.index.item(item).is_none() {
            let name = crate::recipe_ingest::humanize_id(item);
            self.ui
                .recipe_browser
                .index
                .add_item(ItemEntry::new(item, &name));
        }
        match focus {
            RecipeFocus::Recipes => self
                .ui
                .recipe_browser
                .open_detail(item, DetailMode::Recipes),
            RecipeFocus::Uses => self
                .ui
                .recipe_browser
                .open_detail(item, DetailMode::Uses),
            RecipeFocus::Ponder => {
                let scene =
                    crate::recipe_ingest::ponder_scene_for(&self.ui.recipe_browser.index, item);
                self.ui.recipe_browser.ponder = Some(scene);
                self.ui.recipe_browser.tab = BrowserTab::Ponder;
            }
        }
        for action in self.ui.open_recipe_browser() {
            self.dispatch_ui_action(&action);
        }
        self.maybe_regrab();
    }

    /// While a container screen is open: hover highlight, plus vanilla
    /// quick-craft (`mode = 5`) slot painting while the left button is held
    /// after a `DragState` has been armed.
    fn container_cursor_moved(&mut self) {
        let Some(screen) = &mut self.container_screen else {
            return;
        };
        let (mx, my) = self.mouse_pos;
        let (w, h) = self
            .active
            .as_ref()
            .map(|a| (a.size.width as f32, a.size.height as f32))
            .unwrap_or((0.0, 0.0));
        let layout = screen.layout(w, h);
        let hovered = layout
            .slots
            .iter()
            .find(|slot| slot.rect.contains(mx, my))
            .map(|slot| (slot.index, usize::from(slot.index.max(0) as u16)));
        screen.hover(hovered.map(|(_, index)| index));
        if self.left_mouse_held {
            if let Some(DragState { slots, .. }) = screen.drag_mode.as_ref() {
                // The drag is armed: entering a new slot paints it into the
                // quick-craft set (`click_mode` mode 5 handles dedup).
                if let Some(&(slot_index, _)) = hovered.as_ref() {
                    if !slots.contains(&slot_index) {
                        if let Some(action) = screen.click_mode(slot_index, 0, 5) {
                            if let Some(live) = &mut self.live {
                                Self::forward_click_action(live, action);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Recompute the chat bar's `/` suggestions through the session's
    /// `CommandDispatcher` (`complete_suggestions`); plain chat gets none.
    fn refresh_chat_suggestions(&mut self) {
        self.chat_suggestions.clear();
        let (Some(bar), Some(live)) = (&self.chat_bar, &self.live) else {
            return;
        };
        let text = bar.text();
        let Some(input) = text.strip_prefix('/') else {
            return;
        };
        let cursor = bar.cursor().saturating_sub(1);
        if let Ok(suggestions) = live.commands.complete_suggestions(input, cursor) {
            for index in 0..suggestions.len() {
                if let Some(item) = suggestions.get(index) {
                    self.chat_suggestions.push(item.to_string());
                }
            }
        }
    }

    /// Route a key event into the open chat bar: text from `event.text`,
    /// editing keys, Tab cycles badge completions, Enter sends through
    /// `send_chat`, Escape closes and re-grabs the pointer.
    fn chat_bar_key(&mut self, event: &winit::event::KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let mut close = false;
        match event.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => close = true,
            PhysicalKey::Code(KeyCode::Enter) | PhysicalKey::Code(KeyCode::NumpadEnter) => {
                if let Some(bar) = &mut self.chat_bar {
                    let text = bar.text().to_string();
                    if !text.is_empty() {
                        bar.push_history_entry(&text);
                        if let Some(live) = &mut self.live {
                            live.send_chat(&text);
                        }
                    }
                }
                close = true;
            }
            PhysicalKey::Code(KeyCode::Backspace) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.delete_backward();
                }
                self.refresh_chat_suggestions();
            }
            PhysicalKey::Code(KeyCode::Delete) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.delete_forward();
                }
                self.refresh_chat_suggestions();
            }
            PhysicalKey::Code(KeyCode::Tab) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.cycle_completion(true);
                }
            }
            PhysicalKey::Code(KeyCode::ArrowLeft) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.move_cursor_left();
                }
            }
            PhysicalKey::Code(KeyCode::ArrowRight) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.move_cursor_right();
                }
            }
            _ => {
                if let Some(text) = &event.text {
                    if let Some(bar) = &mut self.chat_bar {
                        for ch in text.chars() {
                            bar.push_char(ch);
                        }
                    }
                    self.refresh_chat_suggestions();
                }
            }
        }
        if close {
            self.chat_bar = None;
            self.chat_suggestions.clear();
            self.maybe_regrab();
        }
    }

    /// Mirror the live session's server-driven HUD channels into the UI
    /// tree so `ui.render` composites them: sidebar `ScoreboardState`
    /// (display slot 1), `TitleOverlayState` (re-armed on `title_revision`
    /// edges, ticked at 20 TPS), and the Jade-style `LookingAtBlockState`.
    #[allow(clippy::too_many_arguments)]
    fn apply_live_hud(
        ui: &mut UiState,
        title_seen_rev: &mut u64,
        title_tick_accum: &mut f32,
        dt: f32,
        feed: &LiveHudFeed,
    ) {
        let sidebar_objective = feed.scoreboard.display_slots.get(&1);
        ui.hud.scoreboard = match sidebar_objective {
            Some(objective) => ScoreboardState {
                objective_title: feed
                    .scoreboard
                    .objectives
                    .get(objective)
                    .and_then(|(nbt, _, _)| nbt.as_ref())
                    .and_then(|bytes| {
                        megu_client::protocol::WireReader::new(bytes)
                            .read_nbt()
                            .ok()
                            .map(|value| value.extract_text())
                    }),
                entries: feed
                    .scoreboard
                    .scores
                    .iter()
                    .filter(|((name, _), _)| name == objective)
                    .map(|((_, owner), score)| ScoreboardEntry {
                        label: owner.clone(),
                        score: *score,
                    })
                    .collect(),
                visible: true,
            },
            None => ScoreboardState::default(),
        };

        if feed.title_revision != *title_seen_rev {
            *title_seen_rev = feed.title_revision;
            let [fade_in, stay, fade_out] = feed.title_times.unwrap_or([10, 70, 20]);
            let mut overlay = TitleOverlayState::vanilla();
            overlay.show(
                feed.title_text.clone().unwrap_or_default(),
                feed.subtitle_text.clone().unwrap_or_default(),
                fade_in.max(0) as u32,
                stay.max(0) as u32,
                fade_out.max(0) as u32,
            );
            ui.hud.title_overlay = overlay;
            *title_tick_accum = 0.0;
        }
        *title_tick_accum += dt;
        while *title_tick_accum >= 0.05 {
            ui.hud.title_overlay.tick();
            *title_tick_accum -= 0.05;
        }

        ui.hud.looking_at = match &feed.looking_at_block {
            Some((position, block_id)) => LookingAtBlockState {
                enabled: true,
                block_id: block_id.clone(),
                display_name: block_id.clone(),
                position: Some(*position),
                ..Default::default()
            },
            None => LookingAtBlockState::default(),
        };
    }

    fn render_frame(&mut self) {
        let frame_begin = Instant::now();
        self.frame_start = frame_begin;
        let dt = frame_begin.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = frame_begin;

        // JIT late-latch: mouse deltas that arrived while the loop slept
        // apply once, right here at the top of the frame, so the camera
        // pose read below is the freshest input the OS delivered.
        let (mdx, mdy) = std::mem::take(&mut self.pending_mouse_delta);
        if mdx != 0.0 || mdy != 0.0 {
            self.camera_controller
                .apply_mouse_look(&mut self.camera, mdx, mdy);
        }

        let planes = self.view_frustum_planes();
        if let Some(live) = &mut self.live {
            live.poll(
                self.camera.position,
                planes.as_ref().map_or(&[][..], |p| &p[..]),
            );
            if let Some(pose) = live.camera_sync.take() {
                self.camera.position = [
                    pose.feet[0] as f32,
                    pose.feet[1] as f32 + 1.62,
                    pose.feet[2] as f32,
                ];
                self.camera.yaw_deg = (pose.yaw + 180.0).rem_euclid(360.0);
                self.camera.pitch_deg = (-pose.pitch).clamp(-89.9, 89.9);
            }
        }
        self.sync_container_screen();

        // Camera movement only applies while unpaused and grabbed.
        if !self.paused {
            self.camera.apply_move(self.keys.move_input(), dt.min(0.1));
        }
        self.camera_controller
            .tick(dt, !self.paused && self.keys.sprint);

        if let Some(live) = &mut self.live {
            let input = self.keys.move_input();
            live.send_movement(PlayerMovement {
                pose: PlayerPose {
                    feet: [
                        self.camera.position[0] as f64,
                        (self.camera.position[1] - 1.62) as f64,
                        self.camera.position[2] as f64,
                    ],
                    yaw: (self.camera.yaw_deg - 180.0).rem_euclid(360.0),
                    pitch: -self.camera.pitch_deg,
                },
                strafe: if self.paused { 0.0 } else { input.strafe },
                forward: if self.paused { 0.0 } else { input.forward },
                jump: !self.paused && input.vertical > 0.0,
            });
        }

        // Occluded (minimized / fully covered): zero GPU work. The live
        // session above still polls so the connection stays alive.
        if self.occluded
            && std::env::var_os("MEGU_DISABLE_OCCLUSION_SLEEP").is_none()
        {
            return;
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };
        // Zero extent means a minimized window (Windows reports `0x0`):
        // acquiring or presenting a zero-sized swapchain panics inside
        // DXGI `ResizeBuffers`/`vkCreateSwapchainKHR`, so the policy skips
        // all GPU work here — the live-session poll above stays alive.
        if self.policy.skip_zero_extent_present
            && (active.size.width == 0 || active.size.height == 0)
        {
            return;
        }
        let (w, h) = (active.size.width.max(1), active.size.height.max(1));

        if let Some(live) = &mut self.live {
            active.renderer.set_player_position(self.camera.position);
            if live.terrain_full_sync {
                // First sync / dimension change: full reconcile once.
                active.renderer.update_live_terrain(&live.world);
                live.terrain_full_sync = false;
                live.dirty_terrain_coords.clear();
                live.terrain_order_sweep = false;
                live.terrain_dirty = false;
            } else if live.terrain_dirty
                || !live.dirty_terrain_coords.is_empty()
                || live.terrain_order_sweep
            {
                // O(1) steady path: only the coords `LiveSession` marked
                // per play event plus, when the camera re-sorted the
                // translucent stream, a resident-order sweep. No
                // full-world mesh scan.
                let dirty = std::mem::take(&mut live.dirty_terrain_coords);
                let order_sweep = std::mem::take(&mut live.terrain_order_sweep);
                active
                    .renderer
                    .update_live_terrain_tracked(&live.world, &dirty, order_sweep);
                live.terrain_dirty = false;
            }
            // Revision-gated HUD sync: unchanged server state skips the
            // `HudState` clone entirely (`live.hud_sync_skips` counts hits).
            live.sync_hud_to(&mut self.ui.hud);
            live.record_frame_and_refresh_minihud(dt, &mut self.ui);
            let feed = live.hud_feed();
            Self::apply_live_hud(
                &mut self.ui,
                &mut self.title_seen_rev,
                &mut self.title_tick_accum,
                dt,
                &feed,
            );
            if live.asset_bundle_pending_upload {
                // Dropping the CPU pixel payloads is part of the upload:
                // after this call VRAM owns the sole copy of the atlas and
                // `live.pending_atlas_pixels` is `None` (`atlas_meta`
                // stays resident).
                live.upload_pending_atlas(&mut active.renderer);
            }
            active.renderer.sync_subworlds(&mut live.subworlds);
            // First live frame on the windowed path: hand the sink to the
            // dedicated real-time `AudioMixThread` so gameplay code
            // submits `AudioCmd`s non-blocking (headless/test sessions
            // keep the synchronous `pump()` fallback).
            if !live.dedicated_audio_mix() {
                live.enable_dedicated_audio_mix();
            }
            // Upload admission finished above: drop the CPU mesh copies
            // of the sections VRAM now owns (keeps `packed_mesh_count()`
            // at the working-set bound, V2).
            live.release_uploaded_cpu_copies(&mut active.renderer);
            // Tactile weather/touch sim ticks on the render clock.
            let submerged = live.pose.is_some_and(|pose| {
                let eye = [
                    pose.feet[0].floor() as i32,
                    (pose.feet[1] + 1.62).floor() as i32,
                    pose.feet[2].floor() as i32,
                ];
                live.world
                    .fluid_cell_at(eye[0], eye[1], eye[2])
                    .kind
                    .is_some()
            });
            live.step_tactile_frame(
                dt,
                live.world_time_ticks(),
                live.world.rain_level <= 0.0,
                submerged,
                15,
                0.0,
                live.world.rain_level > 0.0,
            );
            self.ui
                .hud
                .chat_lines
                .extend(self.debug_messages.iter().cloned());
        }

        // HUD/diagnostics through the real client-ui state machine. The
        // F3 string refreshes at 10 Hz (`F3_REFRESH`): steady frames reuse
        // the cached line, which also keeps the draw-list fingerprint
        // stable so the UI mesh rebuild stays skipped.
        self.ui.hud.actionbar = if self.show_diagnostics {
            let stale = self
                .f3_text
                .as_ref()
                .is_none_or(|(_, built)| built.elapsed() >= F3_REFRESH);
            if stale {
                self.f3_text = Some((
                    format!(
                        "F3 | {} | {} | {} | {:.1} ms | x{:.2}{}",
                        active.renderer.gpu().report.resolution.backend.as_str(),
                        self.policy.description,
                        active.renderer.gpu().report.name,
                        self.governor.frame_ms(),
                        self.scale_factor,
                        self.live
                            .as_ref()
                            .map(|live| format!(
                                " | chunks {} | sections {} | VFX {} | ANM {} (rejected {}) | MOT {:?}",
                                live.world.loaded_chunks(),
                                active.renderer.live_section_count(),
                                live.vfx_admitted,
                                live.animation_packets,
                                live.animation_rejected,
                                live.motion.as_ref().map(|motion| motion.phase),
                            ))
                            .unwrap_or_default(),
                    ),
                    Instant::now(),
                ));
            }
            self.f3_text
                .as_ref()
                .map(|(text, _)| text.clone())
                .unwrap_or_default()
        } else {
            self.live
                .as_ref()
                .map_or_else(String::new, |live| match &live.status {
                    SessionStatus::Connecting => "Connecting to server...".to_owned(),
                    SessionStatus::Playing => live.hud.actionbar.clone(),
                    SessionStatus::Disconnected(reason) => format!("Disconnected: {reason}"),
                })
        };
        let mut draw_list = self.ui.render(w, h);
        // Runtime `MEGU EDITION` title branding: with the local JAR's
        // wordmark + patched edition strip GPU-resident, the banner
        // replaces the built-in wordmark text commands and the session's
        // yellow splash joins the title screen.
        let mut brand_logo_rect = None;
        let mut brand_edition_rect = None;
        let mut brand_splash_mesh = None;
        if self.ui.screen == Screen::Title && self.title_branding.has_banner() {
            draw_list.commands.retain(|c| !is_builtin_title_text(c));
            if let (Some(logo), Some(edition)) = (
                self.title_branding.logo.as_ref(),
                self.title_branding.edition.as_ref(),
            ) {
                let layout = crate::title_branding::title_brand_layout(
                    w,
                    h,
                    (logo.width, logo.height),
                    (edition.width, edition.height),
                );
                brand_logo_rect = Some(layout.logo_rect);
                brand_edition_rect = Some(layout.edition_rect);
                let splash = self.title_branding.select_splash(self.splash_seed);
                let mut splash_list = UiDrawList::new();
                splash_list.text(
                    layout.splash_anchor_x - text_width(splash, 1) as i32,
                    layout.splash_y,
                    1,
                    UiColor::rgb(255, 255, 0),
                    splash,
                );
                brand_splash_mesh = Some(tessellate_draw_list(w, h, &splash_list));
            }
        }
        active.renderer.update_title_branding(
            brand_logo_rect,
            brand_edition_rect,
            brand_splash_mesh.as_ref(),
        );
        if let Some(live) = &self.live {
            if !live.magic.is_empty() {
                draw_list
                    .commands
                    .extend(build_magic_hud_draw_list(&live.magic, w, h).commands);
            }
        }
        // Server-driven container screen draws above the HUD.
        if let Some(screen) = &self.container_screen {
            screen.render(w as f32, h as f32, &mut draw_list);
        }
        if self.advancements_open {
            Self::render_advancements_overlay(
                self.live.as_ref(),
                w as i32,
                h as i32,
                &mut draw_list,
            );
        }
        // Dirty-only upload: unchanged HUD frames skip even the quad
        // tessellation; changed frames upload only the small dynamic
        // vertex/index buffers — never a `w*h*4` CPU raster + texture
        // write (`renderer.ui_cpu_raster_uploads` stays at zero).
        let fingerprint = fingerprint_draw_list(w, h, &draw_list);
        if self.ui_cache.should_upload(w, h, fingerprint) {
            let mesh = tessellate_draw_list(w, h, &draw_list);
            active.renderer.update_ui_mesh(w, h, &mesh);
        }
        let render_camera = self.camera_controller.view_camera(&self.camera);
        let sky = active
            .renderer
            .set_camera(&render_camera, w as f32 / h as f32);
        match active.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                match &self.live {
                    Some(live) => {
                        let lights = live.dynamic_colored_lights();
                        active.renderer.render_to_view_with_env(
                            &view,
                            w,
                            h,
                            &sky,
                            &live.atmosphere_blender,
                            &live.chunk_fades,
                            live.world_time_ticks(),
                            &lights,
                        );
                    }
                    None => active.renderer.render_to_view(&view, w, h, &sky),
                }
                // Wayland `wl_surface.frame` pacing: notify the compositor
                // right before present so frame callbacks align with the
                // actual swap (a no-op everywhere else; the policy gates
                // it). `--vsync off` skips the notify entirely — an
                // `Immediate`/`Mailbox` swap must not wait on a frame
                // callback the compositor only schedules for vsync clients.
                if self.policy.call_pre_present_notify
                    && crate::should_call_pre_present_notify(self.config.vsync)
                {
                    active.window.pre_present_notify();
                }
                // One-shot ground-truth readback for capture debugging:
                // MEGU_FRAME_DUMP=/path.ppm copies the presented surface into
                // a CPU buffer on present #30 and writes a binary PPM.
                if let Ok(path) = std::env::var("MEGU_FRAME_DUMP") {
                    static DUMPED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    static PN0: std::sync::atomic::AtomicU32 =
                        std::sync::atomic::AtomicU32::new(0);
                    let pn = PN0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if pn >= 30
                        && !DUMPED.swap(true, std::sync::atomic::Ordering::Relaxed)
                    {
                        let dev = &active.renderer.gpu().device;
                        let queue = &active.renderer.gpu().queue;
                        let (w, h) = (w as u32, h as u32);
                        let row = w * 4;
                        let padded = row.div_ceil(256) * 256;
                        let buf = dev.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("frame-dump"),
                            size: (padded * h) as u64,
                            usage: wgpu::BufferUsages::COPY_DST
                                | wgpu::BufferUsages::MAP_READ,
                            mapped_at_creation: false,
                        });
                        let mut enc = dev.create_command_encoder(
                            &wgpu::CommandEncoderDescriptor {
                                label: Some("frame-dump"),
                            },
                        );
                        enc.copy_texture_to_buffer(
                            wgpu::TexelCopyTextureInfo {
                                texture: &frame.texture,
                                mip_level: 0,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::TexelCopyBufferInfo {
                                buffer: &buf,
                                layout: wgpu::TexelCopyBufferLayout {
                                    offset: 0,
                                    bytes_per_row: Some(padded),
                                    rows_per_image: Some(h),
                                },
                            },
                            wgpu::Extent3d {
                                width: w,
                                height: h,
                                depth_or_array_layers: 1,
                            },
                        );
                        queue.submit([enc.finish()]);
                        let slice = buf.slice(..);
                        let (tx2, rx2) = std::sync::mpsc::channel();
                        slice.map_async(wgpu::MapMode::Read, move |r| {
                            let _ = tx2.send(r);
                        });
                        let _ = dev.poll(wgpu::PollType::Wait {
                            submission_index: None,
                            timeout: Some(std::time::Duration::from_secs(10)),
                        });
                        if let Ok(Ok(())) = rx2.recv() {
                            let data = slice.get_mapped_range().unwrap();
                            let mut img =
                                format!("P6\n{} {}\n255\n", w, h).into_bytes();
                            for y in 0..h as usize {
                                let start = y * padded as usize;
                                img.extend_from_slice(
                                    &data[start..start + row as usize],
                                );
                            }
                            drop(data);
                            buf.unmap();
                            let _ = std::fs::write(&path, &img);
                            eprintln!("[dump] wrote {} ({} bytes)", path, img.len());
                        }
                    }
                }
                active.renderer.gpu().queue.present(frame);
                if std::env::var_os("MEGU_FPS_LOG").is_some() {
                    static FPS: std::sync::OnceLock<
                        std::sync::Mutex<(std::time::Instant, u32)>,
                    > = std::sync::OnceLock::new();
                    let st = FPS.get_or_init(|| {
                        std::sync::Mutex::new((std::time::Instant::now(), 0))
                    });
                    let mut g = st.lock().unwrap();
                    g.1 += 1;
                    let el = g.0.elapsed();
                    if el >= std::time::Duration::from_secs(5) {
                        eprintln!(
                            "[fps] {:.1} fps ({} frames / {:.1}s)",
                            g.1 as f64 / el.as_secs_f64(),
                            g.1,
                            el.as_secs_f64()
                        );
                        *g = (std::time::Instant::now(), 0);
                    }
                }
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {}
            surface_kind @ (wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation) => {
                let caps = active
                    .surface
                    .get_capabilities(&active.renderer.gpu().adapter);
                let cfg = surface_config_for(
                    self.render_config.vsync,
                    self.render_config.effective_fps_cap(self.focused),
                    &self.policy,
                    active.surface_format,
                    active.size,
                    &caps,
                );
                active
                    .surface
                    .configure(&active.renderer.gpu().device, &cfg);
                if matches!(surface_kind, wgpu::CurrentSurfaceTexture::Lost) {
                    // The atlas pixel payloads were dropped after upload;
                    // re-derive them on demand instead of keeping a CPU
                    // duplicate resident for a loss that may never come.
                    if let Some(live) = &mut self.live {
                        live.request_atlas_reload();
                    }
                }
            }
        }
        let stage = self.governor.observe_frame(frame_begin.elapsed());
        // Live P95 feedback edge: the session's real measured
        // frame-time window feeds the renderer's adaptive upload-budget
        // controller — `upload_budget_p95_observations` counts each
        // observation so the loop is provably live, not dead code. The
        // Jain–Chlamtac P² streaming P95 is the primary source once its
        // five markers bootstrap; the ring-window P95 covers startup.
        if let (Some(live), Some(active)) = (&self.live, &mut self.active) {
            let p95_ms = live
                .p95_frame_ms()
                .unwrap_or_else(|| live.frame_timings.frame_ms_p95());
            if p95_ms.is_finite() && p95_ms > 0.0 {
                active.renderer.observe_frame_p95_ms(p95_ms);
            }
        }
        self.apply_governor_stage(stage);
    }

    /// Apply the governor's throttle stage live: `Full` restores the
    /// baseline upload budget, user shadow quality and user render
    /// scale; `TrimRebuilds` caps uploads at one section / 32 KiB per
    /// frame and clamps the dirty rebuild allowance; `TrimLod` adds
    /// shadow shutdown; `MinBudget` drops the 3D viewport to the 50%
    /// rung of the render-scale ladder (UI still renders at native).
    fn apply_governor_stage(&mut self, stage: ThrottleStage) {
        if stage == ThrottleStage::Full {
            if let Some(active) = &mut self.active {
                if let Some(budget) = self.baseline_upload_budget.clone() {
                    active.renderer.set_upload_budget(budget);
                }
                active.renderer.set_shadow_quality(self.user_shadow_quality);
                active.renderer.set_render_scale(self.user_render_scale);
            }
            if let Some(live) = &mut self.live {
                live.governor_rebuild_cap = usize::MAX;
            }
            return;
        }
        let Some(active) = &mut self.active else {
            return;
        };
        active.renderer.set_upload_budget(
            ChunkUploadBudget::with_limits(1, 32 * 1024, 150).with_target_frame_us(1_000),
        );
        if let Some(live) = &mut self.live {
            live.governor_rebuild_cap = 1;
        }
        if stage == ThrottleStage::TrimRebuilds {
            return;
        }
        active
            .renderer
            .set_shadow_quality(BuiltinShadowQuality::Off);
        if stage == ThrottleStage::TrimLod {
            return;
        }
        active
            .renderer
            .set_render_scale(RenderScalePreset::UltraPerf50);
    }

    fn handle_key(&mut self, code: KeyCode, state: ElementState, event_loop: &ActiveEventLoop) {
        let pressed = state == ElementState::Pressed;
        let f3_chord_pressed =
            pressed && self.input_router.f3_held && f3_chord_for_key(code).is_some();
        // JEI hover hotkeys while a container/inventory surface is up:
        // `R` shows recipes, `U` uses, `W` the Ponder scene — but only when
        // a resolvable stack is under the cursor. Otherwise the key keeps
        // its usual binding (`R` sorts, `W` moves forward).
        if pressed
            && !self.paused
            && self.container_screen.is_some()
            && matches!(code, KeyCode::KeyR | KeyCode::KeyU | KeyCode::KeyW)
            && self.hovered_item_name().is_some()
        {
            let focus = match code {
                KeyCode::KeyR => RecipeFocus::Recipes,
                KeyCode::KeyU => RecipeFocus::Uses,
                _ => RecipeFocus::Ponder,
            };
            self.apply_window_input(WindowInputEvent::RecipeBrowserFocus(focus));
            return;
        }
        for event in self.input_router.route_key(code, state, self.keys.sprint) {
            if !self.paused
                || matches!(
                    event,
                    WindowInputEvent::DebugChord(_)
                        | WindowInputEvent::SetDebugOverlayVisible(_)
                        | WindowInputEvent::SetTabListVisible(false)
                )
            {
                self.apply_window_input(event);
            }
        }
        if code == KeyCode::F3 || f3_chord_pressed {
            return;
        }
        match code {
            KeyCode::KeyW => self.keys.forward = pressed,
            KeyCode::KeyS => self.keys.back = pressed,
            KeyCode::KeyA => self.keys.left = pressed,
            KeyCode::KeyD => self.keys.right = pressed,
            KeyCode::Space => self.keys.up = pressed,
            KeyCode::ShiftLeft | KeyCode::ShiftRight => self.keys.down = pressed,
            KeyCode::ControlLeft | KeyCode::ControlRight => self.keys.sprint = pressed,
            KeyCode::KeyC => self.camera_controller.set_zoomed(pressed && !self.paused),
            KeyCode::F5 if pressed && !self.paused => {
                self.camera_controller.cycle_mode();
            }
            KeyCode::Escape if pressed => {
                // Vanilla close order: chat (handled before this point) ->
                // container/inventory screen -> advancements overlay ->
                // pause menu -> UI back-navigation on non-HUD screens.
                if self.close_container_surface() {
                    return;
                }
                if self.advancements_open {
                    self.toggle_advancements();
                    return;
                }
                if self.ui.screen == Screen::InGameHud || self.paused {
                    self.paused = !self.paused;
                    if self.paused {
                        self.camera_controller.set_zoomed(false);
                        if let Some(event) = self.input_router.release_held_keys() {
                            self.apply_window_input(event);
                        }
                        self.ui.navigate(Screen::Pause);
                    } else {
                        for action in self
                            .ui
                            .handle_input(UiInputEvent::KeyPressed(UiKey::Escape))
                        {
                            self.apply_ui_action(&action, event_loop);
                        }
                    }
                    self.maybe_regrab();
                } else {
                    for action in self
                        .ui
                        .handle_input(UiInputEvent::KeyPressed(UiKey::Escape))
                    {
                        self.apply_ui_action(&action, event_loop);
                    }
                }
            }
            _ => {
                if pressed && self.ui_wants_input() {
                    if let Some(key) = ui_key(code) {
                        let actions = self.ui.handle_input(UiInputEvent::KeyPressed(key));
                        for action in actions {
                            self.apply_ui_action(&action, event_loop);
                        }
                    }
                }
            }
        }
    }

    fn apply_window_input(&mut self, event: WindowInputEvent) {
        match event {
            WindowInputEvent::SetDebugOverlayVisible(visible) => {
                self.show_diagnostics = visible;
            }
            WindowInputEvent::DebugChord(action) => {
                self.show_diagnostics = self.input_router.debug_chords.show_debug_overlay;
                match action {
                    DebugChordAction::ChatFeedback(message) => {
                        if self.live.is_none() {
                            self.ui.hud.chat_lines.push(message.clone());
                        }
                        self.debug_messages.push(message);
                    }
                    DebugChordAction::HelpLines(lines) => {
                        if self.live.is_none() {
                            self.ui.hud.chat_lines.extend(lines.iter().cloned());
                        }
                        self.debug_messages.extend(lines);
                    }
                    DebugChordAction::ClearChat => {
                        self.debug_messages.clear();
                        self.ui.hud.chat_lines.clear();
                        if let Some(live) = &mut self.live {
                            live.hud.chat_lines.clear();
                            live.hud_revision += 1;
                        }
                    }
                    _ => {}
                }
                if self.debug_messages.len() > 100 {
                    let excess = self.debug_messages.len() - 100;
                    self.debug_messages.drain(..excess);
                }
                if self.live.is_none() && self.ui.hud.chat_lines.len() > 100 {
                    let excess = self.ui.hud.chat_lines.len() - 100;
                    self.ui.hud.chat_lines.drain(..excess);
                }
            }
            WindowInputEvent::SelectHotbar(slot) => {
                self.ui.hud.selected_slot = usize::from(slot);
                if let Some(live) = &mut self.live {
                    live.hud.selected_slot = usize::from(slot);
                    live.hud_revision += 1;
                }
            }
            WindowInputEvent::SetTabListVisible(visible) => {
                self.ui.hud.show_player_list = visible;
                if let Some(live) = &mut self.live {
                    live.hud.show_player_list = visible;
                    live.hud_revision += 1;
                }
            }
            WindowInputEvent::SwapOffhand => {
                if let Some(live) = &mut self.live {
                    live.swap_offhand();
                }
            }
            WindowInputEvent::DropItem { entire_stack } => {
                if let Some(live) = &mut self.live {
                    live.drop_item(entire_stack);
                }
            }
            WindowInputEvent::OpenChat { slash_prefill } => {
                // Vanilla `T`/`/` open the chat/command bar; the pointer is
                // released for text entry like a server screen.
                let mut bar = ZshCommandBar::new();
                if slash_prefill {
                    bar.push_char('/');
                }
                self.chat_bar = Some(bar);
                self.refresh_chat_suggestions();
                self.set_cursor_grab(false);
            }
            WindowInputEvent::SortInventory => {
                // InventoryTweaks-style QoL sort: hovered slot picks its
                // region (container vs player inventory), otherwise the
                // schema default applies. Local-only — the next server
                // `container_set_content` refresh owns the truth.
                if self.inventory_sort.enabled {
                    if let Some(screen) = &mut self.container_screen {
                        let range = match screen.hovered_slot {
                            Some(index) => self.inventory_sort.slot_range_for(screen, index),
                            None => self.inventory_sort.default_sort_range(screen),
                        };
                        self.inventory_sort.sort_container_range(screen, range);
                    }
                }
            }
            WindowInputEvent::OpenInventory => {
                if self.own_inventory_open {
                    self.close_own_inventory();
                } else if !self.close_container_surface() {
                    self.open_own_inventory();
                }
            }
            WindowInputEvent::OpenAdvancements => {
                self.toggle_advancements();
            }
            WindowInputEvent::RecipeBrowserFocus(focus) => {
                // Re-verify the hovered stack here — a stale event can
                // never open a detail view for an item that is not under
                // the cursor (the key path checks before emitting too).
                if let Some(item) = self.hovered_item_name() {
                    self.open_recipe_browser_focused(&item, focus);
                }
            }
        }
    }

    fn apply_ui_action(&mut self, action: &UiAction, event_loop: &ActiveEventLoop) {
        match action {
            UiAction::QuitGame => event_loop.exit(),
            other => self.dispatch_ui_action(other),
        }
    }

    fn reconfigure(&mut self, size: PhysicalSize<u32>) {
        let Some(active) = &mut self.active else {
            return;
        };
        if size.width == 0 || size.height == 0 {
            return;
        }
        active.size = size;
        let caps = active
            .surface
            .get_capabilities(&active.renderer.gpu().adapter);
        let cfg = surface_config_for(
            self.render_config.vsync,
            self.render_config.effective_fps_cap(self.focused),
            &self.policy,
            active.surface_format,
            active.size,
            &caps,
        );
        active
            .surface
            .configure(&active.renderer.gpu().device, &cfg);
    }
}

/// The effective surface configuration for the current size/caps under the
/// detected display server's [`DisplayPresentationPolicy`].
fn surface_config_for(
    vsync: bool,
    fps_cap: u32,
    policy: &DisplayPresentationPolicy,
    surface_format: wgpu::TextureFormat,
    size: PhysicalSize<u32>,
    caps: &wgpu::SurfaceCapabilities,
) -> wgpu::SurfaceConfiguration {
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | if std::env::var_os("MEGU_FRAME_DUMP").is_some() {
                wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::empty()
            },
        format: surface_format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: present_mode_for(vsync, &caps.present_modes),
        // Policy frame latency: `1` uncapped on Wayland/X11/Windows DX12
        // keeps `Mailbox`/`Immediate` input->photon time minimal; `2`
        // under vsync and on macOS Metal, where the extra buffered frame
        // avoids present stalls. An active FPS cap always drops to `1`
        // so the JIT pacing loop never queues a frame behind present.
        desired_maximum_frame_latency: if fps_cap > 0 {
            policy.frame_latency_capped()
        } else {
            policy.frame_latency(vsync)
        },
        // `Opaque` unlocks direct scanout + VRR wherever the policy asks
        // for it (Wayland, X11, Windows, macOS all do); otherwise honor
        // the surface's own first choice.
        alpha_mode: if policy.prefer_opaque_direct_scanout {
            preferred_alpha_mode(&caps.alpha_modes)
        } else {
            caps.alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        },
        view_formats: vec![],
    }
}

/// The 40-bit value inside an [`megu_proto::InviteCode`], matching
/// `InviteCode`'s private `value()` — `as_bytes` is the code big-endian.
fn invite_code_value(code: &megu_proto::InviteCode) -> u64 {
    let b = code.as_bytes();
    u64::from_be_bytes([0, 0, 0, b[0], b[1], b[2], b[3], b[4]])
}

/// Entropy for the friend-host session secret: `/dev/urandom` where it
/// exists, with the same pid/ASLR/monotonic-jitter mixer the relay crate
/// falls back to when it does not.
fn host_secret_bytes() -> [u8; 16] {
    let mut buf = [0u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .is_ok()
        {
            return buf;
        }
    }
    let marker = Box::new(0u8);
    let mut x = (u64::from(std::process::id()) << 32)
        ^ (&*marker as *const u8 as usize) as u64
        ^ 0x9E37_79B9_7F4A_7C15;
    let t0 = Instant::now();
    for chunk in buf.chunks_mut(8) {
        x ^= t0.elapsed().as_nanos() as u64;
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
    buf
}

/// `UiAction::OpenUrl` — hand the link to the platform's URL handler and
/// report spawn failures back through the status line (the UI only emits
/// configured `https?` links).
fn open_url(url: &str) -> Result<(), String> {
    let spawn = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("{program}: {e}"))
    };
    #[cfg(target_os = "macos")]
    {
        return spawn("open", &[url]);
    }
    #[cfg(target_os = "windows")]
    {
        return spawn("rundll32", &["url.dll,FileProtocolHandler", url]);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        spawn("xdg-open", &[url])
    }
}

fn ui_key(code: KeyCode) -> Option<UiKey> {
    Some(match code {
        KeyCode::Tab => UiKey::Tab,
        KeyCode::ArrowUp => UiKey::Up,
        KeyCode::ArrowDown => UiKey::Down,
        KeyCode::ArrowLeft => UiKey::Left,
        KeyCode::ArrowRight => UiKey::Right,
        KeyCode::Enter => UiKey::Enter,
        KeyCode::Backspace => UiKey::Backspace,
        _ => return None,
    })
}

impl ApplicationHandler for GpuApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_none() && self.fatal.is_none() {
            if let Err(err) = self.init_gpu(event_loop) {
                self.fatal = Some(err);
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.reconfigure(size),
            WindowEvent::Occluded(occluded) => self.occluded = occluded,
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Fractional HiDPI (e.g. Wayland 125%/150%): clamp into a
                // sane range and reconfigure — a `Resized` follows on
                // platforms where the physical size actually moved, and a
                // no-op reconfigure is cheap where it did not.
                self.scale_factor = scale_factor.clamp(0.5, 4.0);
                let size = self
                    .active
                    .as_ref()
                    .map(|active| active.window.inner_size());
                if let Some(size) = size {
                    self.reconfigure(size);
                }
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if !focused {
                    self.keys = Keys::default();
                    if let Some(event) = self.input_router.release_held_keys() {
                        self.apply_window_input(event);
                    }
                    self.camera_controller.set_zoomed(false);
                    self.set_cursor_grab(false);
                } else if !self.paused {
                    self.maybe_regrab();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.chat_bar.is_some() {
                    self.chat_bar_key(&event);
                } else if let PhysicalKey::Code(code) = event.physical_key {
                    if !event.repeat {
                        self.handle_key(code, event.state, event_loop);
                        // Text entry for UI screens (the recipe browser's
                        // search field and its `r`/`u`/`p`/`+` item keys
                        // all ride `CharTyped`, which winit reports via
                        // `event.text`).
                        if event.state == ElementState::Pressed && self.ui_wants_input() {
                            if let Some(text) = &event.text {
                                for ch in text.chars().filter(|c| !c.is_control()) {
                                    for action in
                                        self.ui.handle_input(UiInputEvent::CharTyped(ch))
                                    {
                                        self.apply_ui_action(&action, event_loop);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x as i32, position.y as i32);
                if self.ui_wants_input() {
                    let actions = self.ui.handle_input(UiInputEvent::MouseMove {
                        x: position.x as i32,
                        y: position.y as i32,
                    });
                    for action in actions {
                        self.apply_ui_action(&action, event_loop);
                    }
                }
                self.container_cursor_moved();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    self.left_mouse_held = state == ElementState::Pressed;
                }
                if self.container_screen.is_some() {
                    self.container_mouse_input(state, button);
                } else if state == ElementState::Pressed && button == MouseButton::Left {
                    if self.ui_wants_input() {
                        let (x, y) = self.mouse_pos;
                        let actions = self.ui.handle_input(UiInputEvent::Click { x, y });
                        for action in actions {
                            self.apply_ui_action(&action, event_loop);
                        }
                    } else {
                        self.maybe_regrab();
                    }
                }
            }
            WindowEvent::RedrawRequested => self.render_frame(),
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        // Raw mouse deltas drive the camera while grabbed — unaffected by
        // cursor clamping at screen edges. Deltas accumulate rather than
        // applying per event: `render_frame` latches the summed delta at
        // the top of the frame, so a 1000 Hz motion stream during a
        // WaitUntil sleep costs zero camera math until the pose is read.
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.cursor_grabbed && !self.paused {
                self.pending_mouse_delta.0 += delta.0;
                self.pending_mouse_delta.1 += delta.1;
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.fatal.is_some() {
            event_loop.exit();
            return;
        }
        if self.occluded
            && std::env::var_os("MEGU_DISABLE_OCCLUSION_SLEEP").is_none()
        {
            // Zero-GPU sleep: no redraw requests at all. A slow tick keeps
            // the live session polled until the surface un-occludes.
            if let Some(live) = &mut self.live {
                let planes = Self::frustum_planes_for(
                    &self.camera_controller,
                    &self.camera,
                    self.active.as_ref().map(|a| a.size),
                );
                live.poll(
                    self.camera.position,
                    planes.as_ref().map_or(&[][..], |p| &p[..]),
                );
            }
            let wake = Instant::now() + OCCLUDED_TICK;
            event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
            return;
        }
        // The effective cap drops to `background_fps_limit` while unfocused
        // (the RenderConfig rule); WaitUntil holds the loop to it. Calling
        // `request_redraw` before the deadline would let a 1000 Hz input
        // event stream bypass the cap and storm redundant redraws.
        let cap = self.render_config.effective_fps_cap(self.focused);
        if cap > 0 {
            let deadline = self.frame_start + Duration::from_secs_f64(1.0 / f64::from(cap));
            if Instant::now() < deadline {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                return;
            }
            // Deadline reached: request one redraw and sleep until it
            // fires — a capped loop never enters `Poll` spin.
            if let Some(active) = &self.active {
                active.window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        if let Some(active) = &self.active {
            active.window.request_redraw();
        }
        // Wayland frame-callback starvation: a requested redraw that never
        // gets dispatched means the compositor is throttling this surface
        // (fully occluded/unmapped — winit only re-emits RedrawRequested
        // after a wl_surface.frame callback). Spinning `Poll` here burns a
        // core at ~1.5M `about_to_wait` iterations/sec doing nothing, so
        // after 500 ms without a delivered frame fall back to a slow wake
        // tick; when callbacks resume, `frame_start` refreshes and Poll
        // pacing returns automatically.
        if self.frame_start.elapsed() > Duration::from_millis(500) {
            if let Some(live) = &mut self.live {
                let planes = Self::frustum_planes_for(
                    &self.camera_controller,
                    &self.camera,
                    self.active.as_ref().map(|a| a.size),
                );
                live.poll(
                    self.camera.position,
                    planes.as_ref().map_or(&[][..], |p| &p[..]),
                );
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(33),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Poll);
        }
    }
}

/// Deterministic per-session splash seed: FNV-1a over the launch
/// config's identity fields (`username`/`--version`/`--gameDir`) — zero
/// `SystemTime`, stable across the whole run.
fn splash_seed_for(config: &GpuCliConfig) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(config.username.as_bytes());
    if let Some(id) = config.version_id.as_deref() {
        mix(id.as_bytes());
    }
    if let Some(dir) = config.game_dir.as_deref() {
        mix(dir.to_string_lossy().as_bytes());
    }
    hash
}

/// The built-in `client-ui` title wordmark/tagline `Text` commands the
/// GPU banner replaces while the local JAR supplies real branding.
/// Matched by payload (never position) so a client-ui layout change
/// degrades to "both drawn", never to a panic.
fn is_builtin_title_text(command: &UiCommand) -> bool {
    matches!(
        command,
        UiCommand::Text { scale, text, .. }
            if (*scale == 3 && text == "MEGU")
                || (*scale == 1
                    && (text == "clean-room native client"
                        || text == "クリーンルーム ネイティブクライアント"))
    )
}

pub const MEGU_WEBSITE_URL: &str = "https://megu.megumin.tech/";
pub const MEGU_SUPPORT_URL: &str = "https://megu.megumin.tech/#support";
pub const MEGU_CONTRIBUTE_URL: &str = "https://github.com/nekomario28/megu-mc";
pub const MEGU_REPORT_URL: &str = "https://megu.megumin.tech/";

/// Populate the official Megu links on a windowed [`UiState`] without
/// altering [`UiState::default`] (which stays URL-empty for `client-ui` tests).
pub fn apply_megu_default_links(ui: &mut UiState) {
    ui.website_url = MEGU_WEBSITE_URL.to_owned();
    ui.support_url = MEGU_SUPPORT_URL.to_owned();
    ui.contribute_url = MEGU_CONTRIBUTE_URL.to_owned();
    ui.report_url = MEGU_REPORT_URL.to_owned();
}

/// Run the windowed client. Returns `Err` text when the event loop or GPU
/// setup fails (e.g. no display — use `--headless-probe` there).
pub fn run(config: GpuCliConfig) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| format!("EventLoop::new: {e}"))?;
    let mut app = GpuApp::new(config);
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("run_app: {e}"))?;
    match app.fatal {
        Some(fatal) => Err(fatal),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn megu_default_links_wire_into_gpu_app_and_respect_show_support_links_toggle() {
        // `UiState::default()` stays empty so `client-ui` default-URL tests stay intact.
        let bare = UiState::default();
        assert!(bare.website_url.is_empty());
        assert!(bare.support_url.is_empty());
        assert!(bare.contribute_url.is_empty());
        assert!(bare.report_url.is_empty());

        let mut app = GpuApp::new(GpuCliConfig::default());
        assert_eq!(app.ui.website_url, "https://megu.megumin.tech/");
        assert_eq!(app.ui.support_url, "https://megu.megumin.tech/#support");
        assert_eq!(
            app.ui.contribute_url,
            "https://github.com/nekomario28/megu-mc"
        );
        assert_eq!(app.ui.report_url, "https://megu.megumin.tech/");
        assert_eq!(app.ui.screen, Screen::Title);
        assert!(app.ui.settings.qol.show_support_links);

        // With `show_support_links = true`, all 4 links render on Title and open Megu URLs.
        let _ = app.ui.render(1280, 720);
        for (id, expected) in [
            (
                megu_ui::state::WidgetId::WebsiteLink,
                "https://megu.megumin.tech/",
            ),
            (
                megu_ui::state::WidgetId::SupportLink,
                "https://megu.megumin.tech/#support",
            ),
            (
                megu_ui::state::WidgetId::ContributeLink,
                "https://github.com/nekomario28/megu-mc",
            ),
            (
                megu_ui::state::WidgetId::ReportLink,
                "https://megu.megumin.tech/",
            ),
        ] {
            let hit = app
                .ui
                .hit_regions
                .iter()
                .find(|h| h.id == id)
                .cloned()
                .expect("expected link hit region on Title when show_support_links=true");
            let acts = app.ui.handle_input(megu_ui::state::UiInputEvent::Click {
                x: hit.rect.x + 2,
                y: hit.rect.y + 2,
            });
            assert_eq!(acts, vec![UiAction::OpenUrl(expected.to_owned())]);
        }

        // With `show_support_links = false`, SupportLink and ContributeLink hide on Title.
        app.ui.settings.qol.show_support_links = false;
        let _ = app.ui.render(1280, 720);
        assert!(
            !app.ui
                .hit_regions
                .iter()
                .any(|h| h.id == megu_ui::state::WidgetId::SupportLink),
            "SupportLink must hide when show_support_links=false"
        );
        assert!(
            !app.ui
                .hit_regions
                .iter()
                .any(|h| h.id == megu_ui::state::WidgetId::ContributeLink),
            "ContributeLink must hide when show_support_links=false"
        );
        assert!(
            app.ui
                .hit_regions
                .iter()
                .any(|h| h.id == megu_ui::state::WidgetId::ReportLink),
            "ReportLink stays accessible even when show_support_links=false"
        );
    }

    #[test]
    fn submit_crash_report_honors_off_mode_and_writes_redacted_local_report() {
        let dir = std::env::temp_dir().join(format!(
            "megu-gpu-crash-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cfg = GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        };
        let mut app = GpuApp::new(cfg);
        app.ui.crash = Some(megu_core::crash_report::CrashContext {
            error_kind: "panic".to_string(),
            culprit_mod_or_module: "client-gpu".to_string(),
            mc_version: "1.21.1".to_string(),
            detail_message: "panic at /home/alice/secret with 192.168.1.9".to_string(),
        });

        // Mode Off: writes nothing.
        app.ui.settings.qol.crash_report_mode = megu_ui::state::CrashReportMode::Off;
        app.handle_submit_crash_report();
        assert!(
            !dir.join("crash-reports").exists(),
            "CrashReportMode::Off must not create crash-reports/"
        );

        // Mode Ask (after explicit UI confirmation emits SubmitCrashReport): writes redacted report.
        app.ui.settings.qol.crash_report_mode = megu_ui::state::CrashReportMode::Ask;
        app.handle_submit_crash_report();
        let reports: Vec<_> = std::fs::read_dir(dir.join("crash-reports"))
            .expect("crash-reports dir created")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(reports.len(), 1);
        let body = std::fs::read_to_string(reports[0].path()).unwrap();
        assert!(!body.contains("/home/alice"), "home path must be redacted");
        assert!(!body.contains("192.168.1.9"), "IP must be redacted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_upload_cache_uploads_first_frame_and_skips_identical() {
        let mut cache = UiUploadCache::default();
        // First frame always uploads.
        assert!(cache.should_upload(1280, 720, 0xaaaa));
        assert_eq!(cache.ui_uploads_performed, 1);
        assert_eq!(cache.ui_uploads_skipped, 0);
        // An identical second frame (same size + fingerprint) is skipped.
        assert!(!cache.should_upload(1280, 720, 0xaaaa));
        assert!(!cache.should_upload(1280, 720, 0xaaaa));
        assert_eq!(cache.ui_uploads_performed, 1);
        assert_eq!(cache.ui_uploads_skipped, 2);
    }

    #[test]
    fn ui_upload_cache_reuploads_on_hud_change_and_resize() {
        let mut cache = UiUploadCache::default();
        assert!(cache.should_upload(1280, 720, 0xaaaa));
        // HUD text updated -> new fingerprint -> upload.
        assert!(cache.should_upload(1280, 720, 0xbbbb));
        // Resize -> upload even when the fingerprint collides.
        assert!(cache.should_upload(1920, 1080, 0xbbbb));
        assert_eq!(cache.ui_uploads_performed, 3);
        // Steady state resumes skipping.
        assert!(!cache.should_upload(1920, 1080, 0xbbbb));
        assert_eq!(cache.ui_uploads_skipped, 1);
    }
}
