// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room chunk claims, parties, allies and force-loading in the spirit of
//! Open Parties and Claims (`openpartiesandclaims`) and FTB Chunks
//! (`ftbchunks`), implemented from scratch on `std` + [`megu_core`].
//!
//! The engine is deterministic end to end: every collection is a `BTree*` so
//! iteration order (and therefore persistence bytes, sync deltas and command
//! output) never depends on hash seeds or wall-clock state, and the module
//! keeps no clock at all — "online" is passed in per call.
//!
//! Layers:
//!
//! * [`ClaimChunkKey`] — a `(dimension, cx, cz)` triple whose dimension is
//!   normalized through [`ResourceLocation`], so `"overworld"`,
//!   `"minecraft:the_nether"` and mod dimensions like `"aeronautics:space"`
//!   all work identically.
//! * [`ClaimsWorld`] — claims plus a per-`(owner, sub-claim)` index enforcing
//!   `max_claims`/`max_forceloads` quotas, party membership/ally access
//!   groups, the OpenPAC over-limit restriction (an owner whose count
//!   exceeds their current limit loses party/ally access until back under),
//!   and cross-border grief protection: pistons, fluids, fire, lightning,
//!   explosions, projectiles, contraptions, dispensers/hoppers, vehicles,
//!   and mob/boss griefing. A [`ClaimPermissionSet`] matrix gives
//!   `everyone_else` (`自分以外の全員`) plus per-player overrides granular
//!   control over every player who ever joined (`known_players`).
//! * [`OrganicClaimPolicy`] — an optional, zero-pay-to-win claim cost
//!   economy: play time, advancements and boss defeats mint claim points,
//!   and chunk price floats with contiguity and nearby foreign-claim
//!   density.
//! * [`exec_openpac_command`] — the `/oclaims`, `/oparties`, `/opac` and
//!   `/opm` dispatcher.
//! * [`ClaimsSyncDelta`] — a monotonic `revision`-stamped feed minimap and
//!   claims-overlay adapters (Xaero's World Map / OpenPAC / FTB Chunks
//!   `CustomPacketPayload`) can drain.
//! * [`ClaimsWorld::save_to_dir`] / [`ClaimsWorld::load_from_dir`] — atomic
//!   `openpac_claims.json` / `openpac_parties.json` persistence through
//!   [`crate::atomic::write_atomic`]; corrupt input fails closed with
//!   `InvalidData`, never a half-loaded world.

use crate::atomic::write_atomic;
use crate::claims_wire::{build_delta_batches, build_dimension_sync_batches, ClaimsWireBatch};
use crate::json::{self, Json};
use megu_core::id::ResourceLocation;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::Path;
use std::str::FromStr;

/// The claims store's file name inside a server directory.
pub const CLAIMS_FILE: &str = "openpac_claims.json";
/// The party store's file name inside a server directory.
pub const PARTIES_FILE: &str = "openpac_parties.json";
/// OpenPAC's built-in player claims config id.
pub const DEFAULT_SUB_CLAIM: &str = "main";
/// Party ranks at or above this may invite, ally and kick lower ranks.
const PARTY_OFFICER: PartyRank = PartyRank::Moderator;

// ------------------------------------------------------------- identifiers

/// A `(dimension, chunk-x, chunk-z)` triple. `dimension` is normalized to the
/// canonical `namespace:path` spelling by [`ResourceLocation::parse`], so
/// `"overworld"` and `"minecraft:overworld"` are the same key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaimChunkKey {
    /// Canonical `namespace:path` dimension id.
    pub dimension: String,
    /// Chunk x coordinate (`block_x >> 4`).
    pub cx: i32,
    /// Chunk z coordinate (`block_z >> 4`).
    pub cz: i32,
}

impl ClaimChunkKey {
    /// Build a key; the dimension is validated like any registry id, so
    /// malformed input (bad characters, `..` traversal, extra `:`) is an
    /// error, never silently repaired.
    pub fn new(dimension: &str, cx: i32, cz: i32) -> Result<Self, ClaimsError> {
        let dim = ResourceLocation::parse(dimension)
            .map_err(|e| ClaimsError::InvalidDimension(e.to_string()))?;
        Ok(Self {
            dimension: dim.to_string(),
            cx,
            cz,
        })
    }

    /// The chunk key containing block position `(x, z)`. `>> 4` on `i32` is
    /// an arithmetic shift, i.e. `floor_div` by 16 for negative coordinates,
    /// exactly like Vanilla's `SectionPos.blockToSectionCoord`.
    pub fn for_block(dimension: &str, x: i32, z: i32) -> Result<Self, ClaimsError> {
        Self::new(dimension, x >> 4, z >> 4)
    }
}

impl fmt::Display for ClaimChunkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} chunk ({}, {})", self.dimension, self.cx, self.cz)
    }
}

/// A named sub-claim id — OpenPAC's player claims "sub-configs". `"main"` is
/// the default every player gets; named sub-claims (e.g. `"farm"`) keep
/// separate claims and quotas per owner.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubClaimId(String);

impl SubClaimId {
    /// The default sub-claim id.
    pub const MAIN: &'static str = DEFAULT_SUB_CLAIM;

    /// `1..=32` chars of `[a-z0-9_-]`, normalized to lowercase.
    pub fn parse(name: &str) -> Result<Self, ClaimsError> {
        let n = name.trim().to_ascii_lowercase();
        let ok = !n.is_empty()
            && n.len() <= 32
            && n.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if ok {
            Ok(Self(n))
        } else {
            Err(ClaimsError::InvalidSubClaimId(name.to_owned()))
        }
    }

    /// The default `"main"` sub-claim.
    pub fn main() -> Self {
        Self(DEFAULT_SUB_CLAIM.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SubClaimId {
    type Err = ClaimsError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for SubClaimId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A player the world has seen at least once — the claim UI's roster for
/// 1-click per-player permission editing, including players who are
/// offline or have never been trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownPlayerRecord {
    /// Last seen display name (updated on every sighting).
    pub name: String,
    /// First join tick the player was recorded at.
    pub first_seen_tick: u64,
    /// Most recent sighting tick.
    pub last_seen_tick: u64,
}

/// What a player may do inside a claim — the granular permission matrix
/// behind `everyone_else` and `player_overrides` on
/// [`ClaimProtectionConfig`]. Every field is an independent allow, so a
/// claim UI can offer one toggle per row, per known player or for
/// "everyone else".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClaimPermissionSet {
    /// Break blocks inside the claim.
    pub block_break: bool,
    /// Place blocks inside the claim.
    pub block_place: bool,
    /// Open containers: chests, barrels, shulkers, hoppers, mod machines.
    pub containers: bool,
    /// Use doors, trapdoors, fence gates, buttons, levers, pressure plates.
    pub redstone_and_doors: bool,
    /// Right-click/leash/shear/milk/ride/name entities: armor stands, item
    /// frames, villagers, pets, buckets, nametags.
    pub entity_interact: bool,
    /// Attack or shoot animals, villagers, pets and other entities inside.
    pub entity_damage: bool,
    /// Pick up or drop items inside the claim.
    pub item_pickup_drop: bool,
    /// Teleport in: ender pearls, chorus fruit, modded blinks.
    pub teleport_in: bool,
}

impl ClaimPermissionSet {
    /// Everything denied — the starting point for a stranger.
    pub fn denied_all() -> Self {
        Self::default()
    }

    /// A tourist: doors/redstone and item pickup, nothing else.
    pub fn visitor() -> Self {
        Self {
            redstone_and_doors: true,
            item_pickup_drop: true,
            ..Self::denied_all()
        }
    }

    /// Full access — what the owner and trusted party members get.
    pub fn trusted_builder() -> Self {
        Self {
            block_break: true,
            block_place: true,
            containers: true,
            redstone_and_doors: true,
            entity_interact: true,
            entity_damage: true,
            item_pickup_drop: true,
            teleport_in: true,
        }
    }

    /// Chest/inventory access only.
    pub fn container_only() -> Self {
        Self {
            containers: true,
            ..Self::denied_all()
        }
    }

    /// Field-wise OR — folds the `protect_*`-derived outsider floor into
    /// `everyone_else` so permissive legacy flags keep working.
    fn union(self, other: Self) -> Self {
        Self {
            block_break: self.block_break || other.block_break,
            block_place: self.block_place || other.block_place,
            containers: self.containers || other.containers,
            redstone_and_doors: self.redstone_and_doors || other.redstone_and_doors,
            entity_interact: self.entity_interact || other.entity_interact,
            entity_damage: self.entity_damage || other.entity_damage,
            item_pickup_drop: self.item_pickup_drop || other.item_pickup_drop,
            teleport_in: self.teleport_in || other.teleport_in,
        }
    }

    fn allows(&self, slot: PermSlot) -> bool {
        match slot {
            PermSlot::BlockBreak => self.block_break,
            PermSlot::BlockPlace => self.block_place,
            PermSlot::Containers => self.containers,
            PermSlot::RedstoneAndDoors => self.redstone_and_doors,
            PermSlot::EntityInteract => self.entity_interact,
            PermSlot::EntityDamage => self.entity_damage,
            PermSlot::EntityPush => self.entity_interact || self.entity_damage,
        }
    }
}

/// One row of [`ClaimPermissionSet`], so `check_action` can name an
/// action's required permission without stringly keys. `EntityPush` covers
/// both the "touch" and "hurt" forms of displacement (body-push, fishing
/// rod, knockback).
#[derive(Clone, Copy)]
enum PermSlot {
    BlockBreak,
    BlockPlace,
    Containers,
    RedstoneAndDoors,
    EntityInteract,
    EntityDamage,
    EntityPush,
}

// ------------------------------------------------------------------- model

/// Who a claim belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimOwner {
    /// A player's claim; the string is their UUID.
    Player(String),
    /// An admin/server claim (spawn protection, server infrastructure).
    Server,
    /// A claim whose owner's data expired (OpenPAC's "expired claims"
    /// cleanup state): it still protects, only admins can touch it.
    Expired(String),
}

/// A single claimed chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkClaim {
    pub owner: ClaimOwner,
    /// The sub-claim id this chunk belongs to (`"main"` or a named one).
    pub sub_claim: String,
    /// Force-load ticket: the chunk stays ticking while its owner qualifies
    /// under [`ClaimsWorld::active_forceloaded_chunks`].
    pub forceloaded: bool,
    /// Overlay color `0xRRGGBB` clients render the claim in.
    pub color_rgb: u32,
}

/// Party membership rank; ordering is `Owner > Admin > Moderator > Member`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartyRank {
    Owner,
    Admin,
    Moderator,
    Member,
}

impl PartyRank {
    /// Higher number, more privilege.
    pub fn level(self) -> u8 {
        match self {
            Self::Owner => 3,
            Self::Admin => 2,
            Self::Moderator => 1,
            Self::Member => 0,
        }
    }

    /// `self` carries at least `other`'s privilege.
    pub fn at_least(self, other: PartyRank) -> bool {
        self.level() >= other.level()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Moderator => "moderator",
            Self::Member => "member",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "owner" => Self::Owner,
            "admin" => Self::Admin,
            "moderator" => Self::Moderator,
            "member" => Self::Member,
            _ => return None,
        })
    }
}

/// A player party: claims' `allow_party_members`/`allow_allies` policies key
/// off membership and the `allies` set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Party {
    pub id: String,
    pub name: String,
    pub owner_uuid: String,
    /// `uuid -> rank`; `owner_uuid` always maps to [`PartyRank::Owner`].
    pub members: BTreeMap<String, PartyRank>,
    /// Allied party ids (declared direction; protection treats an alliance
    /// either party declared as mutual, like OpenPAC).
    pub allies: BTreeSet<String>,
    /// Player UUIDs invited but not yet accepted.
    pub pending_invites: BTreeSet<String>,
    pub color_rgb: u32,
}

impl Party {
    /// `uuid`'s rank, or `None` for non-members.
    pub fn rank_of(&self, uuid: &str) -> Option<PartyRank> {
        self.members.get(uuid).copied()
    }
}

/// Per-owner, per-sub-claim protection policy — the fields OpenPAC's player
/// claims config and FTB Chunks' team config both carry, plus the granular
/// permission matrix and cross-border grief guards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimProtectionConfig {
    /// Block break/place by outsiders.
    pub protect_blocks: bool,
    /// Chest/inventory access by outsiders.
    pub protect_containers: bool,
    /// Attacking entities inside the claim.
    pub protect_entities: bool,
    /// Explosion damage inside the claim.
    pub protect_explosions: bool,
    /// Pistons moving blocks across the claim border.
    pub protect_piston_border: bool,
    /// Fluids flowing across the claim border.
    pub protect_fluid_border: bool,
    /// Right-clicking/leashing/shearing entities (armor stands, item
    /// frames, villagers, pets) inside the claim.
    pub protect_entity_interact: bool,
    /// Projectiles (arrows, tridents, potions, mod spells) fired from
    /// outside the claim landing inside it.
    pub protect_projectiles_border: bool,
    /// Fire spread and lightning (including Channeling tridents) crossing
    /// the claim border.
    pub protect_fire_and_lightning: bool,
    /// Mob and boss griefing (endermen, ravagers, withers, colossal
    /// dragons, modded mobs) inside or into the claim.
    pub protect_mob_and_boss_grief: bool,
    /// Modded contraptions/machines (drills, ploughs, quarries) crossing
    /// the claim border.
    pub protect_contraption_border: bool,
    /// Dispensers dumping fluid/items or hoppers siphoning inventories
    /// across the claim border.
    pub protect_dispenser_hopper_border: bool,
    /// Boats, minecarts and modded vehicles pushed/placed across the claim
    /// border (including forced-boarding kidnaps).
    pub protect_vehicle_border: bool,
    /// Outsiders body-pushing, fishing-rod pulling or knockback-displacing
    /// entities inside the claim.
    pub protect_entity_push: bool,
    /// Outsiders luring mobs out of the claim with tempt items (wheat,
    /// carrots, seeds, modded bait).
    pub protect_entity_lure: bool,
    /// Members of the owner's party may act inside the claim.
    pub allow_party_members: bool,
    /// Members of allied parties may act inside the claim.
    pub allow_allies: bool,
    /// This owner's forceloaded chunks keep ticking while they are offline.
    pub offline_forceload: bool,
    /// Claims quota for this `(owner, sub-claim)` pair.
    pub max_claims: usize,
    /// Forceload quota for this `(owner, sub-claim)` pair.
    pub max_forceloads: usize,
    /// What everyone other than the owner, their party members, allies and
    /// explicitly overridden players may do — the "everyone else"
    /// (`自分以外の全員`) row of the permission matrix. The resolved
    /// permission is `everyone_else` OR the floor implied by disabled
    /// `protect_*` flags, so a permissive flag never behaves stricter.
    pub everyone_else: ClaimPermissionSet,
    /// `uuid -> permissions` — explicit per-player overrides, checked
    /// before party/ally trust and `everyone_else`, so one friend can be
    /// trusted fully or one specific player blocked outright.
    pub player_overrides: BTreeMap<String, ClaimPermissionSet>,
    /// Minimap overlay color `0xRRGGBB`; `0` means "auto" — a
    /// deterministic owner-hashed color (see
    /// [`ClaimsWorld::claim_display_color`]).
    pub display_color_rgb: u32,
    /// Optional minimap label for this `(owner, sub-claim)` region.
    pub display_label: Option<String>,
}

impl Default for ClaimProtectionConfig {
    /// The permissive-but-protected default a fresh player starts with:
    /// every guard on, outsiders denied everything, no overrides.
    fn default() -> Self {
        Self {
            protect_blocks: true,
            protect_containers: true,
            protect_entities: true,
            protect_explosions: true,
            protect_piston_border: true,
            protect_fluid_border: true,
            protect_entity_interact: true,
            protect_projectiles_border: true,
            protect_fire_and_lightning: true,
            protect_mob_and_boss_grief: true,
            protect_contraption_border: true,
            protect_dispenser_hopper_border: true,
            protect_vehicle_border: true,
            protect_entity_push: true,
            protect_entity_lure: true,
            allow_party_members: true,
            allow_allies: false,
            offline_forceload: false,
            max_claims: 500,
            max_forceloads: 25,
            everyone_else: ClaimPermissionSet::denied_all(),
            player_overrides: BTreeMap::new(),
            display_color_rgb: 0,
            display_label: None,
        }
    }
}

impl ClaimProtectionConfig {
    /// Server claims protect everything, trust no group, and are not
    /// quota-bound (they are created by admins, not players).
    pub fn server() -> Self {
        Self {
            allow_party_members: false,
            allow_allies: false,
            offline_forceload: true,
            max_claims: usize::MAX,
            max_forceloads: usize::MAX,
            ..Self::default()
        }
    }
}

// ----------------------------------------------------------------- actions

/// Something trying to happen inside (or across the border of) a chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimAction {
    BlockBreak,
    BlockPlace,
    ContainerInteract,
    EntityAttack,
    /// Right-clicking, leashing, shearing, milking, riding or naming an
    /// entity inside the claim: armor stands, item frames, villagers,
    /// pets, nametags, buckets.
    EntityInteract,
    /// Using a door, trapdoor, fence gate, button, lever or pressure plate.
    RedstoneOrDoorInteract,
    /// Body-pushing, fishing-rod pulling or knockback-displacing an entity
    /// inside the claim (the classic "shove the villager out" grief).
    EntityPushOrDisplace,
    /// Luring a mob inside the claim with tempt food/items so its follow
    /// AI walks it out across the border.
    EntityTemptOrLure,
    /// An explosion originating anywhere; no actor.
    Explosion,
    /// A piston in the given source chunk moving blocks into the target.
    PistonFrom(ClaimChunkKey),
    /// Fluid in the given source chunk flowing into the target.
    FluidFrom(ClaimChunkKey),
    /// Fire ignited in the source chunk spreading into the target.
    FireSpreadFrom(ClaimChunkKey),
    /// Lightning (including Channeling tridents) called down in the source
    /// chunk striking inside the target.
    LightningFrom(ClaimChunkKey),
    /// An explosion whose blast originates in `source` and reaches into
    /// the target: TNT cannons, creepers, end crystals, respawn anchors,
    /// beds. `actor_uuid` is who lit it, when known.
    ExplosionFrom {
        source: ClaimChunkKey,
        actor_uuid: Option<String>,
    },
    /// A projectile fired from `source` landing in the target: arrows,
    /// tridents, splash/lingering potions, wind charges, mod spells.
    /// Denied unless the shooter holds `entity_damage`/`block_break`
    /// permission in the target claim.
    ProjectileFrom {
        source: ClaimChunkKey,
        shooter_uuid: Option<String>,
    },
    /// A mob or boss griefing inside, or into, the target: endermen,
    /// ravagers, withers, colossal dragons breathing fire/ice/lightning or
    /// smashing blocks, or any modded mob. `source` is where the mob is,
    /// when known; `is_boss_or_dragon` marks boss-class actors.
    MobOrBossGrief {
        source: Option<ClaimChunkKey>,
        is_boss_or_dragon: bool,
    },
    /// A modded contraption or machine (drill, plough, quarry) based in
    /// `source` reaching into the target. `owner_uuid` is who built it,
    /// when known.
    ContraptionOrMachineFrom {
        source: ClaimChunkKey,
        owner_uuid: Option<String>,
    },
    /// A dispenser or hopper in `source` dumping fluid/items into, or
    /// siphoning inventories out of, the target.
    DispenserOrHopperFrom(ClaimChunkKey),
    /// A boat, minecart or modded vehicle pushed/placed from `source` into
    /// the target, or an actor in `source` forcing an entity to board one
    /// (vehicle kidnap). `actor_uuid` is who moved it, when known.
    VehicleEnterOrInject {
        source: ClaimChunkKey,
        actor_uuid: Option<String>,
    },
}

/// Why a [`ClaimAction`] was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyReason {
    BlocksProtected,
    ContainersProtected,
    EntitiesProtected,
    ExplosionsProtected,
    PistonBorderProtected,
    FluidBorderProtected,
    EntityInteractProtected,
    RedstoneOrDoorProtected,
    EntityPushProtected,
    EntityLureProtected,
    ProjectilesBorderProtected,
    FireAndLightningProtected,
    MobOrBossGriefProtected,
    ContraptionBorderProtected,
    DispenserHopperBorderProtected,
    VehicleBorderProtected,
    /// Server claim: admins only.
    ServerClaim,
    /// Expired claim: admins only.
    ExpiredClaim,
    /// The owner is over their claim limit, so party/ally access — which
    /// would otherwise have applied — is suspended.
    OwnerOverLimit,
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BlocksProtected => "this claim protects blocks",
            Self::ContainersProtected => "this claim protects containers",
            Self::EntitiesProtected => "this claim protects entities",
            Self::ExplosionsProtected => "this claim protects against explosions",
            Self::PistonBorderProtected => "pistons cannot reach across this claim's border",
            Self::FluidBorderProtected => "fluids cannot flow across this claim's border",
            Self::EntityInteractProtected => "this claim protects entity interaction",
            Self::RedstoneOrDoorProtected => {
                "this claim protects doors and redstone components"
            }
            Self::EntityPushProtected => "entities cannot be pushed out of this claim",
            Self::EntityLureProtected => "entities cannot be lured out of this claim",
            Self::ProjectilesBorderProtected => {
                "projectiles cannot cross this claim's border"
            }
            Self::FireAndLightningProtected => {
                "fire and lightning cannot cross this claim's border"
            }
            Self::MobOrBossGriefProtected => "mobs and bosses cannot grief this claim",
            Self::ContraptionBorderProtected => {
                "contraptions and machines cannot cross this claim's border"
            }
            Self::DispenserHopperBorderProtected => {
                "dispensers and hoppers cannot reach across this claim's border"
            }
            Self::VehicleBorderProtected => {
                "vehicles cannot cross this claim's border"
            }
            Self::ServerClaim => "server claim: admins only",
            Self::ExpiredClaim => "expired claim: admins only",
            Self::OwnerOverLimit => {
                "the owner is over their claim limit, so trusted access is suspended"
            }
        })
    }
}

/// The engine's verdict on a [`ClaimAction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimDecision {
    Allowed,
    Denied(DenyReason),
}

impl ClaimDecision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
    pub fn is_denied(self) -> bool {
        matches!(self, Self::Denied(_))
    }
}

// ------------------------------------------------------------------ errors

/// Every way a claims or party operation can fail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimsError {
    /// The dimension string failed [`ResourceLocation`] validation.
    InvalidDimension(String),
    /// A sub-claim id outside `1..=32` of `[a-z0-9_-]`.
    InvalidSubClaimId(String),
    /// The chunk is already claimed by this owner under this sub-claim.
    AlreadyClaimed,
    /// The chunk is claimed by someone else (admins may override).
    ClaimedByOther,
    /// Nothing is claimed at the key.
    NotClaimed,
    /// The actor does not own the claim.
    NotOwner,
    /// The operation needs server-admin privilege.
    NotAuthorized,
    /// `max_claims` reached.
    ClaimLimitReached { used: usize, limit: usize },
    /// `max_forceloads` reached.
    ForceloadLimitReached { used: usize, limit: usize },
    /// The player is already in a party.
    AlreadyInParty,
    /// The player is in no party.
    NotInParty,
    /// No party with that id/name.
    PartyNotFound,
    /// The player holds no invite to that party.
    NotInvited,
    /// The player is already invited to the party.
    AlreadyInvited,
    /// The actor's rank is too low for the operation (or not above the
    /// target's, for kicks).
    InsufficientRank,
    /// Empty or over-long party name.
    InvalidPartyName,
    /// A nonsensical target (self-kick, `Owner` via `set_rank`, …).
    InvalidTarget,
    /// The other party is already an ally.
    AlreadyAllied,
    /// The other party is not an ally.
    NotAllied,
    /// The dimension is not open to player claims under the world's
    /// allowed-claim-dimensions policy (admins/server claims still work).
    DimensionNotClaimable(String),
}

impl fmt::Display for ClaimsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimension(m) => write!(f, "invalid dimension: {m}"),
            Self::InvalidSubClaimId(s) => write!(
                f,
                "invalid sub-claim id {s:?}: use 1-32 chars of [a-z0-9_-]"
            ),
            Self::AlreadyClaimed => write!(f, "chunk is already claimed"),
            Self::ClaimedByOther => write!(f, "chunk is claimed by someone else"),
            Self::NotClaimed => write!(f, "chunk is not claimed"),
            Self::NotOwner => write!(f, "you do not own this claim"),
            Self::NotAuthorized => write!(f, "requires server admin"),
            Self::ClaimLimitReached { used, limit } => {
                write!(f, "claim limit reached: {used}/{limit}")
            }
            Self::ForceloadLimitReached { used, limit } => {
                write!(f, "forceload limit reached: {used}/{limit}")
            }
            Self::AlreadyInParty => write!(f, "already in a party"),
            Self::NotInParty => write!(f, "not in a party"),
            Self::PartyNotFound => write!(f, "no such party"),
            Self::NotInvited => write!(f, "no invite to that party"),
            Self::AlreadyInvited => write!(f, "player already invited"),
            Self::InsufficientRank => write!(f, "insufficient party rank"),
            Self::InvalidPartyName => write!(f, "party name must be 1-48 characters"),
            Self::InvalidTarget => write!(f, "invalid target"),
            Self::AlreadyAllied => write!(f, "already allied"),
            Self::NotAllied => write!(f, "not allied"),
            Self::DimensionNotClaimable(d) => {
                write!(f, "dimension {d:?} is not claimable")
            }
        }
    }
}

impl std::error::Error for ClaimsError {}

// -------------------------------------------------------------- sync delta

/// One entry in the deterministic client-sync feed. `revision` is strictly
/// monotonic per [`ClaimsWorld`], so a client that missed entries can tell.
#[derive(Clone, Debug, PartialEq)]
pub enum ClaimsSyncDelta {
    /// A claim was created, updated (forceload, sub-claim move, expiry) or
    /// removed (`claim == None`).
    ClaimUpdated {
        revision: u64,
        key: ClaimChunkKey,
        claim: Option<ChunkClaim>,
    },
}

impl ClaimsSyncDelta {
    /// The delta's monotonic sequence number.
    pub fn revision(&self) -> u64 {
        match self {
            Self::ClaimUpdated { revision, .. } => *revision,
        }
    }
}

// --------------------------------------------------------------- the world

/// The whole claims+parties engine state.
///
/// Internally every claim owner is reduced to an *owner key* string —
/// `player:<uuid>`, `server`, or `expired:<uuid>` — which keys both the
/// quota index and the config table.
#[derive(Clone, Debug)]
pub struct ClaimsWorld {
    claims: BTreeMap<ClaimChunkKey, ChunkClaim>,
    /// `(owner key, sub-claim) -> chunks` — the quota/access index.
    index: BTreeMap<(String, String), BTreeSet<ClaimChunkKey>>,
    parties: BTreeMap<String, Party>,
    /// `player uuid -> party id` (at most one party per player).
    member_party: BTreeMap<String, String>,
    /// `(owner key, sub-claim) -> policy`; absent entries fall back to the
    /// owner's `"main"` config, then to `default_config`.
    configs: BTreeMap<(String, String), ClaimProtectionConfig>,
    default_config: ClaimProtectionConfig,
    /// Server admins: bypass claim protection and own server/expired claims.
    admins: BTreeSet<String>,
    /// `uuid -> {name, first/last seen}` for every player who has ever
    /// joined — the claim UI's roster for 1-click permission editing.
    known_players: BTreeMap<String, KnownPlayerRecord>,
    /// Dimensions players may claim in; `None` = unrestricted (the
    /// historical default, so existing worlds keep their semantics).
    /// Admin takeovers and server claims are exempt.
    allowed_claim_dimensions: Option<BTreeSet<String>>,
    /// How player claims are priced.
    claim_cost_mode: ClaimCostMode,
    revision: u64,
    deltas: Vec<ClaimsSyncDelta>,
    next_party_id: u64,
}

impl Default for ClaimsWorld {
    fn default() -> Self {
        Self::new()
    }
}

/// How player claims are priced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClaimCostMode {
    /// The historical quota model: `max_claims` chunks per `(owner, sub)`,
    /// nothing else to pay. The default, so existing behavior is exact.
    #[default]
    FreeQuota,
    /// [`OrganicClaimPolicy`]-priced claims: chunks cost organic points
    /// earned by playing, with a fluid contiguity/density curve.
    OrganicFluid,
}

impl ClaimCostMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::FreeQuota => "free_quota",
            Self::OrganicFluid => "organic_fluid",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "free_quota" => Self::FreeQuota,
            "organic_fluid" => Self::OrganicFluid,
            _ => return None,
        })
    }
}

/// The tuning knobs of the organic claim economy — a fluid, zero-pay-to-win
/// territory progression where land value comes from adjacency and local
/// crowding rather than purchases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrganicClaimPolicy {
    /// Chunks each player may claim for free — the starter plot for the
    /// first chest/crafting table. `0` disables the freebie entirely.
    pub starter_free_chunks: usize,
    /// Base price of one chunk in organic points.
    pub base_chunk_cost: u64,
    /// Price multiplier, in basis points (`10_000` = `1.00×`), when the new
    /// chunk is edge-adjacent to the owner's existing claim.
    pub contiguous_multiplier_bps: u32,
    /// Price multiplier, in basis points (`22_500` = `2.25×`), when the new
    /// chunk is a disconnected island — discourages scatter-claiming.
    pub island_multiplier_bps: u32,
    /// Density surcharge per foreign claim within 3 chunks, in basis
    /// points (`1_500` = `+15%` each) — fluid land value near crowded
    /// hubs, cheap claims out on the frontier.
    pub density_step_bps: u32,
    /// Density multiplier cap, in basis points (`30_000` = `3.0×`), so
    /// prices stay attainable.
    pub max_density_multiplier_bps: u32,
    /// Points minted per hour played (`72_000` ticks).
    pub points_per_hour_played: u64,
    /// Points minted per advancement earned.
    pub points_per_advancement: u64,
    /// Points minted per boss defeated.
    pub points_per_boss_defeat: u64,
}

impl Default for OrganicClaimPolicy {
    /// Balanced survival defaults: one free starter chunk, `2.25×` island
    /// penalty, `+15%`-per-foreign-chunk density capped at `3.0×`.
    fn default() -> Self {
        Self {
            starter_free_chunks: 1,
            base_chunk_cost: 100,
            contiguous_multiplier_bps: 10_000,
            island_multiplier_bps: 22_500,
            density_step_bps: 1_500,
            max_density_multiplier_bps: 30_000,
            points_per_hour_played: 120,
            points_per_advancement: 60,
            points_per_boss_defeat: 250,
        }
    }
}

/// The deterministic quote for claiming one chunk under the organic
/// economy — every input that went into `cost_points` is reported so the
/// UI can explain the price.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrganicCostQuote {
    /// Final chunk price in organic points (`0` while starter-free).
    pub cost_points: u64,
    /// The owner is still inside their free starter allowance.
    pub is_starter_free: bool,
    /// The chunk shares an edge with an existing claim of the same owner.
    pub is_contiguous: bool,
    /// Foreign-owned claims within a 3-chunk Chebyshev radius.
    pub nearby_foreign_claims: usize,
    /// The applied density multiplier in basis points, after the cap.
    pub density_multiplier_bps: u32,
}

/// One full hour of play in ticks (`20 tps * 60 s * 60`).
const TICKS_PER_HOUR: u64 = 20 * 60 * 60;

/// The deterministic price of `owner_uuid` claiming `key` under `policy`.
/// Pure: reads the world, touches nothing.
pub fn compute_organic_chunk_cost(
    world: &ClaimsWorld,
    owner_uuid: &str,
    key: &ClaimChunkKey,
    policy: &OrganicClaimPolicy,
) -> OrganicCostQuote {
    let okey = player_key(owner_uuid);
    let owned: usize = world
        .index
        .iter()
        .filter(|((o, _), _)| o == &okey)
        .map(|(_, chunks)| chunks.len())
        .sum();
    let is_starter_free = owned < policy.starter_free_chunks;
    let is_contiguous = [(1_i32, 0_i32), (-1, 0), (0, 1), (0, -1)]
        .iter()
        .any(|(dx, dz)| {
            let nk = ClaimChunkKey {
                dimension: key.dimension.clone(),
                cx: key.cx + dx,
                cz: key.cz + dz,
            };
            world
                .claims
                .get(&nk)
                .map(|c| owner_key(&c.owner) == okey)
                .unwrap_or(false)
        });
    let nearby_foreign_claims = world
        .claims
        .iter()
        .filter(|(k2, c)| {
            k2.dimension == key.dimension
                && owner_key(&c.owner) != okey
                && k2.cx.abs_diff(key.cx) <= 3
                && k2.cz.abs_diff(key.cz) <= 3
        })
        .count();
    let density_multiplier_bps = 10_000_u32
        .saturating_add(
            policy
                .density_step_bps
                .saturating_mul(nearby_foreign_claims as u32),
        )
        .min(policy.max_density_multiplier_bps)
        .max(10_000);
    let multiplier_bps = if is_contiguous {
        policy.contiguous_multiplier_bps
    } else {
        policy.island_multiplier_bps
    };
    let cost_points = if is_starter_free {
        0
    } else {
        let cost = u128::from(policy.base_chunk_cost)
            * u128::from(multiplier_bps)
            * u128::from(density_multiplier_bps)
            / 10_000
            / 10_000;
        u64::try_from(cost).unwrap_or(u64::MAX)
    };
    OrganicCostQuote {
        cost_points,
        is_starter_free,
        is_contiguous,
        nearby_foreign_claims,
        density_multiplier_bps,
    }
}

/// The organic points a player has earned: whole hours played plus
/// advancements plus boss defeats. Saturating — budget arithmetic can
/// never wrap.
pub fn compute_organic_budget(
    played_ticks: u64,
    advancements: u64,
    bosses_defeated: u64,
    policy: &OrganicClaimPolicy,
) -> u64 {
    (played_ticks / TICKS_PER_HOUR)
        .saturating_mul(policy.points_per_hour_played)
        .saturating_add(advancements.saturating_mul(policy.points_per_advancement))
        .saturating_add(bosses_defeated.saturating_mul(policy.points_per_boss_defeat))
}

/// The index/config key for a claim owner.
fn owner_key(owner: &ClaimOwner) -> String {
    match owner {
        ClaimOwner::Player(u) => format!("player:{u}"),
        ClaimOwner::Server => "server".to_owned(),
        ClaimOwner::Expired(u) => format!("expired:{u}"),
    }
}

fn player_key(uuid: &str) -> String {
    format!("player:{uuid}")
}

/// Deterministic `0xRRGGBB` from any stable seed (uuid or party id): FNV-1a.
fn color_for(seed: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in seed.as_bytes() {
        h = (h ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    h & 0x00ff_ffff
}

/// The access outsiders retain purely because a `protect_*` master switch
/// is off — folded into resolved permission sets so disabling a flag never
/// behaves stricter than it did before the permission matrix existed.
fn outsider_floor(c: &ClaimProtectionConfig) -> ClaimPermissionSet {
    ClaimPermissionSet {
        block_break: !c.protect_blocks,
        block_place: !c.protect_blocks,
        containers: !c.protect_containers,
        redstone_and_doors: !c.protect_blocks,
        entity_interact: !c.protect_entity_interact,
        entity_damage: !c.protect_entities,
        item_pickup_drop: false,
        teleport_in: false,
    }
}

/// What every action rule needs to know about the claim under test —
/// resolved once per [`ClaimsWorld::check_action`] call, so helpers take one
/// borrowed context instead of three parallel arguments.
struct ClaimCtx<'a> {
    okey: &'a str,
    sub: &'a str,
    cfg: &'a ClaimProtectionConfig,
}

/// The permission row an action maps to, plus the master flag and deny
/// wording that describe it.
#[derive(Clone, Copy)]
struct SlotRule {
    slot: PermSlot,
    protected: bool,
    reason: DenyReason,
}

/// The permission row `action` needs. Border and whole-claim variants never
/// reach here: [`ClaimsWorld::claim_wide_action`] decides them first.
fn slot_rule(action: &ClaimAction, cfg: &ClaimProtectionConfig) -> SlotRule {
    let (slot, protected, reason) = match action {
        ClaimAction::BlockBreak => (
            PermSlot::BlockBreak, cfg.protect_blocks, DenyReason::BlocksProtected,
        ),
        ClaimAction::BlockPlace => (
            PermSlot::BlockPlace, cfg.protect_blocks, DenyReason::BlocksProtected,
        ),
        ClaimAction::ContainerInteract => (
            PermSlot::Containers, cfg.protect_containers, DenyReason::ContainersProtected,
        ),
        ClaimAction::EntityAttack => (
            PermSlot::EntityDamage, cfg.protect_entities, DenyReason::EntitiesProtected,
        ),
        ClaimAction::EntityInteract => (
            PermSlot::EntityInteract,
            cfg.protect_entity_interact,
            DenyReason::EntityInteractProtected,
        ),
        ClaimAction::RedstoneOrDoorInteract => (
            PermSlot::RedstoneAndDoors, cfg.protect_blocks, DenyReason::RedstoneOrDoorProtected,
        ),
        ClaimAction::EntityPushOrDisplace => (
            PermSlot::EntityPush, cfg.protect_entity_push, DenyReason::EntityPushProtected,
        ),
        ClaimAction::EntityTemptOrLure => (
            PermSlot::EntityInteract, cfg.protect_entity_lure, DenyReason::EntityLureProtected,
        ),
        _ => unreachable!("border actions are decided before the permission matrix"),
    };
    SlotRule { slot, protected, reason }
}

/// `Denied(reason)` while `protected`, otherwise `Allowed` — the shared shape
/// of every master flag and explicit permission row.
fn flag_decision(protected: bool, reason: DenyReason) -> ClaimDecision {
    if protected {
        ClaimDecision::Denied(reason)
    } else {
        ClaimDecision::Allowed
    }
}

/// The dimensions [`ClaimsWorld::new_survival_default`] opens to player
/// claims: Overworld only, so Nether fortresses, End islands and mod
/// dimensions cannot be monopolized.
pub const DEFAULT_SURVIVAL_CLAIM_DIMENSIONS: &[&str] = &["minecraft:overworld"];

impl ClaimsWorld {
    pub fn new() -> Self {
        Self {
            claims: BTreeMap::new(),
            index: BTreeMap::new(),
            parties: BTreeMap::new(),
            member_party: BTreeMap::new(),
            configs: BTreeMap::new(),
            default_config: ClaimProtectionConfig::default(),
            admins: BTreeSet::new(),
            known_players: BTreeMap::new(),
            allowed_claim_dimensions: None,
            claim_cost_mode: ClaimCostMode::FreeQuota,
            revision: 0,
            deltas: Vec::new(),
            next_party_id: 1,
        }
    }

    /// A survival-world world: like [`Self::new`] but player claims are
    /// restricted to [`DEFAULT_SURVIVAL_CLAIM_DIMENSIONS`]. Admin takeovers
    /// and server claims still work in any dimension.
    pub fn new_survival_default() -> Self {
        let mut w = Self::new();
        w.allowed_claim_dimensions = Some(
            DEFAULT_SURVIVAL_CLAIM_DIMENSIONS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        );
        w
    }

    // ------------------------------------------------------------- queries

    /// Every claim, in key order.
    pub fn claims(&self) -> &BTreeMap<ClaimChunkKey, ChunkClaim> {
        &self.claims
    }

    /// The claim at `key`, if any.
    pub fn claim_at(&self, key: &ClaimChunkKey) -> Option<&ChunkClaim> {
        self.claims.get(key)
    }

    /// Every party, in id order.
    pub fn parties(&self) -> &BTreeMap<String, Party> {
        &self.parties
    }

    /// The party `uuid` belongs to, if any.
    pub fn party_of(&self, uuid: &str) -> Option<&Party> {
        self.member_party
            .get(uuid)
            .and_then(|id| self.parties.get(id))
    }

    /// Resolve a party by exact id or case-insensitive name.
    pub fn find_party_id(&self, name_or_id: &str) -> Option<String> {
        if self.parties.contains_key(name_or_id) {
            return Some(name_or_id.to_owned());
        }
        self.parties
            .iter()
            .find(|(_, p)| p.name.eq_ignore_ascii_case(name_or_id))
            .map(|(id, _)| id.clone())
    }

    /// How many chunks `owner_uuid` claims under `sub`.
    pub fn claim_count(&self, owner_uuid: &str, sub: &SubClaimId) -> usize {
        self.index_count(&player_key(owner_uuid), sub.as_str())
    }

    /// How many of those are forceloaded.
    pub fn forceload_count(&self, owner_uuid: &str, sub: &SubClaimId) -> usize {
        self.forceload_index_count(&player_key(owner_uuid), sub.as_str())
    }

    /// OpenPAC's over-limit state: `owner_uuid` holds more `sub` claims than
    /// their current `max_claims` (e.g. after a rank downgrade shrank it).
    /// While true, party/ally access to those claims is suspended.
    pub fn over_limit(&self, owner_uuid: &str, sub: &SubClaimId) -> bool {
        let k = player_key(owner_uuid);
        self.index_count(&k, sub.as_str()) > self.effective_config(&k, sub.as_str()).max_claims
    }

    /// Mark/unmark a UUID as server admin (ops pass this per command; the
    /// set drives `check_action`'s admin bypass and server-claim access).
    pub fn set_admin(&mut self, uuid: &str, is_admin: bool) {
        if is_admin {
            self.admins.insert(uuid.to_owned());
        } else {
            self.admins.remove(uuid);
        }
    }

    pub fn is_admin(&self, uuid: &str) -> bool {
        self.admins.contains(uuid)
    }

    /// The current sync revision (`0` before any mutation).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Undrained deltas, oldest first.
    pub fn pending_deltas(&self) -> &[ClaimsSyncDelta] {
        &self.deltas
    }

    /// Drain the sync queue; the next delta still continues `revision`.
    pub fn drain_deltas(&mut self) -> Vec<ClaimsSyncDelta> {
        std::mem::take(&mut self.deltas)
    }

    /// The pending deltas folded into [`ClaimsWireBatch`]es by
    /// [`crate::claims_wire::build_delta_batches`] — the per-dimension,
    /// revision-ranged payloads the claims sync channels carry. Pure
    /// grouping; encoding is the caller's step through
    /// [`crate::claims_wire::encode_claims_batch`].
    pub fn pending_wire_batches(&self) -> Vec<ClaimsWireBatch> {
        build_delta_batches(self.pending_deltas())
    }

    /// Drain the sync queue and return it as wire batches in one step —
    /// the play loop's per-tick claims-sync flush.
    pub fn drain_wire_batches(&mut self) -> Vec<ClaimsWireBatch> {
        build_delta_batches(&self.drain_deltas())
    }

    /// A full snapshot of one dimension's claims as wire batches, via
    /// [`crate::claims_wire::build_dimension_sync_batches`] — what a client
    /// that just subscribed (or lost the feed) needs first. `max_per_batch`
    /// is clamped to the codec's cap; an invalid dimension yields none.
    pub fn dimension_wire_batches(
        &self,
        dimension: &str,
        max_per_batch: usize,
    ) -> Vec<ClaimsWireBatch> {
        build_dimension_sync_batches(self, dimension, max_per_batch)
    }

    /// Record that `uuid` (display name `name`) was seen at `tick`. The
    /// first sighting fixes `first_seen_tick`; later calls refresh the
    /// name and `last_seen_tick` — deterministic even if callers feed
    /// ticks out of order (min/max merge).
    pub fn record_player_seen(&mut self, uuid: &str, name: &str, tick: u64) {
        match self.known_players.get_mut(uuid) {
            Some(rec) => {
                rec.name = name.to_owned();
                rec.first_seen_tick = rec.first_seen_tick.min(tick);
                rec.last_seen_tick = rec.last_seen_tick.max(tick);
            }
            None => {
                self.known_players.insert(
                    uuid.to_owned(),
                    KnownPlayerRecord {
                        name: name.to_owned(),
                        first_seen_tick: tick,
                        last_seen_tick: tick,
                    },
                );
            }
        }
    }

    /// Every player who has ever joined, in UUID order — the roster a
    /// claim UI lists for 1-click per-player permission editing.
    pub fn known_players(&self) -> &BTreeMap<String, KnownPlayerRecord> {
        &self.known_players
    }

    /// The dimensions players may claim in (`None` = unrestricted).
    pub fn allowed_claim_dimensions(&self) -> Option<&BTreeSet<String>> {
        self.allowed_claim_dimensions.as_ref()
    }

    /// Restrict player claims to `dims` (`Some`) or lift the restriction
    /// (`None`). Dimension strings are normalized like [`ClaimChunkKey`]
    /// ids; invalid input is an error, never silently dropped. Admin
    /// takeovers and server claims ignore this list.
    pub fn set_allowed_claim_dimensions(
        &mut self,
        dims: Option<BTreeSet<String>>,
    ) -> Result<(), ClaimsError> {
        let normalized = dims
            .map(|set| {
                set.iter()
                    .map(|d| {
                        ResourceLocation::parse(d)
                            .map(|r| r.to_string())
                            .map_err(|e| ClaimsError::InvalidDimension(e.to_string()))
                    })
                    .collect::<Result<BTreeSet<String>, _>>()
            })
            .transpose()?;
        self.allowed_claim_dimensions = normalized;
        Ok(())
    }

    /// How player claims are priced.
    pub fn claim_cost_mode(&self) -> ClaimCostMode {
        self.claim_cost_mode
    }

    pub fn set_claim_cost_mode(&mut self, mode: ClaimCostMode) {
        self.claim_cost_mode = mode;
    }

    // ------------------------------------------------- permission matrix

    /// Grant `target_player_uuid` an explicit permission set inside
    /// `owner_uuid`'s `sub` claims — checked before party/ally trust and
    /// `everyone_else`.
    pub fn set_player_permission_override(
        &mut self,
        owner_uuid: &str,
        sub: &SubClaimId,
        target_player_uuid: &str,
        perms: ClaimPermissionSet,
    ) {
        self.protection_config_mut(owner_uuid, sub)
            .player_overrides
            .insert(target_player_uuid.to_owned(), perms);
    }

    /// Remove `target_player_uuid`'s explicit override; party/ally and
    /// `everyone_else` rules apply again.
    pub fn clear_player_permission_override(
        &mut self,
        owner_uuid: &str,
        sub: &SubClaimId,
        target_player_uuid: &str,
    ) {
        self.protection_config_mut(owner_uuid, sub)
            .player_overrides
            .remove(target_player_uuid);
    }

    /// Replace the "everyone else" (`自分以外の全員`) permission row for
    /// `owner_uuid`'s `sub` claims.
    pub fn set_everyone_else_permissions(
        &mut self,
        owner_uuid: &str,
        sub: &SubClaimId,
        perms: ClaimPermissionSet,
    ) {
        self.protection_config_mut(owner_uuid, sub).everyone_else = perms;
    }

    /// The permission set `actor_uuid` effectively holds inside
    /// `owner_uuid`'s `sub` claims: full access for the owner/admins, an
    /// explicit override when one exists, full access for party members
    /// and allies (suspended while the owner is over their claim limit),
    /// else `everyone_else` widened by any permissive `protect_*` flags.
    pub fn effective_permissions_for(
        &self,
        owner_uuid: &str,
        sub: &SubClaimId,
        actor_uuid: &str,
    ) -> ClaimPermissionSet {
        let okey = player_key(owner_uuid);
        let cfg = self.effective_config(&okey, sub.as_str());
        self.perm_set_for(&okey, sub.as_str(), &cfg, actor_uuid)
    }

    /// The minimap overlay color for `owner_uuid`'s `sub` claims:
    /// `display_color_rgb` when set (nonzero), else the party color while
    /// the owner is in a party, else a deterministic owner-hashed color.
    pub fn claim_display_color(&self, owner_uuid: &str, sub: &SubClaimId) -> u32 {
        let cfg = self.effective_config(&player_key(owner_uuid), sub.as_str());
        if cfg.display_color_rgb != 0 {
            cfg.display_color_rgb
        } else {
            self.claim_color(owner_uuid)
        }
    }

    /// The minimap label for `owner_uuid`'s `sub` claims, if any.
    pub fn claim_display_label(&self, owner_uuid: &str, sub: &SubClaimId) -> Option<String> {
        self.effective_config(&player_key(owner_uuid), sub.as_str())
            .display_label
    }

    // -------------------------------------------------------------- config

    /// The fallback policy for owners without a per-owner entry.
    pub fn default_config(&self) -> &ClaimProtectionConfig {
        &self.default_config
    }

    pub fn set_default_config(&mut self, cfg: ClaimProtectionConfig) {
        self.default_config = cfg;
    }

    /// The policy that applies to `owner_uuid`'s `sub` claims: the exact
    /// `(owner, sub)` entry, else the owner's `"main"` entry, else the
    /// default (or [`ClaimProtectionConfig::server`] for server claims).
    pub fn protection_config(&self, owner_uuid: &str, sub: &SubClaimId) -> ClaimProtectionConfig {
        self.effective_config(&player_key(owner_uuid), sub.as_str())
    }

    /// Write an exact `(owner_uuid, sub)` policy entry.
    pub fn set_protection_config(
        &mut self,
        owner_uuid: &str,
        sub: &SubClaimId,
        cfg: ClaimProtectionConfig,
    ) {
        self.configs
            .insert((player_key(owner_uuid), sub.as_str().to_owned()), cfg);
    }

    /// The policy for server claims.
    pub fn server_config(&self) -> ClaimProtectionConfig {
        self.effective_config("server", DEFAULT_SUB_CLAIM)
    }

    pub fn set_server_config(&mut self, cfg: ClaimProtectionConfig) {
        self.configs
            .insert(("server".to_owned(), DEFAULT_SUB_CLAIM.to_owned()), cfg);
    }

    /// Mutable access to `owner_uuid`'s `sub` policy, materializing the
    /// effective value first so the returned reference is always valid.
    pub fn protection_config_mut(
        &mut self,
        owner_uuid: &str,
        sub: &SubClaimId,
    ) -> &mut ClaimProtectionConfig {
        let key = (player_key(owner_uuid), sub.as_str().to_owned());
        if !self.configs.contains_key(&key) {
            let seed = self.effective_config(&key.0, &key.1);
            self.configs.insert(key.clone(), seed);
        }
        self.configs.get_mut(&key).unwrap()
    }

    fn effective_config(&self, okey: &str, sub: &str) -> ClaimProtectionConfig {
        self.configs
            .get(&(okey.to_owned(), sub.to_owned()))
            .or_else(|| {
                if sub == DEFAULT_SUB_CLAIM {
                    None
                } else {
                    self.configs
                        .get(&(okey.to_owned(), DEFAULT_SUB_CLAIM.to_owned()))
                }
            })
            .cloned()
            .unwrap_or_else(|| {
                if okey == "server" {
                    ClaimProtectionConfig::server()
                } else {
                    self.default_config.clone()
                }
            })
    }

    // ------------------------------------------------------ claim mutation

    /// Claim `key` for `actor_uuid` under `sub` (non-admin path).
    pub fn claim_chunk(
        &mut self,
        actor_uuid: &str,
        key: ClaimChunkKey,
        sub: &SubClaimId,
    ) -> Result<(), ClaimsError> {
        self.claim_chunk_as(actor_uuid, false, key, sub)
    }

    /// [`Self::claim_chunk`] with an explicit admin flag: admins may take
    /// over chunks claimed by others (the claim becomes the admin's).
    ///
    /// Re-claiming a chunk the actor already owns under a *different*
    /// sub-claim moves it there (and consumes that sub-claim's quota),
    /// matching OpenPAC's per-sub-config claim assignment.
    pub fn claim_chunk_as(
        &mut self,
        actor_uuid: &str,
        actor_is_admin: bool,
        key: ClaimChunkKey,
        sub: &SubClaimId,
    ) -> Result<(), ClaimsError> {
        let actor_key = player_key(actor_uuid);
        if !actor_is_admin {
            if let Some(dims) = &self.allowed_claim_dimensions {
                if !dims.contains(&key.dimension) {
                    return Err(ClaimsError::DimensionNotClaimable(key.dimension.clone()));
                }
            }
        }
        if let Some(existing) = self.claims.get(&key) {
            let ekey = owner_key(&existing.owner);
            if ekey == actor_key {
                if existing.sub_claim == sub.as_str() {
                    return Err(ClaimsError::AlreadyClaimed);
                }
            } else if !actor_is_admin {
                return Err(ClaimsError::ClaimedByOther);
            }
            let (ekey, old_sub) = (ekey, existing.sub_claim.clone());
            self.check_claim_quota(&actor_key, sub.as_str())?;
            self.index_remove(&ekey, &old_sub, &key);
        } else {
            self.check_claim_quota(&actor_key, sub.as_str())?;
        }
        let claim = ChunkClaim {
            owner: ClaimOwner::Player(actor_uuid.to_owned()),
            sub_claim: sub.as_str().to_owned(),
            forceloaded: false,
            color_rgb: self.claim_color(actor_uuid),
        };
        self.insert_claim(&actor_key, key, claim);
        Ok(())
    }

    /// Admin-created server claim (`/opac claim`). Fails `NotAuthorized`
    /// without the flag; takes over any existing claim.
    pub fn claim_server_chunk(
        &mut self,
        actor_is_admin: bool,
        key: ClaimChunkKey,
        sub: &SubClaimId,
    ) -> Result<(), ClaimsError> {
        if !actor_is_admin {
            return Err(ClaimsError::NotAuthorized);
        }
        if let Some(existing) = self.claims.get(&key) {
            if existing.owner == ClaimOwner::Server && existing.sub_claim == sub.as_str() {
                return Err(ClaimsError::AlreadyClaimed);
            }
            let (ekey, old_sub) = (owner_key(&existing.owner), existing.sub_claim.clone());
            self.index_remove(&ekey, &old_sub, &key);
        }
        let claim = ChunkClaim {
            owner: ClaimOwner::Server,
            sub_claim: sub.as_str().to_owned(),
            forceloaded: false,
            color_rgb: color_for("server"),
        };
        self.insert_claim("server", key, claim);
        Ok(())
    }

    /// Drop the claim at `key` (non-admin path).
    pub fn unclaim_chunk(
        &mut self,
        actor_uuid: &str,
        key: &ClaimChunkKey,
    ) -> Result<(), ClaimsError> {
        self.unclaim_chunk_as(actor_uuid, false, key)
    }

    /// [`Self::unclaim_chunk`] with an explicit admin flag: admins may drop
    /// anyone's claim; server/expired claims *require* it.
    pub fn unclaim_chunk_as(
        &mut self,
        actor_uuid: &str,
        actor_is_admin: bool,
        key: &ClaimChunkKey,
    ) -> Result<(), ClaimsError> {
        let Some(existing) = self.claims.get(key) else {
            return Err(ClaimsError::NotClaimed);
        };
        let ekey = owner_key(&existing.owner);
        match &existing.owner {
            ClaimOwner::Server | ClaimOwner::Expired(_) if !actor_is_admin => {
                return Err(ClaimsError::NotAuthorized)
            }
            ClaimOwner::Player(_) if ekey != player_key(actor_uuid) && !actor_is_admin => {
                return Err(ClaimsError::NotOwner)
            }
            _ => {}
        }
        let (ekey, sub) = (ekey, existing.sub_claim.clone());
        self.index_remove(&ekey, &sub, key);
        self.claims.remove(key);
        self.emit(key.clone(), None);
        Ok(())
    }

    /// Toggle `key`'s forceload ticket (non-admin path).
    pub fn set_forceload(
        &mut self,
        actor_uuid: &str,
        key: &ClaimChunkKey,
        enabled: bool,
    ) -> Result<(), ClaimsError> {
        self.set_forceload_as(actor_uuid, false, key, enabled)
    }

    /// [`Self::set_forceload`] with an explicit admin flag. Enabling is
    /// bounded by `max_forceloads`; disabling is always allowed. Re-setting
    /// the current state is a no-op.
    pub fn set_forceload_as(
        &mut self,
        actor_uuid: &str,
        actor_is_admin: bool,
        key: &ClaimChunkKey,
        enabled: bool,
    ) -> Result<(), ClaimsError> {
        let Some(existing) = self.claims.get(key) else {
            return Err(ClaimsError::NotClaimed);
        };
        let ekey = owner_key(&existing.owner);
        match &existing.owner {
            ClaimOwner::Server | ClaimOwner::Expired(_) if !actor_is_admin => {
                return Err(ClaimsError::NotAuthorized)
            }
            ClaimOwner::Player(_) if ekey != player_key(actor_uuid) && !actor_is_admin => {
                return Err(ClaimsError::NotOwner)
            }
            _ => {}
        }
        if existing.forceloaded == enabled {
            return Ok(());
        }
        if enabled {
            let cfg = self.effective_config(&ekey, &existing.sub_claim);
            let used = self.forceload_index_count(&ekey, &existing.sub_claim);
            if used >= cfg.max_forceloads {
                return Err(ClaimsError::ForceloadLimitReached {
                    used,
                    limit: cfg.max_forceloads,
                });
            }
        }
        let updated = {
            let claim = self.claims.get_mut(key).unwrap();
            claim.forceloaded = enabled;
            claim.clone()
        };
        self.emit(key.clone(), Some(updated));
        Ok(())
    }

    /// Convert every claim of `owner_uuid` to [`ClaimOwner::Expired`] —
    /// OpenPAC's player-data expiry state. Admin-only; returns how many
    /// claims were expired, each emitting a `ClaimUpdated` delta.
    pub fn expire_player_claims(
        &mut self,
        actor_is_admin: bool,
        owner_uuid: &str,
    ) -> Result<usize, ClaimsError> {
        if !actor_is_admin {
            return Err(ClaimsError::NotAuthorized);
        }
        let pkey = player_key(owner_uuid);
        let targets: Vec<ClaimChunkKey> = self
            .claims
            .iter()
            .filter(|(_, c)| owner_key(&c.owner) == pkey)
            .map(|(k, _)| k.clone())
            .collect();
        let ekey = format!("expired:{owner_uuid}");
        for key in &targets {
            let sub = self.claims[key].sub_claim.clone();
            self.index_remove(&pkey, &sub, key);
            let updated = {
                let claim = self.claims.get_mut(key).unwrap();
                claim.owner = ClaimOwner::Expired(owner_uuid.to_owned());
                claim.clone()
            };
            self.index_insert(&ekey, &sub, key.clone());
            self.emit(key.clone(), Some(updated));
        }
        Ok(targets.len())
    }

    /// The chunks whose forceload tickets are *live* right now:
    /// server claims always, a player's claims while they are online, and
    /// offline players' claims only where their config sets
    /// `offline_forceload`. Expired claims never force-load.
    pub fn active_forceloaded_chunks(&self, online_uuids: &BTreeSet<String>) -> Vec<ClaimChunkKey> {
        self.claims
            .iter()
            .filter(|(_, c)| c.forceloaded)
            .filter(|(_, c)| match &c.owner {
                ClaimOwner::Server => true,
                ClaimOwner::Expired(_) => false,
                ClaimOwner::Player(u) => {
                    online_uuids.contains(u)
                        || self
                            .effective_config(&owner_key(&c.owner), &c.sub_claim)
                            .offline_forceload
                }
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    // ---------------------------------------------------- action decisions

    /// The verdict for `action` at `key`. Wilderness is always `Allowed`;
    /// `actor_uuid` is `None` for non-player causes (which get no owner,
    /// party, ally or admin privilege).
    pub fn check_action(
        &self,
        key: &ClaimChunkKey,
        actor_uuid: Option<&str>,
        action: ClaimAction,
    ) -> ClaimDecision {
        // Zero-allocation fast path: an empty store, or no claim at `key`,
        // is wilderness — `Allowed` before any owner key, config clone or
        // permission set is built.
        if self.claims.is_empty() {
            return ClaimDecision::Allowed;
        }
        let Some(claim) = self.claims.get(key) else {
            return ClaimDecision::Allowed;
        };
        let okey = owner_key(&claim.owner);
        let cfg = self.effective_config(&okey, &claim.sub_claim);
        let ctx = ClaimCtx {
            okey: &okey,
            sub: &claim.sub_claim,
            cfg: &cfg,
        };
        if let Some(decision) = self.claim_wide_action(&ctx, &action) {
            return decision;
        }
        let rule = slot_rule(&action, &cfg);
        let actor = actor_uuid.unwrap_or("");
        let is_admin = self.admins.contains(actor);
        match &claim.owner {
            ClaimOwner::Player(owner) => self.player_action(&ctx, owner, actor, rule),
            ClaimOwner::Server | ClaimOwner::Expired(_) => {
                if !rule.protected || cfg.everyone_else.allows(rule.slot) {
                    return ClaimDecision::Allowed;
                }
                let reason = if matches!(&claim.owner, ClaimOwner::Server) {
                    DenyReason::ServerClaim
                } else {
                    DenyReason::ExpiredClaim
                };
                if is_admin {
                    ClaimDecision::Allowed
                } else {
                    ClaimDecision::Denied(reason)
                }
            }
        }
    }

    /// The cross-border and whole-claim grief rules, decided before the
    /// permission matrix; `None` means the matrix decides.
    fn claim_wide_action(&self, ctx: &ClaimCtx, action: &ClaimAction) -> Option<ClaimDecision> {
        let (src, protected, reason) = match action {
            ClaimAction::PistonFrom(src) => (
                src, ctx.cfg.protect_piston_border, DenyReason::PistonBorderProtected,
            ),
            ClaimAction::FluidFrom(src) => (
                src, ctx.cfg.protect_fluid_border, DenyReason::FluidBorderProtected,
            ),
            ClaimAction::FireSpreadFrom(src) | ClaimAction::LightningFrom(src) => (
                src, ctx.cfg.protect_fire_and_lightning, DenyReason::FireAndLightningProtected,
            ),
            ClaimAction::DispenserOrHopperFrom(src) => (
                src,
                ctx.cfg.protect_dispenser_hopper_border,
                DenyReason::DispenserHopperBorderProtected,
            ),
            ClaimAction::MobOrBossGrief { .. } => {
                // Mobs carry no player rights — the `source` chunk is
                // provenance for reporting, not a trust grant, so a boss
                // already inside the claim is covered too.
                return Some(flag_decision(
                    ctx.cfg.protect_mob_and_boss_grief,
                    DenyReason::MobOrBossGriefProtected,
                ));
            }
            ClaimAction::Explosion => {
                return Some(flag_decision(
                    ctx.cfg.protect_explosions,
                    DenyReason::ExplosionsProtected,
                ));
            }
            ClaimAction::ContraptionOrMachineFrom { source, owner_uuid } => {
                let owner = owner_uuid.as_deref();
                return Some(self.contraption_decision(ctx.okey, ctx.cfg, source, owner));
            }
            ClaimAction::ProjectileFrom { source, shooter_uuid } => {
                return Some(self.projectile_decision(ctx, source, shooter_uuid.as_deref()));
            }
            ClaimAction::ExplosionFrom {
                source,
                actor_uuid: cause,
            } => return Some(self.explosion_decision(ctx, source, cause.as_deref())),
            ClaimAction::VehicleEnterOrInject {
                source,
                actor_uuid: driver,
            } => return Some(self.vehicle_decision(ctx, source, driver.as_deref())),
            _ => return None,
        };
        Some(self.border_decision(ctx.okey, src, protected, reason))
    }

    /// The permission-matrix decision for a player-owned claim: owner/admin
    /// first, then an explicit override, then party/ally trust (suspended
    /// while the owner is over quota), then `everyone_else`/the master flag.
    fn player_action(
        &self,
        ctx: &ClaimCtx,
        owner: &str,
        actor: &str,
        rule: SlotRule,
    ) -> ClaimDecision {
        if actor == owner || self.admins.contains(actor) {
            return ClaimDecision::Allowed;
        }
        // An explicit per-player override beats every other rule, so a
        // blocked party member stays blocked and a trusted friend keeps
        // access even where `everyone_else` is denied.
        if let Some(perms) = ctx.cfg.player_overrides.get(actor) {
            return flag_decision(!perms.allows(rule.slot), rule.reason);
        }
        let trusted = (ctx.cfg.allow_party_members && self.shares_party(owner, actor))
            || (ctx.cfg.allow_allies && self.allied(owner, actor));
        if trusted && self.index_count(ctx.okey, ctx.sub) > ctx.cfg.max_claims {
            // Over-limit: trusted access is suspended; outsiders take the
            // everyone_else/flag path below.
            return ClaimDecision::Denied(DenyReason::OwnerOverLimit);
        }
        flag_decision(
            !trusted && rule.protected && !ctx.cfg.everyone_else.allows(rule.slot),
            rule.reason,
        )
    }

    /// The permission set `actor` effectively holds inside `okey`'s `sub`
    /// claims — the single resolver behind
    /// [`ClaimsWorld::effective_permissions_for`] and the actor checks in
    /// the cross-border decisions.
    fn perm_set_for(
        &self,
        okey: &str,
        sub: &str,
        cfg: &ClaimProtectionConfig,
        actor: &str,
    ) -> ClaimPermissionSet {
        let Some(owner) = okey.strip_prefix("player:") else {
            return if self.admins.contains(actor) {
                ClaimPermissionSet::trusted_builder()
            } else {
                cfg.everyone_else.union(outsider_floor(cfg))
            };
        };
        if actor == owner || self.admins.contains(actor) {
            return ClaimPermissionSet::trusted_builder();
        }
        if let Some(perms) = cfg.player_overrides.get(actor) {
            return *perms;
        }
        let trusted = (cfg.allow_party_members && self.shares_party(owner, actor))
            || (cfg.allow_allies && self.allied(owner, actor));
        if trusted && self.index_count(okey, sub) <= cfg.max_claims {
            return ClaimPermissionSet::trusted_builder();
        }
        cfg.everyone_else.union(outsider_floor(cfg))
    }

    /// Cross-border rule: movement is allowed while source and target share
    /// an owner (same sub-claims too), and blocked otherwise when the target
    /// protects that border. A wilderness source is a different owner.
    fn border_decision(
        &self,
        target_okey: &str,
        src: &ClaimChunkKey,
        protected: bool,
        reason: DenyReason,
    ) -> ClaimDecision {
        if self.source_same_owner(target_okey, src) || !protected {
            ClaimDecision::Allowed
        } else {
            ClaimDecision::Denied(reason)
        }
    }

    /// `src`'s claim has the same owner key as the target (wilderness or a
    /// foreign claim does not).
    fn source_same_owner(&self, target_okey: &str, src: &ClaimChunkKey) -> bool {
        self.claims
            .get(src)
            .map(|c| owner_key(&c.owner) == *target_okey)
            .unwrap_or(false)
    }

    /// Modded contraption/quarry rule: same-owner source, or a machine
    /// whose `owner_uuid` is the claim owner, may cross; anything else is
    /// blocked while `protect_contraption_border` holds.
    fn contraption_decision(
        &self,
        target_okey: &str,
        cfg: &ClaimProtectionConfig,
        src: &ClaimChunkKey,
        owner_uuid: Option<&str>,
    ) -> ClaimDecision {
        let own_machine = owner_uuid
            .map(|u| player_key(u) == *target_okey)
            .unwrap_or(false);
        if self.source_same_owner(target_okey, src)
            || own_machine
            || !cfg.protect_contraption_border
        {
            ClaimDecision::Allowed
        } else {
            ClaimDecision::Denied(DenyReason::ContraptionBorderProtected)
        }
    }

    /// Projectile rule: a shot fired from a same-owner chunk always lands;
    /// otherwise the shooter needs `entity_damage` or `block_break`
    /// permission in the target claim while `protect_projectiles_border`
    /// holds.
    fn projectile_decision(
        &self,
        ctx: &ClaimCtx,
        src: &ClaimChunkKey,
        shooter_uuid: Option<&str>,
    ) -> ClaimDecision {
        if self.source_same_owner(ctx.okey, src) || !ctx.cfg.protect_projectiles_border {
            return ClaimDecision::Allowed;
        }
        if let Some(shooter) = shooter_uuid {
            let perms = self.perm_set_for(ctx.okey, ctx.sub, ctx.cfg, shooter);
            if perms.entity_damage || perms.block_break {
                return ClaimDecision::Allowed;
            }
        }
        ClaimDecision::Denied(DenyReason::ProjectilesBorderProtected)
    }

    /// Explosion rule: a blast from a same-owner chunk (or one the
    /// credited actor could have caused there by hand — they hold
    /// `entity_damage`/`block_break` permission) is allowed; anything else
    /// is blocked while `protect_explosions` holds.
    fn explosion_decision(
        &self,
        ctx: &ClaimCtx,
        src: &ClaimChunkKey,
        actor_uuid: Option<&str>,
    ) -> ClaimDecision {
        if self.source_same_owner(ctx.okey, src) || !ctx.cfg.protect_explosions {
            return ClaimDecision::Allowed;
        }
        if let Some(actor) = actor_uuid {
            let perms = self.perm_set_for(ctx.okey, ctx.sub, ctx.cfg, actor);
            if perms.entity_damage || perms.block_break {
                return ClaimDecision::Allowed;
            }
        }
        ClaimDecision::Denied(DenyReason::ExplosionsProtected)
    }

    /// Vehicle rule: a boat/minecart/modded vehicle may enter from a
    /// same-owner chunk, or when the moving actor holds `entity_interact`
    /// permission (owner, trusted, overridden or explicitly allowed in
    /// `everyone_else`); anything else is blocked while
    /// `protect_vehicle_border` holds.
    fn vehicle_decision(
        &self,
        ctx: &ClaimCtx,
        src: &ClaimChunkKey,
        actor_uuid: Option<&str>,
    ) -> ClaimDecision {
        if self.source_same_owner(ctx.okey, src) || !ctx.cfg.protect_vehicle_border {
            return ClaimDecision::Allowed;
        }
        if let Some(actor) = actor_uuid {
            if self.perm_set_for(ctx.okey, ctx.sub, ctx.cfg, actor).entity_interact {
                return ClaimDecision::Allowed;
            }
        }
        ClaimDecision::Denied(DenyReason::VehicleBorderProtected)
    }

    // ------------------------------------------------------------ parties

    /// Create a party owned by `owner_uuid`; returns the new party id
    /// (`party-N`, deterministic). A player belongs to at most one party.
    pub fn create_party(&mut self, owner_uuid: &str, name: &str) -> Result<String, ClaimsError> {
        let name = name.trim();
        if name.is_empty() || name.len() > 48 {
            return Err(ClaimsError::InvalidPartyName);
        }
        if self.member_party.contains_key(owner_uuid) {
            return Err(ClaimsError::AlreadyInParty);
        }
        let id = format!("party-{}", self.next_party_id);
        self.next_party_id += 1;
        let mut members = BTreeMap::new();
        members.insert(owner_uuid.to_owned(), PartyRank::Owner);
        let party = Party {
            id: id.clone(),
            name: name.to_owned(),
            owner_uuid: owner_uuid.to_owned(),
            members,
            allies: BTreeSet::new(),
            pending_invites: BTreeSet::new(),
            color_rgb: color_for(&id),
        };
        self.parties.insert(id.clone(), party);
        self.member_party.insert(owner_uuid.to_owned(), id.clone());
        Ok(id)
    }

    /// Invite `target_uuid` to the actor's party; requires officer rank.
    pub fn invite_player(
        &mut self,
        actor_uuid: &str,
        target_uuid: &str,
    ) -> Result<(), ClaimsError> {
        let pid = self.require_officer(actor_uuid)?;
        if self.member_party.contains_key(target_uuid) {
            return Err(ClaimsError::AlreadyInParty);
        }
        let party = self.parties.get_mut(&pid).unwrap();
        if !party.pending_invites.insert(target_uuid.to_owned()) {
            return Err(ClaimsError::AlreadyInvited);
        }
        Ok(())
    }

    /// Accept an invite. `party_id` pins which party when the player holds
    /// several invites; `None` accepts the lowest-id pending one.
    pub fn accept_invite(
        &mut self,
        player_uuid: &str,
        party_id: Option<&str>,
    ) -> Result<String, ClaimsError> {
        if self.member_party.contains_key(player_uuid) {
            return Err(ClaimsError::AlreadyInParty);
        }
        let pid = match party_id {
            Some(id) => {
                let id = self.find_party_id(id).ok_or(ClaimsError::PartyNotFound)?;
                if !self
                    .parties
                    .get(&id)
                    .map(|p| p.pending_invites.contains(player_uuid))
                    .unwrap_or(false)
                {
                    return Err(ClaimsError::NotInvited);
                }
                id
            }
            None => self
                .parties
                .iter()
                .find(|(_, p)| p.pending_invites.contains(player_uuid))
                .map(|(id, _)| id.clone())
                .ok_or(ClaimsError::NotInvited)?,
        };
        let party = self.parties.get_mut(&pid).unwrap();
        party.pending_invites.remove(player_uuid);
        party
            .members
            .insert(player_uuid.to_owned(), PartyRank::Member);
        self.member_party
            .insert(player_uuid.to_owned(), pid.clone());
        Ok(pid)
    }

    /// Leave the party. The leaver keeps every chunk they personally own —
    /// parties are an umbrella over individual ownership, never a merger —
    /// and instantly loses party access to the remaining members' claims.
    ///
    /// When the leader leaves with members remaining, ownership of the
    /// party itself passes to the highest-ranked remaining member (ties
    /// broken by lowest UUID, deterministically); when the last member
    /// leaves, the party dissolves and pending invites die with it.
    pub fn leave_party(&mut self, player_uuid: &str) -> Result<(), ClaimsError> {
        let pid = self
            .member_party
            .get(player_uuid)
            .cloned()
            .ok_or(ClaimsError::NotInParty)?;
        let (is_empty, successor) = {
            let party = self.parties.get_mut(&pid).unwrap();
            party.members.remove(player_uuid);
            let successor = if party.owner_uuid == player_uuid && !party.members.is_empty() {
                party
                    .members
                    .iter()
                    .max_by(|(ua, ra), (ub, rb)| {
                        ra.level().cmp(&rb.level()).then(ub.cmp(ua))
                    })
                    .map(|(u, _)| u.clone())
            } else {
                None
            };
            (party.members.is_empty(), successor)
        };
        self.member_party.remove(player_uuid);
        if is_empty {
            self.parties.remove(&pid);
        } else if let Some(next) = successor {
            let party = self.parties.get_mut(&pid).unwrap();
            party.owner_uuid = next.clone();
            party.members.insert(next, PartyRank::Owner);
        }
        Ok(())
    }

    /// Remove `target_uuid` from the actor's party: the actor must hold
    /// officer rank *and* outrank the target (so the owner is un-kickable).
    pub fn kick_member(&mut self, actor_uuid: &str, target_uuid: &str) -> Result<(), ClaimsError> {
        if actor_uuid == target_uuid {
            return Err(ClaimsError::InvalidTarget);
        }
        let pid = self.require_officer(actor_uuid)?;
        let party = self.parties.get(&pid).unwrap();
        let (actor_rank, target_rank) = (
            party.rank_of(actor_uuid).unwrap(),
            party
                .rank_of(target_uuid)
                .ok_or(ClaimsError::InvalidTarget)?,
        );
        // Must strictly outrank the target, so the owner is un-kickable.
        if actor_rank.level() <= target_rank.level() {
            return Err(ClaimsError::InsufficientRank);
        }
        self.parties
            .get_mut(&pid)
            .unwrap()
            .members
            .remove(target_uuid);
        self.member_party.remove(target_uuid);
        Ok(())
    }

    /// Owner-only rank assignment; `Owner` cannot be granted this way.
    pub fn set_rank(
        &mut self,
        actor_uuid: &str,
        target_uuid: &str,
        rank: PartyRank,
    ) -> Result<(), ClaimsError> {
        if rank == PartyRank::Owner || actor_uuid == target_uuid {
            return Err(ClaimsError::InvalidTarget);
        }
        let pid = self
            .member_party
            .get(actor_uuid)
            .cloned()
            .ok_or(ClaimsError::NotInParty)?;
        let party = self.parties.get_mut(&pid).unwrap();
        if party.rank_of(actor_uuid) != Some(PartyRank::Owner) {
            return Err(ClaimsError::InsufficientRank);
        }
        match party.members.get_mut(target_uuid) {
            Some(r) => {
                *r = rank;
                Ok(())
            }
            None => Err(ClaimsError::InvalidTarget),
        }
    }

    /// Declare `other_party` an ally of the actor's party (requires Admin).
    /// The declaration is directed; protection checks it symmetrically.
    pub fn add_ally(&mut self, actor_uuid: &str, other_party: &str) -> Result<(), ClaimsError> {
        let pid = self.require_rank(actor_uuid, PartyRank::Admin)?;
        let other = self
            .find_party_id(other_party)
            .ok_or(ClaimsError::PartyNotFound)?;
        if other == pid {
            return Err(ClaimsError::InvalidTarget);
        }
        if !self.parties.get_mut(&pid).unwrap().allies.insert(other) {
            return Err(ClaimsError::AlreadyAllied);
        }
        Ok(())
    }

    /// Drop the actor's party's alliance with `other_party`.
    pub fn remove_ally(&mut self, actor_uuid: &str, other_party: &str) -> Result<(), ClaimsError> {
        let pid = self.require_rank(actor_uuid, PartyRank::Admin)?;
        let other = self
            .find_party_id(other_party)
            .ok_or(ClaimsError::PartyNotFound)?;
        if !self.parties.get_mut(&pid).unwrap().allies.remove(&other) {
            return Err(ClaimsError::NotAllied);
        }
        Ok(())
    }

    /// `/opm` recipients: the sender's party members who are online, sender
    /// excluded, sorted by UUID. Empty when the sender is in no party.
    pub fn party_chat_recipients(
        &self,
        sender_uuid: &str,
        online_uuids: &BTreeSet<String>,
    ) -> Vec<String> {
        self.party_of(sender_uuid)
            .map(|p| {
                p.members
                    .keys()
                    .filter(|u| u.as_str() != sender_uuid && online_uuids.contains(u.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    // ------------------------------------------------------------- helpers

    fn require_officer(&self, actor_uuid: &str) -> Result<String, ClaimsError> {
        self.require_rank(actor_uuid, PARTY_OFFICER)
    }

    fn require_rank(&self, actor_uuid: &str, min: PartyRank) -> Result<String, ClaimsError> {
        let pid = self
            .member_party
            .get(actor_uuid)
            .cloned()
            .ok_or(ClaimsError::NotInParty)?;
        let rank = self.parties[&pid].rank_of(actor_uuid).unwrap();
        if !rank.at_least(min) {
            return Err(ClaimsError::InsufficientRank);
        }
        Ok(pid)
    }

    fn shares_party(&self, a: &str, b: &str) -> bool {
        match (self.member_party.get(a), self.member_party.get(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }

    /// Ally relation between `owner`'s and `actor`'s parties, honored in
    /// either direction: an alliance either party declared counts.
    fn allied(&self, owner: &str, actor: &str) -> bool {
        let (Some(p1), Some(p2)) = (self.member_party.get(owner), self.member_party.get(actor))
        else {
            return false;
        };
        if p1 == p2 {
            return false;
        }
        self.parties
            .get(p1)
            .map(|p| p.allies.contains(p2))
            .unwrap_or(false)
            || self
                .parties
                .get(p2)
                .map(|p| p.allies.contains(p1))
                .unwrap_or(false)
    }

    fn check_claim_quota(&self, okey: &str, sub: &str) -> Result<(), ClaimsError> {
        let cfg = self.effective_config(okey, sub);
        let used = self.index_count(okey, sub);
        if used >= cfg.max_claims {
            return Err(ClaimsError::ClaimLimitReached {
                used,
                limit: cfg.max_claims,
            });
        }
        Ok(())
    }

    fn index_count(&self, okey: &str, sub: &str) -> usize {
        self.index
            .get(&(okey.to_owned(), sub.to_owned()))
            .map(|s| s.len())
            .unwrap_or(0)
    }

    fn forceload_index_count(&self, okey: &str, sub: &str) -> usize {
        self.index
            .get(&(okey.to_owned(), sub.to_owned()))
            .map(|s| s.iter().filter(|k| self.claims[*k].forceloaded).count())
            .unwrap_or(0)
    }

    fn index_insert(&mut self, okey: &str, sub: &str, key: ClaimChunkKey) {
        self.index
            .entry((okey.to_owned(), sub.to_owned()))
            .or_default()
            .insert(key);
    }

    fn index_remove(&mut self, okey: &str, sub: &str, key: &ClaimChunkKey) {
        let ikey = (okey.to_owned(), sub.to_owned());
        let empty = if let Some(set) = self.index.get_mut(&ikey) {
            set.remove(key);
            set.is_empty()
        } else {
            false
        };
        if empty {
            self.index.remove(&ikey);
        }
    }

    /// Insert `claim` under `okey` and emit the `ClaimUpdated` delta.
    fn insert_claim(&mut self, okey: &str, key: ClaimChunkKey, claim: ChunkClaim) {
        self.index_insert(okey, &claim.sub_claim.clone(), key.clone());
        self.claims.insert(key.clone(), claim.clone());
        self.emit(key, Some(claim));
    }

    /// Claims take the owner's party color when they are in a party, else a
    /// stable per-uuid color — like OpenPAC coloring claims by party.
    fn claim_color(&self, owner_uuid: &str) -> u32 {
        self.party_of(owner_uuid)
            .map(|p| p.color_rgb)
            .unwrap_or_else(|| color_for(owner_uuid))
    }

    fn emit(&mut self, key: ClaimChunkKey, claim: Option<ChunkClaim>) {
        self.revision += 1;
        self.deltas.push(ClaimsSyncDelta::ClaimUpdated {
            revision: self.revision,
            key,
            claim,
        });
    }

    // ---------------------------------------------------------- persistence

    /// Atomically write `openpac_claims.json` and `openpac_parties.json`
    /// under `dir` (created if absent).
    pub fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        write_atomic(&dir.join(CLAIMS_FILE), self.claims_json().as_bytes())?;
        write_atomic(&dir.join(PARTIES_FILE), self.parties_json().as_bytes())?;
        Ok(())
    }

    /// Read both files back; a missing file is an empty store, malformed or
    /// mistyped content is an `InvalidData` error (fail closed — a half-read
    /// claims database is a protection bypass, not a repair).
    pub fn load_from_dir(dir: &Path) -> io::Result<Self> {
        let mut world = Self::new();
        match std::fs::read_to_string(dir.join(CLAIMS_FILE)) {
            Ok(text) => world.read_claims_json(&text)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        match std::fs::read_to_string(dir.join(PARTIES_FILE)) {
            Ok(text) => world.read_parties_json(&text)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        world.rebuild_indexes()?;
        Ok(world)
    }

    fn claims_json(&self) -> String {
        let claims: Vec<Json> = self
            .claims
            .iter()
            .map(|(k, c)| {
                Json::Object(vec![
                    str_json("dim", &k.dimension),
                    num_json("cx", f64::from(k.cx)),
                    num_json("cz", f64::from(k.cz)),
                    ("owner".to_owned(), owner_to_json(&c.owner)),
                    str_json("sub", &c.sub_claim),
                    bool_json("forceloaded", c.forceloaded),
                    num_json("color", f64::from(c.color_rgb)),
                ])
            })
            .collect();
        let configs: Vec<Json> = self
            .configs
            .iter()
            .map(|((okey, sub), cfg)| {
                Json::Object(vec![
                    str_json("owner", okey),
                    str_json("sub", sub),
                    ("config".to_owned(), config_to_json(cfg)),
                ])
            })
            .collect();
        let admins = self.admins.iter().map(|a| Json::String(a.clone())).collect();
        let known_players: Vec<Json> = self
            .known_players
            .iter()
            .map(|(u, r)| {
                Json::Object(vec![
                    str_json("uuid", u),
                    str_json("name", &r.name),
                    num_json("firstSeenTick", r.first_seen_tick as f64),
                    num_json("lastSeenTick", r.last_seen_tick as f64),
                ])
            })
            .collect();
        let mut doc = vec![
            num_json("revision", self.revision as f64),
            num_json("nextPartyId", self.next_party_id as f64),
            ("admins".to_owned(), Json::Array(admins)),
            str_json("claimCostMode", self.claim_cost_mode.as_str()),
            (
                "defaultConfig".to_owned(),
                config_to_json(&self.default_config),
            ),
            ("configs".to_owned(), Json::Array(configs)),
            ("claims".to_owned(), Json::Array(claims)),
            ("knownPlayers".to_owned(), Json::Array(known_players)),
        ];
        if let Some(dims) = &self.allowed_claim_dimensions {
            let dims = Json::Array(dims.iter().map(|d| Json::String(d.clone())).collect());
            doc.push(("allowedClaimDimensions".to_owned(), dims));
        }
        Json::Object(doc).to_json_string()
    }

    fn parties_json(&self) -> String {
        let parties: Vec<Json> = self
            .parties
            .values()
            .map(|p| {
                Json::Object(vec![
                    str_json("id", &p.id),
                    str_json("name", &p.name),
                    str_json("ownerUuid", &p.owner_uuid),
                    (
                        "members".to_owned(),
                        Json::Object(
                            p.members
                                .iter()
                                .map(|(u, r)| (u.clone(), Json::String(r.as_str().to_owned())))
                                .collect(),
                        ),
                    ),
                    (
                        "allies".to_owned(),
                        Json::Array(p.allies.iter().map(|a| Json::String(a.clone())).collect()),
                    ),
                    (
                        "invites".to_owned(),
                        Json::Array(
                            p.pending_invites
                                .iter()
                                .map(|a| Json::String(a.clone()))
                                .collect(),
                        ),
                    ),
                    num_json("color", f64::from(p.color_rgb)),
                ])
            })
            .collect();
        Json::Object(vec![("parties".to_owned(), Json::Array(parties))]).to_json_string()
    }

    fn read_claims_json(&mut self, text: &str) -> io::Result<()> {
        let ctx = CLAIMS_FILE;
        let doc = parse_doc(text, ctx)?;
        let Json::Object(_) = doc else {
            return Err(inv(ctx, "expected a JSON object"));
        };
        self.read_claims_header(&doc, ctx)?;
        self.read_known_players_json(&doc, ctx)?;
        self.admins
            .extend(json_str_set(&doc, "admins", ctx, "\"admins\"")?);
        self.read_configs_json(&doc, ctx)?;
        self.read_claim_entries_json(&doc, ctx)
    }

    /// Store-wide scalars and policies: revision, party counter, default
    /// config, cost mode and the claimable-dimension allowlist.
    fn read_claims_header(&mut self, doc: &Json, ctx: &str) -> io::Result<()> {
        self.revision = j_u64(doc, "revision", ctx)?.unwrap_or(0);
        self.next_party_id = j_u64(doc, "nextPartyId", ctx)?.unwrap_or(1).max(1);
        if let Some(d) = doc.get("defaultConfig") {
            self.default_config = config_from_json(d, ctx)?;
        }
        match doc.get("claimCostMode") {
            None | Some(Json::Null) => {}
            Some(Json::String(s)) => {
                self.claim_cost_mode = ClaimCostMode::parse(s)
                    .ok_or_else(|| inv(ctx, format!("bad \"claimCostMode\" {s:?}")))?;
            }
            Some(_) => return Err(inv(ctx, "\"claimCostMode\" must be a string")),
        }
        match doc.get("allowedClaimDimensions") {
            None | Some(Json::Null) => {}
            Some(Json::Array(items)) => {
                self.allowed_claim_dimensions = Some(parse_claim_dimensions(items, ctx)?);
            }
            Some(_) => return Err(inv(ctx, "\"allowedClaimDimensions\" must be an array")),
        }
        Ok(())
    }

    /// The roster of every player the server has ever seen.
    fn read_known_players_json(&mut self, doc: &Json, ctx: &str) -> io::Result<()> {
        for item in j_arr(doc, "knownPlayers", ctx)?.unwrap_or(&[]) {
            let uuid = j_str(item, "uuid", ctx)?.to_owned();
            let name = j_str(item, "name", ctx)?.to_owned();
            let rec = KnownPlayerRecord {
                name,
                first_seen_tick: j_u64(item, "firstSeenTick", ctx)?.unwrap_or(0),
                last_seen_tick: j_u64(item, "lastSeenTick", ctx)?.unwrap_or(0),
            };
            self.known_players.insert(uuid, rec);
        }
        Ok(())
    }

    /// Per-`(owner, sub-claim)` policies; the owner key is validated before
    /// anything else so a typo cannot silently seed a config under it.
    fn read_configs_json(&mut self, doc: &Json, ctx: &str) -> io::Result<()> {
        for entry in j_arr(doc, "configs", ctx)?.unwrap_or(&[]) {
            let okey = j_str(entry, "owner", ctx)?;
            if !is_owner_key(okey) {
                return Err(inv(ctx, format!("bad config owner key {okey:?}")));
            }
            let sub = SubClaimId::parse(j_str(entry, "sub", ctx)?)
                .map_err(|e| inv(ctx, e.to_string()))?;
            let cfg = config_from_json(
                entry
                    .get("config")
                    .ok_or_else(|| inv(ctx, "config entry lacks \"config\""))?,
                ctx,
            )?;
            self.configs
                .insert((okey.to_owned(), sub.as_str().to_owned()), cfg);
        }
        Ok(())
    }

    /// The claimed chunks themselves; a duplicate key is structural damage
    /// (one of the two rows would silently win), so it fails closed.
    fn read_claim_entries_json(&mut self, doc: &Json, ctx: &str) -> io::Result<()> {
        for item in j_arr(doc, "claims", ctx)?.unwrap_or(&[]) {
            let key = ClaimChunkKey::new(
                j_str(item, "dim", ctx)?,
                j_i32(item, "cx", ctx)?,
                j_i32(item, "cz", ctx)?,
            )
            .map_err(|e| inv(ctx, e.to_string()))?;
            let owner = owner_from_json(
                item.get("owner")
                    .ok_or_else(|| inv(ctx, "claim lacks \"owner\""))?,
                ctx,
            )?;
            let sub =
                SubClaimId::parse(j_str(item, "sub", ctx)?).map_err(|e| inv(ctx, e.to_string()))?;
            let forceloaded = j_bool(item, "forceloaded", ctx)?.unwrap_or(false);
            let color = j_u64(item, "color", ctx)?.unwrap_or(0);
            if color > 0x00ff_ffff {
                return Err(inv(ctx, "claim \"color\" out of range"));
            }
            let claim = ChunkClaim {
                owner,
                sub_claim: sub.as_str().to_owned(),
                forceloaded,
                color_rgb: color as u32,
            };
            if self.claims.insert(key.clone(), claim).is_some() {
                return Err(inv(ctx, format!("duplicate claim at {key}")));
            }
        }
        Ok(())
    }

    fn read_parties_json(&mut self, text: &str) -> io::Result<()> {
        let ctx = PARTIES_FILE;
        let doc = parse_doc(text, ctx)?;
        let Json::Object(_) = doc else {
            return Err(inv(ctx, "expected a JSON object"));
        };
        for item in j_arr(&doc, "parties", ctx)?.unwrap_or(&[]) {
            let party = party_from_json(item, ctx)?;
            let id = party.id.clone();
            if self.parties.insert(id.clone(), party).is_some() {
                return Err(inv(ctx, format!("duplicate party id {id:?}")));
            }
            // Keep `next_party_id` ahead of every `party-N` we load so a
            // post-load create cannot collide with an existing id.
            if let Some(n) = id
                .strip_prefix("party-")
                .and_then(|s| s.parse::<u64>().ok())
            {
                self.next_party_id = self.next_party_id.max(n + 1);
            }
        }
        Ok(())
    }

    /// Rebuild `index` and `member_party` from the loaded maps; load-time
    /// structural errors (a player in two parties) surface here.
    fn rebuild_indexes(&mut self) -> io::Result<()> {
        let entries: Vec<(String, String, ClaimChunkKey)> = self
            .claims
            .iter()
            .map(|(key, claim)| {
                (
                    owner_key(&claim.owner),
                    claim.sub_claim.clone(),
                    key.clone(),
                )
            })
            .collect();
        for (okey, sub, key) in entries {
            self.index_insert(&okey, &sub, key);
        }
        for (pid, party) in &self.parties {
            for uuid in party.members.keys() {
                if self
                    .member_party
                    .insert(uuid.clone(), pid.clone())
                    .is_some()
                {
                    return Err(inv(
                        PARTIES_FILE,
                        format!("player {uuid:?} is a member of two parties"),
                    ));
                }
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------ command exec

/// Run one OpenPAC-family command line and return its chat reply.
///
/// `line` may be written with or without the leading `/`. `current_chunk` is
/// the chunk the actor is standing in — chunk commands fail without it.
/// `is_admin` is synced into [`ClaimsWorld::set_admin`] so `check_action`
/// sees the same privilege the dispatcher used.
pub fn exec_openpac_command(
    world: &mut ClaimsWorld,
    actor_uuid: &str,
    actor_name: &str,
    is_admin: bool,
    current_chunk: Option<&ClaimChunkKey>,
    line: &str,
) -> Result<String, String> {
    world.set_admin(actor_uuid, is_admin);
    let trimmed = line.trim().trim_start_matches('/');
    let (root, rest) = match trimmed.find(char::is_whitespace) {
        Some(i) => (&trimmed[..i], trimmed[i..].trim()),
        None => (trimmed, ""),
    };
    match root.to_ascii_lowercase().as_str() {
        "oclaims" | "claims" => cmd_claims(world, actor_uuid, is_admin, current_chunk, rest),
        "oparties" | "parties" => cmd_parties(world, actor_uuid, actor_name, rest),
        "opac" => cmd_opac(world, actor_uuid, is_admin, current_chunk, rest),
        "opm" => cmd_opm(world, actor_uuid, actor_name, rest),
        "" => Err("empty command".to_owned()),
        other => Err(format!("unknown command /{other}")),
    }
}

fn need_chunk(current: Option<&ClaimChunkKey>) -> Result<ClaimChunkKey, String> {
    current
        .cloned()
        .ok_or_else(|| "no chunk context".to_owned())
}

fn cmd_claims(
    world: &mut ClaimsWorld,
    actor_uuid: &str,
    is_admin: bool,
    current_chunk: Option<&ClaimChunkKey>,
    rest: &str,
) -> Result<String, String> {
    let mut t = rest.split_whitespace();
    let sub = t.next().unwrap_or("info").to_ascii_lowercase();
    let fail = |e: ClaimsError| e.to_string();
    match sub.as_str() {
        "claim" => {
            let key = need_chunk(current_chunk)?;
            let sid = SubClaimId::parse(t.next().unwrap_or(DEFAULT_SUB_CLAIM))
                .map_err(|e| e.to_string())?;
            world
                .claim_chunk_as(actor_uuid, is_admin, key.clone(), &sid)
                .map_err(fail)?;
            Ok(format!("claimed {key} under sub-claim {sid}"))
        }
        "unclaim" => {
            let key = need_chunk(current_chunk)?;
            world
                .unclaim_chunk_as(actor_uuid, is_admin, &key)
                .map_err(fail)?;
            Ok(format!("unclaimed {key}"))
        }
        "forceload" | "unforceload" => {
            let key = need_chunk(current_chunk)?;
            let on = sub == "forceload";
            world
                .set_forceload_as(actor_uuid, is_admin, &key, on)
                .map_err(fail)?;
            Ok(format!("{key} forceloaded={on}"))
        }
        "info" => {
            let key = need_chunk(current_chunk)?;
            Ok(claim_info(world, &key))
        }
        other => Err(format!(
            "unknown /oclaims sub-command {other:?}; use claim|unclaim|forceload|unforceload|info"
        )),
    }
}

fn claim_info(world: &ClaimsWorld, key: &ClaimChunkKey) -> String {
    match world.claim_at(key) {
        None => format!("wilderness at {key}"),
        Some(c) => {
            let okey = owner_key(&c.owner);
            let (used, max) = (
                world.index_count(&okey, &c.sub_claim),
                world.effective_config(&okey, &c.sub_claim).max_claims,
            );
            format!(
                "{key}: owner={okey} sub={} forceloaded={} color=#{:06x} claims={}/{}",
                c.sub_claim, c.forceloaded, c.color_rgb, used, max,
            )
        }
    }
}

fn cmd_parties(
    world: &mut ClaimsWorld,
    actor_uuid: &str,
    actor_name: &str,
    rest: &str,
) -> Result<String, String> {
    let mut t = rest.split_whitespace();
    let sub = t.next().unwrap_or("info").to_ascii_lowercase();
    let tail = t.clone().collect::<Vec<_>>().join(" ");
    let fail = |e: ClaimsError| e.to_string();
    match sub.as_str() {
        "create" => {
            let name = if tail.is_empty() {
                format!("{actor_name}'s Party")
            } else {
                tail
            };
            let id = world.create_party(actor_uuid, &name).map_err(fail)?;
            Ok(format!("created party {id}"))
        }
        "invite" => {
            let target = t.next().ok_or("usage: /oparties invite <uuid>")?;
            world.invite_player(actor_uuid, target).map_err(fail)?;
            Ok(format!("invited {target}"))
        }
        "accept" => {
            let pid = world
                .accept_invite(actor_uuid, t.next())
                .map_err(fail)?;
            Ok(format!("joined {pid}"))
        }
        "leave" => {
            world.leave_party(actor_uuid).map_err(fail)?;
            Ok("left the party".to_owned())
        }
        "kick" => {
            let target = t.next().ok_or("usage: /oparties kick <uuid>")?;
            world.kick_member(actor_uuid, target).map_err(fail)?;
            Ok(format!("kicked {target}"))
        }
        "ally" => {
            let other = t.next().ok_or("usage: /oparties ally <party>")?;
            world.add_ally(actor_uuid, other).map_err(fail)?;
            Ok(format!("allied with {other}"))
        }
        "unally" => {
            let other = t.next().ok_or("usage: /oparties unally <party>")?;
            world.remove_ally(actor_uuid, other).map_err(fail)?;
            Ok(format!("no longer allied with {other}"))
        }
        "info" => {
            let Some(p) = world.party_of(actor_uuid) else {
                return Err("not in a party".to_owned());
            };
            let members: Vec<String> = p
                .members
                .iter()
                .map(|(u, r)| format!("{u}={}", r.as_str()))
                .collect();
            let list = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(",");
            Ok(format!(
                "party {} \"{}\" color=#{:06x}: members=[{}] allies=[{}] invites=[{}]",
                p.id,
                p.name,
                p.color_rgb,
                members.join(","),
                list(&p.allies),
                list(&p.pending_invites),
            ))
        }
        other => Err(format!(
            "unknown /oparties sub-command {other:?}; use create|invite|accept|leave|kick|ally|unally|info"
        )),
    }
}

fn cmd_opac(
    world: &mut ClaimsWorld,
    actor_uuid: &str,
    is_admin: bool,
    current_chunk: Option<&ClaimChunkKey>,
    rest: &str,
) -> Result<String, String> {
    let mut t = rest.split_whitespace();
    let sub = t.next().unwrap_or("config").to_ascii_lowercase();
    let fail = |e: ClaimsError| e.to_string();
    match sub.as_str() {
        // Admin server-claim commands, the OpenPAC `/openpac server-claims`
        // equivalent.
        "claim" => {
            let key = need_chunk(current_chunk)?;
            let sid = SubClaimId::parse(t.next().unwrap_or(DEFAULT_SUB_CLAIM))
                .map_err(|e| e.to_string())?;
            world
                .claim_server_chunk(is_admin, key.clone(), &sid)
                .map_err(fail)?;
            Ok(format!("server-claimed {key}"))
        }
        "unclaim" => {
            let key = need_chunk(current_chunk)?;
            world
                .unclaim_chunk_as(actor_uuid, is_admin, &key)
                .map_err(fail)?;
            Ok(format!("unclaimed {key}"))
        }
        "config" => {
            let sid = SubClaimId::main();
            match (t.next(), t.next()) {
                (None, _) => Ok(config_dump(&world.protection_config(actor_uuid, &sid))),
                (Some(k), Some(v)) => {
                    let cfg = world.protection_config_mut(actor_uuid, &sid);
                    config_set(cfg, k, v)?;
                    Ok(format!("{k} set; {}", config_dump(cfg)))
                }
                (Some(k), None) => Err(format!("usage: /opac config [{k} <value>]")),
            }
        }
        other => Err(format!(
            "unknown /opac sub-command {other:?}; use claim|unclaim|config"
        )),
    }
}

fn cmd_opm(
    world: &mut ClaimsWorld,
    actor_uuid: &str,
    actor_name: &str,
    rest: &str,
) -> Result<String, String> {
    if rest.is_empty() {
        return Err("usage: /opm <message>".to_owned());
    }
    // No online set is supplied at the command layer, so recipients are the
    // full member list; the adapter intersects with `online_uuids` through
    // `party_chat_recipients` before delivery.
    let party = world
        .party_of(actor_uuid)
        .ok_or_else(|| "not in a party".to_owned())?;
    let recipients = party
        .members
        .keys()
        .filter(|u| u.as_str() != actor_uuid)
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!("[party] {actor_name}: {rest} (recipients: {recipients})"))
}

/// `key=value` dump, in declaration order.
fn config_dump(c: &ClaimProtectionConfig) -> String {
    format!(
        "protectBlocks={} protectContainers={} protectEntities={} protectExplosions={} \
         protectPistonBorder={} protectFluidBorder={} protectEntityInteract={} \
         protectProjectilesBorder={} protectFireAndLightning={} protectMobAndBossGrief={} \
         protectContraptionBorder={} protectDispenserHopperBorder={} protectVehicleBorder={} \
         protectEntityPush={} protectEntityLure={} allowPartyMembers={} allowAllies={} \
         offlineForceload={} maxClaims={} maxForceloads={}",
        c.protect_blocks,
        c.protect_containers,
        c.protect_entities,
        c.protect_explosions,
        c.protect_piston_border,
        c.protect_fluid_border,
        c.protect_entity_interact,
        c.protect_projectiles_border,
        c.protect_fire_and_lightning,
        c.protect_mob_and_boss_grief,
        c.protect_contraption_border,
        c.protect_dispenser_hopper_border,
        c.protect_vehicle_border,
        c.protect_entity_push,
        c.protect_entity_lure,
        c.allow_party_members,
        c.allow_allies,
        c.offline_forceload,
        c.max_claims,
        c.max_forceloads,
    )
}

/// Apply `key value` to `cfg`; keys match case- and punctuation-insensitively
/// (`protectBlocks`, `protect-blocks`, `protect_blocks` all work).
fn config_set(cfg: &mut ClaimProtectionConfig, key: &str, value: &str) -> Result<(), String> {
    let norm: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    let boolean = |v: &str| -> Result<bool, String> {
        match v.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => Ok(true),
            "false" | "off" | "no" | "0" => Ok(false),
            _ => Err(format!("{v:?} is not a boolean")),
        }
    };
    let number = |v: &str| -> Result<usize, String> {
        v.parse::<usize>()
            .map_err(|_| format!("{v:?} is not a non-negative integer"))
    };
    match norm.as_str() {
        "protectblocks" => cfg.protect_blocks = boolean(value)?,
        "protectcontainers" => cfg.protect_containers = boolean(value)?,
        "protectentities" => cfg.protect_entities = boolean(value)?,
        "protectexplosions" => cfg.protect_explosions = boolean(value)?,
        "protectpistonborder" => cfg.protect_piston_border = boolean(value)?,
        "protectfluidborder" => cfg.protect_fluid_border = boolean(value)?,
        "protectentityinteract" => cfg.protect_entity_interact = boolean(value)?,
        "protectprojectilesborder" => cfg.protect_projectiles_border = boolean(value)?,
        "protectfireandlightning" => cfg.protect_fire_and_lightning = boolean(value)?,
        "protectmobandbossgrief" => cfg.protect_mob_and_boss_grief = boolean(value)?,
        "protectcontraptionborder" => cfg.protect_contraption_border = boolean(value)?,
        "protectdispenserhopperborder" => {
            cfg.protect_dispenser_hopper_border = boolean(value)?
        }
        "protectvehicleborder" => cfg.protect_vehicle_border = boolean(value)?,
        "protectentitypush" => cfg.protect_entity_push = boolean(value)?,
        "protectentitylure" => cfg.protect_entity_lure = boolean(value)?,
        "allowpartymembers" => cfg.allow_party_members = boolean(value)?,
        "allowallies" => cfg.allow_allies = boolean(value)?,
        "offlineforceload" => cfg.offline_forceload = boolean(value)?,
        "maxclaims" => cfg.max_claims = number(value)?,
        "maxforceloads" => cfg.max_forceloads = number(value)?,
        _ => return Err(format!("unknown config key {key:?}")),
    }
    Ok(())
}

/// One party row: ranks, the owning member and the color are all validated,
/// because a half-valid party is a privilege bug, not a repair.
fn party_from_json(item: &Json, ctx: &str) -> io::Result<Party> {
    let id = j_str(item, "id", ctx)?.to_owned();
    let name = j_str(item, "name", ctx)?.to_owned();
    if name.trim().is_empty() || name.len() > 48 {
        return Err(inv(ctx, format!("party {id:?} has a bad name")));
    }
    let owner_uuid = j_str(item, "ownerUuid", ctx)?.to_owned();
    let mut members = BTreeMap::new();
    let Some(Json::Object(rows)) = item.get("members") else {
        return Err(inv(ctx, format!("party {id:?} lacks \"members\"")));
    };
    for (uuid, rank_json) in rows {
        let Json::String(rs) = rank_json else {
            return Err(inv(ctx, format!("party {id:?} member rank is not a string")));
        };
        let rank = PartyRank::parse(rs)
            .ok_or_else(|| inv(ctx, format!("party {id:?} bad rank {rs:?}")))?;
        members.insert(uuid.clone(), rank);
    }
    if members.get(&owner_uuid) != Some(&PartyRank::Owner) {
        return Err(inv(ctx, format!("party {id:?} ownerUuid is not its Owner member")));
    }
    let allies = json_str_set(item, "allies", ctx, &format!("party {id:?} allies"))?;
    let pending_invites = json_str_set(item, "invites", ctx, &format!("party {id:?} invites"))?;
    let color = j_u64(item, "color", ctx)?.unwrap_or(0);
    if color > 0x00ff_ffff {
        return Err(inv(ctx, format!("party {id:?} color out of range")));
    }
    Ok(Party {
        id,
        name,
        owner_uuid,
        members,
        allies,
        pending_invites,
        color_rgb: color as u32,
    })
}

// ------------------------------------------------------------ JSON helpers

fn inv(ctx: &str, msg: impl fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{ctx}: {msg}"))
}

/// `server`, `player:<uuid>` or `expired:<uuid>` — the owner-key spellings
/// the config table accepts.
fn is_owner_key(key: &str) -> bool {
    key == "server" || key.starts_with("player:") || key.starts_with("expired:")
}

/// An optional JSON array of strings as a set: a non-array or a non-string
/// member is an error, an absent array is empty. `label` names the field in
/// the error so each caller keeps its own wording.
fn json_str_set(v: &Json, key: &str, ctx: &str, label: &str) -> io::Result<BTreeSet<String>> {
    let mut set = BTreeSet::new();
    for item in j_arr(v, key, ctx)?.unwrap_or(&[]) {
        let Json::String(s) = item else {
            return Err(inv(ctx, format!("{label} must hold strings")));
        };
        set.insert(s.clone());
    }
    Ok(set)
}

/// Dimension ids from `allowedClaimDimensions`, each validated through
/// [`ResourceLocation`] so a malformed id fails the load.
fn parse_claim_dimensions(items: &[Json], ctx: &str) -> io::Result<BTreeSet<String>> {
    items
        .iter()
        .map(|item| {
            let Json::String(d) = item else {
                return Err(inv(ctx, "\"allowedClaimDimensions\" must hold strings"));
            };
            ResourceLocation::parse(d)
                .map(|dim| dim.to_string())
                .map_err(|e| inv(ctx, e.to_string()))
        })
        .collect()
}

fn parse_doc(text: &str, ctx: &str) -> io::Result<Json> {
    json::parse(text).map_err(|e| inv(ctx, e))
}

/// Required string member.
fn j_str<'a>(v: &'a Json, key: &str, ctx: &str) -> io::Result<&'a str> {
    v.get_str(key)
        .ok_or_else(|| inv(ctx, format!("missing/invalid string {key:?}")))
}

/// Required integral member, bounded to `i32`.
fn j_i32(v: &Json, key: &str, ctx: &str) -> io::Result<i32> {
    match v.get(key) {
        Some(Json::Number(n)) if n.fract() == 0.0 => {
            i32::try_from(*n as i64).map_err(|_| inv(ctx, format!("{key:?} out of i32 range")))
        }
        Some(_) => Err(inv(ctx, format!("{key:?} must be an integer"))),
        None => Err(inv(ctx, format!("missing {key:?}"))),
    }
}

fn j_u64(v: &Json, key: &str, ctx: &str) -> io::Result<Option<u64>> {
    match v.get(key) {
        Some(Json::Number(n)) if n.fract() == 0.0 && *n >= 0.0 => Ok(Some(*n as u64)),
        Some(_) => Err(inv(ctx, format!("{key:?} must be a non-negative integer"))),
        None => Ok(None),
    }
}

fn j_bool(v: &Json, key: &str, ctx: &str) -> io::Result<Option<bool>> {
    match v.get(key) {
        Some(Json::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(inv(ctx, format!("{key:?} must be a boolean"))),
        None => Ok(None),
    }
}

/// Strict-type, lenient-presence bool assignment: a present value must be a
/// bool, an absent one leaves `dst` unchanged.
fn take_bool(v: &Json, key: &str, ctx: &str, dst: &mut bool) -> io::Result<()> {
    if let Some(b) = j_bool(v, key, ctx)? {
        *dst = b;
    }
    Ok(())
}

/// One `"key": "value"` object member.
fn str_json(key: &str, value: &str) -> (String, Json) {
    (key.to_owned(), Json::String(value.to_owned()))
}

/// One `"key": <number>` object member.
fn num_json(key: &str, value: f64) -> (String, Json) {
    (key.to_owned(), Json::Number(value))
}

/// One `"key": true|false` object member — the shared shape of the config
/// and permission-set encoders.
fn bool_json(key: &str, value: bool) -> (String, Json) {
    (key.to_owned(), Json::Bool(value))
}

fn j_arr<'a>(v: &'a Json, key: &str, ctx: &str) -> io::Result<Option<&'a [Json]>> {
    match v.get(key) {
        Some(Json::Array(a)) => Ok(Some(a)),
        Some(_) => Err(inv(ctx, format!("{key:?} must be an array"))),
        None => Ok(None),
    }
}

fn owner_to_json(o: &ClaimOwner) -> Json {
    match o {
        ClaimOwner::Player(u) => Json::Object(vec![
            ("kind".to_owned(), Json::String("player".to_owned())),
            ("uuid".to_owned(), Json::String(u.clone())),
        ]),
        ClaimOwner::Server => {
            Json::Object(vec![("kind".to_owned(), Json::String("server".to_owned()))])
        }
        ClaimOwner::Expired(u) => Json::Object(vec![
            ("kind".to_owned(), Json::String("expired".to_owned())),
            ("uuid".to_owned(), Json::String(u.clone())),
        ]),
    }
}

fn owner_from_json(v: &Json, ctx: &str) -> io::Result<ClaimOwner> {
    match j_str(v, "kind", ctx)? {
        "player" => Ok(ClaimOwner::Player(j_str(v, "uuid", ctx)?.to_owned())),
        "server" => Ok(ClaimOwner::Server),
        "expired" => Ok(ClaimOwner::Expired(j_str(v, "uuid", ctx)?.to_owned())),
        other => Err(inv(ctx, format!("bad owner kind {other:?}"))),
    }
}

fn config_to_json(c: &ClaimProtectionConfig) -> Json {
    let mut pairs = vec![
        bool_json("protectBlocks", c.protect_blocks),
        bool_json("protectContainers", c.protect_containers),
        bool_json("protectEntities", c.protect_entities),
        bool_json("protectExplosions", c.protect_explosions),
        bool_json("protectPistonBorder", c.protect_piston_border),
        bool_json("protectFluidBorder", c.protect_fluid_border),
        bool_json("protectEntityInteract", c.protect_entity_interact),
        bool_json("protectProjectilesBorder", c.protect_projectiles_border),
        bool_json("protectFireAndLightning", c.protect_fire_and_lightning),
        bool_json("protectMobAndBossGrief", c.protect_mob_and_boss_grief),
        bool_json("protectContraptionBorder", c.protect_contraption_border),
        bool_json(
            "protectDispenserHopperBorder",
            c.protect_dispenser_hopper_border,
        ),
        bool_json("protectVehicleBorder", c.protect_vehicle_border),
        bool_json("protectEntityPush", c.protect_entity_push),
        bool_json("protectEntityLure", c.protect_entity_lure),
        bool_json("allowPartyMembers", c.allow_party_members),
        bool_json("allowAllies", c.allow_allies),
        bool_json("offlineForceload", c.offline_forceload),
        num_json("maxClaims", c.max_claims as f64),
        num_json("maxForceloads", c.max_forceloads as f64),
        (
            "everyoneElse".to_owned(),
            perm_set_to_json(&c.everyone_else),
        ),
        (
            "playerOverrides".to_owned(),
            Json::Object(
                c.player_overrides
                    .iter()
                    .map(|(u, p)| (u.clone(), perm_set_to_json(p)))
                    .collect(),
            ),
        ),
        num_json("displayColorRgb", f64::from(c.display_color_rgb)),
    ];
    if let Some(label) = &c.display_label {
        pairs.push(("displayLabel".to_owned(), Json::String(label.clone())));
    }
    Json::Object(pairs)
}

fn perm_set_to_json(p: &ClaimPermissionSet) -> Json {
    Json::Object(vec![
        bool_json("blockBreak", p.block_break),
        bool_json("blockPlace", p.block_place),
        bool_json("containers", p.containers),
        bool_json("redstoneAndDoors", p.redstone_and_doors),
        bool_json("entityInteract", p.entity_interact),
        bool_json("entityDamage", p.entity_damage),
        bool_json("itemPickupDrop", p.item_pickup_drop),
        bool_json("teleportIn", p.teleport_in),
    ])
}

/// Strict types, lenient presence — a stored row that predates a new
/// permission still parses, defaulting to denied.
fn perm_set_from_json(v: &Json, ctx: &str) -> io::Result<ClaimPermissionSet> {
    let Json::Object(_) = v else {
        return Err(inv(ctx, "permission set must be a JSON object"));
    };
    let mut p = ClaimPermissionSet::denied_all();
    take_bool(v, "blockBreak", ctx, &mut p.block_break)?;
    take_bool(v, "blockPlace", ctx, &mut p.block_place)?;
    take_bool(v, "containers", ctx, &mut p.containers)?;
    take_bool(v, "redstoneAndDoors", ctx, &mut p.redstone_and_doors)?;
    take_bool(v, "entityInteract", ctx, &mut p.entity_interact)?;
    take_bool(v, "entityDamage", ctx, &mut p.entity_damage)?;
    take_bool(v, "itemPickupDrop", ctx, &mut p.item_pickup_drop)?;
    take_bool(v, "teleportIn", ctx, &mut p.teleport_in)?;
    Ok(p)
}

/// Strict types, lenient presence: every key that is there must have the
/// right type; absent keys keep `base`'s value.
fn config_from_json(v: &Json, ctx: &str) -> io::Result<ClaimProtectionConfig> {
    let Json::Object(_) = v else {
        return Err(inv(ctx, "config must be a JSON object"));
    };
    let mut c = ClaimProtectionConfig::default();
    take_bool(v, "protectBlocks", ctx, &mut c.protect_blocks)?;
    take_bool(v, "protectContainers", ctx, &mut c.protect_containers)?;
    take_bool(v, "protectEntities", ctx, &mut c.protect_entities)?;
    take_bool(v, "protectExplosions", ctx, &mut c.protect_explosions)?;
    take_bool(v, "protectPistonBorder", ctx, &mut c.protect_piston_border)?;
    take_bool(v, "protectFluidBorder", ctx, &mut c.protect_fluid_border)?;
    take_bool(v, "allowPartyMembers", ctx, &mut c.allow_party_members)?;
    take_bool(v, "allowAllies", ctx, &mut c.allow_allies)?;
    take_bool(v, "offlineForceload", ctx, &mut c.offline_forceload)?;
    take_bool(
        v,
        "protectEntityInteract",
        ctx,
        &mut c.protect_entity_interact,
    )?;
    take_bool(
        v,
        "protectProjectilesBorder",
        ctx,
        &mut c.protect_projectiles_border,
    )?;
    take_bool(
        v,
        "protectFireAndLightning",
        ctx,
        &mut c.protect_fire_and_lightning,
    )?;
    take_bool(
        v,
        "protectMobAndBossGrief",
        ctx,
        &mut c.protect_mob_and_boss_grief,
    )?;
    take_bool(
        v,
        "protectContraptionBorder",
        ctx,
        &mut c.protect_contraption_border,
    )?;
    take_bool(
        v,
        "protectDispenserHopperBorder",
        ctx,
        &mut c.protect_dispenser_hopper_border,
    )?;
    take_bool(
        v,
        "protectVehicleBorder",
        ctx,
        &mut c.protect_vehicle_border,
    )?;
    take_bool(v, "protectEntityPush", ctx, &mut c.protect_entity_push)?;
    take_bool(v, "protectEntityLure", ctx, &mut c.protect_entity_lure)?;
    if let Some(n) = j_u64(v, "maxClaims", ctx)? {
        c.max_claims = usize::try_from(n).map_err(|_| inv(ctx, "maxClaims too large"))?;
    }
    if let Some(n) = j_u64(v, "maxForceloads", ctx)? {
        c.max_forceloads = usize::try_from(n).map_err(|_| inv(ctx, "maxForceloads too large"))?;
    }
    if let Some(e) = v.get("everyoneElse") {
        c.everyone_else = perm_set_from_json(e, ctx)?;
    }
    match v.get("playerOverrides") {
        None | Some(Json::Null) => {}
        Some(Json::Object(rows)) => {
            for (uuid, perms) in rows {
                c.player_overrides
                    .insert(uuid.clone(), perm_set_from_json(perms, ctx)?);
            }
        }
        Some(_) => return Err(inv(ctx, "\"playerOverrides\" must be an object")),
    }
    if let Some(n) = j_u64(v, "displayColorRgb", ctx)? {
        if n > 0x00ff_ffff {
            return Err(inv(ctx, "\"displayColorRgb\" out of range"));
        }
        c.display_color_rgb = n as u32;
    }
    match v.get("displayLabel") {
        None | Some(Json::Null) => {}
        Some(Json::String(s)) => c.display_label = Some(s.clone()),
        Some(_) => return Err(inv(ctx, "\"displayLabel\" must be a string")),
    }
    Ok(c)
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("server-files-claims-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uuid(n: u8) -> String {
        format!("00000000-0000-0000-0000-0000000000{n:02}")
    }

    fn k(dim: &str, cx: i32, cz: i32) -> ClaimChunkKey {
        ClaimChunkKey::new(dim, cx, cz).unwrap()
    }

    fn main_sub() -> SubClaimId {
        SubClaimId::main()
    }

    fn online(uuids: &[&str]) -> BTreeSet<String> {
        uuids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dimension_validation_normalizes_and_rejects() {
        // Unqualified names get the minecraft namespace; mod dims pass.
        assert_eq!(k("overworld", 1, 2).dimension, "minecraft:overworld");
        assert_eq!(k("the_nether", 0, 0).dimension, "minecraft:the_nether");
        assert_eq!(k("aeronautics:space", -3, 4).dimension, "aeronautics:space");
        // ResourceLocation rules apply: traversal, bad chars, extra ':' fail.
        for bad in ["", "UPPER:dim", "ns:a/../b", "a:b:c", "bad dim"] {
            assert!(
                matches!(
                    ClaimChunkKey::new(bad, 0, 0),
                    Err(ClaimsError::InvalidDimension(_))
                ),
                "{bad:?}"
            );
        }
        // Block -> chunk uses floor semantics on negatives.
        let key = ClaimChunkKey::for_block("overworld", -1, 17).unwrap();
        assert_eq!((key.cx, key.cz), (-1, 1));
        // Sub-claim ids validate and lowercase.
        assert_eq!(SubClaimId::parse("Farm_2").unwrap().as_str(), "farm_2");
        for bad in ["", "has space", &"x".repeat(33)] {
            assert!(SubClaimId::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn claim_unclaim_forceload_quota_and_steal_rules() {
        let mut w = ClaimsWorld::new();
        let (a, b) = (uuid(1), uuid(2));
        let main = main_sub();
        // Tight quotas so the limits are exercisable.
        w.protection_config_mut(&a, &main).max_claims = 2;
        w.protection_config_mut(&a, &main).max_forceloads = 1;

        w.claim_chunk(&a, k("overworld", 0, 0), &main).unwrap();
        w.claim_chunk(&a, k("overworld", 1, 0), &main).unwrap();
        assert_eq!(w.claim_count(&a, &main), 2);
        // Third claim hits the quota.
        assert!(matches!(
            w.claim_chunk(&a, k("overworld", 2, 0), &main),
            Err(ClaimsError::ClaimLimitReached { used: 2, limit: 2 })
        ));
        // B cannot steal, cannot unclaim A's chunk.
        let stolen = k("overworld", 0, 0);
        assert_eq!(
            w.claim_chunk(&b, stolen.clone(), &main),
            Err(ClaimsError::ClaimedByOther)
        );
        assert_eq!(w.unclaim_chunk(&b, &stolen), Err(ClaimsError::NotOwner));
        // ... but an admin may take over and release.
        w.claim_chunk_as(&b, true, stolen.clone(), &main).unwrap();
        assert_eq!(
            w.claim_at(&stolen).unwrap().owner,
            ClaimOwner::Player(b.clone())
        );
        w.unclaim_chunk_as(&a, true, &stolen).unwrap();
        assert!(w.claim_at(&stolen).is_none());
        // Forceload quota: one allowed, second refused, then freed.
        let f1 = k("overworld", 1, 0);
        w.set_forceload(&a, &f1, true).unwrap();
        assert!(w.claim_at(&f1).unwrap().forceloaded);
        // Re-claiming (0,0) brings a back to 2 claims; the second forceload
        // is refused by the quota, disabling frees it.
        w.claim_chunk(&a, k("overworld", 0, 0), &main).unwrap();
        assert!(matches!(
            w.set_forceload(&a, &k("overworld", 0, 0), true),
            Err(ClaimsError::ForceloadLimitReached { used: 1, limit: 1 })
        ));
        w.set_forceload(&a, &f1, false).unwrap();
        w.set_forceload(&a, &k("overworld", 0, 0), true).unwrap();
        assert_eq!(w.forceload_count(&a, &main), 1);
        // Unclaiming wilderness and non-ops on missing claims error cleanly.
        assert_eq!(
            w.unclaim_chunk(&a, &k("overworld", 99, 99)),
            Err(ClaimsError::NotClaimed)
        );
    }

    #[test]
    fn server_claims_are_admin_only() {
        let mut w = ClaimsWorld::new();
        let (admin, pleb) = (uuid(1), uuid(2));
        w.set_admin(admin.as_str(), true);
        let main = main_sub();
        let spawn = k("overworld", 0, 0);
        w.claim_server_chunk(true, spawn.clone(), &main).unwrap();
        // Non-admin cannot create server claims.
        assert_eq!(
            w.claim_server_chunk(false, k("overworld", 5, 5), &main),
            Err(ClaimsError::NotAuthorized)
        );
        // Actions inside: pleb denied, admin allowed, wilderness-adjacent
        // nobody allowed for containers either.
        for action in [
            ClaimAction::BlockBreak,
            ClaimAction::BlockPlace,
            ClaimAction::ContainerInteract,
            ClaimAction::EntityAttack,
        ] {
            assert_eq!(
                w.check_action(&spawn, Some(&pleb), action.clone()),
                ClaimDecision::Denied(DenyReason::ServerClaim),
                "{action:?}"
            );
            assert_eq!(
                w.check_action(&spawn, Some(&admin), action),
                ClaimDecision::Allowed
            );
        }
        // Explosions still denied (server config protects everything).
        assert_eq!(
            w.check_action(&spawn, None, ClaimAction::Explosion),
            ClaimDecision::Denied(DenyReason::ExplosionsProtected)
        );
        // Non-admin cannot unclaim a server claim; admin can.
        assert_eq!(
            w.unclaim_chunk(&pleb, &spawn),
            Err(ClaimsError::NotAuthorized)
        );
        w.unclaim_chunk_as(&admin, true, &spawn).unwrap();
    }

    #[test]
    fn check_action_flags_party_ally_and_over_limit() {
        let mut w = ClaimsWorld::new();
        let (owner, member, ally, stranger) = (uuid(1), uuid(2), uuid(3), uuid(4));
        let main = main_sub();
        let home = k("overworld", 0, 0);
        w.claim_chunk(&owner, home.clone(), &main).unwrap();

        // Party member allowed (allow_party_members default true).
        let p1 = w.create_party(&owner, "Home Team").unwrap();
        w.invite_player(&owner, &member).unwrap();
        w.accept_invite(&member, None).unwrap();
        assert_eq!(
            w.check_action(&home, Some(&member), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
        // Ally of the party allowed only when allow_allies is on.
        let p2 = w.create_party(&ally, "Friends").unwrap();
        w.add_ally(&ally, &p1).unwrap();
        assert_eq!(
            w.check_action(&home, Some(&ally), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::BlocksProtected)
        );
        w.protection_config_mut(&owner, &main).allow_allies = true;
        assert_eq!(
            w.check_action(&home, Some(&ally), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
        assert_eq!(p2, "party-2");
        // Stranger denied; containers/entities follow their own flags.
        assert_eq!(
            w.check_action(&home, Some(&stranger), ClaimAction::EntityAttack),
            ClaimDecision::Denied(DenyReason::EntitiesProtected)
        );
        w.protection_config_mut(&owner, &main).protect_entities = false;
        assert_eq!(
            w.check_action(&home, Some(&stranger), ClaimAction::EntityAttack),
            ClaimDecision::Allowed
        );
        // Wilderness: anything goes.
        assert_eq!(
            w.check_action(
                &k("overworld", 50, 50),
                Some(&stranger),
                ClaimAction::BlockBreak
            ),
            ClaimDecision::Allowed
        );

        // Over-limit: shrink the owner's max below their count -> trusted
        // access suspended until back under.
        w.protection_config_mut(&owner, &main).max_claims = 0;
        assert!(w.over_limit(&owner, &main));
        assert_eq!(
            w.check_action(&home, Some(&member), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::OwnerOverLimit)
        );
        assert_eq!(
            w.check_action(&home, Some(&ally), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::OwnerOverLimit)
        );
        // ... but the owner is still fine.
        assert_eq!(
            w.check_action(&home, Some(&owner), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
        // Unclaim back under the limit -> access restored.
        w.unclaim_chunk(&owner, &home).unwrap();
        w.claim_chunk(&owner, home.clone(), &main).unwrap_err(); // quota: max 0
        w.protection_config_mut(&owner, &main).max_claims = 10;
        w.claim_chunk(&owner, home.clone(), &main).unwrap();
        assert!(!w.over_limit(&owner, &main));
        assert_eq!(
            w.check_action(&home, Some(&member), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
    }

    #[test]
    fn piston_and_fluid_border_protection() {
        let mut w = ClaimsWorld::new();
        let (a, b) = (uuid(1), uuid(2));
        let main = main_sub();
        let a_home = k("overworld", 0, 0);
        let a_other = k("overworld", 1, 0);
        let b_home = k("overworld", 2, 0);
        let wild = k("overworld", 3, 0);
        w.claim_chunk(&a, a_home.clone(), &main).unwrap();
        w.claim_chunk(&a, a_other.clone(), &main).unwrap();
        w.claim_chunk(&b, b_home.clone(), &main).unwrap();

        // Same owner pushing/pulling inside own territory: allowed.
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::PistonFrom(a_other.clone())),
            ClaimDecision::Allowed
        );
        // A different owner's piston reaching in: blocked.
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::PistonFrom(b_home.clone())),
            ClaimDecision::Denied(DenyReason::PistonBorderProtected)
        );
        // A wilderness piston reaching in: blocked.
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::PistonFrom(wild.clone())),
            ClaimDecision::Denied(DenyReason::PistonBorderProtected)
        );
        // Same for fluid.
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::FluidFrom(wild.clone())),
            ClaimDecision::Denied(DenyReason::FluidBorderProtected)
        );
        // Pushing OUT into wilderness is fine (the target protects nothing).
        assert_eq!(
            w.check_action(&wild, None, ClaimAction::PistonFrom(a_home.clone())),
            ClaimDecision::Allowed
        );
        // Turn the border protection off and the same reach-in is allowed.
        w.protection_config_mut(&a, &main).protect_piston_border = false;
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::PistonFrom(b_home.clone())),
            ClaimDecision::Allowed
        );
        w.protection_config_mut(&a, &main).protect_fluid_border = false;
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::FluidFrom(b_home)),
            ClaimDecision::Allowed
        );
    }

    #[test]
    fn party_lifecycle_and_chat_recipients() {
        let mut w = ClaimsWorld::new();
        let (owner, officer, member, other) = (uuid(1), uuid(2), uuid(3), uuid(4));
        // One party per player; default names come from the actor.
        let pid = w.create_party(&owner, "Alpha").unwrap();
        assert_eq!(
            w.create_party(&owner, "Again"),
            Err(ClaimsError::AlreadyInParty)
        );
        // Members cannot invite; promote first.
        w.invite_player(&owner, &member).unwrap();
        assert_eq!(
            w.invite_player(&member, &other),
            Err(ClaimsError::NotInParty)
        );
        w.accept_invite(&member, None).unwrap();
        assert_eq!(
            w.invite_player(&member, &other),
            Err(ClaimsError::InsufficientRank)
        );
        w.set_rank(&owner, &member, PartyRank::Moderator).unwrap();
        w.invite_player(&member, &officer).unwrap();
        w.accept_invite(&officer, Some(&pid)).unwrap();
        // Non-owner cannot set ranks.
        assert_eq!(
            w.set_rank(&member, &officer, PartyRank::Member),
            Err(ClaimsError::InsufficientRank)
        );
        assert_eq!(
            w.set_rank(&owner, &officer, PartyRank::Owner),
            Err(ClaimsError::InvalidTarget)
        );
        // Kick: a moderator cannot kick a fellow moderator; owner can kick anyone.
        w.set_rank(&owner, &officer, PartyRank::Moderator).unwrap();
        assert_eq!(
            w.kick_member(&member, &officer),
            Err(ClaimsError::InsufficientRank)
        );
        w.kick_member(&owner, &officer).unwrap();
        assert!(w.party_of(&officer).is_none());
        // The owner leaving hands leadership to the highest-ranked member
        // left behind (member is Moderator here); the last one out
        // dissolves the party.
        w.leave_party(&owner).unwrap();
        assert!(w.party_of(&owner).is_none());
        assert_eq!(w.parties()[&pid].owner_uuid, member);
        assert_eq!(w.parties()[&pid].rank_of(&member), Some(PartyRank::Owner));
        w.leave_party(&member).unwrap();
        assert!(w.party_of(&member).is_none());
        assert_eq!(w.parties().len(), 0);

        // Allies + chat recipients.
        let p1 = w.create_party(&owner, "One").unwrap();
        w.invite_player(&owner, &member).unwrap();
        w.accept_invite(&member, None).unwrap();
        let p2 = w.create_party(&other, "Two").unwrap();
        w.add_ally(&owner, &p2).unwrap();
        assert_eq!(w.add_ally(&owner, &p2), Err(ClaimsError::AlreadyAllied));
        w.remove_ally(&owner, &p2).unwrap();
        assert_eq!(w.remove_ally(&owner, &p2), Err(ClaimsError::NotAllied));
        let _ = p1;

        let online_set = online(&[owner.as_str(), member.as_str()]);
        assert_eq!(
            w.party_chat_recipients(&owner, &online_set),
            vec![member.clone()]
        );
        // Sender excluded; offline members excluded.
        let only_member = online(&[member.as_str()]);
        assert_eq!(
            w.party_chat_recipients(&member, &only_member),
            Vec::<String>::new()
        );
        assert_eq!(
            w.party_chat_recipients(&uuid(9), &online_set),
            Vec::<String>::new()
        );
    }

    #[test]
    fn active_forceloaded_chunks_filters_by_online_and_config() {
        let mut w = ClaimsWorld::new();
        let (a, b) = (uuid(1), uuid(2));
        let main = main_sub();
        let a_chunk = k("overworld", 0, 0);
        let b_chunk = k("overworld", 1, 0);
        let srv = k("overworld", 2, 0);
        w.claim_chunk(&a, a_chunk.clone(), &main).unwrap();
        w.claim_chunk(&b, b_chunk.clone(), &main).unwrap();
        w.claim_server_chunk(true, srv.clone(), &main).unwrap();
        w.set_forceload(&a, &a_chunk, true).unwrap();
        w.set_forceload(&b, &b_chunk, true).unwrap();
        w.set_forceload_as(&a, true, &srv, true).unwrap();

        // Nobody online: only the server claim ticks.
        assert_eq!(
            w.active_forceloaded_chunks(&BTreeSet::new()),
            vec![srv.clone()]
        );
        // A online: theirs join, sorted by key.
        assert_eq!(
            w.active_forceloaded_chunks(&online(&[a.as_str()])),
            vec![a_chunk.clone(), srv.clone()]
        );
        // B offline and offline_forceload off -> still absent...
        assert!(!w
            .active_forceloaded_chunks(&online(&[a.as_str()]))
            .contains(&b_chunk));
        // ... until their config allows offline forceloading.
        w.protection_config_mut(&b, &main).offline_forceload = true;
        assert_eq!(
            w.active_forceloaded_chunks(&online(&[a.as_str()])),
            vec![a_chunk.clone(), b_chunk.clone(), srv.clone()]
        );
        // Expired claims never forceload, config notwithstanding; and with
        // only b "online", a's claims stay parked too — server claims only.
        w.expire_player_claims(true, &b).unwrap();
        assert_eq!(
            w.active_forceloaded_chunks(&online(&[b.as_str()])),
            vec![srv.clone()]
        );
    }

    #[test]
    fn expired_claims_still_protect_from_everyone_but_admins() {
        let mut w = ClaimsWorld::new();
        let (admin, gone, pleb) = (uuid(1), uuid(2), uuid(3));
        w.set_admin(admin.as_str(), true);
        let main = main_sub();
        let home = k("overworld", 0, 0);
        w.claim_chunk(&gone, home.clone(), &main).unwrap();
        assert_eq!(w.expire_player_claims(true, &gone).unwrap(), 1);
        assert_eq!(
            w.claim_at(&home).unwrap().owner,
            ClaimOwner::Expired(gone.clone())
        );
        for action in [ClaimAction::BlockBreak, ClaimAction::ContainerInteract] {
            assert_eq!(
                w.check_action(&home, Some(&pleb), action.clone()),
                ClaimDecision::Denied(DenyReason::ExpiredClaim)
            );
            assert_eq!(
                w.check_action(&home, Some(&admin), action),
                ClaimDecision::Allowed
            );
        }
        // Even the original owner is locked out; non-admin expiry refused.
        assert_eq!(
            w.check_action(&home, Some(&gone), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::ExpiredClaim)
        );
        assert_eq!(
            w.expire_player_claims(false, &pleb),
            Err(ClaimsError::NotAuthorized)
        );
    }

    #[test]
    fn sync_deltas_have_monotonic_revisions() {
        let mut w = ClaimsWorld::new();
        let a = uuid(1);
        let main = main_sub();
        let key = k("overworld", 0, 0);
        w.claim_chunk(&a, key.clone(), &main).unwrap();
        w.set_forceload(&a, &key, true).unwrap();
        w.unclaim_chunk(&a, &key).unwrap();
        let deltas = w.drain_deltas();
        let revs: Vec<u64> = deltas.iter().map(ClaimsSyncDelta::revision).collect();
        assert_eq!(revs, [1, 2, 3]);
        // Last delta is a removal.
        match &deltas[2] {
            ClaimsSyncDelta::ClaimUpdated { key: dk, claim, .. } => {
                assert_eq!(*dk, key);
                assert_eq!(*claim, None);
            }
        }
        // Drain empties the queue; revision keeps counting.
        assert!(w.pending_deltas().is_empty());
        w.claim_chunk(&a, key, &main).unwrap();
        assert_eq!(w.drain_deltas()[0].revision(), 4);
    }

    #[test]
    fn command_dispatch_covers_all_four_roots() {
        let mut w = ClaimsWorld::new();
        let (a, a_name, b) = (uuid(1), "Alice".to_owned(), uuid(2));
        let here = k("overworld", 4, -2);

        // /oclaims
        let out = exec_openpac_command(&mut w, &a, &a_name, false, Some(&here), "/oclaims claim")
            .unwrap();
        assert!(out.contains("claimed"), "{out}");
        let info =
            exec_openpac_command(&mut w, &a, &a_name, false, Some(&here), "oclaims info").unwrap();
        assert!(info.contains(&format!("owner=player:{a}")), "{info}");
        exec_openpac_command(
            &mut w,
            &a,
            &a_name,
            false,
            Some(&here),
            "/oclaims forceload",
        )
        .unwrap();
        assert!(w.claim_at(&here).unwrap().forceloaded);
        exec_openpac_command(
            &mut w,
            &a,
            &a_name,
            false,
            Some(&here),
            "/oclaims unforceload",
        )
        .unwrap();
        // B cannot unclaim A's chunk via command; admin can.
        assert!(
            exec_openpac_command(&mut w, &b, "Bob", false, Some(&here), "/oclaims unclaim")
                .is_err()
        );
        exec_openpac_command(&mut w, &b, "Bob", true, Some(&here), "/oclaims unclaim").unwrap();

        // Missing chunk context fails.
        assert!(exec_openpac_command(&mut w, &a, &a_name, false, None, "/oclaims claim").is_err());

        // /oparties
        let out = exec_openpac_command(
            &mut w,
            &a,
            &a_name,
            false,
            None,
            "/oparties create Cool Crew",
        )
        .unwrap();
        assert!(out.contains("party-1"), "{out}");
        exec_openpac_command(&mut w, &a, &a_name, false, None, "/oparties invite").unwrap_err();
        exec_openpac_command(
            &mut w,
            &a,
            &a_name,
            false,
            None,
            &format!("/oparties invite {b}"),
        )
        .unwrap();
        let out = exec_openpac_command(&mut w, &b, "Bob", false, None, "/oparties accept party-1")
            .unwrap();
        assert!(out.contains("joined"), "{out}");
        let info =
            exec_openpac_command(&mut w, &a, &a_name, false, None, "/oparties info").unwrap();
        assert!(info.contains("Cool Crew") && info.contains(&b), "{info}");

        // /opac config view + set
        let out = exec_openpac_command(&mut w, &a, &a_name, false, None, "/opac config").unwrap();
        assert!(out.contains("protectBlocks=true"), "{out}");
        exec_openpac_command(
            &mut w,
            &a,
            &a_name,
            false,
            None,
            "/opac config allowAllies true",
        )
        .unwrap();
        let out = exec_openpac_command(&mut w, &a, &a_name, false, None, "/opac config").unwrap();
        assert!(out.contains("allowAllies=true"), "{out}");
        exec_openpac_command(&mut w, &a, &a_name, false, None, "/opac config bogusKey 1")
            .unwrap_err();

        // /opac claim is admin-only and makes a server claim.
        assert!(
            exec_openpac_command(&mut w, &a, &a_name, false, Some(&here), "/opac claim").is_err()
        );
        exec_openpac_command(&mut w, &a, &a_name, true, Some(&here), "/opac claim").unwrap();
        assert_eq!(w.claim_at(&here).unwrap().owner, ClaimOwner::Server);

        // /opm: reaches party members.
        let out =
            exec_openpac_command(&mut w, &a, &a_name, false, None, "/opm hello friends").unwrap();
        assert!(out.contains("hello friends") && out.contains(&b), "{out}");
        assert!(exec_openpac_command(&mut w, &uuid(9), "X", false, None, "/opm hi").is_err());
        // Unknown command roots error.
        assert!(exec_openpac_command(&mut w, &a, &a_name, false, None, "/bogus").is_err());
    }

    #[test]
    fn save_load_roundtrip_and_fail_closed() {
        let dir = tmp_dir("io");
        let mut w = ClaimsWorld::new();
        let (a, b, c) = (uuid(1), uuid(2), uuid(3));
        let main = main_sub();
        w.set_admin(a.as_str(), true);
        w.protection_config_mut(&a, &main).max_claims = 9;
        w.protection_config_mut(&a, &main).offline_forceload = true;
        w.claim_chunk(&a, k("overworld", 0, 0), &main).unwrap();
        w.claim_chunk(
            &a,
            k("the_nether", -4, 7),
            &SubClaimId::parse("farm").unwrap(),
        )
        .unwrap();
        w.claim_server_chunk(true, k("aeronautics:space", 1, 1), &main)
            .unwrap();
        w.set_forceload(&a, &k("overworld", 0, 0), true).unwrap();
        let pid = w.create_party(&a, "Persisted").unwrap();
        w.invite_player(&a, &b).unwrap();
        w.accept_invite(&b, None).unwrap();
        let p2 = w.create_party(&c, "Others").unwrap();
        w.add_ally(&a, &p2).unwrap();
        w.parties
            .get_mut(&p2)
            .unwrap()
            .pending_invites
            .insert(uuid(9));

        w.save_to_dir(&dir).unwrap();
        let bytes_claims = std::fs::read_to_string(dir.join(CLAIMS_FILE)).unwrap();
        let bytes_parties = std::fs::read_to_string(dir.join(PARTIES_FILE)).unwrap();

        let back = ClaimsWorld::load_from_dir(&dir).unwrap();
        assert_eq!(back.claims(), w.claims());
        assert_eq!(back.parties(), w.parties());
        assert_eq!(back.revision(), w.revision());
        assert_eq!(back.claim_count(&a, &main), 1);
        assert_eq!(back.party_of(&b).unwrap().id, pid);
        assert_eq!(back.parties()[&p2].pending_invites.len(), 1);
        assert!(back.is_admin(&a));
        assert!(back.protection_config(&a, &main).offline_forceload);

        // Saving again is byte-deterministic.
        back.save_to_dir(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(CLAIMS_FILE)).unwrap(),
            bytes_claims
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(PARTIES_FILE)).unwrap(),
            bytes_parties
        );

        // Missing directory -> empty world; corrupt JSON -> InvalidData.
        let empty = tmp_dir("empty");
        assert!(ClaimsWorld::load_from_dir(&empty)
            .unwrap()
            .claims()
            .is_empty());
        for (file, blob) in [
            (CLAIMS_FILE, "not json"),
            (
                CLAIMS_FILE,
                r#"{"claims":[{"dim":"bad dim","cx":0,"cz":0,"owner":{"kind":"player","uuid":"u"},"sub":"main"}]}"#,
            ),
            (
                CLAIMS_FILE,
                r#"{"claims":[{"dim":"minecraft:o","cx":0.5,"cz":0,"owner":{"kind":"player","uuid":"u"},"sub":"main"}]}"#,
            ),
            (
                CLAIMS_FILE,
                r#"{"claims":[{"dim":"minecraft:o","cx":0,"cz":0,"owner":{"kind":"nobody"},"sub":"main"}]}"#,
            ),
            (
                PARTIES_FILE,
                r#"{"parties":[{"id":"party-1","name":"x","ownerUuid":"u","members":{"u":"member"}}]}"#,
            ),
            (PARTIES_FILE, r#"{"parties":"nope"}"#),
        ] {
            std::fs::write(dir.join(CLAIMS_FILE), &bytes_claims).unwrap();
            std::fs::write(dir.join(PARTIES_FILE), &bytes_parties).unwrap();
            std::fs::write(dir.join(file), blob).unwrap();
            let err = ClaimsWorld::load_from_dir(&dir).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{file}: {blob}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn sub_claims_have_separate_claims_and_quotas() {
        let mut w = ClaimsWorld::new();
        let a = uuid(1);
        let main = main_sub();
        let farm = SubClaimId::parse("farm").unwrap();
        w.protection_config_mut(&a, &farm).max_claims = 1;
        let key = k("overworld", 0, 0);
        w.claim_chunk(&a, key.clone(), &main).unwrap();
        // Moving the chunk to "farm" consumes farm's quota, not main's.
        w.claim_chunk(&a, key.clone(), &farm).unwrap();
        assert_eq!(w.claim_count(&a, &main), 0);
        assert_eq!(w.claim_count(&a, &farm), 1);
        assert_eq!(w.claim_at(&key).unwrap().sub_claim, "farm");
        // Farm is full now; main still has room.
        assert!(matches!(
            w.claim_chunk(&a, k("overworld", 1, 0), &farm),
            Err(ClaimsError::ClaimLimitReached { .. })
        ));
        w.claim_chunk(&a, k("overworld", 1, 0), &main).unwrap();
        // Re-claiming under the same sub-claim is a plain error.
        assert_eq!(
            w.claim_chunk(&a, k("overworld", 1, 0), &main),
            Err(ClaimsError::AlreadyClaimed)
        );
    }

    #[test]
    fn known_players_and_per_player_permission_matrix() {
        let mut w = ClaimsWorld::new();
        let (owner, friend, foe, stranger) = (uuid(1), uuid(2), uuid(3), uuid(4));
        let main = main_sub();

        // The roster records every player who ever joined; re-sightings
        // refresh the name and stretch the seen-tick window.
        w.record_player_seen(&owner, "Owner", 10);
        w.record_player_seen(&friend, "Friend", 20);
        w.record_player_seen(&friend, "FriendRenamed", 30);
        w.record_player_seen(&foe, "Foe", 5);
        assert_eq!(w.known_players().len(), 3);
        let rec = &w.known_players()[&friend];
        assert_eq!(rec.name, "FriendRenamed");
        assert_eq!(rec.first_seen_tick, 20);
        assert_eq!(rec.last_seen_tick, 30);

        let home = k("overworld", 0, 0);
        w.claim_chunk(&owner, home.clone(), &main).unwrap();
        assert_eq!(
            w.check_action(&home, Some(&stranger), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::BlocksProtected)
        );

        // 1-click grant: the friend alone may build and open containers.
        w.set_player_permission_override(
            &owner,
            &main,
            &friend,
            ClaimPermissionSet {
                block_break: true,
                block_place: true,
                containers: true,
                ..ClaimPermissionSet::denied_all()
            },
        );
        assert_eq!(
            w.check_action(&home, Some(&friend), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
        assert_eq!(
            w.check_action(&home, Some(&friend), ClaimAction::ContainerInteract),
            ClaimDecision::Allowed
        );
        // Permissions not granted stay denied.
        assert_eq!(
            w.check_action(&home, Some(&friend), ClaimAction::EntityAttack),
            ClaimDecision::Denied(DenyReason::EntitiesProtected)
        );
        // The override governs completely: no block_place bit -> denied
        // even when the flag would have allowed it.
        w.protection_config_mut(&owner, &main).protect_blocks = false;
        assert_eq!(
            w.check_action(&home, Some(&stranger), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
        w.protection_config_mut(&owner, &main).protect_blocks = true;

        // everyone_else: visitors may open doors and pick up items, nothing
        // else.
        w.set_everyone_else_permissions(&owner, &main, ClaimPermissionSet::visitor());
        assert_eq!(
            w.check_action(&home, Some(&stranger), ClaimAction::RedstoneOrDoorInteract),
            ClaimDecision::Allowed
        );
        assert_eq!(
            w.check_action(&home, Some(&stranger), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::BlocksProtected)
        );

        // Party members are trusted — but an explicit override still wins,
        // so one specific player can be blocked even while their party
        // keeps access.
        let _pid = w.create_party(&owner, "Crew").unwrap();
        w.invite_player(&owner, &foe).unwrap();
        w.accept_invite(&foe, None).unwrap();
        assert_eq!(
            w.check_action(&home, Some(&foe), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
        w.set_player_permission_override(&owner, &main, &foe, ClaimPermissionSet::denied_all());
        assert_eq!(
            w.check_action(&home, Some(&foe), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::BlocksProtected)
        );
        w.clear_player_permission_override(&owner, &main, &foe);
        assert_eq!(
            w.check_action(&home, Some(&foe), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );

        // effective_permissions_for mirrors check_action's resolution.
        let eff = w.effective_permissions_for(&owner, &main, &friend);
        assert!(eff.block_break && eff.containers && !eff.entity_damage);
        let eff = w.effective_permissions_for(&owner, &main, &stranger);
        assert!(eff.redstone_and_doors && eff.item_pickup_drop && !eff.block_break);
        assert_eq!(
            w.effective_permissions_for(&owner, &main, &owner),
            ClaimPermissionSet::trusted_builder()
        );
        // The 1-click presets compose sanely.
        assert_eq!(ClaimPermissionSet::denied_all(), ClaimPermissionSet::default());
        assert!(ClaimPermissionSet::container_only().containers);
        assert!(!ClaimPermissionSet::container_only().block_break);
        assert!(ClaimPermissionSet::trusted_builder().teleport_in);
    }

    #[test]
    fn entity_interact_push_and_lure_protection() {
        let mut w = ClaimsWorld::new();
        let (owner, outsider, guest) = (uuid(1), uuid(2), uuid(3));
        let main = main_sub();
        let home = k("overworld", 0, 0);
        w.claim_chunk(&owner, home.clone(), &main).unwrap();

        // Armor stands, item frames, shearing/leashing/milking pets: all
        // denied to outsiders, allowed for the owner.
        for action in [
            ClaimAction::EntityInteract,
            ClaimAction::EntityPushOrDisplace,
            ClaimAction::EntityTemptOrLure,
        ] {
            assert_eq!(
                w.check_action(&home, Some(&owner), action.clone()),
                ClaimDecision::Allowed,
                "{action:?}"
            );
        }
        assert_eq!(
            w.check_action(&home, Some(&outsider), ClaimAction::EntityInteract),
            ClaimDecision::Denied(DenyReason::EntityInteractProtected)
        );
        // Body-push / fishing-rod / knockback displacement: zero movement
        // for outsiders.
        assert_eq!(
            w.check_action(&home, Some(&outsider), ClaimAction::EntityPushOrDisplace),
            ClaimDecision::Denied(DenyReason::EntityPushProtected)
        );
        // Wheat/carrot/seeds lure: tempt AI ignores the outsider.
        assert_eq!(
            w.check_action(&home, Some(&outsider), ClaimAction::EntityTemptOrLure),
            ClaimDecision::Denied(DenyReason::EntityLureProtected)
        );

        // A player granted entity_interact can push, lure, shear and ride.
        w.set_player_permission_override(
            &owner,
            &main,
            &guest,
            ClaimPermissionSet {
                entity_interact: true,
                ..ClaimPermissionSet::denied_all()
            },
        );
        for action in [
            ClaimAction::EntityInteract,
            ClaimAction::EntityPushOrDisplace,
            ClaimAction::EntityTemptOrLure,
        ] {
            assert_eq!(
                w.check_action(&home, Some(&guest), action.clone()),
                ClaimDecision::Allowed,
                "{action:?}"
            );
        }
        // entity_damage alone also allows displacement (hurt-push), but
        // not tempting or touching.
        w.set_player_permission_override(
            &owner,
            &main,
            &guest,
            ClaimPermissionSet {
                entity_damage: true,
                ..ClaimPermissionSet::denied_all()
            },
        );
        assert_eq!(
            w.check_action(&home, Some(&guest), ClaimAction::EntityPushOrDisplace),
            ClaimDecision::Allowed
        );
        assert_eq!(
            w.check_action(&home, Some(&guest), ClaimAction::EntityInteract),
            ClaimDecision::Denied(DenyReason::EntityInteractProtected)
        );

        // Flags off open the actions to everyone.
        w.clear_player_permission_override(&owner, &main, &guest);
        w.protection_config_mut(&owner, &main).protect_entity_lure = false;
        w.protection_config_mut(&owner, &main).protect_entity_push = false;
        assert_eq!(
            w.check_action(&home, Some(&outsider), ClaimAction::EntityTemptOrLure),
            ClaimDecision::Allowed
        );
        assert_eq!(
            w.check_action(&home, Some(&outsider), ClaimAction::EntityPushOrDisplace),
            ClaimDecision::Allowed
        );
        // Interact is still protected by its own flag.
        assert_eq!(
            w.check_action(&home, Some(&outsider), ClaimAction::EntityInteract),
            ClaimDecision::Denied(DenyReason::EntityInteractProtected)
        );
    }

    #[test]
    fn cross_border_grief_protection_suite() {
        let mut w = ClaimsWorld::new();
        let (a, b) = (uuid(1), uuid(2));
        let main = main_sub();
        let a_home = k("overworld", 0, 0);
        let a_annex = k("overworld", 1, 0);
        let b_home = k("overworld", 2, 0);
        let wild = k("overworld", 3, 0);
        w.claim_chunk(&a, a_home.clone(), &main).unwrap();
        w.claim_chunk(&a, a_annex.clone(), &main).unwrap();
        w.claim_chunk(&b, b_home.clone(), &main).unwrap();

        // --- projectiles: outsider bow/trident/potion shots are denied;
        //     the owner (or anyone with damage/build permission) may shoot
        //     in, and same-owner chunks always allow the flight path.
        for shooter in [Some(b.clone()), None] {
            assert_eq!(
                w.check_action(
                    &a_home,
                    None,
                    ClaimAction::ProjectileFrom {
                        source: wild.clone(),
                        shooter_uuid: shooter.clone(),
                    }
                ),
                ClaimDecision::Denied(DenyReason::ProjectilesBorderProtected),
                "{shooter:?}"
            );
        }
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ProjectileFrom {
                    source: b_home.clone(),
                    shooter_uuid: Some(b.clone()),
                }
            ),
            ClaimDecision::Denied(DenyReason::ProjectilesBorderProtected)
        );
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ProjectileFrom {
                    source: wild.clone(),
                    shooter_uuid: Some(a.clone()),
                }
            ),
            ClaimDecision::Allowed
        );
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ProjectileFrom {
                    source: a_annex.clone(),
                    shooter_uuid: Some(b.clone()),
                }
            ),
            ClaimDecision::Allowed
        );
        // A friend with entity_damage may also shoot in.
        w.set_player_permission_override(
            &a,
            &main,
            &b,
            ClaimPermissionSet {
                entity_damage: true,
                ..ClaimPermissionSet::denied_all()
            },
        );
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ProjectileFrom {
                    source: wild.clone(),
                    shooter_uuid: Some(b.clone()),
                }
            ),
            ClaimDecision::Allowed
        );
        w.clear_player_permission_override(&a, &main, &b);

        // --- fire and lightning crossing the border.
        for action in [
            ClaimAction::FireSpreadFrom(wild.clone()),
            ClaimAction::LightningFrom(wild.clone()),
            ClaimAction::FireSpreadFrom(b_home.clone()),
            ClaimAction::LightningFrom(b_home.clone()),
        ] {
            assert_eq!(
                w.check_action(&a_home, None, action.clone()),
                ClaimDecision::Denied(DenyReason::FireAndLightningProtected),
                "{action:?}"
            );
        }
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::FireSpreadFrom(a_annex.clone())),
            ClaimDecision::Allowed
        );
        // --- explosions: outsider TNT cannons and creepers are denied;
        //     the owner's own blast inside their land is fine.
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ExplosionFrom {
                    source: wild.clone(),
                    actor_uuid: None,
                }
            ),
            ClaimDecision::Denied(DenyReason::ExplosionsProtected)
        );
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ExplosionFrom {
                    source: b_home.clone(),
                    actor_uuid: Some(b.clone()),
                }
            ),
            ClaimDecision::Denied(DenyReason::ExplosionsProtected)
        );
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ExplosionFrom {
                    source: a_annex.clone(),
                    actor_uuid: Some(a.clone()),
                }
            ),
            ClaimDecision::Allowed
        );
        // --- mob and boss grief: endermen, ravagers, withers, colossal
        //     dragons — inside or across the border, all denied.
        for (src, boss) in [
            (None, true),
            (Some(wild.clone()), true),
            (Some(b_home.clone()), false),
            (None, false),
        ] {
            assert_eq!(
                w.check_action(
                    &a_home,
                    None,
                    ClaimAction::MobOrBossGrief {
                        source: src,
                        is_boss_or_dragon: boss,
                    }
                ),
                ClaimDecision::Denied(DenyReason::MobOrBossGriefProtected),
                "boss={boss}"
            );
        }
        // --- modded contraptions/quarries crossing the border.
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ContraptionOrMachineFrom {
                    source: b_home.clone(),
                    owner_uuid: Some(b.clone()),
                }
            ),
            ClaimDecision::Denied(DenyReason::ContraptionBorderProtected)
        );
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ContraptionOrMachineFrom {
                    source: a_annex.clone(),
                    owner_uuid: Some(a.clone()),
                }
            ),
            ClaimDecision::Allowed
        );
        // The owner's own machine parked in the wilderness may reach in.
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::ContraptionOrMachineFrom {
                    source: wild.clone(),
                    owner_uuid: Some(a.clone()),
                }
            ),
            ClaimDecision::Allowed
        );
        // --- dispensers dumping fluid and hoppers siphoning chests.
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::DispenserOrHopperFrom(b_home)),
            ClaimDecision::Denied(DenyReason::DispenserHopperBorderProtected)
        );
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::DispenserOrHopperFrom(wild)),
            ClaimDecision::Denied(DenyReason::DispenserHopperBorderProtected)
        );
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::DispenserOrHopperFrom(a_annex)),
            ClaimDecision::Allowed
        );

        // Flipping each flag off opens that border — per claim, as
        // configured.
        w.protection_config_mut(&a, &main).protect_fire_and_lightning = false;
        assert_eq!(
            w.check_action(&a_home, None, ClaimAction::FireSpreadFrom(k("overworld", 9, 9))),
            ClaimDecision::Allowed
        );
        w.protection_config_mut(&a, &main).protect_mob_and_boss_grief = false;
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::MobOrBossGrief {
                    source: None,
                    is_boss_or_dragon: true,
                }
            ),
            ClaimDecision::Allowed
        );
    }

    #[test]
    fn cross_border_vehicle_injection_protection() {
        let mut w = ClaimsWorld::new();
        let (a, b, guest) = (uuid(1), uuid(2), uuid(3));
        let main = main_sub();
        let a_home = k("overworld", 0, 0);
        let a_annex = k("overworld", 1, 0);
        let wild = k("overworld", 3, 0);
        w.claim_chunk(&a, a_home.clone(), &main).unwrap();
        w.claim_chunk(&a, a_annex.clone(), &main).unwrap();

        // An outsider pushing a boat/minecart in to kidnap a villager:
        // denied, from wilderness or their own claim alike.
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::VehicleEnterOrInject {
                    source: wild.clone(),
                    actor_uuid: Some(b.clone()),
                }
            ),
            ClaimDecision::Denied(DenyReason::VehicleBorderProtected)
        );
        // The owner driving their own cart in, a guest holding
        // entity_interact permission, and same-owner chunks: all fine.
        for (src, actor) in [
            (wild.clone(), Some(a.clone())),
            (a_annex.clone(), Some(b.clone())),
            (a_annex.clone(), None),
        ] {
            assert_eq!(
                w.check_action(
                    &a_home,
                    None,
                    ClaimAction::VehicleEnterOrInject {
                        source: src,
                        actor_uuid: actor.clone(),
                    }
                ),
                ClaimDecision::Allowed,
                "{actor:?}"
            );
        }
        w.set_player_permission_override(
            &a,
            &main,
            &guest,
            ClaimPermissionSet {
                entity_interact: true,
                ..ClaimPermissionSet::denied_all()
            },
        );
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::VehicleEnterOrInject {
                    source: wild.clone(),
                    actor_uuid: Some(guest.clone()),
                }
            ),
            ClaimDecision::Allowed
        );
        // No actor information at all: still protected.
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::VehicleEnterOrInject {
                    source: wild.clone(),
                    actor_uuid: None,
                }
            ),
            ClaimDecision::Denied(DenyReason::VehicleBorderProtected)
        );
        // Flag off: the border opens.
        w.protection_config_mut(&a, &main).protect_vehicle_border = false;
        assert_eq!(
            w.check_action(
                &a_home,
                None,
                ClaimAction::VehicleEnterOrInject {
                    source: wild,
                    actor_uuid: Some(b),
                }
            ),
            ClaimDecision::Allowed
        );
    }

    #[test]
    fn organic_claim_economy_quotes() {
        let mut w = ClaimsWorld::new();
        let (a, b) = (uuid(1), uuid(2));
        let main = main_sub();
        let policy = OrganicClaimPolicy::default();
        assert_eq!(policy.starter_free_chunks, 1);
        assert_eq!(w.claim_cost_mode(), ClaimCostMode::FreeQuota);

        // The first chunk is the free starter plot.
        let first = k("overworld", 0, 0);
        let q0 = compute_organic_chunk_cost(&w, &a, &first, &policy);
        assert!(q0.is_starter_free);
        assert_eq!(q0.cost_points, 0);
        w.claim_chunk(&a, first, &main).unwrap();

        // The second chunk, edge-adjacent to the starter: 1.00x = 100.
        let q = compute_organic_chunk_cost(&w, &a, &k("overworld", 1, 0), &policy);
        assert!(!q.is_starter_free);
        assert!(q.is_contiguous);
        assert_eq!(q.nearby_foreign_claims, 0);
        assert_eq!(q.density_multiplier_bps, 10_000);
        assert_eq!(q.cost_points, 100);

        // A disconnected island pays the 2.25x scatter penalty.
        let qi = compute_organic_chunk_cost(&w, &a, &k("overworld", 40, 40), &policy);
        assert!(!qi.is_contiguous);
        assert_eq!(qi.cost_points, 225);

        // Fluid density: a foreign claim cluster raises land value toward
        // the 3.0x cap while staying cheap on the frontier.
        let hub = k("overworld", 10, 0);
        w.claim_chunk(&a, k("overworld", 9, 0), &main).unwrap(); // adjacency
        for cz in -3..=0 {
            w.claim_chunk(&b, k("overworld", 12, cz), &main).unwrap();
        }
        let q = compute_organic_chunk_cost(&w, &a, &hub, &policy);
        assert!(q.is_contiguous);
        assert_eq!(q.nearby_foreign_claims, 4);
        assert_eq!(q.density_multiplier_bps, 16_000); // +15% * 4
        assert_eq!(q.cost_points, 160);
        // Ten more foreign claims inside the radius saturate the cap.
        for cz in 1..=3 {
            w.claim_chunk(&b, k("overworld", 12, cz), &main).unwrap();
        }
        for cz in -3..=3 {
            w.claim_chunk(&b, k("overworld", 13, cz), &main).unwrap();
        }
        let q = compute_organic_chunk_cost(&w, &a, &hub, &policy);
        assert_eq!(q.nearby_foreign_claims, 14);
        assert_eq!(q.density_multiplier_bps, 30_000);
        assert_eq!(q.cost_points, 300);
        // An island in a crowded hub compounds both multipliers: only 8
        // of the foreign claims sit within its radius, so density is
        // 2.2x and the island pays 100 * 2.25 * 2.2.
        let q = compute_organic_chunk_cost(&w, &a, &k("overworld", 11, 3), &policy);
        assert!(!q.is_contiguous);
        assert_eq!(q.nearby_foreign_claims, 8);
        assert_eq!(q.density_multiplier_bps, 22_000);
        assert_eq!(q.cost_points, 495);
        // Out on the frontier the same player still pays base price.
        w.claim_chunk(&a, k("overworld", 50, 50), &main).unwrap();
        let q = compute_organic_chunk_cost(&w, &a, &k("overworld", 51, 50), &policy);
        assert!(q.is_contiguous);
        assert_eq!(q.nearby_foreign_claims, 0);
        assert_eq!(q.cost_points, 100);

        // Budgets grow deterministically with playtime, advancements and
        // boss defeats; sub-hour playtime mints nothing.
        assert_eq!(compute_organic_budget(71_999, 0, 0, &policy), 0);
        assert_eq!(compute_organic_budget(72_000, 0, 0, &policy), 120);
        assert_eq!(compute_organic_budget(72_000, 3, 1, &policy), 120 + 180 + 250);
        assert_eq!(compute_organic_budget(144_000, 0, 2, &policy), 240 + 500);
    }

    #[test]
    fn party_umbrella_lifecycle_keeps_individual_ownership() {
        let mut w = ClaimsWorld::new();
        let (owner, member, third) = (uuid(1), uuid(2), uuid(3));
        let main = main_sub();
        let a_home = k("overworld", 0, 0);
        let b_home = k("overworld", 5, 0);
        w.claim_chunk(&owner, a_home.clone(), &main).unwrap();
        w.claim_chunk(&member, b_home.clone(), &main).unwrap();

        // JOIN: ownership never merges — the member keeps 100% of their
        // chunks and gains the owner's configured party access. Claims
        // made while in the party take the party's minimap color.
        let pid = w.create_party(&owner, "Umbrella").unwrap();
        w.invite_player(&owner, &member).unwrap();
        w.accept_invite(&member, Some(&pid)).unwrap();
        assert_eq!(
            w.claim_at(&b_home).unwrap().owner,
            ClaimOwner::Player(member.clone())
        );
        assert_eq!(
            w.claim_at(&a_home).unwrap().owner,
            ClaimOwner::Player(owner.clone())
        );
        assert_eq!(
            w.check_action(&a_home, Some(&member), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
        let party_color = w.parties()[&pid].color_rgb;
        let new_chunk = k("overworld", 1, 0);
        w.claim_chunk(&owner, new_chunk.clone(), &main).unwrap();
        assert_eq!(w.claim_at(&new_chunk).unwrap().color_rgb, party_color);
        assert_eq!(w.claim_display_color(&owner, &main), party_color);

        // KICK: the kicked member keeps every chunk and instantly loses
        // party access — and the party instantly loses access to theirs.
        w.kick_member(&owner, &member).unwrap();
        assert_eq!(
            w.claim_at(&b_home).unwrap().owner,
            ClaimOwner::Player(member.clone())
        );
        assert_eq!(
            w.check_action(&a_home, Some(&member), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::BlocksProtected)
        );
        assert_eq!(
            w.check_action(&b_home, Some(&owner), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::BlocksProtected)
        );

        // LEADER LEAVES with members remaining: leadership transfers to
        // the highest-ranked member instead of stranding the party.
        w.invite_player(&owner, &member).unwrap();
        w.accept_invite(&member, None).unwrap();
        w.invite_player(&owner, &third).unwrap();
        w.accept_invite(&third, None).unwrap();
        w.set_rank(&owner, &member, PartyRank::Moderator).unwrap();
        w.leave_party(&owner).unwrap();
        let party = &w.parties()[&pid];
        assert_eq!(party.owner_uuid, member); // Moderator beats Member
        assert_eq!(party.rank_of(&member), Some(PartyRank::Owner));
        assert_eq!(party.rank_of(&third), Some(PartyRank::Member));
        // The ex-leader loses party access to members' chunks instantly;
        // the new leader's chunks stay open to the remaining member.
        assert_eq!(
            w.check_action(&b_home, Some(&owner), ClaimAction::BlockBreak),
            ClaimDecision::Denied(DenyReason::BlocksProtected)
        );
        assert_eq!(
            w.check_action(&b_home, Some(&third), ClaimAction::BlockBreak),
            ClaimDecision::Allowed
        );
        // Succession chains; the last member out dissolves the party.
        w.leave_party(&member).unwrap();
        assert_eq!(w.parties()[&pid].owner_uuid, third);
        w.leave_party(&third).unwrap();
        assert!(w.parties().is_empty());
        assert!(w.party_of(&third).is_none());
        // Everyone's chunks are still theirs.
        assert_eq!(
            w.claim_at(&b_home).unwrap().owner,
            ClaimOwner::Player(member)
        );
        assert_eq!(
            w.claim_at(&a_home).unwrap().owner,
            ClaimOwner::Player(owner)
        );
    }

    #[test]
    fn survival_default_restricts_claim_dimensions() {
        let mut w = ClaimsWorld::new_survival_default();
        let (a, admin) = (uuid(1), uuid(9));
        w.set_admin(admin.as_str(), true);
        let main = main_sub();

        w.claim_chunk(&a, k("overworld", 0, 0), &main).unwrap();
        for dim in ["the_nether", "the_end", "aeronautics:space"] {
            let key = ClaimChunkKey::new(dim, 0, 0).unwrap();
            assert_eq!(
                w.claim_chunk(&a, key, &main),
                Err(ClaimsError::DimensionNotClaimable(
                    ResourceLocation::parse(dim).unwrap().to_string()
                )),
                "{dim}"
            );
        }
        // Admin takeovers and server claims work in any dimension —
        // spawn protection and portal rooms are still possible.
        w.claim_chunk_as(&admin, true, k("the_nether", 0, 0), &main)
            .unwrap();
        w.claim_server_chunk(true, k("the_end", 0, 0), &main).unwrap();

        // A server can opt more dimensions back in — or lift the
        // restriction entirely.
        w.set_allowed_claim_dimensions(Some(
            ["overworld", "the_nether"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        ))
        .unwrap();
        // The setter normalizes ids like ClaimChunkKey does.
        assert!(w
            .allowed_claim_dimensions()
            .unwrap()
            .contains("minecraft:the_nether"));
        w.claim_chunk(&a, k("the_nether", 1, 0), &main).unwrap();
        w.set_allowed_claim_dimensions(None).unwrap();
        w.claim_chunk(&a, k("the_end", 1, 0), &main).unwrap();
        assert!(w
            .set_allowed_claim_dimensions(Some(
                ["bad dim"].iter().map(|s| (*s).to_owned()).collect()
            ))
            .is_err());

        // ClaimsWorld::new() stays unrestricted for compatibility.
        let mut open = ClaimsWorld::new();
        assert!(open.allowed_claim_dimensions().is_none());
        open.claim_chunk(&a, k("the_nether", 2, 2), &main).unwrap();
    }

    #[test]
    fn permission_matrix_metadata_and_policy_persist() {
        let dir = tmp_dir("matrix");
        let mut w = ClaimsWorld::new_survival_default();
        let (a, b) = (uuid(1), uuid(2));
        let main = main_sub();
        w.record_player_seen(&a, "Alice", 100);
        w.record_player_seen(&b, "Bob", 200);
        w.claim_chunk(&a, k("overworld", 0, 0), &main).unwrap();
        w.set_everyone_else_permissions(&a, &main, ClaimPermissionSet::visitor());
        w.set_player_permission_override(
            &a,
            &main,
            &b,
            ClaimPermissionSet::trusted_builder(),
        );
        {
            let cfg = w.protection_config_mut(&a, &main);
            cfg.protect_mob_and_boss_grief = false;
            cfg.protect_entity_push = true;
            cfg.display_color_rgb = 0x0000_aaff;
            cfg.display_label = Some("Base".to_owned());
        }
        w.set_claim_cost_mode(ClaimCostMode::OrganicFluid);

        w.save_to_dir(&dir).unwrap();
        let bytes_claims = std::fs::read_to_string(dir.join(CLAIMS_FILE)).unwrap();
        let back = ClaimsWorld::load_from_dir(&dir).unwrap();

        // Everything new survives the round trip.
        assert_eq!(back.known_players(), w.known_players());
        assert_eq!(back.claim_cost_mode(), ClaimCostMode::OrganicFluid);
        assert_eq!(
            back.allowed_claim_dimensions()
                .unwrap()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["minecraft:overworld"]
        );
        let bc = back.protection_config(&a, &main);
        assert_eq!(bc.everyone_else, ClaimPermissionSet::visitor());
        assert_eq!(
            bc.player_overrides.get(&b),
            Some(&ClaimPermissionSet::trusted_builder())
        );
        assert!(!bc.protect_mob_and_boss_grief && bc.protect_entity_push);
        assert_eq!(bc.display_color_rgb, 0x0000_aaff);
        assert_eq!(bc.display_label.as_deref(), Some("Base"));
        assert_eq!(back.claim_display_color(&a, &main), 0x0000_aaff);
        assert_eq!(back.claim_display_label(&a, &main).as_deref(), Some("Base"));
        assert_eq!(
            back.effective_permissions_for(&a, &main, &b),
            ClaimPermissionSet::trusted_builder()
        );

        // Saving again is byte-deterministic.
        back.save_to_dir(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(CLAIMS_FILE)).unwrap(),
            bytes_claims
        );

        // A legacy file with none of the new keys loads cleanly and keeps
        // the safe defaults; a legacy permissive flag still grants access.
        std::fs::write(
            dir.join(CLAIMS_FILE),
            r#"{"revision":1,"nextPartyId":1,"admins":[],
                "defaultConfig":{"protectBlocks":true,"protectContainers":false},
                "configs":[],"claims":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join(PARTIES_FILE), r#"{"parties":[]}"#).unwrap();
        let legacy = ClaimsWorld::load_from_dir(&dir).unwrap();
        let dc = legacy.default_config();
        assert!(dc.protect_entity_interact);
        assert!(dc.protect_projectiles_border);
        assert!(dc.protect_fire_and_lightning);
        assert!(dc.protect_mob_and_boss_grief);
        assert!(dc.protect_contraption_border);
        assert!(dc.protect_dispenser_hopper_border);
        assert!(dc.protect_vehicle_border);
        assert!(dc.protect_entity_push);
        assert!(dc.protect_entity_lure);
        assert_eq!(dc.everyone_else, ClaimPermissionSet::denied_all());
        assert!(dc.player_overrides.is_empty());
        assert_eq!(dc.display_color_rgb, 0);
        assert_eq!(dc.display_label, None);
        assert!(legacy.known_players().is_empty());
        assert_eq!(legacy.claim_cost_mode(), ClaimCostMode::FreeQuota);
        assert!(legacy.allowed_claim_dimensions().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
