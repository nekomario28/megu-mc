//! `usercache.json` (Vanilla's `CachedUserNameToIdResolver`): the players the
//! server has seen join or looked up by name, so commands resolve a name
//! without asking the profile service again.
//!
//! Entries are most-recently-used ordered and the file keeps the [`DEFAULT_CAPACITY`]
//! most recent, like Vanilla's 1000-entry bound. The cache keeps no clock:
//! `expiresOn` is stored verbatim the way the file carries it and callers
//! stamp it on [`UserCache::upsert`].

use crate::atomic::write_atomic;
use crate::json::{self, Json};
use std::io;
use std::path::Path;

/// The file's Vanilla name.
pub const USERCACHE_FILE: &str = "usercache.json";

/// Vanilla's `MRU_LIMIT`.
pub const DEFAULT_CAPACITY: usize = 1000;

/// One `usercache.json` object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserCacheEntry {
    pub uuid: String,
    pub name: String,
    /// The `expiresOn` stamp (`"yyyy-MM-dd HH:mm:ss Z"` as Vanilla writes, or
    /// whatever the file carried); this crate keeps no clock, so callers
    /// supply it on [`UserCache::upsert`].
    pub expires_on: String,
}

/// A bounded `uuid` <-> `name` most-recently-used cache.
#[derive(Clone, Debug)]
pub struct UserCache {
    /// MRU first: `entries[0]` was upserted or looked up most recently.
    entries: Vec<UserCacheEntry>,
    capacity: usize,
}

impl Default for UserCache {
    fn default() -> Self {
        Self::new()
    }
}

impl UserCache {
    /// Empty cache at Vanilla's bound.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// MRU-first.
    pub fn entries(&self) -> &[UserCacheEntry] {
        &self.entries
    }

    /// Insert or refresh `uuid`/`name`. An existing entry with the same UUID
    /// or the same name (any case) is replaced, so a player who changed their
    /// name does not end up cached twice. The touched entry becomes most
    /// recently used and the least recent one is evicted past `capacity`.
    pub fn upsert(&mut self, uuid: &str, name: &str, expires_on: &str) {
        self.entries.retain(|e| {
            !e.uuid.eq_ignore_ascii_case(uuid) && !e.name.eq_ignore_ascii_case(name)
        });
        self.entries.insert(
            0,
            UserCacheEntry {
                uuid: uuid.to_owned(),
                name: name.to_owned(),
                expires_on: expires_on.to_owned(),
            },
        );
        self.entries.truncate(self.capacity);
    }

    /// Case-insensitive name lookup; a hit becomes most recently used.
    pub fn lookup_by_name(&mut self, name: &str) -> Option<&UserCacheEntry> {
        self.touch(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// UUID lookup; a hit becomes most recently used. Dashed UUID hex
    /// compares case-insensitively, as it denotes the same value either way.
    pub fn lookup_by_uuid(&mut self, uuid: &str) -> Option<&UserCacheEntry> {
        self.touch(|e| e.uuid.eq_ignore_ascii_case(uuid))
    }

    fn touch(&mut self, pred: impl Fn(&UserCacheEntry) -> bool) -> Option<&UserCacheEntry> {
        let index = self.entries.iter().position(pred)?;
        self.entries[..=index].rotate_left(index);
        self.entries.first()
    }

    /// The pretty Vanilla-shaped array, MRU first.
    pub fn to_json(&self) -> String {
        let mut out = String::from("[\n");
        for (i, e) in self.entries.iter().enumerate() {
            out.push_str("  {\n");
            out.push_str(&format!("    \"uuid\": {},\n", json::quote(&e.uuid)));
            out.push_str(&format!("    \"name\": {},\n", json::quote(&e.name)));
            out.push_str(&format!("    \"expiresOn\": {}\n", json::quote(&e.expires_on)));
            out.push_str(if i + 1 == self.entries.len() {
                "  }\n"
            } else {
                "  },\n"
            });
        }
        out.push_str("]\n");
        out
    }

    /// Atomically write [`Self::to_json`] to `path`.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, self.to_json().as_bytes())
    }

    /// Read `path`; a missing file is an empty cache, malformed JSON an
    /// `InvalidData` error (fail closed, like the other lists).
    pub fn load_from(path: &Path) -> io::Result<Self> {
        Self::load_from_with_capacity(path, DEFAULT_CAPACITY)
    }

    /// [`Self::load_from`] at a non-default bound.
    pub fn load_from_with_capacity(path: &Path, capacity: usize) -> io::Result<Self> {
        let mut cache = Self::with_capacity(capacity);
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(cache),
            Err(e) => return Err(e),
        };
        let parsed = json::parse(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("could not parse {}: {e}", path.display()),
            )
        })?;
        // An empty file's `null` reads as an empty cache, as Gson's null does.
        let items = match parsed {
            Json::Null => return Ok(cache),
            Json::Array(items) => items,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("could not parse {}: expected an array, found {other:?}", path.display()),
                ))
            }
        };
        // The file is MRU-first; push entries back to front so the most recent
        // lands at index 0.
        for item in items.iter().rev() {
            let (Some(uuid), Some(name)) = (item.get_str("uuid"), item.get_str("name")) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("could not parse {}: entry lacks \"uuid\"/\"name\"", path.display()),
                ));
            };
            cache.upsert(
                uuid,
                name,
                item.get_str("expiresOn").unwrap_or(""),
            );
        }
        Ok(cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("server-files-uc-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const U1: &str = "00000000-0000-0000-0000-000000000001";
    const U2: &str = "00000000-0000-0000-0000-000000000002";
    const U3: &str = "00000000-0000-0000-0000-000000000003";
    const U4: &str = "00000000-0000-0000-0000-000000000004";

    #[test]
    fn bounded_mru_eviction_and_refresh() {
        let mut cache = UserCache::with_capacity(3);
        cache.upsert(U1, "One", "2030-01-01 00:00:00 +0000");
        cache.upsert(U2, "Two", "2030-01-01 00:00:00 +0000");
        cache.upsert(U3, "Three", "2030-01-01 00:00:00 +0000");
        assert_eq!(cache.len(), 3);

        // The oldest entry survives once it is touched.
        assert_eq!(cache.lookup_by_uuid(U1).unwrap().name, "One");
        cache.upsert(U4, "Four", "2030-01-01 00:00:00 +0000");
        assert_eq!(cache.len(), 3);
        assert!(cache.lookup_by_uuid(U2).is_none()); // least recent evicted
        assert_eq!(
            cache.entries().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["Four", "One", "Three"]
        );

        // Re-upsert refreshes instead of duplicating, even across a rename.
        cache.upsert(U4, "FourRenamed", "2031-01-01 00:00:00 +0000");
        assert_eq!(cache.len(), 3);
        assert!(cache.lookup_by_name("four").is_none());
        assert_eq!(cache.lookup_by_name("FOURRENAMED").unwrap().uuid, U4);
        assert_eq!(
            cache.lookup_by_uuid(U4).unwrap().expires_on,
            "2031-01-01 00:00:00 +0000"
        );
    }

    #[test]
    fn save_load_roundtrip_and_fail_closed() {
        let dir = tmp_dir("io");
        let path = dir.join(USERCACHE_FILE);

        let mut cache = UserCache::new();
        cache.upsert(U1, "Notch", "2030-01-01 00:00:00 +0000");
        cache.upsert(U2, "Alex", "2030-06-01 00:00:00 +0000");
        cache.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"expiresOn\": \"2030-06-01 00:00:00 +0000\""), "{text}");

        let mut back = UserCache::load_from(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.lookup_by_name("notch").unwrap().uuid, U1); // case-insensitive
        assert_eq!(back.lookup_by_uuid(U2).unwrap().name, "Alex");
        // Round trip is stable.
        back.save_to(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        // Missing file is empty; malformed JSON fails closed.
        assert!(UserCache::load_from(&dir.join("nope.json")).unwrap().is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(UserCache::load_from(&path).is_err());
        std::fs::write(&path, r#"[{"uuid":"u"}]"#).unwrap();
        assert!(UserCache::load_from(&path).is_err());
        std::fs::write(&path, "null").unwrap();
        assert!(UserCache::load_from(&path).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
