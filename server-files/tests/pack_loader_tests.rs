// SPDX-License-Identifier: LGPL-3.0-or-later
//! End-to-end pack facade tests with independently constructed ZIP fixtures.

use mc_spine_server_files::pack_loader::{
    load_validated_pack_dir, load_validated_pack_zip, PackLoadError,
};
use mc_spine_server_files::{
    inspect_and_route_pack, PackKind, PackRouteDecision, PackRouterPolicy, PackThreatKind,
};
use std::fs;
use std::path::PathBuf;

const META15: &[u8] = br#"{"pack":{"pack_format":15,"description":"3D Default"}}"#;
const BLOCKSTATE: &[u8] = br#"{"variants":{"":{"model":"minecraft:block/stone"}}}"#;
const MODEL: &[u8] = br#"{"parent":"minecraft:block/cube_all"}"#;
const JEM: &[u8] = br#"{"models":[{"part":"head","model":"head.jpm"}]}"#;
const JPM: &[u8] = br#"{"boxes":[{"coordinates":[0,0,0,2,2,2],"textureOffset":[0,0]}]}"#;
const CTM: &[u8] = b"method=fixed\ntiles=0\n";
const EMISSIVE: &[u8] = b"suffix.emissive=_glow\n";

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
        }
    }
    !crc
}

fn zip(members: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut central = Vec::new();
    for &(name, payload, deflated) in members {
        let encoded = if deflated {
            assert!(payload.len() <= u16::MAX as usize);
            let mut bytes = vec![1];
            let len = payload.len() as u16;
            bytes.extend(len.to_le_bytes());
            bytes.extend((!len).to_le_bytes());
            bytes.extend(payload);
            bytes
        } else {
            payload.to_vec()
        };
        let start = output.len() as u32;
        let method: u16 = if deflated { 8 } else { 0 };
        let crc = crc32(payload);
        let compressed = encoded.len() as u32;
        let size = payload.len() as u32;

        output.extend(0x0403_4b50u32.to_le_bytes());
        output.extend(20u16.to_le_bytes());
        output.extend(0u16.to_le_bytes());
        output.extend(method.to_le_bytes());
        output.extend([0; 4]);
        output.extend(crc.to_le_bytes());
        output.extend(compressed.to_le_bytes());
        output.extend(size.to_le_bytes());
        output.extend((name.len() as u16).to_le_bytes());
        output.extend(0u16.to_le_bytes());
        output.extend(name.as_bytes());
        output.extend(&encoded);

        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(method.to_le_bytes());
        central.extend([0; 4]);
        central.extend(crc.to_le_bytes());
        central.extend(compressed.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u32.to_le_bytes());
        central.extend(start.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let start = output.len() as u32;
    output.extend(&central);
    output.extend(0x0605_4b50u32.to_le_bytes());
    output.extend([0; 4]);
    output.extend((members.len() as u16).to_le_bytes());
    output.extend((members.len() as u16).to_le_bytes());
    output.extend((central.len() as u32).to_le_bytes());
    output.extend(start.to_le_bytes());
    output.extend(0u16.to_le_bytes());
    output
}

fn fixtures() -> Vec<(&'static str, &'static [u8], bool)> {
    vec![
        ("pack.mcmeta", META15, false),
        ("assets/minecraft/blockstates/stone.json", BLOCKSTATE, false),
        ("assets/minecraft/models/block/stone.json", MODEL, false),
        ("assets/minecraft/optifine/cem/example.jem", JEM, false),
        ("assets/minecraft/optifine/cem/head.jpm", JPM, false),
        ("assets/minecraft/optifine/ctm/stone.properties", CTM, false),
        (
            "assets/minecraft/optifine/emissive.properties",
            EMISSIVE,
            false,
        ),
    ]
}

fn assert_loaded(pack: mc_spine_server_files::pack_loader::LoadedPack) {
    assert_eq!(pack.kind, PackKind::ResourcePack);
    assert_eq!(pack.pack_format, Some(15));
    assert_eq!(pack.blockstates.len(), 1);
    assert_eq!(pack.models.len(), 1);
    assert_eq!(pack.jem_models.len(), 1);
    assert_eq!(pack.jpm_models.len(), 1);
    assert_eq!(pack.ctm_rules.len(), 1);
    assert_eq!(pack.emissive.len(), 1);
    assert!(pack.warnings.is_empty(), "{:?}", pack.warnings);
    assert_eq!(
        pack.emissive["assets/minecraft/optifine/emissive.properties"].suffix_emissive,
        "_glow"
    );
}

fn assert_quarantined(bytes: &[u8], expected: PackThreatKind) {
    match inspect_and_route_pack(bytes, &PackRouterPolicy::default()).decision {
        PackRouteDecision::Quarantine { threats } => {
            assert!(
                threats.iter().any(|issue| issue.kind == expected),
                "{threats:?}"
            );
        }
        routed => panic!("expected router quarantine, got {routed:?}"),
    }
    match load_validated_pack_zip(bytes) {
        Err(PackLoadError::Quarantined(threats)) => {
            assert!(
                threats.iter().any(|issue| issue.kind == expected),
                "{threats:?}"
            );
        }
        result => panic!("unsafe ZIP accepted: {result:?}"),
    }
}

#[test]
fn stored_zip_3d_resource_pack_loads() {
    assert_loaded(load_validated_pack_zip(&zip(&fixtures())).unwrap());
}

#[test]
fn model_only_3d_pack_is_a_resource_pack() {
    let pack = load_validated_pack_zip(&zip(&[
        ("pack.mcmeta", META15, false),
        ("assets/minecraft/blockstates/stone.json", BLOCKSTATE, false),
        ("assets/minecraft/models/block/stone.json", MODEL, false),
        ("assets/minecraft/optifine/cem/example.jem", JEM, false),
    ]))
    .unwrap();
    assert_eq!(pack.kind, PackKind::ResourcePack);
    assert_eq!(pack.pack_format, Some(15));
    assert_eq!(pack.blockstates.len(), 1);
    assert_eq!(pack.jem_models.len(), 1);
    assert!(pack.warnings.is_empty());
}

#[test]
fn deflated_zip_3d_resource_pack_loads() {
    let mut members = fixtures();
    for entry in &mut members {
        entry.2 = true;
    }
    assert_loaded(load_validated_pack_zip(&zip(&members)).unwrap());
}

#[test]
fn huffman_deflated_metadata_loads() {
    // Raw RFC 1951 DEFLATE (fixed Huffman), produced by zlib with wbits=-15.
    let compressed_metadata = [
        0xab, 0x56, 0x2a, 0x48, 0x4c, 0xce, 0x56, 0xb2, 0xaa, 0x06, 0xd3, 0xf1, 0x69, 0xf9, 0x45,
        0xb9, 0x89, 0x25, 0x4a, 0x56, 0x86, 0xa6, 0x3a, 0x4a, 0x29, 0xa9, 0xc5, 0xc9, 0x45, 0x99,
        0x05, 0x25, 0x99, 0xf9, 0x79, 0x4a, 0x56, 0x4a, 0xc6, 0x2e, 0x0a, 0x2e, 0xa9, 0x69, 0x89,
        0xa5, 0x39, 0x25, 0x4a, 0xb5, 0xb5, 0x00,
    ];
    let resource_path = "assets/minecraft/optifine/ctm/stone.properties";
    let metadata_path = "pack.mcmeta";
    let mut archive = zip(&[(resource_path, CTM, false), (metadata_path, META15, true)]);
    let old_central = u32::from_le_bytes(
        archive[archive.len() - 6..archive.len() - 2]
            .try_into()
            .unwrap(),
    ) as usize;
    let metadata_local = 30 + resource_path.len() + CTM.len();
    let metadata_data = metadata_local + 30 + metadata_path.len();
    archive.splice(metadata_data..old_central, compressed_metadata);
    let central = metadata_data + compressed_metadata.len();
    archive[metadata_local + 18..metadata_local + 22]
        .copy_from_slice(&(compressed_metadata.len() as u32).to_le_bytes());
    let metadata_central = central + 46 + resource_path.len();
    archive[metadata_central + 20..metadata_central + 24]
        .copy_from_slice(&(compressed_metadata.len() as u32).to_le_bytes());
    let footer = archive.len() - 22;
    archive[footer + 16..footer + 20].copy_from_slice(&(central as u32).to_le_bytes());

    let pack = load_validated_pack_zip(&archive).unwrap();
    assert_eq!(pack.kind, PackKind::ResourcePack);
    assert_eq!(pack.pack_format, Some(15));
    assert_eq!(pack.ctm_rules.len(), 1);
    assert!(pack.warnings.is_empty());
}

#[test]
fn directory_uses_the_same_complete_router_validation() {
    let root = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = (0..1000)
        .map(|index| root.join(format!("pack-loader-{}-{index}", std::process::id())))
        .find(|path| fs::create_dir(path).is_ok())
        .expect("could not allocate isolated fixture directory");
    for (name, contents, _) in fixtures() {
        let path = directory.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    assert_loaded(load_validated_pack_dir(&directory).unwrap());

    fs::write(directory.join("hidden.jar"), b"hostile executable").unwrap();
    assert!(matches!(
        load_validated_pack_dir(&directory),
        Err(PackLoadError::Quarantined(threats)) if threats.iter().any(|issue| issue.kind == PackThreatKind::EmbeddedExecutableOrScript)
    ));
}

#[test]
fn hostile_zip_traversal_zip_bomb_and_executables_are_quarantined() {
    assert_quarantined(
        &zip(&[("../../evil.txt", b"bad", false)]),
        PackThreatKind::PathTraversal,
    );
    for executable in ["assets/mod.jar", "assets/launcher.exe"] {
        assert_quarantined(
            &zip(&[(executable, b"bad", false)]),
            PackThreatKind::EmbeddedExecutableOrScript,
        );
    }
    let mut declared_huge = zip(&[("assets/a/models/block.json", b"x", false)]);
    // The central directory is at byte 30 + name + payload. A huge declared
    // size is rejected before any allocation or extraction attempt.
    let central = 30 + "assets/a/models/block.json".len() + 1;
    declared_huge[central + 24..central + 28].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_quarantined(&declared_huge, PackThreatKind::ZipBomb);
}

#[test]
fn bad_crc_and_oversized_png_are_quarantined() {
    let mut bad_crc = zip(&[("pack.mcmeta", META15, false)]);
    bad_crc[14..18].copy_from_slice(&0u32.to_le_bytes());
    assert_quarantined(&bad_crc, PackThreatKind::EncryptedOrUnsupportedArchive);

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(13u32.to_be_bytes());
    png.extend(b"IHDR");
    png.extend(5000u32.to_be_bytes());
    png.extend(16u32.to_be_bytes());
    png.extend([8, 6, 0, 0, 0]);
    png.extend(crc32(&png[12..29]).to_be_bytes());
    assert_quarantined(
        &zip(&[("assets/minecraft/textures/block/huge.png", &png, false)]),
        PackThreatKind::TextureOomBomb,
    );
}

#[test]
fn malformed_optional_compat_content_is_reported_without_panics() {
    let mut members = fixtures();
    members.retain(|(path, _, _)| {
        !path.ends_with("example.jem") && !path.ends_with("stone.properties")
    });
    members.push((
        "assets/minecraft/optifine/cem/broken.jem",
        b"{broken",
        false,
    ));
    members.push((
        "assets/minecraft/optifine/ctm/broken.properties",
        b"method=unknown\n",
        false,
    ));
    let pack = load_validated_pack_zip(&zip(&members)).unwrap();
    assert_eq!(pack.pack_format, Some(15));
    assert!(pack.jem_models.is_empty());
    assert!(pack.ctm_rules.is_empty());
    assert_eq!(pack.warnings.len(), 2, "{:?}", pack.warnings);
}

#[test]
fn metadata_formats_and_shader_or_datapack_routing() {
    for format in [8, 9, 15, 34, 48] {
        let metadata = format!("{{\"pack\":{{\"pack_format\":{format}}}}}");
        let result = load_validated_pack_zip(&zip(&[
            ("pack.mcmeta", metadata.as_bytes(), false),
            ("data/example/recipe/test.json", b"{}", false),
        ]))
        .unwrap();
        assert_eq!(result.kind, PackKind::Datapack);
        assert_eq!(result.pack_format, Some(format));
    }
    let shader =
        load_validated_pack_zip(&zip(&[("shaders/main.fsh", b"void main(){}", false)])).unwrap();
    assert_eq!(shader.kind, PackKind::ShaderPack);
    assert_eq!(shader.pack_format, None);

    let invalid = load_validated_pack_zip(&zip(&[
        ("pack.mcmeta", br#"{"pack":{"pack_format":49}}"#, false),
        ("assets/a/optifine/ctm/test.properties", CTM, false),
    ]));
    assert!(matches!(invalid, Err(PackLoadError::InvalidPack(_))));
}

#[cfg(unix)]
#[test]
fn directory_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;
    let root = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = (0..1000)
        .map(|index| root.join(format!("pack-loader-link-{}-{index}", std::process::id())))
        .find(|path| fs::create_dir(path).is_ok())
        .expect("could not allocate isolated fixture directory");
    fs::write(directory.join("pack.mcmeta"), META15).unwrap();
    symlink("pack.mcmeta", directory.join("copy.mcmeta")).unwrap();
    assert!(matches!(
        load_validated_pack_dir(&directory),
        Err(PackLoadError::Quarantined(_))
    ));
}
