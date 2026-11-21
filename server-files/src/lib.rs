// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc-spine-server-files`: the Vanilla server's on-disk files, standalone.
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
//!   [`properties::ServerProperties`] view with Vanilla defaults.
//! * [`eula`] — [`eula::check_eula`] (`Accepted`/`NotAccepted`/`Missing`) and
//!   [`eula::write_default_eula`], which always writes `eula=false`.
//! * [`lists`] — `ops.json` (Vanilla levels 0..=4 plus the legacy flat name
//!   array upgrade), `whitelist.json`, `banned-players.json` and
//!   `banned-ips.json`.
//! * [`usercache`] — `usercache.json`: a bounded `uuid` <-> `name` most
//!   recently used cache.

pub mod atomic;
pub mod eula;
pub mod json;
pub mod lists;
pub mod properties;
pub mod usercache;

pub use atomic::write_atomic;
pub use eula::{check_eula, write_default_eula, EulaStatus, EULA_FILE, EULA_LINK};
pub use json::{Json, JsonError};
pub use lists::{
    normalize_ban_ip, offline_uuid, valid_player_name, BanEntry, BannedIps, BannedPlayers,
    IpBanEntry, OpEntry, OpsList, Whitelist, WhitelistEntry, BANNED_IPS_FILE, BANNED_PLAYERS_FILE,
    OPS_FILE, WHITELIST_FILE,
};
pub use properties::{MalformedEscape, PropertiesFile, ServerProperties, PROPERTIES_FILE};
pub use usercache::{UserCache, UserCacheEntry, DEFAULT_CAPACITY, USERCACHE_FILE};
