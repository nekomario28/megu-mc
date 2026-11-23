// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! In-memory ZIP fixtures. No external compressor, clock or filesystem needed.

use megu_server_files::{
    inspect_and_route_pack, PackKind, PackRouteDecision, PackRouterPolicy, PackThreatKind,
    ShaderFallbackTarget,
};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
        }
    }
    !crc
}

// Valid raw DEFLATE with an uncompressed (BTYPE=00) block.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    assert!(data.len() <= u16::MAX as usize);
    let mut out = vec![1];
    let size = data.len() as u16;
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(!size).to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn zip_entries(members: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let mut zip = Vec::new();
    let mut central = Vec::new();
    for &(name, payload, deflated) in members {
        let encoded = if deflated {
            deflate_stored(payload)
        } else {
            payload.to_vec()
        };
        let method: u16 = if deflated { 8 } else { 0 };
        let start = zip.len() as u32;
        let size = payload.len() as u32;
        let compressed = encoded.len() as u32;
        let crc = crc32(payload);
        zip.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&method.to_le_bytes());
        zip.extend_from_slice(&[0; 4]);
        zip.extend_from_slice(&crc.to_le_bytes());
        zip.extend_from_slice(&compressed.to_le_bytes());
        zip.extend_from_slice(&size.to_le_bytes());
        zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(name.as_bytes());
        zip.extend_from_slice(&encoded);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&compressed.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&start.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = zip.len() as u32;
    let cd_size = central.len() as u32;
    zip.extend(central);
    zip.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    zip.extend_from_slice(&[0; 4]);
    zip.extend_from_slice(&(members.len() as u16).to_le_bytes());
    zip.extend_from_slice(&(members.len() as u16).to_le_bytes());
    zip.extend_from_slice(&cd_size.to_le_bytes());
    zip.extend_from_slice(&cd_offset.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip
}

fn zip(members: &[(&str, &[u8])]) -> Vec<u8> {
    zip_entries(
        &members
            .iter()
            .map(|&(name, payload)| (name, payload, false))
            .collect::<Vec<_>>(),
    )
}

fn cd_offset(zip: &[u8]) -> usize {
    let field = &zip[zip.len() - 6..zip.len() - 2];
    u32::from_le_bytes(field.try_into().unwrap()) as usize
}

fn replace_raw_deflate(name: &str, original: &[u8], compressed: &[u8]) -> Vec<u8> {
    let mut archive = zip_entries(&[(name, original, true)]);
    let old_central = cd_offset(&archive);
    let data_start = 30 + name.len();
    archive.splice(data_start..old_central, compressed.iter().copied());
    let central = data_start + compressed.len();
    put_u32(&mut archive, 18, compressed.len() as u32);
    put_u32(&mut archive, central + 20, compressed.len() as u32);
    let footer = archive.len() - 22;
    put_u32(&mut archive, footer + 16, central as u32);
    archive
}

fn put_u32(zip: &mut [u8], at: usize, value: u32) {
    zip[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
    data.extend_from_slice(&13u32.to_be_bytes());
    data.extend_from_slice(b"IHDR");
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&height.to_be_bytes());
    data.extend_from_slice(&[8, 6, 0, 0, 0]);
    let crc = crc32(&data[12..29]);
    data.extend_from_slice(&crc.to_be_bytes());
    data
}

fn report(bytes: &[u8]) -> megu_server_files::PackRoutingReport {
    inspect_and_route_pack(bytes, &PackRouterPolicy::default())
}

fn assert_threat(bytes: &[u8], expected: PackThreatKind) {
    match report(bytes).decision {
        PackRouteDecision::Quarantine { threats } => {
            assert!(
                threats.iter().any(|threat| threat.kind == expected),
                "threats: {threats:?}"
            );
        }
        result => panic!("expected quarantine ({expected:?}), got {result:?}"),
    }
}

#[test]
fn resource_ctm_emissive_fonts_and_sounds_route() {
    let texture = png(16, 16);
    let pack = zip(&[
        ("pack.mcmeta", br#"{"pack":{"pack_format":34}}"#),
        ("assets/minecraft/textures/block/stone.png", &texture),
        (
            "assets/minecraft/optifine/ctm/stone.properties",
            b"matchBlocks=stone",
        ),
        ("assets/minecraft/textures/block/stone_e.png", &texture),
        ("assets/minecraft/font/default.json", b"{}"),
        ("assets/minecraft/sounds/new.ogg", b"OggS"),
    ]);
    let got = report(&pack);
    assert_eq!(got.kind, PackKind::ResourcePack);
    assert!(got.capabilities.has_pack_mcmeta);
    assert!(got.capabilities.has_core_textures);
    assert!(got.capabilities.has_ctm_connected_textures);
    assert!(got.capabilities.has_emissive_overlays);
    assert!(got.capabilities.has_custom_fonts);
    assert!(got.capabilities.has_custom_sounds);
    assert!(matches!(
        got.decision,
        PackRouteDecision::RoutePack {
            kind: PackKind::ResourcePack,
            shader_route: None,
            ..
        }
    ));
}

#[test]
fn shader_bridge_is_gpu_policy_dependent() {
    let pack = zip(&[
        ("shaders/program/main.fsh", b"void main() {}"),
        ("shaders/program/main.vsh", b"void main() {}"),
    ]);
    let high = report(&pack);
    assert!(high.capabilities.has_optifine_iris_shaders);
    assert!(matches!(
        high.decision,
        PackRouteDecision::RoutePack {
            kind: PackKind::ShaderPack,
            shader_route: Some(ShaderFallbackTarget::RouteOptiFineIrisBridge),
            ..
        }
    ));
    let low = inspect_and_route_pack(
        &pack,
        &PackRouterPolicy {
            gpu_supports_optifine_bridge: false,
            ..PackRouterPolicy::default()
        },
    );
    assert!(matches!(
        low.decision,
        PackRouteDecision::RoutePack {
            kind: PackKind::ShaderPack,
            shader_route: Some(ShaderFallbackTarget::FallbackBuiltinJapaneseWgsl),
            ..
        }
    ));
}

#[test]
fn wgsl_is_preferred_and_hybrid_capabilities_work() {
    let pack = zip(&[
        ("shaders/program/main.fsh", b"void main() {}"),
        ("shaders/program/main.wgsl", b"@vertex fn main() {}"),
        ("data/ns/recipe/stone.json", b"{}"),
    ]);
    let got = inspect_and_route_pack(
        &pack,
        &PackRouterPolicy {
            gpu_supports_optifine_bridge: false,
            ..PackRouterPolicy::default()
        },
    );
    assert!(got.capabilities.has_wgsl_shaders);
    assert_eq!(got.kind, PackKind::HybridPack);
    assert!(matches!(
        got.decision,
        PackRouteDecision::RoutePack {
            shader_route: Some(ShaderFallbackTarget::UsePackWgsl),
            ..
        }
    ));
}

#[test]
fn recipes_and_worldgen_are_datapack() {
    let pack = zip(&[
        ("data/ns/recipes/sword.json", b"{}"),
        ("data/ns/worldgen/biome/land.json", b"{}"),
        ("data/ns/dimension/realm.json", b"{}"),
    ]);
    let got = report(&pack);
    assert_eq!(got.kind, PackKind::Datapack);
    assert!(got.capabilities.has_datapack_worldgen_or_recipes);
    assert!(matches!(
        got.decision,
        PackRouteDecision::RoutePack {
            kind: PackKind::Datapack,
            ..
        }
    ));
}

#[test]
fn png_ihdr_bomb_detected_even_when_only_50_bytes() {
    let mut payload = png(65_535, 65_535);
    payload.resize(50, 0);
    let pack = zip(&[("assets/ns/textures/huge.png", &payload)]);
    assert_threat(&pack, PackThreatKind::TextureOomBomb);
}

#[test]
fn policy_tightens_texture_dimensions_and_pixels() {
    let texture = png(128, 128);
    let pack = zip(&[("assets/ns/textures/large.png", &texture)]);
    let got = inspect_and_route_pack(
        &pack,
        &PackRouterPolicy {
            max_texture_dim: 256,
            max_texture_pixels: 10_000,
            ..PackRouterPolicy::default()
        },
    );
    assert!(
        matches!(got.decision, PackRouteDecision::Quarantine { threats }
        if threats.iter().any(|t| t.kind == PackThreatKind::TextureOomBomb))
    );
}

#[test]
fn high_ratio_and_huge_declared_uncompressed_size_are_zip_bombs() {
    let mut pack = zip(&[("pack.mcmeta", b"tiny")]);
    let cd = cd_offset(&pack);
    put_u32(&mut pack, cd + 24, 100_001);
    assert_threat(&pack, PackThreatKind::ZipBomb);

    let mut huge = zip(&[("data/ns/recipe/x.json", b"abc")]);
    let cd = cd_offset(&huge);
    put_u32(&mut huge, cd + 24, 600 * 1024 * 1024);
    assert_threat(&huge, PackThreatKind::ZipBomb);
}

#[test]
fn path_traversal_absolute_backslash_drive_and_nul_are_blocked() {
    for name in [
        "../../etc/passwd",
        "/root/.ssh/id_rsa",
        "assets\\ns\\textures\\a.png",
        "C:/Windows/system.ini",
        "data/./ns/function/a.mcfunction",
        "data/../ns/function/a.mcfunction",
        "data/ns/evil\0name",
        "data//ns/function/a.mcfunction",
    ] {
        assert_threat(&zip(&[(name, b"x")]), PackThreatKind::PathTraversal);
    }
}

#[test]
fn glsl_include_mutual_recursion_is_blocked() {
    let pack = zip(&[
        ("shaders/a.glsl", b"#include \"b.glsl\"\n"),
        ("shaders/b.glsl", b"#include \"a.glsl\"\n"),
        ("shaders/program/main.fsh", b"#include \"../a.glsl\"\n"),
    ]);
    assert_threat(&pack, PackThreatKind::ShaderIncludeCycle);
}

#[test]
fn glsl_include_depth_over_sixteen_is_blocked() {
    let names: Vec<_> = (0..17).map(|i| format!("shaders/{i}.glsl")).collect();
    let bodies: Vec<_> = (0..17)
        .map(|i| {
            if i == 16 {
                String::new()
            } else {
                format!("#include \"{}.glsl\"\n", i + 1)
            }
        })
        .collect();
    let files: Vec<_> = names
        .iter()
        .zip(&bodies)
        .map(|(name, text)| (name.as_str(), text.as_bytes()))
        .collect();
    assert_threat(&zip(&files), PackThreatKind::ShaderIncludeCycle);
}

#[test]
fn datapack_function_recursion_is_blocked() {
    let pack = zip(&[
        ("data/ns/function/a.mcfunction", b"function ns:b\n"),
        ("data/ns/function/b.mcfunction", b"function ns:a\n"),
    ]);
    assert_threat(&pack, PackThreatKind::DatapackFunctionCycle);
}

#[test]
fn datapack_function_excess_fanout_is_blocked() {
    let mut files = vec![("data/ns/function/a.mcfunction".to_owned(), Vec::new())];
    for i in 0..129 {
        files[0]
            .1
            .extend_from_slice(format!("function ns:f{i}\n").as_bytes());
        files.push((
            format!("data/ns/function/f{i}.mcfunction"),
            b"# no-op".to_vec(),
        ));
    }
    let members: Vec<_> = files
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_slice()))
        .collect();
    assert_threat(&zip(&members), PackThreatKind::DatapackFunctionCycle);

    let external = b"function other:external\n".repeat(129);
    assert_threat(
        &zip(&[("data/ns/function/dispatcher.mcfunction", &external)]),
        PackThreatKind::DatapackFunctionCycle,
    );
}

#[test]
fn utf8_bom_does_not_hide_static_function_cycles() {
    let pack = zip(&[
        (
            "data/ns/function/a.mcfunction",
            b"\xef\xbb\xbffunction ns:b\n",
        ),
        ("data/ns/function/b.mcfunction", b"function ns:a\n"),
    ]);
    assert_threat(&pack, PackThreatKind::DatapackFunctionCycle);
}

#[test]
fn embedded_executables_are_quarantined_regardless_of_case() {
    for name in [
        "assets/ns/textures/evil.DLL",
        "assets/ns/optifine/ctm/script.class",
        "assets/ns/textures/patch.sh",
        "assets/ns/runner.jar",
        "data/ns/f.sh",
    ] {
        assert_threat(
            &zip(&[(name, b"binary")]),
            PackThreatKind::EmbeddedExecutableOrScript,
        );
    }
}

#[test]
fn deflated_members_are_inspected_for_shaders_functions_and_png() {
    let texture = png(65_535, 65_535);
    let bomb = zip_entries(&[("assets/ns/textures/huge.png", &texture, true)]);
    assert_threat(&bomb, PackThreatKind::TextureOomBomb);

    let cycle = zip_entries(&[
        ("data/ns/functions/a.mcfunction", b"function ns:b\n", true),
        ("data/ns/functions/b.mcfunction", b"function ns:a\n", true),
    ]);
    assert_threat(&cycle, PackThreatKind::DatapackFunctionCycle);

    let fine = zip_entries(&[(
        "shaders/shaders.properties",
        b"sliders=shadowDistance",
        true,
    )]);
    assert!(matches!(
        report(&fine).decision,
        PackRouteDecision::RoutePack {
            kind: PackKind::ShaderPack,
            ..
        }
    ));
}

#[test]
fn rfc1951_fixed_and_dynamic_huffman_blocks_are_verified() {
    // Produced by Python zlib (wbits=-15) and embedded as fixed test vectors.
    // The first stream uses BTYPE=01, the second BTYPE=10.
    let fixed = [
        0x2b, 0xcb, 0xcf, 0x4c, 0x51, 0xc8, 0x4d, 0xcc, 0xcc, 0xd3, 0xd0, 0x54, 0xa8, 0xae, 0xe5,
        0x2a, 0x1b, 0xe5, 0xe2, 0xe6, 0x02, 0x00,
    ];
    let fixed_source = b"void main() {}\n".repeat(20);
    let fine = replace_raw_deflate("shaders/a.fsh", &fixed_source, &fixed);
    assert!(matches!(
        report(&fine).decision,
        PackRouteDecision::RoutePack {
            kind: PackKind::ShaderPack,
            ..
        }
    ));

    let dynamic = [
        0x2d, 0xcb, 0xb9, 0x11, 0x00, 0x31, 0x08, 0x03, 0xc0, 0xdc, 0x55, 0xa8, 0x03, 0x10, 0xe0,
        0xe7, 0xfa, 0x6f, 0xec, 0x86, 0x11, 0xd1, 0x46, 0x6b, 0x06, 0x87, 0x2f, 0x33, 0x10, 0xd9,
        0x04, 0x4e, 0x93, 0xf8, 0x9a, 0x02, 0xa3, 0xdd, 0xe0, 0x6e, 0x0f, 0xf8, 0x5a, 0x47, 0xe8,
        0x6c, 0x9d, 0xa7, 0x43, 0x4e, 0xaa, 0x49, 0x77, 0x92, 0x4e, 0xe9, 0x5c, 0x1d, 0xfa, 0xa4,
        0x9c, 0x74, 0x94, 0x7c, 0xfd,
    ];
    let dynamic_source = (0..20)
        .map(|i| format!("// {} {}\n", i % 7, (i * 3) % 19))
        .collect::<String>();
    let compressed = replace_raw_deflate("shaders/main.fsh", dynamic_source.as_bytes(), &dynamic);
    assert!(matches!(
        report(&compressed).decision,
        PackRouteDecision::RoutePack {
            kind: PackKind::ShaderPack,
            ..
        }
    ));
    let mut corrupt = dynamic;
    corrupt[0] = 0xff; // reserved DEFLATE block type
    assert_threat(
        &replace_raw_deflate("shaders/main.fsh", dynamic_source.as_bytes(), &corrupt),
        PackThreatKind::EncryptedOrUnsupportedArchive,
    );

    let mut trailing = dynamic.to_vec();
    trailing.extend_from_slice(&[0xaa, 0xbb]);
    assert_threat(
        &replace_raw_deflate("shaders/main.fsh", dynamic_source.as_bytes(), &trailing),
        PackThreatKind::EncryptedOrUnsupportedArchive,
    );
}

#[test]
fn damaged_crc_and_local_header_disagreement_are_quarantined() {
    let mut pack = zip(&[("pack.mcmeta", b"content")]);
    pack[30 + "pack.mcmeta".len()] ^= 0x40;
    assert_threat(&pack, PackThreatKind::EncryptedOrUnsupportedArchive);

    let mut mismatch = zip(&[("pack.mcmeta", b"content")]);
    put_u32(&mut mismatch, 14, 7);
    assert_threat(&mismatch, PackThreatKind::EncryptedOrUnsupportedArchive);
}

#[test]
fn unsupported_methods_encrypted_flags_symlinks_and_duplicate_names_quarantine() {
    let mut method = zip(&[("pack.mcmeta", b"content")]);
    let cd = cd_offset(&method);
    method[8..10].copy_from_slice(&99u16.to_le_bytes());
    method[cd + 10..cd + 12].copy_from_slice(&99u16.to_le_bytes());
    assert_threat(&method, PackThreatKind::EncryptedOrUnsupportedArchive);

    let mut encrypted = zip(&[("pack.mcmeta", b"content")]);
    let cd = cd_offset(&encrypted);
    encrypted[6..8].copy_from_slice(&1u16.to_le_bytes());
    encrypted[cd + 8..cd + 10].copy_from_slice(&1u16.to_le_bytes());
    assert_threat(&encrypted, PackThreatKind::EncryptedOrUnsupportedArchive);

    let mut link = zip(&[("pack.mcmeta", b"content")]);
    let cd = cd_offset(&link);
    link[cd + 5] = 3; // created by Unix
    put_u32(&mut link, cd + 38, 0xa000 << 16); // symlink file mode
    assert_threat(&link, PackThreatKind::EncryptedOrUnsupportedArchive);

    assert_threat(
        &zip(&[("a.txt", b"a"), ("A.txt", b"b")]),
        PackThreatKind::EncryptedOrUnsupportedArchive,
    );
}

#[test]
fn a_data_descriptor_is_checked_against_central_metadata() {
    let mut pack = zip(&[("pack.mcmeta", b"x")]);
    let cd = cd_offset(&pack);
    let crc = crc32(b"x");
    let mut descriptor = Vec::new();
    descriptor.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
    descriptor.extend_from_slice(&crc.to_le_bytes());
    descriptor.extend_from_slice(&1u32.to_le_bytes());
    descriptor.extend_from_slice(&1u32.to_le_bytes());
    pack.splice(cd..cd, descriptor);
    let new_cd = cd + 16;
    pack[6..8].copy_from_slice(&8u16.to_le_bytes());
    pack[new_cd + 8..new_cd + 10].copy_from_slice(&8u16.to_le_bytes());
    put_u32(&mut pack, 14, 0);
    put_u32(&mut pack, 18, 0);
    put_u32(&mut pack, 22, 0);
    let footer = pack.len() - 22;
    put_u32(&mut pack, footer + 16, new_cd as u32);
    assert!(matches!(
        report(&pack).decision,
        PackRouteDecision::RoutePack { .. }
    ));

    pack[cd + 8] ^= 0x80;
    assert_threat(&pack, PackThreatKind::EncryptedOrUnsupportedArchive);
}

#[test]
fn truncated_and_random_bytes_never_route_or_panic() {
    for bad in [
        &b""[..],
        &b"PK"[..],
        &b"garbage bytes!"[..],
        &b"PK\x05\x06"[..],
    ] {
        assert_threat(bad, PackThreatKind::EncryptedOrUnsupportedArchive);
    }
    let valid = zip(&[("pack.mcmeta", b"content")]);
    for cut in 0..valid.len() {
        assert!(
            matches!(
                report(&valid[..cut]).decision,
                PackRouteDecision::Quarantine { .. }
            ),
            "truncated at {cut}"
        );
    }
    let mut seed = 0x1234_5678u32;
    for size in 0..128 {
        let mut bytes = vec![0; size];
        for byte in &mut bytes {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (seed >> 24) as u8;
        }
        assert!(matches!(
            report(&bytes).decision,
            PackRouteDecision::Quarantine { .. }
        ));
    }
}

#[test]
fn zip_stored_and_deflated_zero_length_members_are_valid() {
    let zip = zip_entries(&[
        ("data/ns/recipe/empty.json", b"", false),
        ("shaders/empty.wgsl", b"", true),
    ]);
    assert!(matches!(
        report(&zip).decision,
        PackRouteDecision::RoutePack {
            kind: PackKind::HybridPack,
            shader_route: Some(ShaderFallbackTarget::UsePackWgsl),
            ..
        }
    ));
}
