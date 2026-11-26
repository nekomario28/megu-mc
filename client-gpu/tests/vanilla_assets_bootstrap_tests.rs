// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_client_gpu::live_session::SessionPaths;
use megu_client_gpu::vanilla_bootstrap::{
    discover_local_assets_dir, parse_asset_index_ref, parse_asset_objects,
    spawn_vanilla_assets_bootstrap_with_endpoints, AssetBootstrapProgress, AssetPriority,
    AssetTierFilter, MAX_ASSET_INDEX_BYTES,
};
use megu_core::crypto::sha1_hex;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
type SeenRequests = Arc<Mutex<Vec<String>>>;
type MockFixture = (String, SeenRequests, Vec<u8>, Vec<(String, Vec<u8>)>);

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "megu-assets-{label}-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[derive(Clone)]
struct Reply {
    body: Vec<u8>,
    declared_length: usize,
    delay: Duration,
}

fn serve_request(
    mut stream: TcpStream,
    routes: &HashMap<String, Reply>,
    seen: &Mutex<Vec<String>>,
) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
                if head.len() > 16 * 1024 {
                    return;
                }
            }
        }
    }
    let request = String::from_utf8_lossy(&head);
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
    seen.lock().unwrap().push(path.clone());
    let reply = routes.get(&path).cloned().unwrap_or(Reply {
        body: b"missing".to_vec(),
        declared_length: 7,
        delay: Duration::ZERO,
    });
    let _ = write!(
        stream,
        "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.declared_length
    );
    if !reply.delay.is_zero() {
        std::thread::sleep(reply.delay);
    }
    let _ = stream.write_all(&reply.body);
}

fn mock_server(routes: HashMap<String, Reply>, requests: usize) -> (String, SeenRequests) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let thread_seen = Arc::clone(&seen);
    std::thread::spawn(move || {
        for _ in 0..requests {
            if let Ok((stream, _)) = listener.accept() {
                serve_request(stream, &routes, &thread_seen);
            } else {
                break;
            }
        }
    });
    (base, seen)
}

fn asset_index(entries: &[(&str, &[u8])]) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let mut objects = String::from("{\"objects\":{");
    let mut payloads = Vec::new();
    for (index, (path, bytes)) in entries.iter().enumerate() {
        if index != 0 {
            objects.push(',');
        }
        let hash = sha1_hex(bytes);
        objects.push_str(&format!(
            "\"{path}\":{{\"hash\":\"{hash}\",\"size\":{}}}",
            bytes.len()
        ));
        payloads.push((hash, bytes.to_vec()));
    }
    objects.push_str("}}");
    (objects.into_bytes(), payloads)
}

fn bootstrap(
    index_id: &str,
    version_url: &str,
    base: &str,
    assets_dir: &Path,
    filter: AssetTierFilter,
) -> megu_client_gpu::vanilla_bootstrap::VanillaAssetsBootstrap {
    spawn_vanilla_assets_bootstrap_with_endpoints(
        index_id,
        version_url,
        assets_dir,
        &format!("{base}/index.json"),
        &format!("{base}/objects"),
        filter,
    )
    .expect("test endpoint accepted")
}

fn terminal(
    handle: &megu_client_gpu::vanilla_bootstrap::VanillaAssetsBootstrap,
) -> AssetBootstrapProgress {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let progress = handle.progress();
        if progress.is_terminal() {
            return progress;
        }
        assert!(
            Instant::now() < deadline,
            "asset bootstrap timed out: {progress:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn fixture(
    entries: &[(&str, &[u8])],
    music_delay: Duration,
    index_override: Option<Vec<u8>>,
) -> MockFixture {
    let (index, payloads) = asset_index(entries);
    let index = index_override.unwrap_or(index);
    let version = format!(
        "{{\"assetIndex\":{{\"id\":\"17\",\"sha1\":\"{}\",\"size\":{},\"url\":\"https://piston-meta.mojang.com/assets/17.json\"}}}}",
        sha1_hex(&index), index.len()
    );
    let mut routes = HashMap::new();
    routes.insert(
        "/version.json".to_owned(),
        Reply {
            body: version.into_bytes(),
            declared_length: 0,
            delay: Duration::ZERO,
        },
    );
    let index_reply = Reply {
        body: index.clone(),
        declared_length: index.len(),
        delay: Duration::ZERO,
    };
    routes.insert("/index.json".to_owned(), index_reply);
    let mut object_paths = Vec::new();
    for (hash, bytes) in &payloads {
        let path = format!("/objects/{}/{}", &hash[..2], hash);
        let delay = if path.ends_with(&sha1_hex(b"music track")) {
            music_delay
        } else {
            Duration::ZERO
        };
        routes.insert(
            path.clone(),
            Reply {
                body: bytes.clone(),
                declared_length: bytes.len(),
                delay,
            },
        );
        object_paths.push(path);
    }
    // Fill the version reply length only after the JSON body is built.
    if let Some(reply) = routes.get_mut("/version.json") {
        reply.declared_length = reply.body.len();
    }
    let requests = 2 + payloads.len();
    let (base, seen) = mock_server(routes, requests);
    (base, seen, index, payloads)
}

#[test]
fn full_bootstrap_prioritizes_fonts_and_lang_and_reports_fonts_ready_before_music() {
    let entries = [
        (
            "minecraft/sounds/music/calm1.ogg",
            b"music track".as_slice(),
        ),
        (
            "minecraft/lang/ja_jp.json",
            "{\"menu.done\":\"完了\"}".as_bytes(),
        ),
        (
            "minecraft/font/unifont_jp_patch.zip",
            b"font patch".as_slice(),
        ),
    ];
    let (base, seen, index, payloads) = fixture(&entries, Duration::from_millis(350), None);
    let root = temp_dir("e2e");
    let handle = bootstrap(
        "17",
        &format!("{base}/version.json"),
        &base,
        &root,
        AssetTierFilter::All,
    );
    let mut saw_fonts_ready = false;
    let result = loop {
        match handle.progress() {
            AssetBootstrapProgress::Downloading { fonts_ready, .. } => {
                saw_fonts_ready |= fonts_ready
            }
            state if state.is_terminal() => break state,
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(
        saw_fonts_ready,
        "fonts_ready never became true before completion: {result:?}"
    );
    assert!(
        matches!(result, AssetBootstrapProgress::Ready { .. }),
        "{result:?}"
    );
    assert_eq!(std::fs::read(root.join("indexes/17.json")).unwrap(), index);
    for (hash, bytes) in payloads {
        assert_eq!(
            std::fs::read(root.join("objects").join(&hash[..2]).join(&hash)).unwrap(),
            bytes
        );
    }
    let requested = seen.lock().unwrap().clone();
    let music = requested
        .iter()
        .position(|p| p.contains(&sha1_hex(b"music track")))
        .unwrap();
    let font = requested
        .iter()
        .position(|p| p.contains(&sha1_hex(b"font patch")))
        .unwrap();
    let lang = requested
        .iter()
        .position(|p| p.contains(&sha1_hex("{\"menu.done\":\"完了\"}".as_bytes())))
        .unwrap();
    assert!(
        font < music && lang < music,
        "Tier0 was not fetched before music: {requested:?}"
    );
    assert!(
        requested.len() == 5,
        "version + index + 3 assets expected: {requested:?}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn verified_local_objects_are_reused_without_object_gets_on_second_run() {
    let entries = [
        ("minecraft/font/unifont_jp_patch.zip", b"font".as_slice()),
        ("minecraft/lang/ja_jp.json", b"lang".as_slice()),
    ];
    let (base, seen, _, payloads) = fixture(&entries, Duration::ZERO, None);
    let root = temp_dir("resume");
    let first = bootstrap(
        "17",
        &format!("{base}/version.json"),
        &base,
        &root,
        AssetTierFilter::All,
    );
    let first_result = terminal(&first);
    assert!(
        matches!(first_result, AssetBootstrapProgress::Ready { .. }),
        "{first_result:?}"
    );
    let before = seen.lock().unwrap().len();
    let second = bootstrap(
        "17",
        "https://piston-meta.mojang.com/version.json",
        &base,
        &root,
        AssetTierFilter::All,
    );
    assert!(matches!(
        terminal(&second),
        AssetBootstrapProgress::Ready { .. }
    ));
    let after = seen.lock().unwrap().len();
    assert_eq!(
        after, before,
        "local-first run made unexpected HTTP requests"
    );
    assert_eq!(
        before, 4,
        "first pass should request version, index and two objects"
    );
    for (hash, bytes) in payloads {
        assert_eq!(
            std::fs::read(root.join("objects").join(&hash[..2]).join(hash)).unwrap(),
            bytes
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bad_object_hash_or_truncated_object_fails_without_corrupt_staging_files() {
    for (label, actual, declared, expected_hash) in [
        (
            "sha",
            b"wrong".as_slice(),
            b"wrong".len(),
            sha1_hex(b"right"),
        ),
        ("short", b"abc".as_slice(), 9, sha1_hex(b"abcdefghi")),
    ] {
        let index = format!("{{\"objects\":{{\"minecraft/font/unifont_jp_patch.zip\":{{\"hash\":\"{}\",\"size\":{}}}}}}}", expected_hash, declared).into_bytes();
        let version = format!("{{\"assetIndex\":{{\"id\":\"17\",\"sha1\":\"{}\",\"size\":{},\"url\":\"https://piston-meta.mojang.com/a.json\"}}}}", sha1_hex(&index), index.len());
        let hash = expected_hash;
        let mut routes = HashMap::new();
        routes.insert(
            "/version.json".to_owned(),
            Reply {
                body: version.clone().into_bytes(),
                declared_length: version.len(),
                delay: Duration::ZERO,
            },
        );
        routes.insert(
            "/index.json".to_owned(),
            Reply {
                body: index.clone(),
                declared_length: index.len(),
                delay: Duration::ZERO,
            },
        );
        routes.insert(
            format!("/objects/{}/{}", &hash[..2], hash),
            Reply {
                body: actual.to_vec(),
                declared_length: declared,
                delay: Duration::ZERO,
            },
        );
        let (base, _) = mock_server(routes, 3);
        let root = temp_dir(label);
        let handle = bootstrap(
            "17",
            &format!("{base}/version.json"),
            &base,
            &root,
            AssetTierFilter::All,
        );
        assert!(matches!(
            terminal(&handle),
            AssetBootstrapProgress::Failed(_)
        ));
        let object_files: Vec<_> = std::fs::read_dir(root.join("objects"))
            .unwrap()
            .flat_map(|entry| {
                std::fs::read_dir(entry.unwrap().path())
                    .into_iter()
                    .flatten()
            })
            .map(|entry| entry.unwrap().path())
            .collect();
        assert!(
            object_files.is_empty(),
            "failed object left files: {object_files:?}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn rejects_non_mojang_urls_and_finds_official_assets_directory() {
    let root = temp_dir("discovery");
    assert!(spawn_vanilla_assets_bootstrap_with_endpoints(
        "17",
        "https://evil.example/version.json",
        &root,
        "http://127.0.0.1:1/index.json",
        "http://127.0.0.1:1/objects",
        AssetTierFilter::All
    )
    .is_none());
    let standard_paths = SessionPaths {
        assets_dir: Some(root.join("standard")),
        ..SessionPaths::default()
    };
    assert!(
        megu_client_gpu::vanilla_bootstrap::spawn_vanilla_assets_bootstrap(
            "17",
            "http://127.0.0.1:1/version.json",
            &standard_paths,
            AssetTierFilter::All,
        )
        .is_none()
    );
    let game_dir = root.join(".minecraft");
    let assets = game_dir.join("assets");
    std::fs::create_dir_all(assets.join("indexes")).unwrap();
    std::fs::create_dir_all(assets.join("objects")).unwrap();
    std::fs::write(assets.join("indexes/17.json"), b"{}").unwrap();
    let paths = SessionPaths {
        game_dir: Some(game_dir),
        ..SessionPaths::default()
    };
    assert_eq!(
        discover_local_assets_dir("17", &paths),
        Some(assets.clone())
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn asset_metadata_parsers_enforce_sha1_caps_and_deterministic_priorities() {
    let version = format!("{{\"assetIndex\":{{\"id\":\"17\",\"sha1\":\"{}\",\"size\":1,\"url\":\"https://piston-meta.mojang.com/a.json\"}}}}", "a".repeat(40));
    assert_eq!(parse_asset_index_ref(&version).unwrap().id, "17");
    assert!(
        parse_asset_index_ref(&version.replace("piston-meta.mojang.com", "evil.example")).is_err()
    );
    assert!(parse_asset_index_ref(&version.replace(
        "\"size\":1",
        &format!("\"size\":{}", MAX_ASSET_INDEX_BYTES + 1)
    ))
    .is_err());
    let text = format!("{{\"objects\":{{\"minecraft/sounds/a.ogg\":{{\"hash\":\"{}\",\"size\":1}},\"minecraft/lang/ja_jp.json\":{{\"hash\":\"{}\",\"size\":1}},\"minecraft/font/unifont_all_no_pua.zip\":{{\"hash\":\"{}\",\"size\":1}}}}}}", "a".repeat(40), "b".repeat(40), "c".repeat(40));
    let objects = parse_asset_objects(&text).unwrap();
    assert_eq!(objects[0].priority, AssetPriority::Tier0FontsAndLang);
    assert_eq!(objects[1].priority, AssetPriority::Tier0FontsAndLang);
    assert_eq!(objects[2].priority, AssetPriority::Tier2Sounds);
    assert!(parse_asset_objects(&text.replace("minecraft/sounds/a.ogg", "../escape")).is_err());
}

#[test]
fn sound_definitions_precede_audio_objects_in_the_sound_tier() {
    let (index, _) = asset_index(&[
        ("minecraft/sounds/ambient/example.ogg", b"sample"),
        ("minecraft/sounds.json", b"{}"),
    ]);
    let objects = parse_asset_objects(std::str::from_utf8(&index).unwrap()).unwrap();
    assert_eq!(objects[0].logical_path, "minecraft/sounds.json");
    assert_eq!(objects[0].priority, AssetPriority::Tier2Sounds);
}
