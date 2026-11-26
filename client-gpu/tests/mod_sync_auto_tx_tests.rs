// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_gpu::mod_profile_tx::{
    GenerationModEntry, ModProfileStore, SOURCE_AUTOFETCH_DEPS, SOURCE_USER_EDIT,
};
use megu_client_gpu::mod_sync_auto::{
    build_missing_dep_consent_plan_at, sync_exact_mods_at, ExactModRequirement,
    MissingDependencyConsentChoice, MissingDependencyDescriptor, ModLoader, ModSide,
    ModSyncProgress,
};
use megu_core::crypto::{hex_encode, sha256};
use megu_core::hash::crc32_ieee;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn sha512_hex(bytes: &[u8]) -> String {
    megu_client_gpu::mod_profile_tx::hash_bytes_sha512_hex(bytes)
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn w16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn w32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, bytes) in entries {
        let name = name.as_bytes();
        let offset = out.len() as u32;
        let crc = crc32_ieee(bytes);
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, crc);
        w32(&mut out, bytes.len() as u32);
        w32(&mut out, bytes.len() as u32);
        w16(&mut out, name.len() as u16);
        w16(&mut out, 0);
        out.extend_from_slice(name);
        out.extend_from_slice(bytes);

        w32(&mut central, 0x0201_4b50);
        w16(&mut central, 20);
        w16(&mut central, 20);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, crc);
        w32(&mut central, bytes.len() as u32);
        w32(&mut central, bytes.len() as u32);
        w16(&mut central, name.len() as u16);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, 0);
        w32(&mut central, offset);
        central.extend_from_slice(name);
    }
    let cd_offset = out.len() as u32;
    let cd_len = central.len() as u32;
    out.extend_from_slice(&central);
    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, entries.len() as u16);
    w16(&mut out, entries.len() as u16);
    w32(&mut out, cd_len);
    w32(&mut out, cd_offset);
    w16(&mut out, 0);
    out
}

fn fabric_jar(mod_id: &str, version: &str, depends: &str) -> Vec<u8> {
    let manifest = format!(
        r#"{{"schemaVersion":1,"id":"{mod_id}","version":"{version}","environment":"*","depends":{{{depends}}}}}"#
    );
    stored_zip(&[("fabric.mod.json", manifest.as_bytes())])
}

fn corrupt_first_payload(mut zip: Vec<u8>) -> Vec<u8> {
    let name_len = u16::from_le_bytes([zip[26], zip[27]]) as usize;
    let payload = 30 + name_len;
    zip[payload] ^= 0xff;
    zip
}

type MockRoutes = HashMap<String, (String, Vec<u8>)>;
type CancelTrigger = Arc<Mutex<Option<(String, Arc<AtomicBool>)>>>;

struct MockServer {
    base: String,
    routes: Arc<Mutex<MockRoutes>>,
    cancel_trigger: CancelTrigger,
    shutdown: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(routes: MockRoutes) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("listener addr");
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let routes = Arc::new(Mutex::new(routes));
        let thread_routes = Arc::clone(&routes);
        let cancel_trigger = Arc::new(Mutex::new(None::<(String, Arc<AtomicBool>)>));
        let thread_cancel_trigger = Arc::clone(&cancel_trigger);
        let join = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut line = String::new();
                        BufReader::new(match stream.try_clone() {
                            Ok(clone) => clone,
                            Err(_) => continue,
                        })
                        .read_line(&mut line)
                        .expect("read request line");
                        let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                        if let Some((trigger_path, cancel)) = thread_cancel_trigger
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone()
                        {
                            if path == trigger_path {
                                cancel.store(true, Ordering::Relaxed);
                            }
                        }
                        let (status, body) = lock_routes(&thread_routes)
                            .get(&path)
                            .map(|(status, body)| (status.clone(), body.clone()))
                            .unwrap_or_else(|| {
                                eprintln!("mock 404: {path}");
                                ("404 Not Found".to_owned(), b"not found".to_vec())
                            });
                        let header = format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(&body);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("accept mock request: {e}"),
                }
            }
        });
        Self {
            base: format!("http://{addr}"),
            routes,
            cancel_trigger,
            shutdown,
            join: Some(join),
        }
    }

    fn extend(&self, additional: MockRoutes) {
        lock_routes(&self.routes).extend(additional);
    }

    fn cancel_on_request(&self, path: &str, cancel: &Arc<AtomicBool>) {
        *self
            .cancel_trigger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((path.to_owned(), Arc::clone(cancel)));
    }
}

fn lock_routes<'a>(routes: &'a Mutex<MockRoutes>) -> std::sync::MutexGuard<'a, MockRoutes> {
    routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn json_route(body: String) -> (String, Vec<u8>) {
    ("200 OK".to_owned(), body.into_bytes())
}

fn binary_route(body: Vec<u8>) -> (String, Vec<u8>) {
    ("200 OK".to_owned(), body)
}

fn project_json() -> String {
    r#"{"client_side":"required","server_side":"optional"}"#.to_owned()
}

fn file_json(
    addr: &str,
    filename: &str,
    jar: &[u8],
    sha512_override: Option<&str>,
    primary: bool,
) -> String {
    let sha512 = sha512_override
        .map(str::to_owned)
        .unwrap_or_else(|| sha512_hex(jar));
    let sha256 = hex_encode(&sha256(jar));
    format!(
        r#"{{"url":"{addr}/{filename}","filename":"{filename}","size":{},"primary":{primary},"hashes":{{"sha512":"{sha512}","sha256":"{sha256}"}}}}"#,
        jar.len()
    )
}

fn version_json(
    project_id: &str,
    version_id: &str,
    version_number: &str,
    game_version: &str,
    loader: &str,
    dependencies: &str,
    files: &[String],
) -> String {
    format!(
        r#"{{"id":"{version_id}","project_id":"{project_id}","version_number":"{version_number}","status":"listed","version_type":"release","game_versions":["{game_version}"],"loaders":["{loader}"],"dependencies":[{dependencies}],"files":[{}]}}"#,
        files.join(",")
    )
}

fn versions_path(project_id: &str, game_version: &str, loader: &str) -> String {
    format!(
        "/v2/project/{project_id}/version?game_versions=%5B%22{game_version}%22%5D&loaders=%5B%22{loader}%22%5D"
    )
}

fn exact_req(
    mod_id: &str,
    version: &str,
    project_id: &str,
    sha512: Option<String>,
    sha256: Option<String>,
) -> ExactModRequirement {
    ExactModRequirement {
        mod_id: mod_id.into(),
        required_version: version.into(),
        sha512,
        sha256,
        modrinth_slug: None,
        project_id: Some(project_id.into()),
        version_id: None,
        game_version: "1.21.1".into(),
        loader: ModLoader::Fabric,
        side: ModSide::Client,
        requested_by: vec![mod_id.into()],
    }
}

fn temp_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("megu-w1a-{label}-{}-{nanos}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
}

fn progress() -> Arc<Mutex<ModSyncProgress>> {
    Arc::new(Mutex::new(ModSyncProgress::Resolving))
}

fn install_verified_generation(
    profile_root: &Path,
    mod_id: &str,
    version: &str,
    jar: &[u8],
) -> (ModProfileStore, String) {
    let store = ModProfileStore::open(profile_root).expect("open store");
    let mut staged = store
        .stage_generation(SOURCE_USER_EDIT)
        .expect("stage generation");
    let filename = format!("{mod_id}.jar");
    fs::write(staged.generation_dir.join(&filename), jar).expect("write generation jar");
    let entry = GenerationModEntry {
        mod_id: mod_id.into(),
        version: version.into(),
        filename,
        sha512: sha512_hex(jar),
        sha256: Some(hex_encode(&sha256(jar))),
        source_reason: SOURCE_USER_EDIT.into(),
        modrinth_project_id: None,
        modrinth_version_id: None,
        requested_by: vec![mod_id.into()],
    };
    store
        .publish_staged_generation(&mut staged, SOURCE_USER_EDIT, vec![entry])
        .expect("publish verified generation");
    let id = store
        .active_generation_id()
        .expect("read active")
        .expect("active generation");
    (store, id)
}

fn generation_dirs(profile_root: &Path) -> Vec<String> {
    let mut names = fs::read_dir(profile_root.join("generations"))
        .expect("read generations")
        .map(|entry| {
            entry
                .expect("generation entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn exact_loader_version_and_file_digest_are_required() {
    assert_eq!(
        sha512_hex(b"abc"),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    let good = fabric_jar("mod_a", "1.0", "");
    let wrong = fabric_jar("mod_a", "1.0-wrong", "");
    let root = temp_root("exact-select");
    let profile = root.join("profile");
    let mut routes = HashMap::new();
    let good_sha = sha512_hex(&good);
    let good_sha256 = hex_encode(&sha256(&good));
    let correct_version = version_json(
        "projaaa1",
        "veraaa11",
        "1.0",
        "1.21.1",
        "fabric",
        "",
        &[
            file_json("BASE", "wrong.jar", &wrong, None, false),
            file_json("BASE", "good.jar", &good, None, true),
        ],
    );
    let wrong_loader_version = version_json(
        "projaaa1",
        "veraaa12",
        "1.0",
        "1.21.1",
        "neoforge",
        "",
        &[file_json("BASE", "wrong-loader.jar", &wrong, None, true)],
    );
    let wrong_version = version_json(
        "projaaa1",
        "veraaa13",
        "9.9",
        "1.21.1",
        "fabric",
        "",
        &[file_json("BASE", "wrong-version.jar", &wrong, None, true)],
    );
    let wrong_game_version = version_json(
        "projaaa1",
        "veraaa14",
        "1.0",
        "1.20.4",
        "fabric",
        "",
        &[file_json("BASE", "wrong-game.jar", &wrong, None, true)],
    );
    let server = MockServer::start(HashMap::new());
    let correct_version = correct_version.replace("BASE", &server.base);
    let wrong_loader_version = wrong_loader_version.replace("BASE", &server.base);
    let wrong_version = wrong_version.replace("BASE", &server.base);
    let wrong_game_version = wrong_game_version.replace("BASE", &server.base);
    routes.insert(
        versions_path("projaaa1", "1.21.1", "fabric"),
        json_route(format!(
            "[{wrong_game_version},{wrong_version},{wrong_loader_version},{correct_version}]"
        )),
    );
    routes.insert("/v2/project/projaaa1".into(), json_route(project_json()));
    routes.insert(
        format!("/v2/version_file/{good_sha}?algorithm=sha512"),
        json_route(correct_version.clone()),
    );
    routes.insert("/good.jar".into(), binary_route(good.clone()));
    server.extend(routes);
    let req = exact_req(
        "mod_a",
        "1.0",
        "projaaa1",
        Some(good_sha),
        Some(good_sha256),
    );
    let diff = sync_exact_mods_at(
        &[req],
        &profile,
        &progress(),
        &server.base,
        &AtomicBool::new(false),
    )
    .expect("exact sync selects matching loader/version/file");
    assert_eq!(diff.added_mods.len(), 1);
    assert_eq!(diff.added_mods[0].filename, "good.jar");

    let loader_profile = root.join("loader-profile");
    let loader_diff = sync_exact_mods_at(
        &[exact_req("mod_a", "1.0", "projaaa1", None, None)],
        &loader_profile,
        &progress(),
        &server.base,
        &AtomicBool::new(false),
    )
    .expect("wrong-loader candidate is skipped without falling back to it");
    assert_eq!(loader_diff.added_mods[0].filename, "good.jar");

    let mismatched = fabric_jar("mod_b", "1.0", "");
    let mismatched_sha = sha512_hex(&mismatched);
    let mut bad_routes = HashMap::new();
    let only_wrong_file = version_json(
        "projbbb1",
        "verbbb11",
        "1.0",
        "1.21.1",
        "fabric",
        "",
        &[file_json(
            &server.base,
            "mismatched.jar",
            &mismatched,
            None,
            true,
        )],
    );
    bad_routes.insert(
        versions_path("projbbb1", "1.21.1", "fabric"),
        json_route(format!("[{only_wrong_file}]")),
    );
    bad_routes.insert("/v2/project/projbbb1".into(), json_route(project_json()));
    bad_routes.insert(
        format!("/v2/version_file/{}?algorithm=sha512", "0".repeat(128)),
        json_route(only_wrong_file.clone()),
    );
    bad_routes.insert("/mismatched.jar".into(), binary_route(mismatched));
    let bad_server = MockServer::start(bad_routes);
    let bad_req = exact_req(
        "mod_b",
        "1.0",
        "projbbb1",
        Some("0".repeat(128)),
        Some("0".repeat(64)),
    );
    let bad_profile = root.join("bad-profile");
    assert!(sync_exact_mods_at(
        &[bad_req],
        &bad_profile,
        &progress(),
        &bad_server.base,
        &AtomicBool::new(false),
    )
    .expect_err("mismatched digest must reject every file")
    .contains("digest"));
    assert!(!bad_profile.join("active_generation.json").exists());
    assert_ne!(mismatched_sha, "0".repeat(128));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn transitive_dependency_closure_is_exact_and_cycle_safe() {
    let root = temp_root("closure");
    let profile = root.join("profile");
    let jar_a = fabric_jar("mod_a", "1.0", r#""mod_b":"[1.0]""#);
    let jar_b = fabric_jar("mod_b", "1.0", "");
    let server = MockServer::start(HashMap::new());
    let version_a = version_json(
        "projaaa1",
        "veraaa11",
        "1.0",
        "1.21.1",
        "fabric",
        r#"{"project_id":"projbbb1","version_id":"verbbb11","dependency_type":"required"}"#,
        &[file_json(&server.base, "mod-a.jar", &jar_a, None, true)],
    );
    let version_b = version_json(
        "projbbb1",
        "verbbb11",
        "1.0",
        "1.21.1",
        "fabric",
        r#"{"project_id":"projaaa1","version_id":"veraaa11","dependency_type":"required"}"#,
        &[file_json(&server.base, "mod-b.jar", &jar_b, None, true)],
    );
    let mut routes = HashMap::new();
    routes.insert(
        versions_path("projaaa1", "1.21.1", "fabric"),
        json_route(format!("[{version_a}]")),
    );
    routes.insert("/v2/version/verbbb11".into(), json_route(version_b));
    routes.insert("/v2/project/projaaa1".into(), json_route(project_json()));
    routes.insert("/v2/project/projbbb1".into(), json_route(project_json()));
    routes.insert("/mod-a.jar".into(), binary_route(jar_a));
    routes.insert("/mod-b.jar".into(), binary_route(jar_b.clone()));
    server.extend(routes);
    let diff = sync_exact_mods_at(
        &[exact_req("mod_a", "1.0", "projaaa1", None, None)],
        &profile,
        &progress(),
        &server.base,
        &AtomicBool::new(false),
    )
    .expect("cycle-safe transitive closure resolves");
    assert_eq!(diff.added_mods.len(), 2);
    assert_eq!(diff.auto_fetched_deps.len(), 1);
    assert_eq!(diff.auto_fetched_deps[0].mod_id, "mod_b");
    let store = ModProfileStore::open(&profile).expect("open profile");
    let active = store
        .active_generation_id()
        .expect("active id")
        .expect("active");
    let manifest = store.manifest(&active).expect("manifest");
    assert_eq!(manifest.mods.len(), 2);
    assert!(manifest
        .mods
        .iter()
        .any(|entry| entry.mod_id == "mod_b" && entry.source_reason == SOURCE_AUTOFETCH_DEPS));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn mid_batch_failure_and_cancel_leave_active_generation_untouched() {
    let root = temp_root("atomic");
    let profile = root.join("profile");
    let healthy = fabric_jar("healthy", "1.0", "");
    let (store, original_id) = install_verified_generation(&profile, "healthy", "1.0", &healthy);
    store
        .record_known_good_generation(&original_id)
        .expect("record known good");

    let jar_a = fabric_jar("mod_a", "1.0", "");
    let jar_b = fabric_jar("mod_b", "1.0", "");
    let server = MockServer::start(HashMap::new());
    let version_a = version_json(
        "projaaa1",
        "veraaa11",
        "1.0",
        "1.21.1",
        "fabric",
        "",
        &[file_json(&server.base, "mod-a.jar", &jar_a, None, true)],
    );
    let version_b = version_json(
        "projbbb1",
        "verbbb11",
        "1.0",
        "1.21.1",
        "fabric",
        "",
        &[file_json(
            &server.base,
            "mod-b.jar",
            &jar_b,
            Some("0".repeat(128).as_str()),
            true,
        )],
    );
    let mut routes = HashMap::new();
    for (project, version) in [("projaaa1", version_a), ("projbbb1", version_b)] {
        routes.insert(
            versions_path(project, "1.21.1", "fabric"),
            json_route(format!("[{version}]")),
        );
        routes.insert(format!("/v2/project/{project}"), json_route(project_json()));
    }
    routes.insert("/mod-a.jar".into(), binary_route(jar_a));
    routes.insert("/mod-b.jar".into(), binary_route(jar_b.clone()));
    server.extend(routes);
    let reqs = vec![
        exact_req("mod_a", "1.0", "projaaa1", None, None),
        exact_req("mod_b", "1.0", "projbbb1", None, None),
    ];
    let error = sync_exact_mods_at(
        &reqs,
        &profile,
        &progress(),
        &server.base,
        &AtomicBool::new(false),
    )
    .expect_err("second digest mismatch aborts the batch");
    assert!(error.contains("SHA-512"), "{error}");
    assert_eq!(
        store.active_generation_id().expect("active id").as_deref(),
        Some(original_id.as_str())
    );
    let active_dir = store
        .active_generation_dir()
        .expect("active dir")
        .expect("active");
    assert!(active_dir.join("healthy.jar").is_file());
    assert!(!active_dir.join("mod-a.jar").exists());
    assert!(!active_dir.join("mod-b.jar").exists());
    assert_eq!(generation_dirs(&profile), vec![original_id.clone()]);

    let correct_version_b = version_json(
        "projbbb1",
        "verbbb11",
        "1.0",
        "1.21.1",
        "fabric",
        "",
        &[file_json(&server.base, "mod-b.jar", &jar_b, None, true)],
    );
    server.extend(HashMap::from([(
        versions_path("projbbb1", "1.21.1", "fabric"),
        json_route(format!("[{correct_version_b}]")),
    )]));
    let cancel = Arc::new(AtomicBool::new(false));
    server.cancel_on_request("/mod-b.jar", &cancel);
    let cancel_error = sync_exact_mods_at(&reqs, &profile, &progress(), &server.base, &cancel)
        .expect_err("cancel during the second download aborts the batch");
    assert!(cancel_error.contains("cancelled"), "{cancel_error}");
    assert_eq!(
        store.active_generation_id().expect("active id").as_deref(),
        Some(original_id.as_str())
    );
    assert_eq!(generation_dirs(&profile), vec![original_id]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn bounded_zip_inspection_rejects_corrupt_and_zip_slip_payloads() {
    let good = fabric_jar("mod_a", "1.0", "");
    let corrupt = corrupt_first_payload(good.clone());
    let root = temp_root("zipguard");
    let corrupt_path = root.join("corrupt.jar");
    fs::write(&corrupt_path, &corrupt).expect("write corrupt jar");
    assert!(
        megu_client_gpu::mod_profile_tx::verify_jar_archive_safety(&corrupt_path)
            .expect_err("corrupt payload fails CRC verification")
            .contains("CRC")
    );

    let slip = stored_zip(&[("../evil.txt", b"bad")]);
    let slip_path = root.join("slip.jar");
    fs::write(&slip_path, &slip).expect("write zip-slip jar");
    assert!(megu_client_gpu::mod_profile_tx::verify_jar_archive_safety(&slip_path).is_err());
    let slip_dir = stored_zip(&[("../", b""), ("fabric.mod.json", b"{}")]);
    let slip_dir_path = root.join("slip-dir.jar");
    fs::write(&slip_dir_path, &slip_dir).expect("write zip-slip directory jar");
    assert!(megu_client_gpu::mod_profile_tx::verify_jar_archive_safety(&slip_dir_path).is_err());

    let server = MockServer::start(HashMap::new());
    let version = version_json(
        "projaaa1",
        "veraaa11",
        "1.0",
        "1.21.1",
        "fabric",
        "",
        &[file_json(&server.base, "corrupt.jar", &corrupt, None, true)],
    );
    let mut routes = HashMap::new();
    routes.insert(
        versions_path("projaaa1", "1.21.1", "fabric"),
        json_route(format!("[{version}]")),
    );
    routes.insert("/v2/project/projaaa1".into(), json_route(project_json()));
    routes.insert("/corrupt.jar".into(), binary_route(corrupt));
    server.extend(routes);
    let profile = root.join("profile");
    assert!(sync_exact_mods_at(
        &[exact_req("mod_a", "1.0", "projaaa1", None, None)],
        &profile,
        &progress(),
        &server.base,
        &AtomicBool::new(false),
    )
    .is_err());
    assert!(!profile.join("active_generation.json").exists());
    assert!(generation_dirs(&profile).is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn one_click_rollback_reverts_bad_mod_and_autofetched_dependency() {
    let root = temp_root("rollback");
    let profile = root.join("profile");
    let healthy = fabric_jar("healthy", "1.0", "");
    let (store, good_id) = install_verified_generation(&profile, "healthy", "1.0", &healthy);
    store
        .record_known_good_generation(&good_id)
        .expect("record known good");

    let jar_a = fabric_jar("mod_a", "1.0", r#""mod_b":"[1.0]""#);
    let jar_b = fabric_jar("mod_b", "1.0", "");
    let server = MockServer::start(HashMap::new());
    let version_a = version_json(
        "projaaa1",
        "veraaa11",
        "1.0",
        "1.21.1",
        "fabric",
        r#"{"project_id":"projbbb1","version_id":"verbbb11","dependency_type":"required"}"#,
        &[file_json(&server.base, "mod-a.jar", &jar_a, None, true)],
    );
    let version_b = version_json(
        "projbbb1",
        "verbbb11",
        "1.0",
        "1.21.1",
        "fabric",
        "",
        &[file_json(&server.base, "mod-b.jar", &jar_b, None, true)],
    );
    let mut routes = HashMap::new();
    routes.insert(
        versions_path("projaaa1", "1.21.1", "fabric"),
        json_route(format!("[{version_a}]")),
    );
    routes.insert("/v2/version/verbbb11".into(), json_route(version_b));
    routes.insert("/v2/project/projaaa1".into(), json_route(project_json()));
    routes.insert("/v2/project/projbbb1".into(), json_route(project_json()));
    routes.insert("/mod-a.jar".into(), binary_route(jar_a));
    routes.insert("/mod-b.jar".into(), binary_route(jar_b.clone()));
    server.extend(routes);
    let bad_diff = sync_exact_mods_at(
        &[exact_req("mod_a", "1.0", "projaaa1", None, None)],
        &profile,
        &progress(),
        &server.base,
        &AtomicBool::new(false),
    )
    .expect("bad batch initially activates as one transaction");
    let bad_id = bad_diff.to_generation_id.clone();
    assert_ne!(bad_id, good_id);
    assert_eq!(bad_diff.auto_fetched_deps.len(), 1);

    let rollback = store
        .rollback_to_last_good_generation()
        .expect("one-click rollback");
    assert_eq!(rollback.restored_generation_id, good_id);
    assert_eq!(
        rollback.rolled_back_generation_id.as_deref(),
        Some(bad_id.as_str())
    );
    assert!(rollback.diff.added_mods.is_empty());
    assert_eq!(rollback.diff.removed_mods.len(), 2);
    assert!(rollback
        .diff
        .removed_mods
        .iter()
        .any(|entry| entry.mod_id == "mod_a"));
    assert_eq!(rollback.diff.auto_fetched_deps.len(), 1);
    assert_eq!(rollback.diff.auto_fetched_deps[0].mod_id, "mod_b");
    assert_eq!(
        store.active_generation_id().expect("active").as_deref(),
        Some(good_id.as_str())
    );
    let active_dir = store
        .active_generation_dir()
        .expect("active dir")
        .expect("active");
    assert!(active_dir.join("healthy.jar").is_file());
    assert!(!active_dir.join("mod-a.jar").exists());
    let listed = store.list_generations().expect("list generations");
    assert_eq!(listed.len(), 2);
    assert!(listed
        .iter()
        .any(|entry| entry.generation_id == good_id && entry.active && entry.known_good));
    let explicit = store
        .diff_generations(&bad_id, &good_id)
        .expect("diff bad to good");
    assert_eq!(explicit.removed_mods.len(), 2);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn consent_plan_resolves_candidates_and_isolates_only_requesting_mods() {
    let jar = fabric_jar("dep_mod", "1.5", "");
    let server = MockServer::start(HashMap::new());
    let version = version_json(
        "projdep1",
        "verdep11",
        "1.5",
        "1.21.1",
        "fabric",
        "",
        &[file_json(&server.base, "dep.jar", &jar, None, true)],
    );
    let mut routes = HashMap::new();
    routes.insert(
        versions_path("projdep1", "1.21.1", "fabric"),
        json_route(format!("[{version}]")),
    );
    routes.insert(
        versions_path("projmiss1", "1.21.1", "fabric"),
        json_route("[]".into()),
    );
    routes.insert("/v2/project/projdep1".into(), json_route(project_json()));
    routes.insert("/v2/project/projmiss1".into(), json_route(project_json()));
    server.extend(routes);
    let descriptors = vec![
        MissingDependencyDescriptor {
            mod_id: "dep_mod".into(),
            required_version_range: ">=1.0".into(),
            requesting_mods: vec!["blocked_mod".into()],
            loader: ModLoader::Fabric,
            game_version: "1.21.1".into(),
            side: ModSide::Client,
            project_id: Some("projdep1".into()),
            version_id: None,
            modrinth_slug: None,
        },
        MissingDependencyDescriptor {
            mod_id: "missing_mod".into(),
            required_version_range: ">=1.0".into(),
            requesting_mods: vec!["other_blocked".into()],
            loader: ModLoader::Fabric,
            game_version: "1.21.1".into(),
            side: ModSide::Client,
            project_id: Some("projmiss1".into()),
            version_id: None,
            modrinth_slug: None,
        },
    ];
    let plan = build_missing_dep_consent_plan_at(&descriptors, &server.base)
        .expect("consent plan resolves");
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.candidates[0].mod_id, "dep_mod");
    assert_eq!(plan.candidates[0].version_number, "1.5");
    assert_eq!(plan.candidates[0].size_bytes as usize, jar.len());
    assert_eq!(plan.unsatisfied_dependencies.len(), 1);
    assert_eq!(plan.unsatisfied_dependencies[0].mod_id, "missing_mod");
    assert_eq!(
        plan.choices,
        vec![
            MissingDependencyConsentChoice::ApproveAutoFetchAndSoftReload,
            MissingDependencyConsentChoice::IsolateUnsatisfiedMods,
        ]
    );
    assert!(plan
        .isolated_mods_if_declined
        .iter()
        .any(|mod_id| mod_id == "blocked_mod"));
    assert!(plan
        .isolated_mods_if_declined
        .iter()
        .any(|mod_id| mod_id == "other_blocked"));
    assert!(!plan
        .isolated_mods_if_declined
        .iter()
        .any(|mod_id| mod_id == "healthy_mod"));
}

#[test]
fn sha512_matches_independent_multi_block_known_answer_vectors() {
    assert_eq!(
        sha512_hex(b""),
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
    );
    assert_eq!(
        sha512_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "204a8fc6dda82f0a0ced7beb8e08a41657c16ef468b228a8279be331a703c33596fd15c13b1b07f9aa1d3bea57789ca031ad85c7a71dd70354ec631238ca3445"
    );
    let cases: &[(usize, &str)] = &[
        (55, "b0220c772cbf6c1822e2cb38a437d0e1d58772417a4bbb21c961364f8b6143e05aa6316dca8d1d7b19e16448419076395f6086cb55101fbd6d5497b148e1745f"),
        (56, "962b64aae357d2a4fee3ded8b539bdc9d325081822b0bfc55583133aab44f18bafe11d72a7ae16c79ce2ba620ae2242d5144809161945f1367f41b3972e26e04"),
        (119, "130396a75cb483f2eee8c56d8a668bb3d2641f5243212c0bee2bd33da096ad9eb8179fe18f9eaacf76e09fae9de4c3f14ba13341e345be05bf76c182cc3468cb"),
        (120, "f241de612b01aa2fa3cf01531d2a8e5e17fc761dfd48a704a834a47f57d6eade7804ecc39be42fdef16ec6adeaf7c01c2fd0c4cc97d3860907cfa4a3b36d0c05"),
        (127, "828613968b501dc00a97e08c73b118aa8876c26b8aac93df128502ab360f91bab50a51e088769a5c1eff4782ace147dce3642554199876374291f5d921629502"),
        (128, "b73d1929aa615934e61a871596b3f3b33359f42b8175602e89f7e06e5f658a243667807ed300314b95cacdd579f3e33abdfbe351909519a846d465c59582f321"),
    ];
    for (len, expected) in cases {
        assert_eq!(sha512_hex(&vec![b'a'; *len]), *expected, "len={len}");
    }
}
