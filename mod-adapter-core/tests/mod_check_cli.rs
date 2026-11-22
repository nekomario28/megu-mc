// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! CLI integration tests for the `megu-mod-check` binary
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

const BIN: &str = env!("CARGO_BIN_EXE_megu-mod-check");

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

/// Unique per-test scratch directory under `TMPDIR` (never `SystemTime` —
/// a pid + counter is unique enough and keeps the run deterministic).
fn fixture_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-mod-check-{}-{}-{tag}",
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
        .expect("spawn megu-mod-check");
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
    226, 134, 64, 226, 130, 56, 120, 137, 187, 6, 154, 164, 36, 217, 248, 152, 250, 223, 113, 214,
    74, 128, 16, 18, 156, 230, 220, 94, 191, 137, 236, 199, 206, 129, 55, 62, 88, 72, 119, 24, 162,
    241, 142, 215, 178, 224, 91, 176, 200, 107, 110, 141, 67, 21, 160, 73, 188, 224, 251, 49, 125,
    165, 73, 95, 11, 58, 164, 185, 209, 118, 129, 77, 7, 9, 53, 187, 6, 245, 68, 114, 220, 89, 11,
    225, 141, 50, 49, 249, 64, 186, 113, 12, 152, 30, 93, 204, 162, 221, 96, 96, 201, 51, 124, 197,
    160, 76, 68, 150, 90, 100, 55, 151, 231, 76, 174, 151, 146, 124, 202, 107, 12, 244, 78, 99, 58,
    140, 188, 190, 63, 240, 30, 82, 155, 235, 241, 58, 86, 42, 32, 61, 83, 74, 49, 151, 66, 150,
    43, 49, 19, 43, 241, 8, 217, 223, 66, 108, 243, 133, 3, 143, 45, 44, 229, 156, 110, 192, 137,
    131, 15, 5, 71, 183, 207, 53, 169, 206, 160, 75, 84, 83, 192, 231, 157, 33, 44, 153, 20, 6,
    226, 250, 85, 35, 191, 246, 47, 174, 243, 160, 115, 235, 188, 77, 169, 143, 117, 85, 41, 237,
    4, 181, 31, 140, 75, 173, 80, 222, 86, 26, 18, 84, 176, 81, 19, 143, 35, 130, 135, 145, 217,
    173, 121, 167, 177, 200, 213, 236, 24, 67, 241, 157, 95, 244, 218, 236, 108, 233, 208, 211,
    220, 183, 88, 102, 126, 114, 113, 102, 213, 72, 244, 119, 150, 155, 19, 199, 95, 89, 250, 62,
    209, 162, 66, 247, 79, 150, 180, 159, 19, 155, 159, 44, 231, 19, 74, 18, 53, 246, 232, 52, 58,
    101, 70, 60, 159, 127, 164, 230, 35, 193, 252, 49, 38, 184, 164, 29, 153, 174, 23, 124, 24, 62,
    0,
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
    v.as_object()
        .expect("report JSON root member must be an object")
}

fn str_field<'a>(o: &'a std::collections::BTreeMap<String, JsonValue>, key: &str) -> &'a str {
    o.get(key)
        .and_then(JsonValue::as_str)
        .unwrap_or_else(|| panic!("report JSON field '{key}' must be a string: {o:?}"))
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
    assert!(stdout
        .contains("Pre-flight report for \"Create NeoForge Adventure\" 1.4.0 (Modrinth pack)"));
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
    assert_eq!(
        code, 1,
        "manifest.json routed to CF parser must fail closed"
    );
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
    assert!(
        stdout.contains("\"Deflated Pack\" 9.9.9"),
        "stdout:\n{stdout}"
    );
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
    let (code, stdout, stderr) = run(&["--modpack", pack.to_str().unwrap(), "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let root = parse_json(stdout.as_bytes()).expect("stdout must be valid JSON");
    let r = obj(&root);
    assert_eq!(str_field(r, "schema"), "megu-modpack-preflight/1");
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
    assert_eq!(class_of("mods/create-1.21.1-6.0.6.jar"), "native_flagship");
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
    let (code, stdout, stderr) =
        run(&["--modpack", path.to_str().unwrap(), "--mc-version", "26.3"]);
    assert_eq!(
        code, 0,
        "26.3 pack on 26.3 target must pass, stderr: {stderr}"
    );
    assert!(stdout.contains("matches this server"));

    // Same pack against the default 1.21.1 target: version mismatch -> 2.
    let (code, stdout, _) = run(&["--modpack", path.to_str().unwrap()]);
    assert_eq!(code, 2, "26.3 pack on default target must be refused");
    assert!(
        stdout.contains("versions do not match"),
        "stdout:\n{stdout}"
    );
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
    assert!(stderr.contains("megu-mod-check"), "stderr: {stderr}");

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

// -- Minimal JVM .class builder (for --scan-jar fixtures) ----------------

/// Constant-pool item; indices are plain 1-based insertion order — the
/// fixtures are tiny so no deduplication is needed.
enum CpItem {
    Utf8(String),
    Class(u16),
    Int(i32),
}

#[derive(Default)]
struct Cp(Vec<CpItem>);

impl Cp {
    fn push(&mut self, item: CpItem) -> u16 {
        self.0.push(item);
        self.0.len() as u16
    }
    fn utf8(&mut self, s: &str) -> u16 {
        self.push(CpItem::Utf8(s.to_string()))
    }
    fn class(&mut self, internal_name: &str) -> u16 {
        let name_index = self.utf8(internal_name);
        self.push(CpItem::Class(name_index))
    }
    fn int(&mut self, v: i32) -> u16 {
        self.push(CpItem::Int(v))
    }
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for item in &self.0 {
            match item {
                CpItem::Utf8(s) => {
                    out.push(1);
                    out.extend((s.len() as u16).to_be_bytes());
                    out.extend(s.as_bytes());
                }
                CpItem::Class(name_index) => {
                    out.push(7);
                    out.extend(name_index.to_be_bytes());
                }
                CpItem::Int(v) => {
                    out.push(3);
                    out.extend(v.to_be_bytes());
                }
            }
        }
        out
    }
}

/// One annotation `element_value` for the fixture builder.
enum Ev {
    /// `s` string constant.
    S(String),
    /// `Z` boolean (stored as a `CONSTANT_Integer`).
    Z(bool),
    /// `@` nested annotation (type descriptor, element pairs).
    Nested(&'static str, Vec<(&'static str, Ev)>),
    /// `[` array of element values.
    Arr(Vec<Ev>),
}

fn ev_s(v: &str) -> Ev {
    Ev::S(v.to_string())
}

fn write_ev(out: &mut Vec<u8>, cp: &mut Cp, ev: &Ev) {
    match ev {
        Ev::S(v) => {
            out.push(b's');
            out.extend(cp.utf8(v).to_be_bytes());
        }
        Ev::Z(b) => {
            out.push(b'Z');
            out.extend(cp.int(i32::from(*b)).to_be_bytes());
        }
        Ev::Nested(desc, elems) => {
            out.push(b'@');
            write_ann(out, cp, desc, elems);
        }
        Ev::Arr(items) => {
            out.push(b'[');
            out.extend((items.len() as u16).to_be_bytes());
            for item in items {
                write_ev(out, cp, item);
            }
        }
    }
}

fn write_ann(out: &mut Vec<u8>, cp: &mut Cp, desc: &str, elems: &[(&str, Ev)]) {
    out.extend(cp.utf8(desc).to_be_bytes());
    out.extend((elems.len() as u16).to_be_bytes());
    for (name, ev) in elems {
        out.extend(cp.utf8(name).to_be_bytes());
        write_ev(out, cp, ev);
    }
}

const MIXIN_DESC: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const INJECT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/Inject;";
const AT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/At;";
const REDIRECT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/Redirect;";
const OVERWRITE_DESC: &str = "Lorg/spongepowered/asm/mixin/Overwrite;";

fn at(value: &str) -> Ev {
    Ev::Nested(AT_DESC, vec![("value", ev_s(value))])
}

/// One annotated `()V` method: name, annotation descriptor, elements.
struct FixtureMethod {
    name: &'static str,
    ann_desc: &'static str,
    elems: Vec<(&'static str, Ev)>,
}

/// Assemble a minimal but structurally valid `.class`: public class
/// `internal` extending `java/lang/Object`, zero fields, each method
/// carrying one `RuntimeInvisibleAnnotations` attribute with a single
/// annotation (Mixin's real CLASS retention), and a class-level
/// `@Mixin(targets = [...])` attribute.
fn build_mixin_class(internal: &str, targets: &[&str], methods: &[FixtureMethod]) -> Vec<u8> {
    let mut cp = Cp::default();
    let this_idx = cp.class(internal);
    let super_idx = cp.class("java/lang/Object");
    let rva_idx = cp.utf8("RuntimeInvisibleAnnotations");
    let desc_idx = cp.utf8("()V");

    // Class-level @Mixin payload — the `targets` string-array form real
    // mods use for obfuscated names.
    let mut mixin_payload = 1u16.to_be_bytes().to_vec();
    write_ann(
        &mut mixin_payload,
        &mut cp,
        MIXIN_DESC,
        &[(
            "targets",
            Ev::Arr(targets.iter().map(|t| ev_s(t)).collect()),
        )],
    );

    let mut method_parts = Vec::new();
    for m in methods {
        let name_idx = cp.utf8(m.name);
        let mut payload = 1u16.to_be_bytes().to_vec();
        write_ann(&mut payload, &mut cp, m.ann_desc, &m.elems);
        method_parts.push((name_idx, payload));
    }

    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // minor
    out.extend(61u16.to_be_bytes()); // major (Java 17)
    out.extend(((cp.0.len() + 1) as u16).to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes()); // public + super
    out.extend(this_idx.to_be_bytes());
    out.extend(super_idx.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces_count
    out.extend(0u16.to_be_bytes()); // fields_count
    out.extend((methods.len() as u16).to_be_bytes());
    for (name_idx, payload) in &method_parts {
        out.extend(0x0001u16.to_be_bytes()); // public
        out.extend(name_idx.to_be_bytes());
        out.extend(desc_idx.to_be_bytes());
        out.extend(1u16.to_be_bytes()); // one attribute
        out.extend(rva_idx.to_be_bytes());
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
    }
    out.extend(1u16.to_be_bytes()); // one class attribute
    out.extend(rva_idx.to_be_bytes());
    out.extend((mixin_payload.len() as u32).to_be_bytes());
    out.extend(mixin_payload);
    out
}

/// JAR with `safe.mixins.json` registering a `@Mixin` whose
/// `@Inject(method = "hurt", at = @At("HEAD"), cancellable = true)` on
/// `LivingEntity` lifts fully to `EntityDamagePre`.
fn safe_jar() -> Vec<u8> {
    let config = br#"{
        "required": true,
        "package": "com.safemod.mixin",
        "compatibilityLevel": "JAVA_17",
        "minVersion": "0.8",
        "mixins": ["LivingEntityMixin"],
        "injectors": {"defaultRequire": 1}
    }"#;
    let class = build_mixin_class(
        "com/safemod/mixin/LivingEntityMixin",
        &["net.minecraft.world.entity.LivingEntity"],
        &[FixtureMethod {
            name: "safemod$onHurt",
            ann_desc: INJECT_DESC,
            elems: vec![
                ("method", ev_s("hurt")),
                ("at", at("HEAD")),
                ("cancellable", Ev::Z(true)),
            ],
        }],
    );
    build_zip(&[
        stored_entry("safe.mixins.json", config),
        stored_entry("com/safemod/mixin/LivingEntityMixin.class", &class),
    ])
}

/// JAR whose single registered mixin carries a `@Redirect` and an
/// `@Overwrite` — both unliftable (`UnsupportedAnnotation`).
fn unliftable_jar() -> Vec<u8> {
    let config = br#"{
        "package": "com.badmod.mixin",
        "mixins": ["BrokenMixin"]
    }"#;
    let class = build_mixin_class(
        "com/badmod/mixin/BrokenMixin",
        &["net.minecraft.world.entity.LivingEntity"],
        &[
            FixtureMethod {
                name: "badmod$redirectHurt",
                ann_desc: REDIRECT_DESC,
                elems: vec![
                    ("method", ev_s("hurt")),
                    (
                        "at",
                        Ev::Nested(
                            AT_DESC,
                            vec![
                                ("value", ev_s("INVOKE")),
                                ("target", ev_s("net/minecraft/x/damage")),
                            ],
                        ),
                    ),
                ],
            },
            FixtureMethod {
                name: "badmod$overwriteHurt",
                ann_desc: OVERWRITE_DESC,
                elems: vec![],
            },
        ],
    );
    build_zip(&[
        stored_entry("bad.mixins.json", config),
        stored_entry("com/badmod/mixin/BrokenMixin.class", &class),
    ])
}

/// JAR with a safe `mixins` entry plus a `client`-only mixin carrying an
/// unliftable `@Overwrite` — skipped by the default server-side scan,
/// scanned under `--include-client-mixins`.
fn client_mixin_jar() -> Vec<u8> {
    let config = br#"{
        "package": "com.clientmod.mixin",
        "mixins": ["ServerMixin"],
        "client": ["ClientOnlyMixin"]
    }"#;
    let server = build_mixin_class(
        "com/clientmod/mixin/ServerMixin",
        &["net.minecraft.world.entity.LivingEntity"],
        &[FixtureMethod {
            name: "clientmod$onHurt",
            ann_desc: INJECT_DESC,
            elems: vec![("method", ev_s("hurt")), ("at", at("HEAD"))],
        }],
    );
    let client = build_mixin_class(
        "com/clientmod/mixin/ClientOnlyMixin",
        &["net.minecraft.world.entity.LivingEntity"],
        &[FixtureMethod {
            name: "clientmod$overwriteHurt",
            ann_desc: OVERWRITE_DESC,
            elems: vec![],
        }],
    );
    build_zip(&[
        stored_entry("clientmod.mixins.json", config),
        stored_entry("com/clientmod/mixin/ServerMixin.class", &server),
        stored_entry("com/clientmod/mixin/ClientOnlyMixin.class", &client),
    ])
}

#[test]
fn scan_jar_safe_inject_lifts_and_exits_0() {
    let dir = fixture_dir("scan-safe");
    let jar = write_fixture(&dir, "safe.jar", &safe_jar());
    let jar = jar.to_str().unwrap();

    let (code, stdout, stderr) = run(&["--scan-jar", jar]);
    assert_eq!(code, 0, "fully liftable JAR must exit 0, stderr: {stderr}");
    assert!(stderr.is_empty(), "clean run must not log: {stderr}");
    assert!(stdout.contains("mixin config"), "stdout:\n{stdout}");
    assert!(stdout.contains("EntityDamagePre"), "stdout:\n{stdout}");
    assert!(stdout.contains("LivingEntity"), "stdout:\n{stdout}");
    assert!(stdout.contains("Tier 2 eligible"), "stdout:\n{stdout}");

    let (code, stdout, stderr) = run(&["--scan-jar", jar, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let root = parse_json(stdout.as_bytes()).expect("stdout must be valid JSON");
    let r = obj(&root);
    assert_eq!(str_field(r, "schema"), "megu-jar-mixin-scan/1");
    assert_eq!(str_field(r, "path"), jar);
    assert_eq!(
        r.get("mixin_configs_found").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        r.get("classes_scanned").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        r.get("skipped_client_mixins").and_then(JsonValue::as_u64),
        Some(0)
    );
    assert_eq!(r.get("sites_found").and_then(JsonValue::as_u64), Some(1));
    assert_eq!(str_field(r, "verdict"), "fully_liftable");

    let lifted = r
        .get("lifted_hooks")
        .and_then(JsonValue::as_array)
        .expect("lifted_hooks array");
    assert_eq!(lifted.len(), 1);
    let hook = obj(&lifted[0]);
    assert_eq!(str_field(hook, "hook"), "EntityDamagePre");
    assert_eq!(
        str_field(hook, "mixin_class"),
        "com/safemod/mixin/LivingEntityMixin"
    );
    assert_eq!(str_field(hook, "target_method"), "hurt");
    assert_eq!(str_field(hook, "annotation"), "@Inject");
    assert_eq!(str_field(hook, "at"), "HEAD");
    assert_eq!(
        hook.get("cancellable").and_then(JsonValue::as_bool),
        Some(true)
    );
    assert!(r
        .get("unliftable")
        .and_then(JsonValue::as_array)
        .expect("unliftable array")
        .is_empty());
}

#[test]
fn scan_jar_unliftable_redirect_overwrite_exits_2() {
    let dir = fixture_dir("scan-bad");
    let jar = write_fixture(&dir, "bad.jar", &unliftable_jar());
    let jar = jar.to_str().unwrap();

    let (code, stdout, _) = run(&["--scan-jar", jar]);
    assert_eq!(code, 2, "unliftable sites must exit 2");
    assert!(stdout.contains("BrokenMixin"), "stdout:\n{stdout}");
    assert!(stdout.contains("@Redirect"), "stdout:\n{stdout}");
    assert!(stdout.contains("@Overwrite"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("UnsupportedAnnotation"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("Tier 3"), "stdout:\n{stdout}");
    assert!(stdout.contains("requires fallback"), "stdout:\n{stdout}");

    let (code, stdout, _) = run(&["--scan-jar", jar, "--json"]);
    assert_eq!(code, 2);
    let root = parse_json(stdout.as_bytes()).unwrap();
    let r = obj(&root);
    assert_eq!(str_field(r, "verdict"), "requires_fallback");
    assert_eq!(r.get("sites_found").and_then(JsonValue::as_u64), Some(2));
    let unliftable = r
        .get("unliftable")
        .and_then(JsonValue::as_array)
        .expect("unliftable array");
    assert_eq!(unliftable.len(), 2);
    for site in unliftable {
        let s = obj(site);
        assert_eq!(str_field(s, "reason"), "UnsupportedAnnotation");
        assert_eq!(str_field(s, "mixin_class"), "com/badmod/mixin/BrokenMixin");
    }
    let annotations: Vec<String> = unliftable
        .iter()
        .map(|u| str_field(obj(u), "annotation").to_string())
        .collect();
    assert!(annotations.contains(&"@Redirect".to_string()));
    assert!(annotations.contains(&"@Overwrite".to_string()));
}

#[test]
fn scan_jar_client_mixins_skipped_unless_flagged() {
    let dir = fixture_dir("scan-client");
    let jar = write_fixture(&dir, "client.jar", &client_mixin_jar());
    let jar = jar.to_str().unwrap();

    // Default server-side scan: the client-only mixin is skipped, the
    // server mixin lifts -> exit 0.
    let (code, stdout, stderr) = run(&["--scan-jar", jar, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let root = parse_json(stdout.as_bytes()).unwrap();
    let r = obj(&root);
    assert_eq!(
        r.get("classes_scanned").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        r.get("skipped_client_mixins").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(str_field(r, "verdict"), "fully_liftable");

    // --include-client-mixins scans the client class too — its
    // @Overwrite flips the verdict to requires_fallback.
    let (code, stdout, _) = run(&["--scan-jar", jar, "--include-client-mixins", "--json"]);
    assert_eq!(code, 2);
    let root = parse_json(stdout.as_bytes()).unwrap();
    let r = obj(&root);
    assert_eq!(
        r.get("classes_scanned").and_then(JsonValue::as_u64),
        Some(2)
    );
    assert_eq!(
        r.get("skipped_client_mixins").and_then(JsonValue::as_u64),
        Some(0)
    );
    assert_eq!(str_field(r, "verdict"), "requires_fallback");
}

#[test]
fn scan_jar_missing_and_corrupt_inputs_exit_1() {
    let dir = fixture_dir("scan-errors");
    let (code, _, stderr) = run(&["--scan-jar", "/nonexistent/mod.jar"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("megu-mod-check"), "stderr: {stderr}");

    // Not a ZIP archive at all.
    let garbage = write_fixture(&dir, "garbage.jar", b"not a zip at all");
    let (code, _, _) = run(&["--scan-jar", garbage.to_str().unwrap()]);
    assert_eq!(code, 1, "non-ZIP input must fail closed");

    // Config registers a class absent from the archive.
    let missing_class = write_fixture(
        &dir,
        "missing-class.jar",
        &build_zip(&[stored_entry(
            "x.mixins.json",
            br#"{"package": "com.x.mixin", "mixins": ["Gone"]}"#,
        )]),
    );
    let (code, _, stderr) = run(&["--scan-jar", missing_class.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("Gone"), "stderr: {stderr}");

    // Config-registered class is not a .class file (bad magic).
    let bad_class = write_fixture(
        &dir,
        "bad-class.jar",
        &build_zip(&[
            stored_entry(
                "x.mixins.json",
                br#"{"package": "com.x.mixin", "mixins": ["Broken"]}"#,
            ),
            stored_entry("com/x/mixin/Broken.class", b"definitely not a class"),
        ]),
    );
    let (code, _, stderr) = run(&["--scan-jar", bad_class.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("magic"), "stderr: {stderr}");
}

#[test]
fn modpack_and_scan_jar_are_mutually_exclusive() {
    let dir = fixture_dir("scan-mutex");
    let pack = write_fixture(&dir, "a.mrpack", &mrpack(MODRINTH_1211));
    let jar = write_fixture(&dir, "safe.jar", &safe_jar());

    let (code, _, stderr) = run(&[
        "--modpack",
        pack.to_str().unwrap(),
        "--scan-jar",
        jar.to_str().unwrap(),
    ]);
    assert_eq!(code, 2);
    assert!(stderr.contains("mutually exclusive"), "stderr: {stderr}");

    // Modpack-only flags are usage errors in scan mode.
    for extra in [
        vec!["--mc-version", "26.3"],
        vec!["--flagship", "create"],
        vec!["--profiles", "jei"],
    ] {
        let mut args = vec!["--scan-jar", jar.to_str().unwrap()];
        args.extend(extra);
        let (code, _, _) = run(&args);
        assert_eq!(code, 2, "args {args:?} must exit 2");
    }

    // --include-client-mixins outside --scan-jar mode.
    let (code, _, _) = run(&[
        "--modpack",
        pack.to_str().unwrap(),
        "--include-client-mixins",
    ]);
    assert_eq!(code, 2);

    // Missing --scan-jar value.
    let (code, _, _) = run(&["--scan-jar"]);
    assert_eq!(code, 2);
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
            stderr.contains("megu-mod-check"),
            "args {args:?} stderr must explain the error: {stderr}"
        );
    }
}

// -- --scan-mods-dir ------------------------------------------------------

/// Farmer's Delight-shaped JAR: one `*:cooking` + one `*:cutting` recipe,
/// one `*:add_item` loot modifier, one shared `c:tools/knife` item tag,
/// one unrelated vanilla recipe (counted as skipped), and MANIFEST noise.
/// No `.class` members and no mixin config — pure data.
fn farmers_delight_jar() -> Vec<u8> {
    build_zip(&[
        stored_entry("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
        stored_entry(
            "data/farmersdelight/recipe/cooking/beef_stew.json",
            br#"{"type":"farmersdelight:cooking","ingredients":[{"item":"minecraft:beef"},{"item":"minecraft:carrot"}],"result":{"id":"farmersdelight:beef_stew","count":1},"container":"minecraft:bowl","cookingtime":200}"#,
        ),
        stored_entry(
            "data/farmersdelight/recipe/cutting/straw_from_wheat.json",
            br#"{"type":"farmersdelight:cutting","ingredients":[{"item":"minecraft:wheat"}],"tool":{"tag":"c:tools/knife"},"result":[{"item":{"id":"farmersdelight:straw"},"chance":1.0}]}"#,
        ),
        stored_entry(
            "data/farmersdelight/loot_modifiers/straw_from_grass.json",
            br##"{"type":"farmersdelight:add_item","conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:blocks/short_grass"}],"item":"farmersdelight:straw","chance":1.0}"##,
        ),
        stored_entry(
            "data/c/tags/item/tools/knife.json",
            br#"{"replace":false,"values":["farmersdelight:flint_knife","farmersdelight:iron_knife"]}"#,
        ),
        stored_entry(
            "data/minecraft/recipe/cake.json",
            br#"{"type":"minecraft:crafting_shaped","result":{"id":"minecraft:cake","count":1}}"#,
        ),
    ])
}

/// A `custom_delight` addon JAR no code names: a `*:fermenting` recipe
/// plus a `*:cutting` recipe, both under the legacy `recipes/` dir —
/// proving the discovery is 100 % generic across addon namespaces.
fn custom_delight_addon_jar() -> Vec<u8> {
    build_zip(&[
        stored_entry(
            "data/custom_delight/recipes/fermenting/cider.json",
            br#"{"type":"custom_delight:fermenting","ingredients":[{"item":"minecraft:apple"},{"item":"minecraft:sugar"}],"result":{"id":"custom_delight:cider"},"container":"minecraft:glass_bottle","cookingtime":400}"#,
        ),
        stored_entry(
            "data/custom_delight/recipes/cutting/loaf_slices.json",
            br#"{"type":"custom_delight:cutting","ingredients":[{"item":"custom_delight:loaf"}],"tool":{"item":"minecraft:iron_axe"},"result":[{"item":{"id":"custom_delight:slice","count":4}}]}"#,
        ),
    ])
}

/// The baseline mods/ folder: a superseded perf mod written as literal
/// garbage bytes (proving the auto-skip never reads a byte), the two
/// culinary JARs, a fully liftable mixin JAR, an empty archive exercising
/// the case-insensitive `.JAR` match, and a non-JAR file that must be
/// ignored.
fn write_mixed_mods_dir(dir: &Path) {
    write_fixture(dir, "sodium-0.6.jar", b"not even a zip archive");
    write_fixture(dir, "farmersdelight-1.21.1.jar", &farmers_delight_jar());
    write_fixture(dir, "custom_delight_addon.jar", &custom_delight_addon_jar());
    write_fixture(dir, "safe-inject-mod.jar", &safe_jar());
    write_fixture(dir, "dataonly.JAR", &build_zip(&[]));
    write_fixture(dir, "readme.txt", b"place mod jars here\n");
}

/// Look up one `jars[]` entry of a parsed dir-scan report by file name.
fn jar_entry<'a>(
    jars: &'a [JsonValue],
    name: &str,
) -> &'a std::collections::BTreeMap<String, JsonValue> {
    jars.iter()
        .find(|j| str_field(obj(j), "jar") == name)
        .map(obj)
        .unwrap_or_else(|| panic!("jars[] must contain '{name}'"))
}

#[test]
fn scan_mods_dir_mixed_folder_exits_0() {
    let dir = fixture_dir("modsdir-mixed");
    write_mixed_mods_dir(&dir);
    let d = dir.to_str().unwrap();

    let (code, stdout, stderr) = run(&["--scan-mods-dir", d]);
    assert_eq!(code, 0, "clean dir scan must exit 0, stderr: {stderr}");
    assert!(stderr.is_empty(), "clean run must not log: {stderr}");
    assert!(stdout.contains("Mods directory scan"), "stdout:\n{stdout}");
    assert!(stdout.contains("Found 5 JAR(s)"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("1 non-JAR entry ignored"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("sodium-0.6.jar [sodium]: skipped"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("native culinary"), "stdout:\n{stdout}");
    assert!(stdout.contains("Tier 2"), "stdout:\n{stdout}");
    assert!(stdout.contains("Result: clean"), "stdout:\n{stdout}");

    let (code, stdout, stderr) = run(&["--scan-mods-dir", d, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let root = parse_json(stdout.as_bytes()).expect("stdout must be valid JSON");
    let r = obj(&root);
    assert_eq!(str_field(r, "schema"), "megu-mods-dir-scan/1");
    assert_eq!(str_field(r, "verdict"), "clean");
    assert_eq!(r.get("jars_found").and_then(JsonValue::as_u64), Some(5));
    assert_eq!(
        r.get("non_jar_entries").and_then(JsonValue::as_u64),
        Some(1)
    );

    let totals = obj(r.get("totals").unwrap());
    assert_eq!(
        totals
            .get("superseded_perf_mod")
            .and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        totals.get("native_culinary").and_then(JsonValue::as_u64),
        Some(2)
    );
    assert_eq!(
        totals
            .get("native_data_or_clean")
            .and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        totals.get("lifted_tier2").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        totals
            .get("requires_fallback")
            .and_then(JsonValue::as_u64),
        Some(0)
    );

    let culinary = obj(r.get("culinary").unwrap());
    assert_eq!(
        culinary.get("cooking_recipes").and_then(JsonValue::as_u64),
        Some(2),
        "beef_stew + cider"
    );
    assert_eq!(
        culinary.get("cutting_recipes").and_then(JsonValue::as_u64),
        Some(2),
        "straw + loaf_slices"
    );
    assert_eq!(
        culinary.get("loot_modifiers").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        culinary.get("tag_files").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        culinary.get("tag_values").and_then(JsonValue::as_u64),
        Some(2)
    );
    assert_eq!(
        culinary.get("skipped_recipes").and_then(JsonValue::as_u64),
        Some(1),
        "the vanilla cake recipe is skipped"
    );
    assert_eq!(
        culinary.get("data_files").and_then(JsonValue::as_u64),
        Some(7)
    );

    let jars = r
        .get("jars")
        .and_then(JsonValue::as_array)
        .expect("jars array");
    assert_eq!(jars.len(), 5);
    let names: Vec<String> = jars
        .iter()
        .map(|j| str_field(obj(j), "jar").to_string())
        .collect();
    assert_eq!(
        names,
        [
            "custom_delight_addon.jar",
            "dataonly.JAR",
            "farmersdelight-1.21.1.jar",
            "safe-inject-mod.jar",
            "sodium-0.6.jar"
        ],
        "deterministic filename order"
    );

    let sodium = jar_entry(jars, "sodium-0.6.jar");
    assert_eq!(str_field(sodium, "mod_id"), "sodium");
    assert_eq!(str_field(sodium, "class"), "superseded_perf_mod");
    assert_eq!(
        sodium.get("scanned").and_then(JsonValue::as_bool),
        Some(false),
        "superseded perf mods are never read"
    );

    let fd = jar_entry(jars, "farmersdelight-1.21.1.jar");
    assert_eq!(str_field(fd, "mod_id"), "farmersdelight");
    assert_eq!(str_field(fd, "class"), "native_culinary");
    let fdc = obj(fd.get("culinary").unwrap());
    assert_eq!(
        fdc.get("cooking_recipes").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        fdc.get("cutting_recipes").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        fdc.get("loot_modifiers").and_then(JsonValue::as_u64),
        Some(1)
    );

    let addon = jar_entry(jars, "custom_delight_addon.jar");
    assert_eq!(str_field(addon, "class"), "native_culinary");
    let ac = obj(addon.get("culinary").unwrap());
    assert_eq!(
        ac.get("cooking_recipes").and_then(JsonValue::as_u64),
        Some(1),
        "the generic *:fermenting recipe counts as cooking"
    );

    let safe = jar_entry(jars, "safe-inject-mod.jar");
    assert_eq!(str_field(safe, "class"), "lifted_tier2");
    let lifted = safe
        .get("lifted_hooks")
        .and_then(JsonValue::as_array)
        .expect("lifted_hooks array");
    assert_eq!(lifted.len(), 1);
    assert_eq!(str_field(obj(&lifted[0]), "hook"), "EntityDamagePre");

    let data = jar_entry(jars, "dataonly.JAR");
    assert_eq!(str_field(data, "class"), "native_data_or_clean");
    assert_eq!(str_field(data, "mod_id"), "dataonly");
}

#[test]
fn scan_mods_dir_unliftable_jar_exits_2() {
    let dir = fixture_dir("modsdir-bad");
    write_mixed_mods_dir(&dir);
    write_fixture(&dir, "heavy-redirect.jar", &unliftable_jar());
    let d = dir.to_str().unwrap();

    let (code, stdout, _) = run(&["--scan-mods-dir", d]);
    assert_eq!(code, 2, "an unliftable JAR must exit 2");
    assert!(stdout.contains("heavy-redirect.jar"), "stdout:\n{stdout}");
    assert!(stdout.contains("@Redirect"), "stdout:\n{stdout}");
    assert!(stdout.contains("BrokenMixin"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("UnsupportedAnnotation"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("requires fallback"), "stdout:\n{stdout}");

    let (code, stdout, _) = run(&["--scan-mods-dir", d, "--json"]);
    assert_eq!(code, 2);
    let root = parse_json(stdout.as_bytes()).unwrap();
    let r = obj(&root);
    assert_eq!(str_field(r, "verdict"), "requires_fallback");
    let totals = obj(r.get("totals").unwrap());
    assert_eq!(
        totals
            .get("requires_fallback")
            .and_then(JsonValue::as_u64),
        Some(1)
    );
    let jars = r.get("jars").and_then(JsonValue::as_array).unwrap();
    let heavy = jar_entry(jars, "heavy-redirect.jar");
    assert_eq!(str_field(heavy, "class"), "requires_fallback");
    let unliftable = heavy
        .get("unliftable")
        .and_then(JsonValue::as_array)
        .expect("unliftable array");
    assert_eq!(unliftable.len(), 2);
    for site in unliftable {
        let s = obj(site);
        assert_eq!(str_field(s, "mixin_class"), "com/badmod/mixin/BrokenMixin");
        assert_eq!(str_field(s, "reason"), "UnsupportedAnnotation");
    }
    // The rest of the folder is still classified and reported.
    assert_eq!(
        str_field(jar_entry(jars, "sodium-0.6.jar"), "class"),
        "superseded_perf_mod"
    );
    assert_eq!(
        str_field(jar_entry(jars, "safe-inject-mod.jar"), "class"),
        "lifted_tier2"
    );
}

#[test]
fn scan_mods_dir_include_client_mixins_wires_through() {
    let dir = fixture_dir("modsdir-client");
    write_fixture(&dir, "client.jar", &client_mixin_jar());
    let d = dir.to_str().unwrap();

    // Default server-side scan: the client-only @Overwrite is skipped,
    // the server mixin lifts -> clean, exit 0.
    let (code, stdout, stderr) = run(&["--scan-mods-dir", d, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let root = parse_json(stdout.as_bytes()).unwrap();
    let r = obj(&root);
    let jars = r.get("jars").and_then(JsonValue::as_array).unwrap();
    let entry = jar_entry(jars, "client.jar");
    assert_eq!(str_field(entry, "class"), "lifted_tier2");
    assert_eq!(
        entry
            .get("skipped_client_mixins")
            .and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        entry.get("classes_scanned").and_then(JsonValue::as_u64),
        Some(1)
    );

    // --include-client-mixins scans the client class too — its
    // @Overwrite flips the jar to requires_fallback.
    let (code, stdout, _) =
        run(&["--scan-mods-dir", d, "--include-client-mixins", "--json"]);
    assert_eq!(code, 2);
    let root = parse_json(stdout.as_bytes()).unwrap();
    let r = obj(&root);
    let jars = r.get("jars").and_then(JsonValue::as_array).unwrap();
    let entry = jar_entry(jars, "client.jar");
    assert_eq!(str_field(entry, "class"), "requires_fallback");
    assert_eq!(
        entry.get("classes_scanned").and_then(JsonValue::as_u64),
        Some(2)
    );
}

#[test]
fn scan_mods_dir_missing_and_corrupt_inputs_exit_1() {
    let dir = fixture_dir("modsdir-errors");

    let (code, _, stderr) = run(&["--scan-mods-dir", "/nonexistent/mods"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("megu-mod-check"), "stderr: {stderr}");
    assert!(stderr.contains("not an existing directory"), "stderr: {stderr}");

    // A regular file is not a directory.
    let file = write_fixture(&dir, "a-file", b"x");
    let (code, _, _) = run(&["--scan-mods-dir", file.to_str().unwrap()]);
    assert_eq!(code, 1, "non-directory input must fail closed");

    // A corrupt JAR (not a ZIP at all) aborts the scan and names the jar.
    write_fixture(&dir, "broken.jar", b"not a zip at all");
    let (code, _, stderr) = run(&["--scan-mods-dir", dir.to_str().unwrap()]);
    assert_eq!(code, 1, "corrupt JAR must fail closed");
    assert!(stderr.contains("broken.jar"), "stderr: {stderr}");

    // A mixin config registering an absent class also fails closed.
    let dir2 = fixture_dir("modsdir-missing-class");
    write_fixture(
        &dir2,
        "half.jar",
        &build_zip(&[stored_entry(
            "x.mixins.json",
            br#"{"package": "com.x.mixin", "mixins": ["Gone"]}"#,
        )]),
    );
    let (code, _, stderr) = run(&["--scan-mods-dir", dir2.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("Gone"), "stderr: {stderr}");

    // A subdirectory masquerading as a JAR cannot be read.
    let dir3 = fixture_dir("modsdir-subdir");
    fs::create_dir_all(dir3.join("fake.jar")).expect("create fake jar dir");
    let (code, _, stderr) = run(&["--scan-mods-dir", dir3.to_str().unwrap()]);
    assert_eq!(code, 1, "a *.jar directory must fail closed");
    assert!(stderr.contains("fake.jar"), "stderr: {stderr}");
}

#[test]
fn scan_mods_dir_mutual_exclusion_and_flag_gating_exit_2() {
    let dir = fixture_dir("modsdir-mutex");
    let pack = write_fixture(&dir, "a.mrpack", &mrpack(MODRINTH_1211));
    let jar = write_fixture(&dir, "safe.jar", &safe_jar());
    let d = dir.to_str().unwrap();

    for args in [
        vec!["--scan-mods-dir", d, "--modpack", pack.to_str().unwrap()],
        vec!["--scan-mods-dir", d, "--scan-jar", jar.to_str().unwrap()],
        vec!["--modpack", pack.to_str().unwrap(), "--scan-mods-dir", d],
        vec!["--scan-mods-dir", d, "--mc-version", "26.3"],
        vec!["--scan-mods-dir", d, "--flagship", "create"],
        vec!["--scan-mods-dir", d, "--profiles", "jei"],
        vec!["--scan-mods-dir"],
    ] {
        let (code, _, stderr) = run(&args);
        assert_eq!(code, 2, "args {args:?} must exit 2");
        assert!(
            stderr.contains("megu-mod-check"),
            "args {args:?} stderr must explain the error: {stderr}"
        );
    }

    let (code, _, stderr) =
        run(&["--scan-mods-dir", d, "--modpack", pack.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(stderr.contains("mutually exclusive"), "stderr: {stderr}");
}

#[test]
fn scan_mods_dir_empty_dir_and_equals_form_exit_0() {
    let dir = fixture_dir("modsdir-empty");

    let (code, stdout, stderr) = run(&["--scan-mods-dir", dir.to_str().unwrap()]);
    assert_eq!(code, 0, "empty dir is trivially clean, stderr: {stderr}");
    assert!(stdout.contains("Found 0 JAR(s)"), "stdout:\n{stdout}");
    assert!(stdout.contains("Result: clean"), "stdout:\n{stdout}");

    let arg = format!("--scan-mods-dir={}", dir.to_str().unwrap());
    let (code, stdout, _) = run(&[arg.as_str(), "--json"]);
    assert_eq!(code, 0);
    let root = parse_json(stdout.as_bytes()).unwrap();
    let r = obj(&root);
    assert_eq!(str_field(r, "schema"), "megu-mods-dir-scan/1");
    assert_eq!(str_field(r, "verdict"), "clean");
    assert_eq!(r.get("jars_found").and_then(JsonValue::as_u64), Some(0));
    assert!(r
        .get("jars")
        .and_then(JsonValue::as_array)
        .expect("jars array")
        .is_empty());
}
