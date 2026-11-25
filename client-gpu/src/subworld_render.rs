// SPDX-License-Identifier: LGPL-3.0-or-later
//! 6-DOF rigid-body sub-world rendering for Create contraptions and
//! Aeronautics airships/spacecraft, plus GPU-instanced mechanical part
//! batching (cogwheels, shafts, chests).
//!
//! CPU side: [`SubWorldTransform`] (quaternion -> column-major `mat4x4`),
//! [`SubWorldRenderEntry`]/[`SubWorldRenderBank`] (the authoritative
//! sub-world scene table), and [`InstancedPartRecord`]/[`InstancedPartBatch`]
//! (the `client-port::ber_instancing`-style shared-template instance stream).
//!
//! GPU side: [`SubWorldGpuCache`] keeps one vertex/index buffer pair per
//! live sub-world plus one instance buffer per `mesh_kind`, re-uploading only
//! entries whose `dirty` flag is set, so a steady frame allocates nothing.
//!
//! The WGSL modules [`COMPOSITE_TERRAIN_WGSL`] and [`COMPOSITE_LIT_WGSL`]
//! drive the composed frame: packed-vertex terrain with smooth chunk fade
//! and VFX1 colored point lights, and `TerrainVertex` sub-world/cloud/planet
//! geometry transformed by a per-draw `view_proj * model` uniform.

use std::borrow::Cow;
use std::collections::BTreeMap;

use bytemuck::{Pod, Zeroable};
use megu_client_mesh::ber_instancing::VanillaParticleQuad;
use megu_client_mesh::builtin_shadow_wgsl::VfxLightUniforms;
use megu_client_mesh::BerInstanceData;

use crate::subworld_sync::{SubWorldPoseInterpolator, SubWorldSyncFrame};

use crate::{mat4_mul, Mat4};

// ---------------------------------------------------------------------------
// Shared lit-geometry vertex (`position`/`normal`/`albedo`/`uv`)
// ---------------------------------------------------------------------------

/// World-space lit vertex for sub-worlds, voxel clouds, and cubic
/// planets/suns. 32 bytes — down from the original 48 — cutting vertex
/// bandwidth and VRAM per lit vertex by a third; bound as
/// `@location(0..2, 7..8)` inputs in [`COMPOSITE_LIT_WGSL`].
///
/// `position`/`uv` keep full `f32` precision (any quantization error
/// there is a silhouette seam or a texture swim, both pixel-visible);
/// `normal`/`albedo` pack into one `u32` each because lighting tolerates
/// byte-precision direction and color error far below display
/// quantization.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct TerrainVertex {
    /// Position in the draw's model space (blocks).
    pub position: [f32; 3],
    /// Unit outward normal in model space, octahedral-encoded into two
    /// `unorm16` lanes (`x` in bits 0..16, `y` in bits 16..32).
    /// [`pack_normal_octahedral`]/[`unpack_normal_octahedral`] round-trip
    /// it; the vertex shader's `unpack_oct_normal` decodes the same
    /// words.
    pub packed_normal: u32,
    /// Linear-ish albedo `0.0..=1.0` per channel as `unorm8x3` rgb
    /// (bytes 0..3), plus a material-flags byte (bits 24..32) reserved
    /// for cutout/emissive tagging — `0` today.
    pub packed_albedo: u32,
    /// Original unwrapped sprite UV, including fractional texel geometry.
    pub uv: [f32; 2],
    /// Index into the uploaded atlas rectangle table. `u32::MAX` is untextured.
    pub atlas_index: u32,
}

const _: () = assert!(
    std::mem::size_of::<TerrainVertex>() == 32,
    "TerrainVertex must stay 32 bytes"
);

impl TerrainVertex {
    /// Full-precision constructor: quantizes `normal`/`albedo` into their
    /// packed GPU words. Non-finite inputs fail closed — a non-finite or
    /// zero normal packs as `+Y`, non-finite albedo channels as `0`.
    pub fn new(
        position: [f32; 3],
        normal: [f32; 3],
        albedo: [f32; 3],
        uv: [f32; 2],
        atlas_index: u32,
    ) -> Self {
        Self {
            position,
            packed_normal: pack_normal_octahedral(normal),
            packed_albedo: pack_albedo_unorm8x4(albedo, 0),
            uv,
            atlas_index,
        }
    }

    /// Unpacked unit model-space normal (`+Y` when `packed_normal` is a
    /// degenerate word).
    pub fn normal(&self) -> [f32; 3] {
        unpack_normal_octahedral(self.packed_normal)
    }

    /// Re-pack `normal` into `packed_normal` (the read-modify-write path
    /// the CPU-side vertex reorientation helpers take).
    pub fn set_normal(&mut self, normal: [f32; 3]) {
        self.packed_normal = pack_normal_octahedral(normal);
    }

    /// Unpacked `0.0..=1.0` albedo rgb.
    pub fn albedo(&self) -> [f32; 3] {
        unpack_albedo_unorm8x4(self.packed_albedo)
    }

    /// Re-pack `albedo` into `packed_albedo`, preserving the reserved
    /// material-flags byte.
    pub fn set_albedo(&mut self, albedo: [f32; 3]) {
        self.packed_albedo = pack_albedo_unorm8x4(albedo, self.material_flags());
    }

    /// Reserved material-flags byte (`packed_albedo` bits 24..32).
    pub fn material_flags(&self) -> u8 {
        (self.packed_albedo >> 24) as u8
    }

    /// Write the material-flags byte, preserving packed rgb.
    pub fn set_material_flags(&mut self, flags: u8) {
        self.packed_albedo =
            (self.packed_albedo & 0x00ff_ffff) | (u32::from(flags) << 24);
    }
}

/// Octahedral-encode a (not necessarily unit) direction into two
/// `unorm16` lanes packed in one `u32`. The lower hemisphere folds onto
/// the upper octahedron face; degenerate inputs pack as `+Y`. Quantizing
/// at 16 bits per axis keeps the worst-case angular error near 3e-5 rad —
/// two orders of magnitude below `snorm8`.
pub fn pack_normal_octahedral(normal: [f32; 3]) -> u32 {
    let len = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    let (x, y, z) = if len.is_finite() && len > 1.0e-12 {
        (normal[0] / len, normal[1] / len, normal[2] / len)
    } else {
        (0.0, 1.0, 0.0)
    };
    // Project onto the octahedron: divide by the L1 norm, then fold the
    // lower hemisphere over the diagonal edges.
    let inv_l1 = (x.abs() + y.abs() + z.abs()).recip();
    let (mut ex, mut ey) = (x * inv_l1, y * inv_l1);
    if z < 0.0 {
        let sx = if ex >= 0.0 { 1.0 } else { -1.0 };
        let sy = if ey >= 0.0 { 1.0 } else { -1.0 };
        let folded = ((1.0 - ey.abs()) * sx, (1.0 - ex.abs()) * sy);
        ex = folded.0;
        ey = folded.1;
    }
    let quantize = |v: f32| -> u32 {
        ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * 65535.0).round() as u32 & 0xffff
    };
    quantize(ex) | (quantize(ey) << 16)
}

/// Inverse of [`pack_normal_octahedral`]: decode the two `unorm16` lanes,
/// unfold the lower hemisphere, and renormalize. A degenerate decoded
/// vector returns `+Y` so a zeroed vertex still shades sanely.
pub fn unpack_normal_octahedral(packed: u32) -> [f32; 3] {
    let ex = (packed & 0xffff) as f32 * (2.0 / 65535.0) - 1.0;
    let ey = (packed >> 16) as f32 * (2.0 / 65535.0) - 1.0;
    let mut n = [ex, ey, 1.0 - ex.abs() - ey.abs()];
    let t = (-n[2]).max(0.0);
    n[0] += if n[0] >= 0.0 { -t } else { t };
    n[1] += if n[1] >= 0.0 { -t } else { t };
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len.is_finite() && len > 1.0e-12 {
        [n[0] / len, n[1] / len, n[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

/// Material-flags byte tag for the cubic x realistic hybrid cloud path:
/// cloud deck vertices carry this bit so the lit fragment shader can
/// branch into the analytic cloud optics (forward-scatter silver lining,
/// crown-to-belly vertical shading, wispy shimmer, radial horizon
/// dissolve) while every other lit draw keeps the plain pipeline.
/// Vanilla-mode clouds leave the byte `0`.
pub const MAT_FLAG_CLOUD: u8 = 0x01;

/// `unorm8x3` rgb + flags byte (bits 24..32) — the CPU twin of the
/// vertex shader's `unpack_unorm8_albedo`.
fn pack_albedo_unorm8x4(albedo: [f32; 3], flags: u8) -> u32 {
    let quantize = |v: f32| -> u32 {
        if v.is_finite() {
            (v.clamp(0.0, 1.0) * 255.0).round() as u32
        } else {
            0
        }
    };
    quantize(albedo[0])
        | (quantize(albedo[1]) << 8)
        | (quantize(albedo[2]) << 16)
        | (u32::from(flags) << 24)
}

/// Unpack the `unorm8x3` albedo bytes of `packed_albedo`.
fn unpack_albedo_unorm8x4(packed: u32) -> [f32; 3] {
    [
        (packed & 0xff) as f32 / 255.0,
        ((packed >> 8) & 0xff) as f32 / 255.0,
        ((packed >> 16) & 0xff) as f32 / 255.0,
    ]
}

const LIT_VERTEX_ATTRS: [wgpu::VertexAttribute; 5] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 12,
        shader_location: 1,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 16,
        shader_location: 2,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 20,
        shader_location: 7,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 28,
        shader_location: 8,
    },
];

/// `TerrainVertex` buffer layout for the lit/instanced pipelines.
pub const LIT_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<TerrainVertex>() as wgpu::BufferAddress,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &LIT_VERTEX_ATTRS,
};

/// World-space debug overlays share the composed frame's view-projection uniform.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct DebugLineVertex {
    pub position: [f32; 3],
    pub color: [f32; 4],
}

const DEBUG_LINE_ATTRS: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 12,
        shader_location: 1,
    },
];

pub const DEBUG_LINE_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<DebugLineVertex>() as wgpu::BufferAddress,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &DEBUG_LINE_ATTRS,
};

pub const DEBUG_LINE_WGSL: &str = r#"
struct FrameUniforms {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    fog_start: f32,
    fog_color: vec3<f32>,
    fog_end: f32,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> frame: FrameUniforms;

struct LineInput {
    @location(0) position: vec3<f32>,
    @location(1) color: vec4<f32>,
};
struct LineOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex
fn debug_line_vs(input: LineInput) -> LineOutput {
    var output: LineOutput;
    output.clip = frame.view_proj * vec4<f32>(input.position, 1.0);
    output.color = input.color;
    return output;
}
@fragment
fn debug_line_fs(input: LineOutput) -> @location(0) vec4<f32> {
    return input.color;
}
"#;

// ---------------------------------------------------------------------------
// SubWorldTransform: 6-DOF rigid transform (position + quaternion + scale)
// ---------------------------------------------------------------------------

/// Position + unit quaternion + uniform scale for one rigid sub-world body
/// (a moving Create contraption, a 6-DOF Aeronautics airship, a hanging sky
/// body). Non-finite fields fail closed: identity rotation, unit scale,
/// origin position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubWorldTransform {
    /// World-space translation in blocks.
    pub position: [f32; 3],
    /// Rotation quaternion `(x, y, z, w)`; renormalized on read, identity
    /// on zero/NaN input.
    pub quaternion: [f32; 4],
    /// Uniform scale; non-finite or non-positive reads as `1.0`.
    pub scale: f32,
}

impl SubWorldTransform {
    /// The static world grid transform.
    pub const IDENTITY: Self = Self {
        position: [0.0; 3],
        quaternion: [0.0, 0.0, 0.0, 1.0],
        scale: 1.0,
    };

    /// The quaternion normalized for matrix math; `(0,0,0,1)` when the
    /// stored quaternion is zero-length or non-finite.
    pub fn normalized_quaternion(&self) -> [f32; 4] {
        let q = self.quaternion;
        let len2 = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
        if len2.is_finite() && len2 > 1.0e-12 {
            let inv = len2.sqrt().recip();
            [q[0] * inv, q[1] * inv, q[2] * inv, q[3] * inv]
        } else {
            [0.0, 0.0, 0.0, 1.0]
        }
    }

    /// Finite, positive-scale, normalized view of this transform used by
    /// interpolation and point transforms. Invalid fields fail closed to
    /// origin/identity/unit scale rather than propagating NaN.
    fn sanitized(self) -> Self {
        let position = if self.position.iter().all(|v| v.is_finite()) {
            self.position
        } else {
            [0.0; 3]
        };
        let scale = if self.scale.is_finite() && self.scale > 1.0e-6 {
            self.scale
        } else {
            1.0
        };
        Self {
            position,
            quaternion: self.normalized_quaternion(),
            scale,
        }
    }

    /// Render-rate interpolation between two rigid-body poses. Rotation uses
    /// shortest-arc quaternion SLERP (hemisphere flip on negative dot, linear
    /// normalized fallback above 0.9995); position and uniform scale lerp.
    /// Non-finite `alpha` fails closed to the previous pose and invalid input
    /// fields sanitize exactly like [`SubWorldTransform::model_matrix`].
    pub fn lerp_slerp(prev: &Self, curr: &Self, alpha: f32) -> Self {
        let t = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let a = (*prev).sanitized();
        let b = (*curr).sanitized();
        let mut position = [0.0; 3];
        for (i, slot) in position.iter_mut().enumerate() {
            *slot = a.position[i] + (b.position[i] - a.position[i]) * t;
        }
        let scale = a.scale + (b.scale - a.scale) * t;
        let qa = a.quaternion;
        let mut qb = b.quaternion;
        let mut dot: f32 = qa.iter().zip(qb.iter()).map(|(x, y)| x * y).sum();
        if dot < 0.0 {
            dot = -dot;
            for value in &mut qb {
                *value = -*value;
            }
        }
        let quaternion = if dot > 0.9995 {
            let mut q = [0.0; 4];
            for i in 0..4 {
                q[i] = qa[i] + (qb[i] - qa[i]) * t;
            }
            normalize_quat_or_identity(q)
        } else {
            let theta = dot.clamp(-1.0, 1.0).acos();
            let sin_theta = theta.sin();
            if sin_theta <= 1.0e-7 || !sin_theta.is_finite() {
                qa
            } else {
                let wa = ((1.0 - t) * theta).sin() / sin_theta;
                let wb = (t * theta).sin() / sin_theta;
                let mut q = [0.0; 4];
                for i in 0..4 {
                    q[i] = qa[i] * wa + qb[i] * wb;
                }
                normalize_quat_or_identity(q)
            }
        };
        Self {
            position,
            quaternion,
            scale: if scale.is_finite() && scale > 1.0e-6 {
                scale
            } else {
                1.0
            },
        }
    }

    /// Transform a model-space point into world space:
    /// `position + rotate(quaternion, local * scale)`. Invalid input fails
    /// closed to the transform origin.
    pub fn transform_point(&self, local: [f32; 3]) -> [f32; 3] {
        let transform = (*self).sanitized();
        if !local.iter().all(|v| v.is_finite()) {
            return transform.position;
        }
        let scaled = [
            local[0] * transform.scale,
            local[1] * transform.scale,
            local[2] * transform.scale,
        ];
        let rotated = rotate_unit_quaternion(transform.quaternion, scaled);
        let world = [
            transform.position[0] + rotated[0],
            transform.position[1] + rotated[1],
            transform.position[2] + rotated[2],
        ];
        if world.iter().all(|v| v.is_finite()) {
            world
        } else {
            transform.position
        }
    }

    /// Inverse of [`SubWorldTransform::transform_point`] for rider-camera
    /// local locking. Invalid input or a degenerate scale fails closed to
    /// the model origin.
    pub fn inverse_transform_point(&self, world: [f32; 3]) -> [f32; 3] {
        let transform = (*self).sanitized();
        if !world.iter().all(|v| v.is_finite()) {
            return [0.0; 3];
        }
        let delta = [
            world[0] - transform.position[0],
            world[1] - transform.position[1],
            world[2] - transform.position[2],
        ];
        let inverse = [
            -transform.quaternion[0],
            -transform.quaternion[1],
            -transform.quaternion[2],
            transform.quaternion[3],
        ];
        let unrotated = rotate_unit_quaternion(inverse, delta);
        let local = [
            unrotated[0] / transform.scale,
            unrotated[1] / transform.scale,
            unrotated[2] / transform.scale,
        ];
        if local.iter().all(|v| v.is_finite()) {
            local
        } else {
            [0.0; 3]
        }
    }

    /// Column-major `mat4x4<f32>` model matrix:
    /// `translate(position) * rotate(quaternion) * scale(scale)`.
    pub fn model_matrix(&self) -> Mat4 {
        let [x, y, z, w] = self.normalized_quaternion();
        let s = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };
        let pos = if self.position.iter().all(|v| v.is_finite()) {
            self.position
        } else {
            [0.0; 3]
        };
        let (x2, y2, z2) = (x + x, y + y, z + z);
        let (xx, xy, xz) = (x * x2, x * y2, x * z2);
        let (yy, yz, zz) = (y * y2, y * z2, z * z2);
        let (wx, wy, wz) = (w * x2, w * y2, w * z2);
        [
            [(1.0 - (yy + zz)) * s, (xy + wz) * s, (xz - wy) * s, 0.0],
            [(xy - wz) * s, (1.0 - (xx + zz)) * s, (yz + wx) * s, 0.0],
            [(xz + wy) * s, (yz - wx) * s, (1.0 - (xx + yy)) * s, 0.0],
            [pos[0], pos[1], pos[2], 1.0],
        ]
    }

    /// `view_proj * model_matrix()` — the `mvp` the draw uniform carries.
    pub fn mvp(&self, view_proj: &Mat4) -> Mat4 {
        mat4_mul(view_proj, &self.model_matrix())
    }
}

impl Default for SubWorldTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

fn normalize_quat_or_identity(q: [f32; 4]) -> [f32; 4] {
    let len2 = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
    if len2.is_finite() && len2 > 1.0e-12 {
        let inv = len2.sqrt().recip();
        [q[0] * inv, q[1] * inv, q[2] * inv, q[3] * inv]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

fn rotate_unit_quaternion(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let qv = [q[0], q[1], q[2]];
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let uv = cross(qv, v);
    let uuv = cross(qv, uv);
    [
        v[0] + 2.0 * (q[3] * uv[0] + uuv[0]),
        v[1] + 2.0 * (q[3] * uv[1] + uuv[1]),
        v[2] + 2.0 * (q[3] * uv[2] + uuv[2]),
    ]
}

/// Axis-angle constructor (radians) for ergonomic test/demo authoring.
/// `axis` need not be normalized; zero/NaN yields identity rotation.
pub fn quat_from_axis_angle(axis: [f32; 3], radians: f32) -> [f32; 4] {
    let len2 = axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2];
    if !(len2.is_finite() && len2 > 1.0e-12 && radians.is_finite()) {
        return [0.0, 0.0, 0.0, 1.0];
    }
    let inv = len2.sqrt().recip();
    let (sin, cos) = (radians * 0.5).sin_cos();
    [
        axis[0] * inv * sin,
        axis[1] * inv * sin,
        axis[2] * inv * sin,
        cos,
    ]
}

// ---------------------------------------------------------------------------
// CPU-side sub-world bank (authoritative scene table)
// ---------------------------------------------------------------------------

/// One rigid sub-world's render payload: shared `TerrainVertex` geometry
/// plus the transform placing it this frame. `dirty` marks geometry the GPU
/// cache must re-upload; transform changes alone never set it.
#[derive(Clone, Debug)]
pub struct SubWorldRenderEntry {
    /// Stable sub-world id (contraption/airship handle).
    pub id: u64,
    /// 6-DOF placement for the next frame.
    pub transform: SubWorldTransform,
    /// Placement before the latest tick update. Render-rate consumers use
    /// [`SubWorldRenderEntry::interpolated_transform`] instead of stepping
    /// directly from this pose to `transform`.
    pub prev_transform: SubWorldTransform,
    /// Conservative non-negative model-space expansion applied by culling
    /// for geometry protruding outside its nominal bounds (oversized Create
    /// wheels, modded blocks). Negative or non-finite values read as zero.
    pub protrusion_margin: f32,
    /// Model-space lit vertices.
    pub vertices: Vec<TerrainVertex>,
    /// `u32` index buffer into `vertices`.
    pub indices: Vec<u32>,
    /// Global alpha multiplier (`1.0` opaque; clouds use `< 1`).
    pub alpha: f32,
    /// Packed world-light word the composed pass reads out of
    /// [`CompositeDrawUniforms::params2`]`[2]`: `block_light` nibble at
    /// bits 0..4, `sky_light` nibble at bits 4..8, [`SUBWORLD_LIGHT_ENABLED`]
    /// at bit 8. `0` keeps the legacy env-only shading — `(0, 0)` is a
    /// valid pitch-black sample, never a disable.
    pub world_light: u32,
    /// Set when `vertices`/`indices` changed since the last GPU sync.
    pub dirty: bool,
}

/// Enable bit in the packed world-light word (`SubWorldRenderEntry::
/// world_light` and `CompositeDrawUniforms::params2` bitcast `z`).
pub const SUBWORLD_LIGHT_ENABLED: u32 = 1 << 8;

/// Pack a nibble `(block_light, sky_light)` sample into the enabled
/// world-light word.
pub fn pack_subworld_light(block_light: u8, sky_light: u8) -> u32 {
    u32::from(block_light.min(15))
        | (u32::from(sky_light.min(15)) << 4)
        | SUBWORLD_LIGHT_ENABLED
}

/// Inverse of [`pack_subworld_light`]: `Some((block_light, sky_light))`
/// when the enable bit is set, `None` for the disabled (`0`) word.
pub fn unpack_subworld_light(word: u32) -> Option<(u8, u8)> {
    if word & SUBWORLD_LIGHT_ENABLED == 0 {
        return None;
    }
    Some(((word & 0xf) as u8, ((word >> 4) & 0xf) as u8))
}

impl SubWorldRenderEntry {
    /// New dirty entry at `transform`.
    pub fn new(
        id: u64,
        transform: SubWorldTransform,
        vertices: Vec<TerrainVertex>,
        indices: Vec<u32>,
    ) -> Self {
        Self {
            id,
            transform,
            prev_transform: transform,
            protrusion_margin: 0.0,
            vertices,
            indices,
            alpha: 1.0,
            world_light: 0,
            dirty: true,
        }
    }

    /// Interpolated pose for the current render frame. The bank keeps tick
    /// endpoints in `prev_transform`/`transform`; callers vary `alpha` at
    /// display rate so 20 TPS contraptions remain smooth at high Hz.
    pub fn interpolated_transform(&self, alpha: f32) -> SubWorldTransform {
        SubWorldTransform::lerp_slerp(&self.prev_transform, &self.transform, alpha)
    }

    /// Fail-closed culling expansion: only finite, non-negative margins count.
    pub fn effective_protrusion_margin(&self) -> f32 {
        if self.protrusion_margin.is_finite() && self.protrusion_margin >= 0.0 {
            self.protrusion_margin
        } else {
            0.0
        }
    }
}

/// GPU-instanced mechanical-part record: one shared-template instance
/// (cogwheel, shaft, chest shell). 48 bytes — the same stride
/// `KineticInstanceGpu`/`BlockEntityInstanceGpu` use in client-port, so the
/// instance stream matches the `ber_instancing` contract.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct InstancedPartRecord {
    /// `xyz` = instance origin in model space, `w` = uniform scale.
    pub pos_scale: [f32; 4],
    /// `xyz` = Euler rotation in radians (applied X then Y then Z in the
    /// vertex shader), `w` = animation/atlas frame index.
    pub rot_uv_frame: [f32; 4],
    /// Packed light: block-light nibble | sky-light nibble `<< 4`.
    pub packed_light: u32,
    /// RGBA tint, one byte per channel (R in bits 0..8).
    pub tint_rgba: u32,
    /// Shared mesh template this instance draws.
    pub mesh_kind: u32,
    /// Sub-world slot; `0` is the static world grid.
    pub subworld_id: u32,
}

impl From<BerInstanceData> for InstancedPartRecord {
    /// Lift a `ber_instancing` shell record into the render stream; the
    /// caller supplies `mesh_kind`/`subworld_id` (the batching keys).
    fn from(data: BerInstanceData) -> Self {
        Self {
            pos_scale: data.pos_scale,
            rot_uv_frame: data.rot_uv_frame,
            packed_light: data.packed_light,
            tint_rgba: data.tint_rgba,
            mesh_kind: 0,
            subworld_id: 0,
        }
    }
}

const PART_INSTANCE_ATTRS: [wgpu::VertexAttribute; 4] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 0,
        shader_location: 3,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 16,
        shader_location: 4,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32x2,
        offset: 32,
        shader_location: 5,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32x2,
        offset: 40,
        shader_location: 6,
    },
];

/// Per-instance stream layout (buffer slot 1 of the instanced pipeline).
pub const PART_INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<InstancedPartRecord>() as wgpu::BufferAddress,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &PART_INSTANCE_ATTRS,
};

// ---------------------------------------------------------------------------
// GPU-instanced particle billboards
// ---------------------------------------------------------------------------

/// Static unit-quad corners the billboard vertex shader expands per
/// instance — same order `VanillaParticleWorld::billboard_quads` emits
/// (`(-1,-1) (1,-1) (1,1) (-1,1)`, CCW).
pub const PARTICLE_BILLBOARD_CORNERS: [[f32; 2]; 4] =
    [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];

/// Triangle list for the billboard unit quad — identical winding to
/// `append_dynamic_quad`'s `[base, base+1, base+2, base, base+2, base+3]`.
pub const PARTICLE_BILLBOARD_INDICES: [u32; 6] = [0, 1, 2, 0, 2, 3];

/// One particle's billboard record (76 B — replaces the 216 B of
/// CPU-expanded quad vertices+indices the old path uploaded per particle).
/// `axis_u`/`axis_v` are the quad's half-edge vectors: for a
/// camera-facing quad they are the view right/up scaled by the half
/// extents, so the vertex shader's
/// `center + corner.x*axis_u + corner.y*axis_v` reproduces the original
/// CPU-expanded corner positions. `VanillaParticleQuad` carries no
/// explicit rotation field — in-plane rotation and shear are baked into
/// the axis vectors.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct ParticleInstance {
    /// World-space quad center.
    pub center: [f32; 3],
    /// Half-edge toward the quad's +u corners (camera-right × half-width
    /// for screen-facing billboards).
    pub axis_u: [f32; 3],
    /// Half-edge toward the +v corners (camera-up × half-height).
    pub axis_v: [f32; 3],
    /// Affine UV basis — `uv = uv_origin + c.x*uv_du + c.y*uv_dv` with `c`
    /// the unit corner coordinate `(corner + 1) / 2`. Reproduces any
    /// per-corner `[[f32; 2]; 4]` assignment exactly.
    pub uv_origin: [f32; 2],
    pub uv_du: [f32; 2],
    pub uv_dv: [f32; 2],
    /// `rgb` albedo tint; `a` is the per-particle alpha the CPU path
    /// pushed through per-draw dynamic-offset uniform slots.
    pub color: [f32; 4],
}

/// `corner` slot for the billboard unit quad (buffer 0).
const PARTICLE_QUAD_ATTRS: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
    format: wgpu::VertexFormat::Float32x2,
    offset: 0,
    shader_location: 0,
}];

/// Static unit-quad stream (buffer slot 0 of the billboard pipeline).
pub const PARTICLE_QUAD_LAYOUT: wgpu::VertexBufferLayout<'static> =
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &PARTICLE_QUAD_ATTRS,
    };

/// `@location(1..7)` mirror of [`ParticleInstance`].
const PARTICLE_INSTANCE_ATTRS: [wgpu::VertexAttribute; 7] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 1,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 12,
        shader_location: 2,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 24,
        shader_location: 3,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 36,
        shader_location: 4,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 44,
        shader_location: 5,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 52,
        shader_location: 6,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 60,
        shader_location: 7,
    },
];

/// Per-instance billboard stream (buffer slot 1 of the billboard pipeline).
pub const PARTICLE_INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> =
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<ParticleInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &PARTICLE_INSTANCE_ATTRS,
    };

/// How one CPU-expanded `VanillaParticleQuad` packs for the instanced
/// billboard path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParticleBillboardPack {
    /// Not renderable on either path — non-finite fields or a degenerate
    /// quad, the same inputs `set_particle_billboards` already skips.
    Skip,
    /// Packed billboard instance.
    Instanced(ParticleInstance),
    /// Renderable but outside the bilinear-billboard model
    /// (non-parallelogram positions or non-affine UV corners). The caller
    /// must route the whole call through the CPU quad path so draw order
    /// and output stay identical.
    CpuOnly,
}

/// Pack one CPU-expanded `VanillaParticleQuad` into a [`ParticleInstance`].
/// Mirrors the skip conditions of the `TerrainVertex` staging path:
/// non-finite color, degenerate quad, or non-finite positions/UVs are
/// dropped; quads outside the parallelogram/affine-UV model report
/// [`ParticleBillboardPack::CpuOnly`].
pub fn pack_particle_billboard(quad: &VanillaParticleQuad) -> ParticleBillboardPack {
    if !quad.color.iter().all(|v| v.is_finite()) {
        return ParticleBillboardPack::Skip;
    }
    let p = &quad.positions;
    let u = [
        p[1][0] - p[0][0],
        p[1][1] - p[0][1],
        p[1][2] - p[0][2],
    ];
    let v = [
        p[3][0] - p[0][0],
        p[3][1] - p[0][1],
        p[3][2] - p[0][2],
    ];
    let n = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if !length.is_finite() || length <= f32::EPSILON {
        return ParticleBillboardPack::Skip;
    }
    if !p
        .iter()
        .flatten()
        .chain(quad.uv.iter().flatten())
        .all(|value| value.is_finite())
    {
        return ParticleBillboardPack::Skip;
    }
    let uv_du = [quad.uv[1][0] - quad.uv[0][0], quad.uv[1][1] - quad.uv[0][1]];
    let uv_dv = [quad.uv[3][0] - quad.uv[0][0], quad.uv[3][1] - quad.uv[0][1]];
    let edge_len = (u[0] * u[0] + u[1] * u[1] + u[2] * u[2]).sqrt()
        + (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
        + 1e-3;
    let pos_dev = (p[2][0] - (p[0][0] + u[0] + v[0]))
        .abs()
        .max((p[2][1] - (p[0][1] + u[1] + v[1])).abs())
        .max((p[2][2] - (p[0][2] + u[2] + v[2])).abs());
    let uv_len = uv_du[0].abs() + uv_du[1].abs() + uv_dv[0].abs() + uv_dv[1].abs() + 1e-3;
    let uv_dev = (quad.uv[2][0] - (quad.uv[0][0] + uv_du[0] + uv_dv[0]))
        .abs()
        .max((quad.uv[2][1] - (quad.uv[0][1] + uv_du[1] + uv_dv[1])).abs());
    if pos_dev > 1e-4 * edge_len || uv_dev > 1e-4 * uv_len {
        return ParticleBillboardPack::CpuOnly;
    }
    ParticleBillboardPack::Instanced(ParticleInstance {
        center: [
            (p[0][0] + p[2][0]) * 0.5,
            (p[0][1] + p[2][1]) * 0.5,
            (p[0][2] + p[2][2]) * 0.5,
        ],
        axis_u: [u[0] * 0.5, u[1] * 0.5, u[2] * 0.5],
        axis_v: [v[0] * 0.5, v[1] * 0.5, v[2] * 0.5],
        uv_origin: quad.uv[0],
        uv_du,
        uv_dv,
        color: [
            quad.color[0].clamp(0.0, 1.0),
            quad.color[1].clamp(0.0, 1.0),
            quad.color[2].clamp(0.0, 1.0),
            quad.color[3].clamp(0.0, 1.0),
        ],
    })
}

/// One shared-template instanced batch: every record draws the same
/// `template` mesh, so a thousand cogwheels cost one draw call.
#[derive(Clone, Debug)]
pub struct InstancedPartBatch {
    /// Shared template id (`mesh_kind`).
    pub mesh_kind: u32,
    /// Template geometry in model space.
    pub template_vertices: Vec<TerrainVertex>,
    /// Template index buffer.
    pub template_indices: Vec<u32>,
    /// Per-instance records.
    pub instances: Vec<InstancedPartRecord>,
    /// Batch-level model transform (usually identity or the owning
    /// airship's sub-world matrix).
    pub transform: SubWorldTransform,
    /// Template geometry changed since last GPU sync.
    pub geometry_dirty: bool,
    /// Instance list changed since last GPU sync.
    pub instances_dirty: bool,
}

impl InstancedPartBatch {
    /// New fully-dirty batch with an empty instance list.
    pub fn new(
        mesh_kind: u32,
        template_vertices: Vec<TerrainVertex>,
        template_indices: Vec<u32>,
    ) -> Self {
        Self {
            mesh_kind,
            template_vertices,
            template_indices,
            instances: Vec::new(),
            transform: SubWorldTransform::IDENTITY,
            geometry_dirty: true,
            instances_dirty: true,
        }
    }
}

/// The authoritative sub-world scene table. Entries and batches are keyed
/// maps so a steady frame iterates in deterministic order and uploads
/// nothing.
#[derive(Clone, Debug, Default)]
pub struct SubWorldRenderBank {
    entries: BTreeMap<u64, SubWorldRenderEntry>,
    batches: BTreeMap<u32, InstancedPartBatch>,
    pose_interpolator: SubWorldPoseInterpolator,
}

impl SubWorldRenderBank {
    /// Empty bank.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest one complete SW6D snapshot. Existing geometry remains attached
    /// to surviving ids; new ids receive an empty, dirty render entry and ids
    /// absent from the snapshot are removed.
    pub fn apply_sync_frame(&mut self, frame: &SubWorldSyncFrame) {
        self.pose_interpolator.ingest(frame);
        let live: std::collections::BTreeSet<u64> =
            frame.poses.iter().map(|pose| pose.subworld_id).collect();
        self.entries.retain(|id, _| live.contains(id));
        for pose in &frame.poses {
            let transform = SubWorldTransform {
                position: pose.pos.map(|value| value as f32),
                quaternion: pose.quat,
                scale: 1.0,
            };
            self.entries.entry(pose.subworld_id).or_insert_with(|| {
                SubWorldRenderEntry::new(pose.subworld_id, transform, Vec::new(), Vec::new())
            });
            if let Some(entry) = self.entries.get_mut(&pose.subworld_id) {
                let jump = entry
                    .transform
                    .position
                    .iter()
                    .zip(transform.position.iter())
                    .map(|(a, b)| f64::from(b - a))
                    .map(|d| d * d)
                    .sum::<f64>()
                    .sqrt()
                    > 32.0;
                if jump {
                    entry.prev_transform = transform;
                entry.transform = transform;
                } else {
                    entry.prev_transform = entry.transform;
                    entry.transform = transform;
                }
            }
        }
    }

    /// Apply an interpolated point between the previous and current SW6D
    /// snapshots. `alpha` is clamped to `[0, 1]`; callers can run this once
    /// per render frame while server snapshots arrive at 20 Hz.
    pub fn interpolate_poses(&mut self, alpha: f32) {
        for (id, _) in self.pose_interpolator.poses() {
            if let Some(pose) = self.pose_interpolator.interpolate(id, alpha) {
                if let Some(entry) = self.entries.get_mut(&id) {
                    entry.transform.position = pose.pos.map(|value| value as f32);
                    entry.transform.quaternion = pose.quat;
                }
            }
        }
    }

    /// Insert or replace an entry; the geometry is (re)uploaded on the next
    /// GPU sync only when `entry.dirty` is set.
    pub fn upsert(&mut self, entry: SubWorldRenderEntry) {
        self.entries.insert(entry.id, entry);
    }

    /// Insert or update a rotating Create contraption — a windmill or
    /// mechanical-bearing assembly spun `angle_rad` radians about `axis`
    /// (the bearing's local axis, passed through
    /// [`quat_from_axis_angle`]) and anchored at world-space `origin`.
    /// `blocks_mesh` is the contraption's model-space lit
    /// `(vertices, indices)` geometry, authored around the pivot.
    ///
    /// Cheap per-tick path: calling this every rotation tick with the
    /// same mesh updates only the transform — geometry re-uploads only
    /// when the mesh itself changes, so a spinning water wheel costs
    /// zero vertex writes.
    pub fn upsert_contraption_subworld(
        &mut self,
        id: u64,
        origin: [f32; 3],
        axis: [f32; 3],
        angle_rad: f32,
        blocks_mesh: &(Vec<TerrainVertex>, Vec<u32>),
    ) {
        let transform = SubWorldTransform {
            position: origin,
            quaternion: quat_from_axis_angle(axis, angle_rad),
            scale: 1.0,
        };
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.prev_transform = entry.transform;
                entry.transform = transform;
                if entry.vertices != blocks_mesh.0 || entry.indices != blocks_mesh.1 {
                    entry.vertices = blocks_mesh.0.clone();
                    entry.indices = blocks_mesh.1.clone();
                    entry.dirty = true;
                }
            }
            None => {
                self.entries.insert(
                    id,
                    SubWorldRenderEntry::new(
                        id,
                        transform,
                        blocks_mesh.0.clone(),
                        blocks_mesh.1.clone(),
                    ),
                );
            }
        }
    }

    /// Update an entry's transform without dirtying its geometry.
    /// Returns `false` for an unknown id.
    pub fn set_transform(&mut self, id: u64, transform: SubWorldTransform) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.prev_transform = entry.transform;
                entry.transform = transform;
                true
            }
            None => false,
        }
    }

    /// Snap directly to `transform` with no interpolation smear. Teleports
    /// and large server corrections use this instead of [`Self::set_transform`].
    pub fn snap_transform(&mut self, id: u64, transform: SubWorldTransform) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.prev_transform = transform;
                entry.transform = transform;
                true
            }
            None => false,
        }
    }

    /// Render-rate interpolated transform for `id`, if present.
    pub fn interpolated_transform(&self, id: u64, alpha: f32) -> Option<SubWorldTransform> {
        self.entries
            .get(&id)
            .map(|entry| entry.interpolated_transform(alpha))
    }

    /// Set the conservative culling protrusion margin for modded oversized
    /// geometry. Values are stored verbatim but non-finite/negative values
    /// fail closed to zero when read. Returns `false` for an unknown id.
    pub fn set_protrusion_margin(&mut self, id: u64, margin: f32) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.protrusion_margin = margin;
                true
            }
            None => false,
        }
    }

    /// Replace an entry's geometry and mark it dirty. Returns `false` for
    /// an unknown id.
    pub fn set_geometry(
        &mut self,
        id: u64,
        vertices: Vec<TerrainVertex>,
        indices: Vec<u32>,
    ) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.vertices = vertices;
                entry.indices = indices;
                entry.dirty = true;
                true
            }
            None => false,
        }
    }

    /// Assign a real world `(block_light, sky_light)` nibble sample to a
    /// sub-world: the composed pass folds it into the draw uniform so a
    /// moving contraption or airship darkens in caves and brightens
    /// beside torches instead of keeping fixed env ambient+sun. The
    /// update lands through the draw-uniform pool — geometry stays
    /// clean (no re-upload). Returns `false` for an unknown id.
    pub fn set_subworld_light(&mut self, id: u64, block_light: u8, sky_light: u8) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.world_light = pack_subworld_light(block_light, sky_light);
                true
            }
            None => false,
        }
    }

    /// Drop a sub-world's world-light override back to the legacy
    /// env-only shading. Returns `false` for an unknown id.
    pub fn clear_subworld_light(&mut self, id: u64) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.world_light = 0;
                true
            }
            None => false,
        }
    }

    /// Drop a sub-world; its GPU buffers release on the next sync.
    pub fn remove(&mut self, id: u64) -> bool {
        self.entries.remove(&id).is_some()
    }

    /// Read an entry by id.
    pub fn get(&self, id: u64) -> Option<&SubWorldRenderEntry> {
        self.entries.get(&id)
    }

    /// All entries in ascending id order (deterministic draw order).
    pub fn entries(&self) -> impl Iterator<Item = &SubWorldRenderEntry> {
        self.entries.values()
    }

    /// Live sub-world count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no sub-worlds are registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or replace an instanced batch.
    pub fn upsert_batch(&mut self, batch: InstancedPartBatch) {
        self.batches.insert(batch.mesh_kind, batch);
    }

    /// Replace a batch's instance list (geometry untouched). Returns
    /// `false` for an unknown `mesh_kind`.
    pub fn set_batch_instances(
        &mut self,
        mesh_kind: u32,
        instances: Vec<InstancedPartRecord>,
    ) -> bool {
        match self.batches.get_mut(&mesh_kind) {
            Some(batch) => {
                batch.instances = instances;
                batch.instances_dirty = true;
                true
            }
            None => false,
        }
    }

    /// Drop an instanced batch; its buffers release on the next sync.
    pub fn remove_batch(&mut self, mesh_kind: u32) -> bool {
        self.batches.remove(&mesh_kind).is_some()
    }

    /// All instanced batches in ascending `mesh_kind` order.
    pub fn batches(&self) -> impl Iterator<Item = &InstancedPartBatch> {
        self.batches.values()
    }

    pub(crate) fn entries_mut(&mut self) -> impl Iterator<Item = &mut SubWorldRenderEntry> {
        self.entries.values_mut()
    }

    pub(crate) fn batches_mut(&mut self) -> impl Iterator<Item = &mut InstancedPartBatch> {
        self.batches.values_mut()
    }
}

#[cfg(test)]
mod subworld_sync_tests {
    use super::*;
    use crate::subworld_sync::{SubWorldPose, SubWorldSyncFrame};

    fn pose(id: u64, x: f64, quat: [f32; 4]) -> SubWorldPose {
        SubWorldPose {
            subworld_id: id,
            pos: [x, 0.0, 0.0],
            quat,
            vel: [0.0; 3],
            ang_vel: [0.0; 3],
            flags: 0,
        }
    }

    #[test]
    fn sync_snapshot_inserts_updates_removes_and_interpolates() {
        let mut bank = SubWorldRenderBank::new();
        bank.apply_sync_frame(&SubWorldSyncFrame {
            poses: vec![pose(1, 0.0, [0.0, 0.0, 0.0, 1.0]), pose(2, 8.0, [0.0, 0.0, 0.0, 1.0])],
        });
        assert_eq!(bank.len(), 2);
        bank.apply_sync_frame(&SubWorldSyncFrame {
            poses: vec![pose(1, 10.0, [0.0, 0.0, 1.0, 0.0])],
        });
        assert_eq!(bank.len(), 1);
        assert!(bank.get(2).is_none());
        bank.interpolate_poses(0.5);
        let transform = bank.get(1).unwrap().transform;
        assert!((transform.position[0] - 5.0).abs() < 1e-5);
        assert!((transform.quaternion.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn subworld_light_packs_decodes_and_clears() {
        // The packed word round-trips and defaults to disabled.
        assert_eq!(unpack_subworld_light(pack_subworld_light(14, 9)), Some((14, 9)));
        assert_eq!(unpack_subworld_light(pack_subworld_light(0, 0)), Some((0, 0)));
        assert_eq!(unpack_subworld_light(0), None);
        assert_eq!(pack_subworld_light(255, 255) & 0xff, 0xff);

        let mut bank = SubWorldRenderBank::new();
        let mesh = box_mesh([0.0; 3], [1.0; 3], [1.0; 3]);
        bank.upsert_contraption_subworld(7, [0.0; 3], [0.0, 1.0, 0.0], 0.0, &mesh);
        // Unknown ids fail cleanly; known ids accept the sample without
        // touching the geometry-dirty flag (draw-uniform carry).
        assert!(!bank.set_subworld_light(99, 15, 15));
        assert!(bank.set_subworld_light(7, 4, 15));
        let entry = bank.get(7).unwrap();
        assert_eq!(unpack_subworld_light(entry.world_light), Some((4, 15)));
        assert!(entry.dirty);
        // A pitch-black cave sample stays enabled — `0` is not a disable.
        assert!(bank.set_subworld_light(7, 0, 0));
        assert_eq!(
            unpack_subworld_light(bank.get(7).unwrap().world_light),
            Some((0, 0))
        );
        assert!(bank.clear_subworld_light(7));
        assert_eq!(
            unpack_subworld_light(bank.get(7).unwrap().world_light),
            None
        );
        assert!(!bank.clear_subworld_light(99));
        // Entries minted through the sync path default to env-only shading.
        bank.apply_sync_frame(&SubWorldSyncFrame {
            poses: vec![pose(11, 0.0, [0.0, 0.0, 0.0, 1.0])],
        });
        assert_eq!(
            unpack_subworld_light(bank.get(11).unwrap().world_light),
            None
        );
    }

    #[test]
    fn transform_lerp_slerp_uses_shortest_arc_and_is_nan_safe() {
        let prev = SubWorldTransform::IDENTITY;
        let mut curr = SubWorldTransform::IDENTITY;
        curr.quaternion = quat_from_axis_angle([0.0, 1.0, 0.0], 350.0f32.to_radians());
        curr.position = [10.0, 2.0, -4.0];
        curr.scale = 2.0;
        let mid = SubWorldTransform::lerp_slerp(&prev, &curr, 0.5);
        assert_eq!(mid.position, [5.0, 1.0, -2.0]);
        assert!((mid.scale - 1.5).abs() < 1e-6);
        assert!(
            mid.quaternion[1] < 0.0,
            "350 degrees must interpolate as -10 degrees, not the long way: {:?}",
            mid.quaternion
        );
        assert!((mid.quaternion.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5);

        let at_prev = SubWorldTransform::lerp_slerp(&prev, &curr, f32::NAN);
        assert_eq!(at_prev.position, prev.position);
        let mut bad = curr;
        bad.position[0] = f32::NAN;
        bad.quaternion = [f32::NAN; 4];
        bad.scale = -1.0;
        let sanitized = SubWorldTransform::lerp_slerp(&prev, &bad, 0.5);
        assert!(sanitized.position.iter().all(|v| v.is_finite()));
        assert!(sanitized.quaternion.iter().all(|v| v.is_finite()));
        assert!(sanitized.scale.is_finite() && sanitized.scale > 0.0);
    }

    #[test]
    fn rider_local_lock_point_round_trip() {
        let transform = SubWorldTransform {
            position: [12.5, 70.0, -8.25],
            quaternion: quat_from_axis_angle([0.3, 1.0, -0.2], 67.0f32.to_radians()),
            scale: 1.75,
        };
        let local = [1.25, -0.5, 3.0];
        let world = transform.transform_point(local);
        let round_trip = transform.inverse_transform_point(world);
        for index in 0..3 {
            assert!(
                (round_trip[index] - local[index]).abs() < 2.0e-5,
                "axis {index}: {round_trip:?} != {local:?}"
            );
        }
        assert_eq!(transform.transform_point([f32::NAN; 3]), transform.position);
        assert_eq!(transform.inverse_transform_point([f32::NAN; 3]), [0.0; 3]);
    }

    #[test]
    fn bank_preserves_tick_endpoints_interpolates_and_snaps_teleports() {
        let mut bank = SubWorldRenderBank::new();
        let mesh = box_mesh([0.0; 3], [1.0; 3], [1.0; 3]);
        bank.upsert_contraption_subworld(9, [0.0; 3], [0.0, 1.0, 0.0], 0.0, &mesh);
        let mut target = SubWorldTransform::IDENTITY;
        target.position = [10.0, 0.0, 0.0];
        assert!(bank.set_transform(9, target));
        let mid = bank.interpolated_transform(9, 0.5).unwrap();
        assert_eq!(mid.position, [5.0, 0.0, 0.0]);
        assert_eq!(bank.get(9).unwrap().prev_transform.position, [0.0; 3]);

        let teleport = SubWorldTransform {
            position: [80.0, 0.0, 0.0],
            ..SubWorldTransform::IDENTITY
        };
        assert!(bank.snap_transform(9, teleport));
        let entry = bank.get(9).unwrap();
        assert_eq!(entry.prev_transform.position, [80.0, 0.0, 0.0]);
        assert_eq!(entry.interpolated_transform(0.0).position, [80.0, 0.0, 0.0]);

        assert!(bank.set_protrusion_margin(9, 2.5));
        assert_eq!(bank.get(9).unwrap().effective_protrusion_margin(), 2.5);
        assert!(bank.set_protrusion_margin(9, f32::NAN));
        assert_eq!(bank.get(9).unwrap().effective_protrusion_margin(), 0.0);
        assert!(bank.set_protrusion_margin(9, -1.0));
        assert_eq!(bank.get(9).unwrap().effective_protrusion_margin(), 0.0);
    }
}

// ---------------------------------------------------------------------------
// GPU-side cache: one buffer pair per sub-world, one stream per batch
// ---------------------------------------------------------------------------

/// GPU-resident copy of one [`SubWorldRenderEntry`].
pub struct GpuSubWorldEntry {
    /// Sub-world id mirrored from the bank (cloud decks and shadow
    /// filters key on it).
    pub id: u64,
    /// Transform mirrored from the bank.
    pub transform: SubWorldTransform,
    /// Global alpha multiplier.
    pub alpha: f32,
    /// Packed world-light word mirrored from the bank (`0` = disabled).
    pub world_light: u32,
    /// Conservative culling expansion mirrored from the bank.
    pub protrusion_margin: f32,
    /// Index count in `indices`.
    pub index_count: u32,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    vertex_capacity: usize,
    index_capacity: usize,
}

/// GPU-resident copy of one [`InstancedPartBatch`].
pub struct GpuInstancedBatch {
    /// Shared template id.
    pub mesh_kind: u32,
    /// Batch model transform.
    pub transform: SubWorldTransform,
    /// Template index count.
    pub index_count: u32,
    /// Live instance count in `instances`.
    pub instance_count: u32,
    template_vertices: wgpu::Buffer,
    template_indices: wgpu::Buffer,
    instances: wgpu::Buffer,
    vertex_capacity: usize,
    index_capacity: usize,
    instance_capacity: usize,
}

impl GpuSubWorldEntry {
    /// Vertex buffer accessor for the render pass.
    pub fn vertex_buffer(&self) -> &wgpu::Buffer {
        &self.vertices
    }

    /// Index buffer accessor for the render pass.
    pub fn index_buffer(&self) -> &wgpu::Buffer {
        &self.indices
    }
}

impl GpuInstancedBatch {
    /// Template vertex buffer accessor.
    pub fn template_vertex_buffer(&self) -> &wgpu::Buffer {
        &self.template_vertices
    }

    /// Template index buffer accessor.
    pub fn template_index_buffer(&self) -> &wgpu::Buffer {
        &self.template_indices
    }

    /// Instance record buffer accessor.
    pub fn instance_buffer(&self) -> &wgpu::Buffer {
        &self.instances
    }
}

fn vertex_upload_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    verts: &[TerrainVertex],
    existing: Option<(wgpu::Buffer, usize)>,
) -> (wgpu::Buffer, usize) {
    let needed = verts.len();
    if let Some((buffer, capacity)) = existing {
        if capacity >= needed {
            if needed > 0 {
                queue.write_buffer(&buffer, 0, bytemuck::cast_slice(verts));
            }
            return (buffer, capacity);
        }
    }
    let capacity = needed.max(16) + needed / 2;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: (capacity * std::mem::size_of::<TerrainVertex>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if needed > 0 {
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(verts));
    }
    (buffer, capacity)
}

fn index_upload_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    indices: &[u32],
    existing: Option<(wgpu::Buffer, usize)>,
) -> (wgpu::Buffer, usize) {
    let needed = indices.len();
    if let Some((buffer, capacity)) = existing {
        if capacity >= needed {
            if needed > 0 {
                queue.write_buffer(&buffer, 0, bytemuck::cast_slice(indices));
            }
            return (buffer, capacity);
        }
    }
    let capacity = needed.max(16) + needed / 2;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: (capacity * std::mem::size_of::<u32>()) as u64,
        usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if needed > 0 {
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(indices));
    }
    (buffer, capacity)
}

fn instance_upload_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    records: &[InstancedPartRecord],
    existing: Option<(wgpu::Buffer, usize)>,
) -> (wgpu::Buffer, usize) {
    let needed = records.len();
    if let Some((buffer, capacity)) = existing {
        if capacity >= needed {
            if needed > 0 {
                queue.write_buffer(&buffer, 0, bytemuck::cast_slice(records));
            }
            return (buffer, capacity);
        }
    }
    let capacity = needed.max(16) + needed / 2;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: (capacity * std::mem::size_of::<InstancedPartRecord>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if needed > 0 {
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(records));
    }
    (buffer, capacity)
}

/// GPU-resident mirror of [`SubWorldRenderBank`]: per-entry vertex/index
/// buffers and per-batch template/instance buffers. `sync` re-uploads only
/// dirty payloads and releases removed ids, so a steady frame allocates
/// nothing and touches no geometry.
#[derive(Default)]
pub struct SubWorldGpuCache {
    entries: BTreeMap<u64, GpuSubWorldEntry>,
    batches: BTreeMap<u32, GpuInstancedBatch>,
}

impl SubWorldGpuCache {
    /// Empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Uploaded sub-world count.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Uploaded instanced-batch count.
    pub fn batch_count(&self) -> usize {
        self.batches.len()
    }

    /// GPU entries in id order (draw order).
    pub fn entries(&self) -> impl Iterator<Item = &GpuSubWorldEntry> {
        self.entries.values()
    }

    /// GPU batches in `mesh_kind` order (draw order).
    pub fn batches(&self) -> impl Iterator<Item = &GpuInstancedBatch> {
        self.batches.values()
    }

    /// Mirror `bank` onto the device: upload dirty geometry/instance
    /// payloads, refresh transforms/alphas, release removed ids, and clear
    /// the bank's dirty flags.
    pub fn sync(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bank: &mut SubWorldRenderBank,
    ) {
        // Drop GPU entries whose sub-world was removed (both sides are
        // sorted, so binary search keeps this O(n log n)).
        let live: Vec<u64> = bank.entries().map(|e| e.id).collect();
        self.entries.retain(|id, _| live.binary_search(id).is_ok());
        let live_batches: Vec<u32> = bank.batches().map(|b| b.mesh_kind).collect();
        self.batches
            .retain(|kind, _| live_batches.binary_search(kind).is_ok());

        for entry in bank.entries_mut() {
            let gpu = match self.entries.remove(&entry.id) {
                Some(mut gpu) => {
                    if entry.dirty {
                        let (vbuf, vcap) = vertex_upload_buffer(
                            device,
                            queue,
                            "subworld-verts",
                            &entry.vertices,
                            Some((gpu.vertices, gpu.vertex_capacity)),
                        );
                        gpu.vertices = vbuf;
                        gpu.vertex_capacity = vcap;
                        let (ibuf, icap) = index_upload_buffer(
                            device,
                            queue,
                            "subworld-indices",
                            &entry.indices,
                            Some((gpu.indices, gpu.index_capacity)),
                        );
                        gpu.indices = ibuf;
                        gpu.index_capacity = icap;
                    }
                    gpu
                }
                None => {
                    let (vertices, vertex_capacity) = vertex_upload_buffer(
                        device,
                        queue,
                        "subworld-verts",
                        &entry.vertices,
                        None,
                    );
                    let (indices, index_capacity) = index_upload_buffer(
                        device,
                        queue,
                        "subworld-indices",
                        &entry.indices,
                        None,
                    );
                    GpuSubWorldEntry {
                        id: entry.id,
                        transform: entry.transform,
                        alpha: entry.alpha,
                        world_light: entry.world_light,
                        protrusion_margin: entry.effective_protrusion_margin(),
                        index_count: entry.indices.len() as u32,
                        vertices,
                        indices,
                        vertex_capacity,
                        index_capacity,
                    }
                }
            };
            let mut gpu = gpu;
            gpu.transform = entry.transform;
            gpu.alpha = entry.alpha;
            gpu.world_light = entry.world_light;
            gpu.protrusion_margin = entry.effective_protrusion_margin();
            gpu.index_count = entry.indices.len() as u32;
            entry.dirty = false;
            self.entries.insert(entry.id, gpu);
        }

        for batch in bank.batches_mut() {
            let gpu = match self.batches.remove(&batch.mesh_kind) {
                Some(mut gpu) => {
                    if batch.geometry_dirty {
                        let (vbuf, vcap) = vertex_upload_buffer(
                            device,
                            queue,
                            "part-template-verts",
                            &batch.template_vertices,
                            Some((gpu.template_vertices, gpu.vertex_capacity)),
                        );
                        gpu.template_vertices = vbuf;
                        gpu.vertex_capacity = vcap;
                        let (ibuf, icap) = index_upload_buffer(
                            device,
                            queue,
                            "part-template-indices",
                            &batch.template_indices,
                            Some((gpu.template_indices, gpu.index_capacity)),
                        );
                        gpu.template_indices = ibuf;
                        gpu.index_capacity = icap;
                    }
                    if batch.instances_dirty {
                        let (buf, cap) = instance_upload_buffer(
                            device,
                            queue,
                            "part-instances",
                            &batch.instances,
                            Some((gpu.instances, gpu.instance_capacity)),
                        );
                        gpu.instances = buf;
                        gpu.instance_capacity = cap;
                    }
                    gpu
                }
                None => {
                    let (template_vertices, vertex_capacity) = vertex_upload_buffer(
                        device,
                        queue,
                        "part-template-verts",
                        &batch.template_vertices,
                        None,
                    );
                    let (template_indices, index_capacity) = index_upload_buffer(
                        device,
                        queue,
                        "part-template-indices",
                        &batch.template_indices,
                        None,
                    );
                    let (instances, instance_capacity) = instance_upload_buffer(
                        device,
                        queue,
                        "part-instances",
                        &batch.instances,
                        None,
                    );
                    GpuInstancedBatch {
                        mesh_kind: batch.mesh_kind,
                        transform: batch.transform,
                        index_count: batch.template_indices.len() as u32,
                        instance_count: batch.instances.len() as u32,
                        template_vertices,
                        template_indices,
                        instances,
                        vertex_capacity,
                        index_capacity,
                        instance_capacity,
                    }
                }
            };
            let mut gpu = gpu;
            gpu.transform = batch.transform;
            gpu.index_count = batch.template_indices.len() as u32;
            gpu.instance_count = batch.instances.len() as u32;
            batch.geometry_dirty = false;
            batch.instances_dirty = false;
            self.batches.insert(batch.mesh_kind, gpu);
        }
    }
}

// ---------------------------------------------------------------------------
// Uniform mirrors (Pod) for the composed pass
// ---------------------------------------------------------------------------

/// Per-draw uniform record: `view_proj * model` (pre-composed on the CPU so
/// the shader costs one multiply) plus the model matrix for world-space
/// normals, the chunk-fade alpha, and a global alpha multiplier. Padded to
/// exactly [`DRAW_UNIFORM_STRIDE`] bytes so the scratch vec writes into the
/// dynamic-offset pool at the stride the offsets index.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct CompositeDrawUniforms {
    /// `view_proj * model` clip transform.
    pub mvp: Mat4,
    /// Model matrix for world-space position/normal.
    pub model: Mat4,
    /// `x` = chunk-fade alpha, `y` = global alpha multiplier.
    pub params: [f32; 4],
    /// `xy` = per-draw dynamic-light mask (bitcast `u64`, `0` =
    /// check-all); `z` = bitcast packed sub-world world-light word
    /// ([`pack_subworld_light`], `0` = disabled); `w` reserved.
    pub params2: [f32; 4],
    /// Stride padding: 160 bytes of data + 96 bytes pad = 256.
    pub _stride_pad: [f32; 24],
}

/// Byte stride the dynamic-offset draw uniform pool steps by (the wgpu
/// minimum uniform offset alignment on all primary backends).
pub const DRAW_UNIFORM_STRIDE: u32 = 256;

const _: () = assert!(
    std::mem::size_of::<CompositeDrawUniforms>() == DRAW_UNIFORM_STRIDE as usize,
    "CompositeDrawUniforms must pack to the dynamic-offset stride"
);

/// Shared environment uniform: sun, hemisphere ambient, exponential fog
/// density, and the underwater/cave depth tint the
/// [`crate::atmosphere::BiomeAtmosphereBlender`] resolves each frame.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct CompositeEnvUniforms {
    /// `xyz` = unit direction toward the sun, `w` = sun intensity.
    pub sun: [f32; 4],
    /// `rgb` = hemisphere ambient, `w` = exposure/brightness scale.
    pub ambient: [f32; 4],
    /// `x` = exponential fog density, `yzw` reserved.
    pub atmo: [f32; 4],
    /// `rgb` = depth tint color, `w` = depth tint strength.
    pub depth_tint: [f32; 4],
}

/// `Pod` mirror of client-port's `GpuPointLight` (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct GpuPointLightGpu {
    /// `xyz` = light position, `w` = radius.
    pub position_radius: [f32; 4],
    /// `rgb` = color, `w` = intensity.
    pub color_intensity: [f32; 4],
}

/// `Pod` mirror of client-port's [`VfxLightUniforms`]: 64 point lights plus
/// a count word — the binding [`COMPOSITE_LIT_WGSL`] and
/// [`COMPOSITE_TERRAIN_WGSL`] share.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct VfxLightsGpu {
    /// `x` = live light count.
    pub light_count: [u32; 4],
    /// Packed point lights.
    pub lights: [GpuPointLightGpu; 64],
}

impl From<&VfxLightUniforms> for VfxLightsGpu {
    fn from(src: &VfxLightUniforms) -> Self {
        let mut out = Self {
            light_count: src.light_count,
            lights: [GpuPointLightGpu::default(); 64],
        };
        for (dst, src) in out.lights.iter_mut().zip(src.lights.iter()) {
            *dst = GpuPointLightGpu {
                position_radius: src.position_radius,
                color_intensity: src.color_intensity,
            };
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// Face table for one axis-aligned box: `(outward normal, four corners)`
/// per face in `+X, -X, +Y, -Y, +Z, -Z` order — the same order
/// `vanilla_plus_nature`'s `FACE_POS_X..FACE_NEG_Z` bit flags enumerate.
/// Corners wind counter-clockwise seen from outside.
pub(crate) fn box_face_corners(min: [f32; 3], max: [f32; 3]) -> [([f32; 3], [[f32; 3]; 4]); 6] {
    let (x0, y0, z0) = (min[0], min[1], min[2]);
    let (x1, y1, z1) = (max[0], max[1], max[2]);
    [
        (
            [1.0, 0.0, 0.0],
            [[x1, y0, z0], [x1, y1, z0], [x1, y1, z1], [x1, y0, z1]],
        ),
        (
            [-1.0, 0.0, 0.0],
            [[x0, y0, z1], [x0, y1, z1], [x0, y1, z0], [x0, y0, z0]],
        ),
        (
            [0.0, 1.0, 0.0],
            [[x0, y1, z0], [x0, y1, z1], [x1, y1, z1], [x1, y1, z0]],
        ),
        (
            [0.0, -1.0, 0.0],
            [[x0, y0, z1], [x0, y0, z0], [x1, y0, z0], [x1, y0, z1]],
        ),
        (
            [0.0, 0.0, 1.0],
            [[x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1]],
        ),
        (
            [0.0, 0.0, -1.0],
            [[x1, y0, z0], [x0, y0, z0], [x0, y1, z0], [x1, y1, z0]],
        ),
    ]
}

/// Emit one axis-aligned box as a [`TerrainVertex`] mesh (positions in
/// `min..=max`, outward unit normals, uniform `albedo`). Returns
/// `(vertices, indices)` — 24 verts / 36 indices — for airship hulls,
/// cloud cells, and instanced mechanical-part templates.
pub fn box_mesh(min: [f32; 3], max: [f32; 3], albedo: [f32; 3]) -> (Vec<TerrainVertex>, Vec<u32>) {
    let faces = box_face_corners(min, max);
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (normal, corners) in faces {
        let base = vertices.len() as u32;
        for (corner, position) in corners.into_iter().enumerate() {
            vertices.push(TerrainVertex::new(
                position,
                normal,
                albedo,
                [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]][corner],
                u32::MAX,
            ));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

/// One filled voxel of a sub-world hull grid (a Create contraption block or
/// an Aeronautics airship deck block): the material every exposed face of
/// the cell carries verbatim — albedo color, the `TerrainVertex`
/// material-flags byte, and the atlas tile (`u32::MAX` = untextured).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubWorldCell {
    /// Linear-ish albedo rgb.
    pub albedo: [f32; 3],
    /// Material-flags byte in `TerrainVertex::packed_albedo` bits 24..32.
    pub material_flags: u8,
    /// Index into the uploaded atlas rectangle table; `u32::MAX` untextured.
    pub atlas_index: u32,
}

/// Greedy-merged voxel-hull geometry: [`TerrainVertex`] vertices plus `u32`
/// indices — the exact layout [`box_mesh`] and the lit pipeline consume.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubWorldMesh {
    /// Merged-quad vertices, four per quad.
    pub vertices: Vec<TerrainVertex>,
    /// Two triangles per quad.
    pub indices: Vec<u32>,
}

/// 6-axis 2D greedy voxel-hull mesher for Create contraption and
/// Aeronautics airship sub-worlds.
///
/// `cell_at` samples the bounded `dims` grid: `Some(cell)` is a solid voxel,
/// `None` empty space. A cell face is exposed exactly when the neighbour
/// along its normal is empty or outside the grid — shared interior faces
/// are culled on both sides. Exposed faces are then greedy-merged into
/// maximal coplanar rectangles whose cells carry the identical
/// [`SubWorldCell`] material, so the emitted hull preserves the exact
/// exposed surface area, outward normals, material flags and per-block UV
/// tiling of the naive one-quad-per-face mesh (merged quads keep
/// world-proportional tangent-plane UVs, like the cloud mesher).
pub fn greedy_voxel_hull_mesh(
    dims: [usize; 3],
    mut cell_at: impl FnMut(usize, usize, usize) -> Option<SubWorldCell>,
) -> SubWorldMesh {
    let mut mesh = SubWorldMesh::default();
    if dims.contains(&0) {
        return mesh;
    }
    let mut mask: Vec<Option<SubWorldCell>> = Vec::new();
    for axis in 0..3 {
        let u_ax = (axis + 1) % 3;
        let v_ax = (axis + 2) % 3;
        let (nu, nv) = (dims[u_ax], dims[v_ax]);
        for step in [-1i32, 1] {
            // `box_face_corners` order is +X,-X,+Y,-Y,+Z,-Z.
            let face_index = axis * 2 + usize::from(step < 0);
            for slice in 0..dims[axis] {
                mask.clear();
                mask.resize(nu * nv, None);
                for v in 0..nv {
                    for u in 0..nu {
                        let mut p = [0usize; 3];
                        p[axis] = slice;
                        p[u_ax] = u;
                        p[v_ax] = v;
                        let Some(cell) = cell_at(p[0], p[1], p[2]) else {
                            continue;
                        };
                        let next = p[axis] as i32 + step;
                        let exposed = !(0..dims[axis] as i32).contains(&next) || {
                            let mut q = p;
                            q[axis] = next as usize;
                            cell_at(q[0], q[1], q[2]).is_none()
                        };
                        if exposed {
                            mask[v * nu + u] = Some(cell);
                        }
                    }
                }
                for v in 0..nv {
                    for u in 0..nu {
                        let index = v * nu + u;
                        let Some(cell) = mask[index] else {
                            continue;
                        };
                        let mut width = 1usize;
                        while u + width < nu && mask[v * nu + u + width] == Some(cell) {
                            width += 1;
                        }
                        let mut height = 1usize;
                        'grow: while v + height < nv {
                            for k in 0..width {
                                if mask[(v + height) * nu + u + k] != Some(cell) {
                                    break 'grow;
                                }
                            }
                            height += 1;
                        }
                        for dv in 0..height {
                            mask[(v + dv) * nu + u..(v + dv) * nu + u + width].fill(None);
                        }
                        let mut min = [0.0f32; 3];
                        let mut max = [0.0f32; 3];
                        min[axis] = (slice as i32 + i32::from(step > 0)) as f32;
                        max[axis] = min[axis];
                        min[u_ax] = u as f32;
                        max[u_ax] = (u + width) as f32;
                        min[v_ax] = v as f32;
                        max[v_ax] = (v + height) as f32;
                        let (normal, corners) = box_face_corners(min, max)[face_index];
                        let base = mesh.vertices.len() as u32;
                        for position in corners {
                            let mut vertex = TerrainVertex::new(
                                position,
                                normal,
                                cell.albedo,
                                [position[u_ax], position[v_ax]],
                                cell.atlas_index,
                            );
                            vertex.set_material_flags(cell.material_flags);
                            mesh.vertices.push(vertex);
                        }
                        mesh.indices
                            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
                    }
                }
            }
        }
    }
    mesh
}

// ---------------------------------------------------------------------------
// Composed-pass WGSL modules
// ---------------------------------------------------------------------------

/// Packed-vertex terrain for the composed frame: the same 16-byte
/// `PackedVertex` decode as the built-in opaque shader, plus a per-draw
/// `fade_alpha` dissolve (new chunks blend in from the fog color instead of
/// popping), exponential atmosphere fog, depth tint, and the shared VFX1
/// dynamic colored point lights.
///
/// Bind groups: `0` = `frame` + `section` uniforms (the existing terrain
/// bind groups), `1` = [`CompositeDrawUniforms`] via dynamic offset,
/// `2` = [`CompositeEnvUniforms`] + [`VfxLightsGpu`].
pub const COMPOSITE_TERRAIN_WGSL: &str = "\
// Composed-pass opaque terrain: packed vertex + fade + lights + exp fog.
struct FrameUniforms {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    fog_start: f32,
    fog_color: vec3<f32>,
    fog_end: f32,
    _pad: vec2<f32>,
};

struct SectionUniforms {
    origin: vec3<f32>,
    _pad0: f32,
    biome_tint: vec3<f32>,
    biome_tint_strength: f32,
};

struct DrawUniforms {
    mvp: mat4x4<f32>,
    model: mat4x4<f32>,
    params: vec4<f32>,
    params2: vec4<f32>,
};

struct EnvUniforms {
    sun: vec4<f32>,
    ambient: vec4<f32>,
    atmo: vec4<f32>,
    depth_tint: vec4<f32>,
};

struct PointLight {
    position_radius: vec4<f32>,
    color_intensity: vec4<f32>,
};

struct DynamicLights {
    light_count: vec4<u32>,
    lights: array<PointLight, 64>,
};

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(0) @binding(1) var<uniform> section: SectionUniforms;
@group(1) @binding(0) var<uniform> draw: DrawUniforms;
@group(2) @binding(0) var<uniform> env: EnvUniforms;
@group(2) @binding(1) var<uniform> dyn_lights: DynamicLights;
// One slot per atlas sprite: `rect` is the active `.mcmeta` frame window
// (whole-strip rect for static sprites) and `anim` carries the next
// slot's top edge plus the `interpolate` blend factor.
struct SpriteFrame {
    rect: vec4<f32>,
    anim: vec4<f32>,
};
struct AtlasRects { rects: array<SpriteFrame> };
@group(2) @binding(2) var atlas_tex: texture_2d<f32>;
@group(2) @binding(3) var atlas_sampler: sampler;
@group(2) @binding(4) var<storage, read> atlas_rects: AtlasRects;
struct AtlasStates { sprites: array<u32> };
@group(2) @binding(5) var<storage, read> atlas_states: AtlasStates;

struct TerrainVertex {
    @location(0) packed_pos: u32,
    @location(1) state_id: u32,
    @location(2) attributes: u32,
    @location(3) tint: u32,
};

struct TerrainFragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) albedo: vec3<f32>,
    @location(3) illum: f32,
    @location(4) uv: vec2<f32>,
    @location(5) @interpolate(flat) atlas_rect: vec4<f32>,
    @location(6) fog: f32,
    @location(7) @interpolate(flat) atlas_anim: vec4<f32>,
};

const FACE_SHADE = array<f32, 6>(0.45, 1.0, 0.75, 0.85, 0.7, 0.95);
const AO_SHADE = array<f32, 4>(0.4, 0.6, 0.8, 1.0);

fn face_normal(dir: u32) -> vec3<f32> {
    switch dir {
        case 0u: { return vec3<f32>(0.0, -1.0, 0.0); }
        case 1u: { return vec3<f32>(0.0, 1.0, 0.0); }
        case 2u: { return vec3<f32>(0.0, 0.0, -1.0); }
        case 3u: { return vec3<f32>(0.0, 0.0, 1.0); }
        case 4u: { return vec3<f32>(-1.0, 0.0, 0.0); }
        default: { return vec3<f32>(1.0, 0.0, 0.0); }
    }
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> vec3<f32> {
    let i = floor(h * 6.0);
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let sector = i32(i) % 6;
    if sector == 0 { return vec3<f32>(v, t, p); }
    if sector == 1 { return vec3<f32>(q, v, p); }
    if sector == 2 { return vec3<f32>(p, v, t); }
    if sector == 3 { return vec3<f32>(p, q, v); }
    if sector == 4 { return vec3<f32>(t, p, v); }
    return vec3<f32>(v, p, q);
}

fn palette_color(state_id: u32) -> vec3<f32> {
    return hsv_to_rgb(fract(f32(state_id) * 0.618033988749895), 0.5, 0.85);
}

fn tint_rgb(tint: u32) -> vec3<f32> {
    return vec3<f32>(
        f32((tint >> 16u) & 0xffu) / 255.0,
        f32((tint >> 8u) & 0xffu) / 255.0,
        f32(tint & 0xffu) / 255.0,
    );
}

fn tiled_uv(p: vec3<f32>, direction: u32) -> vec2<f32> {
    if direction < 2u { return p.xz; }
    if direction < 4u { return p.xy; }
    return p.zy;
}

// Per-quad sprite lookup hoisted into the vertex stage: one `atlas_states`
// + `atlas_rects` fetch per vertex replaces two storage reads per pixel.
// A zero-area rect encodes the out-of-range fallback (`vec3(1.0)`).
fn atlas_frame_for(state_id: u32) -> SpriteFrame {
    if state_id >= arrayLength(&atlas_states.sprites) {
        return SpriteFrame(vec4<f32>(0.0), vec4<f32>(0.0));
    }
    let sprite_id = atlas_states.sprites[state_id];
    if sprite_id >= arrayLength(&atlas_rects.rects) {
        return SpriteFrame(vec4<f32>(0.0), vec4<f32>(0.0));
    }
    return atlas_rects.rects[sprite_id];
}

// `anim` is (next slot's v0, interpolate blend): the second fetch only
// runs for `.mcmeta` sprites mid-transition, so static terrain stays a
// single texture fetch.
fn atlas_sample_rect(rect: vec4<f32>, anim: vec4<f32>, uv: vec2<f32>) -> vec3<f32> {
    if rect.z <= 0.0 || rect.w <= 0.0 {
        return vec3<f32>(1.0);
    }
    let duv_dx = dpdx(uv) * rect.zw;
    let duv_dy = dpdy(uv) * rect.zw;
    let cur = textureSampleGrad(
        atlas_tex, atlas_sampler,
        rect.xy + fract(uv) * rect.zw,
        duv_dx, duv_dy,
    ).rgb;
    if anim.y <= 0.0 {
        return cur;
    }
    let next = textureSampleGrad(
        atlas_tex, atlas_sampler,
        vec2<f32>(rect.x + fract(uv.x) * rect.z,
                  anim.x + fract(uv.y) * rect.w),
        duv_dx, duv_dy,
    ).rgb;
    return mix(cur, next, anim.y);
}

// Fog share the vertex stage computes once per vertex; the fragment stage
// consumes the interpolated factor instead of `distance`/`exp` per pixel.
fn fog_factor_at(world: vec3<f32>) -> f32 {
    let dist = distance(world, frame.camera_pos);
    let linear = clamp(
        (dist - frame.fog_start) / max(frame.fog_end - frame.fog_start, 0.001),
        0.0,
        1.0,
    );
    let expo = 1.0 - exp(-dist * env.atmo.x);
    return clamp(max(linear, expo), 0.0, 1.0);
}

// Prefix mask over the first `count` of 64 dynamic lights (u64 as lo/hi).
fn light_mask_prefix(count: u32) -> vec2<u32> {
    if (count >= 64u) {
        return vec2<u32>(0xFFFFFFFFu, 0xFFFFFFFFu);
    }
    if (count == 32u) {
        return vec2<u32>(0xFFFFFFFFu, 0u);
    }
    if (count > 32u) {
        return vec2<u32>(0xFFFFFFFFu, (1u << (count - 32u)) - 1u);
    }
    if (count == 0u) {
        return vec2<u32>(0u, 0u);
    }
    return vec2<u32>((1u << count) - 1u, 0u);
}

// Removes and returns the lowest set bit index of the u64 mask
// (lo = x, hi = y), or 64 when empty — one countTrailingZeros per light.
fn light_mask_take(mask: ptr<function, vec2<u32>>) -> u32 {
    if ((*mask).x != 0u) {
        let i = countTrailingZeros((*mask).x);
        (*mask).x = (*mask).x & ((*mask).x - 1u);
        return i;
    }
    if ((*mask).y != 0u) {
        let i = countTrailingZeros((*mask).y);
        (*mask).y = (*mask).y & ((*mask).y - 1u);
        return 32u + i;
    }
    return 64u;
}

// Draw-scoped light mask: the prefix over `count` admitted lights ANDed
// by the shared union mask in light_count.yz (zero = no shared culling)
// and this draw record's region mask packed into params2.xy (zero =
// check-all for this draw). The walk below visits only bits that could
// reach the drawn AABB.
fn draw_light_mask(count: u32) -> vec2<u32> {
    var mask = light_mask_prefix(count);
    let region = dyn_lights.light_count.yz;
    if (region.x != 0u || region.y != 0u) {
        mask = mask & region;
    }
    let per_draw = vec2<u32>(
        bitcast<u32>(draw.params2.x),
        bitcast<u32>(draw.params2.y),
    );
    if (per_draw.x != 0u || per_draw.y != 0u) {
        mask = mask & per_draw;
    }
    return mask;
}

// Apply one luminance-independent shoulder to preserve highlight chromaticity.
fn filmic(color: vec3<f32>) -> vec3<f32> {
    let peak = max(max(color.r, color.g), color.b);
    let mapped = min((peak * (2.51 * peak + 0.03)) /
        max(peak * (2.43 * peak + 0.59) + 0.14, 0.00001), 1.0);
    return color * (mapped / max(peak, 0.00001));
}

@vertex
fn terrain_vs(in: TerrainVertex) -> TerrainFragment {
    let lx = f32(in.packed_pos & 0x1fu);
    let lz = f32((in.packed_pos >> 5u) & 0x1fu);
    let ly = f32(in.packed_pos >> 10u) / 1024.0;
    let attr = in.attributes;
    let direction = attr & 0x7u;
    let ao = (attr >> 3u) & 0x3u;
    let block_light = f32((attr >> 5u) & 0xfu);
    let sky_light = f32((attr >> 9u) & 0xfu);
    let world = section.origin + vec3<f32>(lx, ly, lz);

    var albedo = palette_color(in.state_id);
    if in.tint != 0u {
        albedo = tint_rgb(in.tint);
    } else {
        albedo = albedo * mix(
            vec3<f32>(1.0),
            section.biome_tint,
            section.biome_tint_strength,
        );
    }

    let light = clamp(max(block_light, sky_light) / 15.0, 0.0, 1.0);
    let illum = 0.25 + 0.75 * light;
    let shade = FACE_SHADE[min(direction, 5u)] * AO_SHADE[min(ao, 3u)] * illum;

    var out: TerrainFragment;
    out.clip = frame.view_proj * vec4<f32>(world, 1.0);
    out.world = world;
    out.normal = face_normal(direction);
    out.albedo = albedo * shade;
    out.illum = shade;
    out.uv = tiled_uv(vec3<f32>(lx, ly, lz), direction);
    let sprite = atlas_frame_for(in.state_id);
    out.atlas_rect = sprite.rect;
    out.atlas_anim = sprite.anim;
    out.fog = fog_factor_at(world);
    return out;
}

struct TranslucentVertex {
    @location(0) packed_pos: u32,
    @location(1) state_id: u32,
    @location(2) attributes: u32,
    @location(3) tint: u32,
};

struct TranslucentFragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) albedo: vec3<f32>,
    @location(3) alpha: f32,
    @location(4) uv: vec2<f32>,
    @location(5) @interpolate(flat) atlas_rect: vec4<f32>,
    @location(6) fog: f32,
    @location(7) @interpolate(flat) atlas_anim: vec4<f32>,
};

@vertex
fn terrain_translucent_vs(in: TranslucentVertex) -> TranslucentFragment {
    let lx = f32(in.packed_pos & 0x1fu);
    let lz = f32((in.packed_pos >> 5u) & 0x1fu);
    let ly = f32(in.packed_pos >> 10u) / 1024.0;
    let attr = in.attributes;
    let direction = attr & 0x7u;
    let ao = (attr >> 3u) & 0x3u;
    let block_light = f32((attr >> 5u) & 0xfu);
    let sky_light = f32((attr >> 9u) & 0xfu);
    let layer = (attr >> 13u) & 0x3u;
    let fluid_kind = (attr >> 15u) & 0x3u;
    let world = section.origin + vec3<f32>(lx, ly, lz);

    var albedo = palette_color(in.state_id);
    if in.tint != 0u {
        albedo = tint_rgb(in.tint);
    } else {
        albedo = albedo * mix(
            vec3<f32>(1.0),
            section.biome_tint,
            section.biome_tint_strength,
        );
    }

    // LAYER_FLUID=3 with FLUID_LAVA=2 is nearly opaque; other fluids and
    // stained glass stay translucent.
    var alpha = 0.55;
    if layer == 3u {
        alpha = select(0.75, 0.95, fluid_kind == 2u);
    }

    let light = clamp(max(block_light, sky_light) / 15.0, 0.0, 1.0);
    let illum = 0.25 + 0.75 * light;
    let shade = FACE_SHADE[min(direction, 5u)] * AO_SHADE[min(ao, 3u)] * illum;

    var out: TranslucentFragment;
    out.clip = frame.view_proj * vec4<f32>(world, 1.0);
    out.world = world;
    out.normal = face_normal(direction);
    out.albedo = albedo * shade;
    out.alpha = alpha;
    out.uv = tiled_uv(vec3<f32>(lx, ly, lz), direction);
    let sprite = atlas_frame_for(in.state_id);
    out.atlas_rect = sprite.rect;
    out.atlas_anim = sprite.anim;
    out.fog = fog_factor_at(world);
    return out;
}

@fragment
fn terrain_fs(in: TerrainFragment) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let texel = atlas_sample_rect(in.atlas_rect, in.atlas_anim, in.uv);
    var color = in.albedo;
    let surface = in.albedo * texel;
    color *= texel;
    var light_mask = draw_light_mask(min(dyn_lights.light_count.x, 64u));
    loop {
        let i = light_mask_take(&light_mask);
        if (i >= 64u) {
            break;
        }
        let pl = dyn_lights.lights[i];
        let delta = pl.position_radius.xyz - in.world;
        let radius = max(pl.position_radius.w, 0.01);
        let dist_sq = dot(delta, delta);
        // Outside the bounding sphere the falloff is exactly zero —
        // skip before paying the sqrt.
        if (dist_sq >= radius * radius) {
            continue;
        }
        let dist = sqrt(max(dist_sq, 0.0001));
        let t = max(1.0 - dist * (1.0 / radius), 0.0);
        let falloff = t * t;
        let diffuse = max(dot(n, delta / dist), 0.0);
        color += surface * pl.color_intensity.xyz * pl.color_intensity.w
            * diffuse * falloff;
    }
    color = mix(color, color * env.depth_tint.rgb, env.depth_tint.w);
    color = mix(color, frame.fog_color, in.fog);
    let fade = clamp(draw.params.x, 0.0, 1.0);
    color = mix(frame.fog_color, color, fade);
    return vec4<f32>(filmic(color), 1.0);
}

@fragment
fn terrain_translucent_fs(in: TranslucentFragment) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let surface = in.albedo * atlas_sample_rect(in.atlas_rect, in.atlas_anim, in.uv);
    var color = surface;
    var light_mask = draw_light_mask(min(dyn_lights.light_count.x, 64u));
    loop {
        let i = light_mask_take(&light_mask);
        if (i >= 64u) {
            break;
        }
        let pl = dyn_lights.lights[i];
        let delta = pl.position_radius.xyz - in.world;
        let radius = max(pl.position_radius.w, 0.01);
        let dist_sq = dot(delta, delta);
        if (dist_sq >= radius * radius) {
            continue;
        }
        let dist = sqrt(max(dist_sq, 0.0001));
        let t = max(1.0 - dist * (1.0 / radius), 0.0);
        let falloff = t * t;
        let diffuse = max(dot(n, delta / dist), 0.0);
        color += surface * pl.color_intensity.xyz * pl.color_intensity.w
            * diffuse * falloff;
    }
    color = mix(color, color * env.depth_tint.rgb, env.depth_tint.w);
    let fog_smooth = in.fog * in.fog * (3.0 - 2.0 * in.fog);
    color = mix(color, frame.fog_color, fog_smooth);
    let fade = clamp(draw.params.x, 0.0, 1.0);
    color = mix(frame.fog_color, color, fade);
    return vec4<f32>(filmic(color), in.alpha * (1.0 - 0.35 * fog_smooth) * fade);
}
";

/// Lit `TerrainVertex` geometry (sub-worlds, clouds, planets) plus the
/// shared-template instanced mechanical-part path. `lit_vs` applies the
/// per-draw `model`/`mvp`; `inst_vs` rotates/scales each instance by its
/// [`InstancedPartRecord`] stream then applies the batch's `model`/`mvp`;
/// `lit_fs` is the shared shaded fragment (hemisphere ambient + sun +
/// VFX1 point lights + depth tint + exp fog + fade/alpha).
///
/// Bind groups: `0` = `frame`, `1` = [`CompositeDrawUniforms`] via dynamic
/// offset, `2` = [`CompositeEnvUniforms`] + [`VfxLightsGpu`].
pub const COMPOSITE_LIT_WGSL: &str = "\
// Composed-pass lit geometry + instanced mechanical parts.
struct FrameUniforms {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    fog_start: f32,
    fog_color: vec3<f32>,
    fog_end: f32,
    _pad: vec2<f32>,
};

struct DrawUniforms {
    mvp: mat4x4<f32>,
    model: mat4x4<f32>,
    params: vec4<f32>,
    params2: vec4<f32>,
};

struct EnvUniforms {
    sun: vec4<f32>,
    ambient: vec4<f32>,
    atmo: vec4<f32>,
    depth_tint: vec4<f32>,
};

struct PointLight {
    position_radius: vec4<f32>,
    color_intensity: vec4<f32>,
};

struct DynamicLights {
    light_count: vec4<u32>,
    lights: array<PointLight, 64>,
};

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var<uniform> draw: DrawUniforms;
@group(2) @binding(0) var<uniform> env: EnvUniforms;
@group(2) @binding(1) var<uniform> dyn_lights: DynamicLights;
// Same `SpriteFrame` layout the packed-terrain module declares: `rect` is
// the active `.mcmeta` frame window, `anim` the next slot's v0 + blend.
struct SpriteFrame {
    rect: vec4<f32>,
    anim: vec4<f32>,
};
struct AtlasRects { rects: array<SpriteFrame> };
@group(2) @binding(2) var atlas_tex: texture_2d<f32>;
@group(2) @binding(3) var atlas_sampler: sampler;
@group(2) @binding(4) var<storage, read> atlas_rects: AtlasRects;

struct LitVertex {
    @location(0) position: vec3<f32>,
    @location(1) packed_normal: u32,
    @location(2) packed_albedo: u32,
    @location(7) uv: vec2<f32>,
    @location(8) atlas_index: u32,
};

struct PartInstance {
    @location(3) pos_scale: vec4<f32>,
    @location(4) rot: vec4<f32>,
    @location(5) light_tint: vec2<u32>,
    @location(6) kind_sub: vec2<u32>,
};

struct LitFragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) albedo: vec3<f32>,
    @location(3) light_add: f32,
    @location(4) uv: vec2<f32>,
    @location(5) @interpolate(flat) atlas_index: u32,
    @location(6) alpha: f32,
    @location(7) @interpolate(flat) mat_flags: u32,
};

// GPU-instanced particle billboards: a static unit quad (slot 0) plus
// per-instance center/axis records (slot 1). The VS expands corners with
// the quad's billboard axes — the camera right/up vectors the producer
// scaled by half extent — so no world positions are built on the CPU.
struct BillboardQuadVertex {
    @location(0) corner: vec2<f32>,
};

struct BillboardInstance {
    @location(1) center: vec3<f32>,
    @location(2) axis_u: vec3<f32>,
    @location(3) axis_v: vec3<f32>,
    @location(4) uv_origin: vec2<f32>,
    @location(5) uv_du: vec2<f32>,
    @location(6) uv_dv: vec2<f32>,
    @location(7) color: vec4<f32>,
};

fn atlas_sample(sprite_id: u32, uv: vec2<f32>) -> vec3<f32> {
    if sprite_id >= arrayLength(&atlas_rects.rects) {
        return vec3<f32>(1.0);
    }
    let frame = atlas_rects.rects[sprite_id];
    let rect = frame.rect;
    let duv_dx = dpdx(uv) * rect.zw;
    let duv_dy = dpdy(uv) * rect.zw;
    let cur = textureSampleGrad(
        atlas_tex, atlas_sampler,
        rect.xy + fract(uv) * rect.zw,
        duv_dx, duv_dy,
    ).rgb;
    if frame.anim.y <= 0.0 {
        return cur;
    }
    let next = textureSampleGrad(
        atlas_tex, atlas_sampler,
        vec2<f32>(rect.x + fract(uv.x) * rect.z,
                  frame.anim.x + fract(uv.y) * rect.w),
        duv_dx, duv_dy,
    ).rgb;
    return mix(cur, next, frame.anim.y);
}

// Prefix mask over the first `count` of 64 dynamic lights (u64 as lo/hi).
fn light_mask_prefix(count: u32) -> vec2<u32> {
    if (count >= 64u) {
        return vec2<u32>(0xFFFFFFFFu, 0xFFFFFFFFu);
    }
    if (count == 32u) {
        return vec2<u32>(0xFFFFFFFFu, 0u);
    }
    if (count > 32u) {
        return vec2<u32>(0xFFFFFFFFu, (1u << (count - 32u)) - 1u);
    }
    if (count == 0u) {
        return vec2<u32>(0u, 0u);
    }
    return vec2<u32>((1u << count) - 1u, 0u);
}

// Removes and returns the lowest set bit index of the u64 mask
// (lo = x, hi = y), or 64 when empty — one countTrailingZeros per light.
fn light_mask_take(mask: ptr<function, vec2<u32>>) -> u32 {
    if ((*mask).x != 0u) {
        let i = countTrailingZeros((*mask).x);
        (*mask).x = (*mask).x & ((*mask).x - 1u);
        return i;
    }
    if ((*mask).y != 0u) {
        let i = countTrailingZeros((*mask).y);
        (*mask).y = (*mask).y & ((*mask).y - 1u);
        return 32u + i;
    }
    return 64u;
}

// Draw-scoped light mask: the prefix over `count` admitted lights ANDed
// by the shared union mask in light_count.yz (zero = no shared culling)
// and this draw record's region mask packed into params2.xy (zero =
// check-all for this draw).
fn draw_light_mask(count: u32) -> vec2<u32> {
    var mask = light_mask_prefix(count);
    let region = dyn_lights.light_count.yz;
    if (region.x != 0u || region.y != 0u) {
        mask = mask & region;
    }
    let per_draw = vec2<u32>(
        bitcast<u32>(draw.params2.x),
        bitcast<u32>(draw.params2.y),
    );
    if (per_draw.x != 0u || per_draw.y != 0u) {
        mask = mask & per_draw;
    }
    return mask;
}

fn filmic(color: vec3<f32>) -> vec3<f32> {
    let peak = max(max(color.r, color.g), color.b);
    let mapped = min((peak * (2.51 * peak + 0.03)) /
        max(peak * (2.43 * peak + 0.59) + 0.14, 0.00001), 1.0);
    return color * (mapped / max(peak, 0.00001));
}

fn rotate_euler(p: vec3<f32>, rot: vec3<f32>) -> vec3<f32> {
    let cx = cos(rot.x);
    let sx = sin(rot.x);
    let cy = cos(rot.y);
    let sy = sin(rot.y);
    let cz = cos(rot.z);
    let sz = sin(rot.z);
    var v = vec3<f32>(p.x, p.y * cx - p.z * sx, p.y * sx + p.z * cx);
    v = vec3<f32>(v.x * cy + v.z * sy, v.y, v.z * cy - v.x * sy);
    v = vec3<f32>(v.x * cz - v.y * sz, v.x * sz + v.y * cz, v.z);
    return v;
}

fn tint_rgb(tint: u32) -> vec3<f32> {
    return vec3<f32>(
        f32(tint & 0xffu) / 255.0,
        f32((tint >> 8u) & 0xffu) / 255.0,
        f32((tint >> 16u) & 0xffu) / 255.0,
    );
}

// Octahedral `unorm16x2` normal decode — the exact inverse of the
// CPU-side `pack_normal_octahedral`: biased unorm16 lanes, lower-
// hemisphere unfold over the diagonal edges, then renormalize.
fn unpack_oct_normal(packed: u32) -> vec3<f32> {
    let ex = f32(packed & 0xffffu) * (2.0 / 65535.0) - 1.0;
    let ey = f32(packed >> 16u) * (2.0 / 65535.0) - 1.0;
    var n = vec3<f32>(ex, ey, 1.0 - abs(ex) - abs(ey));
    let t = max(-n.z, 0.0);
    n.x += select(t, -t, n.x >= 0.0);
    n.y += select(t, -t, n.y >= 0.0);
    return normalize(n);
}

// `unorm8x3` albedo bytes — the vertex shader's twin of the CPU-side
// `pack_albedo_unorm8x4` (byte 3 is the material-flags byte: bit 0 is
// MAT_FLAG_CLOUD and switches `lit_fs` into the hybrid cloud optics).
fn unpack_unorm8_albedo(packed: u32) -> vec3<f32> {
    return vec3<f32>(
        f32(packed & 0xffu) / 255.0,
        f32((packed >> 8u) & 0xffu) / 255.0,
        f32((packed >> 16u) & 0xffu) / 255.0,
    );
}

fn lit_emit(
    clip: vec4<f32>,
    world: vec3<f32>,
    normal: vec3<f32>,
    albedo: vec3<f32>,
    light_add: f32,
    uv: vec2<f32>,
    atlas_index: u32,
    alpha: f32,
    mat_flags: u32,
) -> LitFragment {
    var out: LitFragment;
    out.clip = clip;
    out.world = world;
    out.normal = normal;
    out.albedo = albedo;
    out.light_add = light_add;
    out.uv = uv;
    out.atlas_index = atlas_index;
    out.alpha = alpha;
    out.mat_flags = mat_flags;
    return out;
}

@vertex
fn lit_vs(in: LitVertex) -> LitFragment {
    let world = (draw.model * vec4<f32>(in.position, 1.0)).xyz;
    let nrm = normalize((draw.model * vec4<f32>(unpack_oct_normal(in.packed_normal), 0.0)).xyz);
    return lit_emit(
        draw.mvp * vec4<f32>(in.position, 1.0),
        world,
        nrm,
        unpack_unorm8_albedo(in.packed_albedo),
        0.0,
        in.uv,
        in.atlas_index,
        1.0,
        (in.packed_albedo >> 24u) & 0xffu,
    );
}

@vertex
fn inst_vs(in: LitVertex, inst: PartInstance) -> LitFragment {
    let local = rotate_euler(in.position * inst.pos_scale.w, inst.rot.xyz);
    let nrm = rotate_euler(unpack_oct_normal(in.packed_normal), inst.rot.xyz);
    let base = inst.pos_scale.xyz + local;
    let world = (draw.model * vec4<f32>(base, 1.0)).xyz;
    let n_world = normalize((draw.model * vec4<f32>(nrm, 0.0)).xyz);
    let light = f32(max(inst.light_tint.x & 0xfu, (inst.light_tint.x >> 4u) & 0xfu))
        / 15.0;
    let albedo = unpack_unorm8_albedo(in.packed_albedo) * tint_rgb(inst.light_tint.y);
    return lit_emit(
        draw.mvp * vec4<f32>(base, 1.0),
        world,
        n_world,
        albedo,
        max(light, 0.25),
        in.uv,
        in.atlas_index,
        1.0,
        (in.packed_albedo >> 24u) & 0xffu,
    );
}

@vertex
fn billboard_vs(quad: BillboardQuadVertex, inst: BillboardInstance) -> LitFragment {
    let c = quad.corner * vec2<f32>(0.5) + vec2<f32>(0.5);
    let base = inst.center + quad.corner.x * inst.axis_u + quad.corner.y * inst.axis_v;
    let world = (draw.model * vec4<f32>(base, 1.0)).xyz;
    let n = cross(inst.axis_u, inst.axis_v);
    let nrm = normalize((draw.model * vec4<f32>(n, 0.0)).xyz);
    return lit_emit(
        draw.mvp * vec4<f32>(base, 1.0),
        world,
        nrm,
        inst.color.rgb,
        0.0,
        inst.uv_origin + c.x * inst.uv_du + c.y * inst.uv_dv,
        0xFFFFFFFFu,
        inst.color.a,
        0u,
    );
}

// Bit 0 of the packed_albedo flags byte — MAT_FLAG_CLOUD on the CPU
// side. Cloud-tagged fragments take the analytic hybrid optics branch
// in `lit_fs`; every other material keeps the plain lit path.
const MAT_FLAG_CLOUD_BIT: u32 = 1u;

@fragment
fn lit_fs(in: LitFragment) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let surface = in.albedo * atlas_sample(in.atlas_index, in.uv);
    var light = env.ambient.rgb * mix(0.55, 1.0, n.y * 0.5 + 0.5)
        * env.ambient.w;
    light += vec3<f32>(env.sun.w) * max(dot(n, normalize(env.sun.xyz)), 0.0);
    light += vec3<f32>(in.light_add);
    // Per-sub-world world light: `params2.z` bitcasts to the packed word
    // (block nibble | sky nibble << 4 | enable bit 8) the CPU writes for
    // `SubWorldRenderEntry` draws — `0` keeps the legacy env-only
    // shading. The vanilla `l / (3 - 2l)` brightness ramp scales
    // ambient+sun; the 64 dynamic lights still accumulate on top below.
    let wlight = bitcast<u32>(draw.params2.z);
    if (wlight & 0x100u) != 0u {
        let wb = f32(wlight & 0xfu) / 15.0;
        let ws = f32((wlight >> 4u) & 0xfu) / 15.0;
        let wday = clamp(env.sun.y * 2.0 + 0.2, 0.0, 1.0);
        let wblock = wb / (3.0 - 2.0 * wb);
        let wsky = ws / (3.0 - 2.0 * ws) * wday;
        light *= clamp(max(wblock, wsky), 0.06, 1.0);
    }
    var color = surface * light;
    var light_mask = draw_light_mask(min(dyn_lights.light_count.x, 64u));
    loop {
        let i = light_mask_take(&light_mask);
        if (i >= 64u) {
            break;
        }
        let pl = dyn_lights.lights[i];
        let delta = pl.position_radius.xyz - in.world;
        let radius = max(pl.position_radius.w, 0.01);
        let dist_sq = dot(delta, delta);
        if (dist_sq >= radius * radius) {
            continue;
        }
        let dist = sqrt(max(dist_sq, 0.0001));
        let t = max(1.0 - dist * (1.0 / radius), 0.0);
        let falloff = t * t;
        let diffuse = max(dot(n, delta / dist), 0.0);
        color += surface * pl.color_intensity.xyz * pl.color_intensity.w
            * diffuse * falloff;
    }
    color = mix(color, color * env.depth_tint.rgb, env.depth_tint.w);
    // Cubic x realistic hybrid cloud optics: crisp voxel silhouettes
    // shaded by analytic light transport — a Henyey-Greenstein
    // forward-scatter silver lining, a crown-to-belly vertical gradient,
    // an ultra-subtle living wisp shimmer and a seamless radial horizon
    // dissolve. Zero raymarching, zero noise textures; `frame._pad.x`
    // carries world seconds for the shimmer.
    var cloud_alpha = 1.0;
    if (in.mat_flags & MAT_FLAG_CLOUD_BIT) != 0u {
        let to_frag = in.world - frame.camera_pos;
        let view_dir = to_frag / max(length(to_frag), 0.001);
        let sun_dir = normalize(env.sun.xyz);
        let cos_theta = clamp(dot(view_dir, sun_dir), -1.0, 1.0);
        // HG phase (g = 0.56) renormalized so the forward peak is 1.0 —
        // `silver` stays a bounded multiplier for any sun angle.
        let g = 0.56;
        let hg_den = max(1.0 + g * g - 2.0 * g * cos_theta, 0.0001);
        let hg = (1.0 - g * g) / (hg_den * sqrt(hg_den));
        let hg_peak = (1.0 + g) / ((1.0 - g) * (1.0 - g));
        let silver = clamp(hg / hg_peak, 0.0, 1.0);
        let edge_glow = pow(1.0 - abs(dot(view_dir, n)), 2.0);
        let dusk = 1.0 - smoothstep(0.0, 0.45, abs(sun_dir.y));
        let warm = vec3<f32>(1.0, 0.62, 0.38);
        color += in.albedo
            * mix(vec3<f32>(1.0), warm, dusk * 0.8)
            * silver * (0.35 + 0.65 * edge_glow) * env.sun.w * 0.30;
        // Crown-to-belly shading: tops lean sunlit-sky, undersides pick
        // up ambient plus a warm low-sun under-glow.
        let crown = clamp(n.y * 0.5 + 0.5, 0.0, 1.0);
        color += in.albedo * env.ambient.rgb * crown * 0.08;
        color += in.albedo * warm * (1.0 - crown) * dusk * 0.18;
        // Wispy shimmer: two slow trig waves over world xz — ±3.5%
        // luminance and a soft alpha breath, calm not dizzying.
        let t = frame._pad.x;
        let wisp = sin(in.world.x * 0.061 + t * 0.55)
            + sin(in.world.z * 0.047 - t * 0.41);
        color *= 1.0 + wisp * 0.0175;
        cloud_alpha = 1.0 + wisp * 0.0125;
        // Radial horizon dissolve melts all decks into the sky/fog
        // before the fixed cloud window's hard edge.
        let dist_xz = length(in.world.xz - frame.camera_pos.xz);
        cloud_alpha *= 1.0 - smoothstep(60.0, 96.0, dist_xz);
    }
    let dist = distance(in.world, frame.camera_pos);
    let linear = clamp(
        (dist - frame.fog_start) / max(frame.fog_end - frame.fog_start, 0.001),
        0.0,
        1.0,
    );
    let expo = 1.0 - exp(-dist * env.atmo.x);
    color = mix(color, frame.fog_color, clamp(max(linear, expo), 0.0, 1.0));
    let fade = clamp(draw.params.x, 0.0, 1.0);
    color = mix(frame.fog_color, color, fade);
    return vec4<f32>(filmic(color), draw.params.y * fade * in.alpha * cloud_alpha);
}
";

/// `enable f16;` replacement for `COMPOSITE_LIT_WGSL`'s `lit_fs` — the
/// same half-precision contract `TERRAIN_FRAGMENT_F16_WGSL` applies to
/// the composed-terrain module: the 64 point-light accumulate
/// (the per-pixel ALU hot path) runs on `vec3<f16>` accumulators while
/// light deltas are computed in `f32` and clamped into finite `f16`
/// range, so an extreme coordinate can never feed `inf` into the
/// half-precision dot products. Vertex inputs, varyings, ambient/sun
/// terms, fog mixes and the filmic shoulder stay `f32`; the
/// `light += vec3<f32>(env.sun.w) * ...` line is kept byte-identical so
/// the shadow-sampling splice in
/// [`crate::renderer::composed_shadow_wgsl`] matches both variants.
const LIT_FRAGMENT_F16_WGSL: &str = r#"
@fragment
fn lit_fs(in: LitFragment) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let surface = in.albedo * atlas_sample(in.atlas_index, in.uv);
    var light = env.ambient.rgb * mix(0.55, 1.0, n.y * 0.5 + 0.5)
        * env.ambient.w;
    light += vec3<f32>(env.sun.w) * max(dot(n, normalize(env.sun.xyz)), 0.0);
    light += vec3<f32>(in.light_add);
    // Same per-sub-world world-light decode as the f32 `lit_fs` —
    // `params2.z` packs (block | sky << 4 | enable bit 8), `0` disables.
    let wlight = bitcast<u32>(draw.params2.z);
    if (wlight & 0x100u) != 0u {
        let wb = f32(wlight & 0xfu) / 15.0;
        let ws = f32((wlight >> 4u) & 0xfu) / 15.0;
        let wday = clamp(env.sun.y * 2.0 + 0.2, 0.0, 1.0);
        let wblock = wb / (3.0 - 2.0 * wb);
        let wsky = ws / (3.0 - 2.0 * ws) * wday;
        light *= clamp(max(wblock, wsky), 0.06, 1.0);
    }
    var color = surface * light;
    var light_mask = draw_light_mask(min(dyn_lights.light_count.x, 64u));
    var light16 = vec3<f16>(0.0);
    let n16 = vec3<f16>(n);
    let surface16 = vec3<f16>(surface);
    loop {
        let i = light_mask_take(&light_mask);
        if (i >= 64u) {
            break;
        }
        let pl = dyn_lights.lights[i];
        let d16 = vec3<f16>(clamp(
            pl.position_radius.xyz - in.world,
            vec3<f32>(-32000.0),
            vec3<f32>(32000.0),
        ));
        let radius = f16(max(pl.position_radius.w, 0.01));
        let dist_sq = dot(d16, d16);
        // Outside the bounding sphere the falloff is exactly zero —
        // skip before paying the half-precision sqrt.
        if (dist_sq >= radius * radius) {
            continue;
        }
        let dist = sqrt(max(dist_sq, f16(0.0001)));
        let t = max(f16(1.0) - dist * (f16(1.0) / radius), f16(0.0));
        let falloff = t * t;
        let diffuse = max(dot(n16, d16 / dist), f16(0.0));
        light16 += surface16 * vec3<f16>(pl.color_intensity.xyz)
            * f16(pl.color_intensity.w) * diffuse * falloff;
    }
    color += vec3<f32>(light16);
    color = mix(color, color * env.depth_tint.rgb, env.depth_tint.w);
    // Cubic x realistic hybrid cloud optics — the f32-twin of the
    // baseline `lit_fs` branch (MAT_FLAG_CLOUD): HG silver lining,
    // crown-to-belly gradient, wisp shimmer, radial horizon dissolve.
    var cloud_alpha = 1.0;
    if (in.mat_flags & MAT_FLAG_CLOUD_BIT) != 0u {
        let to_frag = in.world - frame.camera_pos;
        let view_dir = to_frag / max(length(to_frag), 0.001);
        let sun_dir = normalize(env.sun.xyz);
        let cos_theta = clamp(dot(view_dir, sun_dir), -1.0, 1.0);
        let g = 0.56;
        let hg_den = max(1.0 + g * g - 2.0 * g * cos_theta, 0.0001);
        let hg = (1.0 - g * g) / (hg_den * sqrt(hg_den));
        let hg_peak = (1.0 + g) / ((1.0 - g) * (1.0 - g));
        let silver = clamp(hg / hg_peak, 0.0, 1.0);
        let edge_glow = pow(1.0 - abs(dot(view_dir, n)), 2.0);
        let dusk = 1.0 - smoothstep(0.0, 0.45, abs(sun_dir.y));
        let warm = vec3<f32>(1.0, 0.62, 0.38);
        color += in.albedo
            * mix(vec3<f32>(1.0), warm, dusk * 0.8)
            * silver * (0.35 + 0.65 * edge_glow) * env.sun.w * 0.30;
        let crown = clamp(n.y * 0.5 + 0.5, 0.0, 1.0);
        color += in.albedo * env.ambient.rgb * crown * 0.08;
        color += in.albedo * warm * (1.0 - crown) * dusk * 0.18;
        let t = frame._pad.x;
        let wisp = sin(in.world.x * 0.061 + t * 0.55)
            + sin(in.world.z * 0.047 - t * 0.41);
        color *= 1.0 + wisp * 0.0175;
        cloud_alpha = 1.0 + wisp * 0.0125;
        let dist_xz = length(in.world.xz - frame.camera_pos.xz);
        cloud_alpha *= 1.0 - smoothstep(60.0, 96.0, dist_xz);
    }
    let dist = distance(in.world, frame.camera_pos);
    let linear = clamp(
        (dist - frame.fog_start) / max(frame.fog_end - frame.fog_start, 0.001),
        0.0,
        1.0,
    );
    let expo = 1.0 - exp(-dist * env.atmo.x);
    color = mix(color, frame.fog_color, clamp(max(linear, expo), 0.0, 1.0));
    let fade = clamp(draw.params.x, 0.0, 1.0);
    color = mix(frame.fog_color, color, fade);
    return vec4<f32>(filmic(color), draw.params.y * fade * in.alpha * cloud_alpha);
}
"#;

/// `COMPOSITE_LIT_WGSL` with `enable f16;` and the half-precision `lit_fs`
/// — spliced at the fragment entry so the shared prefix (uniforms, the
/// packed-vertex decoders, all three vertex stages, helpers) is inherited
/// verbatim and stays strictly `f32`.
pub fn lit_wgsl_f16() -> String {
    const ANCHOR: &str = "@fragment\nfn lit_fs(in: LitFragment)";
    let split = COMPOSITE_LIT_WGSL
        .find(ANCHOR)
        .expect("COMPOSITE_LIT_WGSL fragment anchor missing");
    format!(
        "enable f16;\n{}{}",
        &COMPOSITE_LIT_WGSL[..split],
        LIT_FRAGMENT_F16_WGSL
    )
}

/// Capability-gated sub-world lit WGSL, the same gate
/// [`crate::renderer::terrain_wgsl`] applies to the terrain module: the
/// `enable f16;` fragment variant when the device granted `SHADER_F16`
/// ([`crate::renderer::AdapterReport::shader_f16`]), the baseline `f32`
/// module everywhere else. Both branches feed the same pipeline
/// signature — vertex buffers, varyings and outputs are identical — so
/// the fallback is a pure module swap.
pub fn lit_wgsl(shader_f16: bool) -> Cow<'static, str> {
    if shader_f16 {
        Cow::Owned(lit_wgsl_f16())
    } else {
        Cow::Borrowed(COMPOSITE_LIT_WGSL)
    }
}

#[cfg(test)]
mod voxel_hull_tests {
    use super::*;

    const DECK: SubWorldCell = SubWorldCell {
        albedo: [0.45, 0.32, 0.18],
        material_flags: 0,
        atlas_index: 7,
    };
    const CABIN: SubWorldCell = SubWorldCell {
        albedo: [0.7, 0.68, 0.6],
        material_flags: 2,
        atlas_index: 11,
    };

    /// Naive per-cell exposed-face counts, one entry per
    /// `box_face_corners` face (`+X, -X, +Y, -Y, +Z, -Z`).
    fn naive_exposed_faces(
        dims: [usize; 3],
        mut cell_at: impl FnMut(usize, usize, usize) -> Option<SubWorldCell>,
    ) -> [usize; 6] {
        let mut counts = [0usize; 6];
        for z in 0..dims[2] {
            for y in 0..dims[1] {
                for x in 0..dims[0] {
                    if cell_at(x, y, z).is_none() {
                        continue;
                    }
                    for axis in 0..3 {
                        for step in [-1i32, 1] {
                            let mut n = [x as i32, y as i32, z as i32];
                            n[axis] += step;
                            let exposed = !(0..dims[axis] as i32).contains(&n[axis])
                                || cell_at(n[0] as usize, n[1] as usize, n[2] as usize)
                                    .is_none();
                            if exposed {
                                counts[axis * 2 + usize::from(step < 0)] += 1;
                            }
                        }
                    }
                }
            }
        }
        counts
    }

    /// Planar-quad area + normal classification for one emitted quad.
    fn quad_area_and_face(quad: &[TerrainVertex]) -> (f32, usize) {
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for vertex in quad {
            for (axis, &p) in vertex.position.iter().enumerate() {
                min[axis] = min[axis].min(p);
                max[axis] = max[axis].max(p);
            }
        }
        let normal = quad[0].normal();
        let (axis, sign) = (0..3)
            .map(|axis| (axis, normal[axis]))
            .find(|(_, n)| n.abs() > 0.5)
            .expect("quad has an axis normal");
        assert!(quad.iter().all(|v| v.normal() == normal));
        let mut area = 1.0f32;
        for a in 0..3 {
            if a != axis {
                area *= max[a] - min[a];
            }
        }
        (area, axis * 2 + usize::from(sign < 0.0))
    }

    /// Per-face area totals of the merged mesh, keyed like
    /// [`naive_exposed_faces`].
    fn merged_areas(mesh: &SubWorldMesh) -> [f32; 6] {
        assert_eq!(mesh.vertices.len() % 4, 0);
        assert_eq!(mesh.indices.len() / 6, mesh.vertices.len() / 4);
        let mut areas = [0.0f32; 6];
        for quad in mesh.vertices.as_chunks::<4>().0 {
            let (area, face) = quad_area_and_face(quad);
            areas[face] += area;
        }
        areas
    }

    fn grid(
        dims: [usize; 3],
        filled: impl Fn(usize, usize, usize) -> bool,
    ) -> Vec<Option<SubWorldCell>> {
        let mut cells = vec![None; dims[0] * dims[1] * dims[2]];
        for z in 0..dims[2] {
            for y in 0..dims[1] {
                for x in 0..dims[0] {
                    if filled(x, y, z) {
                        cells[(z * dims[1] + y) * dims[0] + x] = Some(DECK);
                    }
                }
            }
        }
        cells
    }

    fn airship_hull(dims: [usize; 3]) -> Vec<Option<SubWorldCell>> {
        let mut cells = vec![None; dims[0] * dims[1] * dims[2]];
        let mut put = |x: usize, y: usize, z: usize, cell: SubWorldCell| {
            cells[(z * dims[1] + y) * dims[0] + x] = Some(cell);
        };
        // Keel, flared lower hull, two full main decks, tapered upper
        // deck, a cabin block and a mast — a multi-deck airship hull.
        for z in 3..6 {
            for x in 4..8 {
                put(x, 0, z, DECK);
            }
        }
        for z in 2..7 {
            for x in 2..10 {
                put(x, 1, z, DECK);
            }
        }
        for y in 2..=3 {
            for z in 1..8 {
                for x in 0..12 {
                    put(x, y, z, DECK);
                }
            }
        }
        for z in 2..7 {
            for x in 1..11 {
                put(x, 4, z, DECK);
            }
        }
        for z in 3..6 {
            for x in 5..8 {
                put(x, 5, z, CABIN);
            }
        }
        put(6, 6, 4, CABIN);
        cells
    }

    #[test]
    fn subworld_single_voxel_emits_six_unit_faces() {
        let dims = [1, 1, 1];
        let cells = grid(dims, |_, _, _| true);
        let mesh = greedy_voxel_hull_mesh(dims, |x, y, z| {
            cells[(z * dims[1] + y) * dims[0] + x]
        });
        assert_eq!(mesh.vertices.len(), 24);
        assert_eq!(mesh.indices.len(), 36);
        let areas = merged_areas(&mesh);
        assert_eq!(areas, [1.0; 6]);
        // Untextured cells keep `u32::MAX` and the exact material byte.
        assert!(mesh
            .vertices
            .iter()
            .all(|v| v.atlas_index == 7 && v.material_flags() == 0));
    }

    #[test]
    fn subworld_empty_and_degenerate_grids_emit_nothing() {
        let mesh = greedy_voxel_hull_mesh([4, 4, 4], |_, _, _| None);
        assert!(mesh.vertices.is_empty() && mesh.indices.is_empty());
        for dims in [[0, 4, 4], [4, 0, 4], [4, 4, 0]] {
            let mesh = greedy_voxel_hull_mesh(dims, |_, _, _| Some(DECK));
            assert!(mesh.vertices.is_empty() && mesh.indices.is_empty());
        }
    }

    #[test]
    fn subworld_solid_cube_collapses_to_six_quads() {
        let dims = [4, 4, 4];
        let cells = grid(dims, |_, _, _| true);
        let mesh = greedy_voxel_hull_mesh(dims, |x, y, z| {
            cells[(z * dims[1] + y) * dims[0] + x]
        });
        assert_eq!(mesh.vertices.len(), 24);
        assert_eq!(merged_areas(&mesh), [16.0; 6]);
        assert_eq!(naive_exposed_faces(dims, |x, y, z| {
            cells[(z * dims[1] + y) * dims[0] + x]
        }), [16; 6]);
    }

    #[test]
    fn subworld_airship_hull_reduces_quads_and_preserves_area() {
        let dims = [12, 7, 9];
        let cells = airship_hull(dims);
        let naive = naive_exposed_faces(dims, |x, y, z| {
            cells[(z * dims[1] + y) * dims[0] + x]
        });
        let naive_total: usize = naive.iter().sum();
        let mesh = greedy_voxel_hull_mesh(dims, |x, y, z| {
            cells[(z * dims[1] + y) * dims[0] + x]
        });
        let merged_quads = mesh.vertices.len() / 4;
        assert!(
            merged_quads * 4 <= naive_total,
            "expected >= 75% quad reduction: {merged_quads} vs naive {naive_total}"
        );
        // Exact exposed surface area, per direction and overall.
        let areas = merged_areas(&mesh);
        for face in 0..6 {
            assert_eq!(areas[face], naive[face] as f32, "face {face} area drifted");
        }
        // Two materials survive on the merged hull: cabin quads carry the
        // cabin flags/atlas, deck quads the deck ones.
        let cabin_quads = mesh
            .vertices
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|q| q[0].material_flags() == 2)
            .count();
        assert!(cabin_quads >= 6);
        assert!(mesh
            .vertices
            .iter()
            .all(|v| v.atlas_index == if v.material_flags() == 2 { 11 } else { 7 }));
    }

    #[test]
    fn subworld_material_boundaries_stop_merging() {
        // A 4x2 deck whose right half is a different material: the top
        // must split into exactly two 2x2 quads, never one 4x2.
        let dims = [4, 1, 2];
        let mut cells = [None; 8];
        for z in 0..2 {
            for x in 0..4 {
                cells[z * 4 + x] = Some(if x < 2 { DECK } else { CABIN });
            }
        }
        let mesh = greedy_voxel_hull_mesh(dims, |x, y, z| {
            cells[(z * dims[1] + y) * dims[0] + x]
        });
        let tops: Vec<_> = mesh
            .vertices
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|q| q[0].normal()[1] > 0.5)
            .collect();
        assert_eq!(tops.len(), 2);
        for quad in tops {
            let (area, _) = quad_area_and_face(quad);
            assert_eq!(area, 4.0);
            let want_cabin = quad.iter().all(|v| v.position[0] >= 2.0);
            assert_eq!(
                quad[0].material_flags(),
                if want_cabin { 2 } else { 0 },
                "material flags leaked across the boundary"
            );
        }
    }

    #[test]
    fn subworld_contraption_l_shape_keeps_exact_area() {
        // Stepped contraption platform: two offset slabs sharing a side.
        let dims = [6, 3, 4];
        let cells = grid(dims, |x, y, z| {
            (x < 4 && y == 0 && z < 4) || (x >= 2 && y == 1 && z < 2)
        });
        let naive = naive_exposed_faces(dims, |x, y, z| {
            cells[(z * dims[1] + y) * dims[0] + x]
        });
        let mesh = greedy_voxel_hull_mesh(dims, |x, y, z| {
            cells[(z * dims[1] + y) * dims[0] + x]
        });
        let areas = merged_areas(&mesh);
        for face in 0..6 {
            assert_eq!(areas[face], naive[face] as f32, "face {face} area drifted");
        }
    }
}

