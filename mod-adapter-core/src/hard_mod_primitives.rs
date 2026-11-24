// SPDX-License-Identifier: LGPL-3.0-or-later

//! Deterministic, dependency-free primitives for hard mod archetypes.
//!
//! These registries own logical claims, contents, and transfer accounting.
//! The world integration must commit the corresponding block/entity removal,
//! placement, grave platform, or inventory transfer with the returned event.
//! The mod classifier describes a possible native integration boundary. It
//! does not perform renderer initialization or execute third-party mod code.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

// ---- Carry On ------------------------------------------------------------

/// Complete payload retained while a player carries a block or live entity.
#[derive(Clone, Debug, PartialEq)]
pub enum CarriedPayload {
    Block {
        pos_origin: [i32; 3],
        block_id: String,
        state_id: u32,
        nbt_bytes: Vec<u8>,
        inventory_slots: Vec<(u16, String, u32)>,
    },
    Entity {
        entity_eid: i32,
        entity_type: String,
        custom_name: Option<String>,
        health: f32,
        nbt_bytes: Vec<u8>,
        inventory_items: Vec<(String, u32)>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarryRejectReason {
    HandsNotEmpty,
    NotSneaking,
    OutOfReach,
    AlreadyCarrying,
    TargetAlreadyClaimed,
    ForbiddenImmovableBlock,
    ForbiddenTag,
    ForbiddenMultiBlockHalf,
    EntityTooLargeOrHostile,
    WrongPayloadKind,
    NoCarriedPayload,
    PlacementBlocked,
}

/// Detect immutable locations and opt-out tags before a world-cell claim.
pub fn is_carry_forbidden_block(
    block_id: &str,
    tags: &[&str],
    is_multiblock_half: bool,
) -> Option<CarryRejectReason> {
    const IMMOVABLE: &[&str] = &[
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
    ];
    if IMMOVABLE.contains(&block_id) {
        return Some(CarryRejectReason::ForbiddenImmovableBlock);
    }
    if tags.iter().any(|tag| {
        matches!(
            tag.trim_start_matches('#'),
            "c:relocation_not_supported" | "forge:immovable" | "carryon:block_blacklist"
        )
    }) {
        return Some(CarryRejectReason::ForbiddenTag);
    }
    if is_multiblock_half {
        return Some(CarryRejectReason::ForbiddenMultiBlockHalf);
    }
    None
}

/// Server-authoritative movement modifiers for the carried contents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CarryPhysicsModifier {
    pub mass_score: f32,
    pub speed_multiplier: f32,
    pub allow_sprint: bool,
    pub two_handed_pose: bool,
    pub exhaustion_multiplier: f32,
}

pub fn compute_carry_physics(payload: &CarriedPayload) -> CarryPhysicsModifier {
    let (slots, items, base_mass) = match payload {
        CarriedPayload::Block {
            inventory_slots, ..
        } => (
            inventory_slots.len(),
            inventory_slots
                .iter()
                .map(|(_, _, n)| u64::from(*n))
                .sum::<u64>(),
            1.0,
        ),
        CarriedPayload::Entity {
            inventory_items, ..
        } => (
            inventory_items.len(),
            inventory_items
                .iter()
                .map(|(_, n)| u64::from(*n))
                .sum::<u64>(),
            2.0,
        ),
    };
    // Saturating floating-point arithmetic keeps adversarial inventories
    // bounded even when a payload has an unusually large item count.
    let mass_score = (base_mass + slots as f32 * 0.2 + (items as f32).sqrt() * 0.08).min(100.0);
    CarryPhysicsModifier {
        mass_score,
        speed_multiplier: (0.85 - mass_score * 0.035).clamp(0.35, 0.85),
        allow_sprint: false,
        two_handed_pose: true,
        exhaustion_multiplier: 1.0 + mass_score * 0.075,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum CarryTarget {
    Block([i32; 3]),
    Entity(i32),
}

#[derive(Default)]
struct CarryState {
    carriers: HashMap<u64, CarriedPayload>,
    claims: HashMap<CarryTarget, u64>,
}

/// Locked, compare-and-claim ownership across simultaneous player requests.
#[derive(Default)]
pub struct CarryRegistry {
    state: Mutex<CarryState>,
}

impl CarryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim one block cell with its NBT and complete inventory snapshot.
    /// The caller must remove the source block in the same world transaction.
    #[expect(
        clippy::too_many_arguments,
        reason = "preserve the requested pickup validation API"
    )]
    pub fn try_pickup_block(
        &self,
        player_id: u64,
        payload: CarriedPayload,
        hands_empty: bool,
        sneaking: bool,
        in_reach: bool,
        tags: &[&str],
        is_multiblock_half: bool,
    ) -> Result<CarriedPayload, CarryRejectReason> {
        let CarriedPayload::Block {
            pos_origin,
            ref block_id,
            ..
        } = payload
        else {
            return Err(CarryRejectReason::WrongPayloadKind);
        };
        check_carry_interaction(hands_empty, sneaking, in_reach)?;
        if let Some(reason) = is_carry_forbidden_block(block_id, tags, is_multiblock_half) {
            return Err(reason);
        }
        self.claim(player_id, CarryTarget::Block(pos_origin), payload)
    }

    /// Claim one entity ID, including pet state and inventory NBT.
    /// The caller must despawn the source entity in the same transaction.
    pub fn try_pickup_entity(
        &self,
        player_id: u64,
        payload: CarriedPayload,
        hands_empty: bool,
        sneaking: bool,
        in_reach: bool,
        too_large_or_hostile: bool,
    ) -> Result<CarriedPayload, CarryRejectReason> {
        let CarriedPayload::Entity { entity_eid, .. } = payload else {
            return Err(CarryRejectReason::WrongPayloadKind);
        };
        check_carry_interaction(hands_empty, sneaking, in_reach)?;
        if too_large_or_hostile {
            return Err(CarryRejectReason::EntityTooLargeOrHostile);
        }
        self.claim(player_id, CarryTarget::Entity(entity_eid), payload)
    }

    fn claim(
        &self,
        player_id: u64,
        target: CarryTarget,
        payload: CarriedPayload,
    ) -> Result<CarriedPayload, CarryRejectReason> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.carriers.contains_key(&player_id) {
            return Err(CarryRejectReason::AlreadyCarrying);
        }
        if state.claims.contains_key(&target) {
            return Err(CarryRejectReason::TargetAlreadyClaimed);
        }
        state.claims.insert(target, player_id);
        state.carriers.insert(player_id, payload.clone());
        Ok(payload)
    }

    /// Consume a held payload only after the destination is known replaceable.
    /// On failure, both the claim and held payload remain intact.
    pub fn try_place_carried(
        &self,
        player_id: u64,
        _target_pos: [i32; 3],
        target_is_replaceable_air: bool,
    ) -> Result<CarriedPayload, CarryRejectReason> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !target_is_replaceable_air {
            return Err(CarryRejectReason::PlacementBlocked);
        }
        let payload = state
            .carriers
            .remove(&player_id)
            .ok_or(CarryRejectReason::NoCarriedPayload)?;
        state.claims.remove(&payload_target(&payload));
        Ok(payload)
    }

    /// Drain the payload exactly once for a death/disconnect world drop.
    pub fn emergency_drop_on_death_or_disconnect(
        &self,
        player_id: u64,
        feet_pos: [i32; 3],
    ) -> Option<([i32; 3], CarriedPayload)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let payload = state.carriers.remove(&player_id)?;
        state.claims.remove(&payload_target(&payload));
        Some((feet_pos, payload))
    }

    pub fn is_carrying(&self, player_id: u64) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.carriers.contains_key(&player_id)
    }
}

/// Convenience engine exposing the atomic carry registry.
#[derive(Default)]
pub struct CarryOnEngine {
    pub registry: CarryRegistry,
}

impl CarryOnEngine {
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::ops::Deref for CarryOnEngine {
    type Target = CarryRegistry;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

fn payload_target(payload: &CarriedPayload) -> CarryTarget {
    match payload {
        CarriedPayload::Block { pos_origin, .. } => CarryTarget::Block(*pos_origin),
        CarriedPayload::Entity { entity_eid, .. } => CarryTarget::Entity(*entity_eid),
    }
}

fn check_carry_interaction(
    hands_empty: bool,
    sneaking: bool,
    in_reach: bool,
) -> Result<(), CarryRejectReason> {
    if !hands_empty {
        Err(CarryRejectReason::HandsNotEmpty)
    } else if !sneaking {
        Err(CarryRejectReason::NotSneaking)
    } else if !in_reach {
        Err(CarryRejectReason::OutOfReach)
    } else {
        Ok(())
    }
}

// ---- Backpack and graves -------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackpackRejectReason {
    SelfNesting,
    NestedContainerDisallowed,
    NestingDepthExceeded,
}

fn is_backpack(item_id: &str) -> bool {
    item_id.contains("backpack") || item_id.ends_with(":shulker_box")
}

/// Guard recursive container trees. `inserted_nesting_depth` is the depth of
/// the inserted item before it becomes a child of the destination container.
pub fn validate_backpack_insertion(
    container_item_id: &str,
    inserted_item_id: &str,
    inserted_nesting_depth: u8,
    max_depth: u8,
) -> Result<(), BackpackRejectReason> {
    if container_item_id == inserted_item_id {
        return Err(BackpackRejectReason::SelfNesting);
    }
    let nested_container = is_backpack(inserted_item_id);
    if nested_container && max_depth == 0 {
        return Err(BackpackRejectReason::NestedContainerDisallowed);
    }
    if (nested_container || inserted_nesting_depth > 0) && inserted_nesting_depth >= max_depth {
        return Err(BackpackRejectReason::NestingDepthExceeded);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraveRecord {
    pub grave_id: u64,
    pub owner_uuid: u128,
    pub dimension: String,
    pub position: [i32; 3],
    pub needs_void_platform: bool,
    pub items: Vec<(String, u32)>,
    pub xp_points: u32,
    pub reclaimed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraveError {
    MissingGrave,
    Unauthorized,
    AlreadyReclaimed,
    InvalidWorldBounds,
    IdExhausted,
}

#[derive(Default)]
pub struct GraveStore {
    next_id: u64,
    records: BTreeMap<u64, GraveRecord>,
}

impl GraveStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a persistent logical grave. The caller must create the
    /// supporting void platform before committing the protected contents.
    #[expect(
        clippy::too_many_arguments,
        reason = "preserve the requested death context API"
    )]
    pub fn create_grave(
        &mut self,
        owner_uuid: u128,
        dim: impl Into<String>,
        death_pos: [i32; 3],
        min_world_y: i32,
        max_world_y: i32,
        items: Vec<(String, u32)>,
        xp_points: u32,
    ) -> Result<GraveRecord, GraveError> {
        if min_world_y >= max_world_y {
            return Err(GraveError::InvalidWorldBounds);
        }
        let grave_id = self.next_id.checked_add(1).ok_or(GraveError::IdExhausted)?;
        let needs_void_platform = death_pos[1] < min_world_y;
        let position = [
            death_pos[0],
            death_pos[1].clamp(min_world_y.saturating_add(1), max_world_y),
            death_pos[2],
        ];
        let record = GraveRecord {
            grave_id,
            owner_uuid,
            dimension: dim.into(),
            position,
            needs_void_platform,
            items,
            xp_points,
            reclaimed: false,
        };
        self.records.insert(grave_id, record.clone());
        self.next_id = grave_id;
        Ok(record)
    }

    pub fn get_grave(&self, grave_id: u64) -> Option<&GraveRecord> {
        self.records.get(&grave_id)
    }

    /// Once claimed, the grave retains a spent tombstone for replay defense.
    pub fn reclaim_grave(
        &mut self,
        grave_id: u64,
        requester_uuid: u128,
        allow_grave_robbery: bool,
    ) -> Result<(Vec<(String, u32)>, u32), GraveError> {
        let record = self
            .records
            .get_mut(&grave_id)
            .ok_or(GraveError::MissingGrave)?;
        if record.reclaimed {
            return Err(GraveError::AlreadyReclaimed);
        }
        if record.owner_uuid != requester_uuid && !allow_grave_robbery {
            return Err(GraveError::Unauthorized);
        }
        record.reclaimed = true;
        Ok((
            std::mem::take(&mut record.items),
            std::mem::take(&mut record.xp_points),
        ))
    }
}

// ---- Multiblock clusters and waystones ----------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiBlockCluster {
    pub cluster_id: u64,
    pub controller_pos: [i32; 3],
    pub kind: String,
    pub member_positions: Vec<[i32; 3]>,
    pub shared_inventory_capacity: usize,
    pub buffer_items: Vec<(String, u32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClusterError {
    PositionAlreadyOccupied,
    MissingCluster,
    BufferExceedsCapacity,
    IdExhausted,
}

#[derive(Default)]
pub struct MultiBlockClusterRegistry {
    next_id: u64,
    members: HashMap<[i32; 3], u64>,
    clusters: HashMap<u64, MultiBlockCluster>,
}

impl MultiBlockClusterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Form a cluster, including its controller, if every cell is unclaimed.
    pub fn form_cluster(
        &mut self,
        controller_pos: [i32; 3],
        kind: impl Into<String>,
        member_positions: Vec<[i32; 3]>,
        shared_inventory_capacity: usize,
    ) -> Result<u64, ClusterError> {
        let mut unique = BTreeSet::from([controller_pos]);
        unique.extend(member_positions);
        if unique
            .iter()
            .any(|position| self.members.contains_key(position))
        {
            return Err(ClusterError::PositionAlreadyOccupied);
        }
        let cluster_id = self
            .next_id
            .checked_add(1)
            .ok_or(ClusterError::IdExhausted)?;
        let cluster = MultiBlockCluster {
            cluster_id,
            controller_pos,
            kind: kind.into(),
            member_positions: unique.iter().copied().collect(),
            shared_inventory_capacity,
            buffer_items: Vec::new(),
        };
        for member in &unique {
            self.members.insert(*member, cluster_id);
        }
        self.clusters.insert(cluster_id, cluster);
        self.next_id = cluster_id;
        Ok(cluster_id)
    }

    /// Resolve any component to the shared controller in constant time.
    pub fn controller_at(&self, pos: [i32; 3]) -> Option<[i32; 3]> {
        let cluster_id = *self.members.get(&pos)?;
        Some(self.clusters.get(&cluster_id)?.controller_pos)
    }

    /// Replace the shared buffer; capacity is measured in occupied slots.
    pub fn set_buffer_items(
        &mut self,
        member_pos: [i32; 3],
        items: Vec<(String, u32)>,
    ) -> Result<(), ClusterError> {
        let cluster_id = *self
            .members
            .get(&member_pos)
            .ok_or(ClusterError::MissingCluster)?;
        let cluster = self
            .clusters
            .get_mut(&cluster_id)
            .ok_or(ClusterError::MissingCluster)?;
        if items.len() > cluster.shared_inventory_capacity {
            return Err(ClusterError::BufferExceedsCapacity);
        }
        cluster.buffer_items = items;
        Ok(())
    }

    /// Deform all members as one exclusive operation; the buffer leaves once.
    pub fn on_block_broken(&mut self, pos: [i32; 3]) -> Option<Vec<(String, u32)>> {
        let cluster_id = *self.members.get(&pos)?;
        let cluster = self.clusters.remove(&cluster_id)?;
        for member in &cluster.member_positions {
            self.members.remove(member);
        }
        Some(cluster.buffer_items)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waystone {
    pub waystone_id: u64,
    pub name: String,
    pub dimension: String,
    pub position: [i32; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaystoneTravelCost {
    pub xp_points: u32,
    pub cooldown_ticks: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaystoneError {
    MissingWaystone,
    Undiscovered,
    IdExhausted,
}

#[derive(Default)]
pub struct WaystoneNetwork {
    next_id: u64,
    stones: BTreeMap<u64, Waystone>,
    discoveries: HashMap<u128, HashSet<u64>>,
}

impl WaystoneNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        name: impl Into<String>,
        dimension: impl Into<String>,
        position: [i32; 3],
    ) -> Result<u64, WaystoneError> {
        let waystone_id = self
            .next_id
            .checked_add(1)
            .ok_or(WaystoneError::IdExhausted)?;
        self.stones.insert(
            waystone_id,
            Waystone {
                waystone_id,
                name: name.into(),
                dimension: dimension.into(),
                position,
            },
        );
        self.next_id = waystone_id;
        Ok(waystone_id)
    }

    pub fn discover(&mut self, player_uuid: u128, waystone_id: u64) -> Result<(), WaystoneError> {
        if !self.stones.contains_key(&waystone_id) {
            return Err(WaystoneError::MissingWaystone);
        }
        self.discoveries
            .entry(player_uuid)
            .or_default()
            .insert(waystone_id);
        Ok(())
    }

    /// Deterministic Manhattan-distance tariff; cross-dimension trips have
    /// a higher minimum cost and cooldown. No implicit target discovery.
    pub fn travel_cost(
        &self,
        player_uuid: u128,
        from_id: u64,
        to_id: u64,
    ) -> Result<WaystoneTravelCost, WaystoneError> {
        let from = self
            .stones
            .get(&from_id)
            .ok_or(WaystoneError::MissingWaystone)?;
        let to = self
            .stones
            .get(&to_id)
            .ok_or(WaystoneError::MissingWaystone)?;
        let known = self
            .discoveries
            .get(&player_uuid)
            .ok_or(WaystoneError::Undiscovered)?;
        if !known.contains(&from_id) || !known.contains(&to_id) {
            return Err(WaystoneError::Undiscovered);
        }
        let distance: u64 = from
            .position
            .iter()
            .zip(to.position)
            .map(|(&a, b)| (i64::from(a) - i64::from(b)).unsigned_abs())
            .sum();
        let distant_xp = distance.saturating_sub(128).div_ceil(256) as u32;
        let cross_dimension = from.dimension != to.dimension;
        Ok(WaystoneTravelCost {
            xp_points: distant_xp.saturating_add(if cross_dimension { 15 } else { 0 }),
            cooldown_ticks: if cross_dimension { 1200 } else { 100 },
        })
    }
}

// ---- Collapsed logistics -------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogisticsResourceKind {
    Item,
    Fluid,
    EnergyFe,
}

/// A single resource and the resolved item tags supplied by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogisticsResource {
    Item { item_id: String, tags: Vec<String> },
    Fluid { fluid_id: String },
    EnergyFe,
}

impl LogisticsResource {
    pub fn kind(&self) -> LogisticsResourceKind {
        match self {
            Self::Item { .. } => LogisticsResourceKind::Item,
            Self::Fluid { .. } => LogisticsResourceKind::Fluid,
            Self::EnergyFe => LogisticsResourceKind::EnergyFe,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutingFilter {
    ItemPrefix(String),
    ItemTag(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingMode {
    NearestFirst,
    RoundRobin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractorEndpoint {
    pub endpoint_id: u64,
    pub position: [i32; 3],
    pub resource: LogisticsResource,
    pub available: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InserterEndpoint {
    pub endpoint_id: u64,
    pub position: [i32; 3],
    pub accepted_kind: LogisticsResourceKind,
    pub remaining_capacity: u64,
    pub filter: Option<RoutingFilter>,
}

impl InserterEndpoint {
    fn accepts(&self, resource: &LogisticsResource) -> bool {
        if self.accepted_kind != resource.kind() {
            return false;
        }
        match (&self.filter, resource) {
            (None, _) => true,
            (Some(RoutingFilter::ItemPrefix(prefix)), LogisticsResource::Item { item_id, .. }) => {
                item_id.starts_with(prefix)
            }
            (Some(RoutingFilter::ItemTag(tag)), LogisticsResource::Item { tags, .. }) => tags
                .iter()
                .any(|item_tag| item_tag.trim_start_matches('#') == tag.trim_start_matches('#')),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogisticsReceipt {
    pub extracted: u64,
    pub inserted: Vec<(u64, u64)>,
    pub returned_to_source: u64,
}

impl LogisticsReceipt {
    pub fn total_inserted(&self) -> u64 {
        self.inserted.iter().map(|(_, amount)| amount).sum()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogisticsError {
    DuplicateEndpoint,
    MissingExtractor,
    QuantityOverflow,
}

/// A directed, graph-collapsed transfer planner with no intermediate pipe
/// ticks. The caller commits returned transfer events to physical storage.
#[derive(Default)]
pub struct CollapsedLogisticsNetwork {
    extractors: BTreeMap<u64, ExtractorEndpoint>,
    inserters: BTreeMap<u64, InserterEndpoint>,
    round_robin_cursor: usize,
}

impl CollapsedLogisticsNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_extractor(&mut self, extractor: ExtractorEndpoint) -> Result<(), LogisticsError> {
        if self.extractors.contains_key(&extractor.endpoint_id)
            || self.inserters.contains_key(&extractor.endpoint_id)
        {
            return Err(LogisticsError::DuplicateEndpoint);
        }
        self.extractors.insert(extractor.endpoint_id, extractor);
        Ok(())
    }

    pub fn add_inserter(&mut self, inserter: InserterEndpoint) -> Result<(), LogisticsError> {
        if self.extractors.contains_key(&inserter.endpoint_id)
            || self.inserters.contains_key(&inserter.endpoint_id)
        {
            return Err(LogisticsError::DuplicateEndpoint);
        }
        self.inserters.insert(inserter.endpoint_id, inserter);
        Ok(())
    }

    /// Replenish the source with a checked amount (external extraction).
    pub fn refill_extractor(
        &mut self,
        endpoint_id: u64,
        amount: u64,
    ) -> Result<(), LogisticsError> {
        let extractor = self
            .extractors
            .get_mut(&endpoint_id)
            .ok_or(LogisticsError::MissingExtractor)?;
        extractor.available = extractor
            .available
            .checked_add(amount)
            .ok_or(LogisticsError::QuantityOverflow)?;
        Ok(())
    }

    pub fn extractor_available(&self, endpoint_id: u64) -> Option<u64> {
        Some(self.extractors.get(&endpoint_id)?.available)
    }

    pub fn inserter_remaining_capacity(&self, endpoint_id: u64) -> Option<u64> {
        Some(self.inserters.get(&endpoint_id)?.remaining_capacity)
    }

    /// Every unit of the source budget is either accepted by an endpoint or
    /// retained in its source. Stable endpoint IDs resolve distance ties.
    pub fn route(
        &mut self,
        extractor_id: u64,
        mode: RoutingMode,
    ) -> Result<LogisticsReceipt, LogisticsError> {
        let source = self
            .extractors
            .get(&extractor_id)
            .ok_or(LogisticsError::MissingExtractor)?;
        let extracted = source.available;
        let mut eligible: Vec<(u64, u64)> = self
            .inserters
            .values()
            .filter(|sink| sink.remaining_capacity > 0 && sink.accepts(&source.resource))
            .map(|sink| {
                let distance = source
                    .position
                    .iter()
                    .zip(sink.position)
                    .map(|(&a, b)| (i64::from(a) - i64::from(b)).unsigned_abs())
                    .sum();
                (sink.endpoint_id, distance)
            })
            .collect();
        match mode {
            RoutingMode::NearestFirst => eligible.sort_by_key(|&(id, distance)| (distance, id)),
            RoutingMode::RoundRobin if !eligible.is_empty() => {
                eligible.sort_by_key(|&(id, _)| id);
                let rotation = self.round_robin_cursor % eligible.len();
                eligible.rotate_left(rotation);
                self.round_robin_cursor = self.round_robin_cursor.wrapping_add(1);
            }
            RoutingMode::RoundRobin => {}
        }

        let mut remaining = extracted;
        let mut inserted = BTreeMap::<u64, u64>::new();
        // A partially filled receiver must not strand items while a later
        // receiver still has capacity. Each pass fills at least one receiver
        // or drains the source, with no per-item pipe ticks.
        while remaining > 0 && !eligible.is_empty() {
            let mut accepted = 0;
            for (index, (sink_id, _)) in eligible.iter().enumerate() {
                if remaining == 0 {
                    break;
                }
                let sink = self
                    .inserters
                    .get_mut(sink_id)
                    .expect("eligible endpoint must exist");
                let budget = match mode {
                    RoutingMode::NearestFirst => remaining,
                    RoutingMode::RoundRobin => remaining.div_ceil((eligible.len() - index) as u64),
                };
                let sent = budget.min(sink.remaining_capacity);
                sink.remaining_capacity -= sent;
                remaining -= sent;
                accepted += sent;
                if sent > 0 {
                    *inserted.entry(*sink_id).or_default() += sent;
                }
            }
            if accepted == 0 {
                break;
            }
            eligible.retain(|(id, _)| self.inserters[id].remaining_capacity > 0);
        }
        self.extractors
            .get_mut(&extractor_id)
            .expect("source must exist")
            .available = remaining;
        Ok(LogisticsReceipt {
            extracted,
            inserted: inserted.into_iter().collect(),
            returned_to_source: remaining,
        })
    }
}

// ---- Performance / render mod integration classification -----------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineCompatDisposition {
    NativeAccelerated { native_subsystem: &'static str },
    NativeGameplayPrimitive { engine: &'static str },
    CoexistResourceBridge { bridge: &'static str },
    StandardModAdapter,
}

/// Classify an *integration strategy* for a mod ID. This table makes no
/// assertion about a particular mod version, loader, GPU, or runtime proof.
pub fn classify_perf_or_render_mod_compat(mod_id: &str) -> EngineCompatDisposition {
    use EngineCompatDisposition::*;
    match mod_id.to_ascii_lowercase().as_str() {
        "carryon" => NativeGameplayPrimitive {
            engine: "CarryOnEngine",
        },
        "sophisticatedbackpacks" => NativeGameplayPrimitive {
            engine: "BackpackGuard",
        },
        "corail_tombstone" | "gravestone" | "yigd" => NativeGameplayPrimitive {
            engine: "GraveStore",
        },
        "immersiveengineering" | "mekanism" => NativeGameplayPrimitive {
            engine: "MultiBlockClusterRegistry",
        },
        "waystones" => NativeGameplayPrimitive {
            engine: "WaystoneNetwork",
        },
        "pipez" | "ae2" | "appliedenergistics2" | "refinedstorage" => NativeGameplayPrimitive {
            engine: "CollapsedLogisticsNetwork",
        },
        "vulkanmod" => NativeAccelerated {
            native_subsystem: "wgpu_render_backend",
        },
        "acedium" | "accedium" | "optifine" | "optifabric" | "canvas" | "canvas-renderer" => {
            NativeAccelerated {
                native_subsystem: "wgpu_render_backend",
            }
        }
        "nvidium" => NativeAccelerated {
            native_subsystem: "mesh_shader_renderer",
        },
        "sodium" | "embeddium" => NativeAccelerated {
            native_subsystem: "chunk_renderer",
        },
        "lithium" => NativeAccelerated {
            native_subsystem: "entity_block_scheduler",
        },
        "c2me" => NativeAccelerated {
            native_subsystem: "chunk_task_scheduler",
        },
        "ferritecore" => NativeAccelerated {
            native_subsystem: "compact_registry_storage",
        },
        "modernfix" => NativeAccelerated {
            native_subsystem: "resource_and_allocation_cache",
        },
        "immediatelyfast" => NativeAccelerated {
            native_subsystem: "batched_immediate_renderer",
        },
        "entityculling" | "moreculling" => NativeAccelerated {
            native_subsystem: "occlusion_culling",
        },
        "starlight" | "scalablelux" => NativeAccelerated {
            native_subsystem: "light_engine",
        },
        "krypton" => NativeAccelerated {
            native_subsystem: "network_transport",
        },
        "iris" | "oculus" => CoexistResourceBridge {
            bridge: "shader_pipeline_bridge",
        },
        "distanthorizons" | "voxy" => CoexistResourceBridge {
            bridge: "lod_stream_bridge",
        },
        "continuity" => CoexistResourceBridge {
            bridge: "connected_textures_bridge",
        },
        "lambdynamiclights" => CoexistResourceBridge {
            bridge: "dynamic_lights_bridge",
        },
        "sound_physics_remastered" => CoexistResourceBridge {
            bridge: "audio_occlusion_bridge",
        },
        _ => StandardModAdapter,
    }
}
