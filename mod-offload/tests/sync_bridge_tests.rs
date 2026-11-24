// SPDX-License-Identifier: LGPL-3.0-or-later

//! `OffloadSyncBridge` contract tests: external dirty-node deltas stream into
//! kinetic/fluid/transport/ME kernels with unchanged-node skipping, same-batch
//! coalescing, and deterministic apply order — no mod names anywhere.

use mod_offload::kinetic_graph::{KineticGraph, KineticNode, NetworkStatus};
use mod_offload::me_network::{MeNodeKind, StorageCell};
use mod_offload::region_scheduler::{Region, RegionKey};
use mod_offload::sleeping_transport::SleepingTransport;
use mod_offload::sync_bridge::{
    BridgeError, ExternalGraphDelta, MeGraphTarget, OffloadSyncBridge, MAX_INGEST_DELTAS,
};

fn drive_node(capacity: f64, speed: f64) -> KineticNode {
    KineticNode {
        capacity,
        load: 0.0,
        driven_speed: Some(speed),
    }
}

#[test]
fn region_bridge_ingests_kinetic_fluid_and_transport_deltas() {
    let mut bridge = OffloadSyncBridge::new(Region::new(RegionKey::for_block(0, 0, 0)));
    let deltas = vec![
        ExternalGraphDelta::KineticUpsert {
            id: 1,
            node: drive_node(64.0, 32.0),
        },
        ExternalGraphDelta::KineticUpsert {
            id: 2,
            node: KineticNode {
                capacity: 0.0,
                load: 8.0,
                driven_speed: None,
            },
        },
        ExternalGraphDelta::KineticLink {
            a: 1,
            b: 2,
            ratio: 1.0,
        },
        ExternalGraphDelta::FluidTankUpsert {
            id: 10,
            capacity: 1000,
            elevation: 0,
        },
        ExternalGraphDelta::FluidSet {
            id: 10,
            fluid: Some(7),
            amount: 400,
        },
        ExternalGraphDelta::TransportMachine {
            id: 20,
            capacity: 4,
            delay: 2,
        },
        ExternalGraphDelta::TransportInsert { id: 20, item: 99 },
    ];
    let report = bridge.ingest(&deltas).unwrap();
    assert_eq!(report.received, 7);
    assert_eq!(report.applied, 7);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.coalesced, 0);
    assert!(report.failed.is_empty());

    let region = bridge.sink();
    let sol = region.kinetic.solution(2).unwrap();
    assert_eq!(sol.status, NetworkStatus::Running);
    assert_eq!(sol.speed, 32.0);
    assert_eq!(region.fluid().tank(10).unwrap().amount, 400);

    // Re-ingesting the identical batch changes nothing and touches nothing.
    let replay = bridge.ingest(&deltas).unwrap();
    assert_eq!(replay.applied, 1, "only the event delta re-applies");
    assert_eq!(replay.skipped, 6);
    assert_eq!(replay.failed.len(), 0);
}

#[test]
fn coalescing_keeps_last_write_and_apply_order_is_deterministic() {
    // Same logical batch, scrambled arrival order: nodes after links, and
    // two upserts for one node. The bridge must still produce one applied
    // upsert (the last value) after both nodes exist.
    let scrambled = vec![
        ExternalGraphDelta::KineticLink {
            a: 1,
            b: 2,
            ratio: 1.0,
        },
        ExternalGraphDelta::KineticUpsert {
            id: 2,
            node: KineticNode {
                capacity: 0.0,
                load: 2.0,
                driven_speed: None,
            },
        },
        ExternalGraphDelta::KineticUpsert {
            id: 1,
            node: KineticNode {
                capacity: 0.0,
                load: 1.0,
                driven_speed: None,
            },
        },
        ExternalGraphDelta::KineticUpsert {
            id: 1,
            node: drive_node(64.0, 16.0),
        },
    ];
    let ordered = vec![
        ExternalGraphDelta::KineticUpsert {
            id: 1,
            node: drive_node(64.0, 16.0),
        },
        ExternalGraphDelta::KineticUpsert {
            id: 2,
            node: KineticNode {
                capacity: 0.0,
                load: 2.0,
                driven_speed: None,
            },
        },
        ExternalGraphDelta::KineticLink {
            a: 1,
            b: 2,
            ratio: 1.0,
        },
    ];

    let mut a = OffloadSyncBridge::new(KineticGraph::default());
    let mut b = OffloadSyncBridge::new(KineticGraph::default());
    let ra = a.ingest(&scrambled).unwrap();
    let rb = b.ingest(&ordered).unwrap();
    assert_eq!(ra.coalesced, 1, "duplicate node upsert collapses");
    assert_eq!(ra.applied, 3);
    assert!(ra.failed.is_empty());
    assert_eq!(rb.applied, 3);

    // Identical final kernel state regardless of arrival order.
    let sa = a.sink().solution(1).unwrap();
    let sb = b.sink().solution(1).unwrap();
    assert_eq!(sa, sb);
    assert_eq!(sa.speed, 16.0);
    assert_eq!(a.sink().solution(2).unwrap().speed, 16.0);

    // A changed node re-applies; an unchanged one skips.
    let mut changed = vec![
        ExternalGraphDelta::KineticUpsert {
            id: 1,
            node: drive_node(64.0, 16.0),
        },
        ExternalGraphDelta::KineticUpsert {
            id: 2,
            node: KineticNode {
                capacity: 0.0,
                load: 6.0,
                driven_speed: None,
            },
        },
    ];
    let r = a.ingest(&changed).unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(r.skipped, 1);
    changed.push(ExternalGraphDelta::KineticUnlink { a: 1, b: 2 });
    let r = a.ingest(&changed).unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(a.sink().solution(2).unwrap().status, NetworkStatus::Unpowered);
}

#[test]
fn removals_purge_pair_marks_and_failed_deltas_retry() {
    let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
    bridge
        .ingest(&[
            ExternalGraphDelta::KineticUpsert {
                id: 1,
                node: drive_node(8.0, 4.0),
            },
            ExternalGraphDelta::KineticUpsert {
                id: 2,
                node: KineticNode::default(),
            },
            ExternalGraphDelta::KineticLink {
                a: 1,
                b: 2,
                ratio: 1.0,
            },
        ])
        .unwrap();

    // Removing node 1 clears the pair mark: re-linking afterwards must
    // apply (and fail, since node 2's counterpart is gone) rather than skip.
    let r = bridge
        .ingest(&[
            ExternalGraphDelta::KineticRemove { id: 1 },
            ExternalGraphDelta::KineticLink {
                a: 1,
                b: 2,
                ratio: 1.0,
            },
        ])
        .unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(r.failed.len(), 1);
    assert!(matches!(
        r.failed[0].error,
        BridgeError::Kinetic(mod_offload::kinetic_graph::GraphError::MissingNode(_))
    ));

    // The failed link kept no shadow: retrying it fails identically.
    let retry = bridge
        .ingest(&[ExternalGraphDelta::KineticLink {
            a: 1,
            b: 2,
            ratio: 1.0,
        }])
        .unwrap();
    assert_eq!(retry.failed.len(), 1);
    assert_eq!(retry.applied, 0);
}

#[test]
fn me_graph_target_ingests_channels_and_storage_cells() {
    let mut bridge = OffloadSyncBridge::new(MeGraphTarget::default());
    let mut cell = StorageCell::new(7, 5, 4096, 8, 8);
    cell.contents.insert("minecraft:iron_ingot".into(), 64);
    let deltas = vec![
        ExternalGraphDelta::MeNodeUpsert {
            id: 1,
            kind: MeNodeKind::Controller,
        },
        ExternalGraphDelta::MeNodeUpsert {
            id: 2,
            kind: MeNodeKind::NormalCable,
        },
        ExternalGraphDelta::MeNodeUpsert {
            id: 3,
            kind: MeNodeKind::Device {
                requires_channel: true,
            },
        },
        ExternalGraphDelta::MeLink {
            a: 1,
            b: 2,
            linked: true,
        },
        ExternalGraphDelta::MeLink {
            a: 2,
            b: 3,
            linked: true,
        },
        ExternalGraphDelta::MeCellUpsert { cell: cell.clone() },
    ];
    let report = bridge.ingest(&deltas).unwrap();
    assert_eq!(report.applied, 6);
    assert!(report.failed.is_empty());

    let target = bridge.sink();
    let allocation = target.channels.solve_channels();
    assert!(allocation.online_devices.contains(&3));
    assert!(target.storage.available("minecraft:iron_ingot") >= 64);

    // Unchanged replay: everything skips.
    let replay = bridge.ingest(&deltas).unwrap();
    assert_eq!(replay.skipped, 6);
    assert_eq!(replay.applied, 0);

    // Cell contents change -> re-applies; channel topology unchanged -> skips.
    let mut updated = cell.clone();
    updated.contents.insert("minecraft:iron_ingot".into(), 128);
    let r = bridge
        .ingest(&[ExternalGraphDelta::MeCellUpsert { cell: updated }])
        .unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(
        bridge.sink().storage.cell(7).unwrap().contents["minecraft:iron_ingot"],
        128
    );
}

#[test]
fn transport_events_never_dedup_and_machines_skip_unchanged() {
    let mut bridge = OffloadSyncBridge::new(SleepingTransport::default());
    let machine = ExternalGraphDelta::TransportMachine {
        id: 5,
        capacity: 2,
        delay: 3,
    };
    let insert = ExternalGraphDelta::TransportInsert { id: 5, item: 1 };
    let r = bridge.ingest(&[machine.clone(), insert.clone()]).unwrap();
    assert_eq!(r.applied, 2);
    // Same machine spec skips; the insert event applies again by design.
    let r = bridge.ingest(&[machine, insert]).unwrap();
    assert_eq!(r.skipped, 1);
    assert_eq!(r.applied, 1);
    assert_eq!(bridge.sink().machine(5).unwrap().input_items, 2);
}

#[test]
fn oversized_batch_fails_closed() {
    let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
    let deltas = vec![
        ExternalGraphDelta::TransportInsert { id: 0, item: 0 };
        MAX_INGEST_DELTAS + 1
    ];
    assert!(matches!(
        bridge.ingest(&deltas),
        Err(BridgeError::BatchTooLarge { .. })
    ));
}
