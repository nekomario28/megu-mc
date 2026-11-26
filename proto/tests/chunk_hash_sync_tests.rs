use megu_proto::chunk_hash_sync::*;
use std::collections::BTreeMap;

fn hash(byte: u8) -> [u8; 32] {
    [byte; 32]
}

fn sample_packets() -> Vec<ChunkHashSyncPacket> {
    vec![
        ChunkHashSyncPacket::ClientHaveHashes {
            worldgen_epoch: "seed:7/world:v1".into(),
            dim_id: -1,
            entries: vec![
                ChunkHashEntry {
                    cx: -2,
                    cz: 3,
                    hash: hash(1),
                },
                ChunkHashEntry {
                    cx: 0,
                    cz: 0,
                    hash: hash(2),
                },
            ],
        },
        ChunkHashSyncPacket::ServerChunkMatch {
            dim_id: 0,
            chunks: vec![(-1, 2), (0, 0)],
        },
        ChunkHashSyncPacket::ServerChunkDelta {
            worldgen_epoch: "epoch-a".into(),
            dim_id: 0,
            cx: 4,
            cz: -9,
            base_hash: hash(3),
            new_hash: hash(4),
            edits: vec![
                ChunkBlockDelta {
                    section_y: -1,
                    local_idx: 0,
                    new_state: 12,
                },
                ChunkBlockDelta {
                    section_y: 0,
                    local_idx: 4095,
                    new_state: 13,
                },
            ],
        },
        ChunkHashSyncPacket::ServerRequestFull {
            dim_id: 1,
            chunks: vec![(7, 8)],
            reason: FullSyncReason::VerificationFailed,
        },
    ]
}

#[test]
fn all_packet_variants_round_trip() {
    for packet in sample_packets() {
        let bytes = encode_ckh1(&packet).unwrap();
        assert_eq!(decode_ckh1(&bytes).unwrap(), packet);
    }
}

#[test]
fn malformed_inputs_fail_without_panicking() {
    let valid = encode_ckh1(&sample_packets()[2]).unwrap();
    for end in 0..valid.len() {
        let result = std::panic::catch_unwind(|| decode_ckh1(&valid[..end]));
        assert!(result.is_ok(), "panic on prefix length {end}");
        assert!(result.unwrap().is_err(), "accepted prefix length {end}");
    }
    let mut trailing = valid.clone();
    trailing.push(0);
    assert_eq!(decode_ckh1(&trailing), Err(Ckh1Error::TrailingBytes));
    let mut bad_magic = valid.clone();
    bad_magic[0] ^= 1;
    assert_eq!(decode_ckh1(&bad_magic), Err(Ckh1Error::BadMagic));
    let mut bad_version = valid.clone();
    bad_version[5] = 2;
    assert_eq!(
        decode_ckh1(&bad_version),
        Err(Ckh1Error::UnknownSchemaVersion(2))
    );
    assert_eq!(
        decode_ckh1(&vec![0; MAX_CKH1_FRAME_BYTES + 1]),
        Err(Ckh1Error::FrameTooLarge)
    );

    // ClientHaveHashes count starts after header, kind, epoch, and dimension.
    let mut too_many_chunks = b"CKH1\0\x01\0\0\x01e".to_vec();
    too_many_chunks.extend_from_slice(&0i32.to_be_bytes());
    too_many_chunks.extend_from_slice(&((MAX_CHUNKS_PER_HASH_FRAME + 1) as u16).to_be_bytes());
    let result = std::panic::catch_unwind(|| decode_ckh1(&too_many_chunks));
    assert!(result.is_ok());
    assert!(result.unwrap().is_err());

    // Delta edit count is checked before a vector is allocated or entries read.
    let mut too_many_edits = b"CKH1\0\x01\x02\0\x01e".to_vec();
    too_many_edits.extend_from_slice(&0i32.to_be_bytes());
    too_many_edits.extend_from_slice(&0i32.to_be_bytes());
    too_many_edits.extend_from_slice(&0i32.to_be_bytes());
    too_many_edits.extend_from_slice(&[0; 64]);
    too_many_edits.extend_from_slice(&((MAX_DELTA_EDITS_PER_CHUNK + 1) as u32).to_be_bytes());
    assert!(matches!(
        decode_ckh1(&too_many_edits),
        Err(Ckh1Error::InvalidValue("delta edit count"))
    ));

    // A minimal delta with one edit whose local_idx is exactly 4096.
    let mut bad_local = b"CKH1\0\x01\x02\0\x01e".to_vec();
    bad_local.extend_from_slice(&0i32.to_be_bytes());
    bad_local.extend_from_slice(&0i32.to_be_bytes());
    bad_local.extend_from_slice(&0i32.to_be_bytes());
    bad_local.extend_from_slice(&[0; 64]);
    bad_local.extend_from_slice(&1u32.to_be_bytes());
    bad_local.extend_from_slice(&0i16.to_be_bytes());
    bad_local.extend_from_slice(&4096u16.to_be_bytes());
    bad_local.extend_from_slice(&1u32.to_be_bytes());
    assert!(matches!(
        decode_ckh1(&bad_local),
        Err(Ckh1Error::InvalidValue("local block index"))
    ));
}

#[test]
fn encode_rejects_oversized_and_noncanonical_values() {
    let mut entries = Vec::new();
    for cx in 0..=MAX_CHUNKS_PER_HASH_FRAME as i32 {
        entries.push(ChunkHashEntry {
            cx,
            cz: 0,
            hash: [0; 32],
        });
    }
    assert!(encode_ckh1(&ChunkHashSyncPacket::ClientHaveHashes {
        worldgen_epoch: "e".into(),
        dim_id: 0,
        entries
    })
    .is_err());
    assert!(encode_ckh1(&ChunkHashSyncPacket::ServerChunkDelta {
        worldgen_epoch: "e".into(),
        dim_id: 0,
        cx: 0,
        cz: 0,
        base_hash: [0; 32],
        new_hash: [0; 32],
        edits: vec![ChunkBlockDelta {
            section_y: 0,
            local_idx: 4096,
            new_state: 1
        }],
    })
    .is_err());
    let duplicate = ChunkHashSyncPacket::ServerChunkMatch {
        dim_id: 0,
        chunks: vec![(1, 2), (1, 2)],
    };
    assert!(encode_ckh1(&duplicate).is_err());
}

fn states(value: u32) -> BTreeMap<i16, Vec<u32>> {
    BTreeMap::from([(0, vec![value; 4096])])
}

struct Provider {
    edits: Vec<ChunkBlockDelta>,
}
impl ChunkDeltaProvider for Provider {
    fn delta_from(
        &self,
        _dim_id: i32,
        _cx: i32,
        _cz: i32,
        _base_hash: &[u8; 32],
        _new_hash: &[u8; 32],
    ) -> Option<Vec<ChunkBlockDelta>> {
        Some(self.edits.clone())
    }
}

fn have(epoch: &str, hash: [u8; 32]) -> ChunkHashSyncPacket {
    ChunkHashSyncPacket::ClientHaveHashes {
        worldgen_epoch: epoch.into(),
        dim_id: 0,
        entries: vec![ChunkHashEntry {
            cx: 2,
            cz: -3,
            hash,
        }],
    }
}

#[test]
fn unchanged_chunk_is_confirmed_without_edits() {
    let local = states(0);
    let local_hash = chunk_merkle_hash(
        "epoch",
        0,
        2,
        -3,
        &[section_state_hash(local[&0].as_slice())],
    );
    let server = ServerChunk {
        cx: 2,
        cz: -3,
        sections: local,
    };
    let result =
        plan_chunk_sync(&have("epoch", local_hash), "epoch", 0, &[server], None, 10).unwrap();
    assert_eq!(
        result,
        vec![ChunkHashSyncPacket::ServerChunkMatch {
            dim_id: 0,
            chunks: vec![(2, -3)]
        }]
    );
}

#[test]
fn small_delta_applies_and_matches_server_hash() {
    let original = states(0);
    let mut updated = original.clone();
    updated.get_mut(&0).unwrap()[17] = 9;
    let base_hash = chunk_merkle_hash("epoch", 0, 2, -3, &[section_state_hash(&original[&0])]);
    let server_hash = chunk_merkle_hash("epoch", 0, 2, -3, &[section_state_hash(&updated[&0])]);
    let server = ServerChunk {
        cx: 2,
        cz: -3,
        sections: updated,
    };
    let provider = Provider {
        edits: vec![ChunkBlockDelta {
            section_y: 0,
            local_idx: 17,
            new_state: 9,
        }],
    };
    let packets = plan_chunk_sync(
        &have("epoch", base_hash),
        "epoch",
        0,
        &[server],
        Some(&provider),
        10,
    )
    .unwrap();
    let delta = &packets[0];
    assert!(
        matches!(delta, ChunkHashSyncPacket::ServerChunkDelta { new_hash, .. } if new_hash == &server_hash)
    );
    let mut client = original;
    apply_verified_chunk_delta("epoch", delta, &mut client).unwrap();
    assert_eq!(client[&0][17], 9);
    assert_eq!(
        chunk_merkle_hash("epoch", 0, 2, -3, &[section_state_hash(&client[&0])]),
        server_hash
    );
}

#[test]
fn tampered_delta_and_wrong_base_leave_client_unchanged() {
    let original = states(0);
    let base_hash = chunk_merkle_hash("epoch", 0, 2, -3, &[section_state_hash(&original[&0])]);
    let mut updated = original.clone();
    updated.get_mut(&0).unwrap()[17] = 9;
    let new_hash = chunk_merkle_hash("epoch", 0, 2, -3, &[section_state_hash(&updated[&0])]);
    let valid = ChunkHashSyncPacket::ServerChunkDelta {
        worldgen_epoch: "epoch".into(),
        dim_id: 0,
        cx: 2,
        cz: -3,
        base_hash,
        new_hash,
        edits: vec![ChunkBlockDelta {
            section_y: 0,
            local_idx: 17,
            new_state: 9,
        }],
    };
    let mut tampered = valid.clone();
    if let ChunkHashSyncPacket::ServerChunkDelta { edits, .. } = &mut tampered {
        edits[0].new_state = 10;
    }
    let mut client = original.clone();
    assert_eq!(
        apply_verified_chunk_delta("epoch", &tampered, &mut client),
        Err(Ckh1Error::PostApplyHashMismatch)
    );
    assert_eq!(client, original);
    let mut wrong_base = valid;
    if let ChunkHashSyncPacket::ServerChunkDelta { base_hash, .. } = &mut wrong_base {
        *base_hash = [0x55; 32];
    }
    assert_eq!(
        apply_verified_chunk_delta("epoch", &wrong_base, &mut client),
        Err(Ckh1Error::BaseHashMismatch)
    );
    assert_eq!(client, original);
}

#[test]
fn epoch_mismatch_and_large_delta_fail_closed_to_full_sync() {
    let client_hash = [1; 32];
    let server = ServerChunk {
        cx: 2,
        cz: -3,
        sections: states(0),
    };
    let epoch_mismatch = plan_chunk_sync(
        &have("old", client_hash),
        "new",
        0,
        std::slice::from_ref(&server),
        None,
        10,
    )
    .unwrap();
    assert_eq!(
        epoch_mismatch,
        vec![ChunkHashSyncPacket::ServerRequestFull {
            dim_id: 0,
            chunks: vec![(2, -3)],
            reason: FullSyncReason::EpochMismatch
        }]
    );

    let provider = Provider {
        edits: vec![
            ChunkBlockDelta {
                section_y: 0,
                local_idx: 0,
                new_state: 1,
            },
            ChunkBlockDelta {
                section_y: 0,
                local_idx: 1,
                new_state: 1,
            },
        ],
    };
    let large = plan_chunk_sync(
        &have("new", client_hash),
        "new",
        0,
        &[server],
        Some(&provider),
        1,
    )
    .unwrap();
    assert_eq!(
        large,
        vec![ChunkHashSyncPacket::ServerRequestFull {
            dim_id: 0,
            chunks: vec![(2, -3)],
            reason: FullSyncReason::DeltaTooLarge
        }]
    );
}
