// SPDX-License-Identifier: LGPL-3.0-or-later

use std::time::{Duration, Instant};

use megu_core::hash::crc32_ieee;
use megu_proto::region_merkle_sync::{
    decode_blind_relay_frame, decode_region_merkle_packet, encode_blind_relay_frame,
    encode_region_merkle_packet, negotiate_region_diff, BlindRelayError, BlindRelayFrame,
    BlindRelayPolicy, BlindRelayRouter, NodeId, RegionMerkleError, RegionMerklePacket,
    RegionMerkleTree, BLIND_RELAY_FRAME_OVERHEAD_BYTES, MAX_BLIND_PAYLOAD_BYTES,
    MAX_RMK1_PACKET_BYTES, REGION_CHUNK_COUNT, REGION_TREE_DEPTH, RMK1_MAGIC, ZTR1_MAGIC,
};

const DIM: i32 = 0;
const EPOCH: &str = "epoch-a";

fn digest(seed: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&seed.to_be_bytes());
    for (i, byte) in out.iter_mut().enumerate().skip(8) {
        *byte = (seed as u8).wrapping_mul(31).wrapping_add(i as u8);
    }
    out
}

fn populated_leaves() -> Vec<[u8; 32]> {
    (0..REGION_CHUNK_COUNT)
        .map(|index| digest(index as u64 + 1))
        .collect()
}

fn populated_tree(epoch: &str, rx: i32, rz: i32) -> RegionMerkleTree {
    RegionMerkleTree::with_chunk_hashes(epoch, DIM, rx, rz, &populated_leaves())
        .expect("exactly 1,024 leaves")
}

fn reseal_rmk1(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.truncate(bytes.len() - 4);
    let crc = crc32_ieee(&bytes);
    bytes.extend_from_slice(&crc.to_be_bytes());
    bytes
}

fn reseal_ztr1(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.truncate(bytes.len() - 4);
    let crc = crc32_ieee(&bytes);
    bytes.extend_from_slice(&crc.to_be_bytes());
    bytes
}

fn node(seed: u8) -> NodeId {
    NodeId::new([seed; 32])
}

fn relay_frame(
    session_id: u64,
    source: NodeId,
    destination: NodeId,
    epoch: u32,
) -> BlindRelayFrame {
    BlindRelayFrame {
        session_id,
        source,
        destination,
        migration_epoch: epoch,
        payload: (0..48)
            .map(|i| 0xA0u8.wrapping_add(i as u8).rotate_left(1))
            .collect(),
    }
}

#[test]
fn incremental_update_matches_full_rebuild_bit_for_bit() {
    let mut leaves = populated_leaves();
    let mut incremental = populated_tree(EPOCH, -2, 3);
    let original_root = incremental.root();

    let changed = digest(9_999);
    incremental
        .update_chunk_hash(5, 9, changed)
        .expect("valid local coordinate");
    leaves[9 * 32 + 5] = changed;
    let rebuilt = RegionMerkleTree::with_chunk_hashes(EPOCH, DIM, -2, 3, &leaves)
        .expect("exactly 1,024 leaves");

    assert_eq!(incremental, rebuilt);
    assert_eq!(incremental.root(), rebuilt.root());
    assert_ne!(incremental.root(), original_root);
    assert_eq!(incremental.leaf_hash(5, 9), Some(&changed));
    assert_eq!(incremental.node_count(), 1 + 4 + 16 + 64 + 256 + 1_024);
    assert!(incremental.update_chunk_hash(32, 0, changed).is_err());
    assert!(incremental.update_chunk_hash(0, 32, changed).is_err());
}

#[test]
fn region_root_is_bound_to_epoch_dimension_and_region_coordinates() {
    let leaves = populated_leaves();
    let base = RegionMerkleTree::with_chunk_hashes(EPOCH, 0, 0, 0, &leaves).unwrap();
    let epoch_changed = RegionMerkleTree::with_chunk_hashes("epoch-b", 0, 0, 0, &leaves).unwrap();
    let dim_changed = RegionMerkleTree::with_chunk_hashes(EPOCH, 1, 0, 0, &leaves).unwrap();
    let rx_changed = RegionMerkleTree::with_chunk_hashes(EPOCH, 0, 1, 0, &leaves).unwrap();
    let rz_changed = RegionMerkleTree::with_chunk_hashes(EPOCH, 0, 0, 1, &leaves).unwrap();

    assert_ne!(base.root(), epoch_changed.root());
    assert_ne!(base.root(), dim_changed.root());
    assert_ne!(base.root(), rx_changed.root());
    assert_ne!(base.root(), rz_changed.root());
    assert!(base.world_chunk_coord(31, 31).is_ok());

    let overflow = RegionMerkleTree::new(EPOCH, 0, i32::MAX, i32::MAX);
    assert!(matches!(
        overflow.world_chunk_coord(0, 0),
        Err(RegionMerkleError::CoordinateOverflow)
    ));
}

#[test]
fn negotiation_rejects_epochs_that_cannot_start_rmk1() {
    for epoch in ["", &"x".repeat(129)] {
        let client = RegionMerkleTree::new(epoch, 0, 0, 0);
        let server = RegionMerkleTree::new(epoch, 0, 0, 0);
        assert!(matches!(
            negotiate_region_diff(&client, &server),
            Err(RegionMerkleError::InvalidEpochLength { .. })
        ));
    }
}

#[test]
fn identical_region_root_produces_one_server_region_match() {
    let client = populated_tree(EPOCH, 0, 0);
    let server = client.clone();

    let negotiation = negotiate_region_diff(&client, &server).expect("compatible trees");

    assert!(negotiation.matched);
    assert!(negotiation.dirty_chunks.is_empty());
    assert!(negotiation.client_packets.is_empty());
    assert_eq!(negotiation.server_packets.len(), 1);
    assert_eq!(negotiation.message_count(), 1);
    assert_eq!(
        negotiation.server_packets[0],
        RegionMerklePacket::ServerRegionMatch {
            dim_id: DIM,
            rx: 0,
            rz: 0,
        }
    );
    assert!(client.diff_chunk_coords(&server).unwrap().is_empty());
}

#[test]
fn single_chunk_mutation_is_isolated_with_logarithmic_messages() {
    let client = populated_tree(EPOCH, -2, 3);
    let mut server = client.clone();
    server
        .update_chunk_hash(5, 9, digest(9_999))
        .expect("valid local coordinate");

    let negotiation = negotiate_region_diff(&client, &server).expect("compatible trees");

    assert!(!negotiation.matched);
    assert_eq!(negotiation.dirty_chunks, vec![(-59, 105)]);
    assert_eq!(client.diff_chunk_coords(&server).unwrap(), vec![(-59, 105)]);
    assert_eq!(
        negotiation.server_packets.len(),
        usize::from(REGION_TREE_DEPTH)
    );
    assert_eq!(
        negotiation.client_packets.len(),
        usize::from(REGION_TREE_DEPTH)
    );
    assert_eq!(negotiation.message_count(), 10);
    for round in 0..usize::from(REGION_TREE_DEPTH) {
        let expected_level = round as u8 + 1;
        match &negotiation.server_packets[round] {
            RegionMerklePacket::ServerSubtreeHashes { level, nodes, .. } => {
                assert_eq!(*level, expected_level);
                assert_eq!(nodes.len(), 4);
            }
            other => panic!("expected server subtree packet, got {other:?}"),
        }
        match &negotiation.client_packets[round] {
            RegionMerklePacket::ClientSubtreeHashes { level, nodes, .. } => {
                assert_eq!(*level, expected_level);
                assert_eq!(nodes.len(), 1);
            }
            other => panic!("expected client subtree packet, got {other:?}"),
        }
    }
}

#[test]
fn worldgen_epoch_mismatch_fails_closed_before_any_diff() {
    let client = populated_tree(EPOCH, 0, 0);
    let server = populated_tree("epoch-b", 0, 0);

    let error = negotiate_region_diff(&client, &server).expect_err("epochs must fail closed");
    assert_eq!(
        error,
        RegionMerkleError::EpochMismatch {
            expected: "epoch-b".to_string(),
            actual: EPOCH.to_string(),
        }
    );
    assert!(client.diff_chunk_coords(&server).is_err());
}

#[test]
fn rmk1_packets_round_trip_with_magic_version_and_crc() {
    let packets = vec![
        RegionMerklePacket::ClientRegionRoots {
            worldgen_epoch: EPOCH.to_string(),
            dim_id: DIM,
            regions: vec![(-1, 2, digest(2)), (0, 0, digest(1))],
        },
        RegionMerklePacket::ServerRegionMatch {
            dim_id: DIM,
            rx: -1,
            rz: 2,
        },
        RegionMerklePacket::ServerSubtreeHashes {
            dim_id: DIM,
            rx: -1,
            rz: 2,
            level: 2,
            nodes: vec![(0, digest(3)), (15, digest(4))],
        },
        RegionMerklePacket::ClientSubtreeHashes {
            dim_id: DIM,
            rx: -1,
            rz: 2,
            level: 5,
            nodes: vec![(1_023, digest(5))],
        },
        RegionMerklePacket::EpochMismatch {
            dim_id: DIM,
            expected_worldgen_epoch: "epoch-b".to_string(),
            actual_worldgen_epoch: EPOCH.to_string(),
        },
    ];

    for packet in packets {
        let bytes = encode_region_merkle_packet(&packet).expect("encode");
        assert_eq!(&bytes[..4], &RMK1_MAGIC);
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 1);
        let crc_offset = bytes.len() - 4;
        let stored = u32::from_be_bytes(bytes[crc_offset..].try_into().unwrap());
        assert_eq!(stored, crc32_ieee(&bytes[..crc_offset]));
        assert_eq!(decode_region_merkle_packet(&bytes).expect("decode"), packet);
        assert_eq!(
            RegionMerklePacket::decode(&bytes).expect("inherent decode"),
            packet
        );
    }
}

#[test]
fn rmk1_adversarial_truncation_bitflip_trailing_and_oversize_fail_closed() {
    let packet = RegionMerklePacket::ClientRegionRoots {
        worldgen_epoch: EPOCH.to_string(),
        dim_id: DIM,
        regions: vec![(0, 0, digest(1)), (1, 0, digest(2))],
    };
    let bytes = encode_region_merkle_packet(&packet).expect("encode");

    for cut in 0..bytes.len() {
        assert!(
            decode_region_merkle_packet(&bytes[..cut]).is_err(),
            "truncation to {cut} bytes must fail"
        );
    }
    for position in 0..bytes.len() {
        let mut mutated = bytes.clone();
        mutated[position] ^= 1 << (position % 8);
        assert!(
            decode_region_merkle_packet(&mutated).is_err(),
            "bit flip at {position} must fail"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode_region_merkle_packet(&trailing).is_err());
    assert!(decode_region_merkle_packet(&vec![0u8; MAX_RMK1_PACKET_BYTES + 1]).is_err());
}

#[test]
fn rmk1_resealed_semantic_attacks_fail_closed() {
    let packet = RegionMerklePacket::ClientRegionRoots {
        worldgen_epoch: EPOCH.to_string(),
        dim_id: DIM,
        regions: vec![(0, 0, digest(1)), (1, 0, digest(2))],
    };
    let mut duplicated = encode_region_merkle_packet(&packet).expect("encode");
    let first_entry = 7 + 2 + EPOCH.len() + 4 + 2;
    let second_entry = first_entry + 40;
    duplicated.copy_within(first_entry..first_entry + 8, second_entry);
    let duplicated = reseal_rmk1(duplicated);
    assert!(matches!(
        decode_region_merkle_packet(&duplicated),
        Err(RegionMerkleError::UnsortedOrDuplicate("region coordinates"))
    ));

    let subtree = RegionMerklePacket::ServerSubtreeHashes {
        dim_id: DIM,
        rx: 0,
        rz: 0,
        level: 2,
        nodes: vec![(15, digest(3))],
    };
    let mut invalid_level = encode_region_merkle_packet(&subtree).expect("encode");
    let level_pos = 7 + 4 + 8;
    invalid_level[level_pos] = 6;
    let invalid_level = reseal_rmk1(invalid_level);
    assert!(matches!(
        decode_region_merkle_packet(&invalid_level),
        Err(RegionMerkleError::InvalidNode { level: 6, .. })
    ));

    let mut invalid_index = encode_region_merkle_packet(&subtree).expect("encode");
    let index_pos = level_pos + 1 + 2;
    invalid_index[index_pos..index_pos + 2].copy_from_slice(&16u16.to_be_bytes());
    let invalid_index = reseal_rmk1(invalid_index);
    assert!(matches!(
        decode_region_merkle_packet(&invalid_index),
        Err(RegionMerkleError::InvalidNode {
            level: 2,
            index: 16
        })
    ));
}

#[test]
fn ztr1_frame_round_trip_with_magic_version_and_crc() {
    let frame = relay_frame(77, node(1), node(2), 3);
    let bytes = encode_blind_relay_frame(&frame).expect("encode");

    assert_eq!(&bytes[..4], &ZTR1_MAGIC);
    assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 1);
    assert_eq!(bytes.len(), frame.encoded_len());
    let crc_offset = bytes.len() - 4;
    let stored = u32::from_be_bytes(bytes[crc_offset..].try_into().unwrap());
    assert_eq!(stored, crc32_ieee(&bytes[..crc_offset]));
    assert_eq!(decode_blind_relay_frame(&bytes).expect("decode"), frame);
    assert_eq!(
        BlindRelayFrame::decode(&bytes).expect("inherent decode"),
        frame
    );
}

#[test]
fn ztr1_adversarial_truncation_bitflip_trailing_and_oversize_fail_closed() {
    let frame = relay_frame(77, node(1), node(2), 3);
    let bytes = encode_blind_relay_frame(&frame).expect("encode");

    for cut in 0..bytes.len() {
        assert!(
            decode_blind_relay_frame(&bytes[..cut]).is_err(),
            "truncation to {cut} bytes must fail"
        );
    }
    for position in 0..bytes.len() {
        let mut mutated = bytes.clone();
        mutated[position] ^= 1 << (position % 8);
        assert!(
            decode_blind_relay_frame(&mutated).is_err(),
            "bit flip at {position} must fail"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode_blind_relay_frame(&trailing).is_err());

    let oversized = BlindRelayFrame {
        payload: vec![0u8; MAX_BLIND_PAYLOAD_BYTES + 1],
        ..frame.clone()
    };
    assert!(matches!(
        encode_blind_relay_frame(&oversized),
        Err(BlindRelayError::PayloadTooLarge { .. })
    ));

    let mut forged_len = bytes.clone();
    forged_len[82..86].copy_from_slice(&((MAX_BLIND_PAYLOAD_BYTES + 1) as u32).to_be_bytes());
    let forged_len = reseal_ztr1(forged_len);
    assert!(matches!(
        decode_blind_relay_frame(&forged_len),
        Err(BlindRelayError::PayloadTooLarge { .. })
    ));
}

fn registered_router(policy: BlindRelayPolicy, now: Instant) -> (BlindRelayRouter, NodeId, NodeId) {
    let alice = node(1);
    let bob = node(2);
    let mut router = BlindRelayRouter::new(policy);
    router.register_peer(alice, 101, 1).expect("register alice");
    router.register_peer(bob, 202, 1).expect("register bob");
    router
        .open_session(77, alice, bob, now)
        .expect("open session");
    (router, alice, bob)
}

#[test]
fn blind_relay_router_forwards_opaque_ciphertext_by_node_id() {
    let now = Instant::now();
    let (mut router, alice, bob) = registered_router(BlindRelayPolicy::default(), now);
    let plaintext = b"the diamonds are under spawn";
    let mut frame = relay_frame(77, alice, bob, 1);
    frame.payload = vec![0xC3, 0x91, 0x44, 0xAA, 0x18, 0x72, 0x5F, 0xE0];
    assert!(!frame
        .payload
        .windows(plaintext.len())
        .any(|window| window == plaintext));
    let wire = encode_blind_relay_frame(&frame).expect("encode");

    let delivery = router.route_wire_frame(101, &wire, now).expect("route");

    assert_eq!(delivery.destination, bob);
    assert_eq!(delivery.endpoint, 202);
    assert_eq!(delivery.frame, frame);
    assert_eq!(delivery.wire_bytes, wire);
    assert!(BlindRelayPolicy::amplification_permitted(
        wire.len(),
        delivery.wire_bytes.len()
    ));
    assert_eq!(router.metrics().frames_forwarded, 1);
    assert_eq!(router.metrics().bytes_forwarded, wire.len() as u64);
}

#[test]
fn blind_relay_router_enforces_bandwidth_and_frame_size_caps() {
    let now = Instant::now();
    let alice = node(1);
    let bob = node(2);
    let probe = relay_frame(77, alice, bob, 1);
    let frame_len = probe.encoded_len() as u32;
    let policy = BlindRelayPolicy {
        frames_per_second: 100,
        bytes_per_second: frame_len,
        ..BlindRelayPolicy::default()
    };
    let (mut router, alice, bob) = registered_router(policy, now);

    router
        .route_frame(101, &relay_frame(77, alice, bob, 1), now)
        .expect("first frame consumes exactly one second of byte budget");
    assert!(matches!(
        router.route_frame(101, &relay_frame(77, alice, bob, 1), now),
        Err(BlindRelayError::RateLimited)
    ));
    router
        .route_frame(
            101,
            &relay_frame(77, alice, bob, 1),
            now + Duration::from_secs(1),
        )
        .expect("byte bucket refills after one second");

    let tight_policy = BlindRelayPolicy {
        max_payload_bytes: probe.payload.len() - 1,
        ..BlindRelayPolicy::default()
    };
    let (mut tight_router, alice, bob) = registered_router(tight_policy, now);
    assert!(matches!(
        tight_router.route_frame(101, &relay_frame(77, alice, bob, 1), now),
        Err(BlindRelayError::PayloadTooLarge { .. })
    ));
    assert!(!BlindRelayPolicy::amplification_permitted(10, 11));
    assert!(BlindRelayPolicy::amplification_permitted(10, 10));
}

#[test]
fn blind_relay_router_accepts_monotonic_migration_and_rejects_replay() {
    let now = Instant::now();
    let (mut router, alice, bob) = registered_router(BlindRelayPolicy::default(), now);

    router
        .migrate_peer_endpoint(alice, 2, 111)
        .expect("monotonic migration");
    assert_eq!(router.peer_registration(alice), Some((111, 2)));
    assert!(matches!(
        router.migrate_peer_endpoint(alice, 2, 112),
        Err(BlindRelayError::StaleMigrationEpoch {
            current: 2,
            presented: 2,
        })
    ));
    assert!(matches!(
        router.migrate_peer_endpoint(alice, 1, 112),
        Err(BlindRelayError::StaleMigrationEpoch {
            current: 2,
            presented: 1,
        })
    ));

    let stale = relay_frame(77, alice, bob, 1);
    assert!(matches!(
        router.route_frame(111, &stale, now),
        Err(BlindRelayError::StaleMigrationEpoch {
            current: 2,
            presented: 1,
        })
    ));

    router
        .migrate_peer_endpoint(bob, 2, 222)
        .expect("destination host migration");
    let upgraded = relay_frame(77, alice, bob, 2);
    let delivery = router
        .route_frame(111, &upgraded, now)
        .expect("upgraded epoch routes");
    assert_eq!(delivery.endpoint, 222);
}

#[test]
fn blind_relay_wire_budget_admission_happens_before_crc_validation() {
    let now = Instant::now();
    let alice = node(1);
    let bob = node(2);
    let policy = BlindRelayPolicy {
        frames_per_second: 1,
        ..BlindRelayPolicy::default()
    };
    let (mut router, _, _) = registered_router(policy, now);
    let first = encode_blind_relay_frame(&relay_frame(77, alice, bob, 1)).expect("encode");
    router
        .route_wire_frame(101, &first, now)
        .expect("first frame");

    let mut second = encode_blind_relay_frame(&relay_frame(77, alice, bob, 1)).expect("encode");
    let last = second.len() - 1;
    second[last] ^= 1;
    assert_eq!(
        router.route_wire_frame(101, &second, now),
        Err(BlindRelayError::RateLimited)
    );
}

#[test]
fn delayed_disconnect_after_migration_cannot_remove_new_endpoint() {
    let now = Instant::now();
    let (mut router, alice, bob) = registered_router(BlindRelayPolicy::default(), now);
    router
        .migrate_peer_endpoint(alice, 2, 111)
        .expect("migration");

    assert!(!router.unregister_peer(alice, 101));
    assert_eq!(router.peer_registration(alice), Some((111, 2)));
    let frame = relay_frame(77, alice, bob, 2);
    assert!(router.route_frame(111, &frame, now).is_ok());

    assert!(router.unregister_peer(alice, 111));
    assert_eq!(router.peer_registration(alice), None);
    assert_eq!(
        router.route_frame(111, &frame, now),
        Err(BlindRelayError::UnknownEndpoint(111))
    );
}

#[test]
fn blind_relay_wire_size_rejects_happen_before_crc_or_payload_copy() {
    let now = Instant::now();
    let alice = node(1);
    let bob = node(2);
    let probe = relay_frame(77, alice, bob, 1);
    let policy = BlindRelayPolicy {
        max_payload_bytes: probe.payload.len() - 1,
        ..BlindRelayPolicy::default()
    };
    let (mut router, _, _) = registered_router(policy, now);
    let mut wire = encode_blind_relay_frame(&probe).expect("encode");
    let last = wire.len() - 1;
    wire[last] ^= 1;

    assert!(matches!(
        router.route_wire_frame(101, &wire, now),
        Err(BlindRelayError::PayloadTooLarge { .. })
    ));
    assert_eq!(router.metrics().rate_or_size_rejects, 1);

    let huge = vec![0u8; megu_proto::region_merkle_sync::MAX_BLIND_FRAME_BYTES + 1];
    assert!(matches!(
        router.route_wire_frame(101, &huge, now),
        Err(BlindRelayError::FrameTooLarge { .. })
    ));
    assert_eq!(router.metrics().rate_or_size_rejects, 2);
}

#[test]
fn blind_relay_router_rejects_unknown_nodes_and_session_spoofing() {
    let now = Instant::now();
    let (mut router, alice, bob) = registered_router(BlindRelayPolicy::default(), now);
    let carol = node(3);

    assert!(matches!(
        router.route_frame(101, &relay_frame(77, alice, carol, 1), now),
        Err(BlindRelayError::UnknownNode(_))
    ));
    assert!(matches!(
        router.route_frame(101, &relay_frame(78, alice, bob, 1), now),
        Err(BlindRelayError::UnknownSession(78))
    ));

    router.register_peer(carol, 303, 1).expect("register carol");
    assert_eq!(
        router.route_frame(101, &relay_frame(77, carol, bob, 1), now),
        Err(BlindRelayError::IngressIdentityMismatch {
            endpoint: 101,
            authenticated: alice,
            claimed: carol,
        })
    );
    assert!(matches!(
        router.route_frame(303, &relay_frame(77, carol, bob, 1), now),
        Err(BlindRelayError::SessionNodeMismatch { session_id: 77 })
    ));
    assert!(matches!(
        router.register_peer(node(4), 303, 1),
        Err(BlindRelayError::EndpointAlreadyRegistered(303))
    ));
}

#[test]
fn ztr1_overhead_matches_wire_layout() {
    let frame = BlindRelayFrame {
        session_id: 1,
        source: node(1),
        destination: node(2),
        migration_epoch: 1,
        payload: Vec::new(),
    };
    assert_eq!(frame.encoded_len(), BLIND_RELAY_FRAME_OVERHEAD_BYTES);
    assert_eq!(
        encode_blind_relay_frame(&frame)
            .expect("empty payload encodes")
            .len(),
        BLIND_RELAY_FRAME_OVERHEAD_BYTES
    );
}
