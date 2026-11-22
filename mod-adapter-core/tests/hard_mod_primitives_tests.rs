// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use mod_adapter_core::auto_adapter::{
    auto_adapt_jar, auto_adapt_jar_bytes, auto_adapt_mods_dir, AdaptationTier, AutoAdaptOptions,
};
use mod_adapter_core::hard_mod_primitives::*;

static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mc-spine-hard-mod-adaptation-{}-{}",
            std::process::id(),
            NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create scratch mods directory");
        Self(path)
    }

    fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).expect("write synthetic mod JAR");
        path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn synthetic_jar(mod_id: &str) -> Vec<u8> {
    let manifest = format!(r#"{{"id":"{mod_id}","version":"1.0.0","environment":"*"}}"#);
    let (name, data): (&[u8], &[u8]) = if mod_id.is_empty() {
        (b"README.txt", b"no loader manifest")
    } else {
        (b"fabric.mod.json", manifest.as_bytes())
    };
    let size = data.len() as u32;
    let crc = crc32(data);
    let mut out = Vec::new();
    out.extend(0x0403_4b50u32.to_le_bytes());
    out.extend(10u16.to_le_bytes());
    out.extend([0u8; 8]);
    out.extend(crc.to_le_bytes());
    out.extend(size.to_le_bytes());
    out.extend(size.to_le_bytes());
    out.extend((name.len() as u16).to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(name);
    out.extend(data);

    let central_offset = out.len() as u32;
    out.extend(0x0201_4b50u32.to_le_bytes());
    out.extend(20u16.to_le_bytes());
    out.extend(10u16.to_le_bytes());
    out.extend([0u8; 8]);
    out.extend(crc.to_le_bytes());
    out.extend(size.to_le_bytes());
    out.extend(size.to_le_bytes());
    out.extend((name.len() as u16).to_le_bytes());
    out.extend([0u8; 8]);
    out.extend(0u32.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(name);
    let central_size = out.len() as u32 - central_offset;
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(central_size.to_le_bytes());
    out.extend(central_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

#[test]
fn auto_adapter_classifies_carryon_from_manifest() {
    let scratch = ScratchDir::new();
    let path = scratch.write("renamed.jar", &synthetic_jar("carryon"));
    let result = auto_adapt_jar(&path, &AutoAdaptOptions::default()).unwrap();
    assert_eq!(result.mod_id(), Some("carryon"));
    assert_eq!(
        result.engine_compat,
        EngineCompatDisposition::NativeGameplayPrimitive {
            engine: "CarryOnEngine"
        }
    );
    assert!(result.to_json().contains("\"engine\": \"CarryOnEngine\""));
    assert!(result
        .format_human()
        .contains("native_gameplay_primitive:CarryOnEngine"));
}

#[test]
fn auto_adapter_classifies_vulkan_and_filename_fallback() {
    let result = auto_adapt_jar_bytes(
        &synthetic_jar("vulkanmod"),
        "otherwise-unknown.jar",
        &AutoAdaptOptions::default(),
    )
    .unwrap();
    assert_eq!(result.tier, AdaptationTier::AutoSkipped);
    assert_eq!(
        result.engine_compat,
        EngineCompatDisposition::NativeAccelerated {
            native_subsystem: "wgpu_render_backend"
        }
    );
    assert!(result
        .to_json()
        .contains("\"native_subsystem\": \"wgpu_render_backend\""));
    assert!(result
        .format_human()
        .contains("native_accelerated:wgpu_render_backend"));

    // Filename-inferred vulkanmod/nvidium JARs are skipped before bytecode read
    // and cleanly fall back to the native wgpu renderer.
    let by_name =
        auto_adapt_jar_bytes(&[], "vulkanmod-1.21.1.jar", &AutoAdaptOptions::default()).unwrap();
    assert_eq!(by_name.tier, AdaptationTier::AutoSkipped);
    assert_eq!(
        by_name.engine_compat,
        EngineCompatDisposition::NativeAccelerated {
            native_subsystem: "wgpu_render_backend"
        }
    );

    let fallback = auto_adapt_jar_bytes(
        &synthetic_jar(""),
        "sound_physics_remastered-1.0.jar",
        &AutoAdaptOptions::default(),
    )
    .unwrap();
    assert_eq!(
        fallback.engine_compat,
        EngineCompatDisposition::CoexistResourceBridge {
            bridge: "audio_occlusion_bridge"
        }
    );
    assert_eq!(fallback.tier, AdaptationTier::Tier3FallbackRequired);

    let unknown = auto_adapt_jar_bytes(
        &synthetic_jar(""),
        "unknown-addon.jar",
        &AutoAdaptOptions::default(),
    )
    .unwrap();
    assert_eq!(
        unknown.engine_compat,
        EngineCompatDisposition::StandardModAdapter
    );
    assert!(unknown.requires_fallback());
    assert!(
        auto_adapt_jar_bytes(&[], "corrupt-addon.jar", &AutoAdaptOptions::default()).is_err(),
        "malformed archives must still fail closed"
    );
}

#[test]
fn mods_dir_surfaces_hard_mod_strategies_including_distant_horizons() {
    let scratch = ScratchDir::new();
    let cases = [
        (
            "carryon",
            EngineCompatDisposition::NativeGameplayPrimitive {
                engine: "CarryOnEngine",
            },
        ),
        (
            "sophisticatedbackpacks",
            EngineCompatDisposition::NativeGameplayPrimitive {
                engine: "BackpackGuard",
            },
        ),
        (
            "gravestone",
            EngineCompatDisposition::NativeGameplayPrimitive {
                engine: "GraveStore",
            },
        ),
        (
            "waystones",
            EngineCompatDisposition::NativeGameplayPrimitive {
                engine: "WaystoneNetwork",
            },
        ),
        (
            "pipez",
            EngineCompatDisposition::NativeGameplayPrimitive {
                engine: "CollapsedLogisticsNetwork",
            },
        ),
        (
            "vulkanmod",
            EngineCompatDisposition::NativeAccelerated {
                native_subsystem: "wgpu_render_backend",
            },
        ),
        (
            "nvidium",
            EngineCompatDisposition::NativeAccelerated {
                native_subsystem: "mesh_shader_renderer",
            },
        ),
        (
            "sodium",
            EngineCompatDisposition::NativeAccelerated {
                native_subsystem: "chunk_renderer",
            },
        ),
        (
            "ferritecore",
            EngineCompatDisposition::NativeAccelerated {
                native_subsystem: "compact_registry_storage",
            },
        ),
        (
            "iris",
            EngineCompatDisposition::CoexistResourceBridge {
                bridge: "shader_pipeline_bridge",
            },
        ),
        (
            "distanthorizons",
            EngineCompatDisposition::CoexistResourceBridge {
                bridge: "lod_stream_bridge",
            },
        ),
        (
            "sound_physics_remastered",
            EngineCompatDisposition::CoexistResourceBridge {
                bridge: "audio_occlusion_bridge",
            },
        ),
    ];
    for (index, (mod_id, _)) in cases.iter().enumerate() {
        scratch.write(&format!("fixture-{index:02}.jar"), &synthetic_jar(mod_id));
    }

    let report = auto_adapt_mods_dir(&scratch.0, &AutoAdaptOptions::default()).unwrap();
    assert_eq!(report.jars.len(), cases.len());
    for (result, (mod_id, disposition)) in report.jars.iter().zip(cases) {
        assert_eq!(result.mod_id(), Some(mod_id));
        assert_eq!(result.engine_compat, disposition, "mod id {mod_id}");
        assert!(result.to_json().contains("\"engine_compat\""));
    }
    let distant_horizons = &report.jars[10];
    assert_eq!(distant_horizons.tier, AdaptationTier::AutoSkipped);
    assert!(distant_horizons
        .audit_notes
        .iter()
        .any(|note| note.contains("coexist_resource_bridge:lod_stream_bridge")));
    assert!(report
        .to_json()
        .contains("\"bridge\": \"lod_stream_bridge\""));
    assert!(report.format_human().contains("coexist_resource_bridge:lod_stream_bridge"));
}

fn chest(slots: Vec<(u16, String, u32)>) -> CarriedPayload {
    CarriedPayload::Block {
        pos_origin: [8, 64, -3],
        block_id: "minecraft:chest".into(),
        state_id: 101,
        nbt_bytes: vec![10, 0, 3, 5, 255, 7],
        inventory_slots: slots,
    }
}

fn pet() -> CarriedPayload {
    CarriedPayload::Entity {
        entity_eid: 17,
        entity_type: "minecraft:wolf".into(),
        custom_name: Some("Companion".into()),
        health: 17.5,
        nbt_bytes: vec![10, 2, 3, 4],
        inventory_items: vec![("minecraft:bone".into(), 5)],
    }
}

#[test]
fn carry_on_preserves_filled_chest_and_applies_weight_penalty() {
    let filled = chest(vec![
        (0, "minecraft:diamond".into(), 33),
        (8, "minecraft:iron_ingot".into(), 64),
        (26, "minecraft:apple".into(), 12),
    ]);
    let empty = chest(vec![]);
    let heavy = compute_carry_physics(&filled);
    let light = compute_carry_physics(&empty);
    assert!(heavy.mass_score > light.mass_score);
    assert!(heavy.speed_multiplier < light.speed_multiplier);
    assert!((0.35..=0.85).contains(&heavy.speed_multiplier));
    assert!(!heavy.allow_sprint);
    assert!(heavy.two_handed_pose);
    assert!(heavy.exhaustion_multiplier > light.exhaustion_multiplier);

    let engine = CarryOnEngine::new();
    assert_eq!(
        engine.try_pickup_block(1, filled.clone(), true, true, true, &[], false),
        Ok(filled.clone())
    );
    assert_eq!(
        engine.try_pickup_block(2, filled.clone(), true, true, true, &[], false),
        Err(CarryRejectReason::TargetAlreadyClaimed)
    );
    assert_eq!(
        engine.try_place_carried(1, [9, 64, -3], false),
        Err(CarryRejectReason::PlacementBlocked)
    );
    assert!(engine.is_carrying(1));
    assert_eq!(
        engine.try_place_carried(1, [9, 64, -3], true),
        Ok(filled.clone())
    );
    assert!(!engine.is_carrying(1));
    assert_eq!(
        engine.try_place_carried(1, [9, 64, -3], true),
        Err(CarryRejectReason::NoCarriedPayload)
    );
    assert_eq!(
        engine.try_pickup_block(2, filled.clone(), true, true, true, &[], false),
        Ok(filled)
    );
}

#[test]
fn carry_on_rejects_protected_blocks_tags_halves_and_invalid_interactions() {
    for block in [
        "minecraft:bedrock",
        "minecraft:obsidian",
        "minecraft:crying_obsidian",
        "minecraft:reinforced_deepslate",
        "minecraft:end_portal_frame",
        "minecraft:end_portal",
        "minecraft:nether_portal",
        "minecraft:command_block",
        "minecraft:barrier",
        "minecraft:structure_block",
        "minecraft:jigsaw",
    ] {
        assert_eq!(
            is_carry_forbidden_block(block, &[], false),
            Some(CarryRejectReason::ForbiddenImmovableBlock)
        );
    }
    for tag in [
        "#c:relocation_not_supported",
        "#forge:immovable",
        "#carryon:block_blacklist",
    ] {
        assert_eq!(
            is_carry_forbidden_block("minecraft:chest", &[tag], false),
            Some(CarryRejectReason::ForbiddenTag)
        );
    }
    assert_eq!(
        is_carry_forbidden_block("minecraft:oak_door", &[], true),
        Some(CarryRejectReason::ForbiddenMultiBlockHalf)
    );
    let engine = CarryOnEngine::new();
    let payload = chest(vec![]);
    for (hands, sneaking, reach, expected) in [
        (false, true, true, CarryRejectReason::HandsNotEmpty),
        (true, false, true, CarryRejectReason::NotSneaking),
        (true, true, false, CarryRejectReason::OutOfReach),
    ] {
        assert_eq!(
            engine.try_pickup_block(1, payload.clone(), hands, sneaking, reach, &[], false),
            Err(expected)
        );
    }
    assert_eq!(
        engine.try_pickup_block(
            1,
            payload.clone(),
            true,
            true,
            true,
            &["#forge:immovable"],
            false
        ),
        Err(CarryRejectReason::ForbiddenTag)
    );
    assert_eq!(
        engine.try_pickup_block(1, payload.clone(), true, true, true, &[], true),
        Err(CarryRejectReason::ForbiddenMultiBlockHalf)
    );
    assert!(!engine.is_carrying(1));
}

#[test]
fn carry_on_concurrent_claim_is_single_winner_and_emergency_drop_is_idempotent() {
    let registry = Arc::new(CarryRegistry::new());
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = (1..=2)
        .map(|player_id| {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                (
                    player_id,
                    registry.try_pickup_block(
                        player_id,
                        chest(vec![]),
                        true,
                        true,
                        true,
                        &[],
                        false,
                    ),
                )
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| result == &Err(CarryRejectReason::TargetAlreadyClaimed))
            .count(),
        1
    );
    let winner = results.iter().find(|(_, result)| result.is_ok()).unwrap().0;
    assert_eq!(
        registry.emergency_drop_on_death_or_disconnect(winner, [8, 65, -3]),
        Some(([8, 65, -3], chest(vec![])))
    );
    assert_eq!(
        registry.emergency_drop_on_death_or_disconnect(winner, [8, 65, -3]),
        None
    );
}

#[test]
fn live_entity_carry_preserves_pet_identity_and_rejects_double_pickup() {
    let registry = CarryRegistry::new();
    let payload = pet();
    assert_eq!(
        registry.try_pickup_entity(11, payload.clone(), true, true, true, false),
        Ok(payload.clone())
    );
    assert_eq!(
        registry.try_pickup_entity(12, payload.clone(), true, true, true, false),
        Err(CarryRejectReason::TargetAlreadyClaimed)
    );
    assert_eq!(
        registry.try_place_carried(11, [9, 70, 12], true),
        Ok(payload.clone())
    );
    assert_eq!(
        registry.try_pickup_entity(12, payload.clone(), true, true, true, false),
        Ok(payload.clone())
    );
    assert_eq!(
        registry.try_pickup_entity(13, pet(), true, true, true, true),
        Err(CarryRejectReason::EntityTooLargeOrHostile)
    );
    assert_eq!(
        registry.emergency_drop_on_death_or_disconnect(12, [9, 71, 12]),
        Some(([9, 71, 12], payload))
    );
    assert!(registry
        .emergency_drop_on_death_or_disconnect(12, [9, 71, 12])
        .is_none());
}

#[test]
fn backpack_guard_blocks_recursive_bombs_but_accepts_ordinary_items() {
    assert_eq!(
        validate_backpack_insertion(
            "sophisticatedbackpacks:backpack",
            "sophisticatedbackpacks:backpack",
            0,
            5
        ),
        Err(BackpackRejectReason::SelfNesting)
    );
    assert_eq!(
        validate_backpack_insertion("sophisticatedbackpacks:backpack", "travel:backpack", 0, 0),
        Err(BackpackRejectReason::NestedContainerDisallowed)
    );
    assert_eq!(
        validate_backpack_insertion("sophisticatedbackpacks:backpack", "travel:backpack", 4, 4),
        Err(BackpackRejectReason::NestingDepthExceeded)
    );
    assert_eq!(
        validate_backpack_insertion("sophisticatedbackpacks:backpack", "minecraft:apple", 0, 0),
        Ok(())
    );
    assert_eq!(
        validate_backpack_insertion("sophisticatedbackpacks:backpack", "travel:backpack", 0, 2),
        Ok(())
    );
}

#[test]
fn void_death_grave_clamps_up_reclaims_once_and_enforces_owner() {
    let mut graves = GraveStore::new();
    let contents = vec![
        ("minecraft:diamond".into(), 64),
        ("minecraft:netherite_sword".into(), 1),
    ];
    let record = graves
        .create_grave(
            101,
            "minecraft:the_end",
            [35, -120, 50],
            -64,
            319,
            contents.clone(),
            70,
        )
        .unwrap();
    assert_eq!(record.position, [35, -63, 50]);
    assert!(record.needs_void_platform);
    assert_eq!(
        graves.reclaim_grave(record.grave_id, 202, false),
        Err(GraveError::Unauthorized)
    );
    assert_eq!(
        graves.reclaim_grave(record.grave_id, 101, false),
        Ok((contents, 70))
    );
    assert_eq!(
        graves.reclaim_grave(record.grave_id, 101, true),
        Err(GraveError::AlreadyReclaimed)
    );
    let stored = graves.get_grave(record.grave_id).unwrap();
    assert!(stored.reclaimed && stored.items.is_empty());
    assert_eq!(stored.xp_points, 0);

    let dry = graves
        .create_grave(101, "minecraft:overworld", [0, 85, 0], -64, 319, vec![], 0)
        .unwrap();
    assert!(!dry.needs_void_platform);
    assert_eq!(dry.position, [0, 85, 0]);
    assert_eq!(
        graves.reclaim_grave(dry.grave_id, 202, true),
        Ok((vec![], 0))
    );
    assert_eq!(
        graves.reclaim_grave(100_000, 101, true),
        Err(GraveError::MissingGrave)
    );
}

#[test]
fn multiblock_deforms_whole_3d_cluster_and_drops_buffer_once() {
    let mut registry = MultiBlockClusterRegistry::new();
    let members: Vec<_> = (0..3)
        .flat_map(|x| (0..3).flat_map(move |y| (0..3).map(move |z| [x, y, z])))
        .collect();
    let id = registry
        .form_cluster([1, 1, 1], "example:machine", members, 3)
        .unwrap();
    assert_eq!(id, 1);
    for position in [[0, 0, 0], [2, 2, 2], [1, 1, 1]] {
        assert_eq!(registry.controller_at(position), Some([1, 1, 1]));
    }
    assert_eq!(
        registry.form_cluster([0, 0, 0], "example:overlap", vec![], 0),
        Err(ClusterError::PositionAlreadyOccupied)
    );
    let buffer = vec![
        ("minecraft:iron_ingot".into(), 64),
        ("minecraft:coal".into(), 32),
    ];
    registry
        .set_buffer_items([2, 2, 2], buffer.clone())
        .unwrap();
    assert_eq!(registry.on_block_broken([0, 0, 0]), Some(buffer));
    assert_eq!(registry.on_block_broken([2, 2, 2]), None);
    assert_eq!(registry.controller_at([1, 1, 1]), None);
    assert_eq!(
        registry.set_buffer_items([1, 1, 1], vec![]),
        Err(ClusterError::MissingCluster)
    );
}

#[test]
fn waystones_require_known_source_and_target_and_charge_cross_dimension_xp() {
    let mut network = WaystoneNetwork::new();
    let start = network
        .register("Home", "minecraft:overworld", [0, 70, 0])
        .unwrap();
    let nearby = network
        .register("Village", "minecraft:overworld", [20, 70, 20])
        .unwrap();
    let far = network
        .register("Portal", "minecraft:the_nether", [4000, 64, 0])
        .unwrap();
    assert_eq!(
        network.travel_cost(7, start, nearby),
        Err(WaystoneError::Undiscovered)
    );
    network.discover(7, start).unwrap();
    network.discover(7, nearby).unwrap();
    assert_eq!(network.travel_cost(7, start, nearby).unwrap().xp_points, 0);
    assert_eq!(
        network.travel_cost(7, start, far),
        Err(WaystoneError::Undiscovered)
    );
    network.discover(7, far).unwrap();
    let cost = network.travel_cost(7, start, far).unwrap();
    assert!(cost.xp_points >= 15);
    assert!(
        cost.cooldown_ticks
            > network
                .travel_cost(7, start, nearby)
                .unwrap()
                .cooldown_ticks
    );
    assert_eq!(
        network.travel_cost(7, start, 500),
        Err(WaystoneError::MissingWaystone)
    );
}

fn source(id: u64, kind: LogisticsResource, amount: u64) -> ExtractorEndpoint {
    ExtractorEndpoint {
        endpoint_id: id,
        position: [0, 64, 0],
        resource: kind,
        available: amount,
    }
}

fn sink(id: u64, x: i32, kind: LogisticsResourceKind, capacity: u64) -> InserterEndpoint {
    InserterEndpoint {
        endpoint_id: id,
        position: [x, 64, 0],
        accepted_kind: kind,
        remaining_capacity: capacity,
        filter: None,
    }
}

fn iron() -> LogisticsResource {
    LogisticsResource::Item {
        item_id: "minecraft:iron_ingot".into(),
        tags: vec!["c:ingots/iron".into()],
    }
}

#[test]
fn collapsed_round_robin_rebalances_partial_receivers_without_loss() {
    let mut network = CollapsedLogisticsNetwork::new();
    network.add_extractor(source(1, iron(), 100)).unwrap();
    network
        .add_inserter(sink(2, 10, LogisticsResourceKind::Item, 1))
        .unwrap();
    network
        .add_inserter(sink(3, 20, LogisticsResourceKind::Item, 100))
        .unwrap();
    network
        .add_inserter(sink(4, 30, LogisticsResourceKind::Item, 1))
        .unwrap();
    let receipt = network.route(1, RoutingMode::RoundRobin).unwrap();
    assert_eq!(
        receipt.extracted,
        receipt.total_inserted() + receipt.returned_to_source
    );
    assert_eq!(receipt.total_inserted(), 100);
    assert_eq!(network.extractor_available(1), Some(0));
    assert_eq!(network.inserter_remaining_capacity(2), Some(0));
    assert_eq!(network.inserter_remaining_capacity(3), Some(2));
    assert_eq!(network.inserter_remaining_capacity(4), Some(0));
    network.refill_extractor(1, 20).unwrap();
    let second = network.route(1, RoutingMode::RoundRobin).unwrap();
    assert_eq!(
        second.extracted,
        second.total_inserted() + second.returned_to_source
    );
    assert_eq!(second.total_inserted(), 2);
    assert_eq!(second.returned_to_source, 18);
}

#[test]
fn collapsed_nearest_first_respects_filters_and_conserves_items_fluids_and_fe() {
    let mut network = CollapsedLogisticsNetwork::new();
    network.add_extractor(source(1, iron(), 64)).unwrap();
    let mut wrong = sink(2, 1, LogisticsResourceKind::Item, 200);
    wrong.filter = Some(RoutingFilter::ItemPrefix("minecraft:gold".into()));
    network.add_inserter(wrong).unwrap();
    let mut tagged = sink(3, 2, LogisticsResourceKind::Item, 7);
    tagged.filter = Some(RoutingFilter::ItemTag("#c:ingots/iron".into()));
    network.add_inserter(tagged).unwrap();
    network
        .add_inserter(sink(4, 3, LogisticsResourceKind::Item, 20))
        .unwrap();
    let receipt = network.route(1, RoutingMode::NearestFirst).unwrap();
    assert_eq!(receipt.inserted, vec![(3, 7), (4, 20)]);
    assert_eq!(receipt.returned_to_source, 37);
    assert_eq!(
        receipt.extracted,
        receipt.total_inserted() + receipt.returned_to_source
    );
    assert_eq!(network.inserter_remaining_capacity(2), Some(200));

    for (kind, resource) in [
        (
            LogisticsResourceKind::Fluid,
            LogisticsResource::Fluid {
                fluid_id: "minecraft:water".into(),
            },
        ),
        (LogisticsResourceKind::EnergyFe, LogisticsResource::EnergyFe),
    ] {
        let mut transport = CollapsedLogisticsNetwork::new();
        transport.add_extractor(source(1, resource, 500)).unwrap();
        transport.add_inserter(sink(2, 1, kind, 120)).unwrap();
        transport.add_inserter(sink(3, 2, kind, 180)).unwrap();
        let receipt = transport.route(1, RoutingMode::NearestFirst).unwrap();
        assert_eq!(receipt.extracted, 500);
        assert_eq!(receipt.total_inserted(), 300);
        assert_eq!(receipt.returned_to_source, 200);
        assert_eq!(transport.extractor_available(1), Some(200));
    }
}

#[test]
fn classifier_covers_carryon_vulkan_ultralight_and_bridge_archetypes() {
    use EngineCompatDisposition::*;
    for (id, engine) in [
        ("carryon", "CarryOnEngine"),
        ("waystones", "WaystoneNetwork"),
        ("gravestone", "GraveStore"),
        ("yigd", "GraveStore"),
        ("pipez", "CollapsedLogisticsNetwork"),
    ] {
        assert_eq!(
            classify_perf_or_render_mod_compat(id),
            NativeGameplayPrimitive { engine }
        );
    }
    for (id, native_subsystem) in [
        ("vulkanmod", "wgpu_render_backend"),
        ("nvidium", "mesh_shader_renderer"),
        ("sodium", "chunk_renderer"),
        ("embeddium", "chunk_renderer"),
        ("ferritecore", "compact_registry_storage"),
        ("lithium", "entity_block_scheduler"),
        ("c2me", "chunk_task_scheduler"),
        ("modernfix", "resource_and_allocation_cache"),
        ("immediatelyfast", "batched_immediate_renderer"),
        ("entityculling", "occlusion_culling"),
        ("moreculling", "occlusion_culling"),
        ("starlight", "light_engine"),
        ("scalablelux", "light_engine"),
        ("krypton", "network_transport"),
    ] {
        assert_eq!(
            classify_perf_or_render_mod_compat(id),
            NativeAccelerated { native_subsystem }
        );
    }
    for (id, bridge) in [
        ("iris", "shader_pipeline_bridge"),
        ("oculus", "shader_pipeline_bridge"),
        ("distanthorizons", "lod_stream_bridge"),
        ("voxy", "lod_stream_bridge"),
        ("continuity", "connected_textures_bridge"),
        ("lambdynamiclights", "dynamic_lights_bridge"),
        ("sound_physics_remastered", "audio_occlusion_bridge"),
    ] {
        assert_eq!(
            classify_perf_or_render_mod_compat(id),
            CoexistResourceBridge { bridge }
        );
    }
    assert_eq!(
        classify_perf_or_render_mod_compat("unknownmod"),
        StandardModAdapter
    );
    assert_eq!(
        classify_perf_or_render_mod_compat("CarryOn"),
        NativeGameplayPrimitive {
            engine: "CarryOnEngine"
        }
    );
}
