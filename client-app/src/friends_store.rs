// SPDX-License-Identifier: LGPL-3.0-or-later
//! Persistent `FRL3` device identity and the trusted-friends store.
//!
//! [`load_or_create_identity`] keeps the player's 256-bit X25519 static
//! secret in `<gameDir>/megu-identity.key` (mode `0o600` on Unix) so the
//! `megu-id:` the player shares with friends is stable across restarts.
//!
//! [`FriendsStore`] persists `<gameDir>/megu-friends.toml`: the share mode,
//! the saved relay address (so joining a friend never needs `--relay` on
//! the command line again) and the flat friend list. The allowlist is
//! strictly one hop — there is no "friends of friends" expansion, so a
//! peer is admitted only when its own 256-bit static key was paired
//! directly.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use megu_proto::frl_e2ee::{
    format_friend_id, parse_friend_id, random_32_bytes, FriendAllowlist,
    Frl3Identity,
};

/// File holding the 32-byte `FRL3` static secret key.
pub const IDENTITY_FILE_NAME: &str = "megu-identity.key";
/// Human-readable trusted-friends document inside the game directory.
pub const FRIENDS_FILE_NAME: &str = "megu-friends.toml";

/// Load the persistent `FRL3` identity from `<gameDir>/megu-identity.key`,
/// generating and atomically persisting a fresh one on first launch.
///
/// Fail-closed: a file that is not exactly 32 bytes — or the degenerate
/// all-zero secret — is an error, never silently replaced, so a corrupt
/// store can never quietly rotate the player's identity.
pub fn load_or_create_identity(game_dir: &Path) -> io::Result<Frl3Identity> {
    let path = game_dir.join(IDENTITY_FILE_NAME);
    match fs::read(&path) {
        Ok(bytes) => {
            let secret: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is not a 32-byte FRL3 identity key ({} bytes)",
                        path.display(),
                        bytes.len()
                    ),
                )
            })?;
            if secret == [0u8; 32] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} holds the degenerate all-zero secret", path.display()),
                ));
            }
            Ok(Frl3Identity::from_secret(secret))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let secret = random_32_bytes();
            write_private_atomic(&path, &secret)?;
            Ok(Frl3Identity::from_secret(secret))
        }
        Err(e) => Err(e),
    }
}

/// `path` + `.part` — the sibling a write is staged in before the rename.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Write `bytes` to `path` atomically: staged in `path.part`, fsynced, then
/// renamed over the destination so a crash mid-write can never leave a
/// torn friends file behind.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    ensure_parent(path)?;
    let part = part_path(path);
    {
        let mut file = fs::File::create(&part)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&part, path)
}

/// Like [`write_atomic`] but forces `0o600` on the staged file *before* it
/// is renamed into place — the identity key is never visible with looser
/// permissions at any point of its lifetime.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    ensure_parent(path)?;
    let part = part_path(path);
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&part)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The part file may have pre-existed with wider permissions; force
        // the final mode before the rename exposes the key bytes.
        fs::set_permissions(&part, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&part, path)
}

/// Who a hosted world is shared with over `FRL3-FRIEND`. There is no
/// transitive "friends of friends" tier — only keys paired directly into
/// [`FriendsStore::friends`] are ever admitted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FriendShareMode {
    /// Friend hosting is disabled; the allowlist is always empty.
    #[default]
    Off,
    /// Every paired friend may join.
    FriendsOnly,
    /// Only friends flagged `selected_for_share` may join.
    SelectedFriends,
}

impl FriendShareMode {
    /// The `megu-friends.toml` spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            FriendShareMode::Off => "off",
            FriendShareMode::FriendsOnly => "friends_only",
            FriendShareMode::SelectedFriends => "selected_friends",
        }
    }

    /// Parse the `share_mode` value back; fail closed on anything else.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(FriendShareMode::Off),
            "friends_only" => Some(FriendShareMode::FriendsOnly),
            "selected_friends" => Some(FriendShareMode::SelectedFriends),
            _ => None,
        }
    }
}

/// One paired friend: a validated `megu-id:` plus its decoded 256-bit
/// static public key and the player's own label/note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendEntry {
    /// Display name the player gave this friend.
    pub name: String,
    /// Canonical `megu-id:` rendering of `public_key`.
    pub friend_id: String,
    /// The friend's 256-bit X25519 static public key.
    pub public_key: [u8; 32],
    /// Whether this friend is admitted under
    /// [`FriendShareMode::SelectedFriends`].
    pub selected_for_share: bool,
    /// Free-form note (where they met, which server, ...).
    pub note: String,
}

/// A live world a friend is currently hosting, ready for the
/// "Friends & LAN" list on `Screen::Multiplayer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendLiveWorldEntry {
    /// Display name of the hosting friend.
    pub friend_name: String,
    /// The hosting friend's canonical `megu-id:`.
    pub friend_id: String,
    /// World name the friend registered on the relay.
    pub world_name: String,
    /// Players currently in that world.
    pub online_players: u32,
    /// Host-declared guest cap.
    pub max_players: u32,
    /// Relay `host:port` the world is reachable through.
    pub relay_addr: String,
}

/// The persisted trusted-friends document (`megu-friends.toml`).
#[derive(Clone, Debug, Default)]
pub struct FriendsStore {
    /// Share policy for friend-hosted worlds.
    pub share_mode: FriendShareMode,
    /// Remembered relay `host:port` — a paired beginner never has to pass
    /// `--relay` on the command line once this is saved.
    pub relay_addr: Option<String>,
    /// Directly paired friends. One hop only, by design.
    pub friends: Vec<FriendEntry>,
    /// The local identity's own key — bound at runtime (never serialized)
    /// so [`add_friend`](Self::add_friend) can reject self-adds.
    self_pk: Option<[u8; 32]>,
}

impl FriendsStore {
    /// Load `<gameDir>/megu-friends.toml`; a missing file yields the
    /// default (`Off`, no relay, no friends). A malformed document — bad
    /// syntax, an unknown `share_mode`, a `friend_id` whose checksum does
    /// not verify — is a hard error: trust configuration is never silently
    /// dropped or partially applied.
    pub fn load(game_dir: &Path) -> io::Result<Self> {
        let path = game_dir.join(FRIENDS_FILE_NAME);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        Self::parse(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cannot parse {}: {e}", path.display()),
            )
        })
    }

    /// Atomically persist the store (`path.part` staged + renamed).
    pub fn save(&self, game_dir: &Path) -> io::Result<()> {
        write_atomic(
            &game_dir.join(FRIENDS_FILE_NAME),
            self.to_toml().as_bytes(),
        )
    }

    /// Bind the local identity so [`add_friend`](Self::add_friend) can
    /// reject adding your own key. Not serialized — the identity lives in
    /// `megu-identity.key`.
    pub fn bind_identity(&mut self, identity: &Frl3Identity) {
        self.self_pk = Some(identity.public_key());
    }

    /// The player's own shareable `megu-id:` string.
    pub fn my_friend_id(identity: &Frl3Identity) -> String {
        format_friend_id(&identity.public_key())
    }

    /// Pair a new friend by `megu-id:`. Validates the checksum via
    /// [`megu_proto::frl_e2ee::parse_friend_id`], canonicalizes the stored
    /// id, and fail-closed rejects duplicates (by key, not by string) and
    /// the player's own key when [`bind_identity`](Self::bind_identity)
    /// was called.
    pub fn add_friend(&mut self, name: &str, friend_id: &str) -> Result<&FriendEntry, String> {
        let public_key = parse_friend_id(friend_id)
            .map_err(|e| format!("invalid friend id '{friend_id}': {e}"))?;
        if self.self_pk == Some(public_key) {
            return Err("cannot add your own megu-id as a friend".to_string());
        }
        if let Some(existing) = self
            .friends
            .iter()
            .find(|f| f.public_key == public_key)
        {
            return Err(format!(
                "'{}' is already added as '{}'",
                friend_id, existing.name
            ));
        }
        self.friends.push(FriendEntry {
            name: name.to_string(),
            friend_id: format_friend_id(&public_key),
            public_key,
            selected_for_share: false,
            note: String::new(),
        });
        self.friends
            .last()
            .ok_or_else(|| "friend store failed to retain the new entry".to_string())
    }

    /// Instant revoke: drop the friend whose `megu-id:` matches (tolerant
    /// of case/homoglyph variants via the checksum), `true` iff one was
    /// removed.
    pub fn remove_friend(&mut self, friend_id: &str) -> bool {
        let before = self.friends.len();
        match parse_friend_id(friend_id) {
            Ok(public_key) => self.friends.retain(|f| f.public_key != public_key),
            // A hand-edited entry may carry an unparseable id; still allow
            // exact-string removal so it is never stuck in the list.
            Err(_) => self.friends.retain(|f| f.friend_id != friend_id),
        }
        self.friends.len() != before
    }

    /// Snapshot the keys `FRL3-FRIEND` may admit right now — strictly the
    /// directly paired friends the current [`share_mode`](Self::share_mode)
    /// selects, never a transitive expansion.
    pub fn active_allowlist(&self) -> FriendAllowlist {
        let mut allowlist = FriendAllowlist::new();
        for friend in &self.friends {
            let admitted = match self.share_mode {
                FriendShareMode::Off => false,
                FriendShareMode::FriendsOnly => true,
                FriendShareMode::SelectedFriends => friend.selected_for_share,
            };
            if admitted {
                allowlist.add(friend.public_key);
            }
        }
        allowlist
    }

    /// Serialize to the `megu-friends.toml` document.
    pub fn to_toml(&self) -> String {
        let mut out = String::with_capacity(256 + self.friends.len() * 200);
        out.push_str("# megu trusted friends (FRL3-FRIEND allowlist)\n");
        out.push_str(&format!("share_mode = \"{}\"\n", self.share_mode.as_str()));
        if let Some(relay) = &self.relay_addr {
            out.push_str(&format!("relay_addr = \"{}\"\n", escape_toml(relay)));
        }
        for friend in &self.friends {
            out.push_str("\n[[friends]]\n");
            out.push_str(&format!("name = \"{}\"\n", escape_toml(&friend.name)));
            out.push_str(&format!(
                "friend_id = \"{}\"\n",
                escape_toml(&friend.friend_id)
            ));
            out.push_str(&format!(
                "selected_for_share = {}\n",
                friend.selected_for_share
            ));
            out.push_str(&format!("note = \"{}\"\n", escape_toml(&friend.note)));
        }
        out
    }

    /// Parse a `megu-friends.toml` document; fail closed on malformed
    /// lines, unknown `share_mode`, or an unverifiable `friend_id`.
    fn parse(text: &str) -> Result<Self, String> {
        let mut store = FriendsStore::default();
        let mut friend: Option<FriendEntry> = None;
        for (index, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim().to_string();
            if line.is_empty() {
                continue;
            }
            let fail = |msg: &str| format!("megu-friends.toml line {}: {msg}", index + 1);
            if line == "[[friends]]" {
                if let Some(done) = friend.replace(FriendEntry::default_friend()) {
                    store.friends.push(done);
                }
                continue;
            }
            if line.starts_with('[') {
                return Err(fail(&format!("unknown section '{line}'")));
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| fail("expected 'key = value'"))?;
            let key = key.trim();
            let value = value.trim();
            match &mut friend {
                Some(entry) => match key {
                    "name" => entry.name = parse_toml_string(value).map_err(|e| fail(&e))?,
                    "friend_id" => {
                        let id = parse_toml_string(value).map_err(|e| fail(&e))?;
                        entry.public_key = parse_friend_id(&id).map_err(|e| {
                            fail(&format!("friend_id '{id}' does not verify: {e}"))
                        })?;
                        entry.friend_id = format_friend_id(&entry.public_key);
                    }
                    "selected_for_share" => {
                        entry.selected_for_share = parse_toml_bool(value)
                            .ok_or_else(|| fail("selected_for_share must be true/false"))?
                    }
                    "note" => entry.note = parse_toml_string(value).map_err(|e| fail(&e))?,
                    // Unknown keys are ignored for forward compatibility.
                    _ => {}
                },
                None => match key {
                    "share_mode" => {
                        let mode = parse_toml_string(value).map_err(|e| fail(&e))?;
                        store.share_mode = FriendShareMode::parse(&mode).ok_or_else(|| {
                            fail(&format!("unknown share_mode '{mode}'"))
                        })?;
                    }
                    "relay_addr" => {
                        store.relay_addr =
                            Some(parse_toml_string(value).map_err(|e| fail(&e))?)
                    }
                    _ => {}
                },
            }
        }
        if let Some(done) = friend.take() {
            store.friends.push(done);
        }
        Ok(store)
    }
}

impl FriendEntry {
    fn default_friend() -> Self {
        Self {
            name: String::new(),
            friend_id: String::new(),
            public_key: [0; 32],
            selected_for_share: false,
            note: String::new(),
        }
    }
}

/// Escape a string for a TOML basic string (`"..."`).
fn escape_toml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out
}

/// Strip a trailing `#` comment only when it sits outside a quoted
/// string — a `#` inside `name = "a#b"` is data.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '#' {
            return &line[..index];
        }
    }
    line
}

/// Parse a TOML basic string `"..."` with the escapes [`escape_toml`]
/// emits (plus `\b`, `\f`, `\uXXXX`, `\UXXXXXXXX`). Anything else —
/// unquoted values, literal `'...'` strings, truncated escapes — is a
/// hard error, never a silent truncation.
fn parse_toml_string(value: &str) -> Result<String, String> {
    let body = value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .filter(|_| value.len() >= 2)
        .ok_or_else(|| format!("expected a quoted string, got '{value}'"))?;
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let escaped = chars
            .next()
            .ok_or_else(|| "truncated escape at end of string".to_string())?;
        match escaped {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'u' | 'U' => {
                let digits = if escaped == 'u' { 4 } else { 8 };
                let mut code = 0u32;
                for _ in 0..digits {
                    let d = chars
                        .next()
                        .and_then(|d| d.to_digit(16))
                        .ok_or_else(|| "truncated \\u escape".to_string())?;
                    code = (code << 4) | d;
                }
                out.push(
                    char::from_u32(code).ok_or_else(|| "invalid \\u codepoint".to_string())?,
                );
            }
            other => return Err(format!("unsupported escape '\\{other}'")),
        }
    }
    Ok(out)
}

fn parse_toml_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    /// A unique empty directory under the OS temp dir.
    fn temp_game_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "megu-friends-{tag}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn identity_persists_bit_identical_across_reloads() {
        let dir = temp_game_dir("identity");
        let first = load_or_create_identity(&dir).unwrap();
        let key_path = dir.join(IDENTITY_FILE_NAME);
        assert_eq!(fs::read(&key_path).unwrap().len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&key_path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        for _ in 0..3 {
            let reloaded = load_or_create_identity(&dir).unwrap();
            assert_eq!(reloaded.public_key(), first.public_key());
            assert_eq!(reloaded.friend_id(), first.friend_id());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_fails_closed_on_corrupt_key() {
        let dir = temp_game_dir("corrupt");
        let key_path = dir.join(IDENTITY_FILE_NAME);
        for bad in [vec![0u8; 16], vec![0u8; 33], vec![0u8; 32]] {
            fs::write(&key_path, &bad).unwrap();
            assert!(
                load_or_create_identity(&dir).is_err(),
                "{} bytes must be rejected",
                bad.len()
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_roundtrip_share_modes_relay_and_friends() {
        let dir = temp_game_dir("roundtrip");
        let alice = Frl3Identity::from_secret([0x11; 32]);
        let bob = Frl3Identity::from_secret([0x22; 32]);

        let mut store = FriendsStore::default();
        assert_eq!(store.share_mode, FriendShareMode::Off);
        store.share_mode = FriendShareMode::SelectedFriends;
        store.relay_addr = Some("relay.example:25575".to_string());
        store.add_friend("Al #1 & \"ice\"", &alice.friend_id()).unwrap();
        store.add_friend("Bob", &bob.friend_id()).unwrap();
        store.friends[0].selected_for_share = true;
        store.friends[1].note = "met on the test server\nsecond line".to_string();
        store.save(&dir).unwrap();

        // No staged part file survives the rename.
        assert!(!part_path(&dir.join(FRIENDS_FILE_NAME)).exists());

        let loaded = FriendsStore::load(&dir).unwrap();
        assert_eq!(loaded.share_mode, FriendShareMode::SelectedFriends);
        assert_eq!(loaded.relay_addr.as_deref(), Some("relay.example:25575"));
        assert_eq!(loaded.friends.len(), 2);
        assert_eq!(loaded.friends[0].name, "Al #1 & \"ice\"");
        assert_eq!(loaded.friends[0].friend_id, alice.friend_id());
        assert_eq!(loaded.friends[0].public_key, alice.public_key());
        assert!(loaded.friends[0].selected_for_share);
        assert!(!loaded.friends[1].selected_for_share);
        assert_eq!(loaded.friends[1].note, "met on the test server\nsecond line");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_allowlist_is_one_hop_and_mode_gated() {
        let alice = Frl3Identity::from_secret([0x31; 32]);
        let bob = Frl3Identity::from_secret([0x32; 32]);
        let mut store = FriendsStore::default();
        store.add_friend("Alice", &alice.friend_id()).unwrap();
        store.add_friend("Bob", &bob.friend_id()).unwrap();

        assert!(store.active_allowlist().is_empty(), "Off admits nobody");

        store.share_mode = FriendShareMode::FriendsOnly;
        let list = store.active_allowlist();
        assert_eq!(list.len(), 2);
        assert!(list.contains(&alice.public_key()));
        assert!(list.contains(&bob.public_key()));

        store.share_mode = FriendShareMode::SelectedFriends;
        assert!(store.active_allowlist().is_empty());
        store.friends[1].selected_for_share = true;
        let list = store.active_allowlist();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&bob.public_key()));
        assert!(!list.contains(&alice.public_key()));
    }

    #[test]
    fn add_friend_rejects_bad_id_duplicate_and_self() {
        let me = Frl3Identity::from_secret([0x44; 32]);
        let alice = Frl3Identity::from_secret([0x45; 32]);
        let mut store = FriendsStore::default();
        store.bind_identity(&me);

        assert!(store.add_friend("Me", &me.friend_id()).is_err());
        assert!(store.add_friend("Bad", "megu-id:0000").is_err());
        assert!(store.add_friend("Empty", "not-an-id").is_err());
        store.add_friend("Alice", &alice.friend_id()).unwrap();
        // Duplicate detection keys on the public key, so a case/hyphen
        // variant of the same id is still rejected.
        let variant = alice.friend_id().to_lowercase().replace("megu-id:", "MEGU ID ");
        assert!(store.add_friend("Alice2", &variant).is_err());
        assert_eq!(store.friends.len(), 1);
    }

    #[test]
    fn remove_friend_revokes_instantly_and_tolerantly() {
        let alice = Frl3Identity::from_secret([0x51; 32]);
        let bob = Frl3Identity::from_secret([0x52; 32]);
        let mut store = FriendsStore::default();
        store.add_friend("Alice", &alice.friend_id()).unwrap();
        store.add_friend("Bob", &bob.friend_id()).unwrap();
        store.share_mode = FriendShareMode::FriendsOnly;
        assert_eq!(store.active_allowlist().len(), 2);

        assert!(store.remove_friend(&alice.friend_id().to_lowercase()));
        assert!(!store.remove_friend(&alice.friend_id()));
        let list = store.active_allowlist();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&bob.public_key()));
    }

    #[test]
    fn load_fails_closed_on_malformed_document() {
        let dir = temp_game_dir("bad-toml");
        let path = dir.join(FRIENDS_FILE_NAME);
        for bad in [
            "share_mode = \"friends_of_friends\"\n".to_string(),
            "share_mode off\n".to_string(),
            "[[evil]]\nx = 1\n".to_string(),
            "[[friends]]\nname = \"A\"\nfriend_id = \"megu-id:0000\"\n".to_string(),
            "relay_addr = no-quotes\n".to_string(),
        ] {
            fs::write(&path, bad).unwrap();
            assert!(FriendsStore::load(&dir).is_err());
        }
        // A missing file is the only soft case: a fresh default.
        let _ = fs::remove_file(&path);
        assert!(FriendsStore::load(&dir).unwrap().friends.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn my_friend_id_matches_identity() {
        let id = Frl3Identity::from_secret([0x66; 32]);
        assert_eq!(FriendsStore::my_friend_id(&id), id.friend_id());
        assert_eq!(
            parse_friend_id(&FriendsStore::my_friend_id(&id)).unwrap(),
            id.public_key()
        );
    }
}
