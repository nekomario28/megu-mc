// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_gpu::vanilla_jar::{
    ber_texture_path_for_name, entity_texture_path_for_id,
    load_vanilla_entity_atlas_from_jar_bytes, load_vanilla_particle_atlas_from_jar_bytes,
    particle_texture_paths_for_id, VanillaEntityTextureAtlas,
};

struct Entry<'a> {
    name: &'a str,
    bytes: &'a [u8],
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn png(rgba: [u8; 4]) -> Vec<u8> {
    fn chunk(kind: &[u8; 4], data: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_data = kind.to_vec();
        crc_data.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr, &mut out);
    let mut raw = vec![0];
    raw.extend_from_slice(&rgba);
    let mut zlib = vec![0x78, 0x01, 0x01];
    zlib.extend_from_slice(&(raw.len() as u16).to_le_bytes());
    zlib.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
    zlib.extend_from_slice(&raw);
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    chunk(b"IDAT", &zlib, &mut out);
    chunk(b"IEND", &[], &mut out);
    out
}

fn zip(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        let offset = out.len() as u32;
        let name = entry.name.as_bytes();
        let crc = crc32(entry.bytes);
        out.extend_from_slice(&0x04034b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(entry.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(entry.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(entry.bytes);
        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(entry.bytes.len() as u32).to_le_bytes());
        central.extend_from_slice(&(entry.bytes.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 6]);
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let central_start = out.len() as u32;
    out.extend_from_slice(&central);
    let central_len = central.len() as u32;
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_len.to_le_bytes());
    out.extend_from_slice(&central_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn valid_rect(rect: [f32; 4]) -> bool {
    rect.iter()
        .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        && rect[2] > rect[0]
        && rect[3] > rect[1]
}

#[test]
fn fixed_texture_entities_and_block_entities_have_atlas_regions() {
    let sample = png([20, 80, 140, 255]);
    let base_path = entity_texture_path_for_id(0).unwrap();
    let jar = zip(&[Entry {
        name: base_path,
        bytes: &sample,
    }]);
    let backed = load_vanilla_entity_atlas_from_jar_bytes(&jar, &[]);
    let fallback = load_vanilla_entity_atlas_from_jar_bytes(&[], &[]);
    for id in 0..=129 {
        let Some(path) = entity_texture_path_for_id(id) else {
            let name = megu_client_mesh::entity_rig::classify_entity_rig(id)
                .unwrap()
                .name;
            assert!(matches!(
                name,
                "minecraft:area_effect_cloud"
                    | "minecraft:block_display"
                    | "minecraft:falling_block"
                    | "minecraft:interaction"
                    | "minecraft:item"
                    | "minecraft:item_display"
                    | "minecraft:lightning_bolt"
                    | "minecraft:marker"
                    | "minecraft:ominous_item_spawner"
                    | "minecraft:text_display"
            ));
            assert!(backed.entity_uv(id).is_none());
            assert!(fallback.entity_uv(id).is_none());
            continue;
        };
        assert!(
            path.starts_with("assets/minecraft/textures/"),
            "id {id}: {path}"
        );
        assert!(valid_rect(backed.entity_uv(id).unwrap()), "jar id {id}");
        assert!(
            valid_rect(fallback.entity_uv(id).unwrap()),
            "fallback id {id}"
        );
    }
    for name in [
        "chest",
        "trapped_chest",
        "ender_chest",
        "shulker",
        "bed",
        "sign",
        "banner",
        "bell",
        "enchanting_table",
        "beacon",
        "conduit",
        "decorated_pot",
    ] {
        assert!(ber_texture_path_for_name(name).is_some());
        assert!(valid_rect(backed.ber_uv(name).unwrap()));
        assert!(valid_rect(fallback.ber_uv(name).unwrap()));
    }
    assert!(ber_texture_path_for_name("trapped_chest")
        .unwrap()
        .ends_with("chest/trapped.png"));
    assert!(ber_texture_path_for_name("ender_chest")
        .unwrap()
        .ends_with("chest/ender.png"));
}

#[test]
fn every_particle_id_has_uvs_at_start_middle_and_end_for_jar_and_fallback() {
    let sample = png([90, 30, 220, 255]);
    let sprite = "assets/minecraft/textures/particle/generic_0.png";
    let jar = zip(&[Entry {
        name: sprite,
        bytes: &sample,
    }]);
    let backed = load_vanilla_particle_atlas_from_jar_bytes(&jar, &[]);
    let fallback = load_vanilla_particle_atlas_from_jar_bytes(&[], &[]);
    for id in 0..=108 {
        let paths = particle_texture_paths_for_id(id);
        assert!(!paths.is_empty(), "particle {id}");
        assert!(paths
            .iter()
            .all(|p| { p.starts_with("assets/minecraft/textures/particle/") }));
        for age in [0.0, 0.5, 1.0] {
            assert!(
                valid_rect(backed.particle_uv(id, age).unwrap()),
                "jar id {id} age {age}"
            );
            assert!(
                valid_rect(fallback.particle_uv(id, age).unwrap()),
                "fallback id {id} age {age}"
            );
        }
    }
}

#[test]
fn resource_pack_overrides_base_and_corrupt_png_uses_closed_fallback() {
    let path = entity_texture_path_for_id(0).unwrap();
    let base_png = png([10, 20, 30, 255]);
    let pack_png = png([200, 150, 100, 255]);
    let base = zip(&[Entry {
        name: path,
        bytes: &base_png,
    }]);
    let pack = zip(&[Entry {
        name: &format!("wrapped-pack/{path}"),
        bytes: &pack_png,
    }]);
    let base_atlas = load_vanilla_entity_atlas_from_jar_bytes(&base, &[]);
    let overlaid = load_vanilla_entity_atlas_from_jar_bytes(&base, &[&pack]);
    let pixel = |atlas: &VanillaEntityTextureAtlas| {
        let uv = atlas.entity_uv(0).unwrap();
        let x = (uv[0] * atlas.width as f32) as usize;
        let y = (uv[1] * atlas.height as f32) as usize;
        atlas.rgba[(y * atlas.width as usize + x) * 4..][..4].to_vec()
    };
    assert_eq!(pixel(&base_atlas), [10, 20, 30, 255]);
    assert_eq!(pixel(&overlaid), [200, 150, 100, 255]);
    let corrupt = zip(&[Entry {
        name: path,
        bytes: b"not a png",
    }]);
    let retained = load_vanilla_entity_atlas_from_jar_bytes(&base, &[&pack, &corrupt]);
    assert_eq!(pixel(&retained), [200, 150, 100, 255]);
    let closed = load_vanilla_entity_atlas_from_jar_bytes(&corrupt, &[]);
    let fallback = load_vanilla_entity_atlas_from_jar_bytes(&[], &[]);
    assert!(valid_rect(closed.entity_uv(0).unwrap()));
    assert_eq!(pixel(&closed), pixel(&fallback));
}

#[test]
fn particle_definitions_preserve_pack_frame_order_and_extrude_gutters() {
    use megu_client_mesh::ber_instancing::VanillaParticleKind;
    let id = (0..=108)
        .find(|id| {
            VanillaParticleKind::from_protocol_id(*id)
                .is_some_and(|kind| kind.registry_name() == "trial_spawner_detected_player")
        })
        .unwrap();
    let red = png([255, 0, 0, 255]);
    let blue = png([0, 0, 255, 255]);
    let base = zip(&[
        Entry {
            name: "assets/minecraft/particles/trial_spawner_detection.json",
            bytes: br#"{"textures":["test:red","test:blue"]}"#,
        },
        Entry {
            name: "assets/test/textures/particle/red.png",
            bytes: &red,
        },
        Entry {
            name: "assets/test/textures/particle/blue.png",
            bytes: &blue,
        },
    ]);
    let pack = zip(&[Entry {
        name: "outer/assets/minecraft/particles/trial_spawner_detection.json",
        bytes: br#"{"textures":["test:blue","test:red"]}"#,
    }]);
    let atlas = load_vanilla_particle_atlas_from_jar_bytes(&base, &[&pack]);
    assert!(atlas.width <= 8192 && atlas.height <= 8192);
    for (age, expected) in [(0.0, [0, 0, 255, 255]), (1.0, [255, 0, 0, 255])] {
        let uv = atlas.particle_uv(u16::from(id), age).unwrap();
        let x = (uv[0] * atlas.width as f32).round() as usize;
        let y = (uv[1] * atlas.height as f32).round() as usize;
        for row in y - 1..=y + 1 {
            for column in x - 1..=x + 1 {
                let offset = (row * atlas.width as usize + column) * 4;
                assert_eq!(atlas.rgba[offset..offset + 4], expected);
            }
        }
    }
}
