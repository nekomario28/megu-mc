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
//! * [`admin_cli`] — the admin command line shared by the `megu-init` and
//!   `megu-admin` binaries: `init`, `doctor`, `op`/`deop`, `ban`/`pardon`,
//!   `whitelist` and `claims`, compiled once into the library.
//! * [`atomic`] — [`atomic::write_atomic`]: sibling-temp-file, `sync_all`,
//!   rename. A crash leaves the old file or the new one, never a truncated
//!   one.
//! * [`json`] — the minimal bounded-depth JSON reader/writer the admin
//!   registries share, delegated to the canonical [`megu_core::json`].
//!   Malformed input is an error, never a partial document.
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
//! * [`installer`] — the `megu-installer` lanes: Official Launcher
//!   (`install_client`/`uninstall_client`), Prism Launcher
//!   (`install_prism`) and dedicated-server (`install_server`) installs
//!   plus pure cross-OS directory detection, a `megu-core` SHA-256 for
//!   `--client-bin` pinning and clock-free ISO-8601 stamps.
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
//! * [`server_tui`] — the native smart server console: the
//!   `--cli`/`--tui`/`--no-banner` mode resolver that fails closed to plain
//!   passthrough off a real terminal, the log-line decoder and noise-filtered
//!   categorized log, the pinned three-line HUD renderer
//!   ([`server_tui::ServerTuiState::render_frame`]), and the interactive key
//!   map — a pure-`std` port of `tools/megu_tui.py`.

pub mod admin_cli;
pub mod atomic;
pub mod claims;
pub mod claims_wire;
pub mod custom_portal;
pub mod directory;
pub mod directional_shadow;
pub mod eula;
pub mod installer;
pub mod json;
pub mod lists;
pub mod mods_dir;
pub mod optifine_compat;
pub mod pack_loader;
pub mod pack_router;
pub mod perf_cache;
pub mod properties;
pub mod server_tui;
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
pub use directional_shadow::{
    compute_sun_direction, dynamic_light_level_at, effective_gameplay_light,
    sample_directional_shadow, shadow_allows_undead_burn, shadow_crop_growth_multiplier,
    DirectionalShadowConfig, DynamicGameplayLight, ShadowSample,
};
pub use eula::{check_eula, write_default_eula, EulaStatus, EULA_FILE, EULA_LINK};
pub use installer::{
    detect_minecraft_dir, detect_minecraft_dir_for_os, detect_prism_instances_dir,
    detect_prism_instances_dir_for_os, home_dir, host_os_kind, install_client, install_prism,
    install_server, iso_utc_now, normalize_sha256_hex, prism_instance_name, reject_traversal,
    resolve_minecraft_dir, resolve_prism_instances_dir, sha256_file, sha256_hex,
    uninstall_client, unix_seconds_to_iso8601, version_id, ClientInstallOptions, InstallReport,
    OsKind, ReportEntry, ServerInstallOptions, Sha256, BOOTSTRAP_LIBRARY_COORD,
    BOOTSTRAP_LIBRARY_REL, BOOTSTRAP_MAIN_CLASS, BOOTSTRAP_PATCH_FILE, BOOTSTRAP_PATCH_UID,
    FLATPAK_MINECRAFT_DIR, FLATPAK_PRISM_INSTANCES_DIR, LAUNCHER_PROFILES_FILE,
    LAUNCHER_PROFILES_STORE_FILE, MAX_BOOTSTRAP_JAR_BYTES, RUN_BAT, RUN_SH, SERVER_README,
};
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
pub use server_tui::{
    ansi_color_ok, is_banner_line, resolve_console_mode, resolve_console_mode_env, sparkline,
    startup_banner, strip_ansi, vlen, ConsoleModeOptions, DualLoader, Hotspot, LogCategory,
    LogEntry, LogFilterMode, LogLineKind, ModCounts, ModDetail, ServerConsoleMode,
    ServerTuiState, TuiAction, TuiKey, TuiView, BANNER_ASCII, BANNER_UNICODE,
};
pub use usercache::{UserCache, UserCacheEntry, DEFAULT_CAPACITY, USERCACHE_FILE};
