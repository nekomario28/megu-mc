// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Zero-dependency 3D software rasterizer + RFC 2083 PNG encoder.
//!
//! Consumes the packed 16-byte `PackedVertex` buffers and `sort_translucent`
//! quad indices produced by `client-port`, projects them through a perspective
//! camera, shades with face normal + AO + sky/block light and a deterministic
//! block-ID color palette, then alpha-blends translucent quads back-to-front.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use mc_spine_client_mesh::ffi::PackedVertex;
use mc_spine_client_mesh::{
    box_facing_mask, bucket_draw_mask, GraphCuller, RenderRegions, SectionCoord, DIRECTION_BUCKETS,
    DIRECTION_BUCKET_UNASSIGNED,
};

use crate::protocol::zlib_compress_stored;
use crate::settings::{ClientSettings, CloudMode, ParticleLevel};
use crate::world::WorldState;

const NEAR_Z: f64 = 0.08;
const FAR_Z: f64 = 1024.0;
const DEFAULT_FOV_DEG: f64 = 70.0;

// Packed vertex `attributes` bit layout (see mc_spine_client_mesh.h).
const ATTR_DIRECTION_MASK: u32 = 0x7;
const ATTR_AO_SHIFT: u32 = 3;
const ATTR_BLOCK_LIGHT_SHIFT: u32 = 5;
const ATTR_SKY_LIGHT_SHIFT: u32 = 9;
const ATTR_LAYER_SHIFT: u32 = 13;
const ATTR_FLUID_KIND_SHIFT: u32 = 15;
const ATTR_DIAGONAL_BIT: u32 = 1 << 17;

// client-port layers.
const LAYER_TRANSLUCENT: u32 = 2;
const LAYER_FLUID: u32 = 3;

const FLUID_WATER: u32 = 1;
const FLUID_LAVA: u32 = 2;

/// Everything [`render_frame_with_config`] honors from `ClientSettings`.
/// FOV, smooth lighting and the draw-distance cap take effect in this
/// renderer; the remaining fields are carried 1:1 so a windowed frontend
/// can honor them without re-mapping (the headless rasterizer has no
/// cloud, particle, entity, or present pass to apply them to).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderConfig {
    /// Vertical field of view in degrees.
    pub fov_deg: f64,
    /// Per-vertex ambient occlusion; off keeps flat per-face shading only.
    pub smooth_lighting: bool,
    /// Chebyshev chunk-column draw cap around the camera; `None` draws
    /// everything the visibility culler reaches.
    pub view_distance: Option<i32>,
    /// Cloud drawing preference (no cloud pass in the headless rasterizer).
    pub clouds: CloudMode,
    /// Particle emission preference (no particle pass in the headless rasterizer).
    pub particles: ParticleLevel,
    /// Entity shadow preference (no entity pass in the headless rasterizer).
    pub entity_shadows: bool,
    /// VSync preference for windowed presentation.
    pub vsync: bool,
    /// Frame pacing cap for windowed presentation (`None` = uncapped).
    pub max_fps: Option<u32>,
}

impl RenderConfig {
    /// Map a resolved `ClientSettings` onto the renderer: `view_distance`
    /// becomes the full-detail draw cap and `fov` the projection angle.
    pub fn from_settings(settings: &ClientSettings) -> Self {
        Self {
            fov_deg: f64::from(settings.fov),
            smooth_lighting: settings.smooth_lighting,
            view_distance: Some(i32::from(settings.view_distance)),
            clouds: settings.clouds,
            particles: settings.particles,
            entity_shadows: settings.entity_shadows,
            vsync: settings.vsync,
            max_fps: settings.max_fps,
        }
    }
}

impl Default for RenderConfig {
    /// Legacy behavior: 70 deg FOV, smooth lighting, no distance cap.
    fn default() -> Self {
        Self {
            fov_deg: DEFAULT_FOV_DEG,
            smooth_lighting: true,
            view_distance: None,
            clouds: CloudMode::Fancy,
            particles: ParticleLevel::All,
            entity_shadows: true,
            vsync: true,
            max_fps: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderStats {
    pub render_ms: f64,
    pub triangles_drawn: u64,
    pub pixels_shaded: u64,
    /// Renderable sections the `GraphCuller` BFS reached and that had
    /// packed geometry submitted to the rasterizer.
    pub drawn_sections: u64,
    /// Renderable sections rejected by frustum, occlusion, or the
    /// draw-distance cap.
    pub culled_sections: u64,
    /// Batched multi-draw submissions for the regions holding drawn
    /// sections (one `MultiDrawCommand` per region x facing bucket).
    pub region_draw_calls: u64,
    /// Non-empty direction buckets submitted across drawn sections.
    pub buckets_drawn: u64,
    /// Non-empty direction buckets skipped as backfacing across drawn
    /// sections.
    pub buckets_culled: u64,
}

/// A rendered RGB frame ready for PNG encoding.
pub struct RenderedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB8 pixels, row-major.
    pub rgb: Vec<u8>,
    pub stats: RenderStats,
}

impl RenderedFrame {
    pub fn encode_png(&self) -> Vec<u8> {
        encode_png_rgb(self.width, self.height, &self.rgb)
    }

    pub fn write_png(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
        }
        let png = self.encode_png();
        std::fs::write(path, png).map_err(|e| format!("failed to write {}: {e}", path.display()))
    }
}

#[derive(Clone, Copy, Default)]
struct CamVertex {
    /// Camera-space position: x right, y up, z forward (positive = in front).
    pos: [f64; 3],
    color: [f32; 3],
    layer: u32,
    fluid_kind: u32,
    diagonal: bool,
}

#[derive(Clone, Copy)]
struct ScreenVertex {
    x: f64,
    y: f64,
    z: f64, // view-space depth, smaller is nearer
    color: [f32; 3],
}

struct Rasterizer {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
    depth: Vec<f32>,
    focal: f64,
    stats: RenderStats,
}

impl Rasterizer {
    fn new(width: u32, height: u32, fov_deg: f64) -> Self {
        let (w, h) = (width as usize, height as usize);
        let mut rgb = vec![0u8; w * h * 3];
        // Sky gradient: deep blue at the top horizon to pale at the bottom.
        for y in 0..h {
            let t = y as f64 / h.saturating_sub(1).max(1) as f64;
            let top = [86.0f64, 132.0, 214.0];
            let bottom = [168.0f64, 205.0, 240.0];
            let c = [
                top[0] + (bottom[0] - top[0]) * t,
                top[1] + (bottom[1] - top[1]) * t,
                top[2] + (bottom[2] - top[2]) * t,
            ];
            for x in 0..w {
                let i = (y * w + x) * 3;
                rgb[i] = c[0] as u8;
                rgb[i + 1] = c[1] as u8;
                rgb[i + 2] = c[2] as u8;
            }
        }
        Self {
            width: w,
            height: h,
            rgb,
            depth: vec![f32::INFINITY; w * h],
            focal: (f64::from(height) * 0.5) / (fov_deg.to_radians() * 0.5).tan(),
            stats: RenderStats::default(),
        }
    }

    fn project(&self, v: &CamVertex) -> ScreenVertex {
        let z = v.pos[2];
        let scale = self.focal / z;
        ScreenVertex {
            x: self.width as f64 * 0.5 + v.pos[0] * scale,
            y: self.height as f64 * 0.5 - v.pos[1] * scale,
            z,
            color: v.color,
        }
    }

    /// Rasterize one camera-space triangle. `translucent` selects alpha
    /// blending without a depth write.
    fn triangle(&mut self, verts: &[CamVertex; 3], translucent: bool, alpha: f32) {
        let alpha = f64::from(alpha);
        let p = [
            self.project(&verts[0]),
            self.project(&verts[1]),
            self.project(&verts[2]),
        ];
        // 2x signed screen area; degenerate triangles are skipped.
        let area = (p[1].y - p[2].y) * (p[0].x - p[2].x) + (p[2].x - p[1].x) * (p[0].y - p[2].y);
        if area.abs() < 1e-9 {
            return;
        }
        let min_x = p.iter().map(|v| v.x).fold(f64::INFINITY, f64::min).floor() as isize;
        let max_x = p
            .iter()
            .map(|v| v.x)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil() as isize;
        let min_y = p.iter().map(|v| v.y).fold(f64::INFINITY, f64::min).floor() as isize;
        let max_y = p
            .iter()
            .map(|v| v.y)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil() as isize;
        let x0 = min_x.max(0) as usize;
        let x1 = (max_x + 1).min(self.width as isize).max(0) as usize;
        let y0 = min_y.max(0) as usize;
        let y1 = (max_y + 1).min(self.height as isize).max(0) as usize;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.stats.triangles_drawn += 1;

        let inv = [1.0 / p[0].z, 1.0 / p[1].z, 1.0 / p[2].z];
        for py in y0..y1 {
            let fy = py as f64 + 0.5;
            for px in x0..x1 {
                let fx = px as f64 + 0.5;
                let w0 =
                    ((p[1].y - p[2].y) * (fx - p[2].x) + (p[2].x - p[1].x) * (fy - p[2].y)) / area;
                let w1 =
                    ((p[2].y - p[0].y) * (fx - p[2].x) + (p[0].x - p[2].x) * (fy - p[2].y)) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let iz = w0 * inv[0] + w1 * inv[1] + w2 * inv[2];
                if iz <= 0.0 {
                    continue;
                }
                let depth = 1.0 / iz;
                let idx = py * self.width + px;
                if depth >= self.depth[idx] as f64 {
                    continue;
                }
                let wsum = iz;
                let shade = |c: usize| {
                    (w0 * p[0].color[c] as f64 * inv[0]
                        + w1 * p[1].color[c] as f64 * inv[1]
                        + w2 * p[2].color[c] as f64 * inv[2])
                        / wsum
                };
                let dst = idx * 3;
                let s = [shade(0), shade(1), shade(2)];
                if translucent {
                    for (channel, &sv) in s.iter().enumerate() {
                        let src = sv.clamp(0.0, 1.0) * 255.0;
                        let d = f64::from(self.rgb[dst + channel]);
                        self.rgb[dst + channel] =
                            (src * alpha + d * (1.0 - alpha)).clamp(0.0, 255.0) as u8;
                    }
                } else {
                    for (channel, &sv) in s.iter().enumerate() {
                        self.rgb[dst + channel] = (sv.clamp(0.0, 1.0) * 255.0).round() as u8;
                    }
                    self.depth[idx] = depth as f32;
                }
                self.stats.pixels_shaded += 1;
            }
        }
    }

    /// Clip a camera-space triangle against the near plane `z >= NEAR_Z` and
    /// rasterize the resulting polygon as a triangle fan.
    fn clipped_triangle(&mut self, tri: &[CamVertex; 3], translucent: bool, alpha: f32) {
        let mut poly: Vec<CamVertex> = Vec::with_capacity(4);
        for i in 0..3 {
            let a = tri[i];
            let b = tri[(i + 1) % 3];
            let da = a.pos[2] - NEAR_Z;
            let db = b.pos[2] - NEAR_Z;
            if da >= 0.0 {
                poly.push(a);
            }
            if (da >= 0.0) != (db >= 0.0) {
                let t = da / (da - db);
                let mut hit = a;
                for axis in 0..3 {
                    hit.pos[axis] += (b.pos[axis] - a.pos[axis]) * t;
                }
                for c in 0..3 {
                    hit.color[c] += (b.color[c] - a.color[c]) * t as f32;
                }
                poly.push(hit);
            }
        }
        if poly.len() < 3 {
            return;
        }
        // Drop vertices still beyond the far plane when the whole polygon is.
        if poly.iter().all(|v| v.pos[2] > FAR_Z) {
            return;
        }
        for fan in 1..poly.len() - 1 {
            self.triangle(&[poly[0], poly[fan], poly[fan + 1]], translucent, alpha);
        }
    }
}

// ---------------------------------------------------------------------------
// Vertex decode + shading
// ---------------------------------------------------------------------------

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [f32; 3] {
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f64;
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
    [r as f32, g as f32, b as f32]
}

/// Deterministic block-ID color palette: golden-ratio hue per state id.
fn palette_color(state_id: u32) -> [f32; 3] {
    let hue = (state_id as f64 * 0.618_033_988_749_895).fract();
    hsv_to_rgb(hue, 0.5, 0.85)
}

fn tint_color(tint: u32) -> Option<[f32; 3]> {
    if tint == 0 {
        return None;
    }
    Some([
        ((tint >> 16) & 0xff) as f32 / 255.0,
        ((tint >> 8) & 0xff) as f32 / 255.0,
        (tint & 0xff) as f32 / 255.0,
    ])
}

/// Per-face brightness in vanilla order: down, up, north, south, west, east.
const FACE_SHADE: [f32; 6] = [0.45, 1.0, 0.75, 0.85, 0.7, 0.95];
const AO_SHADE: [f32; 4] = [0.4, 0.6, 0.8, 1.0];

fn decode_vertex(pv: &PackedVertex, origin: [f64; 3], smooth_lighting: bool) -> CamVertex {
    let attr = pv.attributes;
    let lx = f64::from(pv.position & 0x1f);
    let lz = f64::from((pv.position >> 5) & 0x1f);
    let ly = f64::from(pv.position >> 10) / 1024.0;
    let direction = (attr & ATTR_DIRECTION_MASK) as usize;
    let ao = ((attr >> ATTR_AO_SHIFT) & 0x3) as usize;
    let block_light = ((attr >> ATTR_BLOCK_LIGHT_SHIFT) & 0xf) as f32;
    let sky_light = ((attr >> ATTR_SKY_LIGHT_SHIFT) & 0xf) as f32;
    let layer = (attr >> ATTR_LAYER_SHIFT) & 0x3;
    let fluid_kind = (attr >> ATTR_FLUID_KIND_SHIFT) & 0x3;

    let base = tint_color(pv.tint).unwrap_or_else(|| palette_color(pv.state_id));
    let light = (block_light.max(sky_light) / 15.0).clamp(0.0, 1.0);
    let illum = 0.25 + 0.75 * light;
    // Smooth lighting off = flat per-face shading without per-vertex AO.
    let ao_shade = if smooth_lighting { AO_SHADE[ao] } else { 1.0 };
    let shade = FACE_SHADE[direction.min(5)] * ao_shade * illum;
    CamVertex {
        pos: [origin[0] + lx, origin[1] + ly, origin[2] + lz],
        color: [base[0] * shade, base[1] * shade, base[2] * shade],
        layer,
        fluid_kind,
        diagonal: attr & ATTR_DIAGONAL_BIT != 0,
    }
}

fn is_translucent_quad(v: &CamVertex) -> bool {
    v.layer == LAYER_TRANSLUCENT || (v.layer == LAYER_FLUID && v.fluid_kind == FLUID_WATER)
}

fn translucent_alpha(v: &CamVertex) -> f32 {
    match (v.layer, v.fluid_kind) {
        (LAYER_FLUID, FLUID_LAVA) => 0.95,
        (LAYER_FLUID, _) => 0.75,
        _ => 0.55,
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Camera basis for `yaw`/`pitch` (degrees, server convention): forward into
/// the screen, right = forward x up, up = right x forward.
fn camera_basis(yaw: f32, pitch: f32) -> ([f64; 3], [f64; 3], [f64; 3]) {
    let yaw = f64::from(yaw).to_radians();
    let pitch = f64::from(pitch).to_radians();
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let fwd = [-sy * cp, -sp, cy * cp];
    let right = [-fwd[2], 0.0, fwd[0]];
    let right_len = (right[0] * right[0] + right[2] * right[2]).sqrt();
    if right_len > 1e-9 {
        let r = [right[0] / right_len, 0.0, right[2] / right_len];
        let u = [
            r[1] * fwd[2] - r[2] * fwd[1],
            r[2] * fwd[0] - r[0] * fwd[2],
            r[0] * fwd[1] - r[1] * fwd[0],
        ];
        (fwd, r, u)
    } else {
        // Looking straight up/down: fall back to a stable right vector.
        (fwd, [1.0, 0.0, 0.0], [0.0, 0.0, -1.0])
    }
}

/// The six world-space frustum planes of a camera pose at the default FOV,
/// in `mc_spine_client_mesh::box_in_frustum` form (`dot(n, p) + d >= 0`
/// inside). `aspect` is render width / height. Used by the `GraphCuller`
/// walk and to prioritize `MeshCache::rebuild_dirty_ordered`.
pub fn frustum_planes(camera: [f32; 3], yaw: f32, pitch: f32, aspect: f64) -> [[f32; 4]; 6] {
    frustum_planes_fov(camera, yaw, pitch, aspect, DEFAULT_FOV_DEG)
}

/// [`frustum_planes`] with an explicit vertical field of view in degrees.
pub fn frustum_planes_fov(
    camera: [f32; 3],
    yaw: f32,
    pitch: f32,
    aspect: f64,
    fov_deg: f64,
) -> [[f32; 4]; 6] {
    let eye = [
        f64::from(camera[0]),
        f64::from(camera[1]),
        f64::from(camera[2]),
    ];
    let (fwd, right, up) = camera_basis(yaw, pitch);
    let tan_v = (fov_deg.to_radians() * 0.5).tan();
    let tan_h = tan_v * aspect;
    // Side planes pass through the eye: a camera-space point (x, y, z) is
    // inside when |x| <= z*tan_h and |y| <= z*tan_v.
    let through_eye = |n: [f64; 3]| -> [f32; 4] {
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        let (nx, ny, nz) = (n[0] / len, n[1] / len, n[2] / len);
        [
            nx as f32,
            ny as f32,
            nz as f32,
            (-(nx * eye[0] + ny * eye[1] + nz * eye[2])) as f32,
        ]
    };
    let d_fwd = fwd[0] * eye[0] + fwd[1] * eye[1] + fwd[2] * eye[2];
    [
        through_eye([
            right[0] + fwd[0] * tan_h,
            right[1] + fwd[1] * tan_h,
            right[2] + fwd[2] * tan_h,
        ]),
        through_eye([
            fwd[0] * tan_h - right[0],
            fwd[1] * tan_h - right[1],
            fwd[2] * tan_h - right[2],
        ]),
        through_eye([
            up[0] + fwd[0] * tan_v,
            up[1] + fwd[1] * tan_v,
            up[2] + fwd[2] * tan_v,
        ]),
        through_eye([
            fwd[0] * tan_v - up[0],
            fwd[1] * tan_v - up[1],
            fwd[2] * tan_v - up[2],
        ]),
        [
            fwd[0] as f32,
            fwd[1] as f32,
            fwd[2] as f32,
            (-d_fwd - NEAR_Z) as f32,
        ],
        [
            -fwd[0] as f32,
            -fwd[1] as f32,
            -fwd[2] as f32,
            (d_fwd + FAR_Z) as f32,
        ],
    ]
}

/// Render one camera frame of the live world to an RGB buffer.
/// `eye` is the camera position (player pos + eye height); `yaw`/`pitch` are
/// degrees using the server's convention (`forward = (-sin yaw·cos pitch,
/// -sin pitch, cos yaw·cos pitch)`). Equivalent to
/// [`render_frame_with_config`] with [`RenderConfig::default`].
pub fn render_frame(
    world: &WorldState,
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    width: u32,
    height: u32,
) -> Result<RenderedFrame, String> {
    render_frame_with_config(
        world,
        eye,
        yaw,
        pitch,
        width,
        height,
        &RenderConfig::default(),
    )
}

/// [`render_frame`] honoring a resolved [`RenderConfig`]: `fov_deg` drives
/// the projection, `smooth_lighting` the per-vertex AO term, and
/// `view_distance` caps drawn sections to a Chebyshev chunk-column radius
/// around the camera column.
pub fn render_frame_with_config(
    world: &WorldState,
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    width: u32,
    height: u32,
    config: &RenderConfig,
) -> Result<RenderedFrame, String> {
    if width == 0 || height == 0 {
        return Err("render dimensions must be non-zero".to_string());
    }
    let started = Instant::now();
    let mut raster = Rasterizer::new(width, height, config.fov_deg);

    let (fwd, right, up) = camera_basis(yaw, pitch);
    let eye32 = [eye[0] as f32, eye[1] as f32, eye[2] as f32];
    let planes = frustum_planes_fov(
        eye32,
        yaw,
        pitch,
        f64::from(width) / f64::from(height),
        config.fov_deg,
    );

    // Transform world vertices into camera space.
    let to_camera = |p: [f64; 3]| -> [f64; 3] {
        let d = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
        [
            d[0] * right[0] + d[1] * right[1] + d[2] * right[2],
            d[0] * up[0] + d[1] * up[1] + d[2] * up[2],
            d[0] * fwd[0] + d[1] * fwd[1] + d[2] * fwd[2],
        ]
    };

    // Frustum x BFS graph cull over the section visibility graph. Only
    // sections the walk reaches are rasterized; everything else is counted
    // as culled.
    let camera_section = SectionCoord(
        (eye[0] / 16.0).floor() as i32,
        (eye[1] / 16.0).floor() as i32,
        (eye[2] / 16.0).floor() as i32,
    );
    let cull = GraphCuller::new(camera_section, &planes).search(world.section_graphs());
    raster.stats.culled_sections = cull.culled as u64;
    // Draw-distance cap: sections in chunk columns farther than
    // `view_distance` from the camera column are culled like any other
    // reject (they belong to the far-terrain LOD path instead).
    let within_view_distance = |coord: &SectionCoord| -> bool {
        match config.view_distance {
            None => true,
            Some(distance) => {
                let dc = i64::from(coord.0 - camera_section.0)
                    .abs()
                    .max(i64::from(coord.2 - camera_section.2).abs());
                dc <= i64::from(distance)
            }
        }
    };

    let meshes: HashMap<SectionCoord, &[PackedVertex]> = world.packed_meshes().collect();
    // Translucent draw list: (squared camera distance, coord, bucket draw mask).
    let mut translucent_sections: Vec<(f64, SectionCoord, u8)> = Vec::new();
    // Multi-draw accounting: per-region set of drawn-section slab identities
    // (vertex_offset, index_offset) so MultiDraw submissions are only counted
    // when they contain at least one drawn section's draw.
    let mut drawn_slabs: HashMap<[i32; 3], HashSet<(u32, u32)>> = HashMap::new();

    for &coord in &cull.visible {
        if !within_view_distance(&coord) {
            raster.stats.culled_sections += 1;
            continue;
        }
        let Some(&packed) = meshes.get(&coord) else {
            continue;
        };
        if packed.len() < 4 || packed.len() % 4 != 0 {
            continue;
        }
        let Some(buckets) = world.direction_buckets(coord) else {
            continue;
        };
        raster.stats.drawn_sections += 1;
        if let Some(slab) = world
            .render_regions()
            .region(coord)
            .and_then(|region| region.slab(coord))
        {
            drawn_slabs
                .entry(RenderRegions::origin_of(coord))
                .or_default()
                .insert((slab.vertex_offset, slab.index_offset));
        }

        let origin = [
            f64::from(coord.0) * 16.0,
            f64::from(coord.1) * 16.0,
            f64::from(coord.2) * 16.0,
        ];
        // Directional face bucketing: keep only the direction buckets whose
        // section side faces the camera; the unassigned bucket always draws.
        let draw_mask = bucket_draw_mask(box_facing_mask(
            [origin[0] as f32, origin[1] as f32, origin[2] as f32],
            eye32,
        ));
        let mut section_has_translucent = false;
        for bucket in 0..DIRECTION_BUCKETS {
            let (offset, count) = (
                buckets.offset[bucket] as usize,
                buckets.count[bucket] as usize,
            );
            if count == 0 {
                continue;
            }
            if bucket != DIRECTION_BUCKET_UNASSIGNED && draw_mask & (1 << bucket) == 0 {
                raster.stats.buckets_culled += 1;
                continue;
            }
            raster.stats.buckets_drawn += 1;
            for qi in offset..offset + count {
                let quad = &packed[qi * 4..qi * 4 + 4];
                let mut verts = [CamVertex::default(); 4];
                let mut any_translucent = false;
                for (i, pv) in quad.iter().enumerate() {
                    verts[i] = decode_vertex(pv, origin, config.smooth_lighting);
                    any_translucent |= is_translucent_quad(&verts[i]);
                }
                // All four vertices of a quad share layer/fluid/diagonal.
                if any_translucent {
                    section_has_translucent = true;
                    continue;
                }
                let diagonal = verts[0].diagonal;
                for vtx in verts.iter_mut() {
                    vtx.pos = to_camera(vtx.pos);
                }
                let tris: [[usize; 3]; 2] = if diagonal {
                    [[0, 1, 3], [1, 2, 3]]
                } else {
                    [[0, 1, 2], [0, 2, 3]]
                };
                for tri in tris {
                    raster.clipped_triangle(
                        &[verts[tri[0]], verts[tri[1]], verts[tri[2]]],
                        false,
                        1.0,
                    );
                }
            }
        }
        if section_has_translucent {
            let center = [
                origin[0] + 8.0 - eye[0],
                origin[1] + 8.0 - eye[1],
                origin[2] + 8.0 - eye[2],
            ];
            let dist2 = center[0] * center[0] + center[1] * center[1] + center[2] * center[2];
            translucent_sections.push((dist2, coord, draw_mask));
        }
    }

    // Translucent pass: far sections first, `sort_translucent` order within.
    translucent_sections.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (_, coord, draw_mask) in translucent_sections {
        let Some(&packed) = meshes.get(&coord) else {
            continue;
        };
        let origin = [
            f64::from(coord.0) * 16.0,
            f64::from(coord.1) * 16.0,
            f64::from(coord.2) * 16.0,
        ];
        let quad_count = packed.len() / 4;
        let order: Vec<usize> = match world.translucent_order(coord) {
            Some(indices) => indices.iter().map(|&i| i as usize).collect(),
            None => (0..quad_count).collect(),
        };
        for qi in order {
            if qi >= quad_count {
                continue;
            }
            let quad = &packed[qi * 4..qi * 4 + 4];
            // Direction-bucket backface culling applies to translucent quads
            // too: the packed direction attribute is the quad's bucket.
            let direction = quad[0].attributes & ATTR_DIRECTION_MASK;
            if draw_mask & (1 << direction) == 0 {
                continue;
            }
            let mut verts = [CamVertex::default(); 4];
            for (i, pv) in quad.iter().enumerate() {
                verts[i] = decode_vertex(pv, origin, config.smooth_lighting);
            }
            if !is_translucent_quad(&verts[0]) {
                continue;
            }
            let alpha = translucent_alpha(&verts[0]);
            for vtx in verts.iter_mut() {
                vtx.pos = to_camera(vtx.pos);
            }
            let tris: [[usize; 3]; 2] = if verts[0].diagonal {
                [[0, 1, 3], [1, 2, 3]]
            } else {
                [[0, 1, 2], [0, 2, 3]]
            };
            for tri in tris {
                raster.clipped_triangle(
                    &[verts[tri[0]], verts[tri[1]], verts[tri[2]]],
                    true,
                    alpha,
                );
            }
        }
    }

    // One multi-draw submission per (region, facing bucket); a submission
    // counts only when at least one of its indirect draws belongs to a
    // section drawn this frame — culled sections never reach the GPU.
    raster.stats.region_draw_calls = world
        .render_regions()
        .build_multidraw(eye32)
        .iter()
        .filter(|cmd| {
            drawn_slabs.get(&cmd.region).is_some_and(|slabs| {
                cmd.draws
                    .iter()
                    .any(|d| slabs.contains(&(d.base_vertex as u32, d.first_index)))
            })
        })
        .count() as u64;

    raster.stats.render_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(RenderedFrame {
        width,
        height,
        rgb: raster.rgb,
        stats: raster.stats,
    })
}

// ---------------------------------------------------------------------------
// PNG encoding (RFC 2083, 8-bit RGB, zlib stored blocks via RFC 1950)
// ---------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(tag);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    out
}

/// Encode tightly packed RGB8 pixels as a PNG (color type 2, no interlace).
pub fn encode_png_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let stride = width as usize * 3;
    debug_assert_eq!(rgb.len(), stride * height as usize);
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgb.chunks_exact(stride) {
        raw.push(0); // filter type 0: None
        raw.extend_from_slice(row);
    }
    let idat = zlib_compress_stored(&raw);

    let mut out = Vec::with_capacity(8 + 25 + 12 + idat.len() + 12);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolor RGB
    out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&png_chunk(b"IDAT", &idat));
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    out
}

// ---------------------------------------------------------------------------
// Tests: graph occlusion culling, bucket culling, multi-draw region batching
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mc_spine_client_mesh::SECTION_CELLS;

    use crate::protocol::{DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta};
    use crate::settings::QualityPreset;
    use crate::world::{BlockTable, WorldState};

    fn test_dimension() -> DimensionMeta {
        DimensionMeta {
            name: "minecraft:overworld".to_string(),
            min_y: 0,
            height: 64,
            has_skylight: true,
        }
    }

    fn uniform_section(state: u32) -> DecodedSection {
        DecodedSection {
            non_air: if state == 0 { 0 } else { SECTION_CELLS as i16 },
            states: Box::new([state; SECTION_CELLS]),
            biomes: [0; 64],
        }
    }

    fn chunk(cx: i32, cz: i32, sections: Vec<DecodedSection>) -> DecodedChunk {
        DecodedChunk {
            cx,
            cz,
            sections,
            light: DecodedLightData {
                sky_mask: Vec::new(),
                block_mask: Vec::new(),
                empty_sky_mask: Vec::new(),
                empty_block_mask: Vec::new(),
                sky_arrays: Vec::new(),
                block_arrays: Vec::new(),
            },
        }
    }

    /// 4x4-chunk terrain: a solid floor layer (y sections 0-1) carved by a
    /// 2x2-section cave pocket at y=1, under open air (y=2-3). Camera sits in
    /// the cave; the sealed underground sections must be occlusion-culled.
    fn cave_world() -> WorldState {
        let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
        for cx in 0..4 {
            for cz in 0..4 {
                let in_cave = (1..=2).contains(&cx) && (1..=2).contains(&cz);
                let sections = vec![
                    uniform_section(1),
                    uniform_section(if in_cave { 0 } else { 1 }),
                    uniform_section(0),
                    uniform_section(0),
                ];
                world.apply_chunk(chunk(cx, cz, sections)).unwrap();
            }
        }
        world
    }

    #[test]
    fn graph_culler_culls_occluded_underground_sections() {
        let mut world = cave_world();
        let eye = [32.0, 24.0, 32.0];
        let cam32 = [32.0, 24.0, 32.0];
        let planes = frustum_planes(cam32, 0.0, 55.0, 16.0 / 9.0);
        world
            .rebuild_dirty_bounded_ordered(usize::MAX, cam32, &planes)
            .unwrap();

        let frame = render_frame(&world, eye, 0.0, 55.0, 160, 90).unwrap();
        assert!(frame.stats.drawn_sections > 0, "no sections drawn");
        // 12 of the 16 sealed floor sections are fully buried (4 form the
        // cave floor and are drawn); the frustum may cull even more.
        assert!(
            frame.stats.culled_sections >= 12,
            "expected >= 12 occluded underground sections, got {}",
            frame.stats.culled_sections
        );
        assert!(frame.stats.triangles_drawn > 0);
        // All drawn sections share one 8x4x8 region; MultiDraw submissions
        // never exceed the per-section call count they replace.
        assert!(frame.stats.region_draw_calls > 0);
        assert!(frame.stats.region_draw_calls <= frame.stats.drawn_sections);
    }

    #[test]
    fn backfacing_direction_buckets_are_skipped() {
        // One floating stone section surrounded by air emits all six axis
        // buckets; the camera below-north sees only Down + North.
        let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
        let mut sections = vec![uniform_section(0); 4];
        sections[2] = uniform_section(1);
        world.apply_chunk(chunk(0, 0, sections)).unwrap();
        let eye = [8.0, 24.0, -16.0];
        let cam32 = [8.0, 24.0, -16.0];
        let planes = frustum_planes(cam32, 0.0, 0.0, 16.0 / 9.0);
        world
            .rebuild_dirty_bounded_ordered(usize::MAX, cam32, &planes)
            .unwrap();

        let frame = render_frame(&world, eye, 0.0, 0.0, 160, 90).unwrap();
        assert_eq!(frame.stats.drawn_sections, 1);
        // Facing = Down|North -> 2 buckets drawn, 4 backfacing culled.
        assert_eq!(frame.stats.buckets_drawn, 2);
        assert_eq!(frame.stats.buckets_culled, 4);
        assert!(frame.stats.region_draw_calls >= 1);
    }

    #[test]
    fn render_regions_batch_sections_into_fewer_draw_calls() {
        // 2x2 chunks, floor slab at y=0 under air: four drawn sections sharing
        // the same facing bucket collapse into one MultiDraw submission.
        let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
        for cx in 0..2 {
            for cz in 0..2 {
                let sections = vec![
                    uniform_section(1),
                    uniform_section(0),
                    uniform_section(0),
                    uniform_section(0),
                ];
                world.apply_chunk(chunk(cx, cz, sections)).unwrap();
            }
        }
        let eye = [16.0, 70.0, 16.0];
        let cam32 = [16.0, 70.0, 16.0];
        let planes = frustum_planes(cam32, 0.0, 60.0, 16.0 / 9.0);
        world
            .rebuild_dirty_bounded_ordered(usize::MAX, cam32, &planes)
            .unwrap();

        let frame = render_frame(&world, eye, 0.0, 60.0, 160, 90).unwrap();
        assert!(
            frame.stats.drawn_sections >= 2,
            "expected several drawn floor sections, got {}",
            frame.stats.drawn_sections
        );
        assert!(
            frame.stats.region_draw_calls < frame.stats.drawn_sections,
            "expected batching: {} calls vs {} sections",
            frame.stats.region_draw_calls,
            frame.stats.drawn_sections
        );
        assert!(frame.stats.buckets_drawn > 0);
        // Each floor section also emits back-facing side/bottom buckets that
        // must be skipped while the camera is above the slab.
        assert!(frame.stats.buckets_culled > 0);
    }

    /// `RenderConfig::from_settings` mirrors the resolved `ClientSettings`,
    /// and `RenderConfig::default()` keeps the legacy frame behavior.
    #[test]
    fn render_config_from_settings_maps_all_fields() {
        let mut settings = ClientSettings::from_preset(QualityPreset::Fast);
        settings.fov = 100;
        settings.clouds = CloudMode::Off;
        let config = RenderConfig::from_settings(&settings);
        assert_eq!(config.fov_deg, 100.0);
        assert!(!config.smooth_lighting);
        assert_eq!(config.view_distance, Some(8));
        assert_eq!(config.clouds, CloudMode::Off);
        assert_eq!(config.particles, ParticleLevel::Decreased);
        assert!(!config.entity_shadows);
        // Fast leaves vsync/max_fps at the shared defaults.
        assert!(config.vsync);
        assert_eq!(config.max_fps, None);
        // Default (Balanced) settings differ from the legacy renderer
        // default only by gaining the explicit distance cap.
        assert_eq!(
            RenderConfig::from_settings(&ClientSettings::default()),
            RenderConfig {
                view_distance: Some(12),
                ..RenderConfig::default()
            }
        );
    }

    /// `decode_vertex` only darkens by per-vertex AO when smooth lighting is
    /// enabled; with it off the face shade is uniform.
    #[test]
    fn smooth_lighting_toggles_vertex_ao() {
        let mut pv = PackedVertex {
            position: 0,
            state_id: 1,
            attributes: 0,
            tint: 0,
        };
        // direction = Up (index 1), ao = 0 (darkest), full sky light.
        pv.attributes = 1 | (0 << ATTR_AO_SHIFT) | (15 << ATTR_SKY_LIGHT_SHIFT);
        let smooth = decode_vertex(&pv, [0.0; 3], true);
        let flat = decode_vertex(&pv, [0.0; 3], false);
        assert!(flat.color[0] > smooth.color[0]);
        // Full sky light + ao 0: smooth shade = base * AO_SHADE[0], flat = base.
        let base = palette_color(1);
        assert_eq!(smooth.color[0], base[0] * AO_SHADE[0]);
        assert_eq!(flat.color[0], base[0]);
    }

    /// The view-distance cap removes in-frustum chunk columns beyond the
    /// resolved distance without affecting nearer sections.
    #[test]
    fn view_distance_caps_sections_beyond_distance() {
        let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
        // A contiguous 9-column strip so the graph culler reaches the far
        // end; solid floor at y sections 0-1 under air.
        for cz in 0..=8 {
            let sections = vec![
                uniform_section(1),
                uniform_section(1),
                uniform_section(0),
                uniform_section(0),
            ];
            world.apply_chunk(chunk(0, cz, sections)).unwrap();
        }
        // Camera just above the floor looking along +z: every column's
        // exposed top faces are inside the frustum, including the ones the
        // view-distance cap must reject.
        let cam32 = [8.0, 40.0, 8.0];
        let planes = frustum_planes(cam32, 0.0, 0.0, 16.0 / 9.0);
        world
            .rebuild_dirty_bounded_ordered(usize::MAX, cam32, &planes)
            .unwrap();

        let eye = [8.0, 40.0, 8.0];
        let full = render_frame(&world, eye, 0.0, 0.0, 160, 90).unwrap();
        let mut settings = ClientSettings::default();
        settings.view_distance = 6;
        let config = RenderConfig::from_settings(&settings);
        let limited =
            render_frame_with_config(&world, eye, 0.0, 0.0, 160, 90, &config).unwrap();

        assert!(
            limited.stats.drawn_sections < full.stats.drawn_sections,
            "cap drew {} of {} sections",
            limited.stats.drawn_sections,
            full.stats.drawn_sections
        );
        assert!(limited.stats.culled_sections > full.stats.culled_sections);
    }
}
