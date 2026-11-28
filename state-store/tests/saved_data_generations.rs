// SPDX-License-Identifier: LGPL-3.0-or-later
//! Saved-data generation and deletion-tombstone integration tests.
//!
//! These drive only the public `data/` store API, so they pin the contract the
//! server's save orchestration depends on: a save either publishes a new
//! generation or leaves the previous one exactly intact, a deleted document
//! stays deleted across a stale copy of it, a refused document or manifest is
//! preserved rather than silently dropped or overwritten, and concurrent
//! writers can never expose a partial document.

use megu_state_store::datafix::DataValue;
use megu_state_store::saved_data::{
    SavedDataEnvelope, SavedDataError, SavedDataStore, QUARANTINE_SUFFIX, SAVED_DATA_MANIFEST_FILE,
    SAVED_DATA_MANIFEST_MAGIC,
};
use megu_state_store::vanilla_schema::{unknown_keys_to_preserve, SchemaScope};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "megu-saved-data-integration-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn manifest(&self) -> PathBuf {
        self.0.join(SAVED_DATA_MANIFEST_FILE)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn map(
    entries: impl IntoIterator<Item = (&'static str, DataValue)>,
) -> BTreeMap<String, DataValue> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn envelope(name: &str, value: i64) -> SavedDataEnvelope {
    SavedDataEnvelope::new(name, 3955, map([("value", DataValue::Int(value))])).unwrap()
}

/// A mod document: vanilla `data`/`DataVersion` plus an undocumented root key
/// and a namespaced payload key that must survive every save cycle verbatim.
fn mod_root() -> BTreeMap<String, DataValue> {
    let mut root = BTreeMap::new();
    root.insert(
        "data".to_owned(),
        DataValue::Map(map([(
            "mymod:state",
            DataValue::Map(map([("saved", DataValue::Int(41))])),
        )])),
    );
    root.insert("DataVersion".to_owned(), DataValue::Int(3955));
    root.insert(
        "mymod_meta".to_owned(),
        DataValue::Map(map([("owner", DataValue::Str("mymod".into()))])),
    );
    root
}

#[test]
fn one_generation_per_successful_save_and_unknown_mod_data_survives() {
    let dir = TestDir::new("generation");
    let mut store = SavedDataStore::new();
    store
        .insert(SavedDataEnvelope::from_root("mymod_state", mod_root()).unwrap())
        .unwrap();
    assert_eq!(store.generation(), 0);
    store.save_to_dir(dir.path()).unwrap();
    assert_eq!(store.generation(), 1);

    // The manifest is sealed, sits beside the documents and is not a `.dat`
    // document, so vanilla and `load_dir` both ignore it as one.
    let raw = std::fs::read(dir.manifest()).unwrap();
    assert_eq!(&raw[..4], &SAVED_DATA_MANIFEST_MAGIC);
    assert!(dir.path().join("mymod_state.dat").is_file());

    let reloaded = SavedDataStore::load_dir(dir.path()).unwrap();
    assert_eq!(reloaded, store);
    assert_eq!(reloaded.generation(), 1);
    let preserved = reloaded.get("mymod_state").unwrap();
    assert_eq!(preserved.to_root(), mod_root());
    assert_eq!(
        preserved
            .preserved()
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        unknown_keys_to_preserve(
            SchemaScope::AuxMap,
            ["data", "DataVersion", "mymod_meta"].into_iter()
        )
    );
    assert!(preserved.data().contains_key("mymod:state"));
}

#[test]
fn a_deleted_document_stays_deleted_across_a_stale_copy() {
    let dir = TestDir::new("tombstone");
    let mut store = SavedDataStore::new();
    store.insert(envelope("scoreboard", 7)).unwrap();
    store.insert(envelope("map_7", 7)).unwrap();
    store.save_to_dir(dir.path()).unwrap();
    let stale = std::fs::read(dir.path().join("map_7.dat")).unwrap();

    store.remove("map_7").unwrap();
    assert!(store.is_tombstoned("map_7"));
    store.save_to_dir(dir.path()).unwrap();
    assert_eq!(store.generation(), 2);
    assert!(!dir.path().join("map_7.dat").exists());

    let mut reloaded = SavedDataStore::load_dir(dir.path()).unwrap();
    assert!(reloaded.get("map_7").is_none());
    assert_eq!(reloaded.tombstones().collect::<Vec<_>>(), vec!["map_7"]);
    assert_eq!(reloaded, store);

    // A stale copy — a restored backup or a half-applied save — cannot
    // resurrect the deleted document, and the next save removes it again.
    std::fs::write(dir.path().join("map_7.dat"), &stale).unwrap();
    reloaded = SavedDataStore::load_dir(dir.path()).unwrap();
    assert!(reloaded.get("map_7").is_none());
    reloaded.save_to_dir(dir.path()).unwrap();
    assert!(!dir.path().join("map_7.dat").exists());

    // Only an explicit resurrection brings the name back.
    assert!(reloaded.clear_tombstone("map_7"));
    reloaded.insert(envelope("map_7", 9)).unwrap();
    reloaded.save_to_dir(dir.path()).unwrap();
    let resurrected = SavedDataStore::load_dir(dir.path()).unwrap();
    assert_eq!(resurrected, reloaded);
    assert!(resurrected.get("map_7").is_some());
    assert!(!resurrected.is_tombstoned("map_7"));
}

#[test]
fn a_refused_manifest_or_document_never_poisons_a_later_save() {
    let dir = TestDir::new("recovery");
    let mut store = SavedDataStore::new();
    store.insert(envelope("scoreboard", 3)).unwrap();
    store.insert(envelope("map_7", 3)).unwrap();
    store.save_to_dir(dir.path()).unwrap();

    // A corrupt manifest is rejected fail-closed ...
    let mut corrupt = std::fs::read(dir.manifest()).unwrap();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0x01;
    std::fs::write(dir.manifest(), &corrupt).unwrap();
    assert!(matches!(
        SavedDataStore::load_dir(dir.path()).unwrap_err(),
        SavedDataError::MalformedManifest { .. }
    ));
    // ... the recovery load preserves the bytes and loads the documents ...
    let (mut recovered, quarantined) = SavedDataStore::load_dir_quarantining(dir.path()).unwrap();
    assert_eq!(quarantined, vec![dir.manifest()]);
    let preserved_manifest = dir
        .manifest()
        .with_file_name(format!("{SAVED_DATA_MANIFEST_FILE}{QUARANTINE_SUFFIX}"));
    assert_eq!(std::fs::read(&preserved_manifest).unwrap(), corrupt);
    assert_eq!(recovered.generation(), 0);
    // ... and a later save still publishes, so the world keeps saving.
    recovered.save_to_dir(dir.path()).unwrap();
    assert_eq!(recovered.generation(), 1);
    assert_eq!(SavedDataStore::load_dir(dir.path()).unwrap(), recovered);

    // The same holds for one corrupt document: refused, kept, and not able to
    // block the documents around it or a later save.
    let corrupt_document = b"not an nbt document".to_vec();
    std::fs::write(dir.path().join("map_7.dat"), &corrupt_document).unwrap();
    assert!(matches!(
        SavedDataStore::load_dir(dir.path()).unwrap_err(),
        SavedDataError::MalformedDocument { .. }
    ));
    let (mut recovered, quarantined) = SavedDataStore::load_dir_quarantining(dir.path()).unwrap();
    assert_eq!(quarantined, vec![dir.path().join("map_7.dat")]);
    assert_eq!(
        std::fs::read(dir.path().join(format!("map_7.dat{QUARANTINE_SUFFIX}"))).unwrap(),
        corrupt_document
    );
    assert!(recovered.get("scoreboard").is_some());
    assert!(recovered.get("map_7").is_none());
    recovered.insert(envelope("map_8", 1)).unwrap();
    recovered.save_to_dir(dir.path()).unwrap();
    assert_eq!(SavedDataStore::load_dir(dir.path()).unwrap(), recovered);
}

#[test]
fn a_failed_write_leaves_the_published_generation_untouched() {
    let dir = TestDir::new("storage-full");
    let mut store = SavedDataStore::new();
    store.insert(envelope("alpha", 1)).unwrap();
    store.insert(envelope("beta", 1)).unwrap();
    store.save_to_dir(dir.path()).unwrap();
    let before = store.clone();
    let published = [
        std::fs::read(dir.path().join("alpha.dat")).unwrap(),
        std::fs::read(dir.path().join("beta.dat")).unwrap(),
    ];
    let manifest_before = std::fs::read(dir.manifest()).unwrap();

    store.insert(envelope("alpha", 2)).unwrap();
    store.insert(envelope("beta", 2)).unwrap();
    let mut writes = 0usize;
    let error = store
        .save_to_dir_with(dir.path(), |path, bytes| {
            writes += 1;
            if writes == 2 {
                // What an exhausted filesystem reports mid-save.
                return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
            }
            megu_state_store::migrate::write_file_atomic(path, bytes)
        })
        .unwrap_err();
    assert!(matches!(error, SavedDataError::Io { .. }), "{error:?}");
    assert_eq!(writes, 2);

    // Nothing canonical moved: same generation, same documents, same manifest.
    assert_eq!(store.generation(), 1);
    assert_eq!(
        std::fs::read(dir.path().join("alpha.dat")).unwrap(),
        published[0]
    );
    assert_eq!(
        std::fs::read(dir.path().join("beta.dat")).unwrap(),
        published[1]
    );
    assert_eq!(std::fs::read(dir.manifest()).unwrap(), manifest_before);
    assert_eq!(SavedDataStore::load_dir(dir.path()).unwrap(), before);

    // A retry publishes the new generation.
    store.save_to_dir(dir.path()).unwrap();
    assert_eq!(store.generation(), 2);
    assert_eq!(SavedDataStore::load_dir(dir.path()).unwrap(), store);
}

#[test]
fn concurrent_writers_never_expose_a_partial_document() {
    let dir = TestDir::new("concurrent");
    let writers = 4usize;
    let rounds = 8usize;
    let payloads: Vec<String> = (0..writers)
        .map(|writer| char::from(b'a' + writer as u8).to_string().repeat(4096))
        .collect();
    let mut handles = Vec::with_capacity(writers);
    for payload in payloads.iter() {
        let payload = payload.clone();
        let path = dir.path().to_path_buf();
        handles.push(std::thread::spawn(move || {
            let mut store = SavedDataStore::new();
            for _ in 0..rounds {
                store
                    .insert(
                        SavedDataEnvelope::new(
                            "shared",
                            3955,
                            map([("blob", DataValue::Str(payload.clone()))]),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                store.save_to_dir(&path).unwrap();
            }
        }));
    }

    // Read while the writers replace the same document: every load either has
    // no document yet or exactly one writer's payload — never a torn one.
    let mut observed = 0usize;
    let mut spins = 0usize;
    loop {
        let store = match SavedDataStore::load_dir(dir.path()) {
            Ok(store) => store,
            Err(error) => panic!("a concurrent save exposed an unreadable directory: {error}"),
        };
        if let Some(document) = store.get("shared") {
            match document.field("blob") {
                Some(DataValue::Str(text)) => assert!(
                    payloads.iter().any(|payload| payload == text),
                    "a torn or interleaved document was published"
                ),
                other => panic!("unexpected payload: {other:?}"),
            }
            observed += 1;
        }
        if handles.iter().all(|handle| handle.is_finished()) && observed > 0 {
            break;
        }
        spins += 1;
        assert!(spins < 1_000_000, "no writer ever published a document");
    }
    for handle in handles {
        handle.join().expect("writer thread");
    }

    let published = SavedDataStore::load_dir(dir.path()).unwrap();
    match published.get("shared").unwrap().field("blob") {
        Some(DataValue::Str(text)) => assert!(payloads.iter().any(|payload| payload == text)),
        other => panic!("unexpected final payload: {other:?}"),
    }
    assert!(published.generation() >= 1);
    // No staging directory or temp file survives a completed save.
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let name = entry.unwrap().file_name();
        let name = name.to_string_lossy();
        assert!(!name.contains("saved-data-staging"), "{name}");
        assert!(!name.contains("migrate-tmp"), "{name}");
    }
}
