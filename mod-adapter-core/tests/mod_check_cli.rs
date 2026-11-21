// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! CLI integration tests for the `mc-spine-mod-check` binary
//! (`src/bin/mod_check.rs` + `modpack::ModpackManifest::from_path`).
//!
//! Every fixture is generated in-process: stored and raw-deflate `.mrpack`
//! / `.zip` archives are built byte-by-byte so the tests exercise the
//! crate's own EOCD/central-directory/inflate path with no external tools,
//! and the deflate payload below is a real zlib stream produced offline
//! (`zlib.compressobj(9, wbits=-15)` over the JSON in
//! [`DEFLATED_INDEX_DOC`]) so the Huffman decoder is tested against a
//! real-world compressor, not a hand-rolled stream.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use mod_adapter_core::data_loader::{parse_json, JsonValue};

const BIN: &str = env!("CARGO_BIN_EXE_mc-spine-mod-check");

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

/// Unique per-test scratch directory under `TMPDIR` (never `SystemTime` —
/// a pid + counter is unique enough and keeps the run deterministic).
fn fixture_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mc-spine-mod-check-{}-{}-{tag}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).expect("write fixture");
    path
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("spawn mc-spine-mod-check");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// -- Minimal ZIP writer (local headers + central directory + EOCD) ------

fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0; 256];
        let mut i = 0;
        while i < table.len() {
            let mut crc = i as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
                bit += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    };
    !data.iter().fold(!0, |crc, byte| {
        (crc >> 8) ^ TABLE[((crc as u8) ^ byte) as usize]
    })
}

struct ZipEntry<'a> {
    name: &'a str,
    /// Compression method: 0 stored, 8 deflate (payload already deflated).
    method: u16,
    flags: u16,
    /// Member payload as stored in the archive (compressed when method 8).
    payload: &'a [u8],
    /// Uncompressed size written into the directory record.
    uncomp_size: u32,
    /// CRC-32 of the *uncompressed* payload.
    crc32: u32,
}

fn stored_entry<'a>(name: &'a str, payload: &'a [u8]) -> ZipEntry<'a> {
    ZipEntry {
        name,
        method: 0,
        flags: 0,
        payload,
        uncomp_size: payload.len() as u32,
        crc32: crc32(payload),
    }
}

fn build_zip(entries: &[ZipEntry<'_>]) -> Vec<u8> {
    fn u16le(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn u32le(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    let mut out = Vec::new();
    let mut cd = Vec::new();
    for e in entries {
        let offset = out.len() as u32;
        u32le(&mut out, 0x0403_4b50);
        u16le(&mut out, 20);
        u16le(&mut out, e.flags);
        u16le(&mut out, e.method);
        u16le(&mut out, 0);
        u16le(&mut out, 0);
        u32le(&mut out, e.crc32);
        u32le(&mut out, e.payload.len() as u32);
        u32le(&mut out, e.uncomp_size);
        u16le(&mut out, e.name.len() as u16);
        u16le(&mut out, 0);
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(e.payload);

        u32le(&mut cd, 0x0201_4b50);
        u16le(&mut cd, 20);
        u16le(&mut cd, 20);
        u16le(&mut cd, e.flags);
        u16le(&mut cd, e.method);
        u16le(&mut cd, 0);
        u16le(&mut cd, 0);
        u32le(&mut cd, e.crc32);
        u32le(&mut cd, e.payload.len() as u32);
        u32le(&mut cd, e.uncomp_size);
        u16le(&mut cd, e.name.len() as u16);
        u16le(&mut cd, 0);
        u16le(&mut cd, 0);
        u16le(&mut cd, 0);
        u16le(&mut cd, 0);
        u32le(&mut cd, 0);
        u32le(&mut cd, offset);
        cd.extend_from_slice(e.name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&cd);
    u32le(&mut out, 0x0605_4b50);
    u16le(&mut out, 0);
    u16le(&mut out, 0);
    u16le(&mut out, entries.len() as u16);
    u16le(&mut out, entries.len() as u16);
    u32le(&mut out, cd.len() as u32);
    u32le(&mut out, cd_offset);
    u16le(&mut out, 0);
    out
}

// -- Fixtures -----------------------------------------------------------

/// Real-shaped 1.21.1 NeoForge Modrinth index (mirrors the unit-test
/// pack in `src/modpack.rs`): Create + Aether natives, five superseded
/// perf mods, one client-only mod, one datapack, one unknown JAR.
const MODRINTH_1211: &str = r#"{
    "formatVersion": 1,
    "game": "minecraft",
    "versionId": "1.4.0",
    "name": "Create NeoForge Adventure",
    "summary": "Create, Aether, and a performance stack",
    "files": [
        {"path": "mods/create-1.21.1-6.0.6.jar", "hashes": {"sha512": "a"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/abc/create.jar"], "fileSize": 16000000},
        {"path": "mods/aether-1.21.1-1.5.9-neoforge.jar", "hashes": {"sha512": "b"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/def/aether.jar"], "fileSize": 42000000},
        {"path": "mods/sodium-neoforge-0.6.13+mc1.21.1.jar", "hashes": {"sha512": "c"},
         "env": {"client": "required", "server": "optional"},
         "downloads": ["https://cdn.modrinth.com/data/ghi/sodium.jar"], "fileSize": 2000000},
        {"path": "mods/lithium-neoforge-0.15.0+mc1.21.1.jar", "hashes": {"sha512": "d"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/jkl/lithium.jar"], "fileSize": 1000000},
        {"path": "mods/modernfix-neoforge-5.20.2+mc1.21.1.jar", "hashes": {"sha512": "e"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/mno/modernfix.jar"], "fileSize": 3000000},
        {"path": "mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar", "hashes": {"sha512": "f"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/pqr/dh.jar"], "fileSize": 25000000},
        {"path": "mods/ferritecore-7.0.2-neoforge.jar", "hashes": {"sha512": "g"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/stu/ferritecore.jar"], "fileSize": 300000},
        {"path": "mods/iris-neoforge-1.8.0+mc1.21.1.jar", "hashes": {"sha512": "h"},
         "env": {"client": "required", "server": "unsupported"},
         "downloads": ["https://cdn.modrinth.com/data/vwx/iris.jar"], "fileSize": 2500000},
        {"path": "datapacks/terralith-v2.5.8.zip", "hashes": {"sha512": "i"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/yz1/terralith.zip"], "fileSize": 800000},
        {"path": "mods/mysterious-factory-3.2.1.jar", "hashes": {"sha512": "j"},
         "env": {"client": "required", "server": "required"},
         "downloads": ["https://cdn.modrinth.com/data/234/mystery.jar"], "fileSize": 900000}
    ],
    "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.93"}
}"#;

const CURSEFORGE_1211: &str = r#"{
    "minecraft": {
        "version": "1.21.1",
        "modLoaders": [{"id": "neoforge-21.1.93", "primary": true}]
    },
    "manifestType": "minecraftModpack",
    "manifestVersion": 1,
    "name": "CF Adventure",
    "version": "7.1",
    "author": "someone",
    "files": [
        {"projectID": 123, "fileID": 456, "required": true,
         "slug": "create", "fileName": "create-1.21.1-6.0.6.jar"},
        {"projectID": 234, "fileID": 567, "required": true, "slug": "sodium"}
    ],
    "overrides": "overrides"
}"#;

/// The JSON document [`DEFLATED_INDEX_PAYLOAD`] decompresses to.
const DEFLATED_INDEX_DOC: &str = concat!(
    "{\"formatVersion\":1,\"game\":\"minecraft\",\"versionId\":\"9.9.9\",",
    "\"name\":\"Deflated Pack\",\"summary\":\"stored in a deflate member to ",
    "exercise the RFC 1951 decoder\",\"files\":[{\"path\":\"mods/create-1.21.1-6.0.6.jar\",",
    "\"hashes\":{\"sha512\":\"",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "\"},\"env\":{\"client\":\"required\",\"server\":\"required\"},",
    "\"downloads\":[\"https://cdn.modrinth.com/data/abc/create.jar\"],",
    "\"fileSize\":16000000},{\"path\":\"mods/sodium-neoforge-0.6.13+mc1.21.1.jar\",",
    "\"hashes\":{\"sha512\":\"",
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "\"},\"env\":{\"client\":\"required\",\"server\":\"optional\"},",
    "\"downloads\":[\"https://cdn.modrinth.com/data/def/sodium.jar\"],",
    "\"fileSize\":2000000}],\"dependencies\":{\"minecraft\":\"1.21.1\",",
    "\"neoforge\":\"21.1.93\"}}"
);

/// `zlib.compressobj(9, wbits=-15)` over [`DEFLATED_INDEX_DOC`] — a real
/// dynamic-Huffman deflate stream, embedded so the archive reader's
/// inflater is checked against genuine compressor output. CRC-32 of the
/// plaintext: `0x6b5e827b`.
const DEFLATED_INDEX_PAYLOAD: &[u8] = &[
    197, 147, 77, 79, 195, 48, 12, 134, 255, 74, 148, 43, 109, 186, 108, 218, 164, 245, 10, 66,
    226, 134, 64, 226, 130, 56, 120, 137, 187, 6, 154, 164, 36, 217, 248, 152, 250, 223, 113,
    214, 74, 128, 16, 18, 156, 230, 220, 94, 191, 137, 236, 199, 206, 129, 55, 62, 88, 72, 119,
    24, 162, 241, 142, 215, 178, 224, 91, 176, 200, 107, 110, 141, 67, 21, 160, 73, 188, 224,
    251, 49, 125, 165, 73, 95, 11, 58, 164, 185, 209, 118, 129, 77, 7, 9, 53, 187, 6, 245, 68,
    114, 220, 89, 11, 225, 141, 50, 49, 249, 64, 186, 113, 12, 152, 30, 93, 204, 162, 221, 96,
    96, 201, 51, 124, 197, 160, 76, 68, 150, 90, 100, 55, 151, 231, 76, 174, 151, 146, 124, 202,
    107, 12, 244, 78, 99, 58, 140, 188, 190, 63, 240, 30, 82, 155, 235, 241, 58, 86, 42, 32, 61,
    83, 74, 49, 151, 66, 150, 43, 49, 19, 43, 241, 8, 217, 223, 66, 108, 243, 133, 3, 143, 45,
    44, 229, 156, 110, 192, 137, 131, 15, 5, 71, 183, 207, 53, 169, 206, 160, 75, 84, 83, 192,
    231, 157, 33, 44, 153, 20, 6, 226, 250, 85, 35, 191, 246, 47, 174, 243, 160, 115, 235, 188,
    77, 169, 143, 117, 85, 41, 237, 4, 181, 31, 140, 75, 173, 80, 222, 86, 26, 18, 84, 176, 81,
    19, 143, 35, 130, 135, 145, 217, 173, 121, 167, 177, 200, 213, 236, 24, 67, 241, 157, 95,
    244, 218, 236, 108, 233, 208, 211, 220, 183, 88, 102, 126, 114, 113, 102, 213, 72, 244, 119,
    150, 155, 19, 199, 95, 89, 250, 62, 209, 162, 66, 247, 79, 150, 180, 159, 19, 155, 159, 44,
    231, 19, 74, 18, 53, 246, 232, 52, 58, 101, 70, 60, 159, 127, 164, 230, 35, 193, 252, 49, 38,
    184, 164, 29, 153, 174, 23, 124, 24, 62, 0,
];

fn deflated_index_entry() -> ZipEntry<'static> {
    ZipEntry {
        name: "modrinth.index.json",
        method: 8,
        flags: 0,
        payload: DEFLATED_INDEX_PAYLOAD,
        uncomp_size: DEFLATED_INDEX_DOC.len() as u32,
        crc32: 0x6b5e_827b,
    }
}

fn mrpack(bytes_manifest: &str) -> Vec<u8> {
    build_zip(&[
        stored_entry("overrides/options.txt", b"fov:1.0\n"),
        stored_entry("modrinth.index.json", bytes_manifest.as_bytes()),
    ])
}

fn obj(v: &JsonValue) -> &std::collections::BTreeMap<String, JsonValue> {
    v.as_object().expect("report JSON root member must be an object")
}

fn str_field<'a>(o: &'a std::collections::BTreeMap<String, JsonValue>, key: &str) -> &'a str {
    o.get(key).and_then(JsonValue::as_str).unwrap_or_else(|| {
        panic!("report JSON field '{key}' must be a string: {o:?}")
    })
}

fn str_list(o: &std::collections::BTreeMap<String, JsonValue>, key: &str) -> Vec<String> {
    o.get(key)
        .and_then(JsonValue::as_array)
        .unwrap_or_else(|| panic!("report JSON field '{key}' must be an array"))
        .iter()
        .map(|v| v.as_str().expect("list entries are strings").to_string())
        .collect()
}

// -- Happy paths --------------------------------------------------------

#[test]
fn stored_mrpack_prints_human_summary() {
    let dir = fixture_dir("stored");
    let pack = write_fixture(&dir, "adventure.mrpack", &mrpack(MODRINTH_1211));
    let (code, stdout, stderr) = run(&["--modpack", pack.to_str().unwrap()]);
    assert_eq!(code, 0, "expected success, stderr: {stderr}");
    assert!(stderr.is_empty(), "clean run must not log: {stderr}");
    assert!(stdout.contains("Pre-flight report for \"Create NeoForge Adventure\" 1.4.0 (Modrinth pack)"));
    assert!(stdout.contains("matches this server"));
    assert!(stdout.contains("distanthorizons"));
    assert!(stdout.contains("iris"));
    assert!(stdout.contains("mysterious-factory"));
    assert!(stdout.contains("almost ready"), "stdout:\n{stdout}");
}

#[test]
fn standalone_modrinth_index_is_sniffed_by_key() {
    let dir = fixture_dir("standalone-mr");
    // Deliberately *not* named modrinth.index.json: key sniffing decides.
    let path = write_fixture(&dir, "pack.json", MODRINTH_1211.as_bytes());
    let (code, stdout, stderr) = run(&["--modpack", path.to_str().unwrap()]);
    assert_eq!(code, 0, "expected success, stderr: {stderr}");
    assert!(stdout.contains("Modrinth pack"), "stdout:\n{stdout}");
}

#[test]
fn standalone_curseforge_manifest_is_sniffed_by_key_and_name() {
    let dir = fixture_dir("standalone-cf");
    // Key sniff: an arbitrary name still routes on `manifestType`.
    let by_key = write_fixture(&dir, "export.json", CURSEFORGE_1211.as_bytes());
    let (code, stdout, stderr) = run(&["--modpack", by_key.to_str().unwrap()]);
    assert_eq!(code, 0, "key-sniffed manifest must pass, stderr: {stderr}");
    assert!(stdout.contains("CurseForge pack"), "stdout:\n{stdout}");
    assert!(stdout.contains("\"CF Adventure\" 7.1"));

    // Name sniff: a manifest.json without a manifestType-shaped body is
    // still routed to the CurseForge parser (which then rejects it for a
    // missing manifestType — same fail-closed behaviour as mod_launcher).
    let missing_type = r#"{"name": "x", "version": "1",
        "minecraft": {"version": "1.21.1"}, "files": []}"#;
    let by_name = write_fixture(&dir, "manifest.json", missing_type.as_bytes());
    let (code, _, _) = run(&["--modpack", by_name.to_str().unwrap()]);
    assert_eq!(code, 1, "manifest.json routed to CF parser must fail closed");
}

#[test]
fn stored_curseforge_zip_preflights() {
    let dir = fixture_dir("cf-zip");
    let pack = write_fixture(
        &dir,
        "cf-pack.zip",
        &build_zip(&[
            stored_entry("manifest.json", CURSEFORGE_1211.as_bytes()),
            stored_entry("overrides/config.txt", b"x=1\n"),
        ]),
    );
    let (code, stdout, stderr) = run(&["--modpack", pack.to_str().unwrap()]);
    assert_eq!(code, 0, "expected success, stderr: {stderr}");
    assert!(stdout.contains("CurseForge pack"));
    assert!(stdout.contains("sodium"), "perf mod skipped: {stdout}");
    assert!(stdout.contains("create"), "stdout:\n{stdout}");
}

#[test]
fn deflated_mrpack_member_preflights() {
    let dir = fixture_dir("deflated");
    let pack = write_fixture(
        &dir,
        "deflated.mrpack",
        &build_zip(&[
            stored_entry("overrides/note.txt", b"hi\n"),
            deflated_index_entry(),
        ]),
    );
    let (code, stdout, stderr) = run(&["--modpack", pack.to_str().unwrap()]);
    assert_eq!(code, 0, "deflated .mrpack must pass, stderr: {stderr}");
    assert!(stdout.contains("\"Deflated Pack\" 9.9.9"), "stdout:\n{stdout}");
    assert!(stdout.contains("sodium"));
}

#[test]
fn equals_form_and_duplicate_member_handling() {
    let dir = fixture_dir("equals");
    let pack = write_fixture(&dir, "dup.mrpack", &mrpack(MODRINTH_1211));
    let arg = format!("--modpack={}", pack.to_str().unwrap());
    let (code, stdout, _) = run(&[&arg]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Create NeoForge Adventure"));
}

// -- --json report ------------------------------------------------------

#[test]
fn json_report_matches_preflight_schema() {
    let dir = fixture_dir("json");
    let pack = write_fixture(&dir, "a.mrpack", &mrpack(MODRINTH_1211));
    let (code, stdout, stderr) =
        run(&["--modpack", pack.to_str().unwrap(), "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let root = parse_json(stdout.as_bytes()).expect("stdout must be valid JSON");
    let r = obj(&root);
    assert_eq!(str_field(r, "schema"), "mc-spine-modpack-preflight/1");
    assert_eq!(str_field(r, "pack_name"), "Create NeoForge Adventure");
    assert_eq!(str_field(r, "pack_version"), "1.4.0");
    assert_eq!(str_field(r, "format"), "modrinth");
    assert_eq!(str_field(r, "mc_version_declared"), "1.21.1");
    assert_eq!(str_field(r, "target"), "1.21.1");
    assert_eq!(str_field(r, "readiness"), "needs_jar_inspection");

    let verdict = obj(r.get("version_verdict").unwrap());
    assert_eq!(str_field(verdict, "kind"), "compatible");
    assert!(matches!(verdict.get("declared"), Some(JsonValue::Null)));
    assert!(matches!(verdict.get("reason"), Some(JsonValue::Null)));

    let loaders = r
        .get("loaders")
        .and_then(JsonValue::as_array)
        .expect("loaders array");
    assert_eq!(loaders.len(), 1);
    assert_eq!(str_field(obj(&loaders[0]), "id"), "neoforge");

    let entries = r
        .get("entries")
        .and_then(JsonValue::as_array)
        .expect("entries array");
    assert_eq!(entries.len(), 10);
    let class_of = |path: &str| {
        entries
            .iter()
            .find(|e| str_field(obj(e), "path") == path)
            .map(|e| str_field(obj(e), "class").to_string())
            .unwrap()
    };
    assert_eq!(
        class_of("mods/create-1.21.1-6.0.6.jar"),
        "native_flagship"
    );
    assert_eq!(
        class_of("mods/sodium-neoforge-0.6.13+mc1.21.1.jar"),
        "superseded_perf_mod"
    );
    assert_eq!(
        class_of("mods/iris-neoforge-1.8.0+mc1.21.1.jar"),
        "client_only_skipped"
    );
    assert_eq!(
        class_of("mods/mysterious-factory-3.2.1.jar"),
        "needs_inspection"
    );

    assert_eq!(
        str_list(r, "skipped_perf_mods"),
        vec![
            "distanthorizons",
            "ferritecore",
            "lithium",
            "modernfix",
            "sodium"
        ]
    );
    assert_eq!(str_list(r, "client_only_mods"), vec!["iris"]);
    // Default --flagship is just `create`, so `aether` and the mystery
    // JAR both land in needs_inspection.
    assert_eq!(
        str_list(r, "native_mods"),
        vec!["create", "terralith-v2.5.8.zip"]
    );
    assert_eq!(
        str_list(r, "needs_inspection_mods"),
        vec!["aether", "mysterious-factory"]
    );
}

#[test]
fn flagship_and_profiles_flags_change_classification() {
    let dir = fixture_dir("flags");
    let pack = write_fixture(&dir, "a.mrpack", &mrpack(MODRINTH_1211));
    let (code, stdout, _) = run(&[
        "--modpack",
        pack.to_str().unwrap(),
        "--flagship",
        "create,aether,mysterious-factory",
        "--profiles",
        "jei",
        "--json",
    ]);
    assert_eq!(code, 0);
    let root = parse_json(stdout.as_bytes()).unwrap();
    let r = obj(&root);
    // With the mystery JAR claimed as a flagship and everything else
    // skipped-or-native, the pack is fully ready.
    assert_eq!(str_field(r, "readiness"), "ready_with_auto_skips");
    assert!(str_list(r, "needs_inspection_mods").is_empty());
    let native = str_list(r, "native_mods");
    for id in ["aether", "create", "mysterious-factory"] {
        assert!(native.iter().any(|n| n == id), "missing native {id}");
    }
}

// -- --mc-version gating -------------------------------------------------

#[test]
fn mc_version_26_3_target_accepts_matching_pack() {
    let dir = fixture_dir("v263");
    let pack263 = MODRINTH_1211
        .replace("\"minecraft\": \"1.21.1\"", "\"minecraft\": \"26.3\"")
        .replace("1.21.1-6.0.6.jar", "26.3-6.0.6.jar");
    let path = write_fixture(&dir, "v263.mrpack", &mrpack(&pack263));
    let (code, stdout, stderr) = run(&[
        "--modpack",
        path.to_str().unwrap(),
        "--mc-version",
        "26.3",
    ]);
    assert_eq!(code, 0, "26.3 pack on 26.3 target must pass, stderr: {stderr}");
    assert!(stdout.contains("matches this server"));

    // Same pack against the default 1.21.1 target: version mismatch -> 2.
    let (code, stdout, _) = run(&["--modpack", path.to_str().unwrap()]);
    assert_eq!(code, 2, "26.3 pack on default target must be refused");
    assert!(stdout.contains("versions do not match"), "stdout:\n{stdout}");
    assert!(stdout.contains("not compatible"), "stdout:\n{stdout}");
}

#[test]
fn mc_version_26_3_target_refuses_1211_pack_with_exit_2() {
    let dir = fixture_dir("v263-mismatch");
    let path = write_fixture(&dir, "a.mrpack", &mrpack(MODRINTH_1211));
    for extra in [
        vec!["--mc-version", "26.3"],
        vec!["--mc-version", "26.3", "--json"],
    ] {
        let mut args = vec!["--modpack", path.to_str().unwrap()];
        args.extend(extra);
        let (code, stdout, _) = run(&args);
        assert_eq!(code, 2, "version mismatch must exit 2: {args:?}");
        if args.contains(&"--json") {
            let root = parse_json(stdout.as_bytes()).unwrap();
            let r = obj(&root);
            assert_eq!(str_field(r, "readiness"), "version_mismatch");
            assert_eq!(str_field(r, "target"), "26.3.0");
        }
    }
}

// -- Fail-closed errors (exit 1) ----------------------------------------

#[test]
fn missing_file_and_malformed_inputs_exit_1() {
    let dir = fixture_dir("errors");
    let (code, _, stderr) = run(&["--modpack", "/nonexistent/pack.mrpack"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("mc-spine-mod-check"), "stderr: {stderr}");

    let bad_json = write_fixture(&dir, "broken.json", b"{ not json");
    let (code, _, _) = run(&["--modpack", bad_json.to_str().unwrap()]);
    assert_eq!(code, 1, "malformed JSON must fail closed");

    let unrecognizable = write_fixture(&dir, "pack.json", b"{\"hello\": 1}");
    let (code, _, stderr) = run(&["--modpack", unrecognizable.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("cannot determine pack format"),
        "stderr: {stderr}"
    );
}

#[test]
fn corrupt_and_hostile_archives_exit_1() {
    let dir = fixture_dir("hostile");

    // PK local-header prefix with no EOCD -> unreadable archive.
    let truncated = write_fixture(&dir, "truncated.mrpack", b"PK\x03\x04garbage");
    let (code, _, _) = run(&["--modpack", truncated.to_str().unwrap()]);
    assert_eq!(code, 1, "PK file without EOCD must fail closed");

    // Valid archive shape containing neither manifest member.
    let no_manifest = write_fixture(
        &dir,
        "empty.mrpack",
        &build_zip(&[stored_entry("overrides/only.txt", b"x")]),
    );
    let (code, _, stderr) = run(&["--modpack", no_manifest.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("neither modrinth.index.json nor manifest.json"));

    // Manifest member declaring a >16 MiB uncompressed size: rejected
    // from the directory record before any decompression (zip-bomb cap).
    let mut oversized = stored_entry("modrinth.index.json", MODRINTH_1211.as_bytes());
    oversized.uncomp_size = 17 * 1024 * 1024;
    let bomb = write_fixture(&dir, "bomb.mrpack", &build_zip(&[oversized]));
    let (code, _, _) = run(&["--modpack", bomb.to_str().unwrap()]);
    assert_eq!(code, 1, "oversized member must fail closed");

    // CRC-32 mismatch: bit flip in the stored payload.
    let mut bad_crc = stored_entry("modrinth.index.json", MODRINTH_1211.as_bytes());
    bad_crc.crc32 ^= 1;
    let bad = write_fixture(&dir, "badcrc.mrpack", &build_zip(&[bad_crc]));
    let (code, _, _) = run(&["--modpack", bad.to_str().unwrap()]);
    assert_eq!(code, 1, "CRC mismatch must fail closed");

    // Unsupported compression method (bzip2).
    let mut bz = stored_entry("modrinth.index.json", MODRINTH_1211.as_bytes());
    bz.method = 12;
    let bzpath = write_fixture(&dir, "bz.mrpack", &build_zip(&[bz]));
    let (code, _, _) = run(&["--modpack", bzpath.to_str().unwrap()]);
    assert_eq!(code, 1, "unsupported method must fail closed");

    // Encrypted member flag.
    let mut enc = stored_entry("modrinth.index.json", MODRINTH_1211.as_bytes());
    enc.flags = 0x1;
    let encpath = write_fixture(&dir, "enc.mrpack", &build_zip(&[enc]));
    let (code, _, _) = run(&["--modpack", encpath.to_str().unwrap()]);
    assert_eq!(code, 1, "encrypted member must fail closed");
}

#[test]
fn path_traversal_manifest_member_exits_1() {
    let dir = fixture_dir("traversal");
    let hostile = r#"{"formatVersion": 1, "game": "minecraft", "versionId": "1",
        "name": "x", "files": [{"path": "../evil.jar", "hashes": {"sha512": "a"},
        "downloads": [], "fileSize": 1}], "dependencies": {"minecraft": "1.21.1"}}"#;
    for name in ["traversal.json", "traversal.mrpack"] {
        let bytes: Vec<u8> = if name.ends_with(".mrpack") {
            mrpack(hostile)
        } else {
            hostile.as_bytes().to_vec()
        };
        let path = write_fixture(&dir, name, &bytes);
        let (code, _, _) = run(&["--modpack", path.to_str().unwrap()]);
        assert_eq!(code, 1, "'..' member in {name} must fail closed");
    }
}

// -- CLI surface ---------------------------------------------------------

#[test]
fn help_prints_usage_and_exits_0() {
    for flag in ["--help", "-h"] {
        let (code, stdout, stderr) = run(&[flag]);
        assert_eq!(code, 0, "{flag} must exit 0");
        assert!(stderr.is_empty());
        assert!(stdout.contains("--modpack"), "usage text:\n{stdout}");
        assert!(stdout.contains("--mc-version"));
        assert!(stdout.contains("--flagship"));
        assert!(stdout.contains("--profiles"));
        assert!(stdout.contains("--json"));
    }
}

#[test]
fn invalid_cli_args_exit_2() {
    let dir = fixture_dir("cli2");
    let pack = write_fixture(&dir, "a.mrpack", &mrpack(MODRINTH_1211));
    let pack_str = pack.to_str().unwrap();
    for args in [
        // No --modpack at all.
        vec![],
        vec!["--json"],
        // Unknown flags and stray positionals.
        vec!["--bogus"],
        vec!["positional.mrpack"],
        vec!["--modpack", pack_str, "--unknown-flag"],
        vec!["--modpack", pack_str, "--json=pretty"],
        // Missing option values.
        vec!["--modpack"],
        vec!["--modpack", pack_str, "--mc-version"],
        // Unparsable and unsupported targets.
        vec!["--modpack", pack_str, "--mc-version", "wat"],
        vec!["--modpack", pack_str, "--mc-version", "1.20"],
        vec!["--modpack", pack_str, "--mc-version", "26.4"],
    ] {
        let (code, _, stderr) = run(&args);
        assert_eq!(code, 2, "args {args:?} must exit 2");
        assert!(
            stderr.contains("mc-spine-mod-check"),
            "args {args:?} stderr must explain the error: {stderr}"
        );
    }
}
