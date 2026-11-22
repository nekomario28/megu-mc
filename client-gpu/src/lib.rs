// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc-spine-client-gpu`: the real windowed GPU client crate.
//!
//! Layers on top of the headless client stack:
//!
//! * [`renderer`] — a real `wgpu` backend (Vulkan / Metal / DX12 / GL) that
//!   uploads `mc_spine_client_mesh` packed quad geometry, compiles the WGSL
//!   shader suite from `mc_spine_client::shaders`, composites the
//!   `mc_spine_ui` software-rasterized RGBA framebuffer, and validates Megu
//!   native WGSL shaderpack modules against the live `wgpu::Device`.
//! * [`window`] — a `winit` 0.30 `ApplicationHandler` driving the window,
//!   FPS input and the `FrameBudgetGovernor` pacing loop.
//!
//! This module itself stays GPU-API-free where it can: camera math, the
//! quad->buffer conversion, the capability mapping and the CLI parser are
//! pure functions unit tests exercise without an adapter.

pub mod atmosphere;
pub mod live_session;
pub mod renderer;
pub mod subworld_render;
pub mod window;

use std::path::PathBuf;

use mc_spine_client::cli::parse_host_port;
use mc_spine_client_mesh::ffi::PackedVertex;
use mc_spine_client_mesh::{
    mesh_section, resolve_render_backend, Block, GpuCapabilities, GpuShaderCaps, GpuVendor, Layer,
    Quad, RenderBackendPreference, Section, WATER,
};
use mc_spine_ui::draw::{UiColor, UiCommand, UiDrawList, UiRect};

/// Crate/binary name used in diagnostics.
pub const CLIENT_GPU_NAME: &str = "mc-spine-client-gpu";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Everything `mc-spine-client-gpu` accepts on the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuCliConfig {
    /// `--connect <host[:port]>` — the server the window session targets.
    /// Connect to a live offline-authenticated Protocol 767/777 server.
    pub connect: Option<(String, u16)>,
    /// Offline-mode player name, 1–16 ASCII letters/digits/underscores.
    pub username: String,
    /// Minecraft protocol, 767 (1.21.1) or 777 (26.3).
    pub mc_version: String,
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
    /// `--version`.
    pub show_version: bool,
}

impl GpuCliConfig {
    fn new() -> Self {
        Self {
            vsync: true,
            username: "MeguGpu".to_owned(),
            mc_version: "767".to_owned(),
            ..Self::default()
        }
    }
}

impl Default for GpuCliConfig {
    fn default() -> Self {
        Self {
            connect: None,
            username: "MeguGpu".into(),
            mc_version: "767".into(),
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

/// Parse the GPU-client argument vector (`argv[1..]`). Unknown flags and
/// missing values fail closed with a human-readable error.
pub fn parse_gpu_args<S: AsRef<str>>(args: &[S]) -> Result<GpuCliConfig, String> {
    let mut cfg = GpuCliConfig::new();
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
            "--version" | "-V" => cfg.show_version = true,
            "--connect" => {
                let raw = take_value(&mut i)?;
                cfg.connect =
                    Some(parse_host_port(&raw, 25565).map_err(|e| format!("--connect: {e}"))?);
            }
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
            "--mc-version" => {
                let version = take_value(&mut i)?;
                mc_spine_client::cli::parse_mc_version(&version)?;
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
    Ok(cfg)
}

/// Usage text for `--help`.
pub fn gpu_help_text() -> String {
    format!(
        "{CLIENT_GPU_NAME} {version} — real wgpu windowed client for mc-spine (1.21.1 / protocol 767)\n\
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
         \x20   -h, --help                  print this help\n\
         \x20   -V, --version               print the version\n\
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

    /// World-to-view matrix (column-major for WGSL upload).
    pub fn view_matrix(&self) -> Mat4 {
        let f = self.forward_vector();
        let s = self.right_vector();
        let u = cross(s, f);
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
                "mc_spine_client_mesh_pack_vertices rejected the quads (unrepresentable or \
                 unaligned input)"
            ),
            Self::Overflow => write!(f, "quad count overflows the packed vertex buffer"),
        }
    }
}

impl std::error::Error for PackQuadsError {}

/// Pack `quads` through the real `mc_spine_client_mesh` C ABI
/// (`mc_spine_client_mesh_pack_vertices`): four 16-byte [`PackedVertex`]s per
/// quad, in input order. Fails closed when a quad is not representable in
/// the packed format (the [`ffi` contract](PackedVertex) covers that).
pub fn pack_quads_to_vertices(quads: &[Quad]) -> Result<Vec<PackedVertex>, PackQuadsError> {
    use mc_spine_client_mesh::ffi::mc_spine_client_mesh_pack_vertices;
    let mut count = 0usize;
    // Query the required vertex count first (null output, zero capacity).
    // SAFETY: count is a valid writable pointer; all other pointers are null
    // or readable for quad_count elements.
    let status = unsafe {
        mc_spine_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    if status != mc_spine_client_mesh::ffi::ABI_OK {
        return Err(PackQuadsError::InvalidArgument);
    }
    let mut out = vec![PackedVertex::default(); count];
    // SAFETY: `out` has exactly `count` writable vertices; inputs unchanged.
    let status = unsafe {
        mc_spine_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            out.as_mut_ptr(),
            out.len(),
            &mut count,
        )
    };
    match status {
        mc_spine_client_mesh::ffi::ABI_OK => Ok(out),
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
/// `mc_spine_client_mesh_sort_translucent` FFI. `None` when the FFI rejects
/// the input (non-finite camera or quads) — callers draw unsorted then.
pub fn translucent_quad_order(quads: &[Quad], camera: [f32; 3]) -> Option<Vec<u32>> {
    use mc_spine_client_mesh::ffi::mc_spine_client_mesh_sort_translucent;
    let mut count = 0usize;
    // SAFETY: count is writable; null output queries the required capacity.
    let status = unsafe {
        mc_spine_client_mesh_sort_translucent(
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
    if status != mc_spine_client_mesh::ffi::ABI_OK {
        return None;
    }
    let mut order = vec![0u32; count];
    // SAFETY: `order` has `count` writable u32 slots; inputs unchanged.
    let status = unsafe {
        mc_spine_client_mesh_sort_translucent(
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
    (status == mc_spine_client_mesh::ffi::ABI_OK).then_some(order)
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
/// `Auto`/`MeshShader`/`ComputeIndirect`/`Classic` all want the primary
/// modern APIs first (Vulkan/Metal/DX12) with GL kept as the fallback
/// wgpu itself offers; `ClassicLowSpec` deliberately drops to the GL-only
/// downlevel path the legacy-Intel profile was built for.
pub fn wgpu_backends_for(pref: RenderBackendPreference) -> wgpu::Backends {
    match pref {
        RenderBackendPreference::ClassicLowSpec => wgpu::Backends::GL,
        _ => wgpu::Backends::PRIMARY | wgpu::Backends::SECONDARY,
    }
}

/// Present mode a `vsync` toggle should use, preferring the exact mode the
/// surface advertises and degrading gracefully through `AutoVsync`/`AutoNoVsync`.
pub fn present_mode_for(vsync: bool, supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    let prefer = |mode: wgpu::PresentMode| supported.contains(&mode).then_some(mode);
    if vsync {
        prefer(wgpu::PresentMode::Fifo).unwrap_or(wgpu::PresentMode::AutoVsync)
    } else {
        prefer(wgpu::PresentMode::Mailbox)
            .or_else(|| prefer(wgpu::PresentMode::Immediate))
            .unwrap_or(wgpu::PresentMode::AutoNoVsync)
    }
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
// Dirty-only UI upload key (FNV-1a over the rasterized draw list)
// ---------------------------------------------------------------------------

const FNV1A_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a_bytes(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(FNV1A_PRIME);
    }
}

fn fnv1a_u32(hash: &mut u64, value: u32) {
    fnv1a_bytes(hash, &value.to_le_bytes());
}

fn fnv1a_i32(hash: &mut u64, value: i32) {
    fnv1a_bytes(hash, &value.to_le_bytes());
}

fn fnv1a_rect(hash: &mut u64, rect: &UiRect) {
    fnv1a_i32(hash, rect.x);
    fnv1a_i32(hash, rect.y);
    fnv1a_u32(hash, rect.w);
    fnv1a_u32(hash, rect.h);
}

fn fnv1a_color(hash: &mut u64, color: &UiColor) {
    fnv1a_bytes(hash, &[color.r, color.g, color.b, color.a]);
}

/// Deterministic 64-bit FNV-1a fingerprint over everything
/// [`rasterize_draw_list`](mc_spine_ui::draw::rasterize_draw_list) reads:
/// the framebuffer size `(width, height)` plus every [`UiCommand`] in
/// `list` — variant, rects, colors, text payloads and progress fractions.
/// Two draw lists that paint identical pixels share a fingerprint, so the
/// window loop can skip the CPU rasterization and the PCIe texture upload
/// on frames where the HUD did not change.
pub fn fingerprint_draw_list(width: u32, height: u32, list: &UiDrawList) -> u64 {
    let mut hash = FNV1A_OFFSET;
    fnv1a_u32(&mut hash, width);
    fnv1a_u32(&mut hash, height);
    for command in &list.commands {
        match command {
            UiCommand::FillRect { rect, color } => {
                fnv1a_bytes(&mut hash, &[0]);
                fnv1a_rect(&mut hash, rect);
                fnv1a_color(&mut hash, color);
            }
            UiCommand::BorderRect {
                rect,
                color,
                thickness,
            } => {
                fnv1a_bytes(&mut hash, &[1]);
                fnv1a_rect(&mut hash, rect);
                fnv1a_color(&mut hash, color);
                fnv1a_u32(&mut hash, *thickness);
            }
            UiCommand::Text {
                x,
                y,
                scale,
                color,
                text,
            } => {
                fnv1a_bytes(&mut hash, &[2]);
                fnv1a_i32(&mut hash, *x);
                fnv1a_i32(&mut hash, *y);
                fnv1a_u32(&mut hash, *scale);
                fnv1a_color(&mut hash, color);
                fnv1a_bytes(&mut hash, text.as_bytes());
                fnv1a_bytes(&mut hash, &[0xff]);
            }
            UiCommand::ProgressBar {
                rect,
                fraction,
                fill,
                back,
                border,
            } => {
                fnv1a_bytes(&mut hash, &[3]);
                fnv1a_rect(&mut hash, rect);
                // `-0.0 == 0.0` under PartialEq but hashes differently via
                // `to_bits`, so equal draw lists must normalize it.
                let fraction = if *fraction == 0.0 {
                    0.0f32.to_bits()
                } else {
                    fraction.to_bits()
                };
                fnv1a_u32(&mut hash, fraction);
                fnv1a_color(&mut hash, fill);
                fnv1a_color(&mut hash, back);
                fnv1a_color(&mut hash, border);
            }
        }
    }
    hash
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
/// `mc_spine_client_mesh::mesh_section` — a heightmap of grass/dirt/stone, a
/// tree, a stained-glass block, and a water pool — split into the opaque and
/// translucent draw lists (translucent sorted back-to-front from `camera`).
pub fn build_demo_scene(camera: [f32; 3]) -> DemoScene {
    use demo_states::*;
    let mut section = Section::filled(Block::AIR, mc_spine_client_mesh::Light::new(0, 15));
    let full_light = mc_spine_client_mesh::Light::new(0, 15);
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
        flags: mc_spine_client_mesh::TRANSLUCENT | mc_spine_client_mesh::TINTED,
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

/// Channels in the `mc_spine_ui` software-rasterized framebuffer.
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
) -> mc_spine_client_mesh::BackendResolution {
    resolve_render_backend(pref, caps)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            preferred_alpha_mode(&[]),
            wgpu::CompositeAlphaMode::Auto
        );
    }

    #[test]
    fn frame_latency_is_one_uncapped_two_vsynced() {
        assert_eq!(preferred_frame_latency(false), 1);
        assert_eq!(preferred_frame_latency(true), 2);
    }

    fn hud_draw_list(text: &str) -> UiDrawList {
        let mut list = UiDrawList::new();
        list.fill_rect(UiRect::new(4, 4, 120, 40), UiColor::rgb(20, 20, 24));
        list.border_rect(
            UiRect::new(4, 4, 120, 40),
            UiColor::rgb(80, 88, 110),
            1,
        );
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
