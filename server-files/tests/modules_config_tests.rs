// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_server_files::modules_config::{
    parse_modules_toml, verify_directory_against_lockfile, LockfileError, ModDownloadPolicy,
    ModLockEntry, ModLockfile, ModulesConfig,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let path = Path::new("/tmp").join(format!(
        "megu-modules-config-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn entry(id: &str, filename: &str, sha512: &str) -> ModLockEntry {
    ModLockEntry {
        mod_id: id.to_owned(),
        version: "1.2.3".to_owned(),
        filename: filename.to_owned(),
        sha512_hex: sha512.to_owned(),
        source_kind: "local".to_owned(),
        worldgen_epoch: Some("worldgen-v1".to_owned()),
        required_capabilities: vec!["claims.read".to_owned(), "claims.write".to_owned()],
    }
}

#[test]
fn modules_default_to_pure_vanilla_and_toml_round_trips() {
    let config = ModulesConfig::default();
    assert!(!config.container_protection);
    assert!(!config.town_claims);
    assert!(!config.delta_audit_rollback);
    assert!(!config.role_permissions);
    assert!(!config.homes_warps_economy);
    assert!(!config.live_web_map);
    assert!(!config.region_builder);

    let mut enabled = config.clone();
    enabled.town_claims = true;
    enabled.region_builder = true;
    let serialized = enabled.to_toml_string();
    assert_eq!(parse_modules_toml(&serialized).unwrap(), enabled);
    assert_eq!(parse_modules_toml(&config.to_toml_string()).unwrap(), config);
}

#[test]
fn modules_toml_rejects_unknown_missing_unsupported_and_invalid_values() {
    assert!(matches!(
        parse_modules_toml("schema_version = 1\nnew_module = true\n"),
        Err(LockfileError::UnknownKey(_))
    ));
    assert!(matches!(
        parse_modules_toml("container_protection = false\n"),
        Err(LockfileError::MissingField("schema_version"))
    ));
    assert!(matches!(
        parse_modules_toml("schema_version = 2\n"),
        Err(LockfileError::UnsupportedSchema(2))
    ));
    assert!(parse_modules_toml("schema_version = 1\ntown_claims = yes\n").is_err());
}

#[test]
fn lockfile_json_round_trip_is_sorted_and_sha512_is_validated() {
    let digest = "a".repeat(128);
    let lockfile = ModLockfile::new(vec![entry("zeta", "zeta.jar", &digest), entry("alpha", "alpha.jar", &digest)]).unwrap();
    assert_eq!(lockfile.entries[0].mod_id, "alpha");
    let serialized = lockfile.to_json_string().unwrap();
    let parsed = ModLockfile::parse(&serialized).unwrap();
    assert_eq!(parsed, lockfile);
    assert_eq!(parsed.to_json_string().unwrap(), serialized);

    let invalid = serialized.replace(&digest, "abc");
    assert!(matches!(ModLockfile::parse(&invalid), Err(LockfileError::InvalidSha512(_))));
    let unsupported = serialized.replace("\"schema_version\":1", "\"schema_version\":9");
    assert!(matches!(ModLockfile::parse(&unsupported), Err(LockfileError::UnsupportedSchema(9))));
}

#[test]
fn lockfile_rejects_path_traversal_and_duplicate_ids() {
    let digest = "b".repeat(128);
    assert!(matches!(
        ModLockfile::new(vec![entry("bad", "../escape.jar", &digest)]),
        Err(LockfileError::UnsafeFilename(_))
    ));
    assert!(matches!(
        ModLockfile::new(vec![entry("same", "one.jar", &digest), entry("same", "two.jar", &digest)]),
        Err(LockfileError::DuplicateModId(_))
    ));
    assert!(matches!(
        ModLockfile::new(vec![entry("bad", "C:\\mods\\x.jar", &digest)]),
        Err(LockfileError::UnsafeFilename(_))
    ));
}

#[test]
fn offline_policy_blocks_explicit_mode_and_environment_guard() {
    assert!(matches!(
        (ModDownloadPolicy { offline_only: true }).verify_download_allowed(),
        Err(LockfileError::DownloadsDisabled)
    ));
    let prior = std::env::var_os("MEGU_DISABLE_MOD_DOWNLOAD");
    std::env::set_var("MEGU_DISABLE_MOD_DOWNLOAD", "1");
    assert!(matches!(ModDownloadPolicy::default().verify_download_allowed(), Err(LockfileError::DownloadsDisabled)));
    if let Some(value) = prior {
        std::env::set_var("MEGU_DISABLE_MOD_DOWNLOAD", value);
    } else {
        std::env::remove_var("MEGU_DISABLE_MOD_DOWNLOAD");
    }
}

#[test]
fn directory_verification_accepts_matching_sha512_and_fails_closed() {
    let dir = temp_dir();
    let mod_path = dir.join("sample.jar");
    std::fs::write(&mod_path, b"abc").unwrap();
    let lockfile = ModLockfile::new(vec![entry(
        "sample",
        "sample.jar",
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
    )]).unwrap();
    verify_directory_against_lockfile(&dir, &lockfile).unwrap();

    std::fs::write(&mod_path, b"tampered").unwrap();
    assert!(matches!(verify_directory_against_lockfile(&dir, &lockfile), Err(LockfileError::DigestMismatch(_))));
    std::fs::remove_file(&mod_path).unwrap();
    assert!(matches!(verify_directory_against_lockfile(&dir, &lockfile), Err(LockfileError::MissingFile(_))));
    std::fs::remove_dir(&dir).unwrap();
}

#[test]
fn config_and_lockfile_atomic_save_load_round_trip() {
    let dir = temp_dir();
    let config_path = dir.join("megu-modules.toml");
    let lock_path = dir.join("megu-mods.lock.json");
    let config = ModulesConfig::default();
    config.save(&config_path).unwrap();
    assert_eq!(ModulesConfig::load(&config_path).unwrap(), config);
    let lockfile = ModLockfile::new(vec![entry("demo", "demo.jar", &"c".repeat(128))]).unwrap();
    lockfile.save(&lock_path).unwrap();
    assert_eq!(ModLockfile::load(&lock_path).unwrap(), lockfile);
    std::fs::remove_file(config_path).unwrap();
    std::fs::remove_file(lock_path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
