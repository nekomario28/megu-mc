// SPDX-License-Identifier: LGPL-3.0-or-later
//! End-to-end tests for the on-demand PLaMo 3 mod auto-translator. All
//! fixtures are synthetic stored-ZIP mod JARs plus an in-process loopback
//! OpenAI-compatible mock server; no model or network access is needed.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use megu_client_gpu::mod_translator::{
    auto_translation_pack_exists, auto_translation_pack_path, detect_install_status,
    huggingface_download_url_allowed, load_auto_translation_pack_ja_jp, protect_placeholders,
    restore_placeholders, scan_untranslated_mods, set_auto_translation_pack_enabled,
    translate_entries_via_plamo3, translate_mods_against_endpoint, Plamo3Preset,
    TranslatorInstallState, AUTO_TRANSLATION_PACK_DIR, PLAMO3_GUIDANCE_EN, PLAMO3_GUIDANCE_JA,
    PLAMO3_LICENSE_NAME, PLAMO3_LICENSE_URL, PLAMO3_PRESETS,
};
use megu_core::json::{parse_json, write_json_string, JsonValue};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "megu-mod-translator-{tag}-{}-{nonce}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let lh_off = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&lh_off.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn json_object(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut out = String::from("{");
    for (index, (key, value)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_json_string(&mut out, key);
        out.push(':');
        write_json_string(&mut out, value);
    }
    out.push('}');
    out.into_bytes()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MockMode {
    Translate,
    DropMarker,
    BatchMismatch,
}

struct MockLlama {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl MockLlama {
    fn start(mode: MockMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let thread_requests = Arc::clone(&requests);
        let thread_bodies = Arc::clone(&bodies);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                thread_requests.fetch_add(1, Ordering::SeqCst);
                handle_mock_connection(stream, mode, Arc::clone(&thread_bodies));
            }
        });
        Self {
            endpoint: format!("http://127.0.0.1:{port}"),
            requests,
            bodies,
        }
    }

    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn handle_mock_connection(mut stream: TcpStream, mode: MockMode, bodies: Arc<Mutex<Vec<String>>>) {
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return;
        }
        data.extend_from_slice(&buf[..n]);
    };
    let headers = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while data.len() < header_end + content_length {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&data[header_end..header_end + content_length]).into_owned();
    bodies.lock().unwrap().push(body.clone());

    let parsed = parse_json(&body).unwrap();
    let user = parsed
        .get("messages")
        .and_then(JsonValue::as_array)
        .and_then(|messages| messages.get(1))
        .and_then(|message| message.get_str("content"))
        .unwrap()
        .to_owned();
    let inputs: Vec<String> = user
        .lines()
        .filter_map(|line| {
            let (n, text) = line.split_once(". ")?;
            n.parse::<usize>().ok()?;
            Some(text.to_owned())
        })
        .collect();
    let content = if mode == MockMode::BatchMismatch && inputs.len() > 1 {
        "1. 不一致".to_owned()
    } else {
        inputs
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let translated = if mode == MockMode::DropMarker {
                    text.replace("⟦P0⟧", "")
                } else {
                    text.clone()
                };
                format!("{}. 訳:{translated}", index + 1)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut quoted = String::new();
    write_json_string(&mut quoted, &content);
    let response_body = format!(
        "{{\"choices\":[{{\"message\":{{\"role\":\"assistant\",\"content\":{quoted}}}}}]}}"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn write_mod(
    mods_dir: &std::path::Path,
    file: &str,
    namespace: &str,
    en: &[(&str, &str)],
    ja: Option<&[(&str, &str)]>,
) {
    let en_json = json_object(en);
    let mut entries: Vec<(String, Vec<u8>)> =
        vec![(format!("assets/{namespace}/lang/en_us.json"), en_json)];
    if let Some(ja) = ja {
        entries.push((
            format!("assets/{namespace}/lang/ja_jp.json"),
            json_object(ja),
        ));
    }
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    std::fs::write(mods_dir.join(file), stored_zip(&refs)).unwrap();
}

#[test]
fn scan_reports_partial_and_missing_ja_jp() {
    let tmp = TempDir::new("scan");
    let mods = tmp.path().join("mods");
    let packs = tmp.path().join("resourcepacks");
    std::fs::create_dir_all(&mods).unwrap();
    write_mod(
        &mods,
        "partial.jar",
        "partialmod",
        &[("item.partial.one", "One"), ("item.partial.two", "Two")],
        Some(&[("item.partial.one", "一")]),
    );
    write_mod(
        &mods,
        "missing.jar",
        "missingmod",
        &[("block.missing.a", "Alpha"), ("block.missing.b", "Beta")],
        None,
    );

    let scan = scan_untranslated_mods(&mods, &packs).unwrap();
    assert_eq!(scan.len(), 2);
    let partial = scan.iter().find(|s| s.namespace == "partialmod").unwrap();
    assert_eq!(partial.total_keys, 2);
    assert_eq!(partial.already_ja_keys, 1);
    assert_eq!(partial.missing_keys, 1);
    let missing = scan.iter().find(|s| s.namespace == "missingmod").unwrap();
    assert_eq!(missing.total_keys, 2);
    assert_eq!(missing.already_ja_keys, 0);
    assert_eq!(missing.missing_keys, 2);
}

#[test]
fn zip_slip_mod_jar_is_rejected_safely() {
    let tmp = TempDir::new("zipslip");
    let mods = tmp.path().join("mods");
    let packs = tmp.path().join("resourcepacks");
    std::fs::create_dir_all(&mods).unwrap();
    std::fs::write(
        mods.join("evil.jar"),
        stored_zip(&[("assets/evil/lang/../en_us.json", b"{}")]),
    )
    .unwrap();
    let err = scan_untranslated_mods(&mods, &packs).unwrap_err();
    assert!(
        err.contains("cannot be scanned safely") || err.contains("safe mod JAR"),
        "{err}"
    );
}

#[test]
fn placeholder_protection_restores_formatting_and_rejects_corruption() {
    let input = "%1$s has %d §6items\\nnext\ttab%f%%";
    let (protected, placeholders) = protect_placeholders(input);
    assert!(protected.contains("⟦P0⟧"));
    assert!(!protected.contains("%1$s"));
    assert!(!protected.contains('§'));
    assert_eq!(
        restore_placeholders(&protected, &placeholders).as_deref(),
        Some(input)
    );
    let corrupted = protected.replacen("⟦P0⟧", "", 1);
    assert_eq!(restore_placeholders(&corrupted, &placeholders), None);
}

#[test]
fn corrupted_placeholder_translation_falls_back_to_english() {
    let mock = MockLlama::start(MockMode::DropMarker);
    let cancel = AtomicBool::new(false);
    let entries = vec![("key.drop".to_owned(), "%s Drop Marker".to_owned())];
    let out = translate_entries_via_plamo3(&mock.endpoint, &entries, &cancel, |_, _| {}).unwrap();
    assert_eq!(out.get("key.drop").unwrap(), "%s Drop Marker");
}

#[test]
fn batch_line_mismatch_falls_back_to_single_entry_requests() {
    let mock = MockLlama::start(MockMode::BatchMismatch);
    let cancel = AtomicBool::new(false);
    let entries = vec![
        ("key.one".to_owned(), "One".to_owned()),
        ("key.two".to_owned(), "Two".to_owned()),
    ];
    let out = translate_entries_via_plamo3(&mock.endpoint, &entries, &cancel, |_, _| {}).unwrap();
    assert_eq!(out.get("key.one").unwrap(), "訳:One");
    assert_eq!(out.get("key.two").unwrap(), "訳:Two");
    assert_eq!(mock.count(), 3);
}

#[test]
fn end_to_end_translation_writes_standard_resource_pack_and_manifest() {
    let tmp = TempDir::new("e2e");
    let mods = tmp.path().join("mods");
    let packs = tmp.path().join("resourcepacks");
    std::fs::create_dir_all(&mods).unwrap();
    write_mod(
        &mods,
        "testmod.jar",
        "testmod",
        &[
            ("item.testmod.alpha", "Alpha %s"),
            ("item.testmod.beta", "Beta"),
        ],
        Some(&[("item.testmod.alpha", "アルファ %s")]),
    );
    let mock = MockLlama::start(MockMode::Translate);
    let cancel = AtomicBool::new(false);
    let mut progress = Vec::new();
    let summary = translate_mods_against_endpoint(
        &mods,
        &packs,
        &mock.endpoint,
        Plamo3Preset::Standard2B,
        &cancel,
        |done, total| progress.push((done, total)),
    )
    .unwrap();
    assert_eq!(summary.translated_entries, 1);
    assert_eq!(progress, vec![(1, 1)]);
    assert!(auto_translation_pack_exists(&packs));

    let pack = auto_translation_pack_path(&packs);
    let pack_mcmeta = std::fs::read_to_string(pack.join("pack.mcmeta")).unwrap();
    assert_eq!(
        pack_mcmeta,
        r#"{"pack":{"pack_format":34,"description":"Megu Auto-Translated Mod Language Pack (ja_jp)"}}"#
    );
    let generated = std::fs::read_to_string(pack.join("assets/testmod/lang/ja_jp.json")).unwrap();
    assert!(generated.contains("item.testmod.beta"));
    assert!(generated.contains("訳:Beta"));
    let manifest = std::fs::read_to_string(pack.join(".megu_translation_manifest.json")).unwrap();
    assert!(manifest.contains("testmod.jar"));
    assert!(manifest.contains("standard-2b"));
    assert!(manifest.contains("ja_jp"));
    assert_eq!(mock.count(), 1);
    let bodies = mock.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("[plamo-translate]"));
    assert!(bodies[0].contains("target_lang=ja"));
    assert!(bodies[0].contains("\"temperature\":0"));
}

#[test]
fn incremental_second_run_skips_unchanged_mods_without_http_calls() {
    let tmp = TempDir::new("incremental");
    let mods = tmp.path().join("mods");
    let packs = tmp.path().join("resourcepacks");
    std::fs::create_dir_all(&mods).unwrap();
    write_mod(
        &mods,
        "testmod.jar",
        "testmod",
        &[("item.testmod.alpha", "Alpha")],
        None,
    );
    let mock = MockLlama::start(MockMode::Translate);
    let cancel = AtomicBool::new(false);
    translate_mods_against_endpoint(
        &mods,
        &packs,
        &mock.endpoint,
        Plamo3Preset::Standard2B,
        &cancel,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(mock.count(), 1);
    assert!(scan_untranslated_mods(&mods, &packs).unwrap().is_empty());
    let second = translate_mods_against_endpoint(
        &mods,
        &packs,
        &mock.endpoint,
        Plamo3Preset::Standard2B,
        &cancel,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(second.translated_entries, 0);
    assert_eq!(mock.count(), 1);
}

#[test]
fn resource_pack_toggle_and_live_load_are_instant_and_duplicate_free() {
    let tmp = TempDir::new("toggle");
    let mods = tmp.path().join("mods");
    let packs = tmp.path().join("resourcepacks");
    std::fs::create_dir_all(&mods).unwrap();
    write_mod(
        &mods,
        "testmod.jar",
        "testmod",
        &[("item.testmod.alpha", "Alpha")],
        None,
    );
    let mock = MockLlama::start(MockMode::Translate);
    let cancel = AtomicBool::new(false);
    translate_mods_against_endpoint(
        &mods,
        &packs,
        &mock.endpoint,
        Plamo3Preset::Standard2B,
        &cancel,
        |_, _| {},
    )
    .unwrap();
    let loaded = load_auto_translation_pack_ja_jp(&packs).unwrap();
    assert_eq!(loaded.get("item.testmod.alpha").unwrap(), "訳:Alpha");

    let mut enabled = vec!["vanilla".to_owned(), AUTO_TRANSLATION_PACK_DIR.to_owned()];
    set_auto_translation_pack_enabled(&mut enabled, true);
    assert_eq!(
        enabled
            .iter()
            .filter(|p| *p == AUTO_TRANSLATION_PACK_DIR)
            .count(),
        1
    );
    set_auto_translation_pack_enabled(&mut enabled, false);
    assert!(!enabled.iter().any(|p| p == AUTO_TRANSLATION_PACK_DIR));
    assert_eq!(enabled, vec!["vanilla"]);
}

#[test]
fn install_status_distinguishes_missing_model_missing_runtime_and_ready() {
    let tmp = TempDir::new("install");
    let cache = tmp.path().join("cache");
    let preset = Plamo3Preset::Lightweight08B;
    let missing = detect_install_status(&cache, preset);
    assert_eq!(missing.state(), TranslatorInstallState::ModelNotInstalled);
    assert_eq!(missing.action_label(), "[翻訳モデルをインストール]");

    std::fs::create_dir_all(preset.model_path(&cache).parent().unwrap()).unwrap();
    std::fs::write(preset.model_path(&cache), b"gguf").unwrap();
    std::fs::write(preset.chat_template_path(&cache), b"template").unwrap();
    let original_llama_bin = std::env::var_os("MEGU_LLAMA_SERVER_BIN");
    std::env::set_var(
        "MEGU_LLAMA_SERVER_BIN",
        tmp.path().join("definitely-missing-llama-server"),
    );
    let no_runtime = detect_install_status(&cache, preset);
    assert_eq!(no_runtime.state(), TranslatorInstallState::RuntimeMissing);
    assert_eq!(
        no_runtime.action_label(),
        "[llama-server を導入してください / インストール]"
    );
    if let Some(original) = original_llama_bin {
        std::env::set_var("MEGU_LLAMA_SERVER_BIN", original);
    } else {
        std::env::remove_var("MEGU_LLAMA_SERVER_BIN");
    }

    let bin = cache.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("llama-server"), b"#!/bin/sh\n").unwrap();
    let ready = detect_install_status(&cache, preset);
    assert_eq!(ready.state(), TranslatorInstallState::Ready);
    assert_eq!(ready.action_label(), "[未翻訳 Mod を日本語化]");
}

#[test]
fn huggingface_allowlist_rejects_ssrf_and_loopback_endpoint_rejects_remote() {
    assert!(huggingface_download_url_allowed(
        "https://huggingface.co/pfnet/plamo-3-translate-2b-GGUF/resolve/main/x.gguf"
    ));
    assert!(huggingface_download_url_allowed(
        "https://cdn-lfs-us-1.hf.co/object"
    ));
    assert!(huggingface_download_url_allowed(
        "https://cas-bridge.xethub.hf.co/object"
    ));
    assert!(!huggingface_download_url_allowed(
        "https://evil.example/object"
    ));
    assert!(!huggingface_download_url_allowed(
        "https://huggingface.co.evil.example/object"
    ));
    assert!(!huggingface_download_url_allowed(
        "http://huggingface.co/object"
    ));
    assert!(!huggingface_download_url_allowed(
        "https://user@huggingface.co/object"
    ));

    let cancel = AtomicBool::new(false);
    let err = translate_entries_via_plamo3(
        "http://192.0.2.8:8080",
        &[("k".to_owned(), "v".to_owned())],
        &cancel,
        |_, _| {},
    )
    .unwrap_err();
    assert!(err.contains("loopback"));
}

#[test]
fn preset_metadata_is_consent_forward_and_default_is_standard_2b() {
    assert_eq!(Plamo3Preset::default(), Plamo3Preset::Standard2B);
    assert_eq!(PLAMO3_PRESETS.len(), 3);
    assert!(Plamo3Preset::Standard2B.label().contains("推奨"));
    assert!(Plamo3Preset::Lightweight08B.label().contains("軽量"));
    assert!(Plamo3Preset::HighQuality8B.label().contains("高品質"));
    assert!(Plamo3Preset::Standard2B
        .resolve_url(Plamo3Preset::Standard2B.gguf_filename())
        .starts_with("https://huggingface.co/pfnet/plamo-3-translate-2b-GGUF/"));
    assert!(Plamo3Preset::Standard2B.approximate_size_bytes() > 1_500 * 1024 * 1024);
    assert_eq!(PLAMO3_LICENSE_NAME, "PLaMo Community License");
    assert!(PLAMO3_LICENSE_URL.starts_with("https://"));
    assert!(PLAMO3_GUIDANCE_JA.contains("同梱"));
    assert!(PLAMO3_GUIDANCE_JA.contains("ゲームプレイ中"));
    assert!(PLAMO3_GUIDANCE_EN.contains("not bundled"));
    assert!(PLAMO3_GUIDANCE_EN.contains("zero RAM/VRAM"));
}

#[test]
fn overlarge_selected_lang_entry_is_reported_not_silently_dropped() {
    let tmp = TempDir::new("oversize-lang");
    let mods = tmp.path().join("mods");
    let packs = tmp.path().join("resourcepacks");
    std::fs::create_dir_all(&mods).unwrap();
    write_mod(&mods, "oversize.jar", "oversize", &[("k", "v")], None);
    let jar = mods.join("oversize.jar");
    let mut bytes = std::fs::read(&jar).unwrap();
    let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    let declared = (9 * 1024 * 1024u32).to_le_bytes();
    bytes[central + 24..central + 28].copy_from_slice(&declared);
    std::fs::write(&jar, bytes).unwrap();
    let err = scan_untranslated_mods(&mods, &packs).unwrap_err();
    assert!(err.contains("could not be read safely"), "{err}");
}

#[test]
fn unreachable_translation_endpoint_is_an_error_not_fake_success() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cancel = AtomicBool::new(false);
    let err = translate_entries_via_plamo3(
        &format!("http://127.0.0.1:{port}"),
        &[("k".to_owned(), "v".to_owned())],
        &cancel,
        |_, _| {},
    )
    .unwrap_err();
    assert!(err.contains("request failed"), "{err}");
}
