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
use megu_client::protocol::{encode_slot_stack, NbtValue, WireReader};
use megu_client::render::{FrameBudgetGovernor, RenderConfig, ThrottleStage};
use megu_client::server_list::{ServerEntry, ServerListStore};
use megu_client::spatial_audio::SpatialAudioPreset;
use megu_client::world::{TrackedBlockEntity, TrackedEntity};
use megu_client::zsh_command_bar::ZshCommandBar;
use megu_client_mesh::ber_instancing::{
    build_ber_mesh, VanillaBerFacing, VanillaBerKind, VanillaBerState, VanillaParticleQuad,
    VanillaParticleWorld,
};
use megu_client_mesh::builtin_shadow_wgsl::{BuiltinShadowQuality, RenderScalePreset};
use megu_client_mesh::entity_rig::{build_entity_rig_mesh, EntityPose, EntityRigSpec};
use megu_client_mesh::mesher::classify_vanilla_ber;
use megu_client_mesh::tactile_effects::{
    TactileEffectsConfig, TactileParticle, TactileParticleKind, TactilePreset,
};
use megu_client_mesh::vanilla_plus_3d::ExtrudedMesh;
use megu_client_mesh::vanilla_plus_nature::{NatureParticle, NatureParticleKind};
use megu_client_mesh::vfx_engine::{ParticleDraw, ParticleOrientation, VfxFrameOutput};
use megu_client_mesh::ChunkUploadBudget;
use megu_core::hash::{Fnv1a64, FNV1A64_OFFSET_BASIS, FNV1A64_PRIME};
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
    DisconnectedScreenState, MultiplayerScreenState, MultiplayerServerEntry, Screen, SettingsTab,
    UiAction, UiInputEvent, UiKey, UiSettings, UiState, VideoPreset, VisualStylePreset,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};

use crate::live_session::{
    CrosshairPick, LiveSession, PlayerMovement, PlayerPose, SessionPaths, SessionStatus,
};
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
/// Live entity/BER mesh rebuild cadence — the server mutates tracked
/// entity state at 20 Hz, so faster rebuilds re-mesh identical poses.
const LIVE_ENTITY_REFRESH: Duration = Duration::from_millis(50);
/// Stable dispatch slots mirroring the 12 [`VanillaBerKind`] families.
const BER_KIND_SLOTS: usize = 12;

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
    /// Armed whenever a session attaches: the next live frame on any
    /// `GpuRenderer` that has not seen a full strategy push applies the
    /// session's connect-time `ComputeStrategy` (GPU indirect/Hi-Z/MDI
    /// on capable adapters, fail-closed elsewhere).
    live_strategy_pending: bool,
    /// Pooled HUD draw list — `UiState::render_into` refills it in place
    /// so steady frames allocate nothing (text payloads recycle inside
    /// the list).
    ui_draw_list: UiDrawList,
    /// Per-frame live-particle staging scratch (tactile + ambience + VFX1
    /// billboards) — cleared and refilled only while particles exist.
    particle_scratch: Vec<VanillaParticleQuad>,
    /// `true` while the renderer's particle ring holds a non-empty
    /// upload — an all-clear frame stages `[]` once, then stays free.
    live_particles_staged: bool,
    /// Live entity + block-entity mesh staging cache (20 Hz throttle —
    /// the server tick cadence positions actually move at).
    entity_render_cache: LiveEntityRenderCache,
    /// Timestamp of the previous physical Space press — the vanilla
    /// ~7-tick double-jump window used by `LiveSession::toggle_flight`
    /// (key repeats are filtered upstream, so every `Pressed` is real).
    last_space_press: Option<Instant>,
    last_frame: Instant,
    frame_start: Instant,
    fatal: Option<String>,
}

/// Live entity + block-entity render cache: rig meshes keyed by entity
/// id + pose fingerprint, one canonical template per BER family, and the
/// throttle/staging bookkeeping that keeps uploads off the hot path.
struct LiveEntityRenderCache {
    /// Pose-fingerprint-keyed rig meshes; rebuilt only when an entity's
    /// spec or pose actually changes.
    entity_meshes: std::collections::HashMap<i32, (u64, ExtrudedMesh)>,
    /// Canonical template per [`VanillaBerKind`] — the default states are
    /// per-kind constants, so every placed block entity of a family
    /// shares one mesh.
    ber_meshes: [Option<ExtrudedMesh>; BER_KIND_SLOTS],
    /// Earliest next rebuild instant (20 Hz — the server tick cadence).
    next_refresh: Option<Instant>,
    /// `true` while the renderer's entity/BER ring holds a non-empty
    /// upload — a fully empty world still clears the ring once.
    staged: bool,
}

impl Default for LiveEntityRenderCache {
    fn default() -> Self {
        Self {
            entity_meshes: std::collections::HashMap::new(),
            ber_meshes: std::array::from_fn(|_| None),
            next_refresh: None,
            staged: false,
        }
    }
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
                UiSettings::default().video.view_distance as i8,
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
                    UiSettings::default().video.view_distance as i8,
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
            live_strategy_pending: true,
            ui_draw_list: UiDrawList::new(),
            particle_scratch: Vec::new(),
            live_particles_staged: false,
            entity_render_cache: LiveEntityRenderCache::default(),
            last_space_press: None,
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
        app.refresh_singleplayer_worlds();
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
        self.on_live_attached();
    }

    /// One-time per-attach wiring shared by `connect_to`/`attach_live`:
    /// arms the compute-strategy push the next live frame performs and
    /// replays the client-side debug log into the session's chat mirror —
    /// `sync_hud_to` replaces `ui.hud.chat_lines` wholesale on every HUD
    /// revision, so the log lives in `live.hud` where the sync carries it.
    fn on_live_attached(&mut self) {
        self.live_strategy_pending = true;
        let Some(live) = &mut self.live else {
            return;
        };
        live.hud
            .chat_lines
            .extend(self.debug_messages.iter().cloned());
        if live.hud.chat_lines.len() > 100 {
            let excess = live.hud.chat_lines.len() - 100;
            live.hud.chat_lines.drain(..excess);
        }
        live.hud_revision += 1;
    }

    /// Push the session's connect-time `ComputeStrategy` onto this
    /// renderer the first time it draws a live frame —
    /// `on_live_attached` arms `live_strategy_pending` and a fresh
    /// `ActiveWindow` re-arms it, so device re-creation re-applies
    /// uploads/workers/indirect policy. The strategy itself fails
    /// closed: GPU indirect/Hi-Z/MDI engage only when
    /// `indirect_execution_supported()` and the unified plan allow it.
    #[doc(hidden)]
    pub fn ensure_live_compute_strategy(
        live: &mut LiveSession,
        pending: &mut bool,
        renderer: &mut GpuRenderer,
    ) {
        if !*pending {
            return;
        }
        *pending = false;
        live.apply_compute_strategy(renderer);
    }

    /// `true` while the next live frame will push the compute strategy —
    /// armed by `on_live_attached` and every fresh `ActiveWindow`.
    #[doc(hidden)]
    pub fn live_strategy_pending(&self) -> bool {
        self.live_strategy_pending
    }

    /// Drive the pending strategy push for the attached session (test +
    /// headless-hook entry point; the render loop calls
    /// [`Self::ensure_live_compute_strategy`] directly so it can borrow
    /// the fields disjointly).
    #[doc(hidden)]
    pub fn push_pending_compute_strategy(&mut self, renderer: &mut GpuRenderer) {
        let pending = &mut self.live_strategy_pending;
        if let Some(live) = &mut self.live {
            Self::ensure_live_compute_strategy(live, pending, renderer);
        }
    }

    /// Rebuild the live entity + block-entity draw set: articulated rig
    /// meshes keyed by entity id + pose fingerprint (rebuilt only when
    /// the spec or pose actually changes), plus one canonical template
    /// per BER family placed at each tracked block-entity position.
    /// Throttled by the caller to the 20 Hz server cadence; an empty
    /// world costs only the two map scans and clears a stale upload
    /// exactly once.
    fn refresh_live_entity_and_ber(
        live: &LiveSession,
        cache: &mut LiveEntityRenderCache,
        renderer: &mut GpuRenderer,
    ) {
        cache
            .entity_meshes
            .retain(|eid, _| live.entity_rig(*eid).is_some());
        // Pass 1 mutates the caches (new/pose-changed meshes only); pass
        // 2 then borrows them immutably to assemble the draw list — the
        // renderer consumes `&ExtrudedMesh` refs so no mesh is cloned.
        let mut entity_refs: Vec<(i32, [f32; 3], [f32; 3])> = Vec::new();
        for (eid, spec, pos, tracked) in live.entity_render_entries() {
            let pose = live_entity_pose(live, eid, tracked);
            let key = entity_pose_key(spec, &pose);
            if cache
                .entity_meshes
                .get(&eid)
                .is_none_or(|(k, _)| *k != key)
            {
                cache
                    .entity_meshes
                    .insert(eid, (key, build_entity_rig_mesh(spec, pose)));
            }
            entity_refs.push((
                eid,
                [pos[0] as f32, pos[1] as f32, pos[2] as f32],
                live_entity_tint(spec, tracked),
            ));
        }
        let mut ber_refs: Vec<(usize, [f32; 3], [f32; 3])> = Vec::new();
        for (block_pos, be) in &live.world.block_entities {
            let Some(kind) = classify_tracked_block_entity(be) else {
                continue;
            };
            let slot = ber_kind_slot(kind);
            if cache.ber_meshes[slot].is_none() {
                cache.ber_meshes[slot] =
                    Some(build_ber_mesh(kind, default_vanilla_ber_state(kind)));
            }
            ber_refs.push((
                slot,
                [
                    block_pos[0] as f32,
                    block_pos[1] as f32,
                    block_pos[2] as f32,
                ],
                vanilla_ber_base_color_f32(kind),
            ));
        }
        let mut meshes: Vec<(&ExtrudedMesh, [f32; 3], [f32; 3])> =
            Vec::with_capacity(entity_refs.len() + ber_refs.len());
        for (eid, origin, tint) in entity_refs {
            if let Some((_, mesh)) = cache.entity_meshes.get(&eid) {
                meshes.push((mesh, origin, tint));
            }
        }
        for (slot, origin, tint) in ber_refs {
            if let Some(mesh) = cache.ber_meshes[slot].as_ref() {
                meshes.push((mesh, origin, tint));
            }
        }
        if meshes.is_empty() {
            if cache.staged {
                renderer.set_entity_and_ber_meshes(Vec::new());
                cache.staged = false;
            }
            return;
        }
        cache.staged = true;
        renderer.set_entity_and_ber_meshes(meshes);
    }

    /// Expand every live particle system (tactile sim, nature ambience,
    /// VFX1 particles) into camera-facing [`VanillaParticleQuad`]s in
    /// `scratch`, then stage them via
    /// [`GpuRenderer::set_particle_billboards`]. All systems empty ⇒ the
    /// ring is cleared exactly once and subsequent frames cost a few
    /// `is_empty` checks — zero wasted CPU when particles are off.
    fn stage_live_particles(
        live: &LiveSession,
        frame: &VfxFrameOutput,
        camera: &Camera,
        scratch: &mut Vec<VanillaParticleQuad>,
        staged: &mut bool,
        renderer: &mut GpuRenderer,
    ) {
        scratch.clear();
        let (right, up) =
            VanillaParticleWorld::billboard_axes(camera.right_vector(), camera.up_vector());
        for particle in live.tactile_sim().particles() {
            push_tactile_particle(scratch, particle, right, up);
        }
        for particle in live.nature_ambience.particles() {
            push_nature_particle(scratch, particle, right, up);
        }
        for particle in &frame.particles {
            push_vfx_particle(scratch, particle, right, up, camera.forward_vector());
        }
        if scratch.is_empty() {
            if *staged {
                renderer.set_particle_billboards(&[]);
                *staged = false;
            }
            return;
        }
        *staged = true;
        renderer.set_particle_billboards(scratch);
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
            UiAction::OpenScreen(screen) => {
                if *screen == Screen::Singleplayer {
                    self.refresh_singleplayer_worlds();
                }
                if *screen == Screen::Multiplayer {
                    self.refresh_multiplayer_servers();
                }
                self.ui.navigate(*screen);
            }
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
            UiAction::PlayWorld { name, seed } => {
                if let Err(err) = self.start_singleplayer_world(name, *seed) {
                    self.status_line(format!("Singleplayer {name:?}: {err}"));
                }
            }
            UiAction::StartMultiplayer => {
                if let Err(err) = self.join_multiplayer() {
                    self.status_line(format!("Multiplayer: {err}"));
                }
            }
            UiAction::JoinServer { address, port } => {
                self.connect_to(address.clone(), *port);
            }
            UiAction::SaveServer {
                name,
                address,
                port,
            } => {
                if let Err(err) = self.save_server_entry(name, address, *port) {
                    self.multiplayer_error(&err);
                }
            }
            UiAction::DeleteServer { index } => {
                if let Err(err) = self.delete_server_entry(*index) {
                    self.multiplayer_error(&err);
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
            UiAction::PerformRespawn => {
                // The death screen stays up until the server's
                // respawn/health recovery clears `live.death`; only the
                // packet dispatch happens here. A refused request
                // surfaces on the status line instead of failing silent.
                let sent = self
                    .live
                    .as_mut()
                    .is_some_and(|live| live.request_respawn());
                if !sent {
                    self.status_line("Respawn: request not available");
                }
            }
            UiAction::LeaveWorld => self.leave_world_for_title(),
            UiAction::RecipeBrowser(action) => self.dispatch_recipe_browser_action(action),
            _ => {}
        }
    }

    /// Death-screen `Title Screen` button (also the only escape on
    /// hardcore deaths): drop the live session — and the integrated
    /// `megu` child when it hosted the world — plus every in-game
    /// overlay. The UI already navigated to `Screen::Title` inside
    /// `activate`.
    fn leave_world_for_title(&mut self) {
        self.live = None;
        self.singleplayer_server = None;
        self.container_screen = None;
        self.chat_bar = None;
        self.advancements_open = false;
        self.paused = false;
        self.keys = Keys::default();
        self.maybe_regrab();
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
            // Render-distance changes while connected re-announce
            // `client_information` so the server re-centers the chunk
            // view (no-op unless the value actually moved).
            live.resend_client_information(video.view_distance as i8);
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
    /// the message lands in chat both offline and live, is mirrored to
    /// stderr, and stays visible on the Title/Singleplayer screens via
    /// `ui.title_status` (their layouts never read `hud.chat_lines`).
    fn status_line(&mut self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("[{}] {message}", crate::CLIENT_GPU_NAME);
        self.ui.title_status = Some(message.clone());
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
            if live.hud.chat_lines.len() > 100 {
                let excess = live.hud.chat_lines.len() - 100;
                live.hud.chat_lines.drain(..excess);
            }
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
            self.ui.settings.video.view_distance as i8,
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
        self.on_live_attached();
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
        cli.seed = config.seed;
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
        self.start_singleplayer_world(&world, None)
    }

    /// Singleplayer world-select/"Create & Play": launch the integrated
    /// server for `world` (fail-closed `validate_world_name` inside
    /// `integrated::launch`), honoring the UI seed when given and
    /// falling back to `--seed` otherwise, then connect to it.
    fn start_singleplayer_world(&mut self, world: &str, seed: Option<i64>) -> Result<(), String> {
        let mut config = self.config.clone();
        config.seed = seed.or(config.seed);
        let server = Self::launch_singleplayer(&config, world)?;
        let addr = server.addr();
        self.singleplayer_server = Some(server);
        self.connect_to(addr.ip().to_string(), addr.port());
        Ok(())
    }

    /// Refresh the Singleplayer screen's world list from
    /// `<gameDir>/saves/` (`.` when `--gameDir` is unset), sorted
    /// deterministically.
    fn refresh_singleplayer_worlds(&mut self) {
        self.ui.singleplayer.worlds = list_saved_worlds(self.config.game_dir.as_deref());
    }

    /// The Multiplayer screen's server-list store under `<gameDir>`
    /// (`.` when `--gameDir` is unset) — loaded fresh for every refresh
    /// and every mutation so `SaveServer`/`DeleteServer` always persist
    /// through the store's atomic commit instead of a stale snapshot.
    fn server_list_store(&self) -> Result<ServerListStore, String> {
        let dir = self
            .config
            .game_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        ServerListStore::load(dir)
    }

    /// Repopulate `ui.multiplayer.servers` from the store on screen open
    /// and after every mutation; store failures land in the screen's
    /// status banner rather than dropping silently.
    fn refresh_multiplayer_servers(&mut self) {
        let loaded = self.server_list_store();
        let multiplayer: &mut MultiplayerScreenState = &mut self.ui.multiplayer;
        match loaded {
            Ok(store) => {
                multiplayer.servers = store
                    .entries()
                    .iter()
                    .map(|entry| MultiplayerServerEntry {
                        name: entry.name.clone(),
                        address: entry.address.clone(),
                        port: entry.port,
                    })
                    .collect();
                if multiplayer
                    .selected
                    .is_some_and(|index| index >= multiplayer.servers.len())
                {
                    multiplayer.selected = None;
                }
            }
            Err(err) => {
                multiplayer.status = Some(err.clone());
                self.status_line(format!("Multiplayer: {err}"));
            }
        }
    }

    /// `UiAction::SaveServer`: append the direct-connect endpoint through
    /// the store (validation + atomic persistence inside), then refresh
    /// the visible rows so the UI mirrors exactly what was committed.
    fn save_server_entry(&mut self, name: &str, address: &str, port: u16) -> Result<(), String> {
        let mut store = self.server_list_store()?;
        store.add(ServerEntry {
            name: name.to_owned(),
            address: address.to_owned(),
            port,
        })?;
        self.refresh_multiplayer_servers();
        self.ui.multiplayer.status = Some(format!("Saved {name}"));
        Ok(())
    }

    /// `UiAction::DeleteServer`: remove row `index` through the store,
    /// then refresh; a stale row selection clears itself on rebuild.
    fn delete_server_entry(&mut self, index: usize) -> Result<(), String> {
        let mut store = self.server_list_store()?;
        let removed = store.remove(index)?;
        self.refresh_multiplayer_servers();
        self.ui.multiplayer.status = Some(format!("Deleted {}", removed.name));
        Ok(())
    }

    /// Surface a server-list/store failure on the Multiplayer screen's
    /// status banner (and the global status line for logs/chat).
    fn multiplayer_error(&mut self, err: &str) {
        self.ui.multiplayer.status = Some(err.to_owned());
        self.status_line(format!("Multiplayer: {err}"));
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
        // Fresh `GpuRenderer` (first `resumed`, device loss, surface
        // re-create): re-arm the compute-strategy push so the next live
        // frame re-applies indirect/Hi-Z/MDI policy to the new device.
        self.live_strategy_pending = true;
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
            properties: std::collections::BTreeMap<i16, i16>,
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
                    properties: open.properties.clone(),
                })
            });
        // Phase 2: mutate `self.container_screen` / grab state freely.
        match snap {
            Some(Snapshot {
                schema,
                window_id,
                state_id,
                properties,
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
                // `container_set_data` updates (furnace burn/cook bars,
                // brewing progress, enchant hints) carry no `state_id` —
                // mirror the whole property map every frame so a mid-open
                // update reaches the rendered bars.
                screen.properties.clone_from(&properties);
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
    /// Grabbed in-game world clicks — vanilla `startAttack`/`startUseItem`
    /// crosshair dispatch: an entity hit wins only when nearer than the
    /// traced block, then dig/interact falls back to block/air.
    fn world_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        if self.ui_wants_input() {
            return;
        }
        let Some(live) = &mut self.live else { return };
        if live.is_dead() {
            return;
        }
        match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => {
                match live.crosshair_pick() {
                    Some(CrosshairPick::Entity(hit)) => {
                        live.attack_entity(hit.entity_id);
                    }
                    Some(CrosshairPick::Block(hit)) => {
                        live.dig_start(hit.block_pos, hit.face as i8);
                    }
                    None => {}
                }
                live.swing(0);
            }
            (MouseButton::Left, ElementState::Released) => {
                // Releasing mid-dig aborts at the current target, like
                // vanilla's `continueDestroy` key-release path.
                if let Some(CrosshairPick::Block(hit)) = live.crosshair_pick() {
                    live.dig_abort(hit.block_pos, hit.face as i8);
                }
            }
            (MouseButton::Right, ElementState::Pressed) => {
                match live.crosshair_pick() {
                    Some(CrosshairPick::Entity(hit)) => {
                        live.interact_entity(
                            hit.entity_id,
                            Some([
                                hit.hit_pos[0] as f32,
                                hit.hit_pos[1] as f32,
                                hit.hit_pos[2] as f32,
                            ]),
                        );
                    }
                    Some(CrosshairPick::Block(hit)) => {
                        live.use_item_on(
                            0,
                            hit.block_pos,
                            i32::from(hit.face as u8),
                            hit.cursor_in_block,
                            hit.inside,
                        );
                    }
                    None => {
                        live.use_item(0);
                    }
                }
                live.swing(0);
            }
            _ => {}
        }
    }

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

    /// Death loop: `player_combat_kill`/`set_health(<=0)` flipped
    /// `live.death` dead -> put the death screen up (refreshing the
    /// cause/score line while it stays up); `respawn`/`set_health(>0)`
    /// clears it back onto the HUD. The screen replaces
    /// pause/inventory/chat overlays — those belong to the living
    /// player. Called from `render_frame` before the occlusion
    /// early-return so a hidden frame can never miss the transition.
    fn sync_death_screen(&mut self) {
        let Some(live) = &mut self.live else {
            return;
        };
        match live.death_screen_state() {
            Some(state) => {
                if self.ui.screen == Screen::Death {
                    self.ui.update_death(state);
                } else {
                    self.ui.show_death(state);
                    self.paused = false;
                    self.container_screen = None;
                    self.chat_bar = None;
                    self.advancements_open = false;
                    self.keys = Keys::default();
                    self.maybe_regrab();
                }
            }
            None if self.ui.screen == Screen::Death => {
                self.ui.clear_death();
                self.maybe_regrab();
            }
            None => {}
        }
    }

    /// `PlayEvent::Disconnect` / an unexpected session drop: extract the
    /// exact reason, drop the live session and every in-game overlay,
    /// and land on `Screen::Disconnected` with the reason on screen.
    /// Called after `live.poll` each frame so kicks surface within one
    /// frame; the pause/container/chat/cursor state cannot survive it.
    fn sync_disconnected_screen(&mut self) {
        let reason = match self.live.as_ref().map(|live| &live.status) {
            Some(SessionStatus::Disconnected(reason)) => reason.clone(),
            _ => return,
        };
        self.live = None;
        self.singleplayer_server = None;
        self.container_screen = None;
        self.own_inventory_open = false;
        self.chat_bar = None;
        self.advancements_open = false;
        self.paused = false;
        self.keys = Keys::default();
        self.ui.show_disconnected(DisconnectedScreenState {
            title: "Disconnected".to_owned(),
            reason,
        });
        debug_assert_eq!(self.ui.screen, Screen::Disconnected);
        self.maybe_regrab();
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
        self.sync_disconnected_screen();
        self.sync_death_screen();
        self.sync_container_screen();

        let player_dead = self.live.as_ref().is_some_and(|live| live.is_dead());
        // Camera movement only applies while unpaused and alive — a dead
        // player keeps its death pose until the server's respawn teleport
        // lands via `camera_sync`.
        if !self.paused && !player_dead {
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
                sneak: !self.paused && self.keys.down,
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

        let mut goggles_draw: Option<UiDrawList> = None;
        if let Some(live) = &mut self.live {
            active.renderer.set_player_position(self.camera.position);
            // Off-thread/priority-0 mesh payloads the session published
            // this poll ride the reconcile below — the renderer treats
            // them as authoritative until the world derives its own. The
            // revision-gated delta sync replaces the old full-map clone:
            // steady frames compare one u64 and apply nothing.
            active.renderer.sync_external_sections_from(live);
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
            live.set_ui_locale(self.ui.settings.locale.code());
            let feed = live.hud_feed();
            Self::apply_live_hud(
                &mut self.ui,
                &mut self.title_seen_rev,
                &mut self.title_tick_accum,
                dt,
                &feed,
            );
            // Engineer's-Goggles card: when the crosshair rests on a
            // kinetic-synced Create block, build the live telemetry
            // panel (RPM, Stress SU, overstressed warning, processing
            // bar) for compositing into this frame's HUD overlay.
            goggles_draw = if self.ui.screen == Screen::InGameHud {
                live.looking_at_block
                    .as_ref()
                    .map(|(pos, _)| *pos)
                    .and_then(|pos| live.goggles_telemetry_at((pos[0], pos[1], pos[2])))
                    .map(|telemetry| {
                        megu_ui::create_goggles_hud::GogglesHudLayout::build(
                            &telemetry,
                            w,
                            h,
                            self.ui.settings.locale.code(),
                        )
                    })
            } else {
                None
            };
            if live.asset_bundle_pending_upload {
                // Dropping the CPU pixel payloads is part of the upload:
                // after this call VRAM owns the sole copy of the atlas and
                // `live.pending_atlas_pixels` is `None` (`atlas_meta`
                // stays resident).
                live.upload_pending_atlas(&mut active.renderer);
            }
            // `.mcmeta` playback: one `SpriteFrame` rewrite per animated
            // sprite, and only when the client-side 20 Hz clock advanced
            // inside `poll` — steady frames cost one compare.
            active.renderer.tick_atlas_animations(live.atlas_anim_tick());
            active.renderer.sync_subworlds(&mut live.subworlds);
            // First live frame on this renderer (attach order or
            // surface/device re-creation): push the session's
            // connect-time `ComputeStrategy`. `apply_compute_strategy`
            // engages GPU indirect/Hi-Z/MDI only where the adapter
            // proves indirect+compute support and the unified plan
            // allows it; everything else fails closed to the
            // classic/bundle path.
            Self::ensure_live_compute_strategy(
                live,
                &mut self.live_strategy_pending,
                &mut active.renderer,
            );
            // Live entities + block-entity meshes ride the dynamic ring;
            // re-staged at most at the 20 Hz cadence the server actually
            // mutates tracked state, skipped outright while nothing is
            // renderable.
            if self
                .entity_render_cache
                .next_refresh
                .is_none_or(|t| frame_begin >= t)
            {
                self.entity_render_cache.next_refresh =
                    Some(frame_begin + LIVE_ENTITY_REFRESH);
                Self::refresh_live_entity_and_ber(
                    live,
                    &mut self.entity_render_cache,
                    &mut active.renderer,
                );
            }
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
            // Chat/debug lines already live in `live.hud` (chat packets,
            // `status_line`, debug-chord feedback) and `sync_hud_to`
            // mirrors them into `ui.hud` on revision bumps — re-extending
            // `debug_messages` here every frame was an unbounded
            // duplication at render rate.
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
        // Pooled draw list (C4): `render_into` clears + refills
        // `self.ui_draw_list` in place so steady frames allocate
        // nothing (text payloads recycle inside the list). The command
        // stream is identical to `self.ui.render(w, h)` — the
        // fingerprint below feeds the same dirty-check either way.
        self.ui.render_into(w, h, &mut self.ui_draw_list);
        let draw_list = &mut self.ui_draw_list;
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
        // Live goggles telemetry composites beside the crosshair on the
        // in-game HUD only (hidden behind menus/containers by the
        // `Screen::InGameHud` gate where `goggles_draw` was produced).
        if let Some(goggles) = goggles_draw {
            draw_list.commands.extend(goggles.commands);
        }
        // Server-driven container screen draws above the HUD.
        if let Some(screen) = &self.container_screen {
            screen.render(w as f32, h as f32, draw_list);
        }
        if self.advancements_open {
            Self::render_advancements_overlay(
                self.live.as_ref(),
                w as i32,
                h as i32,
                draw_list,
            );
        }
        // Dirty-only upload: unchanged HUD frames skip even the quad
        // tessellation; changed frames upload only the small dynamic
        // vertex/index buffers — never a `w*h*4` CPU raster + texture
        // write (`renderer.ui_cpu_raster_uploads` stays at zero).
        let fingerprint = fingerprint_draw_list(w, h, draw_list);
        if self.ui_cache.should_upload(w, h, fingerprint) {
            let mesh = tessellate_draw_list(w, h, draw_list);
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
                        // One `frame_output` snapshot feeds both the
                        // light UBO (`dynamic_colored_lights` did the
                        // same call and discarded the rest) and the
                        // particle staging below.
                        let vfx_frame =
                            live.vfx.frame_output(render_camera.forward_vector());
                        Self::stage_live_particles(
                            live,
                            &vfx_frame,
                            &render_camera,
                            &mut self.particle_scratch,
                            &mut self.live_particles_staged,
                            &mut active.renderer,
                        );
                        active.renderer.render_to_view_with_env(
                            &view,
                            w,
                            h,
                            &sky,
                            &live.atmosphere_blender,
                            &live.chunk_fades,
                            live.world_time_ticks(),
                            &vfx_frame.lights,
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
            KeyCode::Space => {
                self.keys.up = pressed;
                if pressed && !self.paused {
                    if let Some(live) = &mut self.live {
                        // Vanilla creative double-jump: a second Space
                        // press inside ~7 ticks toggles flight instead of
                        // re-jumping/elytra. `toggle_flight` no-ops
                        // without the server's `may_fly` grant, so normal
                        // jump behavior is preserved outside creative.
                        let double_tap = self
                            .last_space_press
                            .is_some_and(|t| t.elapsed() <= Duration::from_millis(350));
                        self.last_space_press = Some(Instant::now());
                        if !(double_tap && live.toggle_flight()) {
                            live.try_start_elytra();
                        }
                    }
                }
            }
            KeyCode::ShiftLeft | KeyCode::ShiftRight => {
                // Sneak transitions ride `player_command` (0 start /
                // 1 stop) — emitted once per edge, not per frame.
                if pressed != self.keys.down {
                    self.keys.down = pressed;
                    if !self.paused {
                        if let Some(live) = &mut self.live {
                            live.player_command(if pressed { 0 } else { 1 });
                        }
                    }
                }
            }
            KeyCode::ControlLeft | KeyCode::ControlRight => {
                // Sprint transitions ride `player_command` (3 start /
                // 4 stop) — emitted once per edge.
                if pressed != self.keys.sprint {
                    self.keys.sprint = pressed;
                    if !self.paused {
                        if let Some(live) = &mut self.live {
                            live.player_command(if pressed { 3 } else { 4 });
                        }
                    }
                }
            }
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
                        if let Some(live) = &mut self.live {
                            live.hud.chat_lines.push(message.clone());
                            live.hud_revision += 1;
                        } else {
                            self.ui.hud.chat_lines.push(message.clone());
                        }
                        self.debug_messages.push(message);
                    }
                    DebugChordAction::HelpLines(lines) => {
                        if let Some(live) = &mut self.live {
                            live.hud.chat_lines.extend(lines.iter().cloned());
                            live.hud_revision += 1;
                        } else {
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
                if self.ui.hud.chat_lines.len() > 100 {
                    let excess = self.ui.hud.chat_lines.len() - 100;
                    self.ui.hud.chat_lines.drain(..excess);
                }
                if let Some(live) = &mut self.live {
                    if live.hud.chat_lines.len() > 100 {
                        let excess = live.hud.chat_lines.len() - 100;
                        live.hud.chat_lines.drain(..excess);
                    }
                }
            }
            WindowInputEvent::SelectHotbar(slot) => {
                self.ui.hud.selected_slot = usize::from(slot);
                if let Some(live) = &mut self.live {
                    live.hud.selected_slot = usize::from(slot);
                    live.hud_revision += 1;
                    live.set_carried_item(i32::from(slot));
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
                } else if self.cursor_grabbed && !self.paused {
                    // Grabbed in-game clicks: crosshair dig/place/attack
                    // dispatch into the live session.
                    self.world_mouse_input(state, button);
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
            WindowEvent::MouseWheel { delta, .. } => {
                // Grabbed scroll cycles the held hotbar slot (vanilla
                // `mouseScrolling$scrollHotbar`).
                if self.cursor_grabbed && !self.paused && self.live.is_some() {
                    let lines = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                        winit::event::MouseScrollDelta::PixelDelta(pos) => {
                            (pos.y / 40.0) as f32
                        }
                    };
                    let steps = lines.round() as i32;
                    if steps != 0 {
                        let current = i32::from(self.input_router.active_hotbar_slot);
                        let next = (current - steps).rem_euclid(9) as u8;
                        self.apply_window_input(WindowInputEvent::SelectHotbar(next));
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

/// World folder names under `<gameDir>/saves/` — the Singleplayer
/// screen's world list. `game_dir` defaults to `.` like the launcher's
/// `--gameDir` fallback; missing/unreadable `saves/` yields an empty
/// list (the "Create New World" path still works). Sorted by name for
/// deterministic presentation.
fn list_saved_worlds(game_dir: Option<&std::path::Path>) -> Vec<String> {
    let saves = game_dir
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("saves");
    let mut worlds: Vec<String> = match std::fs::read_dir(&saves) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_type()
                    .map(|kind| kind.is_dir())
                    .unwrap_or(false)
            })
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    worlds.sort();
    worlds
}

/// Deterministic per-session splash seed: FNV-1a over the launch
/// config's identity fields (`username`/`--version`/`--gameDir`) — zero
/// `SystemTime`, stable across the whole run.
fn splash_seed_for(config: &GpuCliConfig) -> u64 {
    let mut hash = Fnv1a64::new();
    hash.update(config.username.as_bytes());
    if let Some(id) = config.version_id.as_deref() {
        hash.update_str(id);
    }
    if let Some(dir) = config.game_dir.as_deref() {
        hash.update(dir.to_string_lossy().as_bytes());
    }
    hash.finish()
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

// ---------------------------------------------------------------------------
// Live entity/BER + particle staging helpers
// ---------------------------------------------------------------------------

/// Stable dispatch slot per [`VanillaBerKind`] for the template cache.
fn ber_kind_slot(kind: VanillaBerKind) -> usize {
    match kind {
        VanillaBerKind::Chest => 0,
        VanillaBerKind::Sign => 1,
        VanillaBerKind::Banner => 2,
        VanillaBerKind::Beacon => 3,
        VanillaBerKind::Bed => 4,
        VanillaBerKind::Skull => 5,
        VanillaBerKind::ItemFrame => 6,
        VanillaBerKind::Campfire => 7,
        VanillaBerKind::BrewingStand => 8,
        VanillaBerKind::Lectern => 9,
        VanillaBerKind::DecoratedPot => 10,
        VanillaBerKind::ShulkerBox => 11,
    }
}

/// Classify a tracked block entity for the BER pass: the NBT `id`
/// string is authoritative (chunk payloads carry it), falling back to
/// the protocol-767 `BuiltInRegistries.BLOCK_ENTITY_TYPE` wire id when
/// the payload omits it (bare `block_entity_data` updates).
fn classify_tracked_block_entity(be: &TrackedBlockEntity) -> Option<VanillaBerKind> {
    if let Ok(NbtValue::Compound(map)) = WireReader::new(&be.nbt_bytes).read_nbt() {
        if let Some(NbtValue::String(id)) = map.get("id") {
            if let Some(kind) = classify_vanilla_ber(id) {
                return Some(kind);
            }
        }
    }
    ber_kind_for_wire_id(be.kind)
}

/// Wire-id fallback — `BuiltInRegistries.BLOCK_ENTITY_TYPE` ids in
/// 1.21.1 for the renderable families (the same numbering the server's
/// `live_block_entity_wire_id` pins: sign=7, beacon=14, lectern=29,
/// campfire=32, decorated_pot=40). Item frames are entities, not block
/// entities, so that family only ever arrives via the NBT name path.
fn ber_kind_for_wire_id(kind: i32) -> Option<VanillaBerKind> {
    Some(match kind {
        1..=3 => VanillaBerKind::Chest,
        7 | 8 => VanillaBerKind::Sign,
        19 => VanillaBerKind::Banner,
        14 => VanillaBerKind::Beacon,
        24 => VanillaBerKind::Bed,
        15 => VanillaBerKind::Skull,
        32 => VanillaBerKind::Campfire,
        11 => VanillaBerKind::BrewingStand,
        29 => VanillaBerKind::Lectern,
        40 => VanillaBerKind::DecoratedPot,
        23 => VanillaBerKind::ShulkerBox,
        _ => return None,
    })
}

/// Canonical idle state per BER family — the same defaults the software
/// renderer's `default_vanilla_ber_state` uses, so block entities look
/// identical across the two paths.
fn default_vanilla_ber_state(kind: VanillaBerKind) -> VanillaBerState {
    match kind {
        VanillaBerKind::Chest => VanillaBerState::Chest {
            lid_open: 0.0,
            double: false,
            facing: VanillaBerFacing::South,
        },
        VanillaBerKind::Sign => VanillaBerState::Sign {
            wall: false,
            hanging: false,
            glowing: false,
            facing_yaw: 0.0,
        },
        VanillaBerKind::Banner => VanillaBerState::Banner {
            wall: false,
            sway_rad: 0.0,
            facing_yaw: 0.0,
        },
        VanillaBerKind::Beacon => VanillaBerState::Beacon {
            beam_height: 8.0,
            active: true,
            rgb: [0.35, 0.6, 1.0],
        },
        VanillaBerKind::Bed => VanillaBerState::Bed {
            is_head: false,
            facing: VanillaBerFacing::South,
        },
        VanillaBerKind::Skull => VanillaBerState::Skull {
            wall: false,
            yaw_deg: 0.0,
        },
        VanillaBerKind::ItemFrame => VanillaBerState::ItemFrame {
            facing: VanillaBerFacing::South,
            glow: false,
            rotation: 0,
        },
        VanillaBerKind::Campfire => VanillaBerState::Campfire {
            lit: true,
            item_count: 2,
        },
        VanillaBerKind::BrewingStand => VanillaBerState::BrewingStand {
            bottle_mask: 0b111,
        },
        VanillaBerKind::Lectern => VanillaBerState::Lectern {
            has_book: false,
            facing: VanillaBerFacing::South,
        },
        VanillaBerKind::DecoratedPot => VanillaBerState::DecoratedPot {
            facing: VanillaBerFacing::South,
            wobble: 0.0,
        },
        VanillaBerKind::ShulkerBox => VanillaBerState::ShulkerBox {
            open_progress: 0.0,
            facing: VanillaBerFacing::South,
        },
    }
}

/// Deterministic albedo per BER family — the software renderer's
/// `vanilla_ber_base_color` byte palette in `0..=1` tint form.
fn vanilla_ber_base_color_f32(kind: VanillaBerKind) -> [f32; 3] {
    let [r, g, b]: [u8; 3] = match kind {
        VanillaBerKind::Chest => [162, 116, 63],
        VanillaBerKind::Sign => [172, 134, 82],
        VanillaBerKind::Banner => [196, 62, 62],
        VanillaBerKind::Beacon => [214, 214, 214],
        VanillaBerKind::Bed => [178, 58, 58],
        VanillaBerKind::Skull => [216, 216, 208],
        VanillaBerKind::ItemFrame => [146, 104, 58],
        VanillaBerKind::Campfire => [104, 72, 48],
        VanillaBerKind::BrewingStand => [88, 70, 108],
        VanillaBerKind::Lectern => [138, 98, 58],
        VanillaBerKind::DecoratedPot => [168, 138, 108],
        VanillaBerKind::ShulkerBox => [138, 92, 158],
    };
    [
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    ]
}

/// Pose for one live entity: relative head yaw from the tracked packet
/// state, hurt flash while `hurt_ticks` is hot, and the metadata-driven
/// body poses vanilla exposes (`sneak`/`swim`/`elytra`).
fn live_entity_pose(
    live: &LiveSession,
    entity_id: i32,
    tracked: Option<&TrackedEntity>,
) -> EntityPose {
    let mut pose = EntityPose {
        head_yaw_deg: tracked.map_or(0.0, |t| t.head_yaw_deg - t.yaw_deg),
        head_pitch_deg: tracked.map_or(0.0, |t| t.pitch_deg),
        hurt_flash: tracked.is_some_and(|t| t.hurt_ticks > 0),
        ..EntityPose::default()
    };
    if let Some(meta) = live.entity_metadata.get(&entity_id) {
        // Vanilla pose ids: 1 fall-flying, 3 swimming, 5 sneaking; the
        // shared flags byte keeps the sneak/elytra bits too.
        pose.swimming = meta.pose == Some(3);
        pose.sneaking = meta.pose == Some(5) || meta.flags.is_some_and(|f| f & 0x02 != 0);
        pose.elytra_flying =
            meta.pose == Some(1) || meta.flags.is_some_and(|f| f & 0x80 != 0);
    }
    pose
}

/// Fingerprint of `(spec, pose)` so rig meshes rebuild only on real
/// change — floats quantize to 1/64 to keep jitter from thrashing the
/// cache.
fn entity_pose_key(spec: EntityRigSpec, pose: &EntityPose) -> u64 {
    // Word-wise FNV-1a step (`hash ^= word; hash *= prime`) — not the
    // byte-wise `Fnv1a64` stream, kept bit-identical to the historical
    // digest; only the offset basis and prime come from `megu_core`.
    let mut hash = FNV1A64_OFFSET_BASIS;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    };
    mix(u64::from(spec.entity_type_id));
    for v in [
        pose.limb_swing,
        pose.limb_swing_amount,
        pose.head_yaw_deg,
        pose.head_pitch_deg,
        pose.swing_progress,
        pose.age_ticks,
    ] {
        mix((f64::from(v) * 64.0).round() as i64 as u64);
    }
    mix(u64::from(pose.sneaking)
        | u64::from(pose.swimming) << 1
        | u64::from(pose.elytra_flying) << 2
        | u64::from(pose.hurt_flash) << 3
        | u64::from(pose.baby) << 4
        | u64::from(pose.enable_skin_3d_layers) << 5);
    hash
}

/// Per-entity tint: hurt flash overrides, otherwise the golden-ratio
/// hue keyed on the rig's type id (the same convention the software
/// renderer uses for its entity palette).
fn live_entity_tint(spec: EntityRigSpec, tracked: Option<&TrackedEntity>) -> [f32; 3] {
    if tracked.is_some_and(|t| t.hurt_ticks > 0) {
        return [1.0, 0.45, 0.45];
    }
    let hue = (f64::from(spec.entity_type_id) * 0.618_033_988_749_895).fract() as f32;
    hsv_to_rgb_f32(hue, 0.5, 0.85)
}

/// `hsv_to_rgb` for `0..=1` float inputs.
fn hsv_to_rgb_f32(h: f32, s: f32, v: f32) -> [f32; 3] {
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [r, g, b]
}

/// Push one quad — center + two half-edge vectors — in the corner order
/// and unit-square UV basis [`VanillaParticleWorld::billboard_quads`]
/// emits, so instanced packing (`pack_particle_billboard`) accepts it.
/// `axis_u`/`axis_v` may be any parallelogram edges: camera billboards,
/// flat ground rings, or velocity-stretched streaks.
fn push_particle_quad(
    quads: &mut Vec<VanillaParticleQuad>,
    pos: [f32; 3],
    axis_u: [f32; 3],
    axis_v: [f32; 3],
    color: [f32; 4],
) {
    if !pos.iter().all(|v| v.is_finite()) || !color.iter().all(|v| v.is_finite()) {
        return;
    }
    let positions = [
        [
            pos[0] - axis_u[0] - axis_v[0],
            pos[1] - axis_u[1] - axis_v[1],
            pos[2] - axis_u[2] - axis_v[2],
        ],
        [
            pos[0] + axis_u[0] - axis_v[0],
            pos[1] + axis_u[1] - axis_v[1],
            pos[2] + axis_u[2] - axis_v[2],
        ],
        [
            pos[0] + axis_u[0] + axis_v[0],
            pos[1] + axis_u[1] + axis_v[1],
            pos[2] + axis_u[2] + axis_v[2],
        ],
        [
            pos[0] - axis_u[0] + axis_v[0],
            pos[1] - axis_u[1] + axis_v[1],
            pos[2] - axis_u[2] + axis_v[2],
        ],
    ];
    if positions.iter().flatten().all(|v| v.is_finite()) {
        quads.push(VanillaParticleQuad {
            kind: megu_client_mesh::ber_instancing::VanillaParticleKind::PotionSwirl,
            positions,
            uv: [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
            color,
        });
    }
}

/// Camera-facing quad for a point particle of half-size `half`.
fn push_billboard(
    quads: &mut Vec<VanillaParticleQuad>,
    pos: [f32; 3],
    right: [f32; 3],
    up: [f32; 3],
    half: f32,
    color: [f32; 4],
) {
    if !half.is_finite() || half <= 0.0 {
        return;
    }
    push_particle_quad(
        quads,
        pos,
        right.map(|r| r * half),
        up.map(|u| u * half),
        color,
    );
}

/// Horizontal quad lying on the XZ plane (water ripples, dust rings,
/// ground decals).
fn push_flat_ring(
    quads: &mut Vec<VanillaParticleQuad>,
    pos: [f32; 3],
    half: f32,
    color: [f32; 4],
) {
    if !half.is_finite() || half <= 0.0 {
        return;
    }
    push_particle_quad(quads, pos, [half, 0.0, 0.0], [0.0, 0.0, half], color);
}

/// Tactile sim particle → quad. `size` is the sim's radius; ring kinds
/// lie flat on XZ (they expand on water/ground surfaces), everything
/// else is a camera billboard.
fn push_tactile_particle(
    quads: &mut Vec<VanillaParticleQuad>,
    particle: &TactileParticle,
    right: [f32; 3],
    up: [f32; 3],
) {
    let half = particle.size.max(0.005);
    let flat = matches!(
        particle.kind,
        TactileParticleKind::WaterRippleRing { .. }
            | TactileParticleKind::BubblePopRing { .. }
            | TactileParticleKind::LandingDustRing { .. }
            | TactileParticleKind::DripLandingRing { .. }
            | TactileParticleKind::ShockRing { .. }
    );
    if flat {
        push_flat_ring(quads, particle.pos, half, particle.rgba);
    } else {
        push_billboard(quads, particle.pos, right, up, half, particle.rgba);
    }
}

fn push_nature_particle(
    quads: &mut Vec<VanillaParticleQuad>,
    particle: &NatureParticle,
    right: [f32; 3],
    up: [f32; 3],
) {
    // Gentle tail fade so expiring particles do not pop; the sim owns
    // TTL expiry either way.
    let life = (particle.age_sec / particle.ttl_sec.max(1e-3)).clamp(0.0, 1.0);
    let fade = 1.0 - life * life;
    let (mut color, half, flat) = match particle.kind {
        NatureParticleKind::FallingLeaf { rgb, .. } => {
            ([rgb[0], rgb[1], rgb[2], 0.85], 0.05, false)
        }
        NatureParticleKind::SakuraPetal { rgb, .. } => {
            ([rgb[0], rgb[1], rgb[2], 0.85], 0.045, false)
        }
        NatureParticleKind::Firefly { glow_rgb, .. } => {
            ([glow_rgb[0], glow_rgb[1], glow_rgb[2], 0.9], 0.03, false)
        }
        NatureParticleKind::RainRipple { radius, max_radius } => {
            let alpha = 0.45 * (1.0 - radius / max_radius.max(1e-3)).clamp(0.0, 1.0);
            ([0.70, 0.82, 0.95, alpha], radius.max(0.01), true)
        }
        NatureParticleKind::WaterfallMist { alpha } => {
            ([0.9, 0.93, 1.0, alpha.clamp(0.0, 1.0) * 0.6], 0.15, false)
        }
        NatureParticleKind::SnowMote { .. } => ([0.92, 0.95, 1.0, 0.8], 0.03, false),
    };
    color[3] *= fade;
    if flat {
        push_flat_ring(quads, particle.pos, half, color);
    } else {
        push_billboard(quads, particle.pos, right, up, half, color);
    }
}

/// VFX1 `ParticleDraw` → quad honoring the recorded orientation:
/// camera billboards apply the in-plane `rotation`, velocity-stretched
/// particles become streaks along their motion, and flat decals lie on
/// XZ. `AxisAligned` has no fixed axis stored on the draw, so it shares
/// the camera billboard path.
fn push_vfx_particle(
    quads: &mut Vec<VanillaParticleQuad>,
    particle: &ParticleDraw,
    right: [f32; 3],
    up: [f32; 3],
    forward: [f32; 3],
) {
    let half = particle.size.max(0.005);
    match particle.orientation {
        ParticleOrientation::FlatGroundDecal => {
            push_flat_ring(quads, particle.pos, half, particle.rgba);
        }
        ParticleOrientation::VelocityStretched => {
            let speed = (particle.velocity[0] * particle.velocity[0]
                + particle.velocity[1] * particle.velocity[1]
                + particle.velocity[2] * particle.velocity[2])
            .sqrt();
            if speed > 1e-4 {
                // Streak axis along the velocity (longer when faster);
                // the cross axis stays as camera-perpendicular as the
                // motion allows.
                let axis_u = particle
                    .velocity
                    .map(|v| v / speed * (half + speed * 0.5));
                let cx = axis_u[1] * forward[2] - axis_u[2] * forward[1];
                let cy = axis_u[2] * forward[0] - axis_u[0] * forward[2];
                let cz = axis_u[0] * forward[1] - axis_u[1] * forward[0];
                let len = (cx * cx + cy * cy + cz * cz).sqrt();
                let axis_v = if len > 1e-6 {
                    [cx / len * half, cy / len * half, cz / len * half]
                } else {
                    up.map(|u| u * half)
                };
                push_particle_quad(quads, particle.pos, axis_u, axis_v, particle.rgba);
            } else {
                push_billboard(quads, particle.pos, right, up, half, particle.rgba);
            }
        }
        _ => {
            // CameraBillboard + AxisAligned: rotate the billboard axes
            // in-plane by `rotation`.
            let (sin, cos) = particle.rotation.sin_cos();
            let r2 = [
                right[0] * cos + up[0] * sin,
                right[1] * cos + up[1] * sin,
                right[2] * cos + up[2] * sin,
            ];
            let u2 = [
                up[0] * cos - right[0] * sin,
                up[1] * cos - right[1] * sin,
                up[2] * cos - right[2] * sin,
            ];
            push_particle_quad(
                quads,
                particle.pos,
                r2.map(|v| v * half),
                u2.map(|v| v * half),
                particle.rgba,
            );
        }
    }
}

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
    fn list_saved_worlds_lists_only_directories_sorted() {
        let dir = std::env::temp_dir().join(format!(
            "megu-gpu-saves-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let saves = dir.join("saves");
        std::fs::create_dir_all(saves.join("Beta World")).unwrap();
        std::fs::create_dir_all(saves.join("alpha world")).unwrap();
        std::fs::write(saves.join("options.txt"), b"not a world").unwrap();

        assert_eq!(
            list_saved_worlds(Some(&dir)),
            vec!["Beta World".to_string(), "alpha world".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);

        // A missing saves/ dir yields an empty list, never a panic.
        assert!(list_saved_worlds(Some(&dir)).is_empty());
    }

    #[test]
    fn singleplayer_screen_populates_worlds_from_game_dir() {
        let dir = std::env::temp_dir().join(format!(
            "megu-gpu-sp-worlds-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("saves").join("Test World")).unwrap();

        let mut app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        });
        assert_eq!(app.ui.singleplayer.worlds, vec!["Test World".to_string()]);

        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Singleplayer));
        assert_eq!(app.ui.screen, Screen::Singleplayer);
        assert_eq!(app.ui.singleplayer.worlds, vec!["Test World".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn play_world_launch_failure_surfaces_visible_status() {
        let mut app = GpuApp::new(GpuCliConfig {
            server_bin: Some(std::path::PathBuf::from(
                "/nonexistent/megu-integrated",
            )),
            ..GpuCliConfig::default()
        });
        app.dispatch_ui_action(&UiAction::PlayWorld {
            name: "New World".to_string(),
            seed: Some(7),
        });
        // The failure is visible on Title/Singleplayer screens, not just
        // the HUD chat line those screens never draw (BUG-SP2).
        let status = app.ui.title_status.as_deref().unwrap_or_default();
        assert!(
            status.contains("Singleplayer") && status.contains("New World"),
            "title_status did not capture the failure: {status:?}"
        );
        assert!(app
            .status_messages()
            .iter()
            .any(|m| m.contains("Singleplayer")));
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

    /// The window half of the death loop, driven through the same
    /// `sync_death_screen`/`dispatch_ui_action` path `render_frame` and
    /// the event handlers use: kill packet -> `Screen::Death` -> Respawn
    /// click -> `client_command(0)` queued -> server recovery -> HUD.
    #[test]
    fn death_to_respawn_drives_screen_death_then_hud() {
        use megu_client::protocol::{ClientPlayEvent, DimensionMeta, PlayEvent};

        let mut app = GpuApp::new(GpuCliConfig::default());
        // A real outgoing channel pair: the receiver end must stay alive
        // or `request_respawn`'s `try_send` fails as disconnected.
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.pose = Some(PlayerPose {
            feet: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
        });
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;

        // Anonymous-NBT `{"text":"..."}` — the kill packet's message.
        let mut kill_nbt = vec![10u8, 8];
        kill_nbt.extend_from_slice(&4u16.to_be_bytes());
        kill_nbt.extend_from_slice(b"text");
        let msg = "Tester was slain by Creeper";
        kill_nbt.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        kill_nbt.extend_from_slice(msg.as_bytes());
        kill_nbt.push(0);
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::PlayerCombatKill {
                player_id: 11,
                message_nbt: kill_nbt,
            }))
            .unwrap();

        // The per-frame sync turns death into Screen::Death with the
        // decoded cause; the cursor policy un-grabs (not InGameHud).
        app.sync_death_screen();
        assert_eq!(app.ui.screen, Screen::Death);
        assert_eq!(app.ui.death.cause_message.as_deref(), Some(msg));
        assert!(!app.wants_cursor_grab());
        assert!(app.ui_wants_input());

        // Respawn click -> UiAction::PerformRespawn -> the host queues
        // `client_command(0)` (`respawn_pending` proves dispatch; the
        // WorkerCommand variant itself is covered in live_session tests).
        app.ui.render(1280, 720);
        let region = app
            .ui
            .hit_regions
            .iter()
            .find(|r| r.id == megu_ui::state::WidgetId::DeathRespawn)
            .copied()
            .expect("respawn button region");
        let actions = app.ui.handle_input(UiInputEvent::Click {
            x: region.rect.x + 2,
            y: region.rect.y + 2,
        });
        assert_eq!(actions, vec![UiAction::PerformRespawn]);
        for action in &actions {
            app.dispatch_ui_action(action);
        }
        assert!(
            app.live.as_ref().unwrap().death.respawn_pending(),
            "PerformRespawn dispatch must queue the packet"
        );
        // Still dead — nothing marked the player alive yet.
        app.sync_death_screen();
        assert_eq!(app.ui.screen, Screen::Death);

        // Server recovery clears death; the next sync returns to the HUD.
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::Respawn {
                dimension: DimensionMeta::default(),
            }))
            .unwrap();
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::SetHealth {
                health: 20.0,
                food: 20,
                saturation: 5.0,
            }))
            .unwrap();
        app.sync_death_screen();
        assert_eq!(app.ui.screen, Screen::InGameHud);
        assert!(app.ui.death.cause_message.is_none());
    }

    /// `Title Screen` on the death leaf drops the live session — the
    /// `LeaveWorld` action's whole job.
    #[test]
    fn death_screen_title_button_leaves_world() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let mut live = LiveSession::offline_with_null_audio();
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.show_death(megu_ui::death_screen::DeathScreenState::default());

        // The real click path: the widget emits `LeaveWorld` while the
        // state machine navigates to Title; dispatch drops the session.
        app.ui.render(1280, 720);
        let region = app
            .ui
            .hit_regions
            .iter()
            .find(|r| r.id == megu_ui::state::WidgetId::DeathTitleScreen)
            .copied()
            .expect("title button region");
        let actions = app.ui.handle_input(UiInputEvent::Click {
            x: region.rect.x + 2,
            y: region.rect.y + 2,
        });
        assert_eq!(actions, vec![UiAction::LeaveWorld]);
        for action in &actions {
            app.dispatch_ui_action(action);
        }
        assert!(app.live.is_none(), "LeaveWorld must drop the session");
        assert_eq!(app.ui.screen, Screen::Title);
    }

    /// Pause-menu `Save and Quit to Title` must drop the live session the
    /// same way — the leak this fixes let the socket survive to Title.
    #[test]
    fn pause_save_quit_leaves_world() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let mut live = LiveSession::offline_with_null_audio();
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.navigate(Screen::Pause);

        app.ui.render(1280, 720);
        let region = app
            .ui
            .hit_regions
            .iter()
            .find(|r| r.id == megu_ui::state::WidgetId::PauseSaveQuit)
            .copied()
            .expect("save & quit region");
        let actions = app.ui.handle_input(UiInputEvent::Click {
            x: region.rect.x + 2,
            y: region.rect.y + 2,
        });
        assert_eq!(actions, vec![UiAction::LeaveWorld]);
        for action in &actions {
            app.dispatch_ui_action(action);
        }
        assert!(app.live.is_none(), "Save & Quit must drop the session");
        assert_eq!(app.ui.screen, Screen::Title);
    }

    /// Grabbed in-game clicks dispatch the vanilla crosshair order:
    /// entity-nearer-than-block -> `interact` attack/`interact_at`, then
    /// `swing`; air right-click falls back to `use_item`.
    #[test]
    fn grabbed_world_clicks_dispatch_attack_and_interact() {
        use crate::live_session::WorkerCommand;
        use megu_client::protocol::{ClientPlayEvent, DimensionMeta, PlayEvent};

        let (tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(16);
        let _ = tx;
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: false,
        }))
        .unwrap();
        live.pose = Some(PlayerPose {
            feet: [0.0, 64.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid: 42,
            uuid: [3; 16],
            entity_type: 124,
            x: 0.0,
            y: 64.0,
            z: 3.0,
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        }))
        .unwrap();
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(live);
        app.ui.navigate(Screen::InGameHud);

        // Left click on the entity ahead: attack + swing.
        app.world_mouse_input(ElementState::Pressed, MouseButton::Left);
        // Right click on it: interact_at (hit position) + swing.
        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);

        let mut commands = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            commands.push(cmd);
        }
        assert!(
            commands.iter().any(|c| matches!(
                c,
                WorkerCommand::Interact {
                    target: 42,
                    action: 1,
                    ..
                }
            )),
            "left click on an entity must enqueue attack"
        );
        assert!(commands.iter().any(|c| matches!(
            c,
            WorkerCommand::Interact {
                target: 42,
                action: 2,
                at: Some(_),
                ..
            }
        )));
        assert_eq!(
            commands
                .iter()
                .filter(|c| matches!(c, WorkerCommand::Swing { hand: 0 }))
                .count(),
            2,
            "each click pairs with a main-hand swing"
        );

        // Off-entity right click on air falls back to `use_item`.
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::RemoveEntities {
                eids: vec![42],
            }))
            .unwrap();
        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);
        let mut commands = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            commands.push(cmd);
        }
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, WorkerCommand::UseItem { .. })),
            "air right-click must enqueue use_item"
        );

        // Dead players send nothing.
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::SetHealth {
                health: 0.0,
                food: 20,
                saturation: 5.0,
            }))
            .unwrap();
        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);
        assert!(
            rx.try_recv().is_err(),
            "dead player input must not reach the wire"
        );
    }

    /// Hotbar slot keys (and the scroll handler that reuses the same
    /// event) echo the held slot to the server.
    #[test]
    fn hotbar_selection_sends_set_carried_item() {
        use crate::live_session::WorkerCommand;

        let (tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(8);
        let _ = tx;
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(live);

        app.dispatch_input(WindowInputEvent::SelectHotbar(5));
        assert_eq!(app.ui.hud.selected_slot, 5);
        assert!(
            matches!(rx.try_recv(), Ok(WorkerCommand::SetCarriedItem { slot: 5 })),
            "slot change must echo set_carried_item to the server"
        );
    }

    /// C3 regression: debug-chord chat lands in `live.hud` once, the
    /// per-frame HUD sync mirrors it wholesale, and repeated "frames"
    /// never duplicate or grow the line list past the 100-line cap.
    #[test]
    fn chat_lines_bounded_and_never_duplicated_across_frames() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(LiveSession::offline_with_null_audio());
        for i in 0..140 {
            app.dispatch_input(WindowInputEvent::DebugChord(
                DebugChordAction::ChatFeedback(format!("line {i}")),
            ));
        }
        // Simulate 300 rendered frames worth of HUD syncs.
        for _ in 0..300 {
            app.live
                .as_mut()
                .unwrap()
                .sync_hud_to(&mut app.ui.hud);
        }
        assert!(
            app.ui.hud.chat_lines.len() <= 100,
            "chat_lines must stay bounded, got {}",
            app.ui.hud.chat_lines.len()
        );
        assert_eq!(
            app.ui
                .hud
                .chat_lines
                .iter()
                .filter(|l| l.as_str() == "line 139")
                .count(),
            1,
            "each chat line must appear exactly once no matter how many frames render"
        );
        assert!(
            app.ui.hud.chat_lines.iter().all(|l| l.starts_with("line ")),
            "no stale/duplicated lines may ride along: {:?}",
            app.ui.hud.chat_lines
        );
    }

    /// C3: the offline (no live session) path keeps the same bound.
    #[test]
    fn offline_chat_lines_stay_bounded() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        for i in 0..140 {
            app.dispatch_input(WindowInputEvent::DebugChord(
                DebugChordAction::ChatFeedback(format!("line {i}")),
            ));
        }
        assert!(app.ui.hud.chat_lines.len() <= 100);
        assert_eq!(app.debug_messages.len(), 100);
        assert_eq!(app.ui.hud.chat_lines.first().map(String::as_str), Some("line 40"));
    }

    /// C3: attaching a session replays the pre-connect debug log into
    /// `live.hud` exactly once — the HUD mirror then owns the lines.
    #[test]
    fn attach_live_replays_debug_log_once() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.dispatch_input(WindowInputEvent::DebugChord(
            DebugChordAction::ChatFeedback("pre-connect".to_owned()),
        ));
        app.attach_live(LiveSession::offline_with_null_audio());
        let live = app.live.as_ref().unwrap();
        assert_eq!(
            live.hud
                .chat_lines
                .iter()
                .filter(|l| l.as_str() == "pre-connect")
                .count(),
            1
        );
        assert!(app.live_strategy_pending());
    }

    /// C5: wire-id fallback maps the 1.21.1
    /// `BuiltInRegistries.BLOCK_ENTITY_TYPE` ids for the renderable
    /// families and rejects the rest (spawner=9 has no BER mesh).
    #[test]
    fn block_entity_wire_ids_map_to_ber_families() {
        let be = |kind: i32| TrackedBlockEntity {
            kind,
            nbt_bytes: Vec::new(),
        };
        assert_eq!(
            classify_tracked_block_entity(&be(1)),
            Some(VanillaBerKind::Chest)
        );
        assert_eq!(
            classify_tracked_block_entity(&be(7)),
            Some(VanillaBerKind::Sign)
        );
        assert_eq!(
            classify_tracked_block_entity(&be(14)),
            Some(VanillaBerKind::Beacon)
        );
        assert_eq!(
            classify_tracked_block_entity(&be(32)),
            Some(VanillaBerKind::Campfire)
        );
        assert_eq!(
            classify_tracked_block_entity(&be(40)),
            Some(VanillaBerKind::DecoratedPot)
        );
        assert_eq!(classify_tracked_block_entity(&be(9)), None);
        assert_eq!(classify_tracked_block_entity(&be(0)), None);
    }

    /// C5: the pose fingerprint changes on real pose deltas but ignores
    /// sub-quantization jitter, so rig meshes are not rebuilt per frame.
    #[test]
    fn entity_pose_key_detects_only_real_pose_changes() {
        let spec = megu_client_mesh::entity_rig::classify_entity_rig(114) // zombie
            .expect("zombie rig spec");
        let base = EntityPose::default();
        let key = entity_pose_key(spec, &base);
        let jitter = EntityPose {
            head_yaw_deg: 0.005, // < 1/64 quantization step
            ..EntityPose::default()
        };
        assert_eq!(entity_pose_key(spec, &jitter), key);
        let turned = EntityPose {
            head_yaw_deg: 45.0,
            ..EntityPose::default()
        };
        assert_ne!(entity_pose_key(spec, &turned), key);
        let sneaking = EntityPose {
            sneaking: true,
            ..EntityPose::default()
        };
        assert_ne!(entity_pose_key(spec, &sneaking), key);
    }

    /// C5: quads pushed for live particles are axis-consistent
    /// parallelograms — the shape `pack_particle_billboard` requires for
    /// the instanced path — and degenerate input is skipped.
    #[test]
    fn particle_quad_push_builds_valid_parallelograms() {
        let mut quads = Vec::new();
        let right = [1.0, 0.0, 0.0];
        let up = [0.0, 1.0, 0.0];
        push_billboard(&mut quads, [1.0, 2.0, 3.0], right, up, 0.5, [1.0; 4]);
        push_flat_ring(&mut quads, [0.0, 0.0, 0.0], 0.25, [0.5, 0.5, 0.5, 1.0]);
        assert_eq!(quads.len(), 2);
        let quad = &quads[0];
        let u = [
            quad.positions[1][0] - quad.positions[0][0],
            quad.positions[1][1] - quad.positions[0][1],
            quad.positions[1][2] - quad.positions[0][2],
        ];
        let v = [
            quad.positions[3][0] - quad.positions[0][0],
            quad.positions[3][1] - quad.positions[0][1],
            quad.positions[3][2] - quad.positions[0][2],
        ];
        for axis in 0..3 {
            let expected = quad.positions[0][axis] + u[axis] + v[axis];
            assert!(
                (quad.positions[2][axis] - expected).abs() < 1e-5,
                "corner 2 must close the parallelogram on axis {axis}"
            );
        }
        // Degenerate/NaN input is dropped, not staged.
        push_billboard(&mut quads, [f32::NAN; 3], right, up, 0.5, [1.0; 4]);
        push_billboard(&mut quads, [0.0; 3], right, up, 0.0, [1.0; 4]);
        assert_eq!(quads.len(), 2);
    }

    /// Multiplayer screen `<->` `ServerListStore` roundtrip through the
    /// same `dispatch_ui_action` path the click handlers use: opening
    /// the screen loads `<gameDir>/megu_servers.toml`, `SaveServer`
    /// persists through the store, `DeleteServer` removes it.
    #[test]
    fn multiplayer_screen_loads_and_persists_server_list() {
        let dir = std::env::temp_dir().join(format!(
            "megu-gpu-servers-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        });

        // Opening Multiplayer loads the (empty) store.
        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Multiplayer));
        assert_eq!(app.ui.screen, Screen::Multiplayer);
        assert!(app.ui.multiplayer.servers.is_empty());

        // Save persists through the store and refreshes the visible rows.
        app.dispatch_ui_action(&UiAction::SaveServer {
            name: "Hub".to_string(),
            address: "hub.example.org".to_string(),
            port: 25566,
        });
        assert_eq!(app.ui.multiplayer.servers.len(), 1);
        assert_eq!(app.ui.multiplayer.servers[0].name, "Hub");
        assert_eq!(app.ui.multiplayer.servers[0].port, 25566);
        assert!(dir.join("megu_servers.toml").exists());

        // A fresh load sees the same entry (on-disk roundtrip).
        let store = ServerListStore::load(&dir).unwrap();
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].address, "hub.example.org");

        // Delete removes the row and the persisted entry.
        app.ui.multiplayer.selected = Some(0);
        app.dispatch_ui_action(&UiAction::DeleteServer { index: 0 });
        assert!(app.ui.multiplayer.servers.is_empty());
        let store = ServerListStore::load(&dir).unwrap();
        assert!(store.entries().is_empty());

        // Store-level rejection (port 0 can't arrive from the UI, but a
        // bad index surfaces on the status banner, never silently).
        app.dispatch_ui_action(&UiAction::DeleteServer { index: 9 });
        assert!(app.ui.multiplayer.status.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `SessionStatus::Disconnected` → `Screen::Disconnected` with the
    /// verbatim reason; the live session and every overlay are dropped
    /// and the pointer is released (the same cleanup
    /// `leave_world_for_title` performs).
    #[test]
    fn disconnected_transition_drops_session_and_shows_reason() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Disconnected("Kicked by an operator.".to_string());
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;
        app.paused = true;
        app.chat_bar = Some(ZshCommandBar::new());

        app.sync_disconnected_screen();
        assert!(app.live.is_none());
        assert_eq!(app.ui.screen, Screen::Disconnected);
        assert_eq!(app.ui.disconnected.reason, "Kicked by an operator.");
        assert!(!app.paused);
        assert!(app.chat_bar.is_none());
        assert!(app.container_screen.is_none());

        // Playing sessions are untouched.
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;
        app.sync_disconnected_screen();
        assert!(app.live.is_some());
        assert_eq!(app.ui.screen, Screen::InGameHud);
    }

    /// `container_set_data` properties recorded on `live.open_screen`
    /// reach the rendered `ContainerScreenState.properties` every frame —
    /// they carry no `state_id`, so the mirror must not wait for a bump.
    #[test]
    fn sync_container_screen_mirrors_live_properties() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;

        // Furnace (menu 14) opens, then container_set_data lands without
        // a state_id bump.
        use megu_client::protocol::{ClientPlayEvent, PlayEvent};
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::OpenScreen {
                window_id: 1,
                menu_type: 14,
                title_nbt: Vec::new(),
            }))
            .unwrap();
        app.sync_container_screen();
        assert!(app.container_screen.is_some());
        assert!(app.container_screen.as_ref().unwrap().properties.is_empty());

        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetData {
                window_id: 1,
                property: 2, // furnace cook progress
                value: 42,
            }))
            .unwrap();
        app.sync_container_screen();
        assert_eq!(app.container_screen.as_ref().unwrap().property(2), 42);
    }
}
