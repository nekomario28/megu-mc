// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
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

use std::collections::BTreeMap;

use bytemuck::{Pod, Zeroable};
use megu_client_mesh::builtin_shadow_wgsl::VfxLightUniforms;
use megu_client_mesh::BerInstanceData;

use crate::{mat4_mul, Mat4};

// ---------------------------------------------------------------------------
// Shared lit-geometry vertex (`position`/`normal`/`albedo`/`uv`)
// ---------------------------------------------------------------------------

/// World-space lit vertex for sub-worlds, voxel clouds, and cubic
/// planets/suns. 48 bytes; bound as `@location(0..2, 7..8)` inputs in
/// [`COMPOSITE_LIT_WGSL`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct TerrainVertex {
    /// Position in the draw's model space (blocks).
    pub position: [f32; 3],
    /// Unit outward normal in model space.
    pub normal: [f32; 3],
    /// Linear-ish albedo `0.0..=1.0` per channel.
    pub albedo: [f32; 3],
    /// Original unwrapped sprite UV, including fractional texel geometry.
    pub uv: [f32; 2],
    /// Index into the uploaded atlas rectangle table. `u32::MAX` is untextured.
    pub atlas_index: u32,
}

const LIT_VERTEX_ATTRS: [wgpu::VertexAttribute; 5] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 12,
        shader_location: 1,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 24,
        shader_location: 2,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 36,
        shader_location: 7,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 44,
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

    /// Column-major `mat4x4<f32>` model matrix:
    /// `translate(position) * rotate(quaternion) * scale(scale)`.
    pub fn model_matrix(&self) -> Mat4 {
        let [x, y, z, w] = self.normalized_quaternion();
        let s = if self.scale.is_finite() && self.scale != 0.0 {
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
    /// Model-space lit vertices.
    pub vertices: Vec<TerrainVertex>,
    /// `u32` index buffer into `vertices`.
    pub indices: Vec<u32>,
    /// Global alpha multiplier (`1.0` opaque; clouds use `< 1`).
    pub alpha: f32,
    /// Set when `vertices`/`indices` changed since the last GPU sync.
    pub dirty: bool,
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
            vertices,
            indices,
            alpha: 1.0,
            dirty: true,
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
}

impl SubWorldRenderBank {
    /// Empty bank.
    pub fn new() -> Self {
        Self::default()
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
                entry.transform = transform;
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

// ---------------------------------------------------------------------------
// GPU-side cache: one buffer pair per sub-world, one stream per batch
// ---------------------------------------------------------------------------

/// GPU-resident copy of one [`SubWorldRenderEntry`].
pub struct GpuSubWorldEntry {
    /// Transform mirrored from the bank.
    pub transform: SubWorldTransform,
    /// Global alpha multiplier.
    pub alpha: f32,
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
                        transform: entry.transform,
                        alpha: entry.alpha,
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
    /// Reserved.
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
            vertices.push(TerrainVertex {
                position,
                normal,
                albedo,
                uv: [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]][corner],
                atlas_index: u32::MAX,
            });
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
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
    let count = min(dyn_lights.light_count.x, 64u);
    for (var i = 0u; i < count; i = i + 1u) {
        let pl = dyn_lights.lights[i];
        let delta = pl.position_radius.xyz - in.world;
        let radius = max(pl.position_radius.w, 0.01);
        let dist_sq = max(dot(delta, delta), 0.0001);
        let dist = sqrt(dist_sq);
        let falloff = pow(max(1.0 - dist / radius, 0.0), 2.0);
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
    let count = min(dyn_lights.light_count.x, 64u);
    for (var i = 0u; i < count; i = i + 1u) {
        let pl = dyn_lights.lights[i];
        let delta = pl.position_radius.xyz - in.world;
        let radius = max(pl.position_radius.w, 0.01);
        let dist_sq = max(dot(delta, delta), 0.0001);
        let dist = sqrt(dist_sq);
        let falloff = pow(max(1.0 - dist / radius, 0.0), 2.0);
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
    @location(1) normal: vec3<f32>,
    @location(2) albedo: vec3<f32>,
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

fn lit_emit(
    clip: vec4<f32>,
    world: vec3<f32>,
    normal: vec3<f32>,
    albedo: vec3<f32>,
    light_add: f32,
    uv: vec2<f32>,
    atlas_index: u32,
) -> LitFragment {
    var out: LitFragment;
    out.clip = clip;
    out.world = world;
    out.normal = normal;
    out.albedo = albedo;
    out.light_add = light_add;
    out.uv = uv;
    out.atlas_index = atlas_index;
    return out;
}

@vertex
fn lit_vs(in: LitVertex) -> LitFragment {
    let world = (draw.model * vec4<f32>(in.position, 1.0)).xyz;
    let nrm = normalize((draw.model * vec4<f32>(in.normal, 0.0)).xyz);
    return lit_emit(
        draw.mvp * vec4<f32>(in.position, 1.0),
        world,
        nrm,
        in.albedo,
        0.0,
        in.uv,
        in.atlas_index,
    );
}

@vertex
fn inst_vs(in: LitVertex, inst: PartInstance) -> LitFragment {
    let local = rotate_euler(in.position * inst.pos_scale.w, inst.rot.xyz);
    let nrm = rotate_euler(in.normal, inst.rot.xyz);
    let base = inst.pos_scale.xyz + local;
    let world = (draw.model * vec4<f32>(base, 1.0)).xyz;
    let n_world = normalize((draw.model * vec4<f32>(nrm, 0.0)).xyz);
    let light = f32(max(inst.light_tint.x & 0xfu, (inst.light_tint.x >> 4u) & 0xfu))
        / 15.0;
    let albedo = in.albedo * tint_rgb(inst.light_tint.y);
    return lit_emit(
        draw.mvp * vec4<f32>(base, 1.0),
        world,
        n_world,
        albedo,
        max(light, 0.25),
        in.uv,
        in.atlas_index,
    );
}

@fragment
fn lit_fs(in: LitFragment) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let surface = in.albedo * atlas_sample(in.atlas_index, in.uv);
    var light = env.ambient.rgb * mix(0.55, 1.0, n.y * 0.5 + 0.5)
        * env.ambient.w;
    light += vec3<f32>(env.sun.w) * max(dot(n, normalize(env.sun.xyz)), 0.0);
    light += vec3<f32>(in.light_add);
    var color = surface * light;
    let count = min(dyn_lights.light_count.x, 64u);
    for (var i = 0u; i < count; i = i + 1u) {
        let pl = dyn_lights.lights[i];
        let delta = pl.position_radius.xyz - in.world;
        let radius = max(pl.position_radius.w, 0.01);
        let dist_sq = max(dot(delta, delta), 0.0001);
        let dist = sqrt(dist_sq);
        let falloff = pow(max(1.0 - dist / radius, 0.0), 2.0);
        let diffuse = max(dot(n, delta / dist), 0.0);
        color += surface * pl.color_intensity.xyz * pl.color_intensity.w
            * diffuse * falloff;
    }
    color = mix(color, color * env.depth_tint.rgb, env.depth_tint.w);
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
    return vec4<f32>(filmic(color), draw.params.y * fade);
}
";
