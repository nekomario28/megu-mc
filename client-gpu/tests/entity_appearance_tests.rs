// SPDX-License-Identifier: LGPL-3.0-or-later

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use megu_client_gpu::entity_appearance::{
    EntityAppearanceQuery, EntityOverlayKind, EquipmentItem, RuntimeEntityAppearanceAtlas,
};
use megu_client_gpu::player_skins::{ResolvedPlayerSkin, SkinSource};
use megu_client_gpu::vanilla_jar::load_vanilla_entity_atlas_from_jar_bytes;
use megu_client_gpu::vanilla_official_extras::{default_skin_for_uuid, VanillaOfficialExtras};
use megu_core::hash::crc32_ieee;

static NEXT_JAR: AtomicUsize = AtomicUsize::new(0);

fn atlas_pixel(
    atlas: &megu_client_gpu::vanilla_jar::VanillaEntityTextureAtlas,
    uv: [f32; 4],
) -> [u8; 4] {
    let x = (uv[0] * atlas.width as f32).round() as usize;
    let y = (uv[1] * atlas.height as f32).round() as usize;
    let start = (y * atlas.width as usize + x) * 4;
    atlas.rgba[start..start + 4].try_into().unwrap()
}

fn valid_rect(uv: [f32; 4]) -> bool {
    uv.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)) && uv[0] < uv[2] && uv[1] < uv[3]
}

fn synthetic_extras(entries: &[(&str, u32, u32, [u8; 4])]) -> VanillaOfficialExtras {
    let zip_entries: Vec<(String, Vec<u8>)> = entries
        .iter()
        .map(|(key, width, height, color)| {
            (
                format!("assets/minecraft/textures/{key}"),
                png_rgba(*width, *height, *color),
            )
        })
        .collect();
    let zip_refs: Vec<(&str, &[u8])> = zip_entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let jar = stored_zip(&zip_refs);
    let id = NEXT_JAR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "megu-entity-appearance-{}-{id}.jar",
        std::process::id()
    ));
    fs::write(&path, jar).unwrap();
    let extras = VanillaOfficialExtras::load(Some(&path), None, "17");
    let _ = fs::remove_file(path);
    extras
}

fn player_test_extras(uuid: [u8; 16]) -> VanillaOfficialExtras {
    let (skin, slim) = default_skin_for_uuid(uuid);
    let skin_key = format!(
        "entity/player/{}/{skin}.png",
        if slim { "slim" } else { "wide" }
    );
    let entries = [
        (skin_key.as_str(), 64, 64, [10, 80, 140, 255]),
        ("entity/sheep/sheep_fur.png", 64, 64, [230, 230, 230, 255]),
        (
            "entity/creeper/creeper_armor.png",
            64,
            64,
            [80, 220, 80, 255],
        ),
        ("entity/spider_eyes.png", 64, 64, [255, 20, 20, 255]),
        (
            "entity/enderman/enderman_eyes.png",
            64,
            64,
            [220, 40, 220, 255],
        ),
        ("entity/villager/type/taiga.png", 64, 64, [120, 90, 50, 255]),
        (
            "entity/villager/profession/mason.png",
            64,
            64,
            [100, 100, 110, 255],
        ),
        (
            "entity/villager/profession_level/gold.png",
            64,
            64,
            [240, 200, 40, 255],
        ),
        (
            "entity/wolf/wolf_rusty_tame.png",
            64,
            64,
            [160, 90, 50, 255],
        ),
        ("entity/wolf/wolf_collar.png", 64, 64, [250, 250, 250, 255]),
        ("entity/horse/horse_white.png", 64, 64, [230, 230, 220, 255]),
        (
            "entity/horse/horse_markings_whitefield.png",
            64,
            64,
            [250, 250, 250, 255],
        ),
        (
            "entity/horse/armor/horse_armor_leather.png",
            64,
            64,
            [120, 70, 30, 255],
        ),
        ("entity/bed/red.png", 64, 64, [180, 20, 20, 255]),
        ("entity/bed/blue.png", 64, 64, [20, 20, 180, 255]),
        (
            "entity/shulker/shulker_lime.png",
            64,
            64,
            [80, 220, 40, 255],
        ),
        (
            "models/armor/netherite_layer_1.png",
            64,
            32,
            [45, 40, 45, 255],
        ),
        (
            "models/armor/netherite_layer_2.png",
            64,
            32,
            [45, 40, 45, 255],
        ),
        (
            "models/armor/diamond_layer_1.png",
            64,
            32,
            [60, 220, 220, 255],
        ),
        (
            "models/armor/diamond_layer_2.png",
            64,
            32,
            [60, 220, 220, 255],
        ),
        (
            "models/armor/leather_layer_1.png",
            64,
            32,
            [130, 80, 45, 255],
        ),
        (
            "models/armor/leather_layer_2.png",
            64,
            32,
            [130, 80, 45, 255],
        ),
        (
            "models/armor/leather_layer_1_overlay.png",
            64,
            32,
            [200, 160, 110, 255],
        ),
        (
            "models/armor/leather_layer_2_overlay.png",
            64,
            32,
            [200, 160, 110, 255],
        ),
        (
            "models/armor/chainmail_layer_1.png",
            64,
            32,
            [130, 130, 140, 255],
        ),
        (
            "models/armor/chainmail_layer_2.png",
            64,
            32,
            [130, 130, 140, 255],
        ),
    ];
    synthetic_extras(&entries)
}

fn runtime_with_extras(uuid: [u8; 16]) -> RuntimeEntityAppearanceAtlas {
    let base = load_vanilla_entity_atlas_from_jar_bytes(&[], &[]);
    RuntimeEntityAppearanceAtlas::new(&base, &player_test_extras(uuid))
}

fn kinds(
    layers: &megu_client_gpu::entity_appearance::EntityVisualLayers,
) -> Vec<EntityOverlayKind> {
    layers.overlays.iter().map(|layer| layer.kind).collect()
}

#[test]
fn player_uuid_default_then_custom_mojang_skin_and_cape_bump_revision() {
    let uuid = [0x42; 16];
    let (default_name, default_slim) = default_skin_for_uuid(uuid);
    let mut runtime = runtime_with_extras(uuid);
    assert_eq!(runtime.revision(), 0);
    assert!(runtime.is_dirty());
    assert!(runtime.take_dirty());
    assert!(!runtime.is_dirty());
    assert_eq!(
        runtime.atlas().rgba.len(),
        (runtime.atlas().width * runtime.atlas().height * 4) as usize
    );

    let query = EntityAppearanceQuery {
        entity_name: Some("minecraft:player"),
        uuid: Some(uuid),
        ..Default::default()
    };
    let initial = runtime.resolve_entity(&query);
    assert_eq!(initial.slim_arms, default_slim);
    let default_key = format!(
        "entity/player/{}/{default_name}.png",
        if default_slim { "slim" } else { "wide" }
    );
    assert_eq!(initial.base_uv, runtime.texture_uv(&default_key));
    assert!(valid_rect(initial.base_uv.unwrap()));
    assert_eq!(
        atlas_pixel(runtime.atlas(), initial.base_uv.unwrap()),
        [10, 80, 140, 255]
    );
    assert!(initial.overlays.iter().all(|layer| valid_rect(layer.uv)));

    let skin_rgba = (0..64 * 64)
        .flat_map(|n| [(n % 251) as u8, 30, 200, 255])
        .collect::<Vec<_>>();
    let cape_rgba = (0..64 * 32)
        .flat_map(|n| [220, (n % 241) as u8, 20, 255])
        .collect::<Vec<_>>();
    let resolved = ResolvedPlayerSkin {
        uuid,
        skin_rgba,
        width: 64,
        height: 64,
        slim: !default_slim,
        cape_rgba: Some((64, 32, cape_rgba)),
        source: SkinSource::Mojang,
    };
    assert!(runtime.upsert_player_skin(uuid, &resolved));
    assert_eq!(runtime.revision(), 1);
    assert!(runtime.is_dirty());

    let custom = runtime.resolve_entity(&query);
    assert_eq!(custom.slim_arms, !default_slim);
    assert_eq!(custom.base_uv, runtime.player_skin_uv(uuid));
    assert!(valid_rect(custom.base_uv.unwrap()));
    assert_eq!(
        atlas_pixel(runtime.atlas(), custom.base_uv.unwrap()),
        [0, 30, 200, 255]
    );
    let capes: Vec<_> = custom
        .overlays
        .iter()
        .filter(|layer| layer.kind == EntityOverlayKind::Cape)
        .collect();
    assert_eq!(capes.len(), 1);
    assert_eq!(Some(capes[0].uv), runtime.player_cape_uv(uuid));
    assert!(valid_rect(capes[0].uv));
    assert_eq!(atlas_pixel(runtime.atlas(), capes[0].uv), [220, 0, 20, 255]);

    assert!(!runtime.upsert_player_skin(uuid, &resolved));
    assert_eq!(runtime.revision(), 1);
}

#[test]
fn representative_base_entity_and_ber_pixels_are_retained() {
    let base = load_vanilla_entity_atlas_from_jar_bytes(&[], &[]);
    let zombie_uv = base.entity_uv(124).unwrap();
    let chest_uv = base.ber_uv("chest").unwrap();
    let zombie_pixel = atlas_pixel(&base, zombie_uv);
    let chest_pixel = atlas_pixel(&base, chest_uv);
    let runtime = RuntimeEntityAppearanceAtlas::new(&base, &player_test_extras([0x11; 16]));

    let zombie = runtime.resolve_entity(&EntityAppearanceQuery {
        entity_type_id: Some(124),
        ..Default::default()
    });
    assert_eq!(
        atlas_pixel(runtime.atlas(), zombie.base_uv.unwrap()),
        zombie_pixel
    );
    let chest = runtime.resolve_ber("chest", "").unwrap();
    assert_eq!(atlas_pixel(runtime.atlas(), chest), chest_pixel);
}

#[test]
fn mob_variants_state_overlays_villager_layers_armor_and_ber_lookup() {
    let uuid = [0x24; 16];
    let runtime = runtime_with_extras(uuid);

    let sheep = runtime.resolve_entity(&EntityAppearanceQuery {
        entity_name: Some("sheep"),
        color: Some("blue"),
        ..Default::default()
    });
    assert!(valid_rect(sheep.base_uv.unwrap()));
    let wool: Vec<_> = sheep
        .overlays
        .iter()
        .filter(|layer| layer.kind == EntityOverlayKind::SheepWool)
        .collect();
    assert_eq!(wool.len(), 1);
    assert_ne!(wool[0].tint, [1.0, 1.0, 1.0]);
    assert!(valid_rect(wool[0].uv));
    assert_eq!(
        atlas_pixel(runtime.atlas(), wool[0].uv),
        [230, 230, 230, 255]
    );
    let sheared = runtime.resolve_entity(&EntityAppearanceQuery {
        entity_name: Some("sheep"),
        color: Some("blue"),
        sheared: true,
        ..Default::default()
    });
    assert!(!kinds(&sheared).contains(&EntityOverlayKind::SheepWool));

    let creeper = runtime.resolve_entity(&EntityAppearanceQuery {
        entity_name: Some("creeper"),
        powered: true,
        ..Default::default()
    });
    assert!(kinds(&creeper).contains(&EntityOverlayKind::CreeperCharge));

    for entity in ["spider", "enderman"] {
        let visual = runtime.resolve_entity(&EntityAppearanceQuery {
            entity_name: Some(entity),
            ..Default::default()
        });
        let eyes: Vec<_> = visual
            .overlays
            .iter()
            .filter(|layer| layer.kind == EntityOverlayKind::EmissiveEyes)
            .collect();
        assert_eq!(eyes.len(), 1, "{entity} eyes");
        assert!(eyes[0].emissive);
        assert!(valid_rect(eyes[0].uv));
    }

    let villager = runtime.resolve_entity(&EntityAppearanceQuery {
        entity_name: Some("villager"),
        villager_biome: Some("taiga"),
        villager_profession: Some("mason"),
        villager_level: 3,
        ..Default::default()
    });
    let villager_kinds = kinds(&villager);
    for kind in [
        EntityOverlayKind::VillagerBiome,
        EntityOverlayKind::VillagerProfession,
        EntityOverlayKind::VillagerLevel,
    ] {
        assert!(villager_kinds.contains(&kind), "missing {kind:?}");
    }

    let wolf = runtime.resolve_entity(&EntityAppearanceQuery {
        entity_name: Some("wolf"),
        variant: Some("rusty"),
        tame: true,
        collar_color: Some("blue"),
        ..Default::default()
    });
    assert_eq!(
        wolf.base_uv,
        runtime.texture_uv("entity/wolf/wolf_rusty_tame.png")
    );
    assert_eq!(
        atlas_pixel(runtime.atlas(), wolf.base_uv.unwrap()),
        [160, 90, 50, 255]
    );
    assert!(kinds(&wolf).contains(&EntityOverlayKind::WolfCollar));

    let horse = runtime.resolve_entity(&EntityAppearanceQuery {
        entity_name: Some("horse"),
        variant: Some("white"),
        marking: Some("whitefield"),
        horse_armor: Some("leather"),
        ..Default::default()
    });
    assert_eq!(
        horse.base_uv,
        runtime.texture_uv("entity/horse/horse_white.png")
    );
    let horse_kinds = kinds(&horse);
    assert!(horse_kinds.contains(&EntityOverlayKind::HorseMarking));
    assert!(horse_kinds.contains(&EntityOverlayKind::HorseArmor));

    let armored = runtime.resolve_entity(&EntityAppearanceQuery {
        entity_name: Some("zombie"),
        equipment: megu_client_gpu::entity_appearance::EntityEquipmentSlots {
            head: Some(EquipmentItem::new("minecraft:netherite_helmet", false)),
            chest: Some(EquipmentItem::new("diamond_chestplate", true)),
            legs: Some(EquipmentItem::new("leather_leggings", false)),
            feet: Some(EquipmentItem::new("chainmail_boots", false)),
        },
        ..Default::default()
    });
    let armor_kinds = kinds(&armored);
    assert_eq!(
        armor_kinds
            .iter()
            .filter(|kind| **kind == EntityOverlayKind::ArmorLayer1)
            .count(),
        3
    );
    assert_eq!(
        armor_kinds
            .iter()
            .filter(|kind| **kind == EntityOverlayKind::ArmorLayer2)
            .count(),
        2,
        "leather leggings contribute the dyeable overlay as a second layer"
    );
    assert_eq!(
        armor_kinds
            .iter()
            .filter(|kind| **kind == EntityOverlayKind::EnchantedGlint)
            .count(),
        1
    );
    assert!(armored.overlays.iter().all(|layer| valid_rect(layer.uv)));

    let red_bed = runtime.resolve_ber("bed", "red").unwrap();
    let blue_bed = runtime.resolve_ber("minecraft:bed", "blue").unwrap();
    assert!(valid_rect(red_bed));
    assert!(valid_rect(blue_bed));
    assert_ne!(red_bed, blue_bed);
    assert_eq!(atlas_pixel(runtime.atlas(), blue_bed), [20, 20, 180, 255]);
    assert!(valid_rect(
        runtime.resolve_ber("shulker_box", "lime").unwrap()
    ));
}

fn png_rgba(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut filtered = Vec::with_capacity((row_bytes + 1) * height as usize);
    let row = color.repeat(width as usize);
    for _ in 0..height {
        filtered.extend_from_slice(&[0]);
        filtered.extend_from_slice(&row);
    }
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &zlib_stored(&filtered));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32_ieee(&crc_input).to_be_bytes());
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let count = data.len().div_ceil(65_535);
    for (index, chunk) in data.chunks(65_535).enumerate() {
        out.push(u8::from(index + 1 == count));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in data {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn w16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn w32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for &(name, payload) in entries {
        let local_offset = out.len() as u32;
        let crc = crc32_ieee(payload);
        let len = payload.len() as u32;
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, crc);
        w32(&mut out, len);
        w32(&mut out, len);
        w16(&mut out, name.len() as u16);
        w16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);

        w32(&mut central, 0x0201_4b50);
        w16(&mut central, 20);
        w16(&mut central, 20);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, crc);
        w32(&mut central, len);
        w32(&mut central, len);
        w16(&mut central, name.len() as u16);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, 0);
        w32(&mut central, local_offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, entries.len() as u16);
    w16(&mut out, entries.len() as u16);
    w32(&mut out, central_size);
    w32(&mut out, central_offset);
    w16(&mut out, 0);
    out
}
