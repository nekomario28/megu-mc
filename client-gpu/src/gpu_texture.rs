// SPDX-License-Identifier: LGPL-3.0-or-later

//! Texture formats and sampling policies for uploaded GPU assets.

use wgpu::{AddressMode, FilterMode, MipmapFilterMode, SamplerDescriptor, TextureFormat};

pub fn create_srgb_color_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    mip_level_count: u32,
) -> wgpu::Texture {
    create_rgba8_texture(
        device,
        label,
        width,
        height,
        mip_level_count,
        TextureFormat::Rgba8UnormSrgb,
    )
}

pub fn create_linear_data_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    mip_level_count: u32,
) -> wgpu::Texture {
    create_rgba8_texture(
        device,
        label,
        width,
        height,
        mip_level_count,
        TextureFormat::Rgba8Unorm,
    )
}

fn create_rgba8_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    mip_level_count: u32,
    format: TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Upload tightly packed RGBA8, including unaligned row widths.
pub fn write_rgba8_subresource(
    queue: &wgpu::Queue,
    destination: wgpu::TexelCopyTextureInfo<'_>,
    extent: wgpu::Extent3d,
    rgba: &[u8],
) {
    queue.write_texture(
        destination,
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(extent.width * 4),
            rows_per_image: Some(extent.height),
        },
        extent,
    );
}

pub fn pixel_art_sampler(address: AddressMode) -> SamplerDescriptor<'static> {
    SamplerDescriptor {
        label: Some("pixel-art-sampler"),
        address_mode_u: address,
        address_mode_v: address,
        address_mode_w: address,
        mag_filter: FilterMode::Nearest,
        min_filter: FilterMode::Nearest,
        mipmap_filter: MipmapFilterMode::Nearest,
        ..Default::default()
    }
}

pub fn block_atlas_sampler() -> SamplerDescriptor<'static> {
    SamplerDescriptor {
        label: Some("block-atlas-sampler"),
        mipmap_filter: MipmapFilterMode::Linear,
        ..pixel_art_sampler(AddressMode::ClampToEdge)
    }
}

pub fn linear_color_sampler(address: AddressMode) -> SamplerDescriptor<'static> {
    SamplerDescriptor {
        label: Some("linear-color-sampler"),
        address_mode_u: address,
        address_mode_v: address,
        address_mode_w: address,
        mag_filter: FilterMode::Linear,
        min_filter: FilterMode::Linear,
        ..Default::default()
    }
}

pub fn shadow_compare_sampler() -> SamplerDescriptor<'static> {
    SamplerDescriptor {
        label: Some("shadow-compare-sampler"),
        compare: Some(wgpu::CompareFunction::LessEqual),
        ..linear_color_sampler(AddressMode::ClampToEdge)
    }
}
