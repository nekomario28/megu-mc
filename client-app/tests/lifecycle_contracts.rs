use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use megu_client::server_list::{ServerEntry, ServerListStore, SERVER_LIST_FILE_NAME};
use megu_client::settings::{ClientSettings, SETTINGS_FILE_NAME};
use megu_client::shutdown::install_graceful_shutdown_flag;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        // macOS exposes the temporary directory through `/var`, which is a
        // symlink to `/private/var`. Use its canonical path so the fixture
        // exercises symlinks inside the game directory rather than that OS alias.
        let temp_dir = fs::canonicalize(std::env::temp_dir()).expect("canonicalize temp dir");
        let path = temp_dir.join(format!(
            "megu-lifecycle-contract-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn server_list_atomic_save_reload_and_ordering() {
    let dir = TestDir::new();
    let mut store = ServerListStore::load(&dir.0).expect("missing list starts empty");
    assert!(store.entries().is_empty());
    let first = ServerEntry {
        name: "Local".into(),
        address: "localhost".into(),
        port: 25565,
    };
    let second = ServerEntry {
        name: "Remote".into(),
        address: "mc.example.org".into(),
        port: 25566,
    };
    store.add(first.clone()).expect("add first server");
    store.add(second.clone()).expect("add second server");
    assert_eq!(store.find("Local"), Some(&first));
    assert!(store.move_up(1).expect("move up"));
    assert_eq!(store.entries(), &[second.clone(), first.clone()]);
    assert!(store.move_down(0).expect("move down"));
    assert!(!store.move_up(0).expect("top entry cannot move up"));
    assert!(store.move_up(1).expect("restore order"));
    assert!(!store.move_down(1).expect("bottom entry cannot move down"));
    assert_eq!(store.remove(1).expect("remove"), first);
    assert_eq!(
        fs::read_to_string(dir.0.join(SERVER_LIST_FILE_NAME)).expect("read saved list"),
        "[[servers]]\nname = \"Remote\"\naddress = \"mc.example.org\"\nport = 25566\n\n"
    );
    let reloaded = ServerListStore::load(&dir.0).expect("reload saved list");
    assert_eq!(reloaded.entries(), &[second]);
}

#[test]
fn corrupt_or_unsafe_server_lists_fail_closed() {
    let dir = TestDir::new();
    let path = dir.0.join(SERVER_LIST_FILE_NAME);
    fs::write(&path, "[[servers]]\nname = \"broken\"\nport = 25565\n")
        .expect("write malformed fixture");
    assert!(ServerListStore::load(&dir.0).is_err());

    assert!(ServerListStore::load(dir.0.join(".."))
        .expect_err("parent traversal must be rejected")
        .contains("parent traversal"));

    fs::write(
        &path,
        "[[servers]]\nname = \"bad\\nname\"\naddress = \"host\"\nport = 25565\n",
    )
    .expect("write control escape fixture");
    assert!(ServerListStore::load(&dir.0).is_err());

    fs::write(
        &path,
        "[[servers]]\nname = \"bad\"\naddress = \"../world\"\nport = 25565\n",
    )
    .expect("write traversal fixture");
    assert!(ServerListStore::load(&dir.0).is_err());

    fs::write(
        &path,
        vec![b'x'; megu_client::server_list::MAX_SERVER_LIST_BYTES + 1],
    )
    .expect("write oversized fixture");
    assert!(ServerListStore::load(&dir.0).is_err());
}

#[cfg(unix)]
#[test]
fn server_list_symlink_is_rejected() {
    let dir = TestDir::new();
    let target = dir.0.join("other.toml");
    let path = dir.0.join(SERVER_LIST_FILE_NAME);
    fs::write(&target, "").expect("write symlink target");
    std::os::unix::fs::symlink(&target, &path).expect("create server list symlink");
    assert!(ServerListStore::load(&dir.0)
        .expect_err("server list symlinks must be rejected")
        .contains("regular file"));
}

#[test]
fn client_settings_toml_roundtrip_persists() {
    let dir = TestDir::new();
    let path = dir.0.join(SETTINGS_FILE_NAME);
    let settings = ClientSettings {
        fov: 89,
        view_distance: 20,
        ..ClientSettings::default()
    };
    settings.write_toml(&path).expect("write settings");
    let text = fs::read_to_string(path).expect("read settings");
    let (reloaded, warnings) = ClientSettings::from_toml(&text);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(reloaded, settings);
}

#[test]
fn shutdown_flag_registers_and_signals_atomically() {
    let flag = install_graceful_shutdown_flag().expect("install graceful shutdown flag");
    let clone = flag.clone();
    assert!(!clone.is_requested());
    #[cfg(unix)]
    {
        signal_hook::low_level::raise(signal_hook::consts::SIGTERM)
            .expect("send SIGTERM through the registered safe handler");
        assert!(
            clone.is_requested(),
            "SIGTERM should set every shared flag clone"
        );
    }
    #[cfg(not(unix))]
    flag.request_shutdown();
    assert!(clone.is_requested());
}
