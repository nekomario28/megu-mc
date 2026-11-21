// Production bridge between mc-spine and the local mod-adapter crates.
//
// `include!`d into `main.rs` under `#[cfg(feature = "create-offload")]`, in the same
// flat root-scope style as `anticheat.rs`/`play_protocol.rs`, so the layers module
// graph is untouched: with the feature off nothing here is compiled or linked.
//
// Surfaces wired today:
//   * `mod_offload_tick` — one step per server tick from `game_tick`, driving
//     `CreateFactory` + `UniversalModRuntime` region budgets and translating
//     belt/contraption carry into exact `ac_note_carried_motion` grants so
//     anti-cheat never rubber-bands or false-flags fall damage on riders.
//   * `mod_offload_sync_antixray_tags` — folds `#c:ores`/`#forge:ores`/`#c:chests`/
//     `#forge:chests` (and name-heuristic) mod blocks into `hidden_extra()`, the
//     same anti-xray hidden set `refresh_hidden_tags_from_datapacks` feeds.
//   * `mod_offload_config_payloads` — deterministic Configuration-phase `0x07`
//     registry data and `neoforge:frozen_registry*` custom payloads, emitted only
//     while custom mod blocks are active so vanilla clients see nothing new.
//   * `mod_offload_save_bytes` / `mod_offload_load_bytes` — Save v2 persistence
//     for placed Create blocks and live contraptions, encoded in create-adapter's
//     deterministic binary wire style (BE scalars + VarInt counts + MC strings).

use create_adapter::blockstates::{core_create_registry, CreateBlockRegistry};
use create_adapter::contraption_adapter::{
    ContraptionError, ContraptionKind, ContraptionSnapshot, LiveContraption, WorldBlockEdit,
    DEFAULT_MAX_CONTRAPTION_BLOCKS,
};
use create_adapter::factory::{CreateFactory, DimBlockPos as ModBlockPos};
use create_adapter::neoforge_wire as create_wire;
use mod_adapter_core::blockstates::DEFAULT_BASE_STATE_ID;
use mod_adapter_core::neoforge_wire as mod_wire;
use mod_adapter_core::profile as mod_profile;
use mod_adapter_core::profile::{ModOffloadProfile, UniversalModRuntime};

/// Protocol 767 Configuration clientbound Custom Payload packet id.
#[allow(dead_code)]
const CONFIG_CUSTOM_PAYLOAD_ID: u8 = 0x01;
/// Per-region work/time budget for one `mod_offload` tick step. The kernels are
/// sleeping schedulers: a region with nothing queued returns immediately, so the
/// bound only bites on genuinely busy modded factories.
const MOD_OFFLOAD_REGION_STEPS: usize = 1_024;
const MOD_OFFLOAD_REGION_US: u64 = 500;

/// Save v2 bridge codec: `"MSMO"` + version byte, then create-adapter-style
/// deterministic fields (BE scalars, VarInt counts, MC strings).
#[allow(dead_code)]
const MOD_OFFLOAD_SAVE_MAGIC: [u8; 4] = *b"MSMO";
#[allow(dead_code)]
const MOD_OFFLOAD_SAVE_VERSION: u8 = 1;

/// A player riding an assembled contraption: `anchor` is their world-space point
/// rigidly attached to the contraption frame, updated every stepped tick.
#[derive(Clone, Copy, Debug)]
struct ContraptionRider {
    contraption: u32,
    anchor: [f64; 3],
}

/// Everything the feature-gated mod offload owns at runtime. Constructed empty;
/// `has_work`/`mod_blocks_active` stay false until `MC_SPINE_MOD_OFFLOAD` (or a
/// test) opts pieces in, keeping untouched vanilla connections and worlds at
/// literally zero bridge cost.
#[allow(dead_code)]
struct ModOffloadBridge {
    /// Flagship Create adapter: built on `enable_create` so an empty bridge
    /// carries no registry.
    create: Option<CreateFactory>,
    /// Declarative universal runtime, present iff profiles loaded cleanly.
    universal: Option<UniversalModRuntime>,
    /// Profiles `load_profiles` accepted (kept for config payload parity).
    profiles: Vec<ModOffloadProfile>,
    /// Cached `state_count` of the Create registry so `universal` state IDs can
    /// reserve space deterministically even before `create` exists.
    create_state_count: Option<u32>,
    /// Create blocks this bridge placed, so Save v2 can rebuild the kinetic
    /// network: `pos -> state_id` in the *current* runtime's IDs; persisted as
    /// canonical strings so a reordered registry remaps cleanly.
    placed_create: BTreeMap<ModBlockPos, u32>,
    contraptions: BTreeMap<u32, LiveContraption>,
    next_contraption_id: u32,
    contraption_riders: BTreeMap<i32, ContraptionRider>,
    /// eid -> exact per-tick carry velocity while riding a belt/conveyor.
    belt_riders: BTreeMap<i32, [f64; 3]>,
    /// Loaded `#tag` -> member block ids (mod or vanilla), e.g. from mod jars.
    tag_members: BTreeMap<String, BTreeSet<String>>,
    /// Custom (non-vanilla) state IDs the anti-xray sync decided to hide.
    mod_state_ids: BTreeSet<u32>,
    /// Ticks actually stepped — the empty bridge must never tick (test evidence).
    ticks_stepped: u64,
}

#[allow(dead_code)]
impl ModOffloadBridge {
    fn new() -> Self {
        Self {
            create: None,
            universal: None,
            profiles: Vec::new(),
            create_state_count: None,
            placed_create: BTreeMap::new(),
            contraptions: BTreeMap::new(),
            next_contraption_id: 1,
            contraption_riders: BTreeMap::new(),
            belt_riders: BTreeMap::new(),
            tag_members: BTreeMap::new(),
            mod_state_ids: BTreeSet::new(),
            ticks_stepped: 0,
        }
    }

    /// `MC_SPINE_MOD_OFFLOAD` (comma list): `create` arms the flagship adapter;
    /// `universal`/`all` loads the built-in reference profiles; `ae2`, `mekanism`
    /// and `pipez` load single reference profiles. Absent/empty means a fully
    /// vanilla server with zero mod-offload surface.
    fn from_env() -> Self {
        let mut bridge = Self::new();
        let Ok(spec) = std::env::var("MC_SPINE_MOD_OFFLOAD") else {
            return bridge;
        };
        let mut profiles: Vec<ModOffloadProfile> = Vec::new();
        for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match token {
                "create" => bridge.enable_create(),
                "universal" | "all" => profiles.extend(mod_profile::builtin_reference_profiles()),
                "ae2" => profiles.push(mod_profile::ae2_class_profile()),
                "mekanism" => profiles.push(mod_profile::mekanism_class_profile()),
                "pipez" => profiles.push(mod_profile::pipez_class_profile()),
                other => eprintln!("[mod-offload] ignoring unknown MC_SPINE_MOD_OFFLOAD token {other:?}"),
            }
        }
        if !profiles.is_empty() {
            if let Err(err) = bridge.load_profiles(profiles) {
                eprintln!("[mod-offload] reference profiles failed to build: {err:?}");
            }
        }
        bridge
    }

    fn create_registry(&self) -> Option<&CreateBlockRegistry> {
        self.create.as_ref().map(CreateFactory::registry)
    }

    /// Build the flagship Create registry + factory if not already present.
    fn enable_create(&mut self) {
        self.create.get_or_insert_with(|| {
            CreateFactory::new(core_create_registry(DEFAULT_BASE_STATE_ID))
        });
    }

    /// State ID space after the (always reserved) Create block states, so the
    /// universal runtime's IDs are deterministic regardless of enable order.
    fn universal_base(&mut self) -> u32 {
        let count = match self.create_registry() {
            Some(reg) => reg.state_count() as u32,
            None => *self
                .create_state_count
                .get_or_insert_with(|| core_create_registry(DEFAULT_BASE_STATE_ID).state_count() as u32),
        };
        DEFAULT_BASE_STATE_ID + count
    }

    /// Load declarative universal profiles; the runtime is only installed when
    /// every profile builds, so a bad manifest can never leave a half-loaded
    /// adapter visible to config payloads.
    fn load_profiles(
        &mut self,
        profiles: Vec<ModOffloadProfile>,
    ) -> Result<(), mod_adapter_core::blockstates::RegistryError> {
        let base = self.universal_base();
        let runtime = UniversalModRuntime::from_profiles(base, &profiles)?;
        self.universal = Some(runtime);
        self.profiles = profiles;
        Ok(())
    }

    /// Anything worth ticking or syncing. The empty bridge is a zero-cost no-op.
    fn has_work(&self) -> bool {
        self.create.is_some()
            || self.universal.is_some()
            || !self.contraptions.is_empty()
            || !self.contraption_riders.is_empty()
            || !self.belt_riders.is_empty()
    }

    /// Custom mod block space is live: config-phase payloads exist only now.
    fn mod_blocks_active(&self) -> bool {
        self.create.is_some() || self.universal.is_some()
    }

    /// Place a Create block through the bridge so Save v2 can rebuild the same
    /// kinetic network after a reload.
    fn place_create_block(
        &mut self,
        pos: ModBlockPos,
        state_id: u32,
    ) -> Vec<create_adapter::factory::KineticUpdateTag> {
        self.enable_create();
        let factory = self.create.as_mut().expect("enable_create just ran");
        self.placed_create.insert(pos, state_id);
        factory.place_block(pos, state_id)
    }

    fn remove_create_block(
        &mut self,
        pos: ModBlockPos,
    ) -> Vec<create_adapter::factory::KineticUpdateTag> {
        self.placed_create.remove(&pos);
        match self.create.as_mut() {
            Some(factory) => factory.remove_block(pos),
            None => Vec::new(),
        }
    }

    /// Assemble a world region into a live contraption; the returned world edits
    /// are for the caller to commit (remove the swept blocks).
    fn assemble_contraption(
        &mut self,
        dim: DimId,
        anchor: [i32; 3],
        seed: [i32; 3],
        attached_blocks: &BTreeMap<[i32; 3], u32>,
        kind: ContraptionKind,
    ) -> Result<(u32, Vec<WorldBlockEdit>), ContraptionError> {
        self.enable_create();
        let id = self.next_contraption_id;
        self.next_contraption_id += 1;
        let (contraption, edits) = LiveContraption::assemble_from_world(
            id,
            dim.raw() as i32,
            anchor,
            seed,
            attached_blocks,
            kind,
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )?;
        self.contraptions.insert(id, contraption);
        Ok((id, edits))
    }

    /// Attach player `eid` to a live contraption at world `anchor`. Each tick the
    /// rigid pose delta is granted to anti-cheat exactly.
    fn note_contraption_rider(&mut self, eid: i32, contraption: u32, anchor: [f64; 3]) -> bool {
        if !self.contraptions.contains_key(&contraption) {
            return false;
        }
        self.contraption_riders
            .insert(eid, ContraptionRider { contraption, anchor });
        true
    }

    /// Attach player `eid` to a belt/conveyor carrying them `delta` per tick.
    fn note_belt_rider(&mut self, eid: i32, delta: [f64; 3]) {
        self.belt_riders.insert(eid, delta);
    }

    /// Player left every modded carrier (dismount, disassembly, logout).
    fn stop_carry(&mut self, eid: i32) {
        self.contraption_riders.remove(&eid);
        self.belt_riders.remove(&eid);
    }

    /// Declare a mod block tag's members, e.g. parsed from a loaded jar's
    /// `data/<ns>/tags/block/*.json`. `#` prefixes are normalized away.
    fn declare_tag(&mut self, tag: &str, members: impl IntoIterator<Item = impl Into<String>>) {
        let tag = tag.strip_prefix('#').unwrap_or(tag);
        let entry = self.tag_members.entry(ns_rl(tag)).or_default();
        for member in members {
            let name: String = member.into();
            entry.insert(ns_rl(&name));
        }
    }

    /// Every loaded custom mod block id across both adapter tiers.
    fn mod_block_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Some(reg) = self.create_registry() {
            ids.extend(reg.blocks().iter().map(|b| b.id.clone()));
        }
        if let Some(uni) = &self.universal {
            ids.extend(uni.registry().blocks().iter().map(|b| b.id.clone()));
        }
        ids
    }

    /// All expanded runtime state IDs of loaded mod block `name`.
    fn mod_state_ids_for_block(&self, name: &str) -> Vec<u32> {
        let mut out = Vec::new();
        if let Some(reg) = self.create_registry() {
            out.extend(
                reg.states()
                    .iter()
                    .filter(|e| e.block_id == name)
                    .map(|e| e.state_id),
            );
        }
        if let Some(uni) = &self.universal {
            out.extend(
                uni.registry()
                    .states()
                    .iter()
                    .filter(|e| e.block_id == name)
                    .map(|e| e.state_id),
            );
        }
        out
    }

    /// Fold loaded mod ore/chest blocks into the anti-xray hidden set: members of
    /// the conventional `#c:ores` / `#forge:ores` / `#c:chests` / `#forge:chests`
    /// (and `*:barrels`) tag roots, plus loaded blocks whose names pattern-match
    /// (`*_ore`, `*chest*`, `*barrel*`, covering e.g. `create:zinc_ore` and
    /// `create:deepslate_zinc_ore` when a profile/addon declares them). Vanilla
    /// names resolve through `breg()`; custom mod state IDs are tracked in the
    /// bridge registry — both land in `hidden_extra()` for `is_hidden`.
    fn sync_antixray_tags(&mut self) -> usize {
        let mut names: BTreeSet<String> = BTreeSet::new();
        for (tag, members) in &self.tag_members {
            if mod_hidden_root_tag(tag) {
                names.extend(members.iter().cloned());
            }
        }
        for id in self.mod_block_ids() {
            let path = rl_path(&id);
            if path.ends_with("_ore") || path.contains("chest") || path.contains("barrel") {
                names.insert(id);
            }
        }
        if names.is_empty() {
            return 0;
        }
        let mut added = 0usize;
        {
            let mut extra = hidden_extra().lock().unwrap();
            if let Some(reg) = breg() {
                for st in 0..reg.state_count() as u32 {
                    if names.contains(reg.name_of(st)) && extra.insert(st) {
                        added += 1;
                    }
                }
            }
            for name in &names {
                for st in self.mod_state_ids_for_block(name) {
                    self.mod_state_ids.insert(st);
                    if extra.insert(st) {
                        added += 1;
                    }
                }
            }
        }
        if added > 0 {
            HIDDEN_EXTRA_USED.store(true, Ordering::Relaxed);
        }
        added
    }

    /// Deterministic Configuration-phase packets: one `0x07` Registry Data frame
    /// covering every loaded mod block name (in the same create-then-universal
    /// canonical order the adapters' own wire test uses), then each active mod
    /// registry's `neoforge:frozen_registry*` frames as `0x01` custom payloads.
    /// Empty while no mod offload profile is loaded — vanilla joins are
    /// bit-identical to a build without the feature.
    fn config_payloads(&self) -> Vec<(u8, Vec<u8>)> {
        if !self.mod_blocks_active() {
            return Vec::new();
        }
        let mut block_names = Vec::new();
        if let Some(reg) = self.create_registry() {
            block_names.extend(reg.blocks().iter().map(|b| b.id.clone()));
        }
        if let Some(uni) = &self.universal {
            block_names.extend(uni.registry().blocks().iter().map(|b| b.id.clone()));
        }
        let mut out = Vec::new();
        if let Ok(body) = mod_wire::build_config_registry_data_0x07("minecraft:block", &block_names)
        {
            out.push((mod_wire::CONFIG_REGISTRY_DATA_PACKET_ID, body));
        }
        let push_frames = |out: &mut Vec<(u8, Vec<u8>)>, frames: Vec<(String, Vec<u8>)>| {
            for (channel, payload) in frames {
                let mut body = Vec::with_capacity(1 + 32 + payload.len());
                body.push(CONFIG_CUSTOM_PAYLOAD_ID);
                mod_wire::write_mc_string(&mut body, &channel);
                body.extend_from_slice(&payload);
                out.push((CONFIG_CUSTOM_PAYLOAD_ID, body));
            }
        };
        if let Some(reg) = self.create_registry() {
            push_frames(&mut out, create_wire::build_neoforge_frozen_registry_frames(reg));
        }
        if let Some(uni) = &self.universal {
            push_frames(
                &mut out,
                create_wire::build_neoforge_frozen_registry_frames(uni.registry()),
            );
        }
        out
    }

    /// Advance the whole bridge by one server tick: region budgets for both
    /// adapter tiers, contraption poses, then exact carry grants into anti-cheat.
    fn step_tick(&mut self, tick: u64) {
        self.ticks_stepped += 1;
        if let Some(factory) = &mut self.create {
            let _ = factory.step_tick(tick, MOD_OFFLOAD_REGION_STEPS, Some(MOD_OFFLOAD_REGION_US));
        }
        if let Some(universal) = &mut self.universal {
            let _ = universal.step_tick(tick, MOD_OFFLOAD_REGION_STEPS, Some(MOD_OFFLOAD_REGION_US));
        }
        for contraption in self.contraptions.values_mut() {
            let _ = contraption.step_tick();
        }
        // Exact rigid-carry deltas: `passenger_delta` is the displacement the
        // contraption just applied to the rider's attached point; belts carry a
        // fixed per-tick velocity. Both go through `ac_note_carried_motion`, so
        // the move is granted exactly and NoFall treats the rider as supported —
        // no rubber-banding, no false fall damage, real teleports still flag.
        for (eid, rider) in self.contraption_riders.iter_mut() {
            let Some(contraption) = self.contraptions.get(&rider.contraption) else {
                continue;
            };
            let Some(delta) = contraption.passenger_delta(rider.anchor) else {
                continue;
            };
            for (axis, d) in rider.anchor.iter_mut().zip(delta) {
                *axis += d;
            }
            ac_note_carried_motion(*eid, delta);
        }
        for (&eid, &delta) in &self.belt_riders {
            ac_note_carried_motion(eid, delta);
        }
    }

    /// Deterministic Save v2 binary snapshot: placed Create blocks as canonical
    /// blockstate strings (registry-reorder safe) plus live contraptions in
    /// create-adapter's name-based snapshot form.
    fn save_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(&MOD_OFFLOAD_SAVE_MAGIC);
        out.push(MOD_OFFLOAD_SAVE_VERSION);

        mod_wire::write_varint(&mut out, self.placed_create.len() as u32);
        for (pos, state_id) in &self.placed_create {
            mob_w_i32(&mut out, pos.dim);
            mob_w_i32(&mut out, pos.x);
            mob_w_i32(&mut out, pos.y);
            mob_w_i32(&mut out, pos.z);
            let canonical = self
                .create_registry()
                .and_then(|reg| reg.encode_canonical(*state_id))
                .unwrap_or("");
            mod_wire::write_mc_string(&mut out, canonical);
        }

        let snapshots: Vec<ContraptionSnapshot> = self
            .create_registry()
            .map(|reg| self.contraptions.values().map(|c| c.snapshot(reg)).collect())
            .unwrap_or_default();
        mod_wire::write_varint(&mut out, snapshots.len() as u32);
        for snap in &snapshots {
            mob_w_snapshot(&mut out, snap);
        }
        out
    }

    /// Restore a `save_bytes` snapshot: replaces placed Create blocks and live
    /// contraptions. Canonical strings decode against the current registry, so
    /// reordered runtime IDs remap exactly like Save v2 promises.
    fn load_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        let err = |m: &str| format!("mod-offload save: {m}");
        if bytes.len() < 5 || bytes[..4] != MOD_OFFLOAD_SAVE_MAGIC {
            return Err(err("bad magic"));
        }
        if bytes[4] != MOD_OFFLOAD_SAVE_VERSION {
            return Err(err("unsupported version"));
        }
        let mut cursor = 5usize;

        let placed_count = mod_wire::read_varint(bytes, &mut cursor)
            .ok_or_else(|| err("truncated placed count"))? as usize;
        if placed_count > MAX_BATCH_PLACED {
            return Err(err("placed count out of range"));
        }
        self.enable_create();
        let mut pending = Vec::with_capacity(placed_count);
        for _ in 0..placed_count {
            let dim = mob_r_i32(bytes, &mut cursor).ok_or_else(|| err("truncated pos"))?;
            let x = mob_r_i32(bytes, &mut cursor).ok_or_else(|| err("truncated pos"))?;
            let y = mob_r_i32(bytes, &mut cursor).ok_or_else(|| err("truncated pos"))?;
            let z = mob_r_i32(bytes, &mut cursor).ok_or_else(|| err("truncated pos"))?;
            let canonical = mod_wire::read_mc_string(
                bytes,
                &mut cursor,
                mod_wire::MAX_REGISTRY_DATA_NAME_LEN,
            )
            .ok_or_else(|| err("truncated canonical state"))?;
            let state_id = self
                .create_registry()
                .and_then(|reg| reg.decode_canonical(&canonical))
                .ok_or_else(|| err("unknown canonical state"))?;
            pending.push((ModBlockPos::new(dim, x, y, z), state_id));
        }

        let contraption_count = mod_wire::read_varint(bytes, &mut cursor)
            .ok_or_else(|| err("truncated contraption count"))? as usize;
        if contraption_count > DEFAULT_MAX_CONTRAPTION_BLOCKS {
            return Err(err("contraption count out of range"));
        }
        let mut restored = Vec::with_capacity(contraption_count);
        for _ in 0..contraption_count {
            let snap = mob_r_snapshot(bytes, &mut cursor).ok_or_else(|| err("truncated snapshot"))?;
            let registry = self
                .create_registry()
                .ok_or_else(|| err("create registry unavailable"))?;
            restored.push(
                LiveContraption::restore(&snap, registry)
                    .map_err(|e| err(&format!("corrupt contraption: {e:?}")))?,
            );
        }
        if cursor != bytes.len() {
            return Err(err("trailing bytes"));
        }

        // Decode fully succeeded: commit in one shot so a corrupt save can never
        // leave a half-restored kinetic network.
        self.placed_create.clear();
        for (pos, state_id) in pending {
            self.place_create_block(pos, state_id);
        }
        self.contraptions.clear();
        let mut max_id = 0u32;
        for c in restored {
            max_id = max_id.max(c.id);
            self.contraptions.insert(c.id, c);
        }
        self.next_contraption_id = self.next_contraption_id.max(max_id + 1);
        Ok(())
    }
}

/// Sanity bound for a placed-block section; a real factory is nowhere near this.
#[allow(dead_code)]
const MAX_BATCH_PLACED: usize = 1 << 20;

/// `name` -> always-namespaced resource location (`"chest"` -> `"minecraft:chest"`).
#[allow(dead_code)]
fn ns_rl(name: &str) -> String {
    if name.contains(':') {
        name.to_string()
    } else {
        format!("minecraft:{name}")
    }
}

/// Path component of a resource location (`"create:zinc_ore"` -> `"zinc_ore"`).
#[allow(dead_code)]
fn rl_path(id: &str) -> &str {
    id.split_once(':').map(|(_, p)| p).unwrap_or(id)
}

/// Conventional tags whose members anti-xray should hide (mirrors the predicate
/// `register_hidden_tags_from_dir` uses for datapack tags).
#[allow(dead_code)]
fn mod_hidden_root_tag(tag: &str) -> bool {
    matches!(
        tag,
        "c:ores" | "forge:ores" | "c:chests" | "forge:chests" | "c:barrels" | "forge:barrels"
    ) || tag.starts_with("c:ores/")
        || tag.starts_with("forge:ores/")
        || tag.ends_with(":ores")
}

// ---------- deterministic binary codec (create-adapter wire style) ----------

#[allow(dead_code)]
fn mob_w_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[allow(dead_code)]
fn mob_w_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[allow(dead_code)]
fn mob_w_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[allow(dead_code)]
fn mob_r_i32(bytes: &[u8], cursor: &mut usize) -> Option<i32> {
    let s = bytes.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    Some(i32::from_be_bytes(s.try_into().ok()?))
}

#[allow(dead_code)]
fn mob_r_f32(bytes: &[u8], cursor: &mut usize) -> Option<f32> {
    let s = bytes.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    Some(f32::from_be_bytes(s.try_into().ok()?))
}

#[allow(dead_code)]
fn mob_r_f64(bytes: &[u8], cursor: &mut usize) -> Option<f64> {
    let s = bytes.get(*cursor..*cursor + 8)?;
    *cursor += 8;
    Some(f64::from_be_bytes(s.try_into().ok()?))
}

#[allow(dead_code)]
fn mob_w_snapshot(out: &mut Vec<u8>, snap: &ContraptionSnapshot) {
    out.extend_from_slice(&snap.id.to_be_bytes());
    mob_w_i32(out, snap.dim);
    for v in snap.anchor {
        mob_w_i32(out, v);
    }
    match snap.kind {
        ContraptionKind::Bearing {
            angular_deg_per_tick,
        } => {
            out.push(0);
            mob_w_f32(out, angular_deg_per_tick);
        }
        ContraptionKind::Linear { velocity_per_tick } => {
            out.push(1);
            for v in velocity_per_tick {
                mob_w_f64(out, v);
            }
        }
    }
    mob_w_f32(out, snap.yaw_deg);
    mob_w_f32(out, snap.pitch_deg);
    for v in snap.translation {
        mob_w_f64(out, v);
    }
    mod_wire::write_varint(out, snap.cells.len() as u32);
    for (local, name) in &snap.cells {
        for v in *local {
            mob_w_i32(out, v);
        }
        mod_wire::write_mc_string(out, name);
    }
}

#[allow(dead_code)]
fn mob_r_snapshot(bytes: &[u8], cursor: &mut usize) -> Option<ContraptionSnapshot> {
    let id = u32::from_be_bytes(bytes.get(*cursor..*cursor + 4)?.try_into().ok()?);
    *cursor += 4;
    let dim = mob_r_i32(bytes, cursor)?;
    let mut anchor = [0; 3];
    for v in &mut anchor {
        *v = mob_r_i32(bytes, cursor)?;
    }
    let kind = match *bytes.get(*cursor)? {
        0 => {
            *cursor += 1;
            ContraptionKind::Bearing {
                angular_deg_per_tick: mob_r_f32(bytes, cursor)?,
            }
        }
        1 => {
            *cursor += 1;
            let mut velocity_per_tick = [0.0; 3];
            for v in &mut velocity_per_tick {
                *v = mob_r_f64(bytes, cursor)?;
            }
            if !velocity_per_tick.iter().all(|v| v.is_finite()) {
                return None;
            }
            ContraptionKind::Linear { velocity_per_tick }
        }
        _ => return None,
    };
    let yaw_deg = mob_r_f32(bytes, cursor)?;
    let pitch_deg = mob_r_f32(bytes, cursor)?;
    let mut translation = [0.0; 3];
    for v in &mut translation {
        *v = mob_r_f64(bytes, cursor)?;
    }
    let cell_count = mod_wire::read_varint(bytes, cursor)? as usize;
    if cell_count > DEFAULT_MAX_CONTRAPTION_BLOCKS {
        return None;
    }
    let mut cells = Vec::with_capacity(cell_count);
    for _ in 0..cell_count {
        let mut local = [0; 3];
        for v in &mut local {
            *v = mob_r_i32(bytes, cursor)?;
        }
        let name = mod_wire::read_mc_string(bytes, cursor, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)?;
        cells.push((local, name));
    }
    Some(ContraptionSnapshot {
        id,
        dim,
        anchor,
        kind,
        yaw_deg,
        pitch_deg,
        translation,
        cells,
    })
}

// ---------- global bridge + tick hook ----------

/// The process-wide bridge, lazily armed from `MC_SPINE_MOD_OFFLOAD`.
fn mod_offload_bridge() -> &'static Mutex<ModOffloadBridge> {
    static BRIDGE: OnceLock<Mutex<ModOffloadBridge>> = OnceLock::new();
    BRIDGE.get_or_init(|| Mutex::new(ModOffloadBridge::from_env()))
}

/// Per-tick entry wired from `game_tick` as `run("mod_offload", mod_offload_tick)`.
fn mod_offload_tick() {
    let tick = TICK_IDX.load(Ordering::Relaxed);
    let Ok(mut bridge) = mod_offload_bridge().lock() else {
        return;
    };
    if !bridge.has_work() {
        return;
    }
    bridge.step_tick(tick);
}

/// Configuration-phase packets a joining NeoForge client needs for the custom
/// block space; empty on a vanilla server.
#[allow(dead_code)]
fn mod_offload_config_payloads() -> Vec<(u8, Vec<u8>)> {
    mod_offload_bridge()
        .lock()
        .map(|b| b.config_payloads())
        .unwrap_or_default()
}

/// Forward loaded mod ore/chest blocks into the anti-xray hidden set.
/// Returns how many state IDs were newly hidden.
#[allow(dead_code)]
fn mod_offload_sync_antixray_tags() -> usize {
    mod_offload_bridge()
        .lock()
        .map(|mut b| b.sync_antixray_tags())
        .unwrap_or(0)
}

/// Save v2 bytes for the whole offload state (placed blocks + contraptions).
#[allow(dead_code)]
fn mod_offload_save_bytes() -> Vec<u8> {
    mod_offload_bridge()
        .lock()
        .map(|b| b.save_bytes())
        .unwrap_or_default()
}

/// Restore `mod_offload_save_bytes`; `false` on lock failure or a corrupt buffer
/// (the existing state is left untouched either way).
#[allow(dead_code)]
fn mod_offload_load_bytes(bytes: &[u8]) -> bool {
    mod_offload_bridge()
        .lock()
        .map(|mut b| b.load_bytes(bytes).is_ok())
        .unwrap_or(false)
}

#[cfg(all(test, feature = "create-offload"))]
mod mod_offload_bridge_tests {
    use super::*;

    /// Several tests here reprogram the process-global bridge; serialize them.
    static MOB_LOCK: Mutex<()> = Mutex::new(());

    fn fresh_bridge() {
        *mod_offload_bridge().lock().unwrap() = ModOffloadBridge::new();
    }

    /// Synthetic `Client` (same fixture style as `entity_fluid`'s tests) — a
    /// loopback socket, survival game mode, and an unregistered dimension slot so
    /// `block_at_dim`/`ac_server_supported` read cheap all-air terrain.
    fn mk_client(name: &str, eid: i32, dim: DimId, pos: (f64, f64, f64)) -> Arc<Client> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let _accepted = listener.accept().unwrap();
        Arc::new(Client {
            eid,
            uuid: [7u8; 16],
            name: name.to_string(),
            player_attack_damage_id: None,
            pos: Mutex::new((pos.0, pos.1, pos.2, 0.0, 0.0, true)),
            teleport: Mutex::new(Default::default()),
            hp: Mutex::new(20.0),
            sock: Mutex::new(stream),
            inv: Mutex::new(HashMap::new()),
            cursor: Mutex::new((0, 0, Vec::new())),
            drag: Mutex::new((-1, Vec::new())),
            open_chest: Mutex::new(None),
            chest_sync: Mutex::new(None),
            open_furnace: Mutex::new(None),
            craft: Mutex::new(HashMap::new()),
            craft9: Mutex::new(HashMap::new()),
            open_table: Mutex::new(false),
            open_merchant: Mutex::new(false),
            trade_sel: Mutex::new(-1),
            merchant_in: Mutex::new([(0, 0, Vec::new()), (0, 0, Vec::new())]),
            bow_draw: Mutex::new(None),
            dig: Mutex::new(None),
            eat_start: Mutex::new(None),
            blocking: Mutex::new(false),
            sprinting: Mutex::new(false),
            sneaking: Mutex::new(false),
            last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fall_fly: Mutex::new(false),
            invulnerable: Mutex::new(0),
            last_hurt: Mutex::new(0.0),
            shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            absorb: Mutex::new(0.0),
            pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            xp: Mutex::new(0),
            skin: Mutex::new(None),
            seen: Mutex::new(HashSet::new()),
            chunks: Mutex::new(HashSet::new()),
            chunk_ack: Mutex::new(ChunkFeedback::new(1)),
            gen_sources: Mutex::new(GenSources::default()),
            gm: Mutex::new(0),
            fall_from: Mutex::new(pos.1),
            food: Mutex::new(20),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            armor_pts: Mutex::new(0),
            sat: Mutex::new(5.0),
            walk_acc: Mutex::new(0.0),
            lastp: Mutex::new(pos),
            spawn_pt: Mutex::new(None),
            dim: Mutex::new(dim),
            spawn_dim: Mutex::new(dim),
            view: Mutex::new(2),
            in_portal: Mutex::new(false),
            portal_at: Mutex::new(None),
            held: Mutex::new(0),
            effs: Mutex::new(Vec::new()),
        })
    }

    /// Vanilla sprint-jump allowance is ~0.564 blocks/tick; every carry below
    /// exceeds it, so a missing grant would flag on the first move.
    #[test]
    fn mod_offload_belt_and_contraption_carry_prevents_ac_rubberband() {
        let _serial = MOB_LOCK.lock().unwrap();
        fresh_bridge();
        let dim = DimId::from_raw(9);
        let belt_eid = 977_001;
        let lift_eid = 977_002;
        let hack_eid = 977_003;
        let belt = mk_client("mob_belt_rider", belt_eid, dim, (0.5, 80.0, 0.5));
        let lift = mk_client("mob_lift_rider", lift_eid, dim, (102.5, 65.0, 100.5));
        let hack = mk_client("mob_speed_hacker", hack_eid, dim, (0.5, 80.0, 200.5));
        for c in [&belt, &lift, &hack] {
            ac_test_set_player_preset(&c.name, AcPreset::Standard);
        }

        // A 3-block bearing arm at (100,64,100), one rider at the arm tip, and a
        // belt rider carried at 1.2 blocks/tick — over twice the walk allowance.
        {
            let mut b = mod_offload_bridge().lock().unwrap();
            b.enable_create();
            let shaft = b
                .create_registry()
                .unwrap()
                .default_state("create:shaft")
                .unwrap();
            let mut world = BTreeMap::new();
            world.insert([100, 64, 100], shaft);
            world.insert([101, 64, 100], shaft);
            world.insert([102, 64, 100], shaft);
            let (cid, edits) = b
                .assemble_contraption(
                    dim,
                    [100, 64, 100],
                    [100, 64, 100],
                    &world,
                    ContraptionKind::Bearing {
                        angular_deg_per_tick: 30.0,
                    },
                )
                .unwrap();
            assert_eq!(edits.len(), 3);
            assert!(b.note_contraption_rider(lift_eid, cid, [102.5, 65.0, 100.5]));
            b.note_belt_rider(belt_eid, [1.2, 0.0, 0.0]);
        }

        // Baseline small legal moves: installs the per-eid anti-cheat entries the
        // carry grant then lands in.
        for c in [&belt, &lift, &hack] {
            let v = ac_check_move(c, (0.5, 80.0, 0.5), (0.6, 80.0, 0.5), false);
            assert!(matches!(v, AcMoveVerdict::Accept { .. }));
        }

        // Three sustained carried ticks: each grants the exact delta, every fed
        // move must be accepted with the on-ground claim intact.
        let mut belt_pos = (0.5, 80.0, 0.5);
        let mut lift_from = [102.5, 65.0, 100.5];
        for _ in 0..3 {
            mod_offload_tick();

            let belt_to = (belt_pos.0 + 1.2, belt_pos.1, belt_pos.2);
            let v = ac_check_move(&belt, belt_pos, belt_to, true);
            assert!(
                matches!(v, AcMoveVerdict::Accept { ground: true }),
                "carried belt move must accept with ground intact, got {v:?}"
            );
            belt_pos = belt_to;

            let lift_anchor = mod_offload_bridge().lock().unwrap().contraption_riders
                [&lift_eid]
                .anchor;
            let v = ac_check_move(
                &lift,
                (lift_from[0], lift_from[1], lift_from[2]),
                (lift_anchor[0], lift_anchor[1], lift_anchor[2]),
                true,
            );
            assert!(
                matches!(v, AcMoveVerdict::Accept { ground: true }),
                "carried contraption move must accept with ground intact, got {v:?}"
            );
            lift_from = lift_anchor;
        }
        for eid in [belt_eid, lift_eid] {
            assert_eq!(ac_flag_count(eid, AcCheckId::Movement), 0, "eid {eid}");
            assert_eq!(ac_flag_count(eid, AcCheckId::NoFall), 0, "eid {eid}");
        }

        // The same distance WITHOUT a carry grant still trips the speed check:
        // an uncarried 8-block/tick teleport drains the 40-tick burst credit and
        // gets a setback within a few packets.
        let mut flagged = false;
        let mut hx = 0.5;
        for _ in 0..8 {
            let to = hx + 8.0;
            if matches!(
                ac_check_move(&hack, (hx, 80.0, 200.5), (to, 80.0, 200.5), false),
                AcMoveVerdict::Setback
            ) {
                flagged = true;
            }
            hx = to;
        }
        assert!(flagged, "uncarried speed hack must rubber-band");
        assert!(ac_flag_count(hack_eid, AcCheckId::Movement) > 0);
    }

    #[test]
    fn mod_offload_config_wire_roundtrips_create_and_universal_profiles() {
        let _serial = MOB_LOCK.lock().unwrap();
        {
            let mut b = mod_offload_bridge().lock().unwrap();
            *b = ModOffloadBridge::new();
            b.enable_create();
            b.load_profiles(mod_profile::builtin_reference_profiles())
                .unwrap();
        }
        let payloads = mod_offload_config_payloads();

        // First frame: the merged `minecraft:block` registry data.
        let (pid, body) = &payloads[0];
        assert_eq!(*pid, mod_wire::CONFIG_REGISTRY_DATA_PACKET_ID);
        let (registry, entries) = mod_wire::decode_config_registry_data_0x07(body).unwrap();
        assert_eq!(registry, "minecraft:block");
        let expected_names: Vec<String> = {
            let b = mod_offload_bridge().lock().unwrap();
            let mut names: Vec<String> = b
                .create_registry()
                .unwrap()
                .blocks()
                .iter()
                .map(|d| d.id.clone())
                .collect();
            names.extend(b.universal.as_ref().unwrap().registry().blocks().iter().map(|d| d.id.clone()));
            names
        };
        assert_eq!(entries, expected_names);

        // Then one (start, snapshot, completed) frame triple per active registry.
        let channels = [
            create_wire::CHANNEL_FROZEN_REGISTRY_SYNC_START,
            create_wire::CHANNEL_FROZEN_REGISTRY,
            create_wire::CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED,
        ];
        let frames = &payloads[1..];
        assert_eq!(frames.len(), 6);
        fn states_of<R>(
            reg: &mod_adapter_core::blockstates::BlockRegistry<R>,
        ) -> (usize, Vec<(u32, String)>) {
            (
                reg.state_count(),
                reg.states()
                    .iter()
                    .map(|e| (e.state_id, e.canonical.clone()))
                    .collect(),
            )
        }
        let registries: Vec<(usize, Vec<(u32, String)>)> = {
            let b = mod_offload_bridge().lock().unwrap();
            vec![
                states_of(b.create_registry().unwrap()),
                states_of(b.universal.as_ref().unwrap().registry()),
            ]
        };
        for (i, (pid, body)) in frames.iter().enumerate() {
            assert_eq!(*pid, CONFIG_CUSTOM_PAYLOAD_ID);
            let mut cursor = 1usize;
            let channel =
                mod_wire::read_mc_string(body, &mut cursor, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)
                    .unwrap();
            assert_eq!(channel, channels[i % 3]);
            if i % 3 != 1 {
                continue;
            }
            // The frozen-registry snapshot round-trips every expanded state.
            let snap = &body[cursor..];
            let mut sc = 0usize;
            assert_eq!(
                mod_wire::read_mc_string(snap, &mut sc, 128).as_deref(),
                Some("minecraft:block")
            );
            let count = mod_wire::read_varint(snap, &mut sc).unwrap() as usize;
            let (expected_count, expected_states) = &registries[i / 3];
            assert_eq!(count, *expected_count);
            let mut decoded = Vec::with_capacity(count);
            for _ in 0..count {
                let state_id = mod_wire::read_varint(snap, &mut sc).unwrap();
                let canonical =
                    mod_wire::read_mc_string(snap, &mut sc, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)
                        .unwrap();
                decoded.push((state_id, canonical));
            }
            assert_eq!(sc, snap.len());
            assert_eq!(&decoded, expected_states);
        }

        // Anti-xray tag sync: tag-declared + name-heuristic mod blocks land in
        // the bridge's tracked custom-state set, vanilla names resolve through
        // `breg()`, and unknown names skip cleanly.
        {
            let mut b = mod_offload_bridge().lock().unwrap();
            // A loaded profile block matching `#c:ores` tracks its custom states.
            let custom = ModOffloadProfile::from_json(
                br#"{"mod_id":"testmod","blocks":[{"id":"testmod:quartz_ore","role":"decorative"}]}"#,
            )
            .unwrap();
            b.load_profiles(vec![custom]).unwrap();
            b.declare_tag(
                "c:ores",
                ["create:zinc_ore", "testmod:quartz_ore", "minecraft:gold_ore"],
            );
            let added = b.sync_antixray_tags();
            assert!(added > 0);
            // `testmod:quartz_ore` is a loaded mod block: its expanded custom
            // state IDs are tracked in the bridge registry. `create:zinc_ore` is
            // not in the loaded core catalog, so it skipped cleanly.
            let quartz_states = b.mod_state_ids_for_block("testmod:quartz_ore");
            assert!(!quartz_states.is_empty());
            assert!(quartz_states.iter().all(|st| b.mod_state_ids.contains(st)));
            assert!(b.mod_state_ids_for_block("create:zinc_ore").is_empty());
            let extra = hidden_extra().lock().unwrap();
            for st in &quartz_states {
                assert!(extra.contains(st));
            }
            if let Some(reg) = breg() {
                for st in 0..reg.state_count() as u32 {
                    if reg.name_of(st) == "minecraft:gold_ore" {
                        assert!(extra.contains(&st));
                    }
                }
            }
            assert!(HIDDEN_EXTRA_USED.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn mod_offload_empty_bridge_emits_zero_vanilla_packets() {
        let _serial = MOB_LOCK.lock().unwrap();
        fresh_bridge();
        assert!(mod_offload_config_payloads().is_empty());
        assert!(!mod_offload_bridge().lock().unwrap().has_work());
        mod_offload_tick();
        let b = mod_offload_bridge().lock().unwrap();
        assert_eq!(b.ticks_stepped, 0, "empty bridge must be a no-op, not ticked");
    }

    #[test]
    fn mod_offload_save_load_roundtrips_contraption_state() {
        let _serial = MOB_LOCK.lock().unwrap();
        let mut src = ModOffloadBridge::new();
        src.enable_create();
        let wheel_x = src
            .create_registry()
            .unwrap()
            .with_property(
                src.create_registry()
                    .unwrap()
                    .default_state("create:water_wheel")
                    .unwrap(),
                "axis",
                "x",
            )
            .unwrap();
        let shaft_x = src
            .create_registry()
            .unwrap()
            .with_property(
                src.create_registry()
                    .unwrap()
                    .default_state("create:shaft")
                    .unwrap(),
                "axis",
                "x",
            )
            .unwrap();
        let p_wheel = ModBlockPos::new(0, 0, 64, 0);
        let p_shaft = ModBlockPos::new(0, 1, 64, 0);
        src.place_create_block(p_wheel, wheel_x);
        src.place_create_block(p_shaft, shaft_x);
        let sol = src
            .create
            .as_ref()
            .unwrap()
            .kinetic_solution(p_shaft)
            .unwrap();
        assert!(
            sol.speed.abs() > 0.0,
            "water wheel should drive the shaft (kinetic network running)"
        );

        let shaft = src
            .create_registry()
            .unwrap()
            .default_state("create:shaft")
            .unwrap();
        let mut world = BTreeMap::new();
        world.insert([10, 64, 10], shaft);
        world.insert([11, 64, 10], shaft);
        world.insert([12, 64, 10], shaft);
        let (cid, _) = src
            .assemble_contraption(
                DimId::Overworld,
                [10, 64, 10],
                [10, 64, 10],
                &world,
                ContraptionKind::Bearing {
                    angular_deg_per_tick: 15.0,
                },
            )
            .unwrap();
        src.step_tick(1);
        src.step_tick(2);

        let bytes = src.save_bytes();
        assert_eq!(src.save_bytes(), bytes, "save must be byte-deterministic");

        let mut dst = ModOffloadBridge::new();
        dst.load_bytes(&bytes).unwrap();
        // The kinetic network rebuilt: wheel + shaft re-placed and driving.
        let sol = dst
            .create
            .as_ref()
            .unwrap()
            .kinetic_solution(p_shaft)
            .unwrap();
        assert!(sol.speed.abs() > 0.0);
        // The contraption came back with pose, kind and blocks intact.
        let restored = dst.contraptions.get(&cid).unwrap();
        assert_eq!(restored.block_count(), 3);
        assert!((restored.yaw_deg - 30.0).abs() < 1e-6);
        let snap_src = src
            .contraptions
            .get(&cid)
            .unwrap()
            .snapshot(src.create_registry().unwrap());
        let snap_dst = restored.snapshot(dst.create_registry().unwrap());
        assert_eq!(snap_src, snap_dst);
        // And the loaded bridge serializes back to identical bytes.
        assert_eq!(dst.save_bytes(), bytes);
    }
}
