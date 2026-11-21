// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Unit and integration tests for mc-spine-client (CLI, redaction, Protocol 767,
//! RFC 1950 zlib, chunk decode -> client-port greedy meshing + vertex packing,
//! and client-kernel physics prediction).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;
use std::time::Duration;

use mc_spine_client::cli::{parse_args, parse_host_port, redact_from_argv, ScriptMode};
use mc_spine_client::protocol::{
    decode_chunk_packet, decode_packet_body, decode_section_blocks_update, encode_packet,
    offline_uuid, pack_position, parse_uuid, unpack_position, write_string, write_varint,
    write_varlong, zlib_compress_stored, zlib_decompress, DimensionMeta, PlayEvent, ProtocolClient,
    TeleportSync, WireReader,
};
use mc_spine_client::sim::{PlayerPhysics, TickInput};
use mc_spine_client::world::{pack_vertices, sort_translucent, BlockTable, WorldState};
use mc_spine_client_mesh::ffi::PackedVertex;

#[test]
fn cli_launcher_flags_and_token_redaction() {
    let secret_tok = "eyJhbGciOiJSUzI1NiJ9.SUPER_SECRET_TOKEN_VALUE";
    let secret_xuid = "253549999888777";
    let secret_ctok = "CLIENT_TOKEN_XYZ_987";
    let argv = vec![
        "--username",
        "Steve",
        "--uuid",
        "069a79f4-44e9-4726-a5be-fca90e38aaf5",
        "--accessToken",
        secret_tok,
        "--clientToken=CLIENT_TOKEN_XYZ_987",
        "--clientId",
        "launcher-cid",
        "--xuid",
        secret_xuid,
        "--userType",
        "msa",
        "--version",
        "1.21.1-mc-spine",
        "--versionType",
        "release",
        "--gameDir",
        "/tmp/game",
        "--assetsDir",
        "/tmp/assets",
        "--assetIndex",
        "17",
        "--quickPlayMultiplayer",
        "127.0.0.1:25569",
        "--headless",
        "--ticks",
        "25",
        "--view-distance",
        "4",
        "--script",
        "walk",
        "--json-summary",
    ];
    let cfg = parse_args(&argv).expect("parse launcher args");
    assert_eq!(cfg.username, "Steve");
    assert_eq!(
        cfg.target_endpoint().unwrap(),
        Some(("127.0.0.1".into(), 25569))
    );
    assert!(cfg.headless);
    assert_eq!(cfg.ticks, 25);
    assert_eq!(cfg.view_distance, 4);
    assert_eq!(cfg.script, ScriptMode::Walk);
    assert!(cfg.json_summary);
    assert!(cfg.has_access_token());
    assert!(cfg.has_xuid());

    // Debug representation must never leak any secret.
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains(secret_tok));
    assert!(!dbg.contains(secret_xuid));
    assert!(!dbg.contains(secret_ctok));
    assert!(dbg.contains("<REDACTED>"));

    // Arbitrary error strings and Bearer headers must be sanitized.
    let raw_err = format!("failed auth with {secret_tok}, xuid={secret_xuid}, Bearer abc.def.ghi");
    let clean = cfg.redact(&raw_err);
    assert!(!clean.contains(secret_tok));
    assert!(!clean.contains(secret_xuid));
    assert!(!clean.contains("abc.def.ghi"));
    assert!(clean.contains("Bearer <REDACTED>"));

    // Parse error referencing a secret must also be redacted.
    let bad_argv = vec!["--accessToken", secret_tok, secret_tok];
    let err = parse_args(&bad_argv).unwrap_err();
    let clean_err = redact_from_argv(&err, &bad_argv);
    assert!(!err.contains(secret_tok));
    assert!(!clean_err.contains(secret_tok));
    assert!(err.contains("<REDACTED>"));
}

#[test]
fn host_port_parsing() {
    assert_eq!(
        parse_host_port("127.0.0.1:25565", 25565).unwrap(),
        ("127.0.0.1".into(), 25565)
    );
    assert_eq!(
        parse_host_port("example.com", 25570).unwrap(),
        ("example.com".into(), 25570)
    );
    assert_eq!(
        parse_host_port("[::1]:25577", 25565).unwrap(),
        ("::1".into(), 25577)
    );
}

#[test]
fn wire_varint_varlong_position_uuid() {
    for val in [0, 1, 127, 128, 767, 26_684, -1, -2147483648, 2147483647] {
        let mut buf = Vec::new();
        write_varint(&mut buf, val);
        let mut r = WireReader::new(&buf);
        assert_eq!(r.read_varint().unwrap(), val);
        assert_eq!(r.remaining(), 0);
    }

    for val in [0i64, 1, -1, 123_456_789_012, i64::MIN, i64::MAX] {
        let mut buf = Vec::new();
        write_varlong(&mut buf, val);
        let mut r = WireReader::new(&buf);
        assert_eq!(r.read_varlong().unwrap(), val);
        assert_eq!(r.remaining(), 0);
    }

    for pos in [
        [0, 0, 0],
        [-1, -64, -1],
        [12345, 319, -54321],
        [-33554432, -2048, 33554431],
    ] {
        let packed = pack_position(pos);
        assert_eq!(unpack_position(packed), pos);
    }

    let u = offline_uuid("WireTester");
    assert_eq!(u[6] >> 4, 3);
    assert_eq!(u[8] >> 6, 2);
    assert_eq!(
        parse_uuid("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap(),
        parse_uuid("069a79f444e94726a5befca90e38aaf5").unwrap()
    );
}

#[test]
fn rfc1950_zlib_stored_and_huffman_and_packet_framing() {
    // 1. Stored-block roundtrips (empty, small, >65535 multi-block)
    for size in [0usize, 1, 17, 4096, 70_000] {
        let data: Vec<u8> = (0..size).map(|i| ((i * 31 + 7) & 0xff) as u8).collect();
        let compressed = zlib_compress_stored(&data);
        let restored = zlib_decompress(&compressed, size + 16).expect("decompress stored");
        assert_eq!(restored, data);
    }

    // 2. Real fixed-Huffman RFC 1950 zlib stream ("hello mc-spine! " * 8, Python zlib.compress)
    let fixed_zlib: &[u8] = &[
        0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x4d, 0xd6, 0x2d, 0x2e, 0xc8, 0xcc,
        0x4b, 0x55, 0x54, 0xc8, 0xa0, 0x33, 0x1f, 0x00, 0x53, 0xfd, 0x2c, 0x89,
    ];
    let decoded = zlib_decompress(fixed_zlib, 1024).expect("decompress fixed Huffman zlib");
    assert_eq!(decoded, b"hello mc-spine! ".repeat(8));

    // 3. Real dynamic-Huffman RFC 1950 zlib stream (BTYPE=2, Python zlib.compress level=9)
    let dynamic_zlib: &[u8] = &[
        0x78, 0xda, 0xd5, 0x8d, 0x5d, 0x0e, 0xc2, 0x20, 0x10, 0x06, 0xaf, 0xb2, 0x47, 0xf0, 0xb7,
        0xa8, 0x6f, 0xed, 0x45, 0x1a, 0x64, 0x3f, 0xea, 0x46, 0x60, 0x09, 0xd0, 0x5a, 0x6f, 0x6f,
        0x3d, 0x81, 0xcf, 0x3e, 0x4e, 0x32, 0x93, 0x89, 0x92, 0xe0, 0x8a, 0xf5, 0xed, 0xa6, 0x0b,
        0xca, 0x4b, 0x4b, 0x60, 0xca, 0x45, 0x9b, 0x3a, 0x0d, 0x64, 0x3a, 0x43, 0x53, 0x01, 0xf8,
        0x4d, 0x11, 0xf5, 0x21, 0x69, 0xa2, 0x4d, 0x6a, 0x58, 0x29, 0x5b, 0xf7, 0xfc, 0x62, 0xfc,
        0xe7, 0x7c, 0xb7, 0x3f, 0x1c, 0x4f, 0xe7, 0xce, 0x5c, 0xae, 0xf6, 0xee, 0x18, 0xfe, 0x17,
        0xf7, 0xfd, 0x30, 0x90, 0x0f, 0xb3, 0xf0, 0x28, 0x31, 0xcf, 0xa1, 0x82, 0xb6, 0x51, 0x10,
        0x06, 0x55, 0xb8, 0x26, 0x9a, 0x46, 0x49, 0x8c, 0xf5, 0x03, 0x19, 0x1a, 0x7b, 0x13,
    ];
    let mut expected_dyn =
        b"minecraft:overworld protocol 767 greedy meshing vertex packing ".repeat(4);
    expected_dyn.extend_from_slice(&b"0123456789abcdef".repeat(3));
    expected_dyn.extend_from_slice(b"AABB fluid_impulse collide section_index");
    let decoded_dyn = zlib_decompress(dynamic_zlib, 1024).expect("decompress dynamic Huffman zlib");
    assert_eq!(decoded_dyn, expected_dyn);

    // 4. Uncompressed and compressed Protocol 767 frame roundtrips
    let payload = vec![0x42u8; 512];
    for comp in [None, Some(256), Some(1024), Some(0)] {
        let frame = encode_packet(0x27, &payload, comp);
        let mut r = WireReader::new(&frame);
        let len = r.read_varint().unwrap() as usize;
        let body = r.take(len).unwrap();
        let (pid, out) = decode_packet_body(body, comp).unwrap();
        assert_eq!(pid, 0x27);
        assert_eq!(out, payload);
    }
}

#[test]
fn protocol767_compressed_handshake_mock_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();

    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        let read_pkt = |s: &mut std::net::TcpStream, comp: Option<usize>| -> (i32, Vec<u8>) {
            let mut len = 0u32;
            for i in 0..5 {
                let mut b = [0u8; 1];
                s.read_exact(&mut b).unwrap();
                len |= u32::from(b[0] & 0x7f) << (7 * i);
                if b[0] & 0x80 == 0 {
                    break;
                }
            }
            let mut body = vec![0u8; len as usize];
            s.read_exact(&mut body).unwrap();
            decode_packet_body(&body, comp).unwrap()
        };

        // Handshake (0x00) + Login Start (0x00)
        let (pid, _) = read_pkt(&mut sock, None);
        assert_eq!(pid, 0x00);
        let (pid, _) = read_pkt(&mut sock, None);
        assert_eq!(pid, 0x00);

        // Send Set Compression (0x03) with threshold=16
        let mut sc = Vec::new();
        write_varint(&mut sc, 16);
        sock.write_all(&encode_packet(0x03, &sc, None)).unwrap();
        let comp = Some(16usize);

        // Send Login Success (0x02) compressed
        let mut ls = Vec::new();
        ls.extend_from_slice(&offline_uuid("Tester"));
        write_string(&mut ls, "Tester");
        write_varint(&mut ls, 0);
        ls.push(0);
        sock.write_all(&encode_packet(0x02, &ls, comp)).unwrap();

        // Expect Login Ack (0x03)
        let (pid, _) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x03);

        // Configuration: KeepAlive (0x04), Known Packs (0x0E), Finish Config (0x03)
        sock.write_all(&encode_packet(0x04, &42i64.to_be_bytes(), comp))
            .unwrap();
        let (pid, ka) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x04);
        assert_eq!(ka, 42i64.to_be_bytes());

        let mut kp = Vec::new();
        write_varint(&mut kp, 0);
        sock.write_all(&encode_packet(0x0e, &kp, comp)).unwrap();

        let (pid, _) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x00); // Client Info
        let (pid, _) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x07); // Known Packs

        sock.write_all(&encode_packet(0x03, &[], comp)).unwrap();
        let (pid, _) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x03); // Finish Config Ack

        // Play: KeepAlive (0x26) + Teleport (0x40)
        sock.write_all(&encode_packet(0x26, &99i64.to_be_bytes(), comp))
            .unwrap();
        let mut tp = Vec::new();
        tp.extend_from_slice(&10.5f64.to_be_bytes());
        tp.extend_from_slice(&65.0f64.to_be_bytes());
        tp.extend_from_slice(&(-4.5f64).to_be_bytes());
        tp.extend_from_slice(&0.0f32.to_be_bytes());
        tp.extend_from_slice(&0.0f32.to_be_bytes());
        tp.push(0);
        write_varint(&mut tp, 7);
        sock.write_all(&encode_packet(0x40, &tp, comp)).unwrap();

        let (pid, body) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x18);
        assert_eq!(body, 99i64.to_be_bytes());
        let (pid, body) = read_pkt(&mut sock, comp);
        assert_eq!(pid, 0x00);
        assert_eq!(WireReader::new(&body).read_varint().unwrap(), 7);
    });

    let (mut cli, report) = ProtocolClient::connect(
        "127.0.0.1",
        port,
        "Tester",
        offline_uuid("Tester"),
        2,
        Duration::from_secs(5),
    )
    .expect("connect with compression");
    assert!(report.login_ok && report.config_ok);
    assert_eq!(report.compression_threshold, Some(16));

    let ev1 = cli.recv_play(Duration::from_secs(2)).unwrap().unwrap();
    match ev1 {
        PlayEvent::KeepAlive(id) => {
            assert_eq!(id, 99);
            cli.send_keep_alive(id).unwrap();
        }
        other => panic!("expected KeepAlive, got {other:?}"),
    }
    let ev2 = cli.recv_play(Duration::from_secs(2)).unwrap().unwrap();
    match ev2 {
        PlayEvent::Teleport(tp) => {
            assert_eq!(tp.teleport_id, 7);
            assert_eq!((tp.x, tp.y, tp.z), (10.5, 65.0, -4.5));
            cli.send_confirm_teleport(tp.teleport_id).unwrap();
        }
        other => panic!("expected Teleport, got {other:?}"),
    }
    server_thread.join().unwrap();
}

fn build_synthetic_chunk_packet(cx: i32, cz: i32, section_count: usize) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&cx.to_be_bytes());
    body.extend_from_slice(&cz.to_be_bytes());
    // Minimal unnamed Compound NBT heightmap: TAG_Compound (10) + TAG_End (0)
    body.push(10);
    body.push(0);

    let mut sec_bytes = Vec::new();
    for s in 0..section_count {
        if s == 4 {
            // Section y = 0..15: indirect 4-bit palette [air=0, stone=1, water=80]
            // Layer y=0 -> stone (256 cells), Layer y=1 -> water (256 cells), rest air.
            sec_bytes.extend_from_slice(&512i16.to_be_bytes());
            sec_bytes.push(4); // 4 bits/entry
            write_varint(&mut sec_bytes, 3); // palette len
            write_varint(&mut sec_bytes, 0); // idx 0 = air
            write_varint(&mut sec_bytes, 1); // idx 1 = stone
            write_varint(&mut sec_bytes, 80); // idx 2 = water
            let words_count: i32 = 4096 / 16; // 256 u64s
            write_varint(&mut sec_bytes, words_count);
            for w in 0..words_count {
                let val: u64 = if w < 16 {
                    // y=0: all 16 entries = palette index 1 (stone)
                    0x1111_1111_1111_1111
                } else if w < 32 {
                    // y=1: all 16 entries = palette index 2 (water)
                    0x2222_2222_2222_2222
                } else {
                    0
                };
                sec_bytes.extend_from_slice(&val.to_be_bytes());
            }
        } else {
            // Uniform air section
            sec_bytes.extend_from_slice(&0i16.to_be_bytes());
            sec_bytes.push(0); // 0 bits single-valued
            write_varint(&mut sec_bytes, 0); // air
            write_varint(&mut sec_bytes, 0); // 0 longs
        }
        // Biomes: single value biome 0
        sec_bytes.push(0);
        write_varint(&mut sec_bytes, 0);
        write_varint(&mut sec_bytes, 0);
    }

    write_varint(&mut body, sec_bytes.len() as i32);
    body.extend_from_slice(&sec_bytes);
    write_varint(&mut body, 0); // 0 block entities

    // Light masks: provide sky light for light section 5 (section index 4), empty block light
    write_varint(&mut body, 1);
    body.extend_from_slice(&(1u64 << 5).to_be_bytes()); // sky_mask
    write_varint(&mut body, 0); // block_mask
    write_varint(&mut body, 0); // empty_sky_mask
    write_varint(&mut body, 0); // empty_block_mask
    write_varint(&mut body, 1); // 1 sky array
    write_varint(&mut body, 2048);
    body.extend_from_slice(&[0xffu8; 2048]);
    write_varint(&mut body, 0); // 0 block arrays
    body
}

#[test]
fn chunk_decode_greedy_mesh_pack_vertices_and_sort_translucent() {
    assert_eq!(std::mem::size_of::<PackedVertex>(), 16);

    let json_path = Path::new("../assets/block_states_1211.json");
    let blocks = if json_path.is_file() {
        let t = BlockTable::from_json_file(json_path).expect("load block_states_1211.json");
        assert!(t.loaded_from_json);
        t
    } else {
        BlockTable::builtin()
    };

    let raw = build_synthetic_chunk_packet(0, 0, 24);
    let decoded = decode_chunk_packet(&raw, 24, 64).expect("decode synthetic chunk");
    assert_eq!(decoded.cx, 0);
    assert_eq!(decoded.cz, 0);
    assert_eq!(decoded.sections.len(), 24);

    let mut world = WorldState::new(DimensionMeta::default(), blocks);
    world.apply_chunk(decoded).expect("apply chunk");
    assert_eq!(world.chunks_decoded, 1);
    assert_eq!(world.get_block_state(5, 0, 5), Some(1));
    assert_eq!(world.get_block_state(5, 1, 5), Some(80));
    assert_eq!(world.get_block_state(5, 2, 5), Some(0));

    let rebuilt = world
        .rebuild_dirty_bounded(32, [8.0, 4.0, 8.0])
        .expect("rebuild dirty");
    assert!(rebuilt >= 1);
    assert!(world.metrics.total_quads > 0);
    assert_eq!(world.metrics.packed_vertices, world.metrics.total_quads * 4);
    assert!(world.metrics.translucent_quads > 0);

    // Block update (0x09) + section blocks update (0x49) dirty and remesh cleanly.
    world.apply_block_update([4, 2, 4], 1).unwrap();
    assert_eq!(world.get_block_state(4, 2, 4), Some(1));
    let mut sb = Vec::new();
    let sec_pos: i64 = 0; // (0, 0, 0)
    sb.extend_from_slice(&sec_pos.to_be_bytes());
    write_varint(&mut sb, 1);
    let rec: i64 = (1i64 << 12) | (3 << 8) | (3 << 4) | 2; // x=3, z=3, y=2 -> stone
    write_varlong(&mut sb, rec);
    let updates = decode_section_blocks_update(&sb).unwrap();
    assert_eq!(updates, vec![([3, 2, 3], 1)]);
    for (pos, st) in updates {
        world.apply_block_update(pos, st).unwrap();
    }
    assert!(world.pending_mesh_sections() > 0);
    world.rebuild_dirty_bounded(32, [8.0, 4.0, 8.0]).unwrap();
    assert_eq!(world.metrics.packed_vertices, world.metrics.total_quads * 4);

    // Direct pack_vertices & sort_translucent invariants on empty slice.
    assert!(pack_vertices(&[]).unwrap().is_empty());
    assert!(sort_translucent(&[], [0.0, 0.0, 0.0]).unwrap().is_empty());
}

#[test]
fn physics_prediction_gravity_wall_and_water_fluid_impulse() {
    let raw = build_synthetic_chunk_packet(0, 0, 24);
    let decoded = decode_chunk_packet(&raw, 24, 64).unwrap();
    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    world.apply_chunk(decoded).unwrap();
    // Clear water at (8, 1, 8) area to create dry stone floor at y=0, plus wall at z=10
    for x in 6..=10 {
        for z in 6..=11 {
            world.apply_block_update([x, 1, z], 0).unwrap();
        }
    }
    for x in 6..=10 {
        world.apply_block_update([x, 1, 10], 1).unwrap();
        world.apply_block_update([x, 2, 10], 1).unwrap();
    }

    // 1. Gravity drop onto y=0 stone floor -> lands cleanly at y=1.0
    let mut player = PlayerPhysics::new([8.5, 4.0, 8.5], 0.0, 0.0);
    for _ in 0..20 {
        player.step(&world, TickInput::default()).unwrap();
    }
    assert!(player.on_ground);
    assert!((player.pos[1] - 1.0).abs() < 1e-9);

    // 2. Walk forward (+Z) into wall at z=10 -> collides and stops before z=9.7
    let mut hit_wall = false;
    for _ in 0..20 {
        let out = player
            .step(
                &world,
                TickInput {
                    forward: 0.2,
                    ..TickInput::default()
                },
            )
            .unwrap();
        if out.horizontal_collision {
            hit_wall = true;
        }
    }
    assert!(hit_wall);
    assert!(player.pos[2] <= 10.0 - 0.3 + 1e-6);

    // 3. Relative teleport drift tracking
    let drift = player.apply_teleport(&TeleportSync {
        x: 0.0,
        y: 2.0,
        z: 0.0,
        yaw: 0.0,
        pitch: 0.0,
        flags: 0x07, // relative XYZ
        teleport_id: 1,
    });
    assert!((drift - 2.0).abs() < 1e-9);

    // 4. Water tank immersion triggers client-kernel fluid_impulse
    for x in 1..=3 {
        for z in 1..=3 {
            for y in 1..=3 {
                world.apply_block_update([x, y, z], 80).unwrap();
            }
        }
    }
    let mut swimmer = PlayerPhysics::new([2.5, 2.0, 2.5], 0.0, 0.0);
    let out = swimmer.step(&world, TickInput::default()).unwrap();
    assert!(out.in_fluid);
    assert!(swimmer.in_water);
}

// ---------------------------------------------------------------------------
// Phase 2: kernel Registry + LightEngine, software renderer, integrated SP
// ---------------------------------------------------------------------------

use std::path::PathBuf;

use mc_spine_client::integrated;
use mc_spine_client::render;
use mc_spine_client::sim::run_session;
use mc_spine_client_mesh::{section_index, LIGHT_BYTES};

fn test_block_table() -> BlockTable {
    let json_path = Path::new("../assets/block_states_1211.json");
    if json_path.is_file() {
        BlockTable::from_json_file(json_path).expect("load block_states_1211.json")
    } else {
        BlockTable::builtin()
    }
}

fn read_light_nibble(bytes: &[u8; LIGHT_BYTES], idx: usize) -> u8 {
    (bytes[idx / 2] >> ((idx & 1) * 4)) & 0x0f
}

#[test]
fn kernel_registry_light_engine_local_edit_light_update_remesh() {
    let raw = build_synthetic_chunk_packet(0, 0, 24);
    let decoded = decode_chunk_packet(&raw, 24, 64).expect("decode synthetic chunk");
    let mut world = WorldState::new(DimensionMeta::default(), test_block_table());
    world.apply_chunk(decoded).expect("apply chunk");

    let registry = world
        .registry
        .as_ref()
        .expect("kernel Registry should load from ../assets");
    let torch = registry
        .default_state("minecraft:torch")
        .expect("torch block state");

    // (5,2,5) is air above the synthetic stone floor at y=0.
    world
        .apply_block_update([5, 2, 5], torch)
        .expect("torch edit");
    assert_eq!(world.local_light_edits, 1);

    // The engine's block-light propagation must land in the stored section
    // nibble arrays; stored section index 4 covers world y 0..15.
    let col = world.columns.get(&(0, 0)).expect("chunk column");
    let sec = &col.sections[4];
    let torch_light = read_light_nibble(&sec.block_light, section_index(5, 2, 5));
    let neighbor_light = read_light_nibble(&sec.block_light, section_index(6, 2, 5));
    assert!(torch_light > 0, "torch cell should emit block light");
    assert!(
        neighbor_light > 0 && neighbor_light < torch_light,
        "block light should decay away from the torch (torch={torch_light} near={neighbor_light})"
    );

    // The relit section is marked dirty and remeshes with the new light.
    assert!(world.pending_mesh_sections() > 0);
    world
        .rebuild_dirty_bounded(64, [8.0, 4.0, 8.0])
        .expect("rebuild after local light edit");
    assert!(world.metrics.total_quads > 0);
}

#[test]
fn render_packed_quads_into_valid_nonblank_png() {
    let raw = build_synthetic_chunk_packet(0, 0, 24);
    let decoded = decode_chunk_packet(&raw, 24, 64).expect("decode synthetic chunk");
    let mut world = WorldState::new(DimensionMeta::default(), test_block_table());
    world.apply_chunk(decoded).expect("apply chunk");
    world
        .rebuild_dirty_bounded(64, [8.0, 4.0, 8.0])
        .expect("rebuild dirty");
    assert!(world.metrics.packed_vertices > 0);

    // Eye above the floor, pitched down to frame the meshed chunk.
    let frame =
        render::render_frame(&world, [8.5, 6.0, 8.5], 0.0, 35.0, 160, 90).expect("render frame");
    assert!(frame.stats.triangles_drawn > 0);
    assert!(frame.stats.pixels_shaded > 0);
    assert!(frame.stats.render_ms >= 0.0);

    let png = frame.encode_png();
    assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    assert_eq!(&png[8..12], &13u32.to_be_bytes(), "IHDR length");
    assert_eq!(&png[12..16], b"IHDR");
    assert_eq!(&png[16..20], &160u32.to_be_bytes(), "width");
    assert_eq!(&png[20..24], &90u32.to_be_bytes(), "height");
    assert_eq!(&png[24..29], &[8, 2, 0, 0, 0], "8-bit truecolor RGB");

    // Walk chunks: collect IDAT payload, require a terminating IEND.
    let mut idat = Vec::new();
    let mut pos = 8usize;
    let mut saw_iend = false;
    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let tag = &png[pos + 4..pos + 8];
        let data = &png[pos + 8..pos + 8 + len];
        match tag {
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => {
                saw_iend = true;
                break;
            }
            _ => {}
        }
        pos += 12 + len;
    }
    assert!(!idat.is_empty(), "PNG must contain IDAT");
    assert!(saw_iend, "PNG must contain IEND");

    let stride = 160 * 3 + 1;
    let raw_pixels = zlib_decompress(&idat, stride * 90 + 64).expect("inflate IDAT");
    assert_eq!(raw_pixels.len(), stride * 90);
    let mut distinct = std::collections::HashSet::new();
    for row in raw_pixels.chunks_exact(stride) {
        assert_eq!(row[0], 0, "encoder must emit filter type 0");
        for px in row[1..].as_chunks::<3>().0 {
            distinct.insert([px[0], px[1], px[2]]);
        }
    }
    assert!(
        distinct.len() > 8,
        "rendered frame must not be a flat fill ({distinct:?})"
    );
}

#[test]
fn quickplay_singleplayer_world_name_traversal_rejected() {
    for bad in ["", "..", "../escape", "a/b", "a\\b", "..\\win", "a\u{7}b"] {
        assert!(
            integrated::validate_world_name(bad).is_err(),
            "{bad:?} must be rejected"
        );
    }
    for good in ["testworld", "My World", "world-1_2.3", "slot5"] {
        assert!(
            integrated::validate_world_name(good).is_ok(),
            "{good:?} must be accepted"
        );
    }
}

#[test]
fn cli_quickplay_render_and_server_flags() {
    let argv = vec![
        "--quickPlaySingleplayer",
        "testworld",
        "--gameDir",
        "/tmp/g",
        "--server-bin",
        "/tmp/mc-spine",
        "--seed",
        "42",
        "--render-png",
        "/tmp/f.png",
        "--width",
        "640",
        "--height",
        "360",
        "--headless",
        "--ticks",
        "5",
    ];
    let cfg = parse_args(&argv).expect("parse flags");
    assert_eq!(cfg.quick_play_singleplayer.as_deref(), Some("testworld"));
    assert_eq!(cfg.server_bin.as_deref(), Some(Path::new("/tmp/mc-spine")));
    assert_eq!(cfg.seed, Some(42));
    assert_eq!(cfg.render_png.as_deref(), Some(Path::new("/tmp/f.png")));
    assert_eq!(cfg.render_width, 640);
    assert_eq!(cfg.render_height, 360);

    let clamped = parse_args(&["--render-png", "x.png", "--width", "1", "--height", "99999"])
        .expect("parse clamped dims");
    assert_eq!(clamped.render_width, 64);
    assert_eq!(clamped.render_height, 1080);
    assert_eq!(clamped.render_png.as_deref(), Some(Path::new("x.png")));
}

fn find_server_bin() -> Option<PathBuf> {
    if let Ok(bin) = integrated::resolve_server_bin(None) {
        return Some(bin);
    }
    [
        PathBuf::from("../target/debug/mc-spine"),
        PathBuf::from("../target/release/mc-spine"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

#[test]
fn quickplay_singleplayer_loopback_e2e_renders_png() {
    let Some(bin) = find_server_bin() else {
        eprintln!("mc-spine server binary not built; skipping live E2E");
        return;
    };
    let game_dir = std::env::temp_dir().join(format!("mc-spine-client-e2e-{}", std::process::id()));
    let png_path = game_dir.join("frame.png");
    let _ = std::fs::remove_dir_all(&game_dir);

    let argv: Vec<String> = vec![
        "--headless".to_string(),
        "--quickPlaySingleplayer".to_string(),
        "e2eworld".to_string(),
        "--gameDir".to_string(),
        game_dir.to_string_lossy().into_owned(),
        "--server-bin".to_string(),
        bin.to_string_lossy().into_owned(),
        "--ticks".to_string(),
        "20".to_string(),
        "--script".to_string(),
        "walk".to_string(),
        "--render-png".to_string(),
        png_path.to_string_lossy().into_owned(),
        "--json-summary".to_string(),
    ];
    let cfg = parse_args(&argv).expect("parse e2e args");

    // Mirror main(): launch the integrated server, point the session at it.
    let server = integrated::launch(&cfg).expect("launch integrated server");
    let mut cfg = cfg;
    cfg.server = Some("127.0.0.1".to_string());
    cfg.port = server.port;
    let summary = run_session(&cfg).expect("run session against integrated server");
    drop(server); // SIGTERM + reap before touching artifacts

    let png = std::fs::read(&png_path).expect("render PNG must exist");
    assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    assert!(png.len() > 200, "PNG suspiciously small: {}", png.len());

    assert!(summary.login_ok && summary.config_ok && summary.play_ok);
    assert!(summary.chunks_decoded > 0, "no chunks decoded");
    assert!(
        summary.triangles_drawn > 0 || summary.pixels_shaded > 0,
        "renderer produced no output"
    );
    let _ = std::fs::remove_dir_all(&game_dir);
}

// ---------------------------------------------------------------------------
// Lane H4: --mc-version / --version-dir CLI, SRV redirect, dual 767/777 wire
// ---------------------------------------------------------------------------

#[test]
fn cli_mc_version_named_numeric_and_fail_closed() {
    for (value, want) in [
        ("1.21.1", 767),
        ("767", 767),
        ("26.3", 777),
        ("777", 777),
    ] {
        let cfg = parse_args(&[
            "--mc-version".to_string(),
            value.to_string(),
            "--headless".to_string(),
        ])
        .unwrap_or_else(|e| panic!("--mc-version {value} should parse: {e}"));
        assert_eq!(
            cfg.mc_version.number(),
            want,
            "--mc-version {value} mapped to wrong protocol"
        );
    }
    // Default stays the pinned 1.21.1 profile.
    let cfg = parse_args(&["--headless".to_string()]).expect("default parse");
    assert_eq!(cfg.mc_version.number(), 767);
    assert_eq!(cfg.mc_version, mc_spine_proto::ProtocolVersion::V1_21_1);
    // Unknown values fail closed instead of guessing a table.
    for bad in ["1.20.4", "765", "26.4", "778", "", "vanilla"] {
        assert!(
            parse_args(&["--mc-version".to_string(), bad.to_string()]).is_err(),
            "--mc-version {bad:?} must fail closed"
        );
    }
    // parse_mc_version itself accepts surrounding whitespace-free values only
    // via trim, so a padded-but-valid string still resolves.
    assert_eq!(
        mc_spine_client::cli::parse_mc_version(" 777 ")
            .unwrap()
            .number(),
        777
    );
}

#[test]
fn cli_version_dir_flag_env_fallback_and_precedence() {
    use mc_spine_client::cli::VERSION_DIR_ENV;

    std::env::remove_var(VERSION_DIR_ENV);
    let cfg = parse_args(&["--headless".to_string()]).expect("no dir");
    assert!(cfg.version_dir.is_none());

    // Explicit flag.
    let cfg = parse_args(&[
        "--version-dir".to_string(),
        "/tmp/vdir-flag".to_string(),
        "--headless".to_string(),
    ])
    .expect("--version-dir parse");
    assert_eq!(cfg.version_dir.as_deref(), Some(Path::new("/tmp/vdir-flag")));

    // Environment fallback when the flag is absent.
    std::env::set_var(VERSION_DIR_ENV, "/tmp/vdir-env");
    let cfg = parse_args(&["--headless".to_string()]).expect("env fallback");
    assert_eq!(cfg.version_dir.as_deref(), Some(Path::new("/tmp/vdir-env")));
    // The flag still wins over the environment.
    let cfg = parse_args(&[
        "--version-dir".to_string(),
        "/tmp/vdir-flag".to_string(),
        "--headless".to_string(),
    ])
    .expect("flag precedence");
    assert_eq!(cfg.version_dir.as_deref(), Some(Path::new("/tmp/vdir-flag")));
    // An empty env value is ignored.
    std::env::set_var(VERSION_DIR_ENV, "");
    let cfg = parse_args(&["--headless".to_string()]).expect("empty env");
    assert!(cfg.version_dir.is_none());
    std::env::remove_var(VERSION_DIR_ENV);
}

/// Read one uncompressed `len | pid | payload` frame off a mock server socket.
fn read_mock_frame(sock: &mut std::net::TcpStream) -> (i32, Vec<u8>) {
    let mut len = 0u32;
    for i in 0..5 {
        let mut b = [0u8; 1];
        sock.read_exact(&mut b).unwrap();
        len |= u32::from(b[0] & 0x7f) << (7 * i);
        if b[0] & 0x80 == 0 {
            break;
        }
    }
    let mut frame = vec![0u8; len as usize];
    sock.read_exact(&mut frame).unwrap();
    let mut r = WireReader::new(&frame);
    let pid = r.read_varint().unwrap();
    let body = r.take(r.remaining()).unwrap().to_vec();
    (pid, body)
}

#[test]
fn srv_redirect_connect_preserves_original_host_in_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let backend_port = listener.local_addr().unwrap().port();

    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept backend");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // Handshake must carry the ORIGINAL host/port even though the TCP
        // connection arrived on the loopback "backend" (SRV redirect).
        let (pid, body) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x00, "first packet must be handshake");
        let mut r = WireReader::new(&body);
        assert_eq!(r.read_varint().unwrap(), 767);
        assert_eq!(r.read_string(255).unwrap(), "play.example.org");
        let mut port = [0u8; 2];
        port.copy_from_slice(r.take(2).unwrap());
        assert_eq!(u16::from_be_bytes(port), 25565);
        assert_eq!(r.read_varint().unwrap(), 2, "next_state login");
        // Login Start follows; answer with a login disconnect so the client
        // unwinds cleanly.
        let (pid, _login_start) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x00, "second packet must be login_start");
        let mut disco = Vec::new();
        write_string(&mut disco, "srv test complete");
        sock.write_all(&encode_packet(0x00, &disco, None)).unwrap();
    });

    let result = ProtocolClient::connect_resolved(
        "play.example.org",
        25565,
        "Tester",
        offline_uuid("Tester"),
        2,
        Duration::from_secs(5),
        mc_spine_proto::ProtocolVersion::V1_21_1,
        move |host, port| {
            // The injected `_minecraft._tcp` SRV answer redirects the socket.
            assert_eq!(host, "play.example.org");
            assert_eq!(port, 25565);
            Ok(vec![format!("127.0.0.1:{backend_port}").parse().unwrap()])
        },
    );
    assert!(result.is_err(), "mock server disconnects during login");
    server.join().unwrap();
}

#[test]
fn protocol777_login_config_play_keepalive_mock_server() {
    use mc_spine_proto::{PacketDir, PacketPhase, ProtocolTable};

    // The 767 vs 777 KeepAlive split this test exercises end-to-end.
    let t767 = ProtocolTable::v767_builtin();
    let t777 = ProtocolTable::v777_builtin();
    let play = PacketPhase::Play;
    assert_eq!(
        t767.packet_id(play, PacketDir::Clientbound, "keep_alive"),
        Some(0x26)
    );
    assert_eq!(
        t777.packet_id(play, PacketDir::Clientbound, "keep_alive"),
        Some(0x2d)
    );
    assert_eq!(
        t767.packet_id(play, PacketDir::Serverbound, "keep_alive"),
        Some(0x18)
    );
    assert_eq!(
        t777.packet_id(play, PacketDir::Serverbound, "keep_alive"),
        Some(0x1c)
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();

    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        // Handshake: protocol 777, next_state login.
        let (pid, body) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x00);
        let mut r = WireReader::new(&body);
        assert_eq!(r.read_varint().unwrap(), 777);
        let _host = r.read_string(255).unwrap();
        let _ = r.take(2).unwrap();
        assert_eq!(r.read_varint().unwrap(), 2);

        // Login Start -> 26.3 Login Success: Game Profile
        // (uuid + username + empty property array) + session UUID.
        let (pid, _login) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x00);
        let mut ls = Vec::new();
        ls.extend_from_slice(&offline_uuid("Tester"));
        write_string(&mut ls, "Tester");
        write_varint(&mut ls, 0); // properties: 0
        ls.extend_from_slice(&[7u8; 16]); // session id
        sock.write_all(&encode_packet(0x02, &ls, None)).unwrap();
        let (pid, _) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x03, "login_acknowledged");

        // Configuration keepalive (0x04) -> echo on 0x04.
        sock.write_all(&encode_packet(0x04, &42i64.to_be_bytes(), None))
            .unwrap();
        let (pid, ka) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x04);
        assert_eq!(ka, 42i64.to_be_bytes());

        // Finish Configuration (0x03) -> client_information (0x00) + ack (0x03).
        sock.write_all(&encode_packet(0x03, &[], None)).unwrap();
        let (pid, info) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x00, "client_information");
        // 26.3 client_information carries the extra particle_status VarInt.
        let mut r = WireReader::new(&info);
        let _locale = r.read_string(16).unwrap();
        let _view = r.read_u8().unwrap();
        let _chat = r.read_varint().unwrap();
        let _colors = r.read_u8().unwrap();
        let _skin = r.read_u8().unwrap();
        let _hand = r.read_varint().unwrap();
        let _filter = r.read_u8().unwrap();
        let _listings = r.read_u8().unwrap();
        assert_eq!(r.read_varint().unwrap(), 0, "particle_status present");
        let (pid, _) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x03, "finish_configuration ack");

        // Play KeepAlive (s2c 0x2d / 45) -> c2s echo on 0x1c / 28.
        sock.write_all(&encode_packet(0x2d, &99i64.to_be_bytes(), None))
            .unwrap();
        let (pid, ka) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x1c, "777 c2s keep_alive id");
        assert_eq!(ka, 99i64.to_be_bytes());

        // 26.3 Synchronize Player Position (0x49): teleport id first, then
        // pos/velocity doubles, yaw/pitch, Int flags.
        let mut tp = Vec::new();
        write_varint(&mut tp, 7); // teleport id
        tp.extend_from_slice(&10.5f64.to_be_bytes());
        tp.extend_from_slice(&65.0f64.to_be_bytes());
        tp.extend_from_slice(&(-4.5f64).to_be_bytes());
        tp.extend_from_slice(&0.0f64.to_be_bytes()); // vx
        tp.extend_from_slice(&0.0f64.to_be_bytes()); // vy
        tp.extend_from_slice(&0.0f64.to_be_bytes()); // vz
        tp.extend_from_slice(&15.0f32.to_be_bytes()); // yaw
        tp.extend_from_slice(&(-5.0f32).to_be_bytes()); // pitch
        tp.extend_from_slice(&0i32.to_be_bytes()); // flags
        sock.write_all(&encode_packet(0x49, &tp, None)).unwrap();

        // 26.3 accept_teleportation (0x00) echoes the resulting pose.
        let (pid, ack) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x00, "accept_teleportation");
        let mut r = WireReader::new(&ack);
        assert_eq!(r.read_varint().unwrap(), 7);
        assert_eq!(r.read_f64().unwrap(), 10.5);
        assert_eq!(r.read_f64().unwrap(), 65.0);
        assert_eq!(r.read_f64().unwrap(), -4.5);
        assert_eq!(r.read_f32().unwrap(), 15.0);
        assert_eq!(r.read_f32().unwrap(), -5.0);
        // Vanilla follows with move_player_pos_rot (0x1f).
        let (pid, _pos_rot) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x1f, "move_player_pos_rot after accept_teleportation");

        // 26.3 player_input (c2s 0x2b / 43) is a single flag byte.
        let (pid, input) = read_mock_frame(&mut sock);
        assert_eq!(pid, 0x2b, "777 c2s player_input id");
        assert_eq!(input, vec![0x11], "forward + jump flag bits");
    });

    let (mut cli, report) = ProtocolClient::connect_for_version(
        "127.0.0.1",
        port,
        "Tester",
        offline_uuid("Tester"),
        2,
        Duration::from_secs(5),
        mc_spine_proto::ProtocolVersion::V26_3,
    )
    .expect("777 connect");
    assert!(report.login_ok && report.config_ok);
    assert_eq!(cli.protocol_version.number(), 777);

    match cli.recv_play(Duration::from_secs(2)).unwrap().unwrap() {
        PlayEvent::KeepAlive(id) => {
            assert_eq!(id, 99);
            cli.send_keep_alive(id).unwrap();
        }
        other => panic!("expected KeepAlive, got {other:?}"),
    }
    match cli.recv_play(Duration::from_secs(2)).unwrap().unwrap() {
        PlayEvent::Teleport(tp) => {
            assert_eq!(tp.teleport_id, 7);
            assert_eq!((tp.x, tp.y, tp.z), (10.5, 65.0, -4.5));
            cli.send_accept_teleportation(
                tp.teleport_id,
                [tp.x, tp.y, tp.z],
                tp.yaw,
                tp.pitch,
                true,
            )
            .unwrap();
        }
        other => panic!("expected Teleport, got {other:?}"),
    }
    cli.send_player_input(0.0, 1.0, true, false).unwrap();
    server.join().unwrap();
}

#[test]
fn protocol_table_driven_ids_match_builtins() {
    use mc_spine_proto::{PacketDir, PacketPhase, ProtocolTable};

    let t767 = ProtocolTable::v767_builtin();
    let t777 = ProtocolTable::v777_builtin();
    let play = PacketPhase::Play;
    // Table-driven resolution supplies the ids the client sends — the
    // version-divergent core set must differ where 26.3 remapped them.
    for name in [
        "keep_alive",
        "level_chunk_with_light",
        "player_position",
        "disconnect",
        "chunk_batch_start",
        "chunk_batch_finished",
        "light_update",
        "block_update",
        "section_blocks_update",
        "forget_level_chunk",
        "set_chunk_cache_center",
        "set_health",
        "respawn",
        "login",
        "ticking_state",
    ] {
        assert!(
            t767.packet_id(play, PacketDir::Clientbound, name).is_some(),
            "767 table missing s2c {name}"
        );
        assert!(
            t777.packet_id(play, PacketDir::Clientbound, name).is_some(),
            "777 table missing s2c {name}"
        );
    }
    for name in [
        "accept_teleportation",
        "chunk_batch_received",
        "keep_alive",
        "player_input",
        "move_player_pos",
        "move_player_pos_rot",
    ] {
        assert!(
            t767.packet_id(play, PacketDir::Serverbound, name).is_some(),
            "767 table missing c2s {name}"
        );
        assert!(
            t777.packet_id(play, PacketDir::Serverbound, name).is_some(),
            "777 table missing c2s {name}"
        );
    }
    // The two ids this lane hinges on.
    assert_eq!(
        t777.packet_id(play, PacketDir::Serverbound, "player_input"),
        Some(0x2b)
    );
    assert_eq!(
        t767.packet_id(play, PacketDir::Serverbound, "player_input"),
        Some(0x26)
    );
}

/// Minimal `block_state_facts.bin` row: [flags, emission<<4|opacity,
/// sturdy u24le, reserved].
fn fact_row(flags: u8, emission: u8, opacity: u8, sturdy: u32) -> [u8; 6] {
    let sturdy = sturdy & 0x3f;
    [
        flags,
        emission << 4 | opacity,
        sturdy as u8,
        (sturdy >> 8) as u8,
        (sturdy >> 16) as u8,
        0,
    ]
}

#[test]
fn world_state_opens_version_dir_pack() {
    const FACT_IS_AIR: u8 = 1;
    const FACT_LIQUID: u8 = 1 << 1;
    const FACT_BLOCKS_MOTION: u8 = 1 << 2;
    const FACT_CAN_OCCLUDE: u8 = 1 << 3;
    const FACT_SOLID_RENDER: u8 = 1 << 4;
    const FACT_FLUID_EMPTY: u8 = 1 << 7;

    // A small custom 5-state pack proves `Registry::from_version_dir` drives
    // `WorldState::state_count` instead of the pinned STATE_COUNT.
    let dir = std::env::temp_dir().join(format!(
        "mc-spine-client-vdir-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("blocks.json"),
        r#"[
            {"name": "air", "defaultState": 0, "minStateId": 0, "maxStateId": 0},
            {"name": "stone", "defaultState": 1, "minStateId": 1, "maxStateId": 1},
            {"name": "water", "defaultState": 2, "minStateId": 2, "maxStateId": 4}
        ]"#,
    )
    .unwrap();
    let facts: Vec<u8> = [
        fact_row(FACT_IS_AIR | FACT_FLUID_EMPTY, 0, 0, 0),
        fact_row(
            FACT_SOLID_RENDER | FACT_CAN_OCCLUDE | FACT_BLOCKS_MOTION,
            0,
            15,
            0x3f,
        ),
        fact_row(FACT_LIQUID, 0, 0, 0x3e),
        fact_row(FACT_LIQUID, 0, 0, 0x3e),
        fact_row(FACT_LIQUID, 0, 0, 0x3e),
    ]
    .concat();
    std::fs::write(dir.join("block_state_facts.bin"), facts).unwrap();

    let ws = WorldState::new_from_version_dir(
        DimensionMeta::default(),
        test_block_table(),
        &dir,
    )
    .expect("open custom version pack");
    assert!(ws.registry.is_some());
    assert_eq!(ws.state_count, 5, "pack state_count drives the bound");
    let _ = std::fs::remove_dir_all(&dir);

    // The pinned `assets/` directory is itself a valid version pack
    // (`*_1211` fallback names).
    let pinned_dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../assets"));
    let ws = WorldState::new_from_version_dir(
        DimensionMeta::default(),
        test_block_table(),
        pinned_dir,
    )
    .expect("open pinned assets as version pack");
    assert_eq!(ws.state_count, mc_spine_kernel::STATE_COUNT as usize);

    // A missing directory fails closed.
    assert!(WorldState::new_from_version_dir(
        DimensionMeta::default(),
        test_block_table(),
        Path::new("/nonexistent-mc-spine-version-pack"),
    )
    .is_err());
}
