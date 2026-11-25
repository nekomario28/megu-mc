// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `mod_adapter_core::spine_bridge` — the one universal host spine every
//! mod lane plugs into.
//!
//! Fabric bridges, NeoForge bridges, datapack loaders, mod-offload factory
//! networks and Rust/WASM guests all see the *same* canonical primitives —
//! there are no mod-specific coordinate types, revision schemes or codecs
//! here, only [`megu_core`]'s:
//!
//! * [`ModDimRegistry`] maps any `"namespace:path"` dimension resource
//!   location to a canonical [`DimIndex`]: the three vanilla names are
//!   pinned to `0..=2`, and every modded dimension — Twilight Forest,
//!   the Aether, anything — takes the next dense `DimIndex(3..)` in
//!   registration order. Registration order is the determinism contract:
//!   replicas that replay the same registration sequence (e.g. a sorted
//!   datapack scan) assign identical indices with no consensus round.
//! * [`ModSpineHost`] owns one [`ModDimRegistry`], one shared canonical
//!   [`SharedSectionRevTable`], and the generic [`ModSubscription`] set.
//!   Any mod may subscribe a [`ModWatch::Section`] or [`ModWatch::Chunk`]
//!   key under its `mod_id`; [`ModSpineHost::notify_block_changed`] bumps
//!   the canonical table (including its border-neighbour propagation) and
//!   enqueues one [`ModWakeEvent`] per affected subscription — O(1), zero
//!   polling.
//! * [`ModEventSink`] is the generic mutation-ingress trait the lanes call:
//!   vanilla ticks, loader event buses and the WASM `megu_bump_block`
//!   hostcall all funnel through the same `notify_block_changed` surface.
//! * [`encode_mod_state_lz4`] / [`decode_mod_state_lz4`] persist arbitrary
//!   per-mod state blobs inside a canonical `megu_core::lz4` +
//!   `crc32_ieee` integrity envelope — fail-closed on truncation, wrong
//!   magic, oversized declared payloads, LZ4 faults and CRC mismatch.
//!
//! The whole module is pure `std`; it never touches the network, the
//! clock or the filesystem, so any lane can embed it anywhere `std` runs.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use megu_core::bloom::{BlockedBloomFilter, VoxelBitset4096};
use megu_core::bvh::{Aabb3, BvhNodeId, DynamicBvh};
use megu_core::coords::{BlockPos, ChunkKey, CoordMap, DimIndex, SectionKey};
use megu_core::hash::{crc32_ieee, crc32_ieee_update};
use megu_core::id::{InvalidResourceLocation, ResourceLocation};
use megu_core::intern::{GenArena, GenIndex, StringInterner};
use megu_core::local_transport::{local_pair, LocalEndpoint};
use megu_core::lz4::{compress_block_into, decompress_block_exact, Lz4Error};
use megu_core::registry::{NamespacedId, RegistryError, RegistryId, UniversalRegistry};
use megu_core::rev::{Revision, UiLayer, UiLayerBus};
use megu_core::ring_spsc::{spsc_u64, BoundedRing, SpscU64Consumer, SpscU64Producer};
use megu_core::section_rev::SharedSectionRevTable;
use megu_core::sparse_set::SparseSet;
use megu_core::timing_wheel::{TimerHandle, TimingWheel};
use megu_core::ui_contract::{audit_manifest, ModUiManifest, UiContractError, UiExtensionSlot};
use megu_core::union_find::DisjointSet;

/// First dense dimension index a modded dimension may take (`0..=2` are
/// the pinned vanilla dimensions).
pub const FIRST_MODDED_DIM: u32 = 3;

/// The three vanilla dimension names pinned to their canonical indices.
const VANILLA_DIMS: [(&str, DimIndex); 3] = [
    ("minecraft:overworld", DimIndex::OVERWORLD),
    ("minecraft:the_nether", DimIndex::NETHER),
    ("minecraft:the_end", DimIndex::END),
];

/// Upper bound on [`ModSpineHost::pending_wakes`]; an unreachable backlog
/// means a lane stopped draining, so the oldest wakes drop rather than
/// growing memory without bound.
const MAX_PENDING_WAKES: usize = 65_536;

// ---------------------------------------------------------------------------
// ModDimRegistry — generic dimension name -> canonical DimIndex
// ---------------------------------------------------------------------------

/// Bidirectional map between dimension resource locations and canonical
/// [`DimIndex`] values.
///
/// Vanilla names resolve to the pinned `0..=2`; every other valid
/// `namespace:path` (or namespace-less path, which
/// [`ResourceLocation::parse`] normalises into the `minecraft` namespace)
/// receives the next dense index from [`FIRST_MODDED_DIM`] upward in the
/// order it is first resolved. Two spellings of the same dimension
/// (`"the_nether"`, `"minecraft:the_nether"`) always collapse to one
/// entry.
#[derive(Clone, Debug)]
pub struct ModDimRegistry {
    to_index: HashMap<String, DimIndex>,
    /// Dense reverse map: `to_name[i]` is the canonical name of
    /// `DimIndex(i)`. Registration order keeps indices gap-free.
    to_name: Vec<String>,
}

impl Default for ModDimRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModDimRegistry {
    /// A registry with the three vanilla dimensions pre-registered.
    #[must_use]
    pub fn new() -> Self {
        let mut r = Self {
            to_index: HashMap::with_capacity(VANILLA_DIMS.len() + 8),
            to_name: Vec::with_capacity(VANILLA_DIMS.len() + 8),
        };
        for &(name, index) in &VANILLA_DIMS {
            r.to_index.insert(name.to_string(), index);
            r.to_name.push(name.to_string());
        }
        debug_assert_eq!(r.to_name.len() as u32, FIRST_MODDED_DIM);
        r
    }

    /// Resolve `name` to its canonical [`DimIndex`], assigning the next
    /// dense index when the dimension is new.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidResourceLocation`] when `name` is not a valid
    /// resource location; nothing is registered in that case.
    pub fn resolve_or_register(
        &mut self,
        name: &str,
    ) -> Result<DimIndex, InvalidResourceLocation> {
        let loc = ResourceLocation::parse(name)?;
        if let Some(&index) = self.to_index.get(loc.as_str()) {
            return Ok(index);
        }
        let index = DimIndex(self.to_name.len() as u32);
        self.to_index.insert(loc.as_str().to_string(), index);
        self.to_name.push(loc.as_str().to_string());
        Ok(index)
    }

    /// The canonical index for a previously registered dimension, or
    /// `None` when the name is malformed or was never resolved. Use
    /// [`resolve_or_register`](Self::resolve_or_register) when a malformed
    /// name must be reported rather than collapsed into `None`.
    #[inline]
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<DimIndex> {
        let loc = ResourceLocation::parse(name).ok()?;
        self.to_index.get(loc.as_str()).copied()
    }

    /// The canonical `"namespace:path"` name behind `index`, or `None`
    /// when no dimension has taken it.
    #[inline]
    #[must_use]
    pub fn name_of(&self, index: DimIndex) -> Option<&str> {
        self.to_name.get(index.0 as usize).map(String::as_str)
    }

    /// `true` when `name` is a valid, already-registered dimension.
    #[inline]
    #[must_use]
    pub fn is_registered(&self, name: &str) -> bool {
        self.index_of(name).is_some()
    }

    /// Total registered dimensions, vanilla included.
    #[inline]
    #[must_use]
    pub fn registered_count(&self) -> usize {
        self.to_name.len()
    }
}

// ---------------------------------------------------------------------------
// Universal content registries
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModContentMeta {
    owner: String,
}

impl ModContentMeta {
    pub fn new(owner: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
        }
    }

    #[inline]
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModContentRegistrationError {
    InvalidId(InvalidResourceLocation),
    Registry(RegistryError),
}

impl fmt::Display for ModContentRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(err) => write!(f, "invalid registry id: {err}"),
            Self::Registry(err) => write!(f, "registry insertion failed: {err}"),
        }
    }
}

impl std::error::Error for ModContentRegistrationError {}

impl From<InvalidResourceLocation> for ModContentRegistrationError {
    fn from(value: InvalidResourceLocation) -> Self {
        Self::InvalidId(value)
    }
}

impl From<RegistryError> for ModContentRegistrationError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModContentRegistries {
    blocks: UniversalRegistry<ModContentMeta>,
    items: UniversalRegistry<ModContentMeta>,
    channels: UniversalRegistry<ModContentMeta>,
    ui_screens: UniversalRegistry<ModContentMeta>,
}

impl ModContentRegistries {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    #[must_use]
    pub fn blocks(&self) -> &UniversalRegistry<ModContentMeta> {
        &self.blocks
    }

    #[inline]
    pub fn blocks_mut(&mut self) -> &mut UniversalRegistry<ModContentMeta> {
        &mut self.blocks
    }

    #[inline]
    #[must_use]
    pub fn items(&self) -> &UniversalRegistry<ModContentMeta> {
        &self.items
    }

    #[inline]
    pub fn items_mut(&mut self) -> &mut UniversalRegistry<ModContentMeta> {
        &mut self.items
    }

    #[inline]
    #[must_use]
    pub fn channels(&self) -> &UniversalRegistry<ModContentMeta> {
        &self.channels
    }

    #[inline]
    pub fn channels_mut(&mut self) -> &mut UniversalRegistry<ModContentMeta> {
        &mut self.channels
    }

    #[inline]
    #[must_use]
    pub fn ui_screens(&self) -> &UniversalRegistry<ModContentMeta> {
        &self.ui_screens
    }

    #[inline]
    pub fn ui_screens_mut(&mut self) -> &mut UniversalRegistry<ModContentMeta> {
        &mut self.ui_screens
    }

    pub fn register_block(
        &mut self,
        owner: &str,
        id: &str,
    ) -> Result<RegistryId, ModContentRegistrationError> {
        let parsed = NamespacedId::parse(id)?;
        Ok(self
            .blocks
            .register(parsed, ModContentMeta::new(owner))?)
    }

    pub fn register_item(
        &mut self,
        owner: &str,
        id: &str,
    ) -> Result<RegistryId, ModContentRegistrationError> {
        let parsed = NamespacedId::parse(id)?;
        Ok(self
            .items
            .register(parsed, ModContentMeta::new(owner))?)
    }

    pub fn register_channel(
        &mut self,
        owner: &str,
        id: &str,
    ) -> Result<RegistryId, ModContentRegistrationError> {
        let parsed = NamespacedId::parse(id)?;
        Ok(self
            .channels
            .register(parsed, ModContentMeta::new(owner))?)
    }

    pub fn register_ui_screen(
        &mut self,
        owner: &str,
        id: &str,
    ) -> Result<RegistryId, ModContentRegistrationError> {
        let parsed = NamespacedId::parse(id)?;
        Ok(self
            .ui_screens
            .register(parsed, ModContentMeta::new(owner))?)
    }

    pub fn freeze_all(&mut self) {
        self.blocks.freeze();
        self.items.freeze();
        self.channels.freeze();
        self.ui_screens.freeze();
    }

    #[must_use]
    pub fn digest_crc32(&self) -> u32 {
        let mut crc = 0u32;
        for digest in [
            self.blocks.digest_crc32(),
            self.items.digest_crc32(),
            self.channels.digest_crc32(),
            self.ui_screens.digest_crc32(),
        ] {
            crc = crc32_ieee_update(crc, &digest.to_le_bytes());
        }
        crc
    }
}

// ---------------------------------------------------------------------------
// Mod UI contract wiring
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RegisteredModUi {
    manifest: ModUiManifest,
    registry_id: RegistryId,
    layer: UiLayer,
    revision: Revision,
}

impl RegisteredModUi {
    #[inline]
    #[must_use]
    pub fn manifest(&self) -> &ModUiManifest {
        &self.manifest
    }

    #[inline]
    #[must_use]
    pub fn registry_id(&self) -> RegistryId {
        self.registry_id
    }

    #[inline]
    #[must_use]
    pub fn layer(&self) -> UiLayer {
        self.layer
    }

    #[inline]
    #[must_use]
    pub fn revision(&self) -> Revision {
        self.revision
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModUiRegistrationError {
    Contract(Vec<UiContractError>),
    Content(ModContentRegistrationError),
}

impl fmt::Display for ModUiRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(violations) => {
                write!(f, "mod UI manifest violates the contract")?;
                if let Some(first) = violations.first() {
                    write!(f, ": {first}")?;
                }
                Ok(())
            }
            Self::Content(err) => write!(f, "mod UI registry insertion failed: {err}"),
        }
    }
}

impl std::error::Error for ModUiRegistrationError {}

fn ui_layer_for_slot(slot: UiExtensionSlot) -> UiLayer {
    match slot {
        UiExtensionSlot::HudTopLeft
        | UiExtensionSlot::HudTopRight
        | UiExtensionSlot::HudBottomLeft
        | UiExtensionSlot::HudBottomRight => UiLayer::ModHud,
        UiExtensionSlot::ContainerLeftPanel
        | UiExtensionSlot::ContainerRightPanel
        | UiExtensionSlot::PauseMenuAction
        | UiExtensionSlot::OptionsCategory
        | UiExtensionSlot::CustomScreen => UiLayer::ModOverlay,
    }
}

/// Commands an embedded in-process bridge can send back to the host over
/// the zero-copy local transport. They are deliberately small control
/// messages; bulk world state continues to flow through canonical keys and
/// revisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModBridgeCommand {
    /// Drain all currently queued wake events at the next host opportunity.
    DrainWakes,
    /// Re-read subscriptions after the bridge changed its watch set.
    RefreshSubscriptions,
}

/// Counters exposed by the host end of the embedded loopback transport.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModLocalTransportStats {
    /// Wake events handed to the in-process bridge.
    pub sent: u64,
    /// Bridge commands received by the host.
    pub received: u64,
    /// Conservative TCP/copy avoidance estimate reported by the transport.
    pub bytes_saved_estimate: u64,
}

// ---------------------------------------------------------------------------
// Mod connectivity, attachments, and spatial indexing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModNetworkNodeKind {
    Pipe,
    Cable,
    Belt,
    Contraption,
    Generic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModNetworkNode {
    pub mod_id: String,
    pub kind: ModNetworkNodeKind,
}

#[derive(Clone, Debug, Default)]
pub struct ModNetworkComponents {
    sets: DisjointSet,
    nodes: Vec<ModNetworkNode>,
}

impl ModNetworkComponents {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, mod_id: impl Into<String>, kind: ModNetworkNodeKind) -> u32 {
        let id = self.sets.push();
        self.nodes.push(ModNetworkNode {
            mod_id: mod_id.into(),
            kind,
        });
        id
    }

    pub fn connect_nodes(&mut self, a: u32, b: u32) -> bool {
        self.sets.union(a, b)
    }

    pub fn connected(&mut self, a: u32, b: u32) -> bool {
        self.sets.connected(a, b)
    }

    pub fn component_size(&mut self, node: u32) -> u32 {
        self.sets.component_size(node)
    }

    #[inline]
    #[must_use]
    pub fn component_count(&self) -> usize {
        self.sets.component_count()
    }

    #[inline]
    #[must_use]
    pub fn node(&self, node: u32) -> Option<&ModNetworkNode> {
        self.nodes.get(node as usize)
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModAttachmentTarget {
    Entity { network_id: u32 },
    Block { dim: DimIndex, pos: BlockPos },
}

#[derive(Default)]
pub struct ModComponentStore {
    interner: StringInterner,
    targets: GenArena<ModAttachmentTarget>,
    rows: SparseSet<SparseSet<Vec<u8>>>,
}

impl fmt::Debug for ModComponentStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModComponentStore")
            .field("live_targets", &self.targets.len())
            .field("rows", &self.rows.len())
            .finish()
    }
}

impl ModComponentStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn track_target(&mut self, target: ModAttachmentTarget) -> GenIndex {
        let handle = self.targets.insert(target);
        self.rows.insert(handle.index, SparseSet::new());
        handle
    }

    #[must_use]
    pub fn target(&self, handle: GenIndex) -> Option<&ModAttachmentTarget> {
        self.targets.get(handle)
    }

    pub fn untrack_target(&mut self, handle: GenIndex) -> bool {
        if self.targets.remove(handle).is_none() {
            return false;
        }
        self.rows.remove(handle.index);
        true
    }

    pub fn attach_bytes(&mut self, target: GenIndex, key: &str, bytes: &[u8]) -> bool {
        if !self.targets.contains(target) {
            return false;
        }
        let symbol = self.interner.intern(key);
        if self.rows.get(target.index).is_none() {
            self.rows.insert(target.index, SparseSet::new());
        }
        if let Some(row) = self.rows.get_mut(target.index) {
            row.insert(symbol.0, bytes.to_vec());
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn component(&self, target: GenIndex, key: &str) -> Option<&[u8]> {
        let symbol = self.interner.get(key)?;
        self.rows
            .get(target.index)?
            .get(symbol.0)
            .map(Vec::as_slice)
    }

    pub fn remove_component(&mut self, target: GenIndex, key: &str) -> Option<Vec<u8>> {
        let symbol = self.interner.get(key)?;
        if !self.targets.contains(target) {
            return None;
        }
        self.rows.get_mut(target.index)?.remove(symbol.0)
    }

    #[inline]
    #[must_use]
    pub fn interned_key(&self, key: &str) -> Option<megu_core::intern::Symbol> {
        self.interner.get(key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModSpatialObjectKind {
    Contraption,
    Zone,
    Machine,
    Volume,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModSpatialObject {
    pub mod_id: String,
    pub kind: ModSpatialObjectKind,
}

#[derive(Default)]
pub struct ModSpatialIndex {
    bvh: DynamicBvh<ModSpatialObject>,
}

impl fmt::Debug for ModSpatialIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModSpatialIndex")
            .field("len", &self.bvh.len())
            .finish()
    }
}

impl ModSpatialIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        aabb: Aabb3,
        mod_id: impl Into<String>,
        kind: ModSpatialObjectKind,
    ) -> BvhNodeId {
        self.bvh.insert(
            aabb,
            ModSpatialObject {
                mod_id: mod_id.into(),
                kind,
            },
        )
    }

    pub fn remove(&mut self, id: BvhNodeId) -> Option<ModSpatialObject> {
        self.bvh.remove(id)
    }

    pub fn update(&mut self, id: BvhNodeId, aabb: Aabb3) -> bool {
        self.bvh.update(id, aabb)
    }

    #[must_use]
    pub fn get(&self, id: BvhNodeId) -> Option<&ModSpatialObject> {
        self.bvh.get(id)
    }

    pub fn query_aabb<'a>(
        &'a self,
        query: Aabb3,
        out: &mut Vec<(BvhNodeId, &'a ModSpatialObject)>,
    ) {
        self.bvh.query_aabb(query, out);
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.bvh.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bvh.is_empty()
    }
}

struct ScheduledModCallback(Box<dyn FnOnce(&mut ModSpineHost)>);

// ---------------------------------------------------------------------------
// Generic subscriptions & wakes
// ---------------------------------------------------------------------------

/// The canonical key one mod subscription watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModWatch {
    /// A single `16³` section — invalidated by edits inside it *and* by
    /// border edits in neighbouring sections.
    Section(SectionKey),
    /// A whole `16×height` chunk column — the aggregate invalidator.
    Chunk(ChunkKey),
    /// A registered block identity from [`ModContentRegistries`].
    Block(RegistryId),
}

/// One generic mod subscription: `mod_id` wants a wake whenever `watch`'s
/// canonical revision changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModSubscription {
    /// Unique handle returned by `subscribe_*`; passed to `unsubscribe`.
    pub id: u64,
    /// Free-form mod identity (`"fabric:create"`, `"neoforge:mekanism"`,
    /// `"wasm:counter"`, `"offload:factory-net-3"` — the spine treats it
    /// as opaque text).
    pub mod_id: String,
    /// Watched canonical key.
    pub watch: ModWatch,
}

/// One queued dirty-key wake for one subscribed mod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModWakeEvent {
    /// The subscription's mod identity.
    pub mod_id: String,
    /// The watched key whose revision moved (the *subscription's* key —
    /// for a border-leak bump this is the watched neighbour section, not
    /// the edited block's own section).
    pub watch: ModWatch,
    /// The canonical revision the watched key now holds.
    pub revision: u64,
}

/// The generic world-mutation ingress every mod lane calls into.
///
/// Implementors funnel every block edit — vanilla tick, Fabric/NeoForge
/// event bus, datapack function, mod-offload machine write, or the WASM
/// `megu_bump_block` hostcall — through one canonical bump-and-wake.
pub trait ModEventSink {
    /// Record one block edit at `pos` in dimension `dim`; returns the
    /// edited section's new `(section_rev, chunk_rev)` pair.
    fn notify_block_changed(&mut self, dim: DimIndex, pos: BlockPos) -> (u64, u64);
}

// ---------------------------------------------------------------------------
// ModSpineHost — the shared host object
// ---------------------------------------------------------------------------

/// The universal mod-side host: one dimension registry, one shared
/// canonical revision table, generic subscriptions and a bounded wake
/// queue, all lane-agnostic.
///
/// The revision table lives behind an `Rc<RefCell<_>>` so other hosts in
/// the same tick thread — the WASM `SpineRevHost` adapter, a scripted
/// datapack lane, a test mock — can hold the *same* canonical table via
/// [`shared_rev_table`](Self::shared_rev_table) instead of drifting onto a
/// private copy. Single-threaded sharing matches how the lanes actually
/// execute: mods are driven from the server tick, never concurrently.
pub struct ModSpineHost {
    dims: ModDimRegistry,
    revs: Rc<RefCell<SharedSectionRevTable>>,
    next_sub: u64,
    subs: HashMap<u64, ModSubscription>,
    /// `SectionKey -> subscription ids` — the O(1) wake index.
    section_subs: CoordMap<SectionKey, Vec<u64>>,
    /// `ChunkKey -> subscription ids`.
    chunk_subs: CoordMap<ChunkKey, Vec<u64>>,
    block_subs: HashMap<RegistryId, Vec<u64>>,
    block_watch_filter: BlockedBloomFilter,
    dirty_voxels: CoordMap<SectionKey, VoxelBitset4096>,
    wakes: BoundedRing<ModWakeEvent>,
    wake_producer: SpscU64Producer,
    wake_consumer: SpscU64Consumer,
    content: ModContentRegistries,
    ui_bus: UiLayerBus,
    mod_ui: HashMap<String, RegisteredModUi>,
    timers: TimingWheel<ScheduledModCallback>,
    networks: ModNetworkComponents,
    components: ModComponentStore,
    spatial: ModSpatialIndex,
    /// Host end of the embedded host<->bridge loopback. Wake events are
    /// mirrored here as `Arc` payloads while the bounded wake ring remains
    /// the authoritative backlog for same-thread lanes.
    local_host: LocalEndpoint<ModWakeEvent, ModBridgeCommand>,
    /// Bridge end, handed out exactly once to an embedded bridge lane.
    local_bridge: Option<LocalEndpoint<ModBridgeCommand, ModWakeEvent>>,
}

impl fmt::Debug for ModSpineHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModSpineHost")
            .field("dims", &self.dims)
            .field("next_sub", &self.next_sub)
            .field("subscription_count", &self.subs.len())
            .field("pending_wakes", &self.wakes.len())
            .field("content", &self.content)
            .field("mod_ui_count", &self.mod_ui.len())
            .field("pending_timers", &self.timers.len())
            .field("networks", &self.networks)
            .field("components", &self.components)
            .field("spatial", &self.spatial)
            .finish()
    }
}

impl Default for ModSpineHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ModSpineHost {
    /// A host with a fresh canonical revision table and the vanilla
    /// dimension registry.
    #[must_use]
    pub fn new() -> Self {
        Self::with_rev_table(Rc::new(RefCell::new(SharedSectionRevTable::new())))
    }

    /// A host wired onto an already-shared canonical revision table —
    /// the entry point for lanes that must provably read and bump the
    /// *same* revisions as the rest of the mod ecosystem.
    #[must_use]
    pub fn with_rev_table(revs: Rc<RefCell<SharedSectionRevTable>>) -> Self {
        let (wake_producer, wake_consumer) = spsc_u64(1024);
        let (local_host, local_bridge) = local_pair(1024);
        Self {
            dims: ModDimRegistry::new(),
            revs,
            next_sub: 0,
            subs: HashMap::new(),
            section_subs: CoordMap::default(),
            chunk_subs: CoordMap::default(),
            block_subs: HashMap::new(),
            block_watch_filter: BlockedBloomFilter::with_capacity(1024, 8),
            dirty_voxels: CoordMap::default(),
            wakes: BoundedRing::with_capacity(MAX_PENDING_WAKES),
            wake_producer,
            wake_consumer,
            content: ModContentRegistries::new(),
            ui_bus: UiLayerBus::new(),
            mod_ui: HashMap::new(),
            timers: TimingWheel::new(),
            networks: ModNetworkComponents::new(),
            components: ModComponentStore::new(),
            spatial: ModSpatialIndex::new(),
            local_host,
            local_bridge: Some(local_bridge),
        }
    }

    /// Take the embedded bridge endpoint exactly once. The retained host
    /// endpoint is exercised by every wake-producing block notification;
    /// the returned endpoint receives those events without TCP framing,
    /// compression, or payload copies.
    pub fn take_local_bridge_endpoint(
        &mut self,
    ) -> Option<LocalEndpoint<ModBridgeCommand, ModWakeEvent>> {
        self.local_bridge.take()
    }

    /// Drain control commands sent by the embedded bridge endpoint.
    pub fn drain_local_bridge_commands(&mut self) -> Vec<Arc<ModBridgeCommand>> {
        let mut out = Vec::new();
        while let Some(command) = self.local_host.try_recv() {
            out.push(command);
        }
        out
    }

    /// Host-end counters for the embedded loopback transport.
    #[must_use]
    pub fn local_transport_stats(&self) -> ModLocalTransportStats {
        ModLocalTransportStats {
            sent: self.local_host.sent_count(),
            received: self.local_host.received_count(),
            bytes_saved_estimate: self.local_host.bytes_saved_estimate(),
        }
    }

    /// Cloneable handle to the shared canonical table — hand it to any
    /// lane that must observe or bump the same revisions.
    #[inline]
    #[must_use]
    pub fn shared_rev_table(&self) -> Rc<RefCell<SharedSectionRevTable>> {
        Rc::clone(&self.revs)
    }

    /// The dimension registry (read-only).
    #[inline]
    #[must_use]
    pub fn dims(&self) -> &ModDimRegistry {
        &self.dims
    }

    /// The dimension registry (mutable — register custom dims up front).
    #[inline]
    pub fn dims_mut(&mut self) -> &mut ModDimRegistry {
        &mut self.dims
    }

    /// Current canonical revision of `key` (`0` when never modified).
    #[inline]
    #[must_use]
    pub fn section_rev(&self, key: SectionKey) -> u64 {
        self.revs.borrow().section_rev(key)
    }

    /// Current canonical aggregate revision of `key`.
    #[inline]
    #[must_use]
    pub fn chunk_rev(&self, key: ChunkKey) -> u64 {
        self.revs.borrow().chunk_rev(key)
    }

    /// Subscribe `mod_id` to `watch`; returns the subscription id.
    pub fn subscribe(&mut self, mod_id: impl Into<String>, watch: ModWatch) -> u64 {
        let id = self.next_sub;
        self.next_sub += 1;
        let sub = ModSubscription {
            id,
            mod_id: mod_id.into(),
            watch,
        };
        match watch {
            ModWatch::Section(key) => self.section_subs.entry(key).or_default().push(id),
            ModWatch::Chunk(key) => self.chunk_subs.entry(key).or_default().push(id),
            ModWatch::Block(block) => {
                self.block_watch_filter.insert_u64(u64::from(block.0));
                self.block_subs.entry(block).or_default().push(id);
            }
        }
        self.subs.insert(id, sub);
        id
    }

    /// Shorthand for `subscribe(mod_id, ModWatch::Section(key))`.
    pub fn subscribe_section(&mut self, mod_id: impl Into<String>, key: SectionKey) -> u64 {
        self.subscribe(mod_id, ModWatch::Section(key))
    }

    /// Shorthand for `subscribe(mod_id, ModWatch::Chunk(key))`.
    pub fn subscribe_chunk(&mut self, mod_id: impl Into<String>, key: ChunkKey) -> u64 {
        self.subscribe(mod_id, ModWatch::Chunk(key))
    }

    /// Shorthand for `subscribe(mod_id, ModWatch::Block(block))`.
    pub fn subscribe_block(&mut self, mod_id: impl Into<String>, block: RegistryId) -> u64 {
        self.subscribe(mod_id, ModWatch::Block(block))
    }

    #[inline]
    #[must_use]
    pub fn block_watch_maybe(&self, block: RegistryId) -> bool {
        self.block_watch_filter.contains_u64(u64::from(block.0))
    }

    #[inline]
    #[must_use]
    pub fn block_watcher_count(&self, block: RegistryId) -> usize {
        if !self.block_watch_maybe(block) {
            return 0;
        }
        self.block_subs.get(&block).map_or(0, Vec::len)
    }

    /// Remove one subscription by id; `false` when it no longer exists.
    pub fn unsubscribe(&mut self, id: u64) -> bool {
        let Some(sub) = self.subs.remove(&id) else {
            return false;
        };
        let index = match sub.watch {
            ModWatch::Section(key) => self.section_subs.get_mut(&key),
            ModWatch::Chunk(key) => self.chunk_subs.get_mut(&key),
            ModWatch::Block(block) => self.block_subs.get_mut(&block),
        };
        if let Some(ids) = index {
            ids.retain(|&s| s != id);
        }
        true
    }

    /// Remove every subscription held by `mod_id`; returns the count.
    pub fn unsubscribe_mod(&mut self, mod_id: &str) -> usize {
        let ids: Vec<u64> = self
            .subs
            .values()
            .filter(|s| s.mod_id == mod_id)
            .map(|s| s.id)
            .collect();
        let n = ids.len();
        for id in ids {
            self.unsubscribe(id);
        }
        n
    }

    /// Snapshot of `mod_id`'s live subscriptions (registration order by
    /// id).
    #[must_use]
    pub fn subscriptions_of(&self, mod_id: &str) -> Vec<ModSubscription> {
        let mut out: Vec<ModSubscription> = self
            .subs
            .values()
            .filter(|s| s.mod_id == mod_id)
            .cloned()
            .collect();
        out.sort_by_key(|s| s.id);
        out
    }

    /// Total live subscriptions across every mod.
    #[inline]
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        self.subs.len()
    }

    /// Events queued since the last drain.
    #[inline]
    #[must_use]
    pub fn pending_wakes(&self) -> usize {
        self.wakes.len()
    }

    /// Drain every queued wake (oldest first).
    pub fn drain_wakes(&mut self) -> Vec<ModWakeEvent> {
        let mut out = Vec::with_capacity(self.wakes.len());
        while let Some(event) = self.wakes.pop_front() {
            out.push(event);
        }
        out
    }

    /// Drain only `mod_id`'s queued wakes; other mods' events stay queued.
    pub fn drain_wakes_for(&mut self, mod_id: &str) -> Vec<ModWakeEvent> {
        let mut mine = Vec::new();
        let mut kept = Vec::with_capacity(self.wakes.len());
        while let Some(event) = self.wakes.pop_front() {
            if event.mod_id == mod_id {
                mine.push(event);
            } else {
                kept.push(event);
            }
        }
        for event in kept {
            self.wakes.push_overwrite_oldest(event);
        }
        mine
    }

    #[inline]
    #[must_use]
    pub fn dirty_voxel_mask(&self, key: SectionKey) -> Option<&VoxelBitset4096> {
        self.dirty_voxels.get(&key)
    }

    pub fn clear_dirty_voxels(&mut self, key: SectionKey) -> bool {
        self.dirty_voxels.remove(&key).is_some()
    }

    pub fn drain_wake_signals(&mut self) -> Vec<u64> {
        let mut out = Vec::with_capacity(self.wake_consumer.len());
        while let Some(token) = self.wake_consumer.try_pop() {
            out.push(token);
        }
        out
    }

    /// Generic mutation hook: record one block edit, bump the canonical
    /// table (border propagation included) and wake every subscribed mod
    /// whose watched section or chunk revision moved.
    ///
    /// The affected set is discovered by rev-diffing the ±1 neighbourhood
    /// around the edited block's section — a superset of the keys
    /// [`SharedSectionRevTable::bump_block`] touches — so the wake set
    /// always mirrors the canonical border-propagation rule exactly,
    /// without duplicating it.
    pub fn notify_block_changed(&mut self, dim: DimIndex, pos: BlockPos) -> (u64, u64) {
        self.notify_block_changed_inner(dim, pos, None)
    }

    pub fn notify_block_state_changed(
        &mut self,
        dim: DimIndex,
        pos: BlockPos,
        block: RegistryId,
    ) -> (u64, u64) {
        self.notify_block_changed_inner(dim, pos, Some(block))
    }

    fn notify_block_changed_inner(
        &mut self,
        dim: DimIndex,
        pos: BlockPos,
        block: Option<RegistryId>,
    ) -> (u64, u64) {
        let center = pos.section_key(dim);
        let voxel = pos.local_index() as u16;
        self.dirty_voxels.entry(center).or_default().set(voxel);
        let mut sections: Vec<SectionKey> = Vec::with_capacity(27);
        for dx in -1..=1_i32 {
            for dy in -1..=1_i32 {
                for dz in -1..=1_i32 {
                    let (Some(cx), Some(sy), Some(cz)) = (
                        center.cx.checked_add(dx),
                        center.sy.checked_add(dy),
                        center.cz.checked_add(dz),
                    ) else {
                        continue;
                    };
                    sections.push(SectionKey { dim, cx, sy, cz });
                }
            }
        }
        let mut chunks: Vec<ChunkKey> = Vec::with_capacity(8);
        let mut seen_chunks: HashSet<ChunkKey> = HashSet::with_capacity(8);
        for key in &sections {
            let chunk = key.chunk();
            if seen_chunks.insert(chunk) {
                chunks.push(chunk);
            }
        }

        let (primary, sec_changed, chunk_changed) = {
            let mut table = self.revs.borrow_mut();
            let sec_before: Vec<u64> =
                sections.iter().map(|k| table.section_rev(*k)).collect();
            let chunk_before: Vec<u64> =
                chunks.iter().map(|k| table.chunk_rev(*k)).collect();
            let primary = table.bump_block(dim, pos.x, pos.y, pos.z);
            let sec_changed: Vec<(SectionKey, u64)> = sections
                .iter()
                .zip(sec_before)
                .filter_map(|(&k, old)| {
                    let new = table.section_rev(k);
                    (new != old).then_some((k, new))
                })
                .collect();
            let chunk_changed: Vec<(ChunkKey, u64)> = chunks
                .iter()
                .zip(chunk_before)
                .filter_map(|(&k, old)| {
                    let new = table.chunk_rev(k);
                    (new != old).then_some((k, new))
                })
                .collect();
            (primary, sec_changed, chunk_changed)
        };

        for (key, revision) in sec_changed {
            if let Some(ids) = self.section_subs.get(&key) {
                for &id in ids {
                    if let Some(sub) = self.subs.get(&id) {
                        let event = ModWakeEvent {
                            mod_id: sub.mod_id.clone(),
                            watch: sub.watch,
                            revision,
                        };
                        self.wakes.push_overwrite_oldest(event.clone());
                        if self.local_bridge.is_none() {
                            let _ = self.local_host.try_send(Arc::new(event));
                        }
                        let _ = self.wake_producer.try_push(revision);
                    }
                }
            }
        }
        for (key, revision) in chunk_changed {
            if let Some(ids) = self.chunk_subs.get(&key) {
                for &id in ids {
                    if let Some(sub) = self.subs.get(&id) {
                        let event = ModWakeEvent {
                            mod_id: sub.mod_id.clone(),
                            watch: sub.watch,
                            revision,
                        };
                        self.wakes.push_overwrite_oldest(event.clone());
                        if self.local_bridge.is_none() {
                            let _ = self.local_host.try_send(Arc::new(event));
                        }
                        let _ = self.wake_producer.try_push(revision);
                    }
                }
            }
        }
        if let Some(block) = block {
            if self.block_watch_filter.contains_u64(u64::from(block.0)) {
                if let Some(ids) = self.block_subs.get(&block) {
                    for &id in ids {
                        if let Some(sub) = self.subs.get(&id) {
                            self.wakes.push_overwrite_oldest(ModWakeEvent {
                                mod_id: sub.mod_id.clone(),
                                watch: sub.watch,
                                revision: primary.0,
                            });
                            let _ = self.wake_producer.try_push(primary.0);
                        }
                    }
                }
            }
        }
        primary
    }

    /// Resolve (registering if needed) a dimension resource location and
    /// record the edit — the lane-facing entry for mods that carry names,
    /// not indices.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidResourceLocation`] when `dim_name` is malformed;
    /// no bump or wake is performed.
    pub fn notify_block_changed_named(
        &mut self,
        dim_name: &str,
        pos: BlockPos,
    ) -> Result<(u64, u64), InvalidResourceLocation> {
        let dim = self.dims.resolve_or_register(dim_name)?;
        Ok(self.notify_block_changed(dim, pos))
    }

    #[inline]
    #[must_use]
    pub fn content_registries(&self) -> &ModContentRegistries {
        &self.content
    }

    #[inline]
    pub fn content_registries_mut(&mut self) -> &mut ModContentRegistries {
        &mut self.content
    }

    #[inline]
    #[must_use]
    pub fn ui_layer_bus(&self) -> &UiLayerBus {
        &self.ui_bus
    }

    #[inline]
    pub fn ui_layer_bus_mut(&mut self) -> &mut UiLayerBus {
        &mut self.ui_bus
    }

    fn bump_mod_ui_layer(&mut self, layer: UiLayer) -> Option<Revision> {
        match layer {
            UiLayer::ModHud => Some(self.ui_bus.bump(UiLayer::ModHud.index())),
            UiLayer::ModOverlay => Some(self.ui_bus.bump(UiLayer::ModOverlay.index())),
            _ => None,
        }
    }

    pub fn register_mod_ui(
        &mut self,
        manifest: ModUiManifest,
    ) -> Result<RegistryId, ModUiRegistrationError> {
        let report = audit_manifest(&manifest, 854, 480, 0);
        if !report.is_within_budget() {
            return Err(ModUiRegistrationError::Contract(report.violations));
        }
        let registry_name = format!("{}:{}", manifest.mod_id, manifest.slot.as_str());
        let registry_id = self
            .content
            .register_ui_screen(&manifest.mod_id, &registry_name)
            .map_err(ModUiRegistrationError::Content)?;
        let layer = ui_layer_for_slot(manifest.slot);
        let revision = self
            .bump_mod_ui_layer(layer)
            .expect("mod UI slots always map to a mod layer");
        let key = format!("{}:{}", manifest.mod_id, manifest.slot.as_str());
        self.mod_ui.insert(
            key,
            RegisteredModUi {
                manifest,
                registry_id,
                layer,
                revision,
            },
        );
        Ok(registry_id)
    }

    pub fn mark_mod_ui_dirty(&mut self, mod_id: &str, layer: UiLayer) -> Option<Revision> {
        if !matches!(layer, UiLayer::ModHud | UiLayer::ModOverlay) {
            return None;
        }
        let mut found = false;
        for registered in self.mod_ui.values_mut() {
            if registered.manifest.mod_id == mod_id && registered.layer == layer {
                found = true;
            }
        }
        if !found {
            return None;
        }
        let revision = self.bump_mod_ui_layer(layer)?;
        for registered in self.mod_ui.values_mut() {
            if registered.manifest.mod_id == mod_id && registered.layer == layer {
                registered.revision = revision;
            }
        }
        Some(revision)
    }

    #[must_use]
    pub fn registered_mod_ui(&self, mod_id: &str) -> Vec<&RegisteredModUi> {
        let mut out: Vec<&RegisteredModUi> = self
            .mod_ui
            .values()
            .filter(|ui| ui.manifest.mod_id == mod_id)
            .collect();
        out.sort_by_key(|ui| ui.registry_id);
        out
    }

    #[inline]
    #[must_use]
    pub fn registered_mod_ui_count(&self) -> usize {
        self.mod_ui.len()
    }

    pub fn schedule_mod_callback<F>(&mut self, delay_ticks: u64, callback: F) -> TimerHandle
    where
        F: FnOnce(&mut Self) + 'static,
    {
        self.timers
            .schedule_after(delay_ticks, ScheduledModCallback(Box::new(callback)))
    }

    pub fn cancel_mod_callback(&mut self, handle: TimerHandle) -> bool {
        self.timers.cancel(handle)
    }

    pub fn advance_mod_timers(&mut self) -> usize {
        let mut due = Vec::new();
        self.timers.advance_into(&mut due);
        let count = due.len();
        for scheduled in due {
            (scheduled.0)(self);
        }
        count
    }

    #[inline]
    #[must_use]
    pub fn pending_mod_timers(&self) -> usize {
        self.timers.len()
    }

    #[inline]
    #[must_use]
    pub fn network_components(&self) -> &ModNetworkComponents {
        &self.networks
    }

    #[inline]
    pub fn network_components_mut(&mut self) -> &mut ModNetworkComponents {
        &mut self.networks
    }

    #[inline]
    #[must_use]
    pub fn component_store(&self) -> &ModComponentStore {
        &self.components
    }

    #[inline]
    pub fn component_store_mut(&mut self) -> &mut ModComponentStore {
        &mut self.components
    }

    #[inline]
    #[must_use]
    pub fn spatial_index(&self) -> &ModSpatialIndex {
        &self.spatial
    }

    #[inline]
    pub fn spatial_index_mut(&mut self) -> &mut ModSpatialIndex {
        &mut self.spatial
    }
}

impl ModEventSink for ModSpineHost {
    fn notify_block_changed(&mut self, dim: DimIndex, pos: BlockPos) -> (u64, u64) {
        ModSpineHost::notify_block_changed(self, dim, pos)
    }
}

// ---------------------------------------------------------------------------
// Generic mod-state persistence: canonical LZ4 + CRC32 envelope
// ---------------------------------------------------------------------------

/// Magic tag opening every mod-state blob (`MST1`).
const MOD_STATE_MAGIC: [u8; 4] = *b"MST1";
/// Envelope header: magic + crc32 + declared payload length.
const MOD_STATE_HEADER: usize = 12;

/// Why [`decode_mod_state_lz4`] rejected a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModStateError {
    /// Fewer bytes than the fixed envelope header.
    Truncated,
    /// The blob does not open with the `MST1` magic.
    BadMagic,
    /// The declared payload length exceeds the caller's bound — checked
    /// *before* any decompression so a hostile header cannot amplify.
    DeclaredTooLarge {
        /// Payload length the header claims.
        declared: u64,
        /// The caller's cap.
        max: usize,
    },
    /// The compressed payload failed canonical LZ4 decoding.
    Lz4(Lz4Error),
    /// Decoded payload CRC32 differs from the envelope's.
    CrcMismatch {
        /// CRC stored in the envelope.
        expected: u32,
        /// CRC of the decoded payload.
        actual: u32,
    },
}

impl fmt::Display for ModStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("mod state blob shorter than the MST1 header"),
            Self::BadMagic => f.write_str("mod state blob missing the MST1 magic"),
            Self::DeclaredTooLarge { declared, max } => {
                write!(f, "declared mod state {declared} exceeds bound {max}")
            }
            Self::Lz4(e) => write!(f, "mod state LZ4 payload rejected: {e}"),
            Self::CrcMismatch { expected, actual } => write!(
                f,
                "mod state CRC32 mismatch: stored {expected:08X}, decoded {actual:08X}"
            ),
        }
    }
}

impl std::error::Error for ModStateError {}

/// Encode one opaque per-mod state blob into the canonical `MST1`
/// envelope:
///
/// ```text
/// "MST1" | crc32_ieee(payload) u32 LE | payload_len u32 LE | LZ4 block
/// ```
///
/// The CRC covers the *plaintext* so a tampered stream that still decodes
/// to altered bytes is caught, and the decoder bounds output to the
/// declared length before writing a single byte.
#[must_use]
pub fn encode_mod_state_lz4(state: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MOD_STATE_HEADER + state.len() / 2 + 32);
    out.extend_from_slice(&MOD_STATE_MAGIC);
    out.extend_from_slice(&crc32_ieee(state).to_le_bytes());
    // A state ≥ 4 GiB cannot round-trip the u32 length field; saturating
    // keeps encode total and decode still fails closed on length checks.
    let declared = u32::try_from(state.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&declared.to_le_bytes());
    compress_block_into(state, &mut out);
    out
}

/// Decode an [`encode_mod_state_lz4`] blob, failing closed.
///
/// `max_bytes` is a hard cap on the *decoded* payload, enforced before
/// decompression touches the declared length — a hostile header cannot
/// amplify past it. CRC mismatch, truncation, bad magic and LZ4 faults
/// each produce a distinct [`ModStateError`].
///
/// # Errors
///
/// Returns [`ModStateError`] on any integrity or bound violation.
pub fn decode_mod_state_lz4(encoded: &[u8], max_bytes: usize) -> Result<Vec<u8>, ModStateError> {
    if encoded.len() < MOD_STATE_HEADER {
        return Err(ModStateError::Truncated);
    }
    if encoded[..4] != MOD_STATE_MAGIC {
        return Err(ModStateError::BadMagic);
    }
    let expected_crc = u32::from_le_bytes(encoded[4..8].try_into().unwrap_or([0; 4]));
    let declared = u32::from_le_bytes(encoded[8..12].try_into().unwrap_or([0; 4]));
    if u64::from(declared) > max_bytes as u64 {
        return Err(ModStateError::DeclaredTooLarge {
            declared: u64::from(declared),
            max: max_bytes,
        });
    }
    let payload =
        decompress_block_exact(&encoded[MOD_STATE_HEADER..], declared as usize)
            .map_err(ModStateError::Lz4)?;
    let actual = crc32_ieee(&payload);
    if actual != expected_crc {
        return Err(ModStateError::CrcMismatch {
            expected: expected_crc,
            actual,
        });
    }
    Ok(payload)
}
