// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `winit` 0.30 window + input loop: `ApplicationHandler`, FPS camera
//! controls, pause/settings via `mc_spine_ui`, background-FPS throttling,
//! and `FrameBudgetGovernor` pacing — all driving the real `wgpu` renderer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mc_spine_client::audio_qol::{ZoomState, DEFAULT_ZOOM_SMOOTHING_RATE};
use mc_spine_client::render::{FrameBudgetGovernor, RenderConfig};
use mc_spine_client_mesh::builtin_shadow_wgsl::{BuiltinShadowQuality, RenderScalePreset};
use mc_spine_kernel::debug_chords::{
    finish_gamemode_switcher, handle_f3_chord, DebugChordAction, DebugChordContext,
    DebugChordState, DebugTarget, F3Chord, PlayerDebugInfo,
};
use mc_spine_client::protocol::encode_slot_stack;
use mc_spine_ui::container_screen::{
    menu_schema, ContainerScreenState, DragState, UiItemStack,
};
use mc_spine_ui::draw::rasterize_draw_list;
use mc_spine_ui::magic_hud::build_magic_hud_draw_list;
use mc_spine_ui::state::{
    Screen, UiAction, UiInputEvent, UiKey, UiSettings, UiState, VideoPreset, VisualStylePreset,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};

use crate::live_session::{LiveSession, PlayerMovement, PlayerPose, SessionStatus};
use crate::renderer::{request_gpu_device, GpuRenderer};
use crate::{
    build_demo_scene, detect_display_server_kind, fingerprint_draw_list, preferred_alpha_mode,
    present_mode_for, select_display_presentation_policy, Camera, DisplayPresentationPolicy,
    GpuCliConfig, MoveInput,
};

/// Window title.
pub const WINDOW_TITLE: &str = "mc-spine";

/// Event-loop wake interval while the surface is occluded: slow enough for
/// zero-GPU sleep, fast enough to keep a live session's network pump alive.
const OCCLUDED_TICK: Duration = Duration::from_millis(50);

const THIRD_PERSON_DISTANCE: f32 = 4.0;
const SPRINT_FOV_BOOST: f32 = 0.10;
const SPRINT_FOV_SMOOTHING_RATE: f32 = 12.0;

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
    ui: UiState,
    ui_cache: UiUploadCache,
    render_config: RenderConfig,
    policy: DisplayPresentationPolicy,
    governor: FrameBudgetGovernor,
    last_frame: Instant,
    frame_start: Instant,
    fatal: Option<String>,
}

impl GpuApp {
    /// Build the app state (no GPU or window yet — `resumed` does that).
    pub fn new(config: GpuCliConfig) -> Self {
        let live = if config.offline_demo {
            None
        } else {
            config.connect.as_ref().map(|(host, port)| {
                LiveSession::connect(
                    host.clone(),
                    *port,
                    config.username.clone(),
                    config.mc_version.clone(),
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
        let mut ui = UiState::new();
        ui.settings.video.vsync = render_config.vsync;
        ui.screen = Screen::InGameHud;
        ui.hud = mc_spine_ui::state::HudState::demo();
        Self {
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
            ui,
            ui_cache: UiUploadCache::default(),
            render_config,
            policy,
            governor,
            last_frame: Instant::now(),
            frame_start: Instant::now(),
            fatal: None,
        }
    }

    /// The fatal setup error, if `resumed` hit one (`run` reports it).
    pub fn fatal(&self) -> Option<&str> {
        self.fatal.as_deref()
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
            active.renderer.set_shadow_quality(shadow_quality);
            let scale = match video.preset {
                VideoPreset::Fast => RenderScalePreset::Performance67,
                VideoPreset::Balanced | VideoPreset::High | VideoPreset::Ultra => {
                    RenderScalePreset::Native100
                }
                VideoPreset::Custom => active.renderer.render_scale,
            };
            active.renderer.set_render_scale(scale);
        }
        if vsync_changed {
            if let Some(size) = self.active.as_ref().map(|active| active.size) {
                self.reconfigure(size);
            }
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

    fn init_gpu(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attrs = WindowAttributes::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size(PhysicalSize::new(1280u32, 720u32))
            .with_resizable(true);
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
            renderer: GpuRenderer::new(ctx, surface_format),
            surface_format,
            size,
        };
        let config = surface_config_for(
            self.render_config.vsync,
            &self.policy,
            active.surface_format,
            active.size,
            &caps,
        );
        active
            .surface
            .configure(&active.renderer.gpu().device, &config);
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
        self.set_cursor_grab(true);
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
            schema: &'static mc_spine_ui::container_screen::MenuScreenSchema,
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
                if self.container_screen.take().is_some()
                    && self.live.is_some()
                    && !self.paused
                    && self.focused
                {
                    self.set_cursor_grab(true);
                }
            }
        }
    }

    /// Forward one UI click action as the matching `container_click` /
    /// `container_button_click` serverbound packet on the worker.
    fn forward_click_action(
        live: &mut LiveSession,
        action: mc_spine_ui::container_screen::ClickWindowAction,
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
                if let Some(hit) = layout
                    .slots
                    .iter()
                    .find(|slot| slot.rect.contains(mx, my))
                {
                    let slot_index = hit.index;
                    if let Some(action) = screen.click(slot_index, button_id) {
                        if let Some(live) = &mut self.live {
                            Self::forward_click_action(live, action);
                        }
                    }
                } else if let Some(hit) = layout
                    .buttons
                    .iter()
                    .find(|b| b.rect.contains(mx, my))
                {
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

    fn render_frame(&mut self) {
        let frame_begin = Instant::now();
        self.frame_start = frame_begin;
        let dt = frame_begin.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = frame_begin;

        if let Some(live) = &mut self.live {
            live.poll(self.camera.position);
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
        if self.occluded {
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
            if live.terrain_dirty {
                active.renderer.update_live_terrain(&live.world);
                live.terrain_dirty = false;
            }
            self.ui.hud = live.hud.clone();
            live.record_frame_and_refresh_minihud(dt, &mut self.ui);
            if live.asset_bundle_pending_upload {
                if let Some(bundle) = &live.asset_bundle {
                    match active
                        .renderer
                        .upload_block_atlas(&bundle.atlas, &bundle.mipmaps)
                    {
                        Ok(()) => live.asset_bundle_pending_upload = false,
                        Err(err) => {
                            eprintln!("mc-spine-client assets: atlas upload failed: {err}");
                            live.asset_bundle_pending_upload = false;
                        }
                    }
                }
            }
            active.renderer.sync_subworlds(&mut live.subworlds);
            self.ui
                .hud
                .chat_lines
                .extend(self.debug_messages.iter().cloned());
        }

        // HUD/diagnostics through the real client-ui state machine.
        self.ui.hud.actionbar = if self.show_diagnostics {
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
            )
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
        // Dirty-only upload: unchanged HUD frames skip the `w*h*4` rasterize
        // and the texture upload instead of burning GB/s of PCIe bandwidth.
        let fingerprint = fingerprint_draw_list(w, h, &draw_list);
        if self.ui_cache.should_upload(w, h, fingerprint) {
            let rgba = rasterize_draw_list(w, h, &draw_list);
            if let Err(e) = active.renderer.update_ui_framebuffer(w, h, &rgba) {
                self.fatal = Some(format!("UI upload: {e}"));
                return;
            }
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
                active.renderer.render_to_view(&view, w, h, &sky);
                // Wayland `wl_surface.frame` pacing: notify the compositor
                // right before present so frame callbacks align with the
                // actual swap (a no-op everywhere else; the policy gates it).
                if self.policy.call_pre_present_notify {
                    active.window.pre_present_notify();
                }
                active.renderer.gpu().queue.present(frame);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {}
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => {
                let caps = active
                    .surface
                    .get_capabilities(&active.renderer.gpu().adapter);
                let cfg = surface_config_for(
                    self.render_config.vsync,
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
        self.governor.observe_frame(frame_begin.elapsed());
    }

    fn handle_key(&mut self, code: KeyCode, state: ElementState, event_loop: &ActiveEventLoop) {
        let pressed = state == ElementState::Pressed;
        let f3_chord_pressed =
            pressed && self.input_router.f3_held && f3_chord_for_key(code).is_some();
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
                self.paused = !self.paused;
                if self.paused {
                    self.camera_controller.set_zoomed(false);
                    if let Some(event) = self.input_router.release_held_keys() {
                        self.apply_window_input(event);
                    }
                }
                if self.paused {
                    self.ui.navigate(Screen::Pause);
                } else {
                    for action in self
                        .ui
                        .handle_input(UiInputEvent::KeyPressed(UiKey::Escape))
                    {
                        self.apply_ui_action(&action, event_loop);
                    }
                }
                self.set_cursor_grab(!self.paused);
            }
            _ => {
                if pressed && self.paused {
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
                }
            }
            WindowInputEvent::SetTabListVisible(visible) => {
                self.ui.hud.show_player_list = visible;
                if let Some(live) = &mut self.live {
                    live.hud.show_player_list = visible;
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
            // These bindings are emitted by the router. Inventory/chat screens
            // and server-side item operations require their own runtime APIs.
            WindowInputEvent::OpenInventory
            | WindowInputEvent::OpenChat { .. }
            | WindowInputEvent::SortInventory
            | WindowInputEvent::OpenAdvancements => {}
        }
    }

    fn apply_ui_action(&mut self, action: &UiAction, event_loop: &ActiveEventLoop) {
        match action {
            UiAction::QuitGame => event_loop.exit(),
            UiAction::OpenScreen(screen) => self.ui.navigate(*screen),
            UiAction::Back => {
                self.ui
                    .handle_input(UiInputEvent::KeyPressed(UiKey::Escape));
            }
            UiAction::SettingsChanged => {
                let settings = self.ui.settings.clone();
                self.apply_settings(&settings);
            }
            _ => {}
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
    policy: &DisplayPresentationPolicy,
    surface_format: wgpu::TextureFormat,
    size: PhysicalSize<u32>,
    caps: &wgpu::SurfaceCapabilities,
) -> wgpu::SurfaceConfiguration {
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: present_mode_for(vsync, &caps.present_modes),
        // Policy frame latency: `1` uncapped on Wayland/X11/Windows DX12
        // keeps `Mailbox`/`Immediate` input->photon time minimal; `2`
        // under vsync and on macOS Metal, where the extra buffered frame
        // avoids present stalls.
        desired_maximum_frame_latency: policy.frame_latency(vsync),
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
                    self.set_cursor_grab(true);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    if !event.repeat {
                        self.handle_key(code, event.state, event_loop);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x as i32, position.y as i32);
                if self.paused {
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
                    if self.paused {
                        let (x, y) = self.mouse_pos;
                        let actions = self.ui.handle_input(UiInputEvent::Click { x, y });
                        for action in actions {
                            self.apply_ui_action(&action, event_loop);
                        }
                    } else {
                        self.set_cursor_grab(true);
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
        // cursor clamping at screen edges.
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.cursor_grabbed && !self.paused {
                self.camera_controller
                    .apply_mouse_look(&mut self.camera, delta.0, delta.1);
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.fatal.is_some() {
            event_loop.exit();
            return;
        }
        if self.occluded {
            // Zero-GPU sleep: no redraw requests at all. A slow tick keeps
            // the live session polled until the surface un-occludes.
            if let Some(live) = &mut self.live {
                live.poll(self.camera.position);
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
        }
        if let Some(active) = &self.active {
            active.window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::Poll);
    }
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
