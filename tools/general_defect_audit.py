#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""General permanent defect-class audit for the Megu workspace.

The guards are deliberately textual, deterministic, and fast.  They do not
try to compile Rust; each one encodes a narrow invariant that previously
escaped review.  Existing grandfathered instances are pinned as baselines so
this audit is clean on the audited tree while any new instance fails closed.

Run: python3 tools/general_defect_audit.py [--strict] [--json] [--root DIR]
"""

import argparse
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tokenize

REPO_ROOT = Path(__file__).resolve().parents[1]

RUST_SCOPES = (
    "core/src",
    "proto/src",
    "client-ui/src",
    "client-port/src",
    "client-app/src",
    "client-gpu/src",
    "server-files/src",
    "state-store/src",
    "create-adapter/src",
    "src",
)
WORKSPACE_LIBRS = (
    "core/src/lib.rs",
    "proto/src/lib.rs",
    "client-port/src/lib.rs",
    "client-ui/src/lib.rs",
    "client-app/src/lib.rs",
    "server-files/src/lib.rs",
    "state-store/src/lib.rs",
    "create-adapter/src/lib.rs",
)
EXCLUDED_PARTS = {".git", "target", "node_modules", "evidence"}

# Small, documented synthetic identities used by portable tests.  Real home
# directory owners are never added here; pre-existing instances are baseline
# fingerprints instead of silently widening this list.
SYNTHETIC_HOME_USERS = {
    "alice",
    "bob",
    "user",
    "player",
    "runner",
    "runneradmin",
    "sandbox",
    "example",
    "test",
    "demo",
    "megu",
    "steve",
    "alex",
}

# Baselines captured on origin/main.  They are exact occurrence/type/name
# pins, not broad suppressions: additions, moves into new files, or duplicate
# introductions are findings.
# sha256(path + NUL + matched-path), truncated to 16 hex digits.  Pinning
# digests avoids copying real local account names into this audit while still
# making the baseline exact rather than a count-only suppression.
PERSONAL_PATH_BASELINE_DIGESTS = {
    "2eeeb9156c480f4e": 1,
    "a92a15c2eeb67048": 11,
    "61e4225d267db681": 1,
    "c896a707b126c320": 2,
    "a7e098b637ecfcc8": 1,
}

UNSAFE_BASELINE_COUNTS = {'client-app/src/audio_out.rs': 25,
 'client-app/src/hardware_profile.rs': 2,
 'client-app/src/world.rs': 4,
 'client-gpu/src/lib.rs': 4,
 # One documented zero-copy cast from the dependency-free mesh crate's
 # repr(C) quad record; both size and alignment are asserted at compile time.
 'client-gpu/src/planar_quad_pulling.rs': 1,
 'client-port/src/bin/lod_bench.rs': 8,
 'client-port/src/bin/mesh_bench.rs': 2,
 'client-port/src/ffi.rs': 46,
 'client-port/src/mesh.rs': 27,
 'client-port/src/tests.rs': 24,
 'core/src/hw_adaptive_engine.rs': 1,
 'core/src/simd_voxel.rs': 24,
 'core/src/sys_metrics.rs': 2,
 'core/src/task_pool.rs': 4,
 'proto/src/friend_relay.rs': 1,
 'src/generation_math.rs': 6,
 'src/java_compat.rs': 7,
 'src/main.rs': 1,
 'src/natural_spawner.rs': 1,
 'src/operator_ux.rs': 2,
 'src/worldgen.rs': 23}
UNSAFE_UNDOCUMENTED_BASELINE_COUNTS = {
    'client-app/src/audio_out.rs': 2,
    'client-app/src/hardware_profile.rs': 2,
    'client-app/src/world.rs': 4,
    'client-port/src/bin/lod_bench.rs': 4,
    'client-port/src/ffi.rs': 1,
    'client-port/src/mesh.rs': 7,
    'client-port/src/tests.rs': 22,
    'core/src/simd_voxel.rs': 15,
    'proto/src/friend_relay.rs': 1,
    'src/generation_math.rs': 3,
    'src/java_compat.rs': 7,
    'src/main.rs': 1,
    'src/natural_spawner.rs': 1,
    'src/operator_ux.rs': 2,
    'src/worldgen.rs': 10,
}
UNSAFE_EXPLICIT_ALLOWLIST = {
    "server-files/src/landlock_sandbox.rs",
    "core/src/landlock_sandbox.rs",
    "sdk/megu-mod-sdk/src/native_abi.rs",
}

REMOVE_ZERO_BASELINE_COUNT = 5
CODEC_BASELINE_COUNTS = {"sha256": 6, "crc32": 8, "lz4": 0}
CANONICAL_CODEC_FILES = {"lz4": "core/src/lz4.rs", "crc32": "core/src/hash.rs"}
HOT_LOOP_ALLOC_BASELINE = {
    "client-gpu/src/renderer.rs": 1,
    "client-gpu/src/window.rs": 0,
}
UNBOUNDED_GLOBAL_BASELINE_COUNTS = {'src/anticheat.rs:P:OnceLock<Mutex<HashMap<i32, PlayerAc>>>': 1,
 "src/chest_loot.rs:BODIES:OnceLock<Mutex<HashMap<String, &'static str>>>": 1,
 'src/anticheat.rs:R:OnceLock<Mutex<HashMap<i32, u32>>>': 1,
 'src/anvil_io.rs:ANVIL_BLOCK_ENTITIES:Registry<DimBlockPos, AnvilNbt>': 1,
 'src/anvil_io.rs:ANVIL_BLOCK_TOMBSTONES:Registry<DimChunkKey, HashMap<i8, HashMap<u16, AnvilNbt>>>': 1,
 'src/anvil_io.rs:ANVIL_CHUNK_PRESERVED:Registry< DimChunkKey, std::collections::BTreeMap<crate::nbt::NbtString, AnvilNbt>, >': 1,
 'src/anvil_io.rs:ANVIL_PLAYER_PRESERVED:Registry< [u8; 16], std::collections::BTreeMap<crate::nbt::NbtString, AnvilNbt>, >': 1,
 'src/anvil_io.rs:ANVIL_SECTION_BIOMES:Registry<DimChunkKey, HashMap<i8, AnvilSectionBiomes>>': 1,
 'src/combat_and_health.rs:AGES:OnceLock<Mutex<HashMap<i32, u16>>>': 1,
 'src/combat_and_health.rs:ATTACK_TICKERS:OnceLock<Mutex<HashMap<i32, AttackTicker>>>': 1,
 'src/combat_and_health.rs:BONUSES:OnceLock<Mutex<HashMap<String, f32>>>': 1,
 'src/combat_and_health.rs:ENTITY_DIMS:OnceLock<RwLock<HashMap<i32, DimId>>>': 1,
 'src/combat_and_health.rs:MACE_SMASHES:OnceLock<Mutex<HashMap<i32, MaceSmash>>>': 1,
 'src/combat_and_health.rs:MOB_FIRE_TICKS:OnceLock<Mutex<HashMap<i32, u32>>>': 1,
 'src/combat_and_health.rs:R:OnceLock<Mutex<HashMap<String, HashMap<String, f64>>>>': 1,
 'src/combat_and_health.rs:SESSIONS:Registry<i32, PlayerMechanismSession>': 1,
 'src/combat_and_health.rs:SHIELD_RAISED_AT:OnceLock<Mutex<HashMap<i32, u64>>>': 1,
 'src/combat_and_health.rs:XP_PENDING:OnceLock<Mutex<HashSet<i32>>>': 1,
 'src/commands_core.rs:DISABLED_DATAPACKS:OnceLock<Mutex<HashSet<String>>>': 1,
 'src/commands_core.rs:EFFECTS:OnceLock<Mutex<HashMap<i32, Vec<Eff>>>>': 1,
 'src/commands_core.rs:FORCELOADED:OnceLock<Mutex<HashSet<(i32, i32)>>>': 1,
 'src/commands_core.rs:R:OnceLock<Mutex<HashMap<String, Advancement>>>': 1,
 'src/commands_core.rs:R:OnceLock<Mutex<HashMap<String, Bossbar>>>': 1,
 'src/commands_core.rs:R:OnceLock<Mutex<HashMap<String, HashSet<String>>>>': 1,
 'src/commands_core.rs:R:OnceLock<Mutex<HashMap<String, String>>>': 1,
 'src/commands_core.rs:R:OnceLock<Mutex<HashMap<String, Vec<LootFunc>>>>': 1,
 'src/commands_core.rs:R:OnceLock<Mutex<HashMap<String, Vec<LootPool>>>>': 1,
 'src/commands_core.rs:R:OnceLock<Mutex<HashMap<i64, Vec<LootPool>>>>': 1,
 "src/commands_core.rs:RULES:OnceLock<Mutex<HashMap<&'static str, i32>>>": 1,
 'src/commands_core.rs:T:OnceLock<Mutex<HashMap<String, HashSet<String>>>>': 1,
 'src/commands_core.rs:T:OnceLock<Mutex<HashMap<String, Vec<String>>>>': 1,
 'src/commands_execute.rs:R:OnceLock<Mutex<HashMap<String, String>>>': 1,
 'src/commands_execute.rs:STORAGE:Registry<String, ExecTag>': 1,
 'src/datapack.rs:C:OnceLock<Mutex<HashMap<i32, i32>>>': 1,
 'src/effects.rs:STATES:OnceLock<Mutex<HashMap<i32, EffectTickState>>>': 1,
 "src/entity_registry.rs:INDEX:std::sync::OnceLock<std::collections::HashMap<&'static str, usize>>": 1,
 'src/feature.rs:FEAT_PROF:std::sync::OnceLock<std::sync::Mutex<HashMap<String, (u128, u64)>>>': 1,
 'src/jigsaw_structures.rs:LIVE_JIGSAW_TEMPLATES:std::cell::RefCell<HashMap<String, std::sync::Arc<Template>>>': 1,
 'src/jigsaw_structures.rs:ROTATED_JIGSAWS:std::cell::RefCell< HashMap<(usize, u8), (std::sync::Weak<Template>, Vec<std::sync::Arc<BlockInfo>>)>, >': 1,
 'src/main.rs:ACTORS:OnceLock<Mutex<HashMap<String, Arc<Actor>>>>': 1,
 'src/main.rs:AGES:OnceLock<Mutex<HashMap<i32, i32>>>': 1,
 'src/main.rs:AI:OnceLock<Mutex<HashMap<i32, W1LiveMobAi>>>': 1,
 'src/main.rs:BACKOFF:Registry<DimBlockPos, (u64, u32)>': 1,
 'src/main.rs:CROPS:Registry<DimBlockPos, u8>': 1,
 'src/main.rs:DIGESTS:OnceLock<Mutex<HashMap<(std::path::PathBuf, String), u32>>>': 1,
 'src/main.rs:DONE:OnceLock<Mutex<HashSet<DimChunkKey>>>': 1,
 'src/main.rs:EMPTY_HIDDEN:OnceLock<HashSet<u32>>': 1,
 'src/main.rs:ENGINES:OnceLock<Mutex<HashMap<DimId, LiveLightEngine>>>': 1,
 'src/main.rs:ENTITY_TETHERS:OnceLock<Mutex<HashMap<i32, EntityTether>>>': 1,
 'src/main.rs:FROZEN:OnceLock<Mutex<HashMap<i32, u64>>>': 1,
 'src/main.rs:GLIDE:OnceLock<Mutex<HashMap<i32, u32>>>': 1,
 'src/main.rs:HIDDEN:OnceLock<HashSet<u32>>': 1,
 'src/main.rs:HIDDEN_EXTRA:OnceLock<Mutex<HashSet<u32>>>': 1,
 'src/main.rs:LAST_MOUTHS:OnceLock<Mutex<HashMap<i32, Stack>>>': 1,
 'src/main.rs:LAVAS:OnceLock<Mutex<std::collections::HashSet<(i32, i32, i32)>>>': 1,
 'src/main.rs:LEASH_KNOTS:OnceLock<Mutex<HashMap<i32, (DimId, BlockPos)>>>': 1,
 'src/main.rs:LIGHTNING_RODS:OnceLock<Mutex<HashSet<DimBlockPos>>>': 1,
 'src/main.rs:LIVE_MOB_AI:std::cell::RefCell<HashMap<i32, MobAiProfile>>': 1,
 'src/main.rs:MOB_CUSTOM_NAMES:OnceLock<Mutex<HashMap<i32, String>>>': 1,
 'src/main.rs:PENDING_BLOCKS:Registry<DimBlockPos, HashMap<u16, u32>>': 1,
 'src/main.rs:PENDING_BLOCK_LOOT:Registry<DimBlockPos, PendingBlockLoot>': 1,
 'src/main.rs:PHYS:OnceLock<Mutex<HashMap<i32, W1VehiclePhys>>>': 1,
 'src/main.rs:POWERED_PLATES:OnceLock<Mutex<HashSet<(i32, i32, i32)>>>': 1,
 'src/main.rs:QUEUES:OnceLock<Mutex<HashMap<DimId, tick_queue::TickQueue>>>': 3,
 'src/main.rs:RAILS:OnceLock<Mutex<HashSet<DimBlockPos>>>': 1,
 'src/main.rs:REVEALS:OnceLock<Mutex<HashMap<DimId, Vec<Position>>>>': 1,
 'src/main.rs:SAMPLES:OnceLock<Mutex<HashMap<i32, ElytraMotionSample>>>': 1,
 'src/main.rs:SET:OnceLock<Mutex<HashSet<DimBlockPos>>>': 3,
 'src/main.rs:SHEARED:OnceLock<Mutex<std::collections::HashSet<i32>>>': 1,
 'src/main.rs:SITTING_CATS:OnceLock<Mutex<std::collections::HashSet<i32>>>': 1,
 'src/main.rs:STATE:Registry<DimId, LiveRedstoneState>': 1,
 'src/main.rs:STATES:OnceLock<Mutex<HashMap<i32, LiveMobState>>>': 1,
 'src/main.rs:W1_ARROW_BEFORE:std::cell::RefCell<HashMap<i32, ([f64; 6], bool)>>': 1,
 'src/main.rs:WATERS:OnceLock<Mutex<std::collections::HashSet<(i32, i32, i32)>>>': 1,
 'src/main.rs:WOOL:OnceLock<Mutex<HashMap<i32, u8>>>': 1,
 'src/main.rs:WORLD:OnceLock<ShardedChunks<HashMap<u32, u32>>>': 1,
 'src/menus_and_items.rs:BREWING_SLEEP:OnceLock<Mutex<HashMap<DimBlockPos, u128>>>': 1,
 'src/menus_and_items.rs:CHESTS:Registry<ContainerKey, HashMap<i16, Stack>>': 1,
 'src/menus_and_items.rs:DROP_DIMS:OnceLock<Mutex<HashMap<i32, DimId>>>': 1,
 'src/menus_and_items.rs:DROP_PENDING:OnceLock<Mutex<HashSet<i32>>>': 1,
 'src/menus_and_items.rs:FURNACES:Registry<DimBlockPos, Furnace>': 1,
 'src/menus_and_items.rs:FURNACE_XP_USED:Registry<DimBlockPos, Vec<(u64, u32, f32)>>': 1,
 'src/menus_and_items.rs:GOAT_HORN_CD:Registry<i32, Instant>': 1,
 'src/menus_and_items.rs:HOPPERS:OnceLock<Mutex<std::collections::HashSet<(i32, i32, i32)>>>': 1,
 'src/menus_and_items.rs:LEARNED:Registry<i32, std::collections::BTreeSet<String>>': 1,
 'src/menus_and_items.rs:OVERRIDES:OnceLock<Mutex<HashMap<(DimId, BlockPos), i64>>>': 1,
 'src/menus_and_items.rs:SONGS:OnceLock<HashMap<String, (i32, JukeboxSong)>>': 1,
 'src/menus_and_items.rs:STANDS:Registry<DimBlockPos, LiveBrewingStand>': 1,
 'src/menus_and_items.rs:STATES:Registry<i64, MapState>': 1,
 'src/menus_and_items.rs:T:OnceLock<Mutex<HashMap<String, Vec<String>>>>': 1,
 'src/menus_and_items.rs:VILLAGERS:Registry<i32, Villager>': 1,
 'src/mob_behaviors.rs:INCUBATIONS:OnceLock<Mutex<HashMap<(DimId, BlockPos), IncubationState>>>': 1,
 'src/mob_despawn.rs:FLAGS:OnceLock<Mutex<HashMap<i32, MobPersistenceFlags>>>': 1,
 'src/mob_despawn.rs:ITEMS:OnceLock<Mutex<HashMap<i32, u32>>>': 1,
 'src/play_protocol.rs:ACKED:OnceLock<Mutex<HashSet<i32>>>': 1,
 'src/play_protocol.rs:ANCHORS:OnceLock<Mutex<HashMap<i32, DimBlockPos>>>': 1,
 'src/play_protocol.rs:AUTHENTICATED:OnceLock<Mutex<HashSet<i32>>>': 1,
 'src/play_protocol.rs:BOOKS:OnceLock<Mutex<HashMap<[u8; 16], PlayerRecipeBook>>>': 1,
 'src/play_protocol.rs:COMBAT:OnceLock<Mutex<HashMap<i32, (Instant, Instant)>>>': 1,
 'src/play_protocol.rs:EQUIPMENT:OnceLock<Mutex<HashMap<i32, EntityEquipment>>>': 1,
 'src/play_protocol.rs:FLYING:OnceLock<Mutex<HashMap<i32, bool>>>': 1,
 'src/play_protocol.rs:GATEWAY_WARP:OnceLock<Mutex<HashMap<i32, u64>>>': 1,
 "src/play_protocol.rs:IDS:OnceLock<Mutex<HashMap<&'static str, i32>>>": 1,
 'src/play_protocol.rs:MARKS:OnceLock<Mutex<HashSet<i32>>>': 1,
 'src/play_protocol.rs:OFFSETS:OnceLock<Mutex<HashMap<i32, i64>>>': 1,
 'src/play_protocol.rs:P:OnceLock<Mutex<HashMap<i32, PetBehavior>>>': 1,
 'src/play_protocol.rs:PENDING:OnceLock<Mutex<HashSet<i32>>>': 1,
 'src/play_protocol.rs:PONGS:OnceLock<Mutex<HashMap<i32, i32>>>': 1,
 'src/play_protocol.rs:REQUESTS:OnceLock<Mutex<HashMap<DimBlockPos, (i64, bool)>>>': 1,
 'src/play_protocol.rs:SENT:OnceLock<Mutex<HashSet<(i32, i64)>>>': 1,
 'src/play_protocol.rs:SESSIONS:OnceLock<Mutex<HashMap<i32, ChatSessionMeta>>>': 1,
 'src/play_protocol.rs:SIGNS:Registry<SignKey, SignLines>': 1,
 'src/play_protocol.rs:SIGN_META:Registry<(DimId, i32, i32, i32), SignBlockMeta>': 1,
 'src/play_protocol.rs:SOCKET_WIRE:OnceLock<std::sync::RwLock<HashMap<u64, Arc<Mutex<SocketWireState>>>>>': 1,
 'src/play_protocol.rs:STATS:OnceLock<Mutex<HashMap<[u8; 16], PlayerStats>>>': 1,
 'src/play_protocol.rs:SUBS:OnceLock<Mutex<HashMap<i32, i64>>>': 1,
 'src/play_protocol.rs:UPDATES:OnceLock<Mutex<HashMap<DimBlockPos, JigsawBlockUpdate>>>': 1,
 'src/play_protocol.rs:UPDATES:OnceLock<Mutex<HashMap<DimBlockPos, StructureBlockUpdate>>>': 1,
 'src/player_mechanisms.rs:FISHING_LUCK:OnceLock<Mutex<HashMap<i32, FishingLootWeights>>>': 1,
 'src/player_mechanisms.rs:FISHING_LURE_APPLIED:OnceLock<Mutex<HashSet<i32>>>': 1,
 'src/save_v2.rs:INV_STORE:OnceLock<Mutex<HashMap<String, HashMap<i16, Stack>>>>': 1,
 'src/save_v2.rs:LIVE_CHUNK_TICKETS:Registry<DimChunkKey, u64>': 1,
 'src/save_v2.rs:PLAYERS:Registry<[u8; 16], PlayerSave>': 1,
 'src/save_v2.rs:POS_STORE:Registry<String, DimPosRot>': 1,
 'src/structure_pipeline.rs:STARTS_ROUNDS:std::sync::OnceLock<std::sync::Mutex<HashMap<u64, std::sync::Arc<RoundCache>>>>': 1,
 'src/teams_and_commands.rs:CARTS:OnceLock<Mutex<HashMap<i32, CommandMinecartRecord>>>': 1,
 'src/teams_and_commands.rs:STORE:Registry<DimBlockPos, ExecTag>': 1,
 'src/teams_and_commands.rs:STORE:Registry<String, ExecTag>': 1,
 'src/teams_and_commands.rs:V:OnceLock<Mutex<HashMap<String, String>>>': 1,
 'src/villager_raid.rs:CHARGES:OnceLock<Mutex<HashMap<i32, i32>>>': 1,
 'src/villager_raid.rs:OMENS:Registry<VillageUuid, RaidOmen>': 1,
 'src/villager_raid.rs:POIS:Registry<DimId, PoiRegistry>': 1,
 'src/villager_raid.rs:RAIDERS:Registry<i32, (DimId, BlockPos, RaidRaiderId)>': 1,
 'src/villager_raid.rs:STATES:OnceLock<Mutex<HashMap<i32, NativeUtilityState>>>': 1,
 'src/villager_raid.rs:STATES:OnceLock<Mutex<HashMap<i32, bool>>>': 1,
 'src/villager_raid.rs:TOTALS:Registry<(DimId, BlockPos), i32>': 1,
 'src/workstations_and_mounts.rs:RECIPES:OnceLock<HashMap<String, Vec<(i64, u8)>>>': 1,
 'src/world_mechanisms.rs:CACHE:OnceLock<Mutex<HashMap<DimId, BeTickPositions>>>': 1,
 'src/world_mechanisms.rs:COUNTS:OnceLock<Mutex<HashMap<DimId, Arc<BeDimCounts>>>>': 1,
 'src/world_mechanisms.rs:SPONGE_SEEN:std::cell::RefCell<HashSet<BlockPos>>': 1,
 'src/worldgen.rs:TERRAIN_SHARES:std::sync::OnceLock<Mutex<HashMap<u64, std::sync::Arc<TerrainShare>>>>': 1}
UNWIRED_MODULE_BASELINE = {
    'core/dye_and_items',
    'core/geom',
    'core/os_desktop',
    # Alpha6 foundations, exercised by crate tests but not wired into the
    # live runtime yet. Remove each entry when its consumer lands.
    'core/block_delta',  # Reversible section edits for rollback/editor modules.
    'core/wire_envelope',  # Checked framing for future wire/save consumers.
    'server-files/modules_config',  # Opt-in modules manifest for server startup.
    # Beta-scope foundations: public crate surfaces with crate-level tests,
    # deliberately not yet wired into the live 1.21.1 listener. Keeping them
    # listed here makes the deferral explicit rather than silently dead.
    'core/nbt_sidecar',  # Unknown-NBT preservation ledger for the save migration lane.
    'core/vanilla_physics',  # Bytecode-verified physics constants for the movement port.
    'proto/entity_metadata',  # Canonical typed metadata codec for the client/server metadata migration.
    'state-store/saved_data',  # Atomic SavedData store; SaveBatchWorker integration is beta follow-up.
}

# Per-file ceilings for panic-capable calls in production Rust, counted on
# cfg(test)-stripped, comment/literal-masked source. A file absent from this
# table has a ceiling of zero: the first `.unwrap()`, `.expect()`,
# `panic!`/`unreachable!`/`todo!`/`unimplemented!` in a new or previously
# clean file fails the gate. Counts are exact on the audited tree; lowering
# one is a deliberate edit here.
PANIC_PATH_BASELINE = {
    'client-app/src/audio_out.rs': 1,
    'client-app/src/integrated.rs': 1,
    'client-app/src/kinetic_sync.rs': 1,
    'client-app/src/lod.rs': 6,
    'client-app/src/main.rs': 1,
    'client-app/src/seamless_runtime.rs': 1,
    'client-app/src/spatial_audio.rs': 2,
    'client-app/src/world.rs': 5,
    'client-gpu/src/gpu_hiz.rs': 1,
    'client-gpu/src/live_session.rs': 4,
    'client-gpu/src/renderer.rs': 47,
    'client-gpu/src/subworld_render.rs': 1,
    'client-gpu/src/vanilla_bootstrap.rs': 2,
    'client-gpu/src/vanilla_jar.rs': 2,
    'client-gpu/src/window.rs': 12,
    'client-gpu/src/world_env_pass.rs': 1,
    'client-kernel/build.rs': 5,
    'client-kernel/src/light.rs': 1,
    'client-kernel/src/occlusion_table.rs': 1,
    'client-kernel/src/registry.rs': 1,
    'client-kernel/src/section.rs': 1,
    'client-port/src/asset_pipeline.rs': 4,
    'client-port/src/ber_instancing.rs': 3,
    'client-port/src/bin/crop_contact_sheet.rs': 2,
    'client-port/src/bin/cull_region_bench.rs': 2,
    'client-port/src/bin/lod_bench.rs': 1,
    'client-port/src/bin/mesh_bench.rs': 1,
    'client-port/src/cache.rs': 1,
    'client-port/src/column.rs': 1,
    'client-port/src/compact_vertex.rs': 1,
    'client-port/src/culler.rs': 1,
    'client-port/src/dir_basis.rs': 1,
    'client-port/src/ffi.rs': 1,
    'client-port/src/gpu_residency.rs': 1,
    'client-port/src/mesh.rs': 3,
    'client-port/src/model_baker.rs': 1,
    'client-port/src/png_decode.rs': 3,
    'client-port/src/section.rs': 2,
    'client-port/src/shape_templates.rs': 1,
    'client-port/src/tactile_effects.rs': 1,
    'client-port/src/vanilla_plus_3d.rs': 1,
    'client-port/src/vfx_engine.rs': 8,
    'client-ui/src/gui_bridge.rs': 5,
    'client-ui/src/layout.rs': 1,
    'client-ui/src/mod_manager.rs': 1,
    'client-ui/src/retained_cache.rs': 2,
    'client-ui/src/sign_and_chat.rs': 1,
    'core/src/autostart_service.rs': 1,
    'core/src/bounded.rs': 1,
    'core/src/bvh.rs': 9,
    'core/src/i18n.rs': 5,
    'core/src/intern.rs': 1,
    'core/src/local_transport.rs': 2,
    'core/src/math_opt_foundation.rs': 1,
    'core/src/perf_kit.rs': 1,
    'core/src/quiescent.rs': 1,
    'core/src/redstone_eda.rs': 2,
    'core/src/search_index.rs': 2,
    'core/src/server_perf_governors.rs': 1,
    'core/src/sim_graph.rs': 2,
    'core/src/sparse_set.rs': 2,
    'core/src/task_pool.rs': 1,
    'core/src/tier25_guard_batch.rs': 2,
    'core/src/timing_wheel.rs': 3,
    'core/src/union_find.rs': 2,
    'core/src/world_view.rs': 1,
    'create-adapter/src/aeronautics.rs': 3,
    'create-adapter/src/blockstates.rs': 1,
    'create-adapter/src/contraption_adapter.rs': 1,
    'create-adapter/src/factory.rs': 1,
    'create-adapter/src/offload_bridge.rs': 5,
    'create-adapter/src/rail_network.rs': 19,
    'mod-adapter-core/src/bin/mod_check.rs': 5,
    'mod-adapter-core/src/bytecode_subset_vm.rs': 4,
    'mod-adapter-core/src/custom_media.rs': 3,
    'mod-adapter-core/src/dep_resolver.rs': 1,
    'mod-adapter-core/src/ecosystem_compat.rs': 8,
    'mod-adapter-core/src/hard_mod_primitives.rs': 2,
    'mod-adapter-core/src/live_registry_bridge.rs': 1,
    'mod-adapter-core/src/loader_extensions.rs': 1,
    'mod-adapter-core/src/loot_modifier_engine.rs': 1,
    'mod-adapter-core/src/mod_sync.rs': 4,
    'mod-adapter-core/src/profile.rs': 1,
    'mod-adapter-core/src/provenance_diff.rs': 3,
    'mod-adapter-core/src/spine_bridge.rs': 2,
    'mod-adapter-core/src/structural_compat.rs': 1,
    'mod-adapter-core/src/vanilla_qol_and_holidays.rs': 1,
    'mod-offload/src/combat_motion.rs': 2,
    'mod-offload/src/party_storage.rs': 6,
    'mod-offload/src/region_scheduler.rs': 4,
    'proto/src/bin/voice_relay.rs': 1,
    'proto/src/entity_metadata.rs': 1,
    'proto/src/friend_relay.rs': 2,
    'proto/src/item_components.rs': 3,
    'proto/src/region_merkle_sync.rs': 2,
    'proto/src/table.rs': 3,
    'proto/src/wire.rs': 2,
    'server-files/src/bin/megu_init.rs': 3,
    'server-files/src/bin/megu_installer.rs': 2,
    'server-files/src/claims.rs': 1,
    'server-files/src/installer.rs': 1,
    'server-files/src/modules_config.rs': 3,
    'server-files/src/pack_loader.rs': 1,
    'src/aeronautics_and_dim_bridge.rs': 93,
    'src/anticheat.rs': 1,
    'src/anvil_io.rs': 164,
    'src/block_registry.rs': 4,
    'src/brigadier.rs': 20,
    'src/chunk_ack.rs': 1,
    'src/combat_and_health.rs': 55,
    'src/commands_core.rs': 13,
    'src/commands_execute.rs': 5,
    'src/connection_limits.rs': 2,
    'src/datapack.rs': 21,
    'src/end_city_mansion.rs': 45,
    'src/entity_collision.rs': 16,
    'src/entity_fluid.rs': 25,
    'src/entity_push.rs': 14,
    'src/entity_registry.rs': 1,
    'src/feature.rs': 47,
    'src/fortress_monument.rs': 40,
    'src/generated_content.rs': 7,
    'src/generation_context.rs': 2,
    'src/generation_leaf_queue.rs': 4,
    'src/generation_math.rs': 19,
    'src/jigsaw_structures.rs': 6,
    'src/live_mod_bridge.rs': 221,
    'src/lod_protocol.rs': 7,
    'src/main.rs': 252,
    'src/menus_and_items.rs': 49,
    'src/mineshaft.rs': 17,
    'src/mineshaft_layout.rs': 13,
    'src/mineshaft_piece.rs': 2,
    'src/mob_behaviors.rs': 1,
    'src/mod_offload_bridge.rs': 64,
    'src/natural_spawner.rs': 6,
    'src/nether_spawn_rules.rs': 1,
    'src/paletted.rs': 14,
    'src/play_protocol.rs': 170,
    'src/play_protocol_inventory.rs': 2,
    'src/play_protocol_session.rs': 2,
    'src/player_mechanisms.rs': 4,
    'src/redstone.rs': 1,
    'src/redstone_world.rs': 1,
    'src/save_v2.rs': 17,
    'src/scheduled_ticks.rs': 38,
    'src/server_reliability.rs': 1,
    'src/single_piece_structures.rs': 33,
    'src/spawner.rs': 1,
    'src/stronghold_structures.rs': 27,
    'src/structure_pipeline.rs': 79,
    'src/structure_placement.rs': 12,
    'src/structure_template.rs': 198,
    'src/structure_transform.rs': 22,
    'src/teams_and_commands.rs': 63,
    'src/template_structures.rs': 116,
    'src/tick_persistence.rs': 2,
    'src/tree.rs': 27,
    'src/villager_raid.rs': 8,
    'src/voxel_faces.rs': 9,
    'src/world_mechanisms.rs': 17,
    'src/worldgen.rs': 370,
    'state-store/src/audit_log.rs': 2,
    'state-store/src/bin/world_migrate.rs': 1,
    'state-store/src/chunk_packet_cache.rs': 2,
    'state-store/src/delta_world.rs': 3,
    'state-store/src/incremental_save.rs': 1,
    'state-store/src/snapshot.rs': 2,
    'state-store/src/world_snapshot.rs': 2,
    'tools/offload_ffi_probe/src/frame.rs': 3,
    'tools/palette_bench/build.rs': 4,
    'tools/palette_bench/src/wire.rs': 13,
    'tools/storage_bench/src/main.rs': 26,
}

HOME_NAME_CHARS = r"[A-Za-z0-9._-]+"
HOME_PATH_RE = re.compile(
    r"/home/(?P<linux>" + HOME_NAME_CHARS + r")"
    + r"|/Users/(?P<mac>" + HOME_NAME_CHARS + r")"
    + r"|(?:[A-Za-z]:)?\\Users\\(?P<win>" + HOME_NAME_CHARS + r")",
    re.IGNORECASE,
)

USES_RE = re.compile(r"\buses:\s*([^\s#]+)")
CHECKOUT_RE = re.compile(r"^actions/checkout@[^\s]+$")
FULL_SHA_ACTION_RE = re.compile(r"^[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+@[0-9a-fA-F]{40}$")

UNSAFE_RE = re.compile(r"\bunsafe\s*(?:\{)|\bunsafe\s+fn\b")
HTTP_REQUEST_RE = re.compile(
    r"\b(?:GET|POST|HEAD|PUT|DELETE|PATCH)\s+"
    r"(?:\{[A-Za-z0-9_]*\}|/[^\s\"']*)\s+HTTP/1\.[01]"
)
ALLOC_RE = re.compile(
    r"\b(?:std::collections::)?(?:HashSet|HashMap|BTreeMap|BTreeSet)::new\s*\("
)
RAW_STRING_START_RE = re.compile(r"(?:br|cr|r)(#*)\"")
LEXICAL_START_RE = re.compile(r"//|/\*|(?:br|cr|r)#*\"|[bc]\"|\"|'")
QUOTE_OR_ESCAPE_RE = re.compile(r"\\|\"")
COMMENT_DELIM_RE = re.compile(r"/\*|\*/")
RUST_LEXICAL_TOKEN_RE = re.compile(
    r"//[^\n]*"
    r"|/\*[\s\S]*?\*/"
    r'|\b(?:br|cr|r)(?P<hashes>#*)"[\s\S]*?"(?P=hashes)'
    r"|'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\}|[\s\S])|[^'\\])'"
    r'|"(?:\\[\s\S]|[^"\\])*"'
)
NESTED_COMMENT_HINT_RE = re.compile(r"/\*(?:[^*]|\*(?!/))*?/\*")
PUB_MOD_RE = re.compile(r"(?m)^\s*pub\s+mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;")
REMOVE_ZERO_RE = re.compile(r"\.remove\(\s*0\s*\)")
STATIC_START_RE = re.compile(
    r"(?m)^[ \t]*(?:pub(?:[ \t]*\([^)]*\))?[ \t]+)?static[ \t]+(?:mut[ \t]+)?"
    r"([A-Za-z_][A-Za-z0-9_]*)[ \t]*:[ \t]*"
)
REGISTRY_TYPE_RE = re.compile(r"\bRegistry\s*<")
PANIC_PATH_RE = re.compile(
    r"\.unwrap\s*\(\s*\)"
    r"|\.expect\s*\("
    r"|\bpanic!\s*\("
    r"|\bunreachable!\s*\("
    r"|\btodo!\s*\("
    r"|\bunimplemented!\s*\("
)
PANIC_PATH_INTEREST_RE = re.compile(
    r"unwrap|expect|panic!|unreachable!|todo!|unimplemented!"
)

# Fast lexical masks used by the audit proper.  They are not a Rust parser,
# but they understand the literals and comments that can otherwise masquerade
# as structural code.  Positions and line numbers are preserved exactly.
_BRACE_RE = re.compile(r"[{}]")
_RUST_LEXICAL_CACHE = {}
_CFG_TEST_CACHE = {}
_READ_TEXT_CACHE = {}
_REL_PATH_CACHE = {}
_SPLIT_LINES_CACHE = {}
_PY_COMMENT_MASK_CACHE = {}


def _text_cache_key(text):
    return text


def _blanked_span(span):
    if "\n" not in span:
        return " " * len(span)
    return "\n".join(" " * len(part) for part in span.split("\n"))


def _scan_quoted_literal(text, start):
    index = start + 1
    while index < len(text):
        match = QUOTE_OR_ESCAPE_RE.search(text, index)
        if not match:
            return len(text)
        if match.group(0) == "\\":
            index = match.end() + 1
        else:
            return match.end()
    return len(text)


def _scan_char_literal(text, start):
    prefix = 2 if text.startswith("b'", start) else 1
    index = start + prefix
    if index >= len(text):
        return None
    if text[index] == "\\":
        index += 1
        if index >= len(text):
            return None
        if text[index] == "x":
            if index + 2 >= len(text) or not re.match(r"[0-9A-Fa-f]{2}", text[index + 1:index + 3]):
                return None
            index += 3
        elif text[index] == "u" and index + 1 < len(text) and text[index + 1] == "{":
            close = text.find("}", index + 2, min(len(text), index + 12))
            if close == -1:
                return None
            index = close + 1
        else:
            index += 1
    else:
        index += 1
    if index < len(text) and text[index] == "'":
        return index + 1
    return None


def _rust_lexical_mask_scanned(text, mask_literals):
    """Position-preserving nested-comment-aware lexical mask."""
    spans = []
    position = 0
    length = len(text)
    while True:
        match = LEXICAL_START_RE.search(text, position)
        if not match:
            break
        start = match.start()
        token = match.group(0)
        literal_end = None
        if token == "//":
            end = text.find("\n", match.end())
            end = length if end == -1 else end
            spans.append((start, end))
            position = end
            continue
        if token == "/*":
            depth = 1
            end = match.end()
            while end < length and depth:
                delimiter = COMMENT_DELIM_RE.search(text, end)
                if not delimiter:
                    end = length
                    break
                if delimiter.group(0) == "/*":
                    depth += 1
                else:
                    depth -= 1
                end = delimiter.end()
            spans.append((start, end))
            position = end
            continue

        raw_match = RAW_STRING_START_RE.match(text, start)
        if raw_match:
            close = '"' + raw_match.group(1)
            close_at = text.find(close, raw_match.end())
            literal_end = length if close_at == -1 else close_at + len(close)
        elif token in {'"', 'b"', 'c"'}:
            quote = start if token == '"' else start + 1
            literal_end = _scan_quoted_literal(text, quote)
        elif token == "'":
            literal_end = _scan_char_literal(text, start)

        if literal_end is not None:
            if mask_literals:
                spans.append((start, literal_end))
            position = literal_end
        else:
            position = start + 1

    if not spans:
        return text
    pieces = []
    cursor = 0
    for start, end in spans:
        pieces.append(text[cursor:start])
        pieces.append(_blanked_span(text[start:end]))
        cursor = end
    pieces.append(text[cursor:])
    return "".join(pieces)


def rust_lexical_mask(text, mask_literals):
    """Mask Rust comments, optionally also string/char literal contents."""
    key = (text, mask_literals)
    cached = _RUST_LEXICAL_CACHE.get(key)
    if cached is not None:
        return cached
    if NESTED_COMMENT_HINT_RE.search(text):
        result = _rust_lexical_mask_scanned(text, mask_literals)
    else:
        def replace_token(match):
            token = match.group(0)
            if mask_literals or token.startswith("/"):
                return _blanked_span(token)
            return token

        result = RUST_LEXICAL_TOKEN_RE.sub(replace_token, text)
    _RUST_LEXICAL_CACHE[key] = result
    return result


def rust_code_only(text):
    """Return Rust text with comments and literal contents blanked in place."""
    return rust_lexical_mask(text, True)


def rust_comments_masked(text):
    """Return Rust text with comments blanked while retaining literal text."""
    return rust_lexical_mask(text, False)


def matching_brace(text, opening):
    """Return the index after the brace matching text[opening], or None."""
    depth = 0
    for match in _BRACE_RE.finditer(text, opening):
        if match.group(0) == "{":
            depth += 1
        else:
            depth -= 1
            if depth == 0:
                return match.end()
    return None


def blank_span_preserving_lines(text, start, end):
    return text[:start] + _blanked_span(text[start:end]) + text[end:]


def matching_paren(text, opening):
    """Return the index after the paren matching text[opening], or None."""
    depth = 0
    for pos in range(opening, len(text)):
        if text[pos] == "(":
            depth += 1
        elif text[pos] == ")":
            depth -= 1
            if depth == 0:
                return pos + 1
    return None


def cfg_test_only_expr(expr):
    """Whether a cfg expression requires `test` in every build of the item.

    `cfg(all(test, feature = "..."))` is test-only even when the feature is
    enabled. `cfg(any(test, feature = "..."))` is not: a production feature
    build includes the item, so its panic paths remain production paths.
    """
    expr = " ".join(expr.strip().split())
    if expr == "test":
        return True
    if not expr.startswith("all"):
        return False
    open_at = expr.find("(")
    if open_at < 0:
        return False
    close_at = matching_paren(expr, open_at)
    if close_at != len(expr):
        return False
    inner = expr[open_at + 1:close_at - 1]
    depth = 0
    start = 0
    args = []
    for pos, char in enumerate(inner):
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif char == "," and depth == 0:
            args.append(inner[start:pos].strip())
            start = pos + 1
    args.append(inner[start:].strip())
    return any(arg == "test" for arg in args)


def cfg_test_spans(text):
    """Position spans of items whose positive cfg requires `test`."""
    code = rust_code_only(text)
    attr_re = re.compile(r"#\s*\[\s*cfg\s*\(")
    spans = []
    search_from = 0
    while True:
        match = attr_re.search(code, search_from)
        if not match:
            return spans
        open_paren = code.find("(", match.start())
        attr_end = matching_paren(code, open_paren)
        if open_paren < 0 or attr_end is None:
            search_from = match.end()
            continue
        expr = code[open_paren + 1:attr_end - 1]
        if not cfg_test_only_expr(expr):
            search_from = attr_end
            continue
        item_start = attr_end
        # Additional helper attributes (`#[path = ...]`, `#[allow(...)]`) can
        # sit between cfg and the test module; skip them as part of the item.
        while True:
            while item_start < len(code) and code[item_start].isspace():
                item_start += 1
            if not code.startswith("#[", item_start):
                break
            bracket = code.find("[", item_start)
            if bracket < 0:
                break
            depth = 0
            end_attr = None
            for pos in range(bracket, len(code)):
                if code[pos] == "[":
                    depth += 1
                elif code[pos] == "]":
                    depth -= 1
                    if depth == 0:
                        end_attr = pos + 1
                        break
            if end_attr is None:
                break
            item_start = end_attr
        brace = code.find("{", item_start)
        semicolon = code.find(";", item_start)
        newline = code.find("\n", item_start)
        if brace == -1 or (semicolon != -1 and semicolon < brace):
            end = semicolon + 1 if semicolon != -1 else (newline + 1 if newline != -1 else len(code))
        else:
            close = matching_brace(code, brace)
            end = close if close is not None else len(code)
        spans.append((match.start(), end))
        search_from = max(end, match.end())


def strip_cfg_test_items(text):
    """Blank items directly marked #[cfg(test)] while preserving positions."""
    key = _text_cache_key(text)
    if key in _CFG_TEST_CACHE:
        return _CFG_TEST_CACHE[key]
    result = rust_code_only(text)
    for start, end in cfg_test_spans(text):
        result = blank_span_preserving_lines(result, start, end)
    _CFG_TEST_CACHE[key] = result
    return result


def loop_spans(code):
    """Best-effort body spans of Rust for/while/loop loops in stripped code."""
    spans = []
    keyword_re = re.compile(r"\b(?:for|while|loop)\b")
    for match in keyword_re.finditer(code):
        prefix = code[max(0, match.start() - 120):match.start()]
        # `impl Trait for Type` is not a loop.
        if re.search(r"\bimpl\b[^;{}]{0,100}$", prefix):
            continue
        brace = code.find("{", match.end(), min(len(code), match.end() + 500))
        if brace == -1:
            continue
        semicolon = code.find(";", match.end(), brace)
        if semicolon != -1:
            continue
        end = matching_brace(code, brace)
        if end is not None:
            spans.append((brace, end))
    return spans


def in_any_span(position, spans):
    return any(start <= position < end for start, end in spans)


def cached_lines(text):
    key = _text_cache_key(text)
    if key not in _SPLIT_LINES_CACHE:
        _SPLIT_LINES_CACHE[key] = text.splitlines()
    return _SPLIT_LINES_CACHE[key]


def python_comments_masked(text):
    """Mask Python comments while retaining strings and positions."""
    cached = _PY_COMMENT_MASK_CACHE.get(text)
    if cached is not None:
        return cached
    try:
        tokens = tokenize.generate_tokens(io.StringIO(text).readline)
        comment_rows = [token for token in tokens if token.type == tokenize.COMMENT]
    except (tokenize.TokenError, SyntaxError, IndentationError):
        _PY_COMMENT_MASK_CACHE[text] = text
        return text
    if not comment_rows:
        _PY_COMMENT_MASK_CACHE[text] = text
        return text
    offsets = [0]
    for match in re.finditer("\n", text):
        offsets.append(match.end())
    pieces = []
    cursor = 0
    for token in comment_rows:
        row, column = token.start
        start = offsets[row - 1] + column
        end_row, end_column = token.end
        end = offsets[end_row - 1] + end_column
        pieces.append(text[cursor:start])
        pieces.append(_blanked_span(text[start:end]))
        cursor = end
    pieces.append(text[cursor:])
    result = "".join(pieces)
    _PY_COMMENT_MASK_CACHE[text] = result
    return result


def line_number(text, position):
    return text.count("\n", 0, position) + 1


def file_list(root):
    """Tracked paths under root, or a filesystem walk for synthetic trees."""
    probe = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=False,
    )
    if probe.returncode == 0 and Path(probe.stdout.strip()).resolve() == root:
        listed = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z"],
            capture_output=True,
            text=True,
            check=False,
        )
        if listed.returncode == 0:
            return sorted(root / name for name in listed.stdout.split("\0") if name)
    return sorted(
        path
        for path in root.rglob("*")
        if path.is_file() and not (EXCLUDED_PARTS & set(path.relative_to(root).parts))
    )


def read_text(path):
    key = str(path)
    if key in _READ_TEXT_CACHE:
        return _READ_TEXT_CACHE[key]
    try:
        data = path.read_bytes()
    except OSError:
        _READ_TEXT_CACHE[key] = None
        return None
    if b"\0" in data[:4096]:
        _READ_TEXT_CACHE[key] = None
        return None
    text = data.decode("utf-8", "replace")
    _READ_TEXT_CACHE[key] = text
    return text


def rel_path(root, path):
    key = (str(root), str(path))
    cached = _REL_PATH_CACHE.get(key)
    if cached is None:
        cached = path.relative_to(root).as_posix()
        _REL_PATH_CACHE[key] = cached
    return cached


_GIT_ROOT_CACHE = {}
_GIT_GREP_CACHE = {}


def is_git_root(root):
    key = str(root)
    cached = _GIT_ROOT_CACHE.get(key)
    if cached is not None:
        return cached
    probe = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=False,
    )
    result = probe.returncode == 0 and Path(probe.stdout.strip()).resolve() == root
    _GIT_ROOT_CACHE[key] = result
    return result


def git_grep_files(root, pattern, pathspecs, fixed=False):
    """Tracked files matching pattern, or None outside a git-root scan."""
    if not is_git_root(root):
        return None
    key = (str(root), pattern, tuple(pathspecs), fixed)
    if key in _GIT_GREP_CACHE:
        return _GIT_GREP_CACHE[key]
    command = ["git", "-C", str(root), "grep", "-l"]
    if fixed:
        command.append("-F")
    else:
        command.append("-E")
    command.extend(["-e", pattern, "--"])
    command.extend(pathspecs)
    completed = subprocess.run(command, capture_output=True, text=True, check=False)
    if completed.returncode not in (0, 1):
        result = None
    else:
        result = set(filter(None, completed.stdout.splitlines()))
    _GIT_GREP_CACHE[key] = result
    return result


def git_static_rows(root):
    """Line numbers of static declarations in root src, or None outside git."""
    if not is_git_root(root):
        return None
    pattern = r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?static\s+(?:mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:"
    completed = subprocess.run(
        ["git", "-C", str(root), "grep", "-P", "-n", "-e", pattern, "--", "src/*.rs"],
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode not in (0, 1):
        return None
    rows = {}
    for row in completed.stdout.splitlines():
        pieces = row.split(":", 2)
        if len(pieces) != 3:
            continue
        try:
            line = int(pieces[1])
        except ValueError:
            continue
        rows.setdefault(pieces[0], set()).add(line)
    return rows


def finding(guard, path, line, message):
    return {"guard": guard, "path": path, "line": line, "message": message}


def is_test_path(rel):
    parts = rel.split("/")
    name = parts[-1]
    return "tests" in parts or "test" in name or name.endswith("_wire.py")


def portability_context_guarded(lines, index):
    start = max(0, index - 8)
    context = "\n".join(lines[start:index + 1]).lower()
    markers = (
        "cfg!(windows)",
        "cfg(windows)",
        "cfg(unix)",
        "target_os",
        "os.name",
        "sys.platform",
        "platform.system",
        "if windows",
        "nul\"",
    )
    return any(marker in context for marker in markers)


def guard_portability(root, files):
    """1. Cross-OS /dev/null portability guard."""
    findings = []
    candidates = git_grep_files(root, "/dev/null", ("*.rs", "*.py"), fixed=True)
    scan_files = files if candidates is None else [root / rel for rel in sorted(candidates)]
    for path in scan_files:
        rel = rel_path(root, path)
        if path.suffix not in {".rs", ".py"}:
            continue
        text = read_text(path)
        if text is None or "/dev/null" not in text:
            continue
        scan_text = rust_comments_masked(text) if path.suffix == ".rs" else python_comments_masked(text)
        lines = scan_text.splitlines()
        for index, line in enumerate(lines):
            if "/dev/null" not in line:
                continue
            guarded = portability_context_guarded(lines, index)
            direct = False
            if path.suffix == ".rs":
                direct = re.search(r"\bFile::open\s*\(\s*\"/dev/null\"", line) is not None
            else:
                direct = re.search(r"\b(?:open|Path)\s*\(\s*[\"']/dev/null[\"']", line) is not None
            if direct and not guarded:
                findings.append(finding(
                    "cross-os-portability",
                    rel,
                    index + 1,
                    "unguarded /dev/null open is not portable to Windows",
                ))
            elif is_test_path(rel) and not guarded:
                findings.append(finding(
                    "cross-os-portability",
                    rel,
                    index + 1,
                    "raw /dev/null in a cross-OS test without a Windows guard",
                ))
    return findings


def guard_personal_paths(root, files):
    """2. Personal home-path hygiene ratchet."""
    findings = []
    remaining = dict(PERSONAL_PATH_BASELINE_DIGESTS)
    candidates = git_grep_files(root, r"/home/|/Users/|\\Users\\", ())
    scan_files = files if candidates is None else [root / rel for rel in sorted(candidates)]
    for path in scan_files:
        rel = rel_path(root, path)
        text = read_text(path)
        if text is None or ("/home/" not in text and "/Users/" not in text and "\\Users\\" not in text):
            continue
        for match in HOME_PATH_RE.finditer(text):
            name = next(group for group in match.groups() if group is not None).lower()
            if name in SYNTHETIC_HOME_USERS:
                continue
            digest = hashlib.sha256((rel + "\0" + match.group(0)).encode("utf-8")).hexdigest()[:16]
            if remaining.get(digest, 0):
                remaining[digest] -= 1
                continue
            line = line_number(text, match.start())
            findings.append(finding(
                "personal-path-hygiene",
                rel,
                line,
                f"non-synthetic home path {match.group(0)!r}",
            ))
    return findings


def yaml_code_prefix(line):
    """Return a YAML line up to its first unquoted comment marker."""
    quote = None
    escaped = False
    for index, char in enumerate(line):
        if quote == '"':
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quote = None
            continue
        if quote == "'":
            if char == "'":
                quote = None
            continue
        if char in {'"', "'"}:
            quote = char
        elif char == "#" and (index == 0 or line[index - 1].isspace()):
            return line[:index]
    return line


def guard_workflow_supply_chain(root, files):
    """3. GitHub Actions pinning and checkout credential hygiene."""
    findings = []
    for path in files:
        rel = rel_path(root, path)
        parts = rel.split("/")
        if len(parts) < 3 or parts[0] != ".github" or parts[1] != "workflows":
            continue
        if path.suffix not in {".yml", ".yaml"}:
            continue
        text = read_text(path)
        if text is None:
            continue
        lines = text.splitlines()
        for index, line in enumerate(lines):
            match = USES_RE.search(yaml_code_prefix(line))
            if not match:
                continue
            action = match.group(1).strip().strip('"\'')
            if not FULL_SHA_ACTION_RE.match(action):
                findings.append(finding(
                    "workflow-supply-chain",
                    rel,
                    index + 1,
                    f"action {action!r} is not pinned to a full 40-hex commit SHA",
                ))
            if CHECKOUT_RE.match(action):
                indent = len(line) - len(line.lstrip())
                step_end = len(lines)
                for later in range(index + 1, len(lines)):
                    candidate = lines[later]
                    candidate_indent = len(candidate) - len(candidate.lstrip())
                    if candidate_indent <= indent and candidate.lstrip().startswith("- "):
                        step_end = later
                        break
                step_text = "\n".join(lines[index:step_end])
                if not re.search(r"(?m)^\s*persist-credentials:\s*false\s*(?:#.*)?$", step_text):
                    findings.append(finding(
                        "workflow-supply-chain",
                        rel,
                        index + 1,
                        "actions/checkout does not set persist-credentials: false",
                    ))
    return findings


def unsafe_occurrences(text):
    code = rust_code_only(text)
    return [(match.start(), line_number(text, match.start())) for match in UNSAFE_RE.finditer(code)]


def has_safety_comment(lines, line):
    start = max(0, line - 9)
    return any("// SAFETY:" in lines[index] for index in range(start, line))


def guard_unsafe(root, files):
    """4. Scoped unsafe ratchet with mandatory SAFETY comments."""
    findings = []
    candidates = git_grep_files(root, "unsafe", tuple(scope + "/*.rs" for scope in RUST_SCOPES), fixed=True)
    scan_files = files if candidates is None else [root / rel for rel in sorted(candidates)]
    for path in scan_files:
        rel = rel_path(root, path)
        if path.suffix != ".rs" or not any(rel == scope or rel.startswith(scope + "/") for scope in RUST_SCOPES):
            continue
        text = read_text(path)
        if text is None or "unsafe" not in text:
            continue
        occurrences = unsafe_occurrences(text)
        if not occurrences:
            continue
        baseline = UNSAFE_BASELINE_COUNTS.get(rel)
        lines = text.splitlines()
        if baseline is not None:
            if len(occurrences) > baseline:
                findings.append(finding(
                    "unsafe-safety-ratchet",
                    rel,
                    occurrences[baseline][1],
                    f"unsafe count {len(occurrences)} exceeds pinned baseline {baseline}",
                ))
            undocumented = sum(
                1
                for _position, occurrence_line in occurrences
                if not has_safety_comment(lines, occurrence_line)
            )
            undocumented_baseline = UNSAFE_UNDOCUMENTED_BASELINE_COUNTS.get(rel, 0)
            if undocumented > undocumented_baseline:
                extra_line = next(
                    occurrence_line
                    for _position, occurrence_line in reversed(occurrences)
                    if not has_safety_comment(lines, occurrence_line)
                )
                findings.append(finding(
                    "unsafe-safety-ratchet",
                    rel,
                    extra_line,
                    f"undocumented unsafe count {undocumented} exceeds pinned baseline {undocumented_baseline}",
                ))
            continue
        allowed = rel in UNSAFE_EXPLICIT_ALLOWLIST
        for _position, line in occurrences:
            if not allowed:
                findings.append(finding(
                    "unsafe-safety-ratchet",
                    rel,
                    line,
                    "new unsafe outside the documented allowlist/baseline",
                ))
            if not has_safety_comment(lines, line):
                findings.append(finding(
                    "unsafe-safety-ratchet",
                    rel,
                    line,
                    "unsafe block/function lacks a // SAFETY: comment within 8 lines",
                ))
    return findings


HTTP_VALIDATION_RE = re.compile(
    r"validate_(?:path|host|header)|control or non-ascii|is_ascii\(\)|"
    r"contains\('\\r'\)|contains\('\\n'\)|trim_end_matches\('\\r'\)|"
    r"split\(\"\\r\\n\"\)|b\"\\r\\n\\r\\n\"",
    re.IGNORECASE,
)


def guard_http_crlf(root, files):
    """5. Raw HTTP request/header CRLF-injection validation guard."""
    findings = []
    candidates = git_grep_files(root, "HTTP/1.", ("*.rs",), fixed=True)
    scan_files = files if candidates is None else [root / rel for rel in sorted(candidates)]
    for path in scan_files:
        rel = rel_path(root, path)
        if path.suffix != ".rs" or EXCLUDED_PARTS & set(rel.split("/")[:-1]):
            continue
        text = read_text(path)
        if text is None or "HTTP/1." not in text:
            continue
        code = rust_comments_masked(text)
        test_spans = cfg_test_spans(text)
        matches = [
            match
            for match in HTTP_REQUEST_RE.finditer(code)
            if not in_any_span(match.start(), test_spans)
        ]
        if not matches:
            continue
        if not HTTP_VALIDATION_RE.search(code):
            findings.append(finding(
                "http-crlf-injection",
                rel,
                line_number(text, matches[0].start()),
                "raw HTTP/1.x request construction lacks visible CR/LF validation",
            ))
    return findings


DECOMPRESS_CAP_RE = re.compile(
    r"(?i)(?:aggregate|total|cumulative)[A-Za-z0-9_\s]{0,40}(?:decompress|bytes)|"
    r"\bMAX_[A-Z0-9_]*(?:BYTES|SIZE|CAP|LIMIT)\b|"
    r"\b[A-Z0-9_]*(?:BYTES|SIZE|CAP|LIMIT)_CAP\b|"
    r"\btotal_decompressed\b|\bdecompress(?:ed)?_budget\b|\bbudget\b"
)


def guard_decompression_cap(root, files):
    """6. Aggregate multi-entry decompression cap guard."""
    findings = []
    candidates = git_grep_files(root, "decompress", ("*.rs",), fixed=True)
    scan_files = files if candidates is None else [root / rel for rel in sorted(candidates)]
    for path in scan_files:
        rel = rel_path(root, path)
        if path.suffix != ".rs" or EXCLUDED_PARTS & set(rel.split("/")[:-1]):
            continue
        text = read_text(path)
        if text is None or "decompress" not in text:
            continue
        code = strip_cfg_test_items(text)
        spans = loop_spans(code)
        candidate_positions = []
        for start, end in spans:
            body = code[start:end]
            if "decompress" not in body:
                continue
            if ".push" not in body and ".insert" not in body:
                continue
            position = body.find("decompress") + start
            candidate_positions.append(position)
        if candidate_positions and not DECOMPRESS_CAP_RE.search(code):
            findings.append(finding(
                "archive-decompression-cap",
                rel,
                line_number(text, candidate_positions[0]),
                "multi-entry decompression loop lacks an aggregate byte cap/budget",
            ))
    return findings


def guard_remove_zero_and_codecs(root, files):
    """7. Vec::remove(0) and duplicate codec implementation ratchets."""
    findings = []
    remove_candidates = git_grep_files(root, ".remove(0)", ("src/*.rs",), fixed=True)
    codec_candidates = git_grep_files(
        root,
        r"sha256|Sha256|SHA256|crc32|CRC32|0xedb88320|0xEDB88320|lz4|Lz4|LZ4",
        tuple(scope + "/*.rs" for scope in RUST_SCOPES),
    )
    remove_count = 0
    first_extra = None
    codec_counts = {"sha256": 0, "crc32": 0, "lz4": 0}
    codec_extra_lines = {"sha256": [], "crc32": [], "lz4": []}
    codec_patterns = {
        "sha256": re.compile(r"\bstruct\s+Sha256\b|\bfn\s+sha256\s*\("),
        "crc32": re.compile(r"\bfn\s+crc32[A-Za-z0-9_]*\s*\(|\bcrc32_table\s*\(|0xedb88320", re.IGNORECASE),
        "lz4": re.compile(r"\bfn\s+lz4[A-Za-z0-9_]*\s*\(|\bstruct\s+Lz4[A-Za-z0-9_]*\b|\benum\s+Lz4[A-Za-z0-9_]*\b"),
    }
    for path in files:
        rel = rel_path(root, path)
        if path.suffix != ".rs":
            continue
        in_root_src = rel.startswith("src/")
        in_codec_scope = any(rel == scope or rel.startswith(scope + "/") for scope in RUST_SCOPES)
        if not in_root_src and not in_codec_scope:
            continue
        remove_possible = remove_candidates is None or rel in remove_candidates
        codec_possible = codec_candidates is None or rel in codec_candidates
        if not (in_root_src and remove_possible) and not (in_codec_scope and codec_possible):
            continue
        text = read_text(path)
        if text is None:
            continue
        if in_root_src and remove_possible and ".remove(0)" in text:
            code = rust_code_only(text)
            for match in REMOVE_ZERO_RE.finditer(code):
                remove_count += 1
                if remove_count > REMOVE_ZERO_BASELINE_COUNT and first_extra is None:
                    first_extra = (rel, line_number(text, match.start()))
        codec_interest = (
            "sha256" in text or "Sha256" in text or "SHA256" in text,
            "crc32" in text or "CRC32" in text or "0xedb88320" in text or "0xEDB88320" in text,
            "lz4" in text or "Lz4" in text or "LZ4" in text,
        )
        if in_codec_scope and codec_possible and any(codec_interest):
            candidates = []
            for codec, interested in zip(("sha256", "crc32", "lz4"), codec_interest):
                if interested and codec_patterns[codec].search(text):
                    candidates.append(codec)
            if candidates:
                code = strip_cfg_test_items(text)
                for codec in candidates:
                    if rel == CANONICAL_CODEC_FILES.get(codec):
                        continue
                    if codec == "lz4" and rel == "state-store/src/lz4_region.rs" and "megu_core::lz4::" in code:
                        continue
                    pattern = codec_patterns[codec]
                    for match in pattern.finditer(code):
                        codec_counts[codec] += 1
                        if codec_counts[codec] > CODEC_BASELINE_COUNTS[codec]:
                            codec_extra_lines[codec].append((rel, line_number(text, match.start())))
    if first_extra is not None:
        findings.append(finding(
            "hotloop-remove-zero-codec-ratchet",
            first_extra[0],
            first_extra[1],
            f"Vec::remove(0) count exceeds pinned baseline {REMOVE_ZERO_BASELINE_COUNT}",
        ))
    for codec, extras in codec_extra_lines.items():
        if extras:
            rel, line = extras[0]
            findings.append(finding(
                "hotloop-remove-zero-codec-ratchet",
                rel,
                line,
                f"{codec} codec implementation count exceeds pinned baseline {CODEC_BASELINE_COUNTS[codec]}",
            ))
    return findings


def guard_hot_render_loop_allocations(root, files):
    """8. Hot render-loop collection allocation ratchet."""
    findings = []
    wanted = set(HOT_LOOP_ALLOC_BASELINE)
    for path in files:
        rel = rel_path(root, path)
        if rel not in wanted:
            continue
        text = read_text(path)
        if text is None:
            continue
        code = strip_cfg_test_items(text)
        spans = loop_spans(code)
        occurrences = [
            match
            for match in ALLOC_RE.finditer(code)
            if in_any_span(match.start(), spans)
        ]
        baseline = HOT_LOOP_ALLOC_BASELINE[rel]
        if len(occurrences) > baseline:
            match = occurrences[baseline]
            findings.append(finding(
                "render-loop-allocation-ratchet",
                rel,
                line_number(text, match.start()),
                f"collection allocation in loop exceeds pinned baseline {baseline}",
            ))
    return findings


def bounded_global_context(text, line):
    lines = cached_lines(rust_code_only(text))
    context = "\n".join(lines[max(0, line - 24):min(len(lines), line + 32)])
    return re.search(r"\b[A-Z0-9]+(?:_[A-Z0-9]+)*_MAX(?:_[A-Z0-9]+)*\b|\bMAX_[A-Z0-9_]+\b|\.retain\s*\(|\bevict[A-Za-z0-9_]*\s*\(", context) is not None


def static_hash_globals(text, static_lines=None):
    code = strip_cfg_test_items(text)
    if static_lines is None:
        for match in STATIC_START_RE.finditer(code):
            name = match.group(1)
            type_start = match.end()
            # Static type annotations in this workspace wrap over at most a
            # few lines.  Bounding the search keeps the scan linear and
            # prevents a malformed file from consuming the rest of the input.
            limit = min(len(code), type_start + 1200)
            equals = code.find("=", type_start, limit)
            if equals == -1:
                continue
            type_text = " ".join(code[type_start:equals].split())
            registry_alias = REGISTRY_TYPE_RE.search(type_text) is not None
            if "HashMap" in type_text or "HashSet" in type_text or registry_alias:
                yield name, type_text, line_number(text, match.start())
        return

    lines = cached_lines(code)
    for line in sorted(static_lines):
        if line > len(lines):
            continue
        line_text = lines[line - 1]
        match = STATIC_START_RE.match(line_text)
        if match is None:
            continue
        fragment = line_text[match.end():]
        next_index = line
        while "=" not in fragment and len(fragment) < 1200 and next_index < len(lines):
            fragment += "\n" + lines[next_index]
            next_index += 1
        equals = fragment.find("=", 0, 1200)
        if equals == -1:
            continue
        type_text = " ".join(fragment[:equals].split())
        registry_alias = REGISTRY_TYPE_RE.search(type_text) is not None
        if "HashMap" in type_text or "HashSet" in type_text or registry_alias:
            yield match.group(1), type_text, line


def guard_unbounded_global_registries(root, files):
    """9. Unbounded session/global HashMap/HashSet registry ratchet."""
    findings = []
    remaining_unbounded = dict(UNBOUNDED_GLOBAL_BASELINE_COUNTS)
    static_rows = git_static_rows(root)
    for path in files:
        rel = rel_path(root, path)
        if path.suffix != ".rs" or not rel.startswith("src/"):
            continue
        text = read_text(path)
        if text is None or ("HashMap" not in text and "HashSet" not in text and "Registry<" not in text):
            continue
        if static_rows is None:
            if not re.search(r"(?m)^\s*(?:pub(?:\s*\([^)]*\))?\s+)?static\s+", text):
                continue
            lines_for_file = None
        else:
            lines_for_file = static_rows.get(rel, set())
            if not lines_for_file:
                continue
        for name, type_text, static_line in static_hash_globals(text, lines_for_file):
            bounded = bounded_global_context(text, static_line)
            identity = f"{rel}:{name}:{' '.join(type_text.split())}"
            if bounded:
                continue
            if remaining_unbounded.get(identity, 0):
                remaining_unbounded[identity] -= 1
                continue
            findings.append(finding(
                "unbounded-global-registry",
                rel,
                static_line,
                f"static global {name} ({type_text}) lacks a nearby MAX_/eviction/retain bound",
            ))
    return findings


def production_rust_files(root, files):
    for path in files:
        rel = rel_path(root, path)
        if path.suffix != ".rs" or EXCLUDED_PARTS & set(rel.split("/")[:-1]):
            continue
        parts = rel.split("/")
        if "tests" in parts or "test" in path.stem or path.name.endswith("_tests.rs"):
            continue
        yield rel, path


def guard_production_panic_paths(root, files):
    """11. unwrap/expect/panic-macro ceiling ratchet over every production .rs.

    Network decoders, save parsers and hot loops must return `Result`/`Option`
    instead of panicking; a bare `.unwrap()` on untrusted input is a remote
    crash. The per-file ceilings freeze today's count so the class cannot grow
    silently, and any file not in PANIC_PATH_BASELINE starts at zero.
    """
    findings = []
    for rel, path in production_rust_files(root, files):
        text = read_text(path)
        if text is None or not PANIC_PATH_INTEREST_RE.search(text):
            continue
        code = strip_cfg_test_items(text)
        matches = list(PANIC_PATH_RE.finditer(code))
        limit = PANIC_PATH_BASELINE.get(rel, 0)
        if len(matches) > limit:
            match = matches[limit]
            findings.append(finding(
                "production-panic-path-ratchet",
                rel,
                line_number(text, match.start()),
                f"panic-capable call count {len(matches)} exceeds pinned baseline {limit}",
            ))
    return findings


def guard_unwired_public_modules(root, files):
    """10. Public library modules must not silently remain dead prototypes."""
    findings = []
    module_rows = []
    file_map = {rel_path(root, path): path for path in files}
    for lib_rel in WORKSPACE_LIBRS:
        lib_path = file_map.get(lib_rel)
        if lib_path is None or not lib_path.exists():
            continue
        text = read_text(lib_path)
        if text is None:
            continue
        crate = lib_rel.split("/")[0]
        lib_code = rust_code_only(text)
        for match in PUB_MOD_RE.finditer(lib_code):
            module_rows.append((crate, match.group(1), lib_rel, line_number(text, match.start())))
    module_names = sorted({module for _, module, _, _ in module_rows}, key=len, reverse=True)
    if not module_names:
        return findings
    module_name_set = set(module_names)
    token_re = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*::")
    use_module_re = re.compile(
        r"\b(?:pub\s+)?use\s+(?:crate|self|super)::([A-Za-z_][A-Za-z0-9_]*)\s*;"
    )

    def production_rel(rel):
        if not rel.endswith(".rs") or EXCLUDED_PARTS & set(rel.split("/")[:-1]):
            return False
        parts = rel.split("/")
        stem = parts[-1].rsplit(".", 1)[0]
        return "tests" not in parts and "test" not in stem and not parts[-1].endswith("_tests.rs")

    # In a git checkout, git grep performs the expensive whole-workspace scan
    # in native code.  The Python fallback below is retained for synthetic
    # fixture roots and non-git unpacked trees.
    alternation = "|".join(re.escape(name) for name in module_names)
    pattern = (
        r"\b(?:" + alternation + r")\s*::"
        + r"|\b(?:pub\s+)?use\s+(?:crate|self|super)::(?:" + alternation + r")\s*;"
    )
    grep = subprocess.run(
        ["git", "-C", str(root), "grep", "-P", "-n", "--column", "-e", pattern, "--", "*.rs"],
        capture_output=True,
        text=True,
        check=False,
    )
    raw_rows = []
    if grep.returncode in (0, 1):
        for row in grep.stdout.splitlines():
            pieces = row.split(":", 3)
            if len(pieces) != 4:
                continue
            rel, line_text_no, _column, line_text = pieces
            if not production_rel(rel):
                continue
            try:
                row_line = int(line_text_no)
            except ValueError:
                continue
            for match in token_re.finditer(line_text):
                token = match.group(1)
                if token in module_name_set:
                    raw_rows.append((rel, row_line, match.start(), token))
            for match in use_module_re.finditer(line_text):
                token = match.group(1)
                if token in module_name_set:
                    raw_rows.append((rel, row_line, match.start(1), token))
    else:
        candidate_re = re.compile(pattern)
        for rel, path in production_rust_files(root, files):
            text = read_text(path)
            if text is None or not candidate_re.search(text):
                continue
            for line_no, line_text in enumerate(text.splitlines(), 1):
                if not candidate_re.search(line_text):
                    continue
                for match in token_re.finditer(line_text):
                    token = match.group(1)
                    if token in module_name_set:
                        raw_rows.append((rel, line_no, match.start(), token))
                for match in use_module_re.finditer(line_text):
                    token = match.group(1)
                    if token in module_name_set:
                        raw_rows.append((rel, line_no, match.start(1), token))

    references = {}
    rows_by_file = {}
    for row in raw_rows:
        rows_by_file.setdefault(row[0], []).append(row)
    for rel, rows in rows_by_file.items():
        path = file_map.get(rel, root / rel)
        text = read_text(path)
        if text is None:
            continue
        # The authoritative mask removes comments and cfg(test) items while
        # preserving line/column positions.  Only files with a native-search
        # candidate pay this cost.
        code_lines = strip_cfg_test_items(text).splitlines()
        for _rel, row_line, column, token in rows:
            if row_line > len(code_lines):
                continue
            if code_lines[row_line - 1].startswith(token, column):
                references.setdefault(token, set()).add(rel)

    for crate, module, lib_rel, line in module_rows:
        identity = f"{crate}/{module}"
        own_prefix = f"{crate}/src/{module}"
        reference_files = references.get(module, set())
        referenced = any(
            not (rel == own_prefix + ".rs" or rel.startswith(own_prefix + "/"))
            for rel in reference_files
        )
        if not referenced and identity not in UNWIRED_MODULE_BASELINE:
            findings.append(finding(
                "unwired-public-module",
                lib_rel,
                line,
                f"public module {crate}/{module} has no production reference",
            ))
    return findings


GUARDS = (
    ("cross-os-portability", guard_portability),
    ("personal-path-hygiene", guard_personal_paths),
    ("workflow-supply-chain", guard_workflow_supply_chain),
    ("unsafe-safety-ratchet", guard_unsafe),
    ("http-crlf-injection", guard_http_crlf),
    ("archive-decompression-cap", guard_decompression_cap),
    ("hotloop-remove-zero-codec-ratchet", guard_remove_zero_and_codecs),
    ("render-loop-allocation-ratchet", guard_hot_render_loop_allocations),
    ("unbounded-global-registry", guard_unbounded_global_registries),
    ("unwired-public-module", guard_unwired_public_modules),
    ("production-panic-path-ratchet", guard_production_panic_paths),
)


def audit(root):
    root = Path(root).resolve()
    files = file_list(root)
    findings = []
    summary = {}
    for name, guard in GUARDS:
        guard_findings = guard(root, files)
        summary[name] = len(guard_findings)
        findings.extend(guard_findings)
    findings.sort(key=lambda row: (row["guard"], row["path"], row["line"]))
    return {"ok": not findings, "root": str(root), "summary": summary, "findings": findings}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--json", action="store_true", help="emit a machine-readable report")
    parser.add_argument("--strict", action="store_true", help="exit nonzero when any guard reports a finding")
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    options = parser.parse_args(argv)

    report = audit(options.root)
    if options.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        for row in report["findings"]:
            print(f"{row['guard']}: FAIL {row['path']}:{row['line']} — {row['message']}")
        total = len(report["findings"])
        status = "PASS" if report["ok"] else f"FAIL ({total} finding(s))"
        counts = ", ".join(f"{name}={count}" for name, count in sorted(report["summary"].items()))
        print(f"general defect audit: {status} [{counts}]")
    if options.strict and not report["ok"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
