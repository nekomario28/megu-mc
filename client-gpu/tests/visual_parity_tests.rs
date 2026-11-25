// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_client_gpu::atmosphere::{
    build_precipitation_sheet, resolve_vanilla_sky, vanilla_moon_phase, PrecipitationKind,
    VanillaAtmosphereInput, VanillaDimension, VanillaFogOverride,
};
use megu_client_gpu::vanilla_gui_sprites::{VanillaGuiAssets, VanillaGuiSpriteId};
use megu_client_gpu::renderer::{build_entity_ber_geometry_into, EntityBerGeometry};
use megu_client_gpu::subworld_render::{pack_particle_billboard, ParticleBillboardPack};
use megu_client_mesh::ber_instancing::{VanillaParticleKind, VanillaParticleQuad};
use megu_client_mesh::vanilla_plus_3d::{ExtrudedMesh, ExtrudedQuad};
use std::path::PathBuf;

#[test]
fn missing_jar_keeps_crisp_pixel_art_sprite_fallbacks_available() {
    let assets = VanillaGuiAssets::fallback();
    assert!(assets.sprites.is_empty(), "fallbacks must not claim JAR provenance");
    for id in [
        VanillaGuiSpriteId::HeartFull,
        VanillaGuiSpriteId::FoodFull,
        VanillaGuiSpriteId::ArmorFull,
        VanillaGuiSpriteId::Button,
    ] {
        let image = assets.sprite_or_fallback(id).expect("procedural sprite");
        assert_eq!(image.rgba.len(), image.width as usize * image.height as usize * 4);
        assert!(image.width >= 16 && image.height >= 16);
        assert!(image.rgba.chunks(4).any(|pixel| pixel[3] != 0));
    }
    let atlas = assets.build_atlas_with_fallbacks();
    assert!(atlas.entries.len() >= 20);
    assert!(!atlas.rgba.is_empty());
}

#[test]
fn local_official_jar_populates_workstation_hud_and_complete_gui_atlas() {
    let jar = std::env::var_os("MEGU_CLIENT_JAR").map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home).join(".megu-mc/versions/1.21.1/1.21.1.jar")
        })
    });
    let Some(jar) = jar.filter(|path| path.is_file()) else {
        return; // Portable on CI hosts without a locally installed client.
    };
    let assets = megu_client_gpu::vanilla_gui_sprites::load_vanilla_gui_from_path(&jar, &[]);
    for name in [
        "container/furnace.png",
        "container/brewing_stand.png",
        "container/enchanting_table.png",
        "sprites/container/anvil/error.png",
        "sprites/container/beacon/confirm.png",
        "sprites/container/brewing_stand/fuel_length.png",
        "sprites/container/furnace/lit_progress.png",
        "sprites/hud/heart/poisoned_full.png",
        "sprites/boss_bar/notched_20_progress.png",
    ] {
        assert!(assets.named_texture(name).is_some(), "missing Vanilla GUI texture {name}");
    }
    let atlas = assets.build_full_atlas();
    assert!(atlas.named_entries.len() >= 400, "all 400 Vanilla GUI sprites should be atlas packed");
}

#[test]
fn vanilla_moon_cycle_and_dimension_fog_are_deterministic() {
    let phases: Vec<_> = (0..8).map(vanilla_moon_phase).collect();
    assert_eq!(phases, [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(vanilla_moon_phase(8), 0);
    assert_eq!(vanilla_moon_phase(-1), 7);

    let overworld = resolve_vanilla_sky(VanillaAtmosphereInput::default());
    let nether = resolve_vanilla_sky(VanillaAtmosphereInput {
        dimension: VanillaDimension::Nether,
        ..VanillaAtmosphereInput::default()
    });
    let end = resolve_vanilla_sky(VanillaAtmosphereInput {
        dimension: VanillaDimension::End,
        ..VanillaAtmosphereInput::default()
    });
    assert!(nether.atmosphere.fog_density > overworld.atmosphere.fog_density);
    assert_eq!(nether.atmosphere.sun_intensity, 0.0);
    assert_eq!(end.atmosphere.sun_intensity, 0.0);
    assert_ne!(nether.atmosphere.sky_horizon_rgb, end.atmosphere.sky_horizon_rgb);

    let underwater = resolve_vanilla_sky(VanillaAtmosphereInput {
        fog_override: VanillaFogOverride::Underwater {
            biome_tint: [0.2, 0.45, 0.8],
            depth_blocks: 12.0,
        },
        ..VanillaAtmosphereInput::default()
    });
    assert!(underwater.atmosphere.fog_density > overworld.atmosphere.fog_density);
    for fog_override in [
        VanillaFogOverride::Lava,
        VanillaFogOverride::PowderSnow,
        VanillaFogOverride::Blindness { ticks_remaining: 20 },
        VanillaFogOverride::Darkness { ticks_remaining: 40 },
    ] {
        let fogged = resolve_vanilla_sky(VanillaAtmosphereInput {
            fog_override,
            ..VanillaAtmosphereInput::default()
        });
        assert!(fogged.atmosphere.fog_rgb.iter().all(|channel| channel.is_finite()));
        assert!(fogged.atmosphere.fog_density > 0.0);
    }
    let storm = resolve_vanilla_sky(VanillaAtmosphereInput {
        rain_strength: 0.8,
        snow_strength: 0.5,
        lightning_flash: 0.9,
        ..VanillaAtmosphereInput::default()
    });
    assert_eq!(storm.rain_strength, 0.8);
    assert_eq!(storm.snow_strength, 0.5);
    assert_eq!(storm.lightning_flash, 0.9);
    assert!(storm.atmosphere.sky_zenith_rgb[0] > overworld.atmosphere.sky_zenith_rgb[0]);
}

#[test]
fn precipitation_sheets_are_fixed_capacity_deterministic_geometry() {
    let rain = build_precipitation_sheet([12.0, 70.0, -4.0], 0.5, PrecipitationKind::Rain, 91);
    let rain_again = build_precipitation_sheet([12.0, 70.0, -4.0], 0.5, PrecipitationKind::Rain, 91);
    let snow = build_precipitation_sheet([12.0, 70.0, -4.0], 0.5, PrecipitationKind::Snow, 91);
    assert_eq!(rain.len, 32);
    assert_eq!(rain, rain_again);
    assert_ne!(rain.quads[0], snow.quads[0]);
    assert!(rain.quads[..rain.len]
        .iter()
        .flat_map(|quad| quad.positions.iter().flatten())
        .all(|value| value.is_finite()));
    assert_eq!(
        build_precipitation_sheet([f32::NAN, 0.0, 0.0], 1.0, PrecipitationKind::Snow, 0).len,
        0
    );
}

#[test]
fn both_sky_shader_variants_apply_the_uniform_moon_phase() {
    for source in [
        megu_client_gpu::renderer::sky_wgsl_with_moon_phase(false).into_owned(),
        megu_client_gpu::renderer::sky_wgsl_with_moon_phase(true).into_owned(),
    ] {
        assert!(source.contains("shadow.sun_color.w"));
        assert!(source.contains("let moon_lit = smoothstep"));
        assert!(source.contains("let twinkle ="));
    }
}

#[test]
fn entity_ber_and_particle_geometry_pack_for_headless_gpu_upload() {
    let mut mesh = ExtrudedMesh::new();
    mesh.quads.push(ExtrudedQuad {
        positions: [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
        normal: [0.0, 0.0, 1.0],
        uv: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        tint_index: -1,
    });
    let meshes = [(&mesh, [4.0, 5.0, 6.0], [1.0, 0.5, 0.25])];
    let mut light = |_, _, _| (7, 11);
    let mut packed = EntityBerGeometry {
        lit_vertices: Vec::with_capacity(4),
        lit_indices: Vec::with_capacity(6),
        vertices: Vec::with_capacity(4),
        indices: Vec::with_capacity(6),
    };
    build_entity_ber_geometry_into(&mut packed, &meshes, &mut light, 1.0);
    let capacities = (
        packed.lit_vertices.capacity(),
        packed.lit_indices.capacity(),
        packed.vertices.capacity(),
        packed.indices.capacity(),
    );
    build_entity_ber_geometry_into(&mut packed, &meshes, &mut light, 1.0);
    assert_eq!(
        capacities,
        (
            packed.lit_vertices.capacity(),
            packed.lit_indices.capacity(),
            packed.vertices.capacity(),
            packed.indices.capacity(),
        )
    );
    assert_eq!(packed.vertices.len(), 4);
    assert_eq!(packed.indices, [0, 1, 2, 0, 2, 3]);
    assert_eq!(packed.vertices[0].position, [4.0, 5.0, 6.0]);
    assert_eq!(packed.lit_vertices[0].light_attr, (7 << 5) | (11 << 9));

    let quad = VanillaParticleQuad {
        kind: VanillaParticleKind::Smoke,
        positions: [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0], [-1.0, 1.0, 0.0]],
        uv: [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        color: [0.7, 0.8, 0.9, 0.4],
    };
    let ParticleBillboardPack::Instanced(instance) = pack_particle_billboard(&quad) else {
        panic!("planar affine particle should use instance packing");
    };
    assert_eq!(instance.center, [0.0; 3]);
    assert_eq!(instance.color, quad.color);
    assert_eq!(instance.axis_u, [1.0, 0.0, 0.0]);
}
