// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Clean-room chunk claims, parties, allies and force-loading in the spirit of
//! Open Parties and Claims (`openpartiesandclaims`) and FTB Chunks
//! (`ftbchunks`), implemented from scratch on `std` + [`mc_spine_core`].
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
//!   and cross-border piston/fluid grief protection.
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
use crate::json::{self, Json};
use mc_spine_core::id::ResourceLocation;
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
/// claims config and FTB Chunks' team config both carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

impl Default for ClaimProtectionConfig {
    /// The permissive-but-protected default a fresh player starts with.
    fn default() -> Self {
        Self {
            protect_blocks: true,
            protect_containers: true,
            protect_entities: true,
            protect_explosions: true,
            protect_piston_border: true,
            protect_fluid_border: true,
            allow_party_members: true,
            allow_allies: false,
            offline_forceload: false,
            max_claims: 500,
            max_forceloads: 25,
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
    /// An explosion originating anywhere; no actor.
    Explosion,
    /// A piston in the given source chunk moving blocks into the target.
    PistonFrom(ClaimChunkKey),
    /// Fluid in the given source chunk flowing into the target.
    FluidFrom(ClaimChunkKey),
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
    revision: u64,
    deltas: Vec<ClaimsSyncDelta>,
    next_party_id: u64,
}

impl Default for ClaimsWorld {
    fn default() -> Self {
        Self::new()
    }
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
            revision: 0,
            deltas: Vec::new(),
            next_party_id: 1,
        }
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
            .copied()
            .unwrap_or_else(|| {
                if okey == "server" {
                    ClaimProtectionConfig::server()
                } else {
                    self.default_config
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
        let Some(claim) = self.claims.get(key) else {
            return ClaimDecision::Allowed;
        };
        let okey = owner_key(&claim.owner);
        let cfg = self.effective_config(&okey, &claim.sub_claim);
        match &action {
            ClaimAction::PistonFrom(src) => return self.border_decision(&okey, &cfg, src, true),
            ClaimAction::FluidFrom(src) => return self.border_decision(&okey, &cfg, src, false),
            ClaimAction::Explosion => {
                return if cfg.protect_explosions {
                    ClaimDecision::Denied(DenyReason::ExplosionsProtected)
                } else {
                    ClaimDecision::Allowed
                };
            }
            _ => {}
        }
        let (protected, reason) = match action {
            ClaimAction::BlockBreak | ClaimAction::BlockPlace => {
                (cfg.protect_blocks, DenyReason::BlocksProtected)
            }
            ClaimAction::ContainerInteract => {
                (cfg.protect_containers, DenyReason::ContainersProtected)
            }
            ClaimAction::EntityAttack => (cfg.protect_entities, DenyReason::EntitiesProtected),
            _ => unreachable!(),
        };
        if !protected {
            return ClaimDecision::Allowed;
        }
        let actor = actor_uuid.unwrap_or("");
        let is_admin = self.admins.contains(actor);
        match &claim.owner {
            ClaimOwner::Server => {
                if is_admin {
                    ClaimDecision::Allowed
                } else {
                    ClaimDecision::Denied(DenyReason::ServerClaim)
                }
            }
            ClaimOwner::Expired(_) => {
                if is_admin {
                    ClaimDecision::Allowed
                } else {
                    ClaimDecision::Denied(DenyReason::ExpiredClaim)
                }
            }
            ClaimOwner::Player(owner) => {
                if actor == owner || is_admin {
                    return ClaimDecision::Allowed;
                }
                let trusted = (cfg.allow_party_members && self.shares_party(owner, actor))
                    || (cfg.allow_allies && self.allied(owner, actor));
                if self.index_count(&okey, &claim.sub_claim) > cfg.max_claims {
                    // Over-limit: trusted access is suspended; outsiders were
                    // denied anyway, with the action's own reason.
                    return if trusted {
                        ClaimDecision::Denied(DenyReason::OwnerOverLimit)
                    } else {
                        ClaimDecision::Denied(reason)
                    };
                }
                if trusted {
                    ClaimDecision::Allowed
                } else {
                    ClaimDecision::Denied(reason)
                }
            }
        }
    }

    /// Cross-border rule: movement is allowed while source and target share
    /// an owner (same sub-claims too), and blocked otherwise when the target
    /// protects that border. A wilderness source is a different owner.
    fn border_decision(
        &self,
        target_okey: &str,
        cfg: &ClaimProtectionConfig,
        src: &ClaimChunkKey,
        piston: bool,
    ) -> ClaimDecision {
        let same_owner = self
            .claims
            .get(src)
            .map(|c| owner_key(&c.owner) == *target_okey)
            .unwrap_or(false);
        if same_owner {
            return ClaimDecision::Allowed;
        }
        let (protected, reason) = if piston {
            (cfg.protect_piston_border, DenyReason::PistonBorderProtected)
        } else {
            (cfg.protect_fluid_border, DenyReason::FluidBorderProtected)
        };
        if protected {
            ClaimDecision::Denied(reason)
        } else {
            ClaimDecision::Allowed
        }
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

    /// Leave the party. The owner leaving disbands it — every member is
    /// released and pending invites die with the party.
    pub fn leave_party(&mut self, player_uuid: &str) -> Result<(), ClaimsError> {
        let pid = self
            .member_party
            .get(player_uuid)
            .cloned()
            .ok_or(ClaimsError::NotInParty)?;
        let is_owner = self.parties[&pid].owner_uuid == player_uuid;
        if is_owner {
            let party = self.parties.remove(&pid).unwrap();
            for uuid in party.members.keys() {
                self.member_party.remove(uuid);
            }
        } else {
            self.parties
                .get_mut(&pid)
                .unwrap()
                .members
                .remove(player_uuid);
            self.member_party.remove(player_uuid);
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
                    ("dim".to_owned(), Json::String(k.dimension.clone())),
                    ("cx".to_owned(), Json::Number(f64::from(k.cx))),
                    ("cz".to_owned(), Json::Number(f64::from(k.cz))),
                    ("owner".to_owned(), owner_to_json(&c.owner)),
                    ("sub".to_owned(), Json::String(c.sub_claim.clone())),
                    ("forceloaded".to_owned(), Json::Bool(c.forceloaded)),
                    ("color".to_owned(), Json::Number(f64::from(c.color_rgb))),
                ])
            })
            .collect();
        let configs: Vec<Json> = self
            .configs
            .iter()
            .map(|((okey, sub), cfg)| {
                Json::Object(vec![
                    ("owner".to_owned(), Json::String(okey.clone())),
                    ("sub".to_owned(), Json::String(sub.clone())),
                    ("config".to_owned(), config_to_json(cfg)),
                ])
            })
            .collect();
        let admins: Vec<Json> = self
            .admins
            .iter()
            .map(|a| Json::String(a.clone()))
            .collect();
        Json::Object(vec![
            ("revision".to_owned(), Json::Number(self.revision as f64)),
            (
                "nextPartyId".to_owned(),
                Json::Number(self.next_party_id as f64),
            ),
            ("admins".to_owned(), Json::Array(admins)),
            (
                "defaultConfig".to_owned(),
                config_to_json(&self.default_config),
            ),
            ("configs".to_owned(), Json::Array(configs)),
            ("claims".to_owned(), Json::Array(claims)),
        ])
        .to_json_string()
    }

    fn parties_json(&self) -> String {
        let parties: Vec<Json> = self
            .parties
            .values()
            .map(|p| {
                Json::Object(vec![
                    ("id".to_owned(), Json::String(p.id.clone())),
                    ("name".to_owned(), Json::String(p.name.clone())),
                    ("ownerUuid".to_owned(), Json::String(p.owner_uuid.clone())),
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
                    ("color".to_owned(), Json::Number(f64::from(p.color_rgb))),
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
        self.revision = j_u64(&doc, "revision", ctx)?.unwrap_or(0);
        self.next_party_id = j_u64(&doc, "nextPartyId", ctx)?.unwrap_or(1).max(1);
        if let Some(d) = doc.get("defaultConfig") {
            self.default_config = config_from_json(d, ctx)?;
        }
        for admin in j_arr(&doc, "admins", ctx)?.unwrap_or(&[]) {
            let Json::String(a) = admin else {
                return Err(inv(ctx, "\"admins\" must hold strings"));
            };
            self.admins.insert(a.clone());
        }
        for entry in j_arr(&doc, "configs", ctx)?.unwrap_or(&[]) {
            let okey = j_str(entry, "owner", ctx)?;
            let ok =
                okey == "server" || okey.starts_with("player:") || okey.starts_with("expired:");
            if !ok {
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
        for item in j_arr(&doc, "claims", ctx)?.unwrap_or(&[]) {
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
            if self
                .claims
                .insert(
                    key.clone(),
                    ChunkClaim {
                        owner,
                        sub_claim: sub.as_str().to_owned(),
                        forceloaded,
                        color_rgb: color as u32,
                    },
                )
                .is_some()
            {
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
            let id = j_str(item, "id", ctx)?.to_owned();
            let name = j_str(item, "name", ctx)?.to_owned();
            if name.trim().is_empty() || name.len() > 48 {
                return Err(inv(ctx, format!("party {id:?} has a bad name")));
            }
            let owner_uuid = j_str(item, "ownerUuid", ctx)?.to_owned();
            let mut members = BTreeMap::new();
            let Some(Json::Object(mj)) = item.get("members") else {
                return Err(inv(ctx, format!("party {id:?} lacks \"members\"")));
            };
            for (uuid, rank_json) in mj {
                let Json::String(rs) = rank_json else {
                    return Err(inv(
                        ctx,
                        format!("party {id:?} member rank is not a string"),
                    ));
                };
                let rank = PartyRank::parse(rs)
                    .ok_or_else(|| inv(ctx, format!("party {id:?} bad rank {rs:?}")))?;
                members.insert(uuid.clone(), rank);
            }
            if members.get(&owner_uuid) != Some(&PartyRank::Owner) {
                return Err(inv(
                    ctx,
                    format!("party {id:?} ownerUuid is not its Owner member"),
                ));
            }
            let mut allies = BTreeSet::new();
            for a in j_arr(item, "allies", ctx)?.unwrap_or(&[]) {
                let Json::String(a) = a else {
                    return Err(inv(ctx, format!("party {id:?} allies must hold strings")));
                };
                allies.insert(a.clone());
            }
            let mut pending_invites = BTreeSet::new();
            for a in j_arr(item, "invites", ctx)?.unwrap_or(&[]) {
                let Json::String(a) = a else {
                    return Err(inv(ctx, format!("party {id:?} invites must hold strings")));
                };
                pending_invites.insert(a.clone());
            }
            let color = j_u64(item, "color", ctx)?.unwrap_or(0);
            if color > 0x00ff_ffff {
                return Err(inv(ctx, format!("party {id:?} color out of range")));
            }
            let party = Party {
                id: id.clone(),
                name,
                owner_uuid,
                members,
                allies,
                pending_invites,
                color_rgb: color as u32,
            };
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
    let tail = rest
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
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
            let allies: Vec<&String> = p.allies.iter().collect();
            let invites: Vec<&String> = p.pending_invites.iter().collect();
            Ok(format!(
                "party {} \"{}\" color=#{:06x}: members=[{}] allies=[{}] invites=[{}]",
                p.id,
                p.name,
                p.color_rgb,
                members.join(","),
                allies
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                invites
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
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
    let recipients: Vec<&String> = party
        .members
        .keys()
        .filter(|u| u.as_str() != actor_uuid)
        .collect();
    Ok(format!(
        "[party] {actor_name}: {rest} (recipients: {})",
        recipients
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",")
    ))
}

/// `key=value` dump, in declaration order.
fn config_dump(c: &ClaimProtectionConfig) -> String {
    format!(
        "protectBlocks={} protectContainers={} protectEntities={} protectExplosions={} \
         protectPistonBorder={} protectFluidBorder={} allowPartyMembers={} allowAllies={} \
         offlineForceload={} maxClaims={} maxForceloads={}",
        c.protect_blocks,
        c.protect_containers,
        c.protect_entities,
        c.protect_explosions,
        c.protect_piston_border,
        c.protect_fluid_border,
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
        "allowpartymembers" => cfg.allow_party_members = boolean(value)?,
        "allowallies" => cfg.allow_allies = boolean(value)?,
        "offlineforceload" => cfg.offline_forceload = boolean(value)?,
        "maxclaims" => cfg.max_claims = number(value)?,
        "maxforceloads" => cfg.max_forceloads = number(value)?,
        _ => return Err(format!("unknown config key {key:?}")),
    }
    Ok(())
}

// ------------------------------------------------------------ JSON helpers

fn inv(ctx: &str, msg: impl fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{ctx}: {msg}"))
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
    Json::Object(vec![
        ("protectBlocks".to_owned(), Json::Bool(c.protect_blocks)),
        (
            "protectContainers".to_owned(),
            Json::Bool(c.protect_containers),
        ),
        ("protectEntities".to_owned(), Json::Bool(c.protect_entities)),
        (
            "protectExplosions".to_owned(),
            Json::Bool(c.protect_explosions),
        ),
        (
            "protectPistonBorder".to_owned(),
            Json::Bool(c.protect_piston_border),
        ),
        (
            "protectFluidBorder".to_owned(),
            Json::Bool(c.protect_fluid_border),
        ),
        (
            "allowPartyMembers".to_owned(),
            Json::Bool(c.allow_party_members),
        ),
        ("allowAllies".to_owned(), Json::Bool(c.allow_allies)),
        (
            "offlineForceload".to_owned(),
            Json::Bool(c.offline_forceload),
        ),
        ("maxClaims".to_owned(), Json::Number(c.max_claims as f64)),
        (
            "maxForceloads".to_owned(),
            Json::Number(c.max_forceloads as f64),
        ),
    ])
}

/// Strict types, lenient presence: every key that is there must have the
/// right type; absent keys keep `base`'s value.
fn config_from_json(v: &Json, ctx: &str) -> io::Result<ClaimProtectionConfig> {
    let Json::Object(_) = v else {
        return Err(inv(ctx, "config must be a JSON object"));
    };
    let mut c = ClaimProtectionConfig::default();
    if let Some(b) = j_bool(v, "protectBlocks", ctx)? {
        c.protect_blocks = b;
    }
    if let Some(b) = j_bool(v, "protectContainers", ctx)? {
        c.protect_containers = b;
    }
    if let Some(b) = j_bool(v, "protectEntities", ctx)? {
        c.protect_entities = b;
    }
    if let Some(b) = j_bool(v, "protectExplosions", ctx)? {
        c.protect_explosions = b;
    }
    if let Some(b) = j_bool(v, "protectPistonBorder", ctx)? {
        c.protect_piston_border = b;
    }
    if let Some(b) = j_bool(v, "protectFluidBorder", ctx)? {
        c.protect_fluid_border = b;
    }
    if let Some(b) = j_bool(v, "allowPartyMembers", ctx)? {
        c.allow_party_members = b;
    }
    if let Some(b) = j_bool(v, "allowAllies", ctx)? {
        c.allow_allies = b;
    }
    if let Some(b) = j_bool(v, "offlineForceload", ctx)? {
        c.offline_forceload = b;
    }
    if let Some(n) = j_u64(v, "maxClaims", ctx)? {
        c.max_claims = usize::try_from(n).map_err(|_| inv(ctx, "maxClaims too large"))?;
    }
    if let Some(n) = j_u64(v, "maxForceloads", ctx)? {
        c.max_forceloads = usize::try_from(n).map_err(|_| inv(ctx, "maxForceloads too large"))?;
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
        // Owner leaving disbands.
        w.leave_party(&owner).unwrap();
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
}
