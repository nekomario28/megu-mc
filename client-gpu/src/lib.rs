// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-client-gpu`: the real windowed GPU client crate.
//!
//! Layers on top of the headless client stack:
//!
//! * [`renderer`] — a real `wgpu` backend (Vulkan / Metal / DX12) that
//!   uploads `megu_client_mesh` packed quad geometry, compiles the WGSL
//!   shader suite from `megu_client::shaders`, composites the
//!   `megu_ui` software-rasterized RGBA framebuffer, and validates Megu
//!   native WGSL shaderpack modules against the live `wgpu::Device`.
//! * [`window`] — a `winit` 0.30 `ApplicationHandler` driving the window,
//!   FPS input and the `FrameBudgetGovernor` pacing loop.
//! * [`linux_desktop`] — compositor, IME, cursor-grab and XDG desktop-entry
//!   policies for Linux desktop sessions.
//!
//! This module itself stays GPU-API-free where it can: camera math, the
//! quad->buffer conversion, the capability mapping and the CLI parser are
//! pure functions unit tests exercise without an adapter.

pub mod atmosphere;
pub mod clipboard;
pub mod gpu_hiz;
pub mod live_session;
pub mod linux_desktop;
pub mod mesh_pool;
pub mod mod_sync_auto;
pub mod pipeline_cache_store;
pub mod recipe_ingest;
pub mod renderer;
pub mod replay_ring;
pub mod subworld_render;
pub mod subworld_sync;
pub mod title_branding;
pub mod vanilla_bootstrap;
pub mod vanilla_atlas_extras;
pub mod vanilla_font;
pub mod vanilla_i18n_data;
pub mod vanilla_gui_sprites;
pub mod vanilla_jar;
pub mod vanilla_world_assets;
pub mod window;

use std::path::PathBuf;

use megu_client::cli::parse_host_port;
use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::{
    mesh_section, resolve_render_backend, Block, GpuCapabilities, GpuShaderCaps, GpuVendor, Layer,
    Quad, RenderBackendPreference, Section, WATER,
};
use megu_core::hash::Fnv1a64;
use megu_ui::draw::{UiColor, UiCommand, UiDrawList, UiRect};

/// Crate/binary name used in diagnostics.
pub const CLIENT_GPU_NAME: &str = "megu-client-gpu";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// A credential value accepted from the launcher (`--accessToken`,
/// `--xuid`, `MEGU_LAUNCHER_ACCESS_TOKEN`) that must never appear in
/// diagnostics: `Debug` prints `<redacted>` and no `Display` exists.
/// `reveal` is deliberately the *only* way to read the value back so call
/// sites that format it stand out in review.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a raw secret.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Expose the inner secret for wire use only; never format it.
    pub fn reveal(&self) -> &str {
        &self.0
    }

    /// `true` when a real (non-empty) secret is present.
    pub fn is_present(&self) -> bool {
        !self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Everything `megu-client-gpu` accepts on the command line.
///
/// The Official/Prism/Modrinth launchers invoke the game with a fixed
/// argument surface (`--username`, `--version`, `--gameDir`,
/// `--quickPlayMultiplayer`, ...). Every one of those flags parses
/// cleanly here; megu's own flags (`--connect`, `--mc-version`,
/// `--shader-pack`, `--gpu-backend`, `--offline-demo`, `--headless-probe`,
/// `--vsync`, `--relay`, `--server-bin`) keep their existing meanings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuCliConfig {
    /// `--connect <host[:port]>` or `--quickPlayMultiplayer <host[:port]>` —
    /// the server the window session targets.
    /// Connect to a live offline-authenticated Protocol 767/777 server.
    pub connect: Option<(String, u16)>,
    /// Offline-mode player name, 1–16 ASCII letters/digits/underscores.
    pub username: String,
    /// Minecraft protocol, 767 (1.21.1) or 777 (26.3).
    pub mc_version: String,
    /// Launcher `--version <id>` (`1.21.1`, `megu-1.21.1`): the
    /// `<gameDir>/versions/<id>/<id>.jar` discovery key. When the id also
    /// parses as a protocol (`1.21.1`/`767`/`26.3`/`777`), `mc_version`
    /// follows it.
    pub version_id: Option<String>,
    /// `--uuid <uuid>` — the account id the launcher reports (kept for
    /// parity; offline sessions still use `offline_uuid(username)`).
    pub uuid: Option<String>,
    /// `--accessToken <token>` or `MEGU_LAUNCHER_ACCESS_TOKEN`; always
    /// [`SecretString`]-wrapped so `Debug` of this config can never leak it.
    pub access_token: Option<SecretString>,
    /// `--clientToken`/`--authToken`/`--token` — Mojang session id.
    pub client_token: Option<SecretString>,
    /// `--xuid <x>` — Xbox user id (token-class data, redacted).
    pub xuid: Option<SecretString>,
    /// `--clientId <id>`.
    pub client_id: Option<String>,
    /// `--userType <t>` (`msa`, `mojang`, `legacy`).
    pub user_type: Option<String>,
    /// `--versionType <t>` (`release`, `snapshot`, custom labels).
    pub version_type: Option<String>,
    /// `--userProperties <json>` — accepted verbatim; not interpreted.
    pub user_properties: Option<String>,
    /// `--gameDir <dir>` — the user's `.minecraft` root: versioned client
    /// JARs, `resourcepacks/`, `saves/` and `options.txt` all resolve here.
    pub game_dir: Option<PathBuf>,
    /// `--assetsDir <dir>` — the asset store for
    /// `indexes/<assetIndex>.json` + `objects/` sound resolution.
    pub assets_dir: Option<PathBuf>,
    /// `--assetIndex <id>` — e.g. `17` for 1.21.1.
    pub asset_index: Option<String>,
    /// `--resourcePackDir <dir>` — the launcher's resourcepacks folder
    /// (defaults to `<gameDir>/resourcepacks` when absent).
    pub resource_pack_dir: Option<PathBuf>,
    /// `--quickPlaySingleplayer <world>` — validated by
    /// `megu_client::integrated::validate_world_name` at launch time.
    pub quick_play_singleplayer: Option<String>,
    /// `--quickPlayPath <path>` — accepted for compatibility; no consumer.
    pub quick_play_path: Option<String>,
    /// `--quickPlayRealms <id>` — accepted; realms join is unimplemented
    /// and surfaces a status message rather than failing silently.
    pub quick_play_realms: Option<String>,
    /// `--server-bin <path>` — explicit `megu` binary for the
    /// integrated singleplayer server.
    pub server_bin: Option<PathBuf>,
    /// `--seed <n>` — world seed forwarded to the integrated
    /// singleplayer server as `MEGU_SEED` (new worlds only).
    pub seed: Option<i64>,
    /// `--relay <host:port>` — friend-invite relay (`JoinWithCode`
    /// dispatches a [`megu_client::friend_client::FriendGuestBridge`]
    /// to it).
    pub relay: Option<String>,
    /// `--width <px>` — initial window width.
    pub width: Option<u32>,
    /// `--height <px>` — initial window height.
    pub height: Option<u32>,
    /// `--fullscreen` — start fullscreen instead of windowed.
    pub fullscreen: bool,
    /// `--shader-pack <path>` — directory or `.zip` holding a
    /// `megu-shader.toml` Megu native WGSL pack (or an OptiFine/Iris pack,
    /// which fails closed to built-in rendering with a notice).
    pub shader_pack: Option<PathBuf>,
    /// `--gpu-backend <auto|mesh-shader|compute-indirect|classic|classic-low-spec>`.
    pub gpu_backend: RenderBackendPreference,
    /// `--offline-demo` — render the built-in demo section instead of a
    /// server world so the window works with no network at all.
    pub offline_demo: bool,
    /// `--headless-probe` — enumerate wgpu adapters, print the negotiated
    /// pipeline and exit without opening a window.
    pub headless_probe: bool,
    /// `--vsync off` — uncapped `Immediate`/`Mailbox` presentation instead
    /// of the default FIFO vsync.
    pub vsync: bool,
    /// `--help`.
    pub show_help: bool,
    /// Bare `--version` at end-of-args or `-V` — print the binary version.
    /// (`--version <id>` with a real value is the launcher version id and
    /// lands in [`Self::version_id`] instead.)
    pub show_version: bool,
}

impl Default for GpuCliConfig {
    fn default() -> Self {
        Self {
            connect: None,
            username: "MeguGpu".into(),
            mc_version: "767".into(),
            version_id: None,
            uuid: None,
            access_token: None,
            client_token: None,
            xuid: None,
            client_id: None,
            user_type: None,
            version_type: None,
            user_properties: None,
            game_dir: None,
            assets_dir: None,
            asset_index: None,
            resource_pack_dir: None,
            quick_play_singleplayer: None,
            quick_play_path: None,
            quick_play_realms: None,
            server_bin: None,
            seed: None,
            relay: None,
            width: None,
            height: None,
            fullscreen: false,
            shader_pack: None,
            gpu_backend: RenderBackendPreference::default(),
            offline_demo: false,
            headless_probe: false,
            vsync: true,
            show_help: false,
            show_version: false,
        }
    }
}

/// Record the launcher's `--version <id>`: `version_id` keeps the raw id
/// for JAR discovery, and `mc_version` follows when the id (or the part
/// after a `megu-` prefix) parses as a supported protocol.
fn apply_launcher_version_id(cfg: &mut GpuCliConfig, id: &str) {
    cfg.version_id = Some(id.to_owned());
    let stripped = id.strip_prefix("megu-").unwrap_or(id);
    if megu_client::cli::parse_mc_version(stripped).is_ok() {
        cfg.mc_version = stripped.to_owned();
    }
}

/// Parse the GPU-client argument vector (`argv[1..]`). Unknown flags and
/// missing values fail closed with a human-readable error. Secret-flag
/// values are never echoed back inside the error text.
pub fn parse_gpu_args<S: AsRef<str>>(args: &[S]) -> Result<GpuCliConfig, String> {
    let mut cfg = GpuCliConfig::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_ref();
        let mut value = None;
        let flag = match arg.split_once('=') {
            Some((flag, inline)) => {
                value = Some(inline.to_string());
                flag
            }
            None => arg,
        };
        let mut take_value = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = value.take() {
                return Ok(v);
            }
            *i += 1;
            args.get(*i)
                .map(|s| s.as_ref().to_string())
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag {
            "--help" | "-h" => cfg.show_help = true,
            "-V" => cfg.show_version = true,
            // Launchers pass `--version <id>`; the bare flag (end of args or
            // before another flag) is this binary's own version request.
            "--version" => match value.take() {
                Some(v) => apply_launcher_version_id(&mut cfg, &v),
                None => {
                    let next_is_value = args
                        .get(i + 1)
                        .is_some_and(|s| !s.as_ref().starts_with('-'));
                    if next_is_value {
                        i += 1;
                        let v = args[i].as_ref();
                        apply_launcher_version_id(&mut cfg, v);
                    } else {
                        cfg.show_version = true;
                    }
                }
            },
            "--connect" => {
                let raw = take_value(&mut i)?;
                cfg.connect =
                    Some(parse_host_port(&raw, 25565).map_err(|e| format!("--connect: {e}"))?);
            }
            "--quickPlayMultiplayer" => {
                let raw = take_value(&mut i)?;
                cfg.connect = Some(
                    parse_host_port(&raw, 25565)
                        .map_err(|e| format!("--quickPlayMultiplayer: {e}"))?,
                );
            }
            "--quickPlaySingleplayer" => {
                cfg.quick_play_singleplayer = Some(take_value(&mut i)?);
            }
            "--quickPlayPath" => cfg.quick_play_path = Some(take_value(&mut i)?),
            "--quickPlayRealms" => cfg.quick_play_realms = Some(take_value(&mut i)?),
            "--username" => {
                let name = take_value(&mut i)?;
                if !(1..=16).contains(&name.len())
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    return Err(
                        "--username: expected 1-16 ASCII letters, digits, or underscores".into(),
                    );
                }
                cfg.username = name;
            }
            "--uuid" => cfg.uuid = Some(take_value(&mut i)?),
            "--accessToken" => cfg.access_token = Some(SecretString::new(take_value(&mut i)?)),
            "--clientToken" | "--authToken" | "--token" => {
                cfg.client_token = Some(SecretString::new(take_value(&mut i)?));
            }
            "--xuid" => cfg.xuid = Some(SecretString::new(take_value(&mut i)?)),
            "--clientId" => cfg.client_id = Some(take_value(&mut i)?),
            "--userType" => cfg.user_type = Some(take_value(&mut i)?),
            "--versionType" => cfg.version_type = Some(take_value(&mut i)?),
            "--userProperties" => cfg.user_properties = Some(take_value(&mut i)?),
            "--gameDir" => cfg.game_dir = Some(PathBuf::from(take_value(&mut i)?)),
            "--assetsDir" => cfg.assets_dir = Some(PathBuf::from(take_value(&mut i)?)),
            "--assetIndex" => cfg.asset_index = Some(take_value(&mut i)?),
            "--resourcePackDir" => {
                cfg.resource_pack_dir = Some(PathBuf::from(take_value(&mut i)?));
            }
            "--width" => {
                let raw = take_value(&mut i)?;
                cfg.width = Some(raw.parse::<u32>().map_err(|_| {
                    format!("--width: expected a positive pixel count, got {raw:?}")
                })?);
            }
            "--height" => {
                let raw = take_value(&mut i)?;
                cfg.height = Some(raw.parse::<u32>().map_err(|_| {
                    format!("--height: expected a positive pixel count, got {raw:?}")
                })?);
            }
            "--fullscreen" => cfg.fullscreen = true,
            "--relay" => cfg.relay = Some(take_value(&mut i)?),
            "--server-bin" => cfg.server_bin = Some(PathBuf::from(take_value(&mut i)?)),
            "--seed" => {
                let raw = take_value(&mut i)?;
                cfg.seed = Some(raw.parse::<i64>().map_err(|_| {
                    format!("--seed: expected a signed 64-bit integer, got {raw:?}")
                })?);
            }
            // Launcher/loader flags with no GPU-session meaning; accepting
            // them keeps the real argv surface crash-free.
            "--demo" | "--disableMultiplayer" | "--disableChat" => {}
            "--proxyHost" | "--proxyPort" | "--proxyUser" | "--proxyPass" | "--natives"
            | "--classpath" | "--windowTitle" | "--iconPath" => {
                let _ = take_value(&mut i)?;
            }
            "--mc-version" => {
                let version = take_value(&mut i)?;
                megu_client::cli::parse_mc_version(&version)?;
                cfg.mc_version = version;
            }
            "--shader-pack" => cfg.shader_pack = Some(PathBuf::from(take_value(&mut i)?)),
            "--gpu-backend" => {
                let raw = take_value(&mut i)?;
                cfg.gpu_backend = RenderBackendPreference::parse(&raw).ok_or_else(|| {
                    format!(
                        "--gpu-backend: unknown backend {raw:?} (expected auto, mesh-shader, \
                         compute-indirect, classic, or classic-low-spec)"
                    )
                })?;
            }
            "--offline-demo" => cfg.offline_demo = true,
            "--headless-probe" => cfg.headless_probe = true,
            "--vsync" => match take_value(&mut i)?.as_str() {
                "on" | "fifo" => cfg.vsync = true,
                "off" | "immediate" | "mailbox" => cfg.vsync = false,
                other => return Err(format!("--vsync: expected on|off|mailbox, got {other:?}")),
            },
            other => return Err(format!("unknown argument {other:?} (see --help)")),
        }
        i += 1;
    }
    // Launcher-env credential fallback: the launcher can hand the token via
    // the environment instead of argv; argv wins when both are present.
    if cfg.access_token.is_none() {
        if let Ok(token) = std::env::var("MEGU_LAUNCHER_ACCESS_TOKEN") {
            if !token.is_empty() {
                cfg.access_token = Some(SecretString::new(token));
            }
        }
    }
    Ok(cfg)
}

/// Usage text for `--help`.
pub fn gpu_help_text() -> String {
    format!(
        "{CLIENT_GPU_NAME} {version} — real wgpu windowed client for megu (1.21.1 / protocol 767)\n\
         \n\
         USAGE:\n    {CLIENT_GPU_NAME} [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \x20   --connect <host[:port]>     target server (default port 25565)\n\
         \x20   --username <name>          player username (default: MeguGpu)\n\
         \x20   --mc-version <767|777>     server protocol (default: 767)\n\
         \x20   --shader-pack <path>        megu-shader.toml WGSL pack (dir or .zip)\n\
         \x20   --gpu-backend <backend>     auto | mesh-shader | compute-indirect |\n\
         \x20                             classic | classic-low-spec (default: auto)\n\
         \x20   --offline-demo              render the built-in demo section, no server\n\
         \x20   --headless-probe            list wgpu adapters + chosen pipeline, then exit\n\
         \x20   --vsync <on|off|mailbox>    presentation mode (default: on)\n\
         \x20   --relay <host:port>         friend-invite relay for invite-code joins\n\
         \x20   --server-bin <path>         explicit megu binary for singleplayer\n\
         \x20   --seed <n>                  world seed for a new singleplayer world\n\
         \x20   -h, --help                  print this help\n\
         \x20   -V, --version               print the version\n\
         \n\
         Standard launcher arguments are also accepted: --version <id>,\n\
         \x20   --uuid, --accessToken, --clientToken/--authToken/--token, --xuid,\n\
         \x20   --clientId, --userType, --versionType, --userProperties, --gameDir,\n\
         \x20   --assetsDir, --assetIndex, --resourcePackDir, --quickPlayMultiplayer\n\
         \x20   (same as --connect), --quickPlaySingleplayer <world>,\n\
         \x20   --quickPlayPath, --quickPlayRealms, --width, --height, --fullscreen.\n\
         \x20   Textures auto-load from <gameDir>/versions/<id>/<id>.jar and\n\
         \x20   sounds from <assetsDir>/indexes/<assetIndex>.json when present.\n\
         \n\
         CONTROLS:\n\
         \x20   WASD move, Space/Shift up/down, mouse look, F3 diagnostics,\n\
         \x20   Esc pause/settings\n",
        version = env!("CARGO_PKG_VERSION"),
    )
}

// ---------------------------------------------------------------------------
// Camera math (column-major `Mat4`, the layout WGSL `mat4x4<f32>` expects)
// ---------------------------------------------------------------------------

/// Column-major 4×4 matrix: `m[col][row]`, matching WGSL uniform layout.
pub type Mat4 = [[f32; 4]; 4];

/// `(a * b)` for column-major matrices.
pub fn mat4_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0; 4]; 4];
    for col in 0..4 {
        for row in 0..4 {
            out[col][row] = (0..4).map(|k| a[k][row] * b[col][k]).sum();
        }
    }
    out
}

/// `m * v` for a column-major matrix.
pub fn mat4_mul_vec4(m: &Mat4, v: [f32; 4]) -> [f32; 4] {
    let mut out = [0.0; 4];
    for (row, o) in out.iter_mut().enumerate() {
        *o = (0..4).map(|col| m[col][row] * v[col]).sum();
    }
    out
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len > 0.0 {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 0.0, -1.0]
    }
}

/// Degrees of yaw/pitch one mouse-motion pixel turns the camera.
pub const MOUSE_SENSITIVITY_DEG_PER_PX: f32 = 0.12;
/// Walk speed in blocks per second.
pub const WALK_SPEED: f32 = 4.3;
/// Sprint multiplier over [`WALK_SPEED`].
pub const SPRINT_MULTIPLIER: f32 = 1.3;

/// Per-frame movement input the window feeds the camera.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveInput {
    /// +1 forward / -1 back.
    pub forward: f32,
    /// +1 right / -1 left.
    pub strafe: f32,
    /// +1 up / -1 down (fly-style vertical).
    pub vertical: f32,
    /// Sprint modifier key held.
    pub sprint: bool,
}

/// FPS camera: position + Euler yaw/pitch in degrees, wgpu clip space
/// (`z in [0, 1]`, right-handed world with +Y up).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub position: [f32; 3],
    /// `0` looks down -Z; positive yaw turns right (clockwise from above).
    pub yaw_deg: f32,
    /// Clamped to `(-89.9, 89.9)`; positive looks up.
    pub pitch_deg: f32,
    /// Vertical field of view in degrees.
    pub fov_deg: f32,
    /// View-axis roll in radians (`0` upright) — used only by the view
    /// matrix, so strafe/billboard axes stay level. The tactile
    /// micro-motion layer feeds transient landing/turn rolls through
    /// here.
    pub roll_rad: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            position: [8.0, 20.0, 24.0],
            yaw_deg: 0.0,
            pitch_deg: -20.0,
            fov_deg: 70.0,
            roll_rad: 0.0,
            near: 0.1,
            far: 512.0,
        }
    }
}

impl Camera {
    /// Unit forward direction from yaw/pitch (`yaw=0` -> `-Z`).
    pub fn forward_vector(&self) -> [f32; 3] {
        let yaw = self.yaw_deg.to_radians();
        let pitch = self.pitch_deg.to_radians();
        [
            yaw.sin() * pitch.cos(),
            pitch.sin(),
            -yaw.cos() * pitch.cos(),
        ]
    }

    /// Unit right direction (cross of forward with world up).
    pub fn right_vector(&self) -> [f32; 3] {
        normalize(cross(self.forward_vector(), [0.0, 1.0, 0.0]))
    }

    /// Unit up direction — the view matrix's `u` row (cross of right
    /// with forward).
    pub fn up_vector(&self) -> [f32; 3] {
        cross(self.right_vector(), self.forward_vector())
    }

    /// World-to-view matrix (column-major for WGSL upload).
    pub fn view_matrix(&self) -> Mat4 {
        let f = self.forward_vector();
        let mut s = self.right_vector();
        let mut u = cross(s, f);
        if self.roll_rad != 0.0 {
            // Roll around the view axis: rotate the screen basis (s, u)
            // inside the plane perpendicular to `f`.
            let (sin, cos) = self.roll_rad.sin_cos();
            let s2 = [
                s[0] * cos + u[0] * sin,
                s[1] * cos + u[1] * sin,
                s[2] * cos + u[2] * sin,
            ];
            u = [
                u[0] * cos - s[0] * sin,
                u[1] * cos - s[1] * sin,
                u[2] * cos - s[2] * sin,
            ];
            s = s2;
        }
        let e = self.position;
        [
            [s[0], u[0], -f[0], 0.0],
            [s[1], u[1], -f[1], 0.0],
            [s[2], u[2], -f[2], 0.0],
            [-dot(s, e), -dot(u, e), dot(f, e), 1.0],
        ]
    }

    /// Perspective projection for wgpu NDC (`z in [0,1]`), right-handed.
    pub fn projection_matrix(&self, aspect: f32) -> Mat4 {
        let aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            1.0
        };
        let f = 1.0 / (self.fov_deg.to_radians() * 0.5).tan();
        let (near, far) = (self.near, self.far);
        [
            [f / aspect, 0.0, 0.0, 0.0],
            [0.0, f, 0.0, 0.0],
            [0.0, 0.0, far / (near - far), -1.0],
            [0.0, 0.0, (near * far) / (near - far), 0.0],
        ]
    }

    /// `projection * view`, ready for the `frame.view_proj` uniform.
    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        mat4_mul(&self.projection_matrix(aspect), &self.view_matrix())
    }

    /// Mouse-look: positive `dx` turns right, positive `dy` looks down
    /// (screen-space convention), pitch clamped inside ±89.9°.
    pub fn apply_mouse_look(&mut self, dx_px: f64, dy_px: f64) {
        self.yaw_deg =
            (self.yaw_deg + dx_px as f32 * MOUSE_SENSITIVITY_DEG_PER_PX).rem_euclid(360.0);
        self.pitch_deg =
            (self.pitch_deg - dy_px as f32 * MOUSE_SENSITIVITY_DEG_PER_PX).clamp(-89.9, 89.9);
    }

    /// Integrate one frame of movement input, dt in seconds.
    pub fn apply_move(&mut self, input: MoveInput, dt_secs: f32) {
        let speed = WALK_SPEED * if input.sprint { SPRINT_MULTIPLIER } else { 1.0 } * dt_secs;
        let fwd = self.forward_vector();
        let right = self.right_vector();
        // Horizontal movement follows the view projection on the XZ plane so
        // looking down does not slow walking.
        let fwd_h = normalize([fwd[0], 0.0, fwd[2]]);
        for axis in 0..3 {
            self.position[axis] += fwd_h[axis] * input.forward * speed
                + right[axis] * input.strafe * speed
                + if axis == 1 {
                    input.vertical * speed
                } else {
                    0.0
                };
        }
    }
}

// ---------------------------------------------------------------------------
// Packed-quad -> GPU buffer conversion (real client-port FFI path)
// ---------------------------------------------------------------------------

/// Why [`pack_quads_to_vertices`] rejected the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackQuadsError {
    /// The client-port FFI contract rejected the call.
    InvalidArgument,
    /// The reported vertex count overflowed.
    Overflow,
}

impl std::fmt::Display for PackQuadsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument => write!(
                f,
                "megu_client_mesh_pack_vertices rejected the quads (unrepresentable or \
                 unaligned input)"
            ),
            Self::Overflow => write!(f, "quad count overflows the packed vertex buffer"),
        }
    }
}

impl std::error::Error for PackQuadsError {}

/// Pack `quads` through the real `megu_client_mesh` C ABI
/// (`megu_client_mesh_pack_vertices`): four 16-byte [`PackedVertex`]s per
/// quad, in input order. Fails closed when a quad is not representable in
/// the packed format (the [`ffi` contract](PackedVertex) covers that).
pub fn pack_quads_to_vertices(quads: &[Quad]) -> Result<Vec<PackedVertex>, PackQuadsError> {
    use megu_client_mesh::ffi::megu_client_mesh_pack_vertices;
    let mut count = 0usize;
    // Query the required vertex count first (null output, zero capacity).
    // SAFETY: count is a valid writable pointer; all other pointers are null
    // or readable for quad_count elements.
    let status = unsafe {
        megu_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    if status != megu_client_mesh::ffi::ABI_OK {
        return Err(PackQuadsError::InvalidArgument);
    }
    let mut out = vec![PackedVertex::default(); count];
    // SAFETY: `out` has exactly `count` writable vertices; inputs unchanged.
    let status = unsafe {
        megu_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            out.as_mut_ptr(),
            out.len(),
            &mut count,
        )
    };
    match status {
        megu_client_mesh::ffi::ABI_OK => Ok(out),
        _ => Err(PackQuadsError::InvalidArgument),
    }
}

/// Index buffer honoring each quad's [`Quad::diagonal`] triangulation:
/// six indices per quad (`quad.indices()`), quad `i` vertices at `4i..4i+4`.
pub fn build_index_buffer(quads: &[Quad]) -> Vec<u32> {
    let mut indices = Vec::with_capacity(quads.len() * 6);
    for (i, quad) in quads.iter().enumerate() {
        let base = (i * 4) as u32;
        indices.extend(quad.indices().iter().map(|&local| base + u32::from(local)));
    }
    indices
}

/// `true` for quads the alpha-blended pass draws (mirrors the client-port
/// `is_translucent` rule: translucent layer, plus water fluid cells).
pub fn is_translucent_quad(quad: &Quad) -> bool {
    quad.layer == Layer::Translucent || (quad.layer == Layer::Fluid && quad.fluid_kind == WATER)
}

/// Split `quads` into `(opaque, translucent)` preserving input order.
pub fn split_opaque_translucent(quads: &[Quad]) -> (Vec<Quad>, Vec<Quad>) {
    quads.iter().partition(|q| !is_translucent_quad(q))
}

/// Back-to-front translucent quad order against `camera`, through the real
/// `megu_client_mesh_sort_translucent` FFI. `None` when the FFI rejects
/// the input (non-finite camera or quads) — callers draw unsorted then.
pub fn translucent_quad_order(quads: &[Quad], camera: [f32; 3]) -> Option<Vec<u32>> {
    use megu_client_mesh::ffi::megu_client_mesh_sort_translucent;
    let mut count = 0usize;
    // SAFETY: count is writable; null output queries the required capacity.
    let status = unsafe {
        megu_client_mesh_sort_translucent(
            quads.as_ptr(),
            quads.len(),
            camera[0],
            camera[1],
            camera[2],
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    if status != megu_client_mesh::ffi::ABI_OK {
        return None;
    }
    let mut order = vec![0u32; count];
    // SAFETY: `order` has `count` writable u32 slots; inputs unchanged.
    let status = unsafe {
        megu_client_mesh_sort_translucent(
            quads.as_ptr(),
            quads.len(),
            camera[0],
            camera[1],
            camera[2],
            order.as_mut_ptr(),
            order.len(),
            &mut count,
        )
    };
    (status == megu_client_mesh::ffi::ABI_OK).then_some(order)
}

// ---------------------------------------------------------------------------
// wgpu adapter -> client-port capability mapping (pure; testable off-GPU)
// ---------------------------------------------------------------------------

/// Map a `wgpu` vendor PCI id onto the coarse [`GpuVendor`] the client-port
/// backend negotiator expects.
pub fn gpu_vendor_for_id(vendor_id: u32) -> GpuVendor {
    match vendor_id {
        0x10de => GpuVendor::Nvidia,
        0x1002 | 0x1022 => GpuVendor::Amd,
        0x8086 => GpuVendor::Intel,
        0x106b => GpuVendor::Apple,
        _ => GpuVendor::Other,
    }
}

/// Build [`GpuCapabilities`] from real `wgpu` feature/limit queries, so
/// [`resolve_render_backend`] negotiates `MeshShader`/`ComputeIndirect`/
/// `Classic`/`ClassicLowSpec` against the same rules the Vulkan path uses.
pub fn gpu_capabilities_for(
    features: wgpu::Features,
    limits: &wgpu::Limits,
    vendor_id: u32,
) -> GpuCapabilities {
    let mesh = features.contains(wgpu::Features::EXPERIMENTAL_MESH_SHADER);
    GpuCapabilities {
        vendor: gpu_vendor_for_id(vendor_id),
        supports_ext_mesh_shader: mesh,
        max_mesh_output_vertices: if mesh {
            limits.max_mesh_output_vertices
        } else {
            0
        },
        max_mesh_output_primitives: if mesh {
            limits.max_mesh_output_primitives
        } else {
            0
        },
        supports_multi_draw_indirect_count: features
            .contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT),
        supports_compute_shaders: limits.max_compute_workgroups_per_dimension > 0
            && limits.max_compute_invocations_per_workgroup > 0,
    }
}

/// Map real `wgpu` limits onto the shaderpack [`GpuShaderCaps`] profile the
/// `MeguWgslPack` validator checks requirements against. `features` is
/// reserved for capability flags the pack profile may grow.
pub fn gpu_shader_caps_for(
    backend: wgpu::Backend,
    _features: wgpu::Features,
    limits: &wgpu::Limits,
) -> GpuShaderCaps {
    let max_glsl_version = match backend {
        wgpu::Backend::Vulkan | wgpu::Backend::Metal | wgpu::Backend::Dx12 => 460,
        _ => 410,
    };
    let supports_compute = limits.max_compute_workgroups_per_dimension > 0;
    let supports_ssbo = limits.max_storage_buffers_per_shader_stage > 0
        && limits.max_storage_buffer_binding_size > 0;
    GpuShaderCaps {
        max_glsl_version,
        supports_compute,
        supports_ssbo,
        supports_custom_images: limits.max_storage_textures_per_shader_stage > 0,
        max_ssbo_bytes: limits.max_storage_buffer_binding_size,
        max_color_attachments: limits.max_color_attachments,
    }
}

/// Backend bitflags a [`RenderBackendPreference`] asks `wgpu` for.
///
/// Delegates to [`select_display_presentation_policy`] for the detected
/// display server so the OS-aware mask reaches `request_gpu_device` and
/// `probe_adapters`: `Auto`/`MeshShader`/`ComputeIndirect`/`Classic` get
/// the native modern API (Vulkan on Linux, DX12 on Windows, Metal on
/// macOS). `wgpu` is built without any GL backend, so there is no GL
/// fallback on any preference — `ClassicLowSpec` is a feature/limit
/// budget, not a backend switch.
pub fn wgpu_backends_for(pref: RenderBackendPreference) -> wgpu::Backends {
    select_display_presentation_policy(detect_display_server_kind(), pref).preferred_backends
}

/// Present mode a `vsync` toggle should use, preferring the exact mode the
/// surface advertises and degrading gracefully through `AutoVsync`/`AutoNoVsync`.
/// Uncapped mode wants the lowest latency swap the surface supports:
/// `Immediate` → `Mailbox` → `AutoNoVsync` → `Fifo`.
pub fn select_present_mode(vsync: bool, supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    let prefer = |mode: wgpu::PresentMode| supported.contains(&mode).then_some(mode);
    if vsync {
        prefer(wgpu::PresentMode::Fifo).unwrap_or(wgpu::PresentMode::AutoVsync)
    } else {
        prefer(wgpu::PresentMode::Immediate)
            .or_else(|| prefer(wgpu::PresentMode::Mailbox))
            .or_else(|| prefer(wgpu::PresentMode::AutoNoVsync))
            .or_else(|| prefer(wgpu::PresentMode::Fifo))
            .unwrap_or(wgpu::PresentMode::AutoNoVsync)
    }
}

/// Whether the frame loop should call `window.pre_present_notify()`: only
/// under vsync, where compositor pacing applies. With `--vsync off` the
/// `Immediate`/`Mailbox` swap must not wait on a `wl_surface.frame`
/// callback, so the notify is skipped entirely.
pub fn should_call_pre_present_notify(vsync: bool) -> bool {
    vsync
}

/// Present mode a `vsync` toggle should use — kept for existing callers;
/// implemented by [`select_present_mode`].
pub fn present_mode_for(vsync: bool, supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    select_present_mode(vsync, supported)
}

/// Composite alpha mode a surface should prefer: [`Opaque`](wgpu::CompositeAlphaMode::Opaque)
/// first so Wayland compositors (KWin, Mutter, Hyprland, Sway) can bypass
/// alpha blending and put the swapchain on a zero-copy direct-scanout plane
/// with FreeSync/VRR, then `Inherit`, then the first advertised mode, then
/// `Auto` when the surface reports nothing at all.
pub fn preferred_alpha_mode(supported: &[wgpu::CompositeAlphaMode]) -> wgpu::CompositeAlphaMode {
    let prefer = |mode: wgpu::CompositeAlphaMode| supported.contains(&mode).then_some(mode);
    prefer(wgpu::CompositeAlphaMode::Opaque)
        .or_else(|| prefer(wgpu::CompositeAlphaMode::Inherit))
        .or_else(|| supported.first().copied())
        .unwrap_or(wgpu::CompositeAlphaMode::Auto)
}

/// `desired_maximum_frame_latency` for the swapchain: `1` when `vsync` is
/// off so `Immediate`/`Mailbox` presentation keeps input->photon latency at
/// one frame, `2` under `Fifo` vsync so the queue holds one buffered frame
/// without stalling the pipeline.
pub fn preferred_frame_latency(vsync: bool) -> u32 {
    if vsync {
        2
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// Universal cross-OS display & presentation policy
// ---------------------------------------------------------------------------

/// Display server / OS presentation family the windowed client runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayServerKind {
    /// Linux Wayland (`WAYLAND_DISPLAY` set): `wl_surface.frame` pacing,
    /// `zwp_locked_pointer_v1`, compositor direct scanout.
    Wayland,
    /// Linux/Unix X11 (`DISPLAY` set, no Wayland): XI2 raw deltas.
    X11,
    /// Windows 10/11: DXGI swapchain, Independent Flip / MPO.
    Windows,
    /// macOS: `CAMetalLayer` presentation.
    MacOs,
    /// No display server detected (offscreen / `--headless-probe`).
    Headless,
}

/// Detect the display server this process presents through — pure `cfg` +
/// `std::env::var_os`, no display connection. On Unix `WAYLAND_DISPLAY` wins
/// over `DISPLAY` (an XWayland-only session still prefers native Wayland);
/// Windows and macOS are fixed by `target_os`; anything else is headless.
pub fn detect_display_server_kind() -> DisplayServerKind {
    #[cfg(target_os = "windows")]
    {
        DisplayServerKind::Windows
    }
    #[cfg(target_os = "macos")]
    {
        DisplayServerKind::MacOs
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            DisplayServerKind::Wayland
        } else if std::env::var_os("DISPLAY").is_some() {
            DisplayServerKind::X11
        } else {
            DisplayServerKind::Headless
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        DisplayServerKind::Headless
    }
}

/// The presentation/backend negotiation policy for one display server:
/// which `wgpu` backends to enumerate, which cursor-grab order is safe,
/// whether to send the compositor pre-present notify, the swapchain
/// frame-latency targets, and the zero-extent present guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayPresentationPolicy {
    /// Detected display server / OS family.
    pub display: DisplayServerKind,
    /// `wgpu` backends to enumerate, in wgpu's own preference order.
    pub preferred_backends: wgpu::Backends,
    /// `true` tries `CursorGrabMode::Locked` before `Confined` (Wayland
    /// `zwp_locked_pointer_v1` + `zwp_relative_pointer_v1`, macOS, Windows
    /// raw input); `false` tries `Confined` first — X11, where XI2
    /// `DeviceEvent::MouseMotion` already delivers unaccelerated raw deltas
    /// and pointer lock trips X11/RDP cursor quirks.
    pub prefer_locked_pointer_first: bool,
    /// `true` calls `Window::pre_present_notify` right before present —
    /// Wayland `wl_surface.frame` pacing; a safe no-op everywhere else.
    pub call_pre_present_notify: bool,
    /// `true` prefers `CompositeAlphaMode::Opaque` so the compositor can
    /// place the swapchain on a zero-copy direct-scanout plane.
    pub prefer_opaque_direct_scanout: bool,
    /// `desired_maximum_frame_latency` when vsync is off.
    pub uncapped_frame_latency: u32,
    /// `desired_maximum_frame_latency` under `Fifo` vsync.
    pub vsync_frame_latency: u32,
    /// `true` skips surface acquire/present while either extent is `0` —
    /// a minimized Windows `0x0` swapchain panics inside DXGI
    /// `ResizeBuffers`, so zero GPU work is the only safe answer.
    pub skip_zero_extent_present: bool,
    /// Concise F3 diagnostics tag (`"wayland/vulkan"`, `"x11/vulkan"`,
    /// `"windows/dx12-flip"`, `"macos/metal"`).
    pub description: &'static str,
}

impl DisplayPresentationPolicy {
    /// `desired_maximum_frame_latency` for the given vsync mode.
    pub fn frame_latency(&self, vsync: bool) -> u32 {
        if vsync {
            self.vsync_frame_latency
        } else {
            self.uncapped_frame_latency
        }
    }

    /// `desired_maximum_frame_latency` while an FPS cap drives the
    /// WaitUntil pacing loop: always `1` — the cap's software deadline
    /// already paces frames, so an extra buffered frame would only add
    /// input->photon latency.
    pub fn frame_latency_capped(&self) -> u32 {
        1
    }
}

/// Select the presentation policy for `display` under backend preference
/// `pref`. Every preference — `ClassicLowSpec` included — runs pure
/// `wgpu::Backends::PRIMARY` native APIs (Vulkan / DX12 / Metal); the
/// low-spec profile is a feature/limit budget, never a GL fallback.
/// The native modern API leads: Vulkan on Linux, DX12 on Windows (native
/// DXGI Independent Flip / Multi-Plane Overlay, zero-flicker Alt+Tab),
/// Metal on macOS.
pub fn select_display_presentation_policy(
    display: DisplayServerKind,
    _pref: RenderBackendPreference,
) -> DisplayPresentationPolicy {
    let preferred_backends = match display {
        DisplayServerKind::Wayland | DisplayServerKind::X11 => wgpu::Backends::VULKAN,
        DisplayServerKind::Windows => wgpu::Backends::DX12 | wgpu::Backends::VULKAN,
        DisplayServerKind::MacOs => wgpu::Backends::METAL,
        DisplayServerKind::Headless => wgpu::Backends::PRIMARY,
    };
    DisplayPresentationPolicy {
        display,
        preferred_backends,
        prefer_locked_pointer_first: display != DisplayServerKind::X11,
        call_pre_present_notify: true,
        prefer_opaque_direct_scanout: true,
        // Metal triple-buffers: latency 2 keeps a frame in flight so
        // `Immediate`-style present never stalls the pipeline.
        uncapped_frame_latency: match display {
            DisplayServerKind::MacOs => 2,
            _ => 1,
        },
        vsync_frame_latency: 2,
        skip_zero_extent_present: true,
        description: match display {
            DisplayServerKind::Wayland => "wayland/vulkan",
            DisplayServerKind::X11 => "x11/vulkan",
            DisplayServerKind::Windows => "windows/dx12-flip",
            DisplayServerKind::MacOs => "macos/metal",
            DisplayServerKind::Headless => "headless/auto",
        },
    }
}

// ---------------------------------------------------------------------------
// Dirty-only UI upload key (FNV-1a over the rasterized draw list)
// ---------------------------------------------------------------------------

fn fnv1a_rect(hash: &mut Fnv1a64, rect: &UiRect) {
    hash.update_i32(rect.x);
    hash.update_i32(rect.y);
    hash.update_u32(rect.w);
    hash.update_u32(rect.h);
}

fn fnv1a_color(hash: &mut Fnv1a64, color: &UiColor) {
    hash.update(&[color.r, color.g, color.b, color.a]);
}

/// Deterministic 64-bit FNV-1a fingerprint over everything
/// [`rasterize_draw_list`](megu_ui::draw::rasterize_draw_list) reads:
/// the framebuffer size `(width, height)` plus every [`UiCommand`] in
/// `list` — variant, rects, colors, text payloads and progress fractions.
/// Two draw lists that paint identical pixels share a fingerprint, so the
/// window loop can skip the CPU rasterization and the PCIe texture upload
/// on frames where the HUD did not change.
pub fn fingerprint_draw_list(width: u32, height: u32, list: &UiDrawList) -> u64 {
    let mut hash = Fnv1a64::new();
    hash.update_u32(width);
    hash.update_u32(height);
    for command in &list.commands {
        match command {
            UiCommand::FillRect { rect, color } => {
                hash.update_u8(0);
                fnv1a_rect(&mut hash, rect);
                fnv1a_color(&mut hash, color);
            }
            UiCommand::BorderRect {
                rect,
                color,
                thickness,
            } => {
                hash.update_u8(1);
                fnv1a_rect(&mut hash, rect);
                fnv1a_color(&mut hash, color);
                hash.update_u32(*thickness);
            }
            UiCommand::Text {
                x,
                y,
                scale,
                color,
                text,
            } => {
                hash.update_u8(2);
                hash.update_i32(*x);
                hash.update_i32(*y);
                hash.update_u32(*scale);
                fnv1a_color(&mut hash, color);
                hash.update_str(text);
                hash.update_u8(0xff);
            }
            UiCommand::ProgressBar {
                rect,
                fraction,
                fill,
                back,
                border,
            } => {
                hash.update_u8(3);
                fnv1a_rect(&mut hash, rect);
                // `-0.0 == 0.0` under PartialEq but hashes differently via
                // `to_bits`, so equal draw lists must normalize it.
                let fraction = if *fraction == 0.0 {
                    0.0f32.to_bits()
                } else {
                    fraction.to_bits()
                };
                hash.update_u32(fraction);
                fnv1a_color(&mut hash, fill);
                fnv1a_color(&mut hash, back);
                fnv1a_color(&mut hash, border);
            }
            UiCommand::Sprite { rect, sprite } => {
                hash.update_u8(5);
                fnv1a_rect(&mut hash, rect);
                hash.update_u8(*sprite as u8);
            }
            UiCommand::ItemIcon { rect, item_id } => {
                hash.update_u8(4);
                fnv1a_rect(&mut hash, rect);
                hash.update_str(item_id);
                hash.update_u8(0xff);
            }
        }
    }
    hash.finish()
}

// ---------------------------------------------------------------------------
// Demo chunk scene (real client-port mesher, real packed-vertex buffers)
// ---------------------------------------------------------------------------

/// `state_id` values the demo scene uses; the deterministic palette in the
/// terrain WGSL maps any registry id to a stable color, so these just need
/// to be distinct.
pub mod demo_states {
    pub const GRASS: u32 = 10;
    pub const DIRT: u32 = 11;
    pub const STONE: u32 = 12;
    pub const LOG: u32 = 13;
    pub const LEAVES: u32 = 14;
    pub const GLASS: u32 = 15;
    pub const WATER_BLOCK: u32 = 16;
}

/// A meshed demo section: opaque quads, translucent quads, and the section
/// origin in world space.
#[derive(Clone, Debug, Default)]
pub struct DemoScene {
    /// `Layer::{Opaque,Cutout}` quads, in mesher order.
    pub opaque_quads: Vec<Quad>,
    /// Translucent + water quads, back-to-front for the spawn camera.
    pub translucent_quads: Vec<Quad>,
    /// World-space origin of the section these quads mesh.
    pub section_origin: [f32; 3],
}

impl DemoScene {
    /// Total quad count across both passes.
    pub fn quad_count(&self) -> usize {
        self.opaque_quads.len() + self.translucent_quads.len()
    }
}

/// Build the `--offline-demo` world: one 16³ section meshed through the real
/// `megu_client_mesh::mesh_section` — a heightmap of grass/dirt/stone, a
/// tree, a stained-glass block, and a water pool — split into the opaque and
/// translucent draw lists (translucent sorted back-to-front from `camera`).
pub fn build_demo_scene(camera: [f32; 3]) -> DemoScene {
    use demo_states::*;
    let mut section = Section::filled(Block::AIR, megu_client_mesh::Light::new(0, 15));
    let full_light = megu_client_mesh::Light::new(0, 15);
    // Heightmap: gentle 4..9 rolling terrain, stone deep, dirt middle,
    // grass on top.
    for x in 0..16 {
        for z in 0..16 {
            let h = 6 + ((x * 7 + z * 11 + x * z) % 5);
            for y in 0..h {
                let state = if y + 1 == h {
                    GRASS
                } else if y + 3 >= h {
                    DIRT
                } else {
                    STONE
                };
                section.set(x, y, z, Block::opaque_cube(state), full_light);
            }
        }
    }
    // Water pool in the low corner: real fluid quads on the translucent pass
    // (fluid_level 0 = source block, the vanilla 8/9-height surface).
    let water = Block {
        state_id: WATER_BLOCK,
        fluid_kind: WATER,
        ..Block::AIR
    };
    for x in 1..4 {
        for z in 1..4 {
            let h = 6 + ((x * 7 + z * 11 + x * z) % 5);
            for y in h..8 {
                section.set(x, y, z, water, full_light);
            }
        }
    }
    // A small tree: log column + leaf cube.
    for y in 9..12 {
        section.set(12, y, 12, Block::opaque_cube(LOG), full_light);
    }
    for x in 10..=14 {
        for y in 11..=13 {
            for z in 10..=14 {
                if (x, y, z) != (12, 11, 12) && (x, y, z) != (12, 12, 12) {
                    section.set(x, y, z, Block::opaque_cube(LEAVES), full_light);
                }
            }
        }
    }
    // Stained-glass panes: real `Layer::Translucent` quads with a tint.
    let glass = Block {
        state_id: GLASS,
        tint: 0x3366cc,
        flags: megu_client_mesh::TRANSLUCENT | megu_client_mesh::TINTED,
        ..Block::AIR
    };
    for z in 6..=10 {
        section.set(5, 9, z, glass, full_light);
    }

    let quads = mesh_section(&section.view());
    let (opaque, mut translucent) = split_opaque_translucent(&quads);
    // The sorter returns exactly the translucent quads, back-to-front.
    if let Some(order) = translucent_quad_order(&quads, camera) {
        if order.len() == translucent.len() && order.iter().all(|&i| (i as usize) < quads.len()) {
            translucent = order.iter().map(|&i| quads[i as usize]).collect();
        }
    }
    DemoScene {
        opaque_quads: opaque,
        translucent_quads: translucent,
        section_origin: [0.0, 0.0, 0.0],
    }
}

// ---------------------------------------------------------------------------
// UI framebuffer sizing (client-ui RGBA -> wgpu texture upload)
// ---------------------------------------------------------------------------

/// Channels in the `megu_ui` software-rasterized framebuffer.
pub const UI_RGBA_CHANNELS: usize = 4;

/// Expected byte length of a `rasterize_draw_list` framebuffer.
pub fn ui_rgba_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * UI_RGBA_CHANNELS
}

/// Bytes-per-row `queue.write_texture`/`copy_texture_to_buffer` must use:
/// `width * 4` rounded up to [`wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`] (256).
/// Only padded copies need the alignment — direct `write_texture` uploads
/// take tightly-packed rows.
pub fn aligned_copy_bytes_per_row(width: u32) -> u32 {
    let row = width * UI_RGBA_CHANNELS as u32;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    row.div_ceil(align) * align
}

// ---------------------------------------------------------------------------
// UI blit WGSL (fullscreen alpha-blended textured quad)
// ---------------------------------------------------------------------------

/// Fullscreen UI/HUD blit: samples the `client-ui` RGBA framebuffer texture
/// and alpha-blends it over the terrain frame. A single fullscreen triangle
/// keeps the vertex stage buffer-free.
pub const UI_BLIT_WGSL: &str = "\
// UI/HUD overlay: fullscreen textured triangle, alpha-blended.
struct UiVertex {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn ui_blit_vs(@builtin(vertex_index) index: u32) -> UiVertex {
    // Fullscreen triangle: (-1,-1) (3,-1) (-1,3).
    let pos = vec2<f32>(
        f32(i32(index & 1u) * 4 - 1),
        f32(i32(index >> 1u) * 4 - 1),
    );
    var out: UiVertex;
    out.clip = vec4<f32>(pos, 0.0, 1.0);
    out.uv = vec2<f32>(pos.x * 0.5 + 0.5, 0.5 - pos.y * 0.5);
    return out;
}

@group(0) @binding(0) var ui_tex: texture_2d<f32>;
@group(0) @binding(1) var ui_sampler: sampler;

@fragment
fn ui_blit_fs(in: UiVertex) -> @location(0) vec4<f32> {
    return textureSample(ui_tex, ui_sampler, in.uv);
}
";

/// Validate the backend negotiated for `pref` against `caps` — one call so
/// the probe printout and the renderer agree on the active pipeline.
pub fn negotiate_pipeline(
    pref: RenderBackendPreference,
    caps: &GpuCapabilities,
) -> megu_client_mesh::BackendResolution {
    resolve_render_backend(pref, caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_flag_parses_signed_i64_and_rejects_garbage() {
        let cfg = parse_gpu_args(&["--seed", "-42"]).expect("--seed -42");
        assert_eq!(cfg.seed, Some(-42));
        let cfg = parse_gpu_args(&["--seed=9223372036854775807"]).expect("--seed=max");
        assert_eq!(cfg.seed, Some(i64::MAX));
        assert_eq!(parse_gpu_args::<&str>(&[]).unwrap().seed, None);
        assert!(parse_gpu_args(&["--seed", "notanum"]).is_err());
        assert!(parse_gpu_args(&["--seed"]).is_err());
    }

    #[test]
    fn alpha_mode_prefers_opaque_for_direct_scanout() {
        assert_eq!(
            preferred_alpha_mode(&[
                wgpu::CompositeAlphaMode::PostMultiplied,
                wgpu::CompositeAlphaMode::Opaque,
                wgpu::CompositeAlphaMode::PreMultiplied,
            ]),
            wgpu::CompositeAlphaMode::Opaque
        );
    }

    #[test]
    fn alpha_mode_falls_back_through_inherit_first_auto() {
        assert_eq!(
            preferred_alpha_mode(&[
                wgpu::CompositeAlphaMode::PreMultiplied,
                wgpu::CompositeAlphaMode::Inherit,
            ]),
            wgpu::CompositeAlphaMode::Inherit
        );
        assert_eq!(
            preferred_alpha_mode(&[wgpu::CompositeAlphaMode::PreMultiplied]),
            wgpu::CompositeAlphaMode::PreMultiplied
        );
        assert_eq!(preferred_alpha_mode(&[]), wgpu::CompositeAlphaMode::Auto);
    }

    #[test]
    fn frame_latency_is_one_uncapped_two_vsynced() {
        assert_eq!(preferred_frame_latency(false), 1);
        assert_eq!(preferred_frame_latency(true), 2);
    }

    #[test]
    fn detect_display_server_kind_matches_env_and_target() {
        let kind = detect_display_server_kind();
        #[cfg(target_os = "windows")]
        assert_eq!(kind, DisplayServerKind::Windows);
        #[cfg(target_os = "macos")]
        assert_eq!(kind, DisplayServerKind::MacOs);
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let expected = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                DisplayServerKind::Wayland
            } else if std::env::var_os("DISPLAY").is_some() {
                DisplayServerKind::X11
            } else {
                DisplayServerKind::Headless
            };
            assert_eq!(kind, expected);
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        assert_eq!(kind, DisplayServerKind::Headless);
    }

    #[test]
    fn wayland_policy_prefers_vulkan_locked_pointer_and_low_latency() {
        let policy = select_display_presentation_policy(
            DisplayServerKind::Wayland,
            RenderBackendPreference::Auto,
        );
        assert_eq!(policy.display, DisplayServerKind::Wayland);
        assert_eq!(policy.preferred_backends, wgpu::Backends::VULKAN);
        assert!(policy.prefer_locked_pointer_first);
        assert!(policy.call_pre_present_notify);
        assert!(policy.prefer_opaque_direct_scanout);
        assert_eq!(policy.uncapped_frame_latency, 1);
        assert_eq!(policy.vsync_frame_latency, 2);
        assert!(policy.skip_zero_extent_present);
        assert_eq!(policy.description, "wayland/vulkan");
    }

    #[test]
    fn x11_policy_prefers_vulkan_and_confined_pointer_first() {
        let policy = select_display_presentation_policy(
            DisplayServerKind::X11,
            RenderBackendPreference::Auto,
        );
        assert_eq!(policy.preferred_backends, wgpu::Backends::VULKAN);
        assert!(!policy.prefer_locked_pointer_first);
        assert_eq!(policy.uncapped_frame_latency, 1);
        assert_eq!(policy.description, "x11/vulkan");
    }

    #[test]
    fn windows_policy_prefers_dx12_and_guards_zero_extent_present() {
        let policy = select_display_presentation_policy(
            DisplayServerKind::Windows,
            RenderBackendPreference::Auto,
        );
        assert!(policy.preferred_backends.contains(wgpu::Backends::DX12));
        assert!(policy.preferred_backends.contains(wgpu::Backends::VULKAN));
        assert!(policy.prefer_locked_pointer_first);
        assert!(policy.skip_zero_extent_present);
        assert_eq!(policy.uncapped_frame_latency, 1);
        assert_eq!(policy.description, "windows/dx12-flip");
    }

    #[test]
    fn macos_policy_selects_metal_and_triple_buffer_latency() {
        let policy = select_display_presentation_policy(
            DisplayServerKind::MacOs,
            RenderBackendPreference::Auto,
        );
        assert_eq!(policy.preferred_backends, wgpu::Backends::METAL);
        assert_eq!(policy.uncapped_frame_latency, 2);
        assert_eq!(policy.vsync_frame_latency, 2);
        assert!(policy.prefer_locked_pointer_first);
        assert_eq!(policy.description, "macos/metal");
    }

    #[test]
    fn classic_low_spec_is_primary_on_every_display_server() {
        for display in [
            DisplayServerKind::Wayland,
            DisplayServerKind::X11,
            DisplayServerKind::Windows,
            DisplayServerKind::MacOs,
            DisplayServerKind::Headless,
        ] {
            let policy = select_display_presentation_policy(
                display,
                RenderBackendPreference::ClassicLowSpec,
            );
            assert!(
                wgpu::Backends::PRIMARY.contains(policy.preferred_backends),
                "{display:?}"
            );
        }
    }

    fn hud_draw_list(text: &str) -> UiDrawList {
        let mut list = UiDrawList::new();
        list.fill_rect(UiRect::new(4, 4, 120, 40), UiColor::rgb(20, 20, 24));
        list.border_rect(UiRect::new(4, 4, 120, 40), UiColor::rgb(80, 88, 110), 1);
        list.text(8, 10, 1, UiColor::rgb(238, 238, 238), text);
        list.progress_bar(
            UiRect::new(8, 30, 112, 8),
            0.5,
            UiColor::rgb(210, 45, 45),
            UiColor::rgb(40, 42, 50),
            UiColor::rgb(14, 15, 20),
        );
        list
    }

    #[test]
    fn fingerprint_is_stable_for_equal_draw_lists() {
        let a = hud_draw_list("health 20/20");
        let b = hud_draw_list("health 20/20");
        assert_eq!(a, b);
        assert_eq!(
            fingerprint_draw_list(1280, 720, &a),
            fingerprint_draw_list(1280, 720, &b)
        );
    }

    #[test]
    fn fingerprint_changes_on_text_rect_color_and_viewport() {
        let base = hud_draw_list("health 20/20");
        let fp = fingerprint_draw_list(1280, 720, &base);

        let text_changed = hud_draw_list("health 19/20");
        assert_ne!(fp, fingerprint_draw_list(1280, 720, &text_changed));

        let mut rect_changed = base.clone();
        if let UiCommand::FillRect { rect, .. } = &mut rect_changed.commands[0] {
            rect.w += 1;
        }
        assert_ne!(fp, fingerprint_draw_list(1280, 720, &rect_changed));

        let mut color_changed = base.clone();
        if let UiCommand::FillRect { color, .. } = &mut color_changed.commands[0] {
            color.g += 1;
        }
        assert_ne!(fp, fingerprint_draw_list(1280, 720, &color_changed));

        let mut fraction_changed = base.clone();
        if let UiCommand::ProgressBar { fraction, .. } = &mut fraction_changed.commands[3] {
            *fraction = 0.75;
        }
        assert_ne!(fp, fingerprint_draw_list(1280, 720, &fraction_changed));

        assert_ne!(fp, fingerprint_draw_list(1920, 720, &base));
        assert_ne!(fp, fingerprint_draw_list(1280, 1080, &base));
    }

    #[test]
    fn fingerprint_treats_negative_zero_fraction_as_zero() {
        let mut neg = UiDrawList::new();
        neg.progress_bar(
            UiRect::new(0, 0, 10, 10),
            -0.0,
            UiColor::rgb(1, 1, 1),
            UiColor::rgb(2, 2, 2),
            UiColor::rgb(3, 3, 3),
        );
        let mut pos = UiDrawList::new();
        pos.progress_bar(
            UiRect::new(0, 0, 10, 10),
            0.0,
            UiColor::rgb(1, 1, 1),
            UiColor::rgb(2, 2, 2),
            UiColor::rgb(3, 3, 3),
        );
        assert_eq!(
            fingerprint_draw_list(64, 64, &neg),
            fingerprint_draw_list(64, 64, &pos)
        );
    }
}
