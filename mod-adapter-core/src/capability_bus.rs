// SPDX-License-Identifier: LGPL-3.0-or-later

//! Generic cross-mod block-capability bus (`XMOD1-CORE`) — a pure-Rust
//! model of NeoForge 1.21.1 `BlockCapabilities` / Fabric Transfer API so
//! ANY loaded mod (and vanilla hoppers/chests/furnaces) can exchange
//! items, fluids, and energy through one shared registry with **zero**
//! hardcoded mod-to-mod special cases.
//!
//! - [`FaceDirection`] + dimension-generic [`BlockCoord`] (never
//!   Overworld-only — `dim_id` carries `"minecraft:the_nether"`,
//!   `"mod_a:aether"`, anything).
//! - [`CapabilityItemStack`] is the lossless 1.21.1 item stack: item id,
//!   count, and opaque `raw_components` (Data Components bytes) preserved
//!   byte-for-byte across every hop; stacks merge only when id **and**
//!   components are identical.
//! - [`ItemTransferHandler`], [`FluidTransferHandler`], and
//!   [`EnergyTransferHandler`] mirror the `IItemHandler` / `IFluidHandler`
//!   / `IEnergyStorage` contract exactly, including `simulate: bool`
//!   probes that never mutate state.
//! - [`BlockCapabilityBus::transfer_item_between`] /
//!   [`transfer_fluid_between`](Self::transfer_fluid_between) /
//!   [`transfer_energy_between`](Self::transfer_energy_between) are
//!   atomic simulate-then-commit: a full or rejecting destination can
//!   never duplicate or vanish cargo.
//! - [`SimpleSlotInventory`], [`SimpleFluidTank`], and
//!   [`SimpleEnergyBuffer`] are ready-made generic endpoints;
//!   [`CookingPotItemAdapter`] wraps
//!   [`crate::culinary_adapter::CookingPotState`] as living cross-mod
//!   proof (top/side insert inputs, bottom extracts the finished meal).

use std::collections::{BTreeSet, HashMap};

use crate::culinary_adapter::{CookingPotState, CulinaryRegistry};

/// The six world faces a capability may be queried through. `None` on
/// the transfer traits means internal/programmatic access (no face).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FaceDirection {
    /// -Y (bottom face).
    Down,
    /// +Y (top face).
    Up,
    /// -Z.
    North,
    /// +Z.
    South,
    /// -X.
    West,
    /// +X.
    East,
}

/// Dimension-generic block position. `dim_id` is the canonical
/// dimension resource id (`"minecraft:overworld"`,
/// `"minecraft:the_nether"`, `"mod_a:aether"`, …) so endpoints in
/// different dimensions can never alias each other.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockCoord {
    /// Dimension resource id.
    pub dim_id: String,
    /// Block X.
    pub x: i32,
    /// Block Y.
    pub y: i32,
    /// Block Z.
    pub z: i32,
}

impl BlockCoord {
    /// Construct a coord.
    pub fn new(dim_id: impl Into<String>, x: i32, y: i32, z: i32) -> Self {
        Self {
            dim_id: dim_id.into(),
            x,
            y,
            z,
        }
    }
}

/// Lossless 1.21.1 item stack for capability transfer: canonical
/// `ns:item` id, count, and opaque Data Component bytes. `raw_components`
/// is preserved byte-for-byte across every hop; two stacks merge **only**
/// when `item_id` and `raw_components` are both identical.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityItemStack {
    /// Canonical `"namespace:item"` id (`"minecraft:iron_ingot"`,
    /// `"create:brass_sheet"`, `"farmersdelight:tomato"`, …).
    pub item_id: String,
    /// Item count (`0` = empty).
    pub count: u32,
    /// Opaque 1.21.1 Data Components bytes — never inspected by the bus,
    /// carried verbatim through every transfer.
    pub raw_components: Vec<u8>,
}

impl CapabilityItemStack {
    /// Stack of `count` items with no components.
    pub fn new(item_id: impl Into<String>, count: u32) -> Self {
        Self {
            item_id: item_id.into(),
            count,
            raw_components: Vec::new(),
        }
    }

    /// Stack of `count` items carrying `components`.
    pub fn with_components(
        item_id: impl Into<String>,
        count: u32,
        components: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            item_id: item_id.into(),
            count,
            raw_components: components.into(),
        }
    }

    /// `true` when this stack holds nothing.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Can `other` merge onto this stack? Same id **and** identical
    /// component bytes — a 1.21.1 Data-Component-aware merge rule.
    pub fn can_merge(&self, other: &CapabilityItemStack) -> bool {
        self.item_id == other.item_id && self.raw_components == other.raw_components
    }

    /// Split off up to `n` items, leaving the remainder behind.
    pub fn take(&mut self, n: u32) -> CapabilityItemStack {
        let moved = n.min(self.count);
        self.count -= moved;
        CapabilityItemStack {
            item_id: self.item_id.clone(),
            count: moved,
            raw_components: self.raw_components.clone(),
        }
    }
}

/// Fabric fluid units in one vanilla/NeoForge bucket (1,000 mB).
pub const DROPLETS_PER_BUCKET: u64 = 81_000;
/// Fabric fluid units in one milli-bucket.
pub const DROPLETS_PER_MB: u64 = 81;

/// Exact fluid amount backed by Fabric droplets. Using droplets as the
/// canonical integer avoids the historical `81x` scaling error: every
/// milli-bucket is exactly 81 droplets and one bucket exactly 81,000.
/// Conversion back to milli-buckets is lossless only when the droplet count
/// is divisible by 81; use [`FluidAmount::try_to_mb`] to enforce that or
/// [`FluidAmount::to_mb_parts`] to expose the exact remainder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FluidAmount {
    droplets: u128,
}

impl FluidAmount {
    /// Zero fluid.
    pub const ZERO: Self = Self { droplets: 0 };

    /// Exact amount from milli-buckets.
    pub const fn from_mb(amount_mb: u64) -> Self {
        Self {
            droplets: amount_mb as u128 * DROPLETS_PER_MB as u128,
        }
    }

    /// Exact amount from Fabric droplets.
    pub const fn from_droplets(droplets: u64) -> Self {
        Self {
            droplets: droplets as u128,
        }
    }

    /// Exact droplet count. The canonical representation is `u128` so even
    /// `from_mb(u64::MAX)` round-trips without saturation or hidden loss.
    pub fn to_droplets(self) -> u128 {
        self.droplets
    }

    /// Whole milli-buckets, discarding any sub-mB droplet remainder. Prefer
    /// [`Self::try_to_mb`] when fractional mB must never be silently dropped.
    pub fn to_mb(self) -> u64 {
        u64::try_from(self.droplets / DROPLETS_PER_MB as u128).unwrap_or(u64::MAX)
    }

    /// Exact `(whole_milli_buckets, droplet_remainder)` decomposition.
    pub fn to_mb_parts(self) -> (u64, u64) {
        let whole = u64::try_from(self.droplets / DROPLETS_PER_MB as u128).unwrap_or(u64::MAX);
        let rem = u64::try_from(self.droplets % DROPLETS_PER_MB as u128).unwrap_or(0);
        (whole, rem)
    }

    /// Exact milli-buckets, or `None` when a non-zero droplet remainder
    /// would be lost.
    pub fn try_to_mb(self) -> Option<u64> {
        let (whole, remainder) = self.to_mb_parts();
        (remainder == 0).then_some(whole)
    }

    /// `true` when the amount is an exact whole number of milli-buckets.
    pub fn is_exact_mb(self) -> bool {
        self.droplets.is_multiple_of(DROPLETS_PER_MB as u128)
    }
}

/// Lossless 1.21.1 fluid stack for capability transfer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityFluidStack {
    /// Canonical `"namespace:fluid"` id (`"minecraft:water"`,
    /// `"mymod:molten_zinc"`, …).
    pub fluid_id: String,
    /// Amount in milli-buckets.
    pub amount_mb: u64,
    /// Opaque fluid component bytes, carried verbatim.
    pub raw_components: Vec<u8>,
}

impl CapabilityFluidStack {
    /// Stack of `amount_mb` milli-buckets with no components.
    pub fn new(fluid_id: impl Into<String>, amount_mb: u64) -> Self {
        Self {
            fluid_id: fluid_id.into(),
            amount_mb,
            raw_components: Vec::new(),
        }
    }

    /// Stack of `amount_mb` milli-buckets carrying `components`.
    pub fn with_components(
        fluid_id: impl Into<String>,
        amount_mb: u64,
        components: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            fluid_id: fluid_id.into(),
            amount_mb,
            raw_components: components.into(),
        }
    }

    /// `true` when this stack holds nothing.
    pub fn is_empty(&self) -> bool {
        self.amount_mb == 0
    }

    /// Exact fluid amount represented by this legacy milli-bucket stack.
    pub fn amount(&self) -> FluidAmount {
        FluidAmount::from_mb(self.amount_mb)
    }

    /// Exact droplet count represented by this legacy milli-bucket stack.
    pub fn amount_droplets(&self) -> u128 {
        self.amount().to_droplets()
    }

    /// Build a stack from a droplet-exact amount, returning `None` when it
    /// is not representable in the legacy milli-bucket field without loss.
    pub fn from_fluid_amount(fluid_id: impl Into<String>, amount: FluidAmount) -> Option<Self> {
        amount
            .try_to_mb()
            .map(|amount_mb| Self::new(fluid_id, amount_mb))
    }

    /// Same fluid id **and** identical component bytes.
    pub fn can_merge(&self, other: &CapabilityFluidStack) -> bool {
        self.fluid_id == other.fluid_id && self.raw_components == other.raw_components
    }
}

// ---------------------------------------------------------------------------
// Transfer traits — exact NeoForge IItemHandler / IFluidHandler /
// IEnergyStorage contract: `simulate: true` probes without mutating.
// ---------------------------------------------------------------------------

/// Item-side block capability (`IItemHandler` analogue).
pub trait ItemTransferHandler {
    /// Downcast support so callers holding `Box<dyn …>` can recover the
    /// concrete endpoint type.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Mutable downcast support.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Insert `stack` through `face`; returns the remainder that did not
    /// fit. `simulate: true` must not mutate state.
    fn insert_item(
        &mut self,
        face: Option<FaceDirection>,
        stack: &CapabilityItemStack,
        simulate: bool,
    ) -> CapabilityItemStack;

    /// Extract up to `max_count` items matching `filter` (all stacks when
    /// `None`) through `face`. `simulate: true` must not mutate state.
    fn extract_item(
        &mut self,
        face: Option<FaceDirection>,
        max_count: u32,
        filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
        simulate: bool,
    ) -> Option<CapabilityItemStack>;

    /// Number of addressable item slots.
    fn slots(&self) -> usize;
}

/// Fluid-side block capability (`IFluidHandler` analogue).
pub trait FluidTransferHandler {
    /// Downcast support.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Mutable downcast support.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Offer `resource` through `face`; returns milli-buckets accepted.
    /// `simulate: true` must not mutate state.
    fn fill_fluid(
        &mut self,
        face: Option<FaceDirection>,
        resource: &CapabilityFluidStack,
        simulate: bool,
    ) -> u64;

    /// Drain up to `max_mb` through `face`, restricted to `filter_fluid`
    /// when given. `simulate: true` must not mutate state.
    fn drain_fluid(
        &mut self,
        face: Option<FaceDirection>,
        max_mb: u64,
        filter_fluid: Option<&str>,
        simulate: bool,
    ) -> Option<CapabilityFluidStack>;

    /// Number of addressable tanks. Single-tank endpoints (the common
    /// case) keep the default `1`; multi-tank machines override.
    fn fluid_tank_count(&self) -> usize {
        1
    }

    /// Non-mutating snapshot of tank `tank`'s contents. The default
    /// reports `None`; endpoints that can observe a tank without
    /// draining it override (see [`SimpleFluidTank`]).
    fn peek_fluid(&self, tank: usize) -> Option<CapabilityFluidStack> {
        let _ = tank;
        None
    }

    /// Face-agnostic convenience fill (internal access, `None` face):
    /// returns milli-buckets accepted. `simulate: true` must not mutate.
    fn insert_fluid(&mut self, fluid: CapabilityFluidStack, simulate: bool) -> u64 {
        self.fill_fluid(None, &fluid, simulate)
    }

    /// Face-agnostic convenience drain (internal access, `None` face),
    /// restricted to `fluid_filter` when given. `simulate: true` must
    /// not mutate.
    fn extract_fluid(
        &mut self,
        fluid_filter: Option<&str>,
        max_mb: u64,
        simulate: bool,
    ) -> Option<CapabilityFluidStack> {
        self.drain_fluid(None, max_mb, fluid_filter, simulate)
    }
}

/// Energy-side block capability (`IEnergyStorage` analogue; amounts in
/// FE).
pub trait EnergyTransferHandler {
    /// Downcast support.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Mutable downcast support.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Offer up to `max_receive_fe` through `face`; returns FE accepted.
    /// `simulate: true` must not mutate state.
    fn receive_energy(
        &mut self,
        face: Option<FaceDirection>,
        max_receive_fe: u64,
        simulate: bool,
    ) -> u64;

    /// Draw up to `max_extract_fe` through `face`; returns FE provided.
    /// `simulate: true` must not mutate state.
    fn extract_energy(
        &mut self,
        face: Option<FaceDirection>,
        max_extract_fe: u64,
        simulate: bool,
    ) -> u64;

    /// FE currently stored.
    fn energy_stored(&self) -> u64;

    /// FE capacity.
    fn max_energy_stored(&self) -> u64;

    /// Face-agnostic convenience alias for
    /// [`energy_stored`](Self::energy_stored).
    fn stored_fe(&self) -> u64 {
        self.energy_stored()
    }

    /// Face-agnostic convenience alias for
    /// [`max_energy_stored`](Self::max_energy_stored).
    fn capacity_fe(&self) -> u64 {
        self.max_energy_stored()
    }

    /// Face-agnostic convenience receive (internal access, `None`
    /// face): returns FE accepted. `simulate: true` must not mutate.
    fn receive_fe(&mut self, max_fe: u64, simulate: bool) -> u64 {
        self.receive_energy(None, max_fe, simulate)
    }

    /// Face-agnostic convenience extract (internal access, `None`
    /// face): returns FE provided. `simulate: true` must not mutate.
    fn extract_fe(&mut self, max_fe: u64, simulate: bool) -> u64 {
        self.extract_energy(None, max_fe, simulate)
    }
}

// ---------------------------------------------------------------------------
// Energy units — one internal FE canonical form, declared unit kept
// alongside so cross-mod conversions stay explicit.
// ---------------------------------------------------------------------------

/// Energy unit family an [`EnergyAmount`] is denominated in. FE is the
/// bus-internal canonical unit; RF, EU, and Create-style kinetic Stress
/// (SU) convert through [`EnergyConversionRates`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EnergyUnit {
    /// Forge Energy (the canonical unit).
    Fe,
    /// Redstone Flux — `1 RF == 1 FE` by default.
    Rf,
    /// Industrial EU — `1 EU == 4 FE` by default.
    Eu,
    /// Rotational Stress Units — `1 SU == 2 FE` by default.
    StressSu,
}

/// Integer FE-per-unit conversion table. All math is saturating: a
/// hostile or absurd `u64::MAX` amount clamps instead of overflowing,
/// and there is no floating point anywhere (zero NaN, zero rounding
/// drift). A `0` rate is treated as `1` so division can never panic —
/// callers that need lossless conversion check
/// [`EnergyConversionRates::converts_losslessly`] first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnergyConversionRates {
    /// FE per 1 RF (default `1`).
    pub fe_per_rf: u64,
    /// FE per 1 EU (default `4`).
    pub fe_per_eu: u64,
    /// FE per 1 Stress SU (default `2`).
    pub fe_per_stress_su: u64,
}

impl Default for EnergyConversionRates {
    fn default() -> Self {
        Self {
            fe_per_rf: 1,
            fe_per_eu: 4,
            fe_per_stress_su: 2,
        }
    }
}

impl EnergyConversionRates {
    /// FE-per-unit rate for `unit`, never `0` (a `0`-configured rate
    /// degrades to identity rather than dividing by zero).
    pub fn fe_per(&self, unit: EnergyUnit) -> u64 {
        let rate = match unit {
            EnergyUnit::Fe => 1,
            EnergyUnit::Rf => self.fe_per_rf,
            EnergyUnit::Eu => self.fe_per_eu,
            EnergyUnit::StressSu => self.fe_per_stress_su,
        };
        rate.max(1)
    }

    /// Convert `amount` of `unit` into canonical FE, saturating at
    /// `u64::MAX`.
    pub fn to_fe(&self, unit: EnergyUnit, amount: u64) -> u64 {
        amount.saturating_mul(self.fe_per(unit))
    }

    /// Convert canonical `fe` into `unit`, rounding down (the remainder
    /// cannot be represented in the coarser unit).
    pub fn from_fe(&self, unit: EnergyUnit, fe: u64) -> u64 {
        fe / self.fe_per(unit)
    }

    /// `true` when `amount` of `from` converts to `to` with no remainder
    /// — the strict check callers use before a lossless-only exchange.
    pub fn converts_losslessly(
        &self,
        from: EnergyUnit,
        to: EnergyUnit,
        amount: u64,
    ) -> bool {
        let fe = self.to_fe(from, amount);
        fe != u64::MAX && fe.is_multiple_of(self.fe_per(to))
    }
}

/// An amount of energy denominated in a specific [`EnergyUnit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnergyAmount {
    /// Unit family.
    pub unit: EnergyUnit,
    /// Amount in `unit`.
    pub amount: u64,
}

impl EnergyAmount {
    /// `amount` of `unit`.
    pub fn new(unit: EnergyUnit, amount: u64) -> Self {
        Self { unit, amount }
    }

    /// Canonical FE equivalent under `rates` (saturating).
    pub fn to_fe(&self, rates: &EnergyConversionRates) -> u64 {
        rates.to_fe(self.unit, self.amount)
    }

    /// Re-denominate into `target`, rounding down when the FE value is
    /// not an exact multiple of the target rate.
    pub fn convert(&self, target: EnergyUnit, rates: &EnergyConversionRates) -> EnergyAmount {
        EnergyAmount {
            unit: target,
            amount: rates.from_fe(target, self.to_fe(rates)),
        }
    }

    /// Re-denominate into `target` only when the conversion is exact;
    /// `None` on any remainder or saturation.
    pub fn convert_exact(
        &self,
        target: EnergyUnit,
        rates: &EnergyConversionRates,
    ) -> Option<EnergyAmount> {
        if rates.converts_losslessly(self.unit, target, self.amount) {
            Some(self.convert(target, rates))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The bus
// ---------------------------------------------------------------------------

/// Exact accounting of one committed item transfer so **zero** items can
/// silently vanish: `delivered + returned_to_source + overflow.count` is
/// always exactly the count extracted from the source (`taken`). A
/// destination that refused part of the stack drives `delivered` down;
/// a source that refuses the rollback surfaces the unreturned remainder
/// as [`ItemTransferResult::overflow`] instead of deleting it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemTransferResult {
    /// Items the destination actually accepted.
    pub delivered: u32,
    /// Items pushed back into the source (destination-refused remainder).
    pub returned_to_source: u32,
    /// Items neither endpoint accepted — surfaced, never dropped.
    pub overflow: Option<CapabilityItemStack>,
}

impl ItemTransferResult {
    /// Total items accounted for: `delivered + returned + overflow`.
    pub fn accounted(&self) -> u32 {
        self.delivered
            .saturating_add(self.returned_to_source)
            .saturating_add(self.overflow.as_ref().map_or(0, |s| s.count))
    }
}

/// Exact accounting of one committed fluid transfer (milli-buckets):
/// `delivered + returned_to_source + overflow.amount_mb` is always exactly
/// the amount drained from the source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FluidTransferResult {
    /// Milli-buckets the destination tank accepted.
    pub delivered: u64,
    /// Milli-buckets pushed back into the source tank.
    pub returned_to_source: u64,
    /// Fluid neither tank accepted — surfaced, never deleted.
    pub overflow: Option<CapabilityFluidStack>,
}

impl FluidTransferResult {
    /// Total milli-buckets accounted for.
    pub fn accounted_mb(&self) -> u64 {
        self.delivered
            .saturating_add(self.returned_to_source)
            .saturating_add(self.overflow.as_ref().map_or(0, |f| f.amount_mb))
    }
}

/// Exact accounting of one committed energy transfer (FE): `delivered +
/// returned_to_source + overflow_fe` is always exactly the amount drawn
/// from the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EnergyTransferResult {
    /// FE the destination buffer accepted.
    pub delivered: u64,
    /// FE pushed back into the source buffer.
    pub returned_to_source: u64,
    /// FE neither buffer accepted — surfaced, never deleted.
    pub overflow_fe: u64,
}

impl EnergyTransferResult {
    /// Total FE accounted for.
    pub fn accounted_fe(&self) -> u64 {
        self.delivered
            .saturating_add(self.returned_to_source)
            .saturating_add(self.overflow_fe)
    }
}

/// One registry of heterogeneous item/fluid/energy endpoints keyed by
/// [`BlockCoord`]. Vanilla chests and hoppers, Create belts and tanks,
/// culinary pots, and arbitrary mod machines all register in the same
/// bus; the `transfer_*_between` helpers then move cargo between any two
/// endpoints atomically.
///
/// Every register/unregister/[`invalidate_at`](Self::invalidate_at) bumps
/// a per-coord revision counter, the NeoForge
/// `invalidateCapabilities(pos)` contract: a listener that cached a
/// capability compares [`invalidation_rev`](Self::invalidation_rev)
/// against the revision it cached and re-queries on any bump.
#[derive(Default)]
pub struct BlockCapabilityBus {
    items: HashMap<BlockCoord, Box<dyn ItemTransferHandler + Send>>,
    fluids: HashMap<BlockCoord, Box<dyn FluidTransferHandler + Send>>,
    energy: HashMap<BlockCoord, Box<dyn EnergyTransferHandler + Send>>,
    /// Per-coord capability invalidation revision (NeoForge
    /// `invalidateCapabilities`); bumped on register/unregister/block-change.
    invalidation_revs: HashMap<BlockCoord, u64>,
}

impl BlockCapabilityBus {
    /// Empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an item endpoint at `coord`; returns the replaced
    /// handler, if any. Bumps the coord's capability invalidation
    /// revision so cached `IItemHandler` lookups re-query.
    pub fn register_item_handler(
        &mut self,
        coord: BlockCoord,
        handler: Box<dyn ItemTransferHandler + Send>,
    ) -> Option<Box<dyn ItemTransferHandler + Send>> {
        let replaced = self.items.insert(coord.clone(), handler);
        self.bump_invalidation(&coord);
        replaced
    }

    /// Register a fluid endpoint at `coord`; returns the replaced
    /// handler, if any. Bumps the coord's invalidation revision.
    pub fn register_fluid_handler(
        &mut self,
        coord: BlockCoord,
        handler: Box<dyn FluidTransferHandler + Send>,
    ) -> Option<Box<dyn FluidTransferHandler + Send>> {
        let replaced = self.fluids.insert(coord.clone(), handler);
        self.bump_invalidation(&coord);
        replaced
    }

    /// Register an energy endpoint at `coord`; returns the replaced
    /// handler, if any. Bumps the coord's invalidation revision.
    pub fn register_energy_handler(
        &mut self,
        coord: BlockCoord,
        handler: Box<dyn EnergyTransferHandler + Send>,
    ) -> Option<Box<dyn EnergyTransferHandler + Send>> {
        let replaced = self.energy.insert(coord.clone(), handler);
        self.bump_invalidation(&coord);
        replaced
    }

    /// Remove the item endpoint at `coord`, if any. Bumps the
    /// invalidation revision only when a handler was actually removed.
    pub fn unregister_item_handler(
        &mut self,
        coord: &BlockCoord,
    ) -> Option<Box<dyn ItemTransferHandler + Send>> {
        let removed = self.items.remove(coord);
        if removed.is_some() {
            self.bump_invalidation(coord);
        }
        removed
    }

    /// Remove the fluid endpoint at `coord`, if any.
    pub fn unregister_fluid_handler(
        &mut self,
        coord: &BlockCoord,
    ) -> Option<Box<dyn FluidTransferHandler + Send>> {
        let removed = self.fluids.remove(coord);
        if removed.is_some() {
            self.bump_invalidation(coord);
        }
        removed
    }

    /// Remove the energy endpoint at `coord`, if any.
    pub fn unregister_energy_handler(
        &mut self,
        coord: &BlockCoord,
    ) -> Option<Box<dyn EnergyTransferHandler + Send>> {
        let removed = self.energy.remove(coord);
        if removed.is_some() {
            self.bump_invalidation(coord);
        }
        removed
    }

    /// The coord's capability invalidation revision (`0` when it has
    /// never changed). NeoForge-style contract: a listener that cached a
    /// capability handle records this revision and re-queries when it
    /// bumps.
    pub fn invalidation_rev(&self, pos: &BlockCoord) -> u64 {
        self.invalidation_revs.get(pos).copied().unwrap_or(0)
    }

    /// Explicitly invalidate every cached capability at `pos` — the
    /// block-change path (`setBlock`/`neighborChanged`) that NeoForge's
    /// `level.invalidateCapabilities(pos)` models. Always bumps, even
    /// when no endpoint is currently registered, because a block swap may
    /// have replaced a handler outside this registry.
    pub fn invalidate_at(&mut self, pos: &BlockCoord) {
        self.bump_invalidation(pos);
    }

    /// Increment `coord`'s invalidation revision (wrapping at `u64::MAX`
    /// rather than panicking in a long-lived registry).
    fn bump_invalidation(&mut self, coord: &BlockCoord) {
        let rev = self.invalidation_revs.entry(coord.clone()).or_insert(0);
        *rev = rev.wrapping_add(1);
    }

    /// Borrow the item endpoint at `coord`.
    pub fn item_endpoint(&self, coord: &BlockCoord) -> Option<&(dyn ItemTransferHandler + Send)> {
        self.items.get(coord).map(Box::as_ref)
    }

    /// Borrow the item endpoint at `coord` mutably.
    pub fn item_endpoint_mut(
        &mut self,
        coord: &BlockCoord,
    ) -> Option<&mut (dyn ItemTransferHandler + Send + 'static)> {
        self.items.get_mut(coord).map(Box::as_mut)
    }

    /// Borrow the fluid endpoint at `coord`.
    pub fn fluid_endpoint(&self, coord: &BlockCoord) -> Option<&(dyn FluidTransferHandler + Send)> {
        self.fluids.get(coord).map(Box::as_ref)
    }

    /// Borrow the fluid endpoint at `coord` mutably.
    pub fn fluid_endpoint_mut(
        &mut self,
        coord: &BlockCoord,
    ) -> Option<&mut (dyn FluidTransferHandler + Send + 'static)> {
        self.fluids.get_mut(coord).map(Box::as_mut)
    }

    /// Borrow the energy endpoint at `coord`.
    pub fn energy_endpoint(
        &self,
        coord: &BlockCoord,
    ) -> Option<&(dyn EnergyTransferHandler + Send)> {
        self.energy.get(coord).map(Box::as_ref)
    }

    /// Borrow the energy endpoint at `coord` mutably.
    pub fn energy_endpoint_mut(
        &mut self,
        coord: &BlockCoord,
    ) -> Option<&mut (dyn EnergyTransferHandler + Send + 'static)> {
        self.energy.get_mut(coord).map(Box::as_mut)
    }

    /// Atomically move up to `max_count` items from `from` to `to`.
    ///
    /// Simulate-then-commit: first a simulated extract learns *what*
    /// would leave the source, then a simulated insert learns how much of
    /// it the destination would take. Only that amount is committed —
    /// with a same-identity extract filter — so a full or rejecting
    /// destination can neither duplicate nor vanish items. Anything the
    /// committed insert refuses is pushed back into the source.
    ///
    /// Returns the item count actually delivered to the destination —
    /// exactly [`ItemTransferResult::delivered`] of
    /// [`transfer_item_checked`](Self::transfer_item_checked). `0` when
    /// either endpoint is missing, the source is empty, the destination
    /// rejects, or `from == to`.
    pub fn transfer_item_between(
        &mut self,
        from: &BlockCoord,
        from_face: Option<FaceDirection>,
        to: &BlockCoord,
        to_face: Option<FaceDirection>,
        max_count: u32,
        filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
    ) -> u32 {
        self.transfer_item_checked(from, from_face, to, to_face, max_count, filter)
            .delivered
    }

    /// [`transfer_item_between`](Self::transfer_item_between) with exact
    /// conservation accounting: `result.delivered +
    /// result.returned_to_source + result.overflow.count` is always the
    /// count committed out of the source. `delivered` is
    /// `taken − leftover` — the destination's refused remainder is rolled
    /// back into the source, and anything the source refuses on the
    /// rollback is surfaced as `overflow` rather than counted as
    /// delivered or silently deleted.
    pub fn transfer_item_checked(
        &mut self,
        from: &BlockCoord,
        from_face: Option<FaceDirection>,
        to: &BlockCoord,
        to_face: Option<FaceDirection>,
        max_count: u32,
        filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
    ) -> ItemTransferResult {
        let empty = ItemTransferResult::default;
        if from == to || max_count == 0 {
            return empty();
        }
        let Some(src) = self.items.get_mut(from) else {
            return empty();
        };
        let Some(probe) = src.extract_item(from_face, max_count, filter, true) else {
            return empty();
        };
        if probe.is_empty() {
            return empty();
        }
        let Some(dst) = self.items.get_mut(to) else {
            return empty();
        };
        let remainder = dst.insert_item(to_face, &probe, true);
        let movable = probe.count.saturating_sub(remainder.count);
        if movable == 0 {
            return empty();
        }
        // Commit: extract exactly the probed identity (components must
        // match too) so a destination never receives a different stack
        // than it simulated.
        let probe_id = probe.item_id.clone();
        let probe_components = probe.raw_components.clone();
        let src = self.items.get_mut(from);
        let Some(taken) = src.and_then(|h| {
            h.extract_item(
                from_face,
                movable,
                Some(&|s: &CapabilityItemStack| {
                    s.item_id == probe_id
                        && s.raw_components == probe_components
                        && filter.is_none_or(|f| f(s))
                }),
                false,
            )
        }) else {
            return empty();
        };
        let taken_count = taken.count;
        let mut result = ItemTransferResult {
            delivered: taken_count,
            ..ItemTransferResult::default()
        };
        let Some(dst) = self.items.get_mut(to) else {
            result.overflow = Some(taken);
            result.delivered = 0;
            return result;
        };
        let leftover = dst.insert_item(to_face, &taken, false);
        // `insert_item` returns the unaccepted remainder, so the delivered
        // count is `taken − leftover` — never the source's re-insert
        // response.
        result.delivered = taken_count.saturating_sub(leftover.count);
        if leftover.count > 0 {
            // Roll the refused remainder back into the source. Whatever
            // the source refuses is overflow: it left the source, reached
            // neither endpoint, and must be surfaced rather than vanish.
            match self.items.get_mut(from) {
                Some(src) => {
                    let refused = src.insert_item(from_face, &leftover, false);
                    result.returned_to_source = leftover.count.saturating_sub(refused.count);
                    if refused.count > 0 {
                        result.overflow = Some(refused);
                    }
                }
                None => {
                    result.overflow = Some(leftover);
                }
            }
        }
        result
    }

    /// Atomically move up to `max_mb` of fluid from `from` to `to` via
    /// simulate-then-commit; a rejecting or full destination rolls the
    /// committed drain back into the source. Returns mB delivered —
    /// exactly [`FluidTransferResult::delivered`] of
    /// [`transfer_fluid_checked`](Self::transfer_fluid_checked).
    pub fn transfer_fluid_between(
        &mut self,
        from: &BlockCoord,
        from_face: Option<FaceDirection>,
        to: &BlockCoord,
        to_face: Option<FaceDirection>,
        max_mb: u64,
    ) -> u64 {
        self.transfer_fluid_impl(from, from_face, to, to_face, None, max_mb)
            .delivered
    }

    /// [`transfer_fluid_between`](Self::transfer_fluid_between) with
    /// exact conservation accounting: `delivered +
    /// returned_to_source + overflow.amount_mb` is always the amount
    /// drained from the source. A rollback the source refuses is
    /// surfaced as `overflow`, never counted as delivered or deleted.
    pub fn transfer_fluid_checked(
        &mut self,
        from: &BlockCoord,
        from_face: Option<FaceDirection>,
        to: &BlockCoord,
        to_face: Option<FaceDirection>,
        max_mb: u64,
    ) -> FluidTransferResult {
        self.transfer_fluid_impl(from, from_face, to, to_face, None, max_mb)
    }

    /// Face-agnostic [`transfer_fluid_between`](Self::transfer_fluid_between)
    /// restricted to a single fluid id: only `fluid_filter` (when
    /// `Some`) may move, and a rejecting or full destination leaks zero
    /// mB. Internal (`None` face) access on both endpoints.
    pub fn transfer_fluid_between_filtered(
        &mut self,
        from: &BlockCoord,
        to: &BlockCoord,
        fluid_filter: Option<&str>,
        max_mb: u64,
    ) -> u64 {
        self.transfer_fluid_impl(from, None, to, None, fluid_filter, max_mb)
            .delivered
    }

    /// Shared simulate-then-commit fluid move. `fluid_filter`
    /// constrains the probe drain; the committed drain then pins the
    /// *probed* fluid id so the destination can never receive a fluid
    /// it did not simulate. The result conserves exactly:
    /// `delivered + returned_to_source + overflow.amount_mb ==
    /// taken.amount_mb` — mB the source refuses on rollback are reported
    /// as `overflow`, not silently absorbed into `delivered`.
    fn transfer_fluid_impl(
        &mut self,
        from: &BlockCoord,
        from_face: Option<FaceDirection>,
        to: &BlockCoord,
        to_face: Option<FaceDirection>,
        fluid_filter: Option<&str>,
        max_mb: u64,
    ) -> FluidTransferResult {
        let empty = FluidTransferResult::default;
        if from == to || max_mb == 0 {
            return empty();
        }
        let Some(src) = self.fluids.get_mut(from) else {
            return empty();
        };
        let Some(probe) = src.drain_fluid(from_face, max_mb, fluid_filter, true) else {
            return empty();
        };
        if probe.is_empty() {
            return empty();
        }
        let Some(dst) = self.fluids.get_mut(to) else {
            return empty();
        };
        let accepted = dst.fill_fluid(to_face, &probe, true).min(probe.amount_mb);
        if accepted == 0 {
            return empty();
        }
        let probe_id = probe.fluid_id.clone();
        let Some(taken) = self
            .fluids
            .get_mut(from)
            .and_then(|h| h.drain_fluid(from_face, accepted, Some(&probe_id), false))
        else {
            return empty();
        };
        let mut result = FluidTransferResult::default();
        let mut pending = taken;
        if let Some(dst) = self.fluids.get_mut(to) {
            let filled = dst.fill_fluid(to_face, &pending, false);
            result.delivered = filled;
            pending.amount_mb = pending.amount_mb.saturating_sub(filled);
        }
        if pending.amount_mb > 0 {
            // Roll the undelivered remainder back into the source; any
            // mB the source refuses are overflow — surfaced, not
            // silently counted as delivered.
            if let Some(src) = self.fluids.get_mut(from) {
                let back = src.fill_fluid(from_face, &pending, false);
                result.returned_to_source = back;
                pending.amount_mb = pending.amount_mb.saturating_sub(back);
            }
            if pending.amount_mb > 0 {
                result.overflow = Some(pending);
            }
        }
        result
    }

    /// Atomically move up to `max_fe` of energy from `from` to `to` via
    /// simulate-then-commit; any FE the committed receive refuses is
    /// pushed back into the source. Returns FE delivered — exactly
    /// [`EnergyTransferResult::delivered`] of
    /// [`transfer_energy_checked`](Self::transfer_energy_checked).
    pub fn transfer_energy_between(
        &mut self,
        from: &BlockCoord,
        from_face: Option<FaceDirection>,
        to: &BlockCoord,
        to_face: Option<FaceDirection>,
        max_fe: u64,
    ) -> u64 {
        self.transfer_energy_checked(from, from_face, to, to_face, max_fe)
            .delivered
    }

    /// [`transfer_energy_between`](Self::transfer_energy_between) with
    /// exact conservation accounting: `delivered + returned_to_source +
    /// overflow_fe` is always the amount drawn from the source.
    pub fn transfer_energy_checked(
        &mut self,
        from: &BlockCoord,
        from_face: Option<FaceDirection>,
        to: &BlockCoord,
        to_face: Option<FaceDirection>,
        max_fe: u64,
    ) -> EnergyTransferResult {
        let empty = EnergyTransferResult::default;
        if from == to || max_fe == 0 {
            return empty();
        }
        let Some(src) = self.energy.get_mut(from) else {
            return empty();
        };
        let available = src.extract_energy(from_face, max_fe, true);
        if available == 0 {
            return empty();
        }
        let Some(dst) = self.energy.get_mut(to) else {
            return empty();
        };
        let wanted = dst.receive_energy(to_face, available, true).min(available);
        if wanted == 0 {
            return empty();
        }
        let drawn = self
            .energy
            .get_mut(from)
            .map_or(0, |h| h.extract_energy(from_face, wanted, false));
        let mut result = EnergyTransferResult::default();
        if drawn > 0 {
            let accepted = self
                .energy
                .get_mut(to)
                .map_or(0, |h| h.receive_energy(to_face, drawn, false));
            result.delivered = accepted;
            let bounced = drawn.saturating_sub(accepted);
            if bounced > 0 {
                // Roll refused FE back into the source; whatever the
                // source refuses is overflow — surfaced, never deleted.
                let recovered = self
                    .energy
                    .get_mut(from)
                    .map_or(0, |h| h.receive_energy(from_face, bounced, false));
                result.returned_to_source = recovered;
                result.overflow_fe = bounced.saturating_sub(recovered);
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Built-in generic endpoints
// ---------------------------------------------------------------------------

/// `true` when `face` is permitted: a `None` allow-set accepts
/// everything; `Some(set)` accepts exactly the faces listed (use
/// `Some(FaceDirection)` members inside the set for world faces and a
/// bare `None` member for internal access).
fn face_allowed(
    set: &Option<BTreeSet<Option<FaceDirection>>>,
    face: Option<FaceDirection>,
) -> bool {
    match set {
        None => true,
        Some(allowed) => allowed.contains(&face),
    }
}

/// Generic N-slot inventory endpoint — vanilla chests/hoppers/furnaces
/// and any data-driven mod block register this shape. Insert merges into
/// existing identical stacks first (id + components), then fills empty
/// slots; per-face insert/extract allow-sets are optional.
#[derive(Clone, Debug)]
pub struct SimpleSlotInventory {
    slots: Vec<Option<CapabilityItemStack>>,
    max_stack: u32,
    insert_faces: Option<BTreeSet<Option<FaceDirection>>>,
    extract_faces: Option<BTreeSet<Option<FaceDirection>>>,
}

impl SimpleSlotInventory {
    /// `slots` empty slots, each holding up to `max_stack` items.
    pub fn new(slots: usize, max_stack: u32) -> Self {
        Self {
            slots: vec![None; slots],
            max_stack: max_stack.max(1),
            insert_faces: None,
            extract_faces: None,
        }
    }

    /// Restrict insertion to exactly these faces (`None` member inside
    /// the set permits internal access).
    pub fn with_insert_faces(
        mut self,
        faces: impl IntoIterator<Item = Option<FaceDirection>>,
    ) -> Self {
        self.insert_faces = Some(faces.into_iter().collect());
        self
    }

    /// Restrict extraction to exactly these faces.
    pub fn with_extract_faces(
        mut self,
        faces: impl IntoIterator<Item = Option<FaceDirection>>,
    ) -> Self {
        self.extract_faces = Some(faces.into_iter().collect());
        self
    }

    /// Direct slot access (world setup, tests, adapters).
    pub fn slot(&self, index: usize) -> Option<&CapabilityItemStack> {
        self.slots.get(index).and_then(Option::as_ref)
    }

    /// Direct slot write (world setup, tests, adapters).
    pub fn set_slot(&mut self, index: usize, stack: Option<CapabilityItemStack>) {
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = stack;
        }
    }

    /// Total item count across all slots.
    pub fn total_items(&self) -> u64 {
        self.slots
            .iter()
            .filter_map(Option::as_ref)
            .map(|s| u64::from(s.count))
            .sum()
    }

    fn insert_impl(
        slots: &mut [Option<CapabilityItemStack>],
        max_stack: u32,
        stack: &CapabilityItemStack,
    ) -> CapabilityItemStack {
        let mut remainder = stack.clone();
        // Merge pass: identical id + components only.
        for slot in slots.iter_mut().flatten() {
            if remainder.is_empty() {
                return remainder;
            }
            if slot.can_merge(&remainder) && slot.count < max_stack {
                let room = max_stack - slot.count;
                let moved = room.min(remainder.count);
                slot.count += moved;
                remainder.count -= moved;
            }
        }
        // Fill pass: first empty slots.
        for slot in slots.iter_mut() {
            if remainder.is_empty() {
                break;
            }
            if slot.is_none() {
                let moved = max_stack.min(remainder.count);
                *slot = Some(remainder.take(moved));
            }
        }
        remainder
    }
}

impl ItemTransferHandler for SimpleSlotInventory {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn insert_item(
        &mut self,
        face: Option<FaceDirection>,
        stack: &CapabilityItemStack,
        simulate: bool,
    ) -> CapabilityItemStack {
        if !face_allowed(&self.insert_faces, face) || stack.is_empty() {
            return stack.clone();
        }
        if simulate {
            let mut copy = self.slots.clone();
            Self::insert_impl(&mut copy, self.max_stack, stack)
        } else {
            Self::insert_impl(&mut self.slots, self.max_stack, stack)
        }
    }

    fn extract_item(
        &mut self,
        face: Option<FaceDirection>,
        max_count: u32,
        filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
        simulate: bool,
    ) -> Option<CapabilityItemStack> {
        if !face_allowed(&self.extract_faces, face) || max_count == 0 {
            return None;
        }
        let index = self.slots.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|s| !s.is_empty() && filter.is_none_or(|f| f(s)))
        })?;
        if simulate {
            let mut probe = self.slots[index].clone()?;
            Some(probe.take(max_count))
        } else {
            let slot = self.slots[index].as_mut()?;
            let out = slot.take(max_count);
            if slot.count == 0 {
                self.slots[index] = None;
            }
            Some(out)
        }
    }

    fn slots(&self) -> usize {
        self.slots.len()
    }
}

/// Generic single-fluid tank endpoint. Fill accepts only the stored
/// fluid (id + components) or any fluid while empty.
#[derive(Clone, Debug)]
pub struct SimpleFluidTank {
    fluid: Option<CapabilityFluidStack>,
    capacity_mb: u64,
}

impl SimpleFluidTank {
    /// Empty tank holding up to `capacity_mb` milli-buckets.
    pub fn new(capacity_mb: u64) -> Self {
        Self {
            fluid: None,
            capacity_mb,
        }
    }

    /// Currently stored fluid, if any.
    pub fn fluid(&self) -> Option<&CapabilityFluidStack> {
        self.fluid.as_ref()
    }

    /// Direct write (world setup, tests).
    pub fn set_fluid(&mut self, fluid: Option<CapabilityFluidStack>) {
        self.fluid = fluid.map(|mut f| {
            f.amount_mb = f.amount_mb.min(self.capacity_mb);
            f
        });
    }

    fn fill_impl(
        fluid: &mut Option<CapabilityFluidStack>,
        capacity_mb: u64,
        resource: &CapabilityFluidStack,
    ) -> u64 {
        match fluid {
            Some(stored) => {
                if !stored.can_merge(resource) {
                    return 0;
                }
                let room = capacity_mb.saturating_sub(stored.amount_mb);
                let accepted = room.min(resource.amount_mb);
                stored.amount_mb += accepted;
                accepted
            }
            slot @ None => {
                let accepted = capacity_mb.min(resource.amount_mb);
                if accepted > 0 {
                    *slot = Some(CapabilityFluidStack {
                        fluid_id: resource.fluid_id.clone(),
                        amount_mb: accepted,
                        raw_components: resource.raw_components.clone(),
                    });
                }
                accepted
            }
        }
    }
}

impl FluidTransferHandler for SimpleFluidTank {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn fill_fluid(
        &mut self,
        _face: Option<FaceDirection>,
        resource: &CapabilityFluidStack,
        simulate: bool,
    ) -> u64 {
        if resource.is_empty() {
            return 0;
        }
        if simulate {
            let mut copy = self.fluid.clone();
            Self::fill_impl(&mut copy, self.capacity_mb, resource)
        } else {
            Self::fill_impl(&mut self.fluid, self.capacity_mb, resource)
        }
    }

    fn drain_fluid(
        &mut self,
        _face: Option<FaceDirection>,
        max_mb: u64,
        filter_fluid: Option<&str>,
        simulate: bool,
    ) -> Option<CapabilityFluidStack> {
        if max_mb == 0 {
            return None;
        }
        let stored = self.fluid.as_ref()?;
        if filter_fluid.is_some_and(|id| id != stored.fluid_id) {
            return None;
        }
        let amount = max_mb.min(stored.amount_mb);
        if simulate {
            Some(CapabilityFluidStack {
                fluid_id: stored.fluid_id.clone(),
                amount_mb: amount,
                raw_components: stored.raw_components.clone(),
            })
        } else {
            let stored = self.fluid.as_mut()?;
            stored.amount_mb -= amount;
            let out = CapabilityFluidStack {
                fluid_id: stored.fluid_id.clone(),
                amount_mb: amount,
                raw_components: stored.raw_components.clone(),
            };
            if stored.amount_mb == 0 {
                self.fluid = None;
            }
            Some(out)
        }
    }

    fn fluid_tank_count(&self) -> usize {
        1
    }

    fn peek_fluid(&self, tank: usize) -> Option<CapabilityFluidStack> {
        if tank == 0 {
            self.fluid.clone()
        } else {
            None
        }
    }
}

/// Generic FE buffer endpoint with per-direction rate limits.
#[derive(Clone, Debug)]
pub struct SimpleEnergyBuffer {
    stored_fe: u64,
    capacity_fe: u64,
    max_receive_fe: u64,
    max_extract_fe: u64,
}

impl SimpleEnergyBuffer {
    /// Buffer of `capacity_fe`, unlimited per-call in/out rates.
    pub fn new(capacity_fe: u64) -> Self {
        Self {
            stored_fe: 0,
            capacity_fe,
            max_receive_fe: u64::MAX,
            max_extract_fe: u64::MAX,
        }
    }

    /// Per-call rate limits (like NeoForge `maxReceive`/`maxExtract`).
    pub fn with_limits(mut self, max_receive_fe: u64, max_extract_fe: u64) -> Self {
        self.max_receive_fe = max_receive_fe;
        self.max_extract_fe = max_extract_fe;
        self
    }

    /// Direct write (world setup, tests); clamped to capacity.
    pub fn set_stored(&mut self, fe: u64) {
        self.stored_fe = fe.min(self.capacity_fe);
    }
}

impl EnergyTransferHandler for SimpleEnergyBuffer {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn receive_energy(
        &mut self,
        _face: Option<FaceDirection>,
        max_receive_fe: u64,
        simulate: bool,
    ) -> u64 {
        let room = self.capacity_fe.saturating_sub(self.stored_fe);
        let accepted = room.min(self.max_receive_fe).min(max_receive_fe);
        if !simulate {
            self.stored_fe += accepted;
        }
        accepted
    }

    fn extract_energy(
        &mut self,
        _face: Option<FaceDirection>,
        max_extract_fe: u64,
        simulate: bool,
    ) -> u64 {
        let drawn = self.stored_fe.min(self.max_extract_fe).min(max_extract_fe);
        if !simulate {
            self.stored_fe -= drawn;
        }
        drawn
    }

    fn energy_stored(&self) -> u64 {
        self.stored_fe
    }

    fn max_energy_stored(&self) -> u64 {
        self.capacity_fe
    }
}

/// [`SimpleEnergyBuffer`] under its generic storage name — the ready-made
/// reusable FE endpoint for tests and mod machines.
pub type SimpleEnergyStorage = SimpleEnergyBuffer;

// ---------------------------------------------------------------------------
// Living cross-mod proof: culinary cooking pot as an ItemTransferHandler
// ---------------------------------------------------------------------------

/// [`ItemTransferHandler`] over a [`CookingPotState`]: world faces
/// (`Up`/`North`/`South`/`West`/`East`) insert ingredients into the six
/// input slots, `Down` inserts into the container slot and extracts the
/// finished meal from the output slot, and `None` (internal access) may
/// insert inputs or extract output. Component bytes are tracked
/// per-input-slot so what a funnel pushed in keeps its identity.
#[derive(Clone, Debug)]
pub struct CookingPotItemAdapter {
    pot: CookingPotState,
    input_components: [Vec<u8>; crate::culinary_adapter::POT_INPUT_SLOTS],
    input_ids: [Option<String>; crate::culinary_adapter::POT_INPUT_SLOTS],
    container_components: Vec<u8>,
    container_id: Option<String>,
    output_components: Vec<u8>,
    output_id: Option<String>,
}

impl Default for CookingPotItemAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CookingPotItemAdapter {
    /// Adapter over an empty pot.
    pub fn new() -> Self {
        Self {
            pot: CookingPotState::new(),
            input_components: std::array::from_fn(|_| Vec::new()),
            input_ids: std::array::from_fn(|_| None),
            container_components: Vec::new(),
            container_id: None,
            output_components: Vec::new(),
            output_id: None,
        }
    }

    /// Borrow the underlying pot (recipe/heat wiring lives outside the
    /// capability face).
    pub fn pot(&self) -> &CookingPotState {
        &self.pot
    }

    /// Mutably borrow the pot (heat/container wiring).
    pub fn pot_mut(&mut self) -> &mut CookingPotState {
        &mut self.pot
    }

    /// Advance the pot's sleeping-timer craft and keep the component
    /// shadow table consistent (consumed input slots drop their tracked
    /// components; a newly produced meal starts with empty components).
    pub fn advance_to(
        &mut self,
        now: u64,
        registry: &CulinaryRegistry,
    ) -> Option<crate::culinary_adapter::CookResult> {
        let result = self.pot.advance_to(now, registry);
        self.sync_components();
        result
    }

    /// Reconcile the shadow component tables with the pot's public slot
    /// state — clears components for slots whose contents changed
    /// identity (craft consumption, external `pot_mut` writes).
    fn sync_components(&mut self) {
        for (i, slot) in self.pot.inputs.iter().enumerate() {
            let id = slot.as_ref().map(|(id, _)| id.clone());
            if self.input_ids[i] != id {
                self.input_ids[i] = id;
                self.input_components[i] = Vec::new();
            }
        }
        let container_id = self.pot.container.as_ref().map(|(id, _)| id.clone());
        if self.container_id != container_id {
            self.container_id = container_id;
            self.container_components = Vec::new();
        }
        let output_id = self.pot.output.as_ref().map(|(id, _)| id.clone());
        if self.output_id != output_id {
            self.output_id = output_id;
            self.output_components = Vec::new();
        }
    }
}

impl ItemTransferHandler for CookingPotItemAdapter {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn insert_item(
        &mut self,
        face: Option<FaceDirection>,
        stack: &CapabilityItemStack,
        simulate: bool,
    ) -> CapabilityItemStack {
        if stack.is_empty() {
            return stack.clone();
        }
        if simulate {
            let mut copy = self.clone();
            return copy.insert_item(face, stack, false);
        }
        self.sync_components();
        let mut remainder = stack.clone();
        let item_id = crate::culinary_adapter::normalize_id(&stack.item_id);
        if face == Some(FaceDirection::Down) {
            // Bottom face feeds the container slot only.
            if self.pot.container.is_none() {
                let count = remainder.count.min(u32::from(u16::MAX)) as u16;
                self.pot.container = Some((item_id.clone(), count));
                self.container_id = Some(item_id);
                self.container_components = stack.raw_components.clone();
                remainder.count -= u32::from(count);
            }
            return remainder;
        }
        // Top/side/internal faces feed ingredient input slots.
        for i in 0..self.pot.inputs.len() {
            if remainder.is_empty() {
                break;
            }
            if self.pot.inputs[i].is_none() {
                let count = remainder.count.min(u32::from(u16::MAX)) as u16;
                self.pot.inputs[i] = Some((item_id.clone(), count));
                self.input_ids[i] = Some(item_id.clone());
                self.input_components[i] = stack.raw_components.clone();
                remainder.count -= u32::from(count);
            }
        }
        remainder
    }

    fn extract_item(
        &mut self,
        face: Option<FaceDirection>,
        max_count: u32,
        filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
        simulate: bool,
    ) -> Option<CapabilityItemStack> {
        // Only the bottom face (or internal access) serves the output.
        if !matches!(face, None | Some(FaceDirection::Down)) || max_count == 0 {
            return None;
        }
        self.sync_components();
        let (id, count) = self.pot.output.as_ref()?;
        let probe = CapabilityItemStack {
            item_id: id.clone(),
            count: u32::from(*count).min(max_count),
            raw_components: self.output_components.clone(),
        };
        if !filter.is_none_or(|f| f(&probe)) {
            return None;
        }
        if simulate {
            return Some(probe);
        }
        let moved = (*count).min(max_count as u16);
        let remaining = count.saturating_sub(moved);
        if remaining == 0 {
            self.pot.output = None;
            self.output_id = None;
            self.output_components = Vec::new();
        } else {
            self.pot.output = Some((id.clone(), remaining));
        }
        Some(probe)
    }

    fn slots(&self) -> usize {
        crate::culinary_adapter::POT_INPUT_SLOTS + 2 // inputs + container + output
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn coord(dim: &str, x: i32) -> BlockCoord {
        BlockCoord::new(dim, x, 64, 0)
    }

    fn stored_fluid(bus: &BlockCapabilityBus, at: &BlockCoord) -> Option<CapabilityFluidStack> {
        bus.fluid_endpoint(at)
            .and_then(|h| h.as_any().downcast_ref::<SimpleFluidTank>())
            .and_then(|t| t.fluid().cloned())
    }

    fn stored_fe(bus: &BlockCapabilityBus, at: &BlockCoord) -> Option<u64> {
        bus.energy_endpoint(at).map(|h| h.energy_stored())
    }

    #[test]
    fn filtered_fluid_transfer_rolls_back_and_never_leaks() {
        let mut bus = BlockCapabilityBus::new();
        let src_at = coord("mod_a:dim", 0);
        let dst_at = coord("mod_b:dim", 1);
        let missing_at = coord("mod_b:dim", 9);

        let mut src = SimpleFluidTank::new(4000);
        src.set_fluid(Some(CapabilityFluidStack::new("mod_a:steam", 2000)));
        bus.register_fluid_handler(src_at.clone(), Box::new(src));
        bus.register_fluid_handler(dst_at.clone(), Box::new(SimpleFluidTank::new(500)));

        // Filter mismatch: destination never sees the probe.
        assert_eq!(
            bus.transfer_fluid_between_filtered(&src_at, &dst_at, Some("mod_a:lava"), 2000),
            0
        );
        // Matching filter, destination caps at 500 mB.
        assert_eq!(
            bus.transfer_fluid_between_filtered(&src_at, &dst_at, Some("mod_a:steam"), 2000),
            500
        );
        assert_eq!(stored_fluid(&bus, &src_at).map(|f| f.amount_mb), Some(1500));
        assert_eq!(
            stored_fluid(&bus, &dst_at),
            Some(CapabilityFluidStack::new("mod_a:steam", 500))
        );

        // Destination now full: zero draw, zero leak.
        assert_eq!(
            bus.transfer_fluid_between_filtered(&src_at, &dst_at, Some("mod_a:steam"), 1000),
            0
        );
        assert_eq!(stored_fluid(&bus, &src_at).map(|f| f.amount_mb), Some(1500));

        // Incompatible fluid in the destination rejects everything.
        let mut dst = SimpleFluidTank::new(1000);
        dst.set_fluid(Some(CapabilityFluidStack::new("mod_b:oil", 100)));
        let oil_at = coord("mod_b:dim", 2);
        bus.register_fluid_handler(oil_at.clone(), Box::new(dst));
        assert_eq!(
            bus.transfer_fluid_between_filtered(&src_at, &oil_at, Some("mod_a:steam"), 1000),
            0,
            "destination holding a different fluid rejects the fill"
        );
        assert_eq!(
            stored_fluid(&bus, &src_at).map(|f| f.amount_mb),
            Some(1500),
            "rejected commit rolls back into the source whole"
        );
        assert_eq!(
            stored_fluid(&bus, &oil_at).map(|f| f.amount_mb),
            Some(100),
            "destination untouched"
        );

        // Missing endpoints and self-transfers move nothing.
        assert_eq!(
            bus.transfer_fluid_between_filtered(&src_at, &missing_at, None, 100),
            0
        );
        assert_eq!(
            bus.transfer_fluid_between_filtered(&src_at, &src_at, None, 100),
            0
        );
        assert_eq!(stored_fluid(&bus, &src_at).map(|f| f.amount_mb), Some(1500));
    }

    #[test]
    fn faceless_fluid_convenience_and_peek() {
        let mut tank = SimpleFluidTank::new(1000);
        assert_eq!(tank.fluid_tank_count(), 1);
        assert!(tank.peek_fluid(0).is_none());
        assert!(tank.peek_fluid(1).is_none(), "out-of-range tank");

        // Simulated insert must not mutate.
        assert_eq!(
            tank.insert_fluid(CapabilityFluidStack::new("mod_a:biofuel", 2000), true),
            1000
        );
        assert!(tank.peek_fluid(0).is_none(), "simulate left the tank empty");

        assert_eq!(
            tank.insert_fluid(CapabilityFluidStack::new("mod_a:biofuel", 700), false),
            700
        );
        assert_eq!(
            tank.peek_fluid(0),
            Some(CapabilityFluidStack::new("mod_a:biofuel", 700))
        );

        // Filtered extract honours the fluid id; simulate does not mutate.
        assert!(tank
            .extract_fluid(Some("mod_b:other"), 500, true)
            .is_none());
        assert_eq!(
            tank.extract_fluid(Some("mod_a:biofuel"), 500, true),
            Some(CapabilityFluidStack::new("mod_a:biofuel", 500))
        );
        assert_eq!(
            tank.extract_fluid(Some("mod_a:biofuel"), 500, false),
            Some(CapabilityFluidStack::new("mod_a:biofuel", 500))
        );
        assert_eq!(
            tank.peek_fluid(0),
            Some(CapabilityFluidStack::new("mod_a:biofuel", 200))
        );
    }

    #[test]
    fn energy_units_convert_saturating_and_exact() {
        let rates = EnergyConversionRates::default();
        assert_eq!(EnergyAmount::new(EnergyUnit::Rf, 7).to_fe(&rates), 7);
        assert_eq!(EnergyAmount::new(EnergyUnit::Eu, 7).to_fe(&rates), 28);
        assert_eq!(EnergyAmount::new(EnergyUnit::StressSu, 7).to_fe(&rates), 14);
        assert_eq!(EnergyAmount::new(EnergyUnit::Fe, 7).to_fe(&rates), 7);

        // Saturating: no overflow, no panic.
        assert_eq!(
            EnergyAmount::new(EnergyUnit::Eu, u64::MAX).to_fe(&rates),
            u64::MAX
        );

        // Floor division vs exact conversion.
        let three_eu = EnergyAmount::new(EnergyUnit::Eu, 3); // 12 FE
        assert_eq!(three_eu.convert(EnergyUnit::StressSu, &rates).amount, 6);
        let five_fe = EnergyAmount::new(EnergyUnit::Fe, 5); // 5 FE -> 1 EU (+1 remainder)
        assert_eq!(five_fe.convert(EnergyUnit::Eu, &rates).amount, 1);
        assert!(five_fe.convert_exact(EnergyUnit::Eu, &rates).is_none());
        assert_eq!(
            EnergyAmount::new(EnergyUnit::Fe, 8)
                .convert_exact(EnergyUnit::Eu, &rates),
            Some(EnergyAmount::new(EnergyUnit::Eu, 2))
        );

        // A zero-configured rate degrades to 1:1 rather than panicking.
        let zero = EnergyConversionRates {
            fe_per_rf: 1,
            fe_per_eu: 0,
            fe_per_stress_su: 0,
        };
        assert_eq!(zero.from_fe(EnergyUnit::Eu, 9), 9);
        assert_eq!(zero.to_fe(EnergyUnit::StressSu, 9), 9);
    }

    #[test]
    fn three_mod_energy_chain_conserves_fe() {
        let mut bus = BlockCapabilityBus::new();
        let gen_at = coord("mod_a:dim", 0); // generator
        let battery_at = coord("mod_b:dim", 1); // battery
        let consumer_at = coord("mod_c:dim", 2); // consumer

        // Mod A generator produces EU; the bus stores canonical FE.
        let rates = EnergyConversionRates::default();
        let generated = EnergyAmount::new(EnergyUnit::Eu, 2_500);
        let mut gen = SimpleEnergyBuffer::new(100_000);
        gen.set_stored(generated.to_fe(&rates)); // 10_000 FE
        bus.register_energy_handler(gen_at.clone(), Box::new(gen));
        bus.register_energy_handler(
            battery_at.clone(),
            Box::new(SimpleEnergyBuffer::new(8_000)),
        );
        bus.register_energy_handler(
            consumer_at.clone(),
            Box::new(SimpleEnergyStorage::new(4_000)),
        );

        let before: u64 = [&gen_at, &battery_at, &consumer_at]
            .iter()
            .filter_map(|at| stored_fe(&bus, at))
            .sum();
        assert_eq!(before, 10_000);

        // Generator -> battery: clamped by the battery's 8k capacity.
        assert_eq!(
            bus.transfer_energy_between(&gen_at, None, &battery_at, None, 10_000),
            8_000
        );
        // Battery -> consumer: clamped by the consumer's 4k capacity.
        assert_eq!(
            bus.transfer_energy_between(&battery_at, None, &consumer_at, None, 8_000),
            4_000
        );

        // Strict conservation: 2000 gen + 4000 battery + 4000 consumer.
        assert_eq!(stored_fe(&bus, &gen_at), Some(2_000));
        assert_eq!(stored_fe(&bus, &battery_at), Some(4_000));
        assert_eq!(stored_fe(&bus, &consumer_at), Some(4_000));
        let after: u64 = [&gen_at, &battery_at, &consumer_at]
            .iter()
            .filter_map(|at| stored_fe(&bus, at))
            .sum();
        assert_eq!(after, before, "zero FE created or destroyed");

        // Face-agnostic convenience API mutates only on commit.
        {
            let consumer = bus
                .energy_endpoint_mut(&consumer_at)
                .expect("consumer registered");
            assert_eq!(consumer.capacity_fe(), 4_000);
            assert_eq!(consumer.extract_fe(1_000, true), 1_000);
            assert_eq!(consumer.stored_fe(), 4_000, "simulate did not drain");
            assert_eq!(consumer.extract_fe(1_000, false), 1_000);
            assert_eq!(consumer.stored_fe(), 3_000);
            assert_eq!(consumer.receive_fe(500, false), 500);
            assert_eq!(consumer.stored_fe(), 3_500);
        }
        // One conservation-still-holds check after the convenience calls:
        // extracted 1000, received 500 -> net -500.
        let final_total: u64 = [&gen_at, &battery_at, &consumer_at]
            .iter()
            .filter_map(|at| stored_fe(&bus, at))
            .sum();
        assert_eq!(final_total, 9_500);
    }

    // -- Divergent test endpoints -------------------------------------
    // Well-behaved endpoints can never reach the rollback paths (their
    // commit always matches their simulation), so these stubs accept at
    // simulate and refuse at commit — or refuse whole directions — to
    // prove the bus's accounting stays exact under NeoForge-legal
    // misbehaviour.

    /// Item destination that simulates inserts honestly but refuses
    /// every committed insert.
    struct RejectOnCommitItems {
        inner: SimpleSlotInventory,
    }

    impl ItemTransferHandler for RejectOnCommitItems {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn insert_item(
            &mut self,
            face: Option<FaceDirection>,
            stack: &CapabilityItemStack,
            simulate: bool,
        ) -> CapabilityItemStack {
            if simulate {
                self.inner.insert_item(face, stack, true)
            } else {
                stack.clone()
            }
        }
        fn extract_item(
            &mut self,
            _face: Option<FaceDirection>,
            _max_count: u32,
            _filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
            _simulate: bool,
        ) -> Option<CapabilityItemStack> {
            None
        }
        fn slots(&self) -> usize {
            self.inner.slots()
        }
    }

    /// Fluid source that drains honestly but refuses every fill — the
    /// rollback-refusing source that forces the overflow path.
    struct DrainOnlyFluid {
        inner: SimpleFluidTank,
    }

    impl FluidTransferHandler for DrainOnlyFluid {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn fill_fluid(
            &mut self,
            _face: Option<FaceDirection>,
            _resource: &CapabilityFluidStack,
            _simulate: bool,
        ) -> u64 {
            0
        }
        fn drain_fluid(
            &mut self,
            face: Option<FaceDirection>,
            max_mb: u64,
            filter_fluid: Option<&str>,
            simulate: bool,
        ) -> Option<CapabilityFluidStack> {
            self.inner.drain_fluid(face, max_mb, filter_fluid, simulate)
        }
        fn peek_fluid(&self, tank: usize) -> Option<CapabilityFluidStack> {
            self.inner.peek_fluid(tank)
        }
    }

    /// Fluid destination that simulates fills honestly but refuses every
    /// committed fill.
    struct RejectOnCommitFluid {
        inner: SimpleFluidTank,
    }

    impl FluidTransferHandler for RejectOnCommitFluid {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn fill_fluid(
            &mut self,
            face: Option<FaceDirection>,
            resource: &CapabilityFluidStack,
            simulate: bool,
        ) -> u64 {
            if simulate {
                self.inner.fill_fluid(face, resource, true)
            } else {
                0
            }
        }
        fn drain_fluid(
            &mut self,
            _face: Option<FaceDirection>,
            _max_mb: u64,
            _filter_fluid: Option<&str>,
            _simulate: bool,
        ) -> Option<CapabilityFluidStack> {
            None
        }
    }

    /// Energy destination that simulates receives honestly but refuses
    /// every committed receive.
    struct RejectOnCommitEnergy {
        inner: SimpleEnergyBuffer,
    }

    impl EnergyTransferHandler for RejectOnCommitEnergy {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn receive_energy(
            &mut self,
            face: Option<FaceDirection>,
            max_receive_fe: u64,
            simulate: bool,
        ) -> u64 {
            if simulate {
                self.inner.receive_energy(face, max_receive_fe, true)
            } else {
                0
            }
        }
        fn extract_energy(
            &mut self,
            _face: Option<FaceDirection>,
            _max_extract_fe: u64,
            _simulate: bool,
        ) -> u64 {
            0
        }
        fn energy_stored(&self) -> u64 {
            self.inner.energy_stored()
        }
        fn max_energy_stored(&self) -> u64 {
            self.inner.max_energy_stored()
        }
    }

    /// Energy source that extracts honestly but refuses every receive —
    /// the rollback-refusing source that forces `overflow_fe`.
    struct ExtractOnlyEnergy {
        inner: SimpleEnergyBuffer,
    }

    impl EnergyTransferHandler for ExtractOnlyEnergy {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn receive_energy(
            &mut self,
            _face: Option<FaceDirection>,
            _max_receive_fe: u64,
            _simulate: bool,
        ) -> u64 {
            0
        }
        fn extract_energy(
            &mut self,
            face: Option<FaceDirection>,
            max_extract_fe: u64,
            simulate: bool,
        ) -> u64 {
            self.inner.extract_energy(face, max_extract_fe, simulate)
        }
        fn energy_stored(&self) -> u64 {
            self.inner.energy_stored()
        }
        fn max_energy_stored(&self) -> u64 {
            self.inner.max_energy_stored()
        }
    }

    fn stored_items(bus: &BlockCapabilityBus, at: &BlockCoord) -> Option<u64> {
        bus.item_endpoint(at)
            .and_then(|h| h.as_any().downcast_ref::<SimpleSlotInventory>())
            .map(|inv| inv.total_items())
    }

    #[test]
    fn item_transfer_checked_counts_delivered_exactly_on_partial() {
        let mut bus = BlockCapabilityBus::new();
        let src_at = coord("mod_a:dim", 10);
        let dst_at = coord("mod_a:dim", 11);
        let mut src = SimpleSlotInventory::new(1, 64);
        src.set_slot(0, Some(CapabilityItemStack::new("minecraft:cobblestone", 10)));
        let mut dst = SimpleSlotInventory::new(1, 64);
        dst.set_slot(0, Some(CapabilityItemStack::new("minecraft:cobblestone", 58)));
        bus.register_item_handler(src_at.clone(), Box::new(src));
        bus.register_item_handler(dst_at.clone(), Box::new(dst));

        let result = bus.transfer_item_checked(&src_at, None, &dst_at, None, 10, None);
        assert_eq!(result.delivered, 6, "exactly the destination's room");
        assert_eq!(result.returned_to_source, 0);
        assert!(result.overflow.is_none());
        assert_eq!(result.accounted(), 6, "every committed item accounted");
        assert_eq!(stored_items(&bus, &src_at), Some(4));
        assert_eq!(stored_items(&bus, &dst_at), Some(64));

        // A now-full destination refuses: nothing extracted, nothing lost.
        let refused = bus.transfer_item_checked(&src_at, None, &dst_at, None, 4, None);
        assert_eq!(refused, ItemTransferResult::default());
        assert_eq!(stored_items(&bus, &src_at), Some(4));
    }

    #[test]
    fn commit_refusal_rolls_back_and_never_overreports() {
        // Regression: `insert_item` returns the *unaccepted remainder*,
        // so `delivered = taken − leftover` — never `taken − reinserted`.
        // A destination refusing at commit must report `delivered == 0`.
        let mut bus = BlockCapabilityBus::new();
        let src_at = coord("mod_a:dim", 12);
        let dst_at = coord("mod_a:dim", 13);
        let mut src = SimpleSlotInventory::new(1, 64);
        src.set_slot(0, Some(CapabilityItemStack::new("minecraft:cobblestone", 10)));
        bus.register_item_handler(src_at.clone(), Box::new(src));
        bus.register_item_handler(
            dst_at.clone(),
            Box::new(RejectOnCommitItems {
                inner: SimpleSlotInventory::new(1, 64),
            }),
        );

        let result = bus.transfer_item_checked(&src_at, None, &dst_at, None, 10, None);
        assert_eq!(result.delivered, 0, "refused commit delivers nothing");
        assert_eq!(result.returned_to_source, 10, "whole stack rolled back");
        assert!(result.overflow.is_none());
        assert_eq!(result.accounted(), 10);
        assert_eq!(stored_items(&bus, &src_at), Some(10), "source restored");
        // The convenience API reports the same exact delivered count.
        assert_eq!(
            bus.transfer_item_between(&src_at, None, &dst_at, None, 10, None),
            0
        );
        assert_eq!(stored_items(&bus, &src_at), Some(10));
    }

    #[test]
    fn refused_rollback_surfaces_overflow_instead_of_vanishing() {
        // Source extracts through `Up` but refuses inserts through `Up`;
        // the destination refuses at commit — the 10 items must surface
        // as `overflow`, not silently vanish.
        let mut bus = BlockCapabilityBus::new();
        let src_at = coord("mod_a:dim", 14);
        let dst_at = coord("mod_a:dim", 15);
        let mut src =
            SimpleSlotInventory::new(1, 64).with_insert_faces([Some(FaceDirection::Down)]);
        src.set_slot(0, Some(CapabilityItemStack::new("minecraft:cobblestone", 10)));
        bus.register_item_handler(src_at.clone(), Box::new(src));
        bus.register_item_handler(
            dst_at.clone(),
            Box::new(RejectOnCommitItems {
                inner: SimpleSlotInventory::new(1, 64),
            }),
        );

        let result = bus.transfer_item_checked(
            &src_at,
            Some(FaceDirection::Up),
            &dst_at,
            None,
            10,
            None,
        );
        assert_eq!(result.delivered, 0);
        assert_eq!(result.returned_to_source, 0, "source refused the rollback");
        assert_eq!(
            result.overflow,
            Some(CapabilityItemStack::new("minecraft:cobblestone", 10)),
            "unreturned items surface, never delete"
        );
        assert_eq!(result.accounted(), 10);
        assert_eq!(
            stored_items(&bus, &src_at),
            Some(0),
            "items did leave the source — the overflow now owns them"
        );
    }

    #[test]
    fn fluid_transfer_checked_conserves_delivered_returned_overflow() {
        let mut bus = BlockCapabilityBus::new();
        let src_at = coord("mod_a:dim", 16);
        let dst_at = coord("mod_b:dim", 17);
        let mut src = SimpleFluidTank::new(4000);
        src.set_fluid(Some(CapabilityFluidStack::new("minecraft:water", 2000)));
        bus.register_fluid_handler(src_at.clone(), Box::new(src));
        bus.register_fluid_handler(dst_at.clone(), Box::new(SimpleFluidTank::new(500)));

        // Partial acceptance: exactly the destination's capacity.
        let result = bus.transfer_fluid_checked(&src_at, None, &dst_at, None, 2000);
        assert_eq!(result.delivered, 500);
        assert_eq!(result.returned_to_source, 0);
        assert!(result.overflow.is_none());
        assert_eq!(result.accounted_mb(), 500);

        // Diverging destination + honest source: the whole commit rolls
        // back, `delivered` stays 0.
        let src2_at = coord("mod_a:dim", 18);
        let dst2_at = coord("mod_b:dim", 19);
        let mut src2 = SimpleFluidTank::new(4000);
        src2.set_fluid(Some(CapabilityFluidStack::new("mod_a:steam", 2000)));
        bus.register_fluid_handler(src2_at.clone(), Box::new(src2));
        bus.register_fluid_handler(
            dst2_at.clone(),
            Box::new(RejectOnCommitFluid {
                inner: SimpleFluidTank::new(1000),
            }),
        );
        let result = bus.transfer_fluid_checked(&src2_at, None, &dst2_at, None, 2000);
        assert_eq!(result.delivered, 0);
        assert_eq!(result.returned_to_source, 1000);
        assert!(result.overflow.is_none());
        assert_eq!(result.accounted_mb(), 1000);
        assert_eq!(
            stored_fluid(&bus, &src2_at).map(|f| f.amount_mb),
            Some(2000),
            "rollback restored the source whole"
        );

        // Drain-only source + diverging destination: the undelivered mB
        // surface as overflow, never counted as delivered.
        let src3_at = coord("mod_a:dim", 20);
        let dst3_at = coord("mod_b:dim", 21);
        let mut inner = SimpleFluidTank::new(4000);
        inner.set_fluid(Some(CapabilityFluidStack::new("mod_c:lava", 1500)));
        bus.register_fluid_handler(src3_at.clone(), Box::new(DrainOnlyFluid { inner }));
        bus.register_fluid_handler(
            dst3_at.clone(),
            Box::new(RejectOnCommitFluid {
                inner: SimpleFluidTank::new(1000),
            }),
        );
        let result = bus.transfer_fluid_checked(&src3_at, None, &dst3_at, None, 2000);
        assert_eq!(result.delivered, 0);
        assert_eq!(result.returned_to_source, 0);
        assert_eq!(
            result.overflow,
            Some(CapabilityFluidStack::new("mod_c:lava", 1000)),
            "undeliverable, unreturnable mB surfaced"
        );
        assert_eq!(result.accounted_mb(), 1000);
        // The convenience API keeps reporting the exact delivered amount.
        assert_eq!(
            bus.transfer_fluid_between(&src3_at, None, &dst3_at, None, 100),
            0
        );
    }

    #[test]
    fn energy_transfer_checked_conserves_every_fe() {
        let mut bus = BlockCapabilityBus::new();
        let src_at = coord("mod_a:dim", 22);
        let dst_at = coord("mod_b:dim", 23);
        let mut src = SimpleEnergyBuffer::new(4000);
        src.set_stored(1000);
        bus.register_energy_handler(src_at.clone(), Box::new(src));
        bus.register_energy_handler(dst_at.clone(), Box::new(SimpleEnergyBuffer::new(400)));

        let result = bus.transfer_energy_checked(&src_at, None, &dst_at, None, 800);
        assert_eq!(result.delivered, 400, "clamped by destination capacity");
        assert_eq!(result.returned_to_source, 0);
        assert_eq!(result.overflow_fe, 0);
        assert_eq!(result.accounted_fe(), 400);
        assert_eq!(stored_fe(&bus, &src_at), Some(600));

        // Diverging destination + honest source: bounced FE rolls back.
        let src2_at = coord("mod_a:dim", 24);
        let dst2_at = coord("mod_b:dim", 25);
        let mut src2 = SimpleEnergyBuffer::new(4000);
        src2.set_stored(900);
        bus.register_energy_handler(src2_at.clone(), Box::new(src2));
        bus.register_energy_handler(
            dst2_at.clone(),
            Box::new(RejectOnCommitEnergy {
                inner: SimpleEnergyBuffer::new(1000),
            }),
        );
        let result = bus.transfer_energy_checked(&src2_at, None, &dst2_at, None, 900);
        assert_eq!(result.delivered, 0);
        assert_eq!(result.returned_to_source, 900);
        assert_eq!(result.overflow_fe, 0);
        assert_eq!(result.accounted_fe(), 900);
        assert_eq!(stored_fe(&bus, &src2_at), Some(900), "source restored");

        // Extract-only source + diverging destination: refused FE surface
        // as `overflow_fe`, never silently deleted.
        let src3_at = coord("mod_a:dim", 26);
        let dst3_at = coord("mod_b:dim", 27);
        let mut inner = SimpleEnergyBuffer::new(4000);
        inner.set_stored(700);
        bus.register_energy_handler(src3_at.clone(), Box::new(ExtractOnlyEnergy { inner }));
        bus.register_energy_handler(
            dst3_at.clone(),
            Box::new(RejectOnCommitEnergy {
                inner: SimpleEnergyBuffer::new(1000),
            }),
        );
        let result = bus.transfer_energy_checked(&src3_at, None, &dst3_at, None, 700);
        assert_eq!(result.delivered, 0);
        assert_eq!(result.returned_to_source, 0);
        assert_eq!(result.overflow_fe, 700, "orphaned FE surfaced");
        assert_eq!(result.accounted_fe(), 700);
    }

    #[test]
    fn invalidation_rev_bumps_on_register_unregister_and_block_change() {
        let mut bus = BlockCapabilityBus::new();
        let at = coord("mod_a:dim", 30);
        let other = coord("mod_a:dim", 31);
        assert_eq!(bus.invalidation_rev(&at), 0);

        bus.register_item_handler(at.clone(), Box::new(SimpleSlotInventory::new(1, 64)));
        assert_eq!(bus.invalidation_rev(&at), 1);
        bus.register_fluid_handler(at.clone(), Box::new(SimpleFluidTank::new(100)));
        assert_eq!(bus.invalidation_rev(&at), 2);
        bus.register_item_handler(at.clone(), Box::new(SimpleSlotInventory::new(1, 64)));
        assert_eq!(bus.invalidation_rev(&at), 3, "replacing a handler invalidates");
        assert!(bus.unregister_item_handler(&at).is_some());
        assert_eq!(bus.invalidation_rev(&at), 4);
        assert!(bus.unregister_energy_handler(&at).is_none());
        assert_eq!(
            bus.invalidation_rev(&at),
            4,
            "removing nothing invalidates nothing"
        );
        bus.invalidate_at(&at);
        assert_eq!(bus.invalidation_rev(&at), 5);
        assert_eq!(bus.invalidation_rev(&other), 0, "unrelated coord untouched");
    }
}
