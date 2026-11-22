// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu-server-files`: the Vanilla server's on-disk files, standalone.
//!
//! A zero-dependency crate owning the file formats a dedicated server shares
//! with Vanilla — `server.properties`, `eula.txt`, the four admin registries
//! (`ops.json`, `whitelist.json`, `banned-players.json`, `banned-ips.json`) and
//! `usercache.json` — plus the atomic persistence discipline every save uses.
//! The schemas byte-match what `src/server_ops.rs` and `src/server_config.rs`
//! already read and write, so a directory managed here stays interchangeable
//! with a real Vanilla server.
//!
//! Layers:
//!
//! * [`atomic`] — [`atomic::write_atomic`]: sibling-temp-file, `sync_all`,
//!   rename. A crash leaves the old file or the new one, never a truncated
//!   one.
//! * [`json`] — the minimal bounded-depth JSON reader/writer the admin
//!   registries share. Malformed input is an error, never a partial document.
//! * [`properties`] — a lossless Java `.properties` document (comments and
//!   entry order survive a parse/store round trip) and the typed
//!   [`properties::ServerProperties`] view with Vanilla defaults, including
//!   the strictly opt-in fair-play map policy
//!   ([`properties::ServerFairPlayPolicy`]).
//! * [`optifine_compat`] — bounded clean-room parsers for OptiFine/EMF CEM
//!   `.jem`/`.jpm` models and CTM/emissive `.properties` files.
//! * [`eula`] — [`eula::check_eula`] (`Accepted`/`NotAccepted`/`Missing`) and
//!   [`eula::write_default_eula`], which always writes `eula=false`.
//! * [`lists`] — `ops.json` (Vanilla levels 0..=4 plus the legacy flat name
//!   array upgrade), `whitelist.json`, `banned-players.json` and
//!   `banned-ips.json`.
//! * [`usercache`] — `usercache.json`: a bounded `uuid` <-> `name` most
//!   recently used cache.
//! * [`claims`] — a clean-room OpenPAC/FTB-Chunks-style engine: chunk claims
//!   with per-owner sub-claims and quotas, parties/allies, force-loading
//!   budgets, the `/oclaims`/`/oparties`/`/opac`/`/opm` command dispatcher, a
//!   monotonic sync-delta feed for claims overlays, and atomic
//!   `openpac_claims.json`/`openpac_parties.json` persistence.
//! * [`claims_wire`] — the deterministic `CustomPacketPayload` codec that
//!   carries those claims and sync deltas to client overlays over the
//!   `openpartiesandclaims:claims_sync` / `ftbchunks:claims_sync` /
//!   `megu:claims_v1` channels.
//! * [`directory`] — [`directory::ServerDirectory`], the facade that owns the
//!   whole set: `load_or_init`, Vanilla-precedence `admit_login` and
//!   `save_all_atomic`, pinned to a [`megu_core::version::TargetVersion`].
//!   Also home to the standard folder layouts:
//!   [`directory::ensure_server_game_dir_layout`] /
//!   [`directory::ensure_client_game_dir_layout`] scaffold the
//!   Vanilla/Fabric/NeoForge dedicated-server and `.minecraft`/Prism/Modrinth
//!   client `<gameDir>` trees idempotently and fail closed on traversal
//!   `level-name`s and symlinks.
//! * [`mods_dir`] — [`mods_dir::scan_mods_folder`], the launcher-grade
//!   `mods/` scanner: deterministic `.jar` discovery (plus the optional
//!   `<mc_version>` subfolder) with a typed [`mods_dir::ModsDirSkipReason`]
//!   for every entry it sets aside.
//! * [`perf_cache`] — zero-side-effect performance caches: a shared
//!   `Arc<[u8]>` compressed chunk packet cache keyed on every
//!   rendering/protocol discriminator, a view-direction chunk send queue
//!   covering the exact Vanilla `(2d + 1)^2` square, and a hash-verified
//!   `SBC1` startup datapack/registry binary cache that fails closed to a
//!   clean miss on any stale or corrupted input.

pub mod atomic;
pub mod claims;
pub mod claims_wire;
pub mod directory;
pub mod eula;
pub mod json;
pub mod lists;
pub mod mods_dir;
pub mod optifine_compat;
pub mod pack_loader;
pub mod pack_router;
pub mod perf_cache;
pub mod properties;
pub mod usercache;

pub use atomic::write_atomic;
pub use claims::{
    exec_openpac_command, ClaimAction, ClaimChunkKey, ClaimDecision, ClaimOwner,
    ClaimProtectionConfig, ClaimsError, ClaimsSyncDelta, ClaimsWorld, ChunkClaim, DenyReason,
    Party, PartyRank, SubClaimId, CLAIMS_FILE, PARTIES_FILE,
};
pub use claims_wire::{
    build_delta_batches, build_dimension_sync_batches, decode_claims_batch,
    decode_custom_payload_frame, encode_claims_batch, encode_custom_payload_frame, ClaimsWireBatch,
    ClaimsWireCell, ClaimsWireEntry, ClaimsWireError, WireOwnerKind,
    CHANNEL_FTBCHUNKS_CLAIMS_SYNC, CHANNEL_MEGU_CLAIMS_V1, CHANNEL_OPENPAC_CLAIMS_SYNC,
    MAX_WIRE_CLAIMS_PER_BATCH, MAX_WIRE_STRING_BYTES, WIRE_PROTOCOL_TAG,
};
pub use directory::{
    ensure_client_game_dir_layout, ensure_server_game_dir_layout, AdmissionDecision,
    FolderOutcome, ServerDirectory, StandardLayoutEntry, StandardLayoutReport,
    CLIENT_STANDARD_FOLDERS, SERVER_STANDARD_FOLDERS,
};
pub use eula::{check_eula, write_default_eula, EulaStatus, EULA_FILE, EULA_LINK};
pub use json::{Json, JsonError};
pub use lists::{
    normalize_ban_ip, offline_uuid, valid_player_name, BanEntry, BannedIps, BannedPlayers,
    IpBanEntry, OpEntry, OpsList, Whitelist, WhitelistEntry, BANNED_IPS_FILE, BANNED_PLAYERS_FILE,
    OPS_FILE, WHITELIST_FILE,
};
pub use mods_dir::{scan_mods_folder, ModsDirSkipReason, ModsFolderScan, SkippedModEntry};
pub use pack_router::{
    inspect_and_route_pack, PackCapabilities, PackKind, PackRouteDecision, PackRouterPolicy,
    PackRoutingReport, PackThreat, PackThreatKind, ShaderFallbackTarget,
};
pub use perf_cache::{
    look_dir_from_yaw, CacheLookup, CacheMissReason, ChunkCacheStats, ChunkWireKey,
    DirectionalChunkQueue, SharedChunkWireCache, StartupBinaryCache, StartupInputFile,
    StartupInputFingerprint, CACHE_SCHEMA_VERSION, STARTUP_CACHE_MAGIC,
};
pub use properties::{
    can_see_player_on_radar, players_are_party_or_allies, MalformedEscape, MinimapMode,
    PlayerRadarScope, PropertiesFile, ServerFairPlayPolicy, ServerProperties, PROPERTIES_FILE,
};
pub use usercache::{UserCache, UserCacheEntry, DEFAULT_CAPACITY, USERCACHE_FILE};
