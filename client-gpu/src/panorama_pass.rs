// SPDX-License-Identifier: LGPL-3.0-or-later
//! Standalone GPU pass for Minecraft Java 1.21.1's rotating title panorama.
//!
//! The pass owns one cubemap pipeline and a tiny uniform buffer. Uploads are
//! allowed to allocate (they happen only when title assets arrive); the
//! per-frame [`TitlePanoramaPass::encode`] path writes one stack uniform and
//! records one fullscreen-triangle draw without constructing any CPU-side
//! container.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};

use crate::vanilla_gui_sprites::{VanillaGuiAssets, VanillaGuiSpriteId};
use crate::vanilla_official_extras::VanillaOfficialExtras;

/// wgpu cubemap layer index for each Minecraft `panorama_{0..5}.png` face.
///
/// wgpu orders cube layers `+X, -X, +Y, -Y, +Z, -Z`. Minecraft names the
/// same physical faces `front/-Z, right/+X, back/+Z, left/-X, up/+Y,
/// down/-Y`, hence the permutation below.
pub const MINECRAFT_PANORAMA_TO_WGPU_LAYER: [usize; 6] = [5, 0, 4, 1, 2, 3];

/// Inverse of [`MINECRAFT_PANORAMA_TO_WGPU_LAYER`], indexed by wgpu layer.
pub const WGPU_LAYER_TO_MINECRAFT_PANORAMA: [usize; 6] = [1, 3, 4, 5, 2, 0];

/// Vertical field of view used by vanilla's `CubeMap.render` perspective.
pub const TITLE_PANORAMA_VERTICAL_FOV_DEG: f32 = 85.0;

/// Return the `(horizontal, vertical)` texel flip needed before uploading a
/// Minecraft panorama face into wgpu's cube-layer texel convention.
///
/// Vanilla stores each face exactly as an outward-looking camera image: side
/// faces have screen-up along `+Y`; the up face has screen-down toward `-Z`;
/// the down face has screen-down toward `+Z`. wgpu's cube sampler addresses
/// layers using the outward-face `S/T` axes (`+X` is `S=-Z,T=-Y`, and so on).
/// That differs by a horizontal mirror on every side face and a vertical
/// mirror on the two pole faces. Applying these flips once at upload time
/// keeps the runtime shader simple and the shared edges continuous.
pub const fn minecraft_panorama_face_flip(face_index: usize) -> (bool, bool) {
    match face_index {
        0..=3 => (true, false),
        4 | 5 => (false, true),
        _ => (false, false),
    }
}

/// Validate one six-face RGBA8 cubemap payload without needing a GPU.
pub fn validate_panorama_face_data(
    faces: [&[u8]; 6],
    face_size: u32,
    max_texture_dimension: u32,
) -> Result<(), String> {
    if face_size == 0 {
        return Err("panorama face size must be nonzero".to_owned());
    }
    if face_size > max_texture_dimension {
        return Err(format!(
            "panorama face size {face_size} exceeds device limit {max_texture_dimension}"
        ));
    }
    let expected = u64::from(face_size)
        .checked_mul(u64::from(face_size))
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| "panorama face dimensions overflow".to_owned())?;
    let expected = usize::try_from(expected)
        .map_err(|_| "panorama face byte count exceeds addressable memory".to_owned())?;
    for (index, face) in faces.iter().enumerate() {
        if face.len() != expected {
            return Err(format!(
                "panorama face {index} has {} RGBA bytes, expected {expected} for {face_size}x{face_size}",
                face.len()
            ));
        }
    }
    Ok(())
}

/// Validate one optional RGBA8 overlay image without needing a GPU.
pub fn validate_panorama_overlay_data(
    rgba: &[u8],
    width: u32,
    height: u32,
    max_texture_dimension: u32,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("panorama overlay dimensions must be nonzero".to_owned());
    }
    if width > max_texture_dimension || height > max_texture_dimension {
        return Err(format!(
            "panorama overlay {width}x{height} exceeds device limit {max_texture_dimension}"
        ));
    }
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| "panorama overlay dimensions overflow".to_owned())?;
    let expected = usize::try_from(expected)
        .map_err(|_| "panorama overlay byte count exceeds addressable memory".to_owned())?;
    if rgba.len() != expected {
        return Err(format!(
            "panorama overlay has {} RGBA bytes, expected {expected} for {width}x{height}",
            rgba.len()
        ));
    }
    Ok(())
}

/// Convert one Minecraft camera-oriented face into wgpu's cube-layer texel
/// orientation. This is upload-path work, never part of the frame hot path.
pub fn reorient_minecraft_panorama_face(
    face_rgba: &[u8],
    face_size: u32,
    face_index: usize,
) -> Result<Vec<u8>, String> {
    if face_index >= 6 {
        return Err(format!("panorama face index {face_index} is outside 0..6"));
    }
    if face_size == 0 {
        return Err("panorama face size must be nonzero".to_owned());
    }
    let expected = u64::from(face_size)
        .checked_mul(u64::from(face_size))
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| "panorama face dimensions overflow".to_owned())?;
    if usize::try_from(expected).ok() != Some(face_rgba.len()) {
        return Err(format!(
            "panorama face has {} RGBA bytes, expected {expected} for {face_size}x{face_size}",
            face_rgba.len()
        ));
    }
    let (flip_horizontal, flip_vertical) = minecraft_panorama_face_flip(face_index);
    let mut out = vec![0_u8; face_rgba.len()];
    let size = face_size as usize;
    for y in 0..size {
        let source_y = if flip_vertical { size - 1 - y } else { y };
        for x in 0..size {
            let source_x = if flip_horizontal { size - 1 - x } else { x };
            let src = (source_y * size + source_x) * 4;
            let dst = (y * size + x) * 4;
            out[dst..dst + 4].copy_from_slice(&face_rgba[src..src + 4]);
        }
    }
    Ok(out)
}

/// Extract the optional official `panorama_overlay.png` RGBA payload as the
/// `(rgba, width, height)` tuple accepted by
/// [`TitlePanoramaPass::upload_from_faces`].
pub fn panorama_overlay_from_extras(extras: &VanillaOfficialExtras) -> Option<(&[u8], u32, u32)> {
    let image = extras.gui_sprite_rgba("title/background/panorama_overlay.png")?;
    Some((image.rgba.as_slice(), image.width, image.height))
}

/// Continuous title-panorama camera angles in radians `(yaw, pitch)`.
///
/// The yaw is deliberately unwrapped (a continuous `3 deg/s` progression),
/// while pitch is Vanilla's fixed 10 degree downward title-camera pitch.
/// Non-finite input falls back to the `t = 0` pose rather than contaminating
/// a uniform buffer with NaN.
pub fn vanilla_panorama_angles(elapsed_secs: f32) -> (f32, f32) {
    let t = if elapsed_secs.is_finite() {
        f64::from(elapsed_secs)
    } else {
        0.0
    };
    let yaw_rad = t * 3.0_f64.to_radians();
    (yaw_rad as f32, 10_f32.to_radians())
}

/// WGSL for the one-draw panorama pass. Bindings are intentionally stable:
/// `0` uniform, `1` cubemap, `2` sampler, `3` overlay.
pub const TITLE_PANORAMA_WGSL: &str = r#"
struct PanoramaUniform {
    resolution: vec4<f32>,
    rotation: vec4<f32>,
    params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> panorama: PanoramaUniform;
@group(0) @binding(1) var panorama_cube: texture_cube<f32>;
@group(0) @binding(2) var panorama_sampler: sampler;
@group(0) @binding(3) var panorama_overlay: texture_2d<f32>;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) ray: vec3<f32>,
};

fn rotate_panorama_ray(camera_ray: vec3<f32>) -> vec3<f32> {
    let sin_yaw = panorama.rotation.x;
    let cos_yaw = panorama.rotation.y;
    let sin_pitch = panorama.rotation.z;
    let cos_pitch = panorama.rotation.w;

    let pitched = vec3<f32>(
        camera_ray.x,
        camera_ray.y * cos_pitch - camera_ray.z * sin_pitch,
        camera_ray.y * sin_pitch + camera_ray.z * cos_pitch,
    );
    return vec3<f32>(
        pitched.x * cos_yaw - pitched.z * sin_yaw,
        pitched.y,
        pitched.x * sin_yaw + pitched.z * cos_yaw,
    );
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let ndc = positions[vertex_index];
    let camera_ray = vec3<f32>(
        ndc.x * panorama.resolution.z * panorama.params.x,
        ndc.y * panorama.params.x,
        -1.0,
    );

    var out: VertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.ray = rotate_panorama_ray(camera_ray);
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let ray = normalize(in.ray);
    var up_reference = vec3<f32>(0.0, 1.0, 0.0);
    if abs(ray.y) > 0.98 {
        up_reference = vec3<f32>(1.0, 0.0, 0.0);
    }
    let tangent = normalize(cross(up_reference, ray));
    let bitangent = normalize(cross(ray, tangent));
    let radius = panorama.params.y;

    // Center plus four small tangent-plane taps: the same softening role as
    // vanilla's jittered multi-pass panorama draw, but in one shader pass.
    var color = textureSample(panorama_cube, panorama_sampler, ray).rgb * 0.4;
    color += textureSample(panorama_cube, panorama_sampler, ray + tangent * radius).rgb * 0.15;
    color += textureSample(panorama_cube, panorama_sampler, ray - tangent * radius).rgb * 0.15;
    color += textureSample(panorama_cube, panorama_sampler, ray + bitangent * radius).rgb * 0.15;
    color += textureSample(panorama_cube, panorama_sampler, ray - bitangent * radius).rgb * 0.15;

    let overlay_uv = in.position.xy / max(panorama.resolution.xy, vec2<f32>(1.0));
    var overlay = textureSample(panorama_overlay, panorama_sampler, overlay_uv);
    overlay = vec4<f32>(overlay.rgb, overlay.a * panorama.params.z);
    color = mix(color, overlay.rgb, clamp(overlay.a, 0.0, 1.0));

    let fade = clamp(panorama.resolution.w, 0.0, 1.0);
    return vec4<f32>(color * fade, fade);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct TitlePanoramaUniform {
    resolution: [f32; 4],
    rotation: [f32; 4],
    params: [f32; 4],
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

fn create_cubemap_texture(device: &wgpu::Device, face_size: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("title-panorama-cubemap"),
        size: wgpu::Extent3d {
            width: face_size,
            height: face_size,
            depth_or_array_layers: 6,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn create_overlay_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("title-panorama-overlay"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_rgba_layer(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    layer: u32,
    rgba: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: 0,
                y: 0,
                z: layer,
            },
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

/// One retained title-panorama render pass. Construct once per target format;
/// call the upload methods when vanilla assets become available and
/// [`Self::encode`] once per frame.
pub struct TitlePanoramaPass {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    sampler: wgpu::Sampler,
    bind_group: wgpu::BindGroup,
    _cube_texture: wgpu::Texture,
    _overlay_texture: wgpu::Texture,
    face_size: u32,
    overlay_size: Option<(u32, u32)>,
    real_panorama: bool,
}

impl TitlePanoramaPass {
    /// Create the pass with a complete 1×1 sky-blue cubemap and transparent
    /// overlay already bound, so title rendering never depends on a client
    /// JAR being present.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("title-panorama-bind-group-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::Cube,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("title-panorama-shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(TITLE_PANORAMA_WGSL)),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("title-panorama-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("title-panorama-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("title-panorama-uniform"),
            size: std::mem::size_of::<TitlePanoramaUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("title-panorama-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let cube_texture = create_cubemap_texture(device, 1);
        let sky_blue = [98, 164, 223, 255];
        for layer in 0..6 {
            write_rgba_layer(queue, &cube_texture, 1, 1, layer, &sky_blue);
        }
        let overlay_texture = create_overlay_texture(device, 1, 1);
        write_rgba_layer(queue, &overlay_texture, 1, 1, 0, &[0, 0, 0, 0]);
        let cube_view = cube_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("title-panorama-cubemap-view"),
            dimension: Some(wgpu::TextureViewDimension::Cube),
            ..Default::default()
        });
        let overlay_view = overlay_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("title-panorama-overlay-view"),
            dimension: Some(wgpu::TextureViewDimension::D2),
            ..Default::default()
        });
        let bind_group = Self::create_panorama_bind_group(
            device,
            &bind_group_layout,
            &uniform,
            &cube_view,
            &sampler,
            &overlay_view,
        );

        Self {
            pipeline,
            bind_group_layout,
            uniform,
            sampler,
            bind_group,
            _cube_texture: cube_texture,
            _overlay_texture: overlay_texture,
            face_size: 1,
            overlay_size: None,
            real_panorama: false,
        }
    }

    fn create_panorama_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform: &wgpu::Buffer,
        cube_view: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
        overlay_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("title-panorama-bind-group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(cube_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(overlay_view),
                },
            ],
        })
    }

    /// Upload six tightly packed RGBA8 faces in Minecraft `panorama_0..5`
    /// order. `face_size` is their shared square edge length. Invalid sizes
    /// or byte lengths fail before replacing the currently bound panorama.
    pub fn upload_from_faces(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        faces: [&[u8]; 6],
        face_size: u32,
        overlay: Option<(&[u8], u32, u32)>,
    ) -> Result<(), String> {
        let max_dimension = device.limits().max_texture_dimension_2d;
        validate_panorama_face_data(faces, face_size, max_dimension)?;
        if let Some((rgba, width, height)) = overlay {
            validate_panorama_overlay_data(rgba, width, height, max_dimension)?;
        }

        let cube_texture = create_cubemap_texture(device, face_size);
        for (face_index, face) in faces.iter().enumerate() {
            let oriented = reorient_minecraft_panorama_face(face, face_size, face_index)?;
            let layer = MINECRAFT_PANORAMA_TO_WGPU_LAYER[face_index] as u32;
            write_rgba_layer(queue, &cube_texture, face_size, face_size, layer, &oriented);
        }

        let (overlay_texture, overlay_size) = if let Some((rgba, width, height)) = overlay {
            let texture = create_overlay_texture(device, width, height);
            write_rgba_layer(queue, &texture, width, height, 0, rgba);
            (texture, Some((width, height)))
        } else {
            let texture = create_overlay_texture(device, 1, 1);
            write_rgba_layer(queue, &texture, 1, 1, 0, &[0, 0, 0, 0]);
            (texture, None)
        };
        let cube_view = cube_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("title-panorama-cubemap-view"),
            dimension: Some(wgpu::TextureViewDimension::Cube),
            ..Default::default()
        });
        let overlay_view = overlay_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("title-panorama-overlay-view"),
            dimension: Some(wgpu::TextureViewDimension::D2),
            ..Default::default()
        });
        let bind_group = Self::create_panorama_bind_group(
            device,
            &self.bind_group_layout,
            &self.uniform,
            &cube_view,
            &self.sampler,
            &overlay_view,
        );

        self.bind_group = bind_group;
        self._cube_texture = cube_texture;
        self._overlay_texture = overlay_texture;
        self.face_size = face_size;
        self.overlay_size = overlay_size;
        self.real_panorama = true;
        Ok(())
    }

    /// Upload the six validated faces already decoded into
    /// [`VanillaGuiAssets`]. Returns `false` when the assets do not contain a
    /// complete equal-size panorama or when the GPU rejects their dimensions.
    pub fn upload_from_gui_assets(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        assets: &VanillaGuiAssets,
        overlay: Option<(&[u8], u32, u32)>,
    ) -> bool {
        if !assets.has_panorama() {
            return false;
        }
        let ids = [
            VanillaGuiSpriteId::Panorama0,
            VanillaGuiSpriteId::Panorama1,
            VanillaGuiSpriteId::Panorama2,
            VanillaGuiSpriteId::Panorama3,
            VanillaGuiSpriteId::Panorama4,
            VanillaGuiSpriteId::Panorama5,
        ];
        let mut faces: [&[u8]; 6] = [&[], &[], &[], &[], &[], &[]];
        let mut face_size = 0;
        for (index, id) in ids.into_iter().enumerate() {
            let Some(image) = assets.sprites.get(&id) else {
                return false;
            };
            if index == 0 {
                face_size = image.width;
            }
            faces[index] = image.rgba.as_slice();
        }
        self.upload_from_faces(device, queue, faces, face_size, overlay)
            .is_ok()
    }

    /// `true` once real six-face panorama pixels have been uploaded. The
    /// initial procedural sky-blue fallback renders normally but does not
    /// count as a vanilla panorama.
    pub fn has_panorama(&self) -> bool {
        self.real_panorama
    }

    /// Shared edge length of the currently bound cubemap.
    pub fn face_size(&self) -> u32 {
        self.face_size
    }

    /// Dimensions of the currently bound official overlay, when one was supplied.
    pub fn overlay_size(&self) -> Option<(u32, u32)> {
        self.overlay_size
    }

    /// Encode the panorama as one fullscreen-triangle draw. Besides wgpu's
    /// internal queue machinery, this hot path performs no heap allocation:
    /// the only CPU payload is one 48-byte uniform value built on the stack.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        queue: &wgpu::Queue,
        target_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        yaw_rad: f32,
        pitch_rad: f32,
        fade_alpha: f32,
    ) {
        let width = width.max(1) as f32;
        let height = height.max(1) as f32;
        let yaw = finite_or(yaw_rad, 0.0);
        let pitch = finite_or(pitch_rad, 0.0);
        let fade = finite_or(fade_alpha, 1.0).clamp(0.0, 1.0);
        let blur_radius = (0.75 / self.face_size.max(1) as f32).min(0.002);
        let uniform = TitlePanoramaUniform {
            resolution: [width, height, width / height, fade],
            rotation: [yaw.sin(), yaw.cos(), pitch.sin(), pitch.cos()],
            params: [
                (TITLE_PANORAMA_VERTICAL_FOV_DEG * 0.5).to_radians().tan(),
                blur_radius,
                if self.overlay_size.is_some() {
                    1.0
                } else {
                    0.0
                },
                0.0,
            ],
        };
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&uniform));

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("title-panorama"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, Some(&self.bind_group), &[]);
        pass.draw(0..3, 0..1);
    }
}
