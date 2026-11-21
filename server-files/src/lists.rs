//! The Vanilla admin registries: `ops.json`, `whitelist.json`,
//! `banned-players.json` and `banned-ips.json`.
//!
//! The on-disk schemas byte-match what `src/server_ops.rs` writes, so a file
//! this crate saves is interchangeable with one a real Vanilla server (or the
//! existing mc-spine runtime) produces: pretty-printed arrays of objects,
//! `level` clamped to `0..=4`, `expires` as `"forever"` or an ISO date, and a
//! legacy flat name array in `ops.json` upgraded to level-4 operators.
//!
//! Corrupt input fails closed — [`std::io::ErrorKind::InvalidData`] — because
//! a silently empty operator or ban list is a security regression, not a
//! repair.

use crate::atomic::write_atomic;
use crate::json::{self, Json};
use std::io;
use std::path::Path;

/// The four files' Vanilla names (`ops.json` included — `commands_core` uses
/// the same name for the same file).
pub const OPS_FILE: &str = "ops.json";
pub const WHITELIST_FILE: &str = "whitelist.json";
pub const BANNED_PLAYERS_FILE: &str = "banned-players.json";
pub const BANNED_IPS_FILE: &str = "banned-ips.json";

// ------------------------------------------------------------- identifiers

/// Vanilla's player-name rule: 1..=16 chars of `[A-Za-z0-9_]`.
pub fn valid_player_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The offline-mode UUID Vanilla derives for a name (`OfflinePlayer:` + name,
/// MD5 with the version-3/variant bits set), rendered dashed the way the
/// Vanilla files write it.
pub fn offline_uuid(name: &str) -> String {
    let mut digest = md5(format!("OfflinePlayer:{name}").as_bytes());
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// RFC 1321 MD5 — needed for the name-based offline UUIDs, no external crate.
fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20,
        5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_le_bytes());
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in msg.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            b = b
                .wrapping_add(a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]).rotate_left(S[i]));
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

// -------------------------------------------------------------- parse glue

fn invalid(path: &Path, why: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("could not parse {}: {why}", path.display()),
    )
}

/// The top-level array a list file must be, or `InvalidData`.
fn parse_list(path: &Path, text: &str) -> io::Result<Vec<Json>> {
    match json::parse(text).map_err(|e| invalid(path, &e))? {
        Json::Array(items) => Ok(items),
        other => Err(invalid(path, format!("expected an array, found {other:?}"))),
    }
}

fn read_list_file(path: &Path) -> io::Result<Option<Vec<Json>>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(parse_list(path, &text)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn str_field(item: &Json, key: &str) -> Option<String> {
    item.get_str(key).map(str::to_owned)
}

/// The pretty layout Vanilla's Gson writer produces (two-space indents) and
/// `src/server_ops.rs` already emits.
struct Writer {
    out: String,
}

impl Writer {
    fn new() -> Self {
        Self {
            out: String::from("[\n"),
        }
    }
    fn object(&mut self, fields: &[(&str, String)], last: bool) {
        self.out.push_str("  {\n");
        for (i, (key, value)) in fields.iter().enumerate() {
            self.out.push_str(&format!("    {}: {}", json::quote(key), value));
            self.out
                .push_str(if i + 1 == fields.len() { "\n" } else { ",\n" });
        }
        self.out.push_str(if last { "  }\n" } else { "  },\n" });
    }
    fn finish(mut self) -> String {
        self.out.push_str("]\n");
        self.out
    }
}

// ------------------------------------------------------------------ ops.json

/// One Vanilla `ops.json` object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpEntry {
    pub uuid: String,
    pub name: String,
    /// Vanilla permission level, clamped to `0..=4` on read and on write.
    pub level: u8,
    pub bypasses_player_limit: bool,
}

/// `ops.json`: Vanilla objects, plus the legacy flat name array
/// (`["alice","bob"]`) which reads as level-4 operators like the old schema
/// meant — Vanilla's default `op-permission-level`.
#[derive(Clone, Default, Debug)]
pub struct OpsList {
    pub entries: Vec<OpEntry>,
}

impl OpsList {
    /// Missing file is an empty list; malformed JSON is an error for the
    /// caller to report (a corrupt operator file must not silently de-op
    /// everyone).
    pub fn load_from(path: &Path) -> io::Result<Self> {
        let Some(items) = read_list_file(path)? else {
            return Ok(Self::default());
        };
        let mut entries = Vec::new();
        for item in &items {
            match item {
                Json::String(name) if valid_player_name(name) => entries.push(OpEntry {
                    uuid: offline_uuid(name),
                    name: name.clone(),
                    level: 4,
                    bypasses_player_limit: false,
                }),
                Json::String(_) => {
                    return Err(invalid(path, "legacy entry is not a valid player name"))
                }
                obj => {
                    let name = str_field(obj, "name")
                        .ok_or_else(|| invalid(path, "entry has no \"name\""))?;
                    entries.push(OpEntry {
                        uuid: str_field(obj, "uuid").unwrap_or_else(|| offline_uuid(&name)),
                        name,
                        level: obj.get_i64("level", 4).clamp(0, 4) as u8,
                        bypasses_player_limit: obj.get_bool("bypassesPlayerLimit", false),
                    });
                }
            }
        }
        Ok(Self { entries })
    }

    /// Serialize as the Vanilla object array (legacy entries long upgraded).
    pub fn to_json(&self) -> String {
        let mut w = Writer::new();
        for (i, e) in self.entries.iter().enumerate() {
            w.object(
                &[
                    ("uuid", json::quote(&e.uuid)),
                    ("name", json::quote(&e.name)),
                    ("level", format!("{}", e.level.min(4))),
                    (
                        "bypassesPlayerLimit",
                        if e.bypasses_player_limit {
                            "true".to_owned()
                        } else {
                            "false".to_owned()
                        },
                    ),
                ],
                i + 1 == self.entries.len(),
            );
        }
        w.finish()
    }

    /// Atomically write [`Self::to_json`] to `path`.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, self.to_json().as_bytes())
    }

    fn find(&self, name: &str) -> Option<&OpEntry> {
        self.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// `0` when the name is not an operator, else its `0..=4` level.
    pub fn op_level_of(&self, name: &str) -> u8 {
        self.find(name).map(|e| e.level).unwrap_or(0)
    }

    /// Presence in the file — even a level-0 operator counts, as Vanilla's
    /// `opList.contains(profile)` does for the whitelist bypass.
    pub fn is_op(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    pub fn bypasses_player_limit(&self, name: &str) -> bool {
        self.find(name).map(|e| e.bypasses_player_limit).unwrap_or(false)
    }

    /// Insert or update an operator (new entries get the offline UUID);
    /// `true` when anything changed.
    pub fn set(&mut self, name: &str, level: u8, bypasses_player_limit: bool) -> bool {
        let level = level.min(4);
        if let Some(entry) = self.entries.iter_mut().find(|e| e.name.eq_ignore_ascii_case(name)) {
            let changed = entry.level != level || entry.bypasses_player_limit != bypasses_player_limit;
            entry.level = level;
            entry.bypasses_player_limit = bypasses_player_limit;
            return changed;
        }
        self.entries.push(OpEntry {
            uuid: offline_uuid(name),
            name: name.to_owned(),
            level,
            bypasses_player_limit,
        });
        true
    }

    /// `true` when an operator was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !e.name.eq_ignore_ascii_case(name));
        self.entries.len() != before
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }
}

// ------------------------------------------------------------ whitelist.json

/// One `whitelist.json` object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhitelistEntry {
    pub uuid: String,
    pub name: String,
}

/// The whitelist file. Its enforcement switch is not here — Vanilla keeps it
/// in `server.properties` (`white-list=`/`enforce-whitelist=`), so the file
/// schema stays exactly `[{"uuid":…,"name":…}]`.
#[derive(Clone, Default, Debug)]
pub struct Whitelist {
    pub entries: Vec<WhitelistEntry>,
}

impl Whitelist {
    pub fn load_from(path: &Path) -> io::Result<Self> {
        let Some(items) = read_list_file(path)? else {
            return Ok(Self::default());
        };
        let mut entries = Vec::new();
        for item in &items {
            entries.push(WhitelistEntry {
                uuid: str_field(item, "uuid").unwrap_or_default(),
                name: str_field(item, "name")
                    .ok_or_else(|| invalid(path, "entry has no \"name\""))?,
            });
        }
        Ok(Self { entries })
    }

    pub fn to_json(&self) -> String {
        let mut w = Writer::new();
        for (i, e) in self.entries.iter().enumerate() {
            w.object(
                &[
                    ("uuid", json::quote(&e.uuid)),
                    ("name", json::quote(&e.name)),
                ],
                i + 1 == self.entries.len(),
            );
        }
        w.finish()
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, self.to_json().as_bytes())
    }

    /// Name lookup is case-insensitive, like the login check.
    pub fn contains_name(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// UUID lookup; the dashed hex spelling is case-insensitive, as a UUID
    /// denotes the same value either way.
    pub fn contains_uuid(&self, uuid: &str) -> bool {
        self.entries.iter().any(|e| e.uuid.eq_ignore_ascii_case(uuid))
    }

    /// Add a player by name (offline UUID); `true` when the list changed.
    pub fn add(&mut self, name: &str) -> bool {
        self.add_with_uuid(&offline_uuid(name), name)
    }

    /// Add a player with a resolved UUID; `true` when the list changed.
    pub fn add_with_uuid(&mut self, uuid: &str, name: &str) -> bool {
        if self.contains_name(name) || self.contains_uuid(uuid) {
            return false;
        }
        self.entries.push(WhitelistEntry {
            uuid: uuid.to_owned(),
            name: name.to_owned(),
        });
        true
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !e.name.eq_ignore_ascii_case(name));
        self.entries.len() != before
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }
}

// -------------------------------------------------------------- ban entries

/// One `banned-players.json` object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BanEntry {
    pub uuid: String,
    pub name: String,
    pub created: String,
    pub source: String,
    /// `"forever"` or an ISO date, as Vanilla writes it.
    pub expires: String,
    pub reason: String,
}

/// One `banned-ips.json` object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpBanEntry {
    pub ip: String,
    pub created: String,
    pub source: String,
    pub expires: String,
    pub reason: String,
}

/// Canonical IP form for comparison: an `IpAddr` round-trip collapses IPv6
/// spelling and leading zeros; a stray leading `/` (Java `InetAddress` form)
/// is dropped; anything else compares case-insensitively, covering Vanilla
/// hostname bans.
pub fn normalize_ban_ip(ip: &str) -> String {
    let ip = ip.trim().trim_start_matches('/');
    ip.parse::<std::net::IpAddr>()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| ip.to_ascii_lowercase())
}

fn ban_fields(e: &BanEntry) -> [(&str, String); 6] {
    [
        ("uuid", json::quote(&e.uuid)),
        ("name", json::quote(&e.name)),
        ("created", json::quote(&e.created)),
        ("source", json::quote(&e.source)),
        ("expires", json::quote(&e.expires)),
        ("reason", json::quote(&e.reason)),
    ]
}

fn ip_ban_fields(e: &IpBanEntry) -> [(&str, String); 5] {
    [
        ("ip", json::quote(&e.ip)),
        ("created", json::quote(&e.created)),
        ("source", json::quote(&e.source)),
        ("expires", json::quote(&e.expires)),
        ("reason", json::quote(&e.reason)),
    ]
}

// ---------------------------------------------------------- banned players

/// `banned-players.json`.
#[derive(Clone, Default, Debug)]
pub struct BannedPlayers {
    pub entries: Vec<BanEntry>,
}

impl BannedPlayers {
    pub fn load_from(path: &Path) -> io::Result<Self> {
        let Some(items) = read_list_file(path)? else {
            return Ok(Self::default());
        };
        let mut entries = Vec::new();
        for item in &items {
            entries.push(BanEntry {
                uuid: str_field(item, "uuid").unwrap_or_default(),
                name: str_field(item, "name")
                    .ok_or_else(|| invalid(path, "entry has no \"name\""))?,
                created: str_field(item, "created").unwrap_or_default(),
                source: str_field(item, "source").unwrap_or_else(|| "Server".into()),
                expires: str_field(item, "expires").unwrap_or_else(|| "forever".into()),
                reason: str_field(item, "reason").unwrap_or_default(),
            });
        }
        Ok(Self { entries })
    }

    pub fn to_json(&self) -> String {
        let mut w = Writer::new();
        for (i, e) in self.entries.iter().enumerate() {
            w.object(&ban_fields(e), i + 1 == self.entries.len());
        }
        w.finish()
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, self.to_json().as_bytes())
    }

    /// Player-name lookup is case-insensitive, like the login reservation.
    pub fn find(&self, name: &str) -> Option<&BanEntry> {
        self.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// Ban `name` (offline UUID, `expires` `"forever"`); `created`/`source`
    /// are the caller's stamp, as this crate keeps no clock. `true` when the
    /// ban was added.
    pub fn ban(&mut self, name: &str, reason: &str, source: &str, created: &str) -> bool {
        if self.find(name).is_some() {
            return false;
        }
        self.entries.push(BanEntry {
            uuid: offline_uuid(name),
            name: name.to_owned(),
            created: created.to_owned(),
            source: source.to_owned(),
            expires: "forever".to_owned(),
            reason: reason.to_owned(),
        });
        true
    }

    /// `true` when an entry was removed.
    pub fn pardon(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !e.name.eq_ignore_ascii_case(name));
        self.entries.len() != before
    }
}

// -------------------------------------------------------------- banned ips

/// `banned-ips.json`.
#[derive(Clone, Default, Debug)]
pub struct BannedIps {
    pub entries: Vec<IpBanEntry>,
}

impl BannedIps {
    pub fn load_from(path: &Path) -> io::Result<Self> {
        let Some(items) = read_list_file(path)? else {
            return Ok(Self::default());
        };
        let mut entries = Vec::new();
        for item in &items {
            entries.push(IpBanEntry {
                ip: str_field(item, "ip")
                    .map(|ip| normalize_ban_ip(&ip))
                    .ok_or_else(|| invalid(path, "entry has no \"ip\""))?,
                created: str_field(item, "created").unwrap_or_default(),
                source: str_field(item, "source").unwrap_or_else(|| "Server".into()),
                expires: str_field(item, "expires").unwrap_or_else(|| "forever".into()),
                reason: str_field(item, "reason").unwrap_or_default(),
            });
        }
        Ok(Self { entries })
    }

    pub fn to_json(&self) -> String {
        let mut w = Writer::new();
        for (i, e) in self.entries.iter().enumerate() {
            w.object(&ip_ban_fields(e), i + 1 == self.entries.len());
        }
        w.finish()
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, self.to_json().as_bytes())
    }

    pub fn find(&self, ip: &str) -> Option<&IpBanEntry> {
        let ip = normalize_ban_ip(ip);
        self.entries.iter().find(|e| e.ip == ip)
    }

    /// `true` when the ban was added (`false` = already banned).
    pub fn ban(&mut self, ip: &str, reason: &str, source: &str, created: &str) -> bool {
        let ip = normalize_ban_ip(ip);
        if self.find(&ip).is_some() {
            return false;
        }
        self.entries.push(IpBanEntry {
            ip,
            created: created.to_owned(),
            source: source.to_owned(),
            expires: "forever".to_owned(),
            reason: reason.to_owned(),
        });
        true
    }

    pub fn pardon(&mut self, ip: &str) -> bool {
        let ip = normalize_ban_ip(ip);
        let before = self.entries.len();
        self.entries.retain(|e| e.ip != ip);
        self.entries.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("server-files-lists-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn offline_uuid_matches_vanilla_name_based_shape() {
        // Deterministic, dashed, version-3 (nibble 3 at index 14), variant
        // 10xx at index 19 — Vanilla's `OfflinePlayer:` MD5 derivation.
        let uuid = offline_uuid("Notch");
        assert_eq!(uuid, offline_uuid("Notch"));
        assert_eq!(uuid.len(), 36, "{uuid}");
        assert_eq!(&uuid[14..15], "3", "{uuid}");
        assert!("89ab".contains(&uuid[19..20]), "{uuid}");
        assert_ne!(offline_uuid("Notch"), offline_uuid("notch")); // case matters
    }

    #[test]
    fn md5_matches_rfc_vectors() {
        let hex = |d: &[u8]| md5(d).iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn ops_levels_and_legacy_upgrade() {
        let dir = tmp_dir("ops");
        let path = dir.join(OPS_FILE);

        // Vanilla object schema: levels kept 0..=4, bypassesPlayerLimit honored.
        std::fs::write(
            &path,
            r#"[
              {"uuid":"00000000-0000-0000-0000-000000000001","name":"Root","level":4,"bypassesPlayerLimit":true},
              {"uuid":"00000000-0000-0000-0000-000000000002","name":"Mod","level":3,"bypassesPlayerLimit":false},
              {"uuid":"00000000-0000-0000-0000-000000000003","name":"Honorary","level":0,"bypassesPlayerLimit":false},
              {"uuid":"00000000-0000-0000-0000-000000000004","name":"Over","level":9,"bypassesPlayerLimit":false}
            ]"#,
        )
        .unwrap();
        let ops = OpsList::load_from(&path).unwrap();
        assert_eq!(ops.op_level_of("root"), 4); // case-insensitive
        assert_eq!(ops.op_level_of("MOD"), 3);
        assert_eq!(ops.op_level_of("Honorary"), 0);
        assert!(ops.is_op("honorary")); // level 0 is still an operator
        assert_eq!(ops.op_level_of("Over"), 4); // clamped on read
        assert_eq!(ops.op_level_of("stranger"), 0);
        assert!(ops.bypasses_player_limit("ROOT"));
        assert!(!ops.bypasses_player_limit("Mod"));

        // The legacy flat array upgrades to level-4 operators and saves back
        // as Vanilla objects.
        std::fs::write(&path, r#"["alice","bob"]"#).unwrap();
        let mut ops = OpsList::load_from(&path).unwrap();
        assert_eq!(ops.op_level_of("alice"), 4);
        assert_eq!(ops.op_level_of("BOB"), 4);
        assert_eq!(ops.entries[0].uuid, offline_uuid("alice"));
        ops.set("carol", 2, true);
        ops.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"level\": 4"), "{text}");
        assert!(text.contains("\"bypassesPlayerLimit\": true"), "{text}");
        let mut reread = OpsList::load_from(&path).unwrap();
        assert_eq!(reread.op_level_of("carol"), 2);
        assert!(reread.bypasses_player_limit("carol"));
        assert!(reread.remove("alice") && !reread.remove("alice"));

        // Missing file is empty; malformed JSON is an error (fail closed), as
        // is a legacy array holding an impossible player name.
        assert!(OpsList::load_from(&dir.join("nope.json")).unwrap().entries.is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(OpsList::load_from(&path).is_err());
        std::fs::write(&path, r#"["not a name!"]"#).unwrap();
        assert!(OpsList::load_from(&path).is_err());
        std::fs::write(&path, r#"[{"level":3}]"#).unwrap();
        assert!(OpsList::load_from(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whitelist_name_and_uuid_lookups_roundtrip() {
        let dir = tmp_dir("wl");
        let path = dir.join(WHITELIST_FILE);
        let mut wl = Whitelist::default();
        assert!(wl.add("Friend"));
        assert!(!wl.add("friend")); // already there, any case
        assert!(wl.add_with_uuid("00000000-0000-0000-0000-0000000000AB", "Vip"));
        assert!(!wl.add_with_uuid("00000000-0000-0000-0000-0000000000ab", "Other")); // same uuid
        assert!(wl.contains_name("FRIEND"));
        assert!(wl.contains_uuid("00000000-0000-0000-0000-0000000000AB"));
        assert!(!wl.contains_uuid("00000000-0000-0000-0000-0000000000cd"));

        wl.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"name\": \"Friend\""), "{text}");
        let mut wl = Whitelist::load_from(&path).unwrap();
        assert_eq!(wl.names(), ["Friend", "Vip"]);
        assert!(wl.remove("friend") && !wl.remove("friend"));

        std::fs::write(&path, "not json").unwrap();
        assert!(Whitelist::load_from(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ban_files_roundtrip_with_vanilla_fields() {
        let dir = tmp_dir("bans");
        let players_path = dir.join(BANNED_PLAYERS_FILE);
        let ips_path = dir.join(BANNED_IPS_FILE);

        let mut players = BannedPlayers::default();
        assert!(players.ban("Griefer", "griefing", "Server", "2026-11-21T00:00:00Z"));
        assert!(!players.ban("griefer", "again", "Server", "2026-11-21T00:00:00Z"));
        players.save_to(&players_path).unwrap();
        let text = std::fs::read_to_string(&players_path).unwrap();
        assert!(text.contains("\"expires\": \"forever\""), "{text}");
        assert!(text.contains("\"reason\": \"griefing\""), "{text}");
        assert!(text.contains("\"source\": \"Server\""), "{text}");
        let players = BannedPlayers::load_from(&players_path).unwrap();
        let ban = players.find("GRIEFER").unwrap();
        assert_eq!(ban.reason, "griefing");
        assert_eq!(ban.uuid, offline_uuid("Griefer"));

        let mut ips = BannedIps::default();
        assert!(ips.ban("10.0.0.9", "scanning", "Server", "2026-11-21T00:00:00Z"));
        assert!(!ips.ban("/10.0.0.9", "dup", "Server", "x")); // InetAddress form
        assert!(ips.ban("EXAMPLE.com", "host", "Server", "t"));
        assert!(ips.find("10.0.0.9").is_some());
        assert!(ips.find("example.COM").is_some()); // hostnames fold case
        ips.save_to(&ips_path).unwrap();
        let ips = BannedIps::load_from(&ips_path).unwrap();
        assert!(ips.find("10.0.0.9").is_some());
        assert_eq!(ips.find("10.0.0.9").unwrap().expires, "forever");

        // An IPv6 spelling normalized on load matches the canonical form.
        std::fs::write(&ips_path, r#"[{"ip":"0:0:0:0:0:0:0:1","created":"x","source":"s","expires":"forever","reason":"r"}]"#).unwrap();
        let ips = BannedIps::load_from(&ips_path).unwrap();
        assert!(ips.find("::1").is_some());
        assert!(ips.find("127.0.0.1").is_none());

        std::fs::write(&players_path, "{ not an array").unwrap();
        assert!(BannedPlayers::load_from(&players_path).is_err());
        std::fs::write(&ips_path, r#"{"ip":"1.2.3.4"}"#).unwrap();
        assert!(BannedIps::load_from(&ips_path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
