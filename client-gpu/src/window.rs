// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `winit` 0.30 window + input loop: `ApplicationHandler`, FPS camera
//! controls, pause/settings via `mc_spine_ui`, background-FPS throttling,
//! and `FrameBudgetGovernor` pacing — all driving the real `wgpu` renderer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mc_spine_client::render::{FrameBudgetGovernor, RenderConfig};
use mc_spine_ui::draw::rasterize_draw_list;
use mc_spine_ui::magic_hud::build_magic_hud_draw_list;
use mc_spine_ui::state::{Screen, UiAction, UiInputEvent, UiKey, UiState};
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
    keys: Keys,
    focused: bool,
    occluded: bool,
    scale_factor: f64,
    cursor_grabbed: bool,
    paused: bool,
    show_diagnostics: bool,
    mouse_pos: (i32, i32),
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
        ui.screen = Screen::InGameHud;
        ui.hud = mc_spine_ui::state::HudState::demo();
        Self {
            config,
            live,
            active: None,
            camera,
            keys: Keys::default(),
            focused: true,
            occluded: false,
            scale_factor: 1.0,
            cursor_grabbed: false,
            paused: false,
            show_diagnostics: false,
            mouse_pos: (0, 0),
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
        self.set_cursor_grab(true);
        Ok(())
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

        // Camera movement only applies while unpaused and grabbed.
        if !self.paused {
            self.camera.apply_move(self.keys.move_input(), dt.min(0.1));
        }

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
        let sky = active
            .renderer
            .set_camera(&self.camera, w as f32 / h as f32);
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
        match code {
            KeyCode::KeyW => self.keys.forward = pressed,
            KeyCode::KeyS => self.keys.back = pressed,
            KeyCode::KeyA => self.keys.left = pressed,
            KeyCode::KeyD => self.keys.right = pressed,
            KeyCode::Space => self.keys.up = pressed,
            KeyCode::ShiftLeft | KeyCode::ShiftRight => self.keys.down = pressed,
            KeyCode::ControlLeft | KeyCode::ControlRight => self.keys.sprint = pressed,
            KeyCode::F3 if pressed => self.show_diagnostics = !self.show_diagnostics,
            KeyCode::Escape if pressed => {
                self.paused = !self.paused;
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

    fn apply_ui_action(&mut self, action: &UiAction, event_loop: &ActiveEventLoop) {
        match action {
            UiAction::QuitGame => event_loop.exit(),
            UiAction::OpenScreen(screen) => self.ui.navigate(*screen),
            UiAction::Back => {
                self.ui
                    .handle_input(UiInputEvent::KeyPressed(UiKey::Escape));
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
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
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
                self.camera.apply_mouse_look(delta.0, delta.1);
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
