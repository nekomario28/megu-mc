// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Security & functional tests for the hardened server→client mod sync
//! protocol (`mod-adapter-core/src/mod_sync.rs`).
//!
//! Coverage: FIPS 180-4 SHA-256 known vectors and streaming equivalence,
//! `MSY1` wire roundtrip and every fail-closed decode rejection, the full
//! hostile-URL and hostile-filename batteries, consent gating, digest/size
//! mismatch refusal with no temp-file leakage, atomic verified installs
//! that re-plan as `already_installed_verified`, superseded-perf-mod
//! auto-skip, and symlink-escape refusal.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use mod_adapter_core::mod_sync::{
    compute_manifest_diff, compute_manifest_diff_with_policy, decode_root_hash_probe,
    decode_sync_manifest, digest_eq, digest_hex, encode_root_hash_probe, encode_sync_manifest,
    install_verified_bytes, is_superseded_sync_entry, is_trusted_mod_cdn_host, manifest_root_hash,
    materialize_into_profile, parse_sha256_hex, plan_mod_sync, prune_removed, sha256, sha256_hex,
    validate_download_url, validate_jar_file_name, validate_sync_entry, ConsentToken,
    ContentAddressedModCache, ModSyncEntry, ModSyncErrorKind, Sha256, SyncManifest, SyncPolicy,
    MAX_MOD_JAR_BYTES, MAX_SYNC_MODS, MOD_SYNC_CHANNEL, MSH1_MAGIC, MSY1_MAGIC,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

/// Unique per-test scratch directory under `TMPDIR` (never `SystemTime` —
/// a pid + counter is unique enough and keeps the run deterministic).
fn fixture_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-mod-sync-{}-{}-{tag}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

fn jar_bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}

fn entry(mod_id: &str, file_name: &str, url: &str, bytes: &[u8]) -> ModSyncEntry {
    ModSyncEntry {
        mod_id: mod_id.to_string(),
        file_name: file_name.to_string(),
        download_url: url.to_string(),
        sha256_hex: sha256_hex(bytes),
        size_bytes: bytes.len() as u64,
    }
}

fn good_entry() -> (ModSyncEntry, Vec<u8>) {
    let bytes = jar_bytes(0x42, 4096);
    (
        entry(
            "jei",
            "jei-1.21.1-neoforge-19.21.0.247.jar",
            "https://cdn.modrinth.com/data/abc/jei.jar",
            &bytes,
        ),
        bytes,
    )
}

fn policy() -> SyncPolicy {
    SyncPolicy::canonical()
}

/// Recursively list file names directly inside `dir` (no recursion needed —
/// the jail forbids subdirectories).
fn dir_file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read dir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// SHA-256 — FIPS 180-4 known-answer vectors
// ---------------------------------------------------------------------------

#[test]
fn sha256_fips_180_4_known_vectors() {
    // Empty string — exercises the single padded block path.
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    // "abc" — FIPS 180-4 §B.1.
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // 56-byte multi-block vector — FIPS 180-4 §B.2, straddles the
    // message-length padding boundary.
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // 896-bit vector — FIPS 180-4 §B.3.
    assert_eq!(
        sha256_hex(
            b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
        ),
        "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
    );
    // 1,000,000 × 'a' — the classic long-message vector.
    assert_eq!(
        sha256_hex(&vec![b'a'; 1_000_000]),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[test]
fn sha256_streaming_matches_one_shot_for_every_chunking() {
    let data = jar_bytes(0x11, 10_000);
    let one_shot = sha256(&data);
    for chunk in [1usize, 7, 55, 56, 63, 64, 65, 127, 8192] {
        let mut h = Sha256::new();
        for piece in data.chunks(chunk) {
            h.update(piece);
        }
        assert_eq!(h.finalize(), one_shot, "chunk size {chunk} diverged");
    }
}

#[test]
fn digest_hex_parse_and_consttime_equality() {
    let d = sha256(b"megu");
    let hex = digest_hex(&d);
    assert_eq!(hex.len(), 64);
    assert_eq!(parse_sha256_hex(&hex), Some(d));
    // Uppercase hex parses to the same digest.
    assert_eq!(parse_sha256_hex(&hex.to_ascii_uppercase()), Some(d));
    assert!(digest_eq(&d, &d));
    let mut other = d;
    other[31] ^= 1;
    assert!(!digest_eq(&d, &other));
    // Strict 64-hex only — wrong lengths and non-hex bytes fail closed.
    for bad in [
        "",
        "abc",
        &"0".repeat(63),
        &"0".repeat(65),
        &"g".repeat(64),
        &" ".repeat(64),
    ] {
        assert!(parse_sha256_hex(bad).is_none(), "{bad:?} must not parse");
    }
}

// ---------------------------------------------------------------------------
// MSY1 wire codec
// ---------------------------------------------------------------------------

fn manifest_of(entries: Vec<ModSyncEntry>) -> SyncManifest {
    SyncManifest { entries }
}

#[test]
fn wire_roundtrip_preserves_every_field_and_is_deterministic() {
    let (e1, _b1) = good_entry();
    let b2 = jar_bytes(0x77, 300);
    let e2 = entry(
        "create",
        "create-1.21.1-6.0.6.jar",
        "https://edge.forgecdn.net/files/1/2/create.jar",
        &b2,
    );
    let b3 = jar_bytes(0x03, 64);
    let e3 = entry(
        "ae2",
        "appliedenergistics2-19.0.jar",
        "https://mediafilez.forgecdn.net/files/ae2.jar?token=x%3D",
        &b3,
    );
    let manifest = manifest_of(vec![e1.clone(), e2.clone(), e3.clone()]);

    let body = encode_sync_manifest(&manifest).unwrap();
    assert_eq!(&body[..4], &MSY1_MAGIC);
    // Deterministic: encoding twice yields identical bytes.
    assert_eq!(encode_sync_manifest(&manifest).unwrap(), body);

    let decoded = decode_sync_manifest(&body).unwrap();
    assert_eq!(decoded, manifest);
    assert_eq!(decoded.entries[0], e1);
    assert_eq!(decoded.entries[1], e2);
    assert_eq!(decoded.entries[2], e3);
    // The channel constant stays the documented payload channel.
    assert_eq!(MOD_SYNC_CHANNEL, "megu:mod_sync_v1");
}

#[test]
fn wire_decodes_empty_manifest() {
    let body = encode_sync_manifest(&manifest_of(vec![])).unwrap();
    assert_eq!(body.len(), 6);
    assert_eq!(decode_sync_manifest(&body).unwrap().entries.len(), 0);
}

#[test]
fn wire_rejects_bad_magic_and_short_body() {
    let (e, _) = good_entry();
    let body = encode_sync_manifest(&manifest_of(vec![e])).unwrap();

    let mut bad = body.clone();
    bad[0] = b'X';
    assert!(decode_sync_manifest(&bad).is_err());
    assert!(decode_sync_manifest(&body[..5]).is_err());
    assert!(decode_sync_manifest(b"").is_err());
    assert!(decode_sync_manifest(b"MSY2\x00\x01").is_err());
}

#[test]
fn wire_rejects_oversized_entry_count_before_allocation() {
    // 513 > MAX_SYNC_MODS — rejected on the count alone.
    let mut over = Vec::new();
    over.extend_from_slice(&MSY1_MAGIC);
    over.extend_from_slice(&((MAX_SYNC_MODS as u16) + 1).to_be_bytes());
    let e = decode_sync_manifest(&over).unwrap_err();
    assert_eq!(e.kind(), ModSyncErrorKind::TooManyEntries);

    // A plausible-but-impossible count: 10 entries declared in a body that
    // cannot hold even one minimum-size entry — fails before allocation.
    let mut lying = Vec::new();
    lying.extend_from_slice(&MSY1_MAGIC);
    lying.extend_from_slice(&10u16.to_be_bytes());
    lying.extend_from_slice(&[0u8; 8]);
    assert!(decode_sync_manifest(&lying).is_err());
}

#[test]
fn wire_rejects_truncated_and_trailing_bodies() {
    let (e, _) = good_entry();
    let body = encode_sync_manifest(&manifest_of(vec![e])).unwrap();

    // Every strict prefix is a truncation — all must fail closed.
    for cut in [6, 7, body.len() - 33, body.len() - 1] {
        assert!(
            decode_sync_manifest(&body[..cut]).is_err(),
            "prefix of {cut} bytes must fail"
        );
    }
    // Any trailing byte after the last entry is hostile padding.
    let mut extra = body.clone();
    extra.push(0);
    assert!(decode_sync_manifest(&extra).is_err());
}

#[test]
fn wire_rejects_over_cap_string_lengths_without_allocating() {
    let (e, _) = good_entry();
    let body = encode_sync_manifest(&manifest_of(vec![e])).unwrap();

    // Inflate the mod_id length byte past its 128 cap — the declared length
    // is checked before any allocation or slice.
    let mut inflated = body.clone();
    inflated[6] = 200;
    assert!(decode_sync_manifest(&inflated).is_err());

    // URL length field (u16 BE) beyond MAX_SYNC_URL_LEN.
    let id_len = body[6] as usize;
    let name_len_off = 7 + id_len;
    let name_len = body[name_len_off] as usize;
    let url_len_off = name_len_off + 1 + name_len;
    let mut big_url = body.clone();
    big_url[url_len_off] = 0xFF;
    big_url[url_len_off + 1] = 0xFF;
    assert!(decode_sync_manifest(&big_url).is_err());
}

#[test]
fn wire_rejects_entries_failing_structural_validation() {
    // Hand-build an MSY1 body whose file_name violates the jail — the
    // encoder refuses it, so craft the bytes directly to prove the decoder
    // also fails closed rather than trusting its peer.
    let bad_name = "../evil.jar";
    let mut body = Vec::new();
    body.extend_from_slice(&MSY1_MAGIC);
    body.extend_from_slice(&1u16.to_be_bytes());
    body.push(4);
    body.extend_from_slice(b"evil");
    body.push(bad_name.len() as u8);
    body.extend_from_slice(bad_name.as_bytes());
    let url = "https://cdn.modrinth.com/data/x/evil.jar";
    body.extend_from_slice(&(url.len() as u16).to_be_bytes());
    body.extend_from_slice(url.as_bytes());
    body.extend_from_slice(&10u64.to_be_bytes());
    body.extend_from_slice(&sha256(b"0123456789"));
    let e = decode_sync_manifest(&body).unwrap_err();
    assert_eq!(e.kind(), ModSyncErrorKind::UnsafeFileName);

    // Same construction but with a non-HTTPS URL.
    let mut body = Vec::new();
    body.extend_from_slice(&MSY1_MAGIC);
    body.extend_from_slice(&1u16.to_be_bytes());
    body.push(4);
    body.extend_from_slice(b"evil");
    body.push(8);
    body.extend_from_slice(b"evil.jar");
    let url = "http://cdn.modrinth.com/evil.jar";
    body.extend_from_slice(&(url.len() as u16).to_be_bytes());
    body.extend_from_slice(url.as_bytes());
    body.extend_from_slice(&10u64.to_be_bytes());
    body.extend_from_slice(&sha256(b"0123456789"));
    let e = decode_sync_manifest(&body).unwrap_err();
    assert_eq!(e.kind(), ModSyncErrorKind::UnsafeUrl);
}

#[test]
fn wire_encode_refuses_invalid_entries_and_over_cap_manifests() {
    let (mut e, bytes) = good_entry();
    e.file_name = "evil.exe".to_string();
    assert!(encode_sync_manifest(&manifest_of(vec![e])).is_err());

    let (mut e, _) = good_entry();
    e.download_url = "file:///etc/passwd".to_string();
    assert!(encode_sync_manifest(&manifest_of(vec![e])).is_err());

    let (e, _) = good_entry();
    let too_many = manifest_of(vec![e; MAX_SYNC_MODS + 1]);
    let err = encode_sync_manifest(&too_many).unwrap_err();
    assert_eq!(err.kind(), ModSyncErrorKind::TooManyEntries);

    // Boundary: exactly MAX_SYNC_MODS encodes and decodes.
    let at_cap = manifest_of(
        (0..MAX_SYNC_MODS)
            .map(|i| {
                let mut e = entry(
                    &format!("mod{i}"),
                    &format!("mod{i}.jar"),
                    "https://cdn.modrinth.com/data/x/m.jar",
                    &bytes,
                );
                e.file_name = format!("mod{i}.jar");
                e
            })
            .collect(),
    );
    let body = encode_sync_manifest(&at_cap).unwrap();
    assert_eq!(
        decode_sync_manifest(&body).unwrap().entries.len(),
        MAX_SYNC_MODS
    );
}

// ---------------------------------------------------------------------------
// URL allowlist & SSRF battery
// ---------------------------------------------------------------------------

#[test]
fn trusted_cdn_urls_accepted() {
    let p = policy();
    for url in [
        "https://cdn.modrinth.com/data/P7dR8mSH/versions/abc/create-1.21.1.jar",
        "https://cdn.modrinth.com/data/abc/jei.jar?expires=1&signature=x",
        "https://edge.forgecdn.net/files/5856/123/create-1.21.1-6.0.6.jar",
        "https://mediafilez.forgecdn.net/files/5856/123/create.jar",
        // Scheme/host case-insensitivity is still HTTPS on a trusted host;
        // a trailing DNS root dot names the same host.
        "HTTPS://CDN.Modrinth.COM/data/abc/jei.jar",
        "https://cdn.modrinth.com./data/abc/jei.jar",
        "https://cdn.modrinth.com:443/data/abc/jei.jar",
    ] {
        validate_download_url(url, &p).unwrap_or_else(|e| panic!("{url} rejected: {e}"));
    }
}

#[test]
fn hostile_urls_rejected() {
    let p = policy();
    let hostile = [
        // Wrong / dangerous schemes.
        "http://cdn.modrinth.com/a.jar",
        "file:///etc/passwd",
        "ftp://cdn.modrinth.com/a.jar",
        "javascript:alert(1)",
        "data:text/plain;base64,AAAA",
        "cdn.modrinth.com/a.jar",
        "//cdn.modrinth.com/a.jar",
        "",
        // SSRF: raw IP literals in every accepted spelling.
        "https://127.0.0.1/x.jar",
        "https://127.1/x.jar",
        "https://0x7f000001/x.jar",
        "https://2130706433/x.jar",
        "https://169.254.169.254/latest/meta-data/iam",
        "https://10.0.0.4/x.jar",
        "https://192.168.1.1/x.jar",
        "https://[::1]/x.jar",
        "https://[fd00::1]/x.jar",
        // SSRF: loopback / internal hostnames.
        "https://localhost/x.jar",
        "https://evil.localhost/x.jar",
        "https://printer.local/x.jar",
        "https://nas.internal/x.jar",
        // Untrusted domains, including lookalike suffix tricks.
        "https://evil.example/a.jar",
        "https://cdn.modrinth.com.evil.example/a.jar",
        "https://cdn-modrinth.com/a.jar",
        "https://modrinth.com/a.jar",
        // Credentials / userinfo — both orders are host-spoofing vectors.
        "https://cdn.modrinth.com@evil.example/a.jar",
        "https://user:pass@cdn.modrinth.com/a.jar",
        "https://evil.example@cdn.modrinth.com/a.jar",
        // Non-443 port.
        "https://cdn.modrinth.com:8443/a.jar",
        "https://cdn.modrinth.com:80/a.jar",
        // Empty / malformed authorities and labels.
        "https:///a.jar",
        "https://:443/a.jar",
        "https://cdn..modrinth..com/a.jar",
        "https://-cdn.modrinth.com/a.jar",
        // Control bytes / whitespace / non-ASCII.
        "https://cdn.modrinth.com/a.jar\n",
        "https://cdn.modrinth.com/a jar",
        "https://cdn.mödrinth.com/a.jar",
    ];
    for url in hostile {
        let e = validate_download_url(url, &p)
            .expect_err(&format!("hostile URL {url:?} must be rejected"));
        assert_eq!(e.kind(), ModSyncErrorKind::UnsafeUrl, "{url:?}");
    }
}

#[test]
fn over_cap_url_rejected() {
    let p = policy();
    let url = format!("https://cdn.modrinth.com/{}a.jar", "x".repeat(3000));
    assert_eq!(
        validate_download_url(&url, &p).unwrap_err().kind(),
        ModSyncErrorKind::UnsafeUrl
    );
}

#[test]
fn operator_mirror_hosts_extend_allowlist_safely() {
    let p = SyncPolicy::with_operator_mirrors(&["Mirror.Example.Org"]).unwrap();
    validate_download_url("https://mirror.example.org/mods/x.jar", &p)
        .expect("pinned mirror must be accepted");
    assert!(is_trusted_mod_cdn_host("mirror.example.org", &p));
    // The mirror is pinned exactly — subdomains and lookalikes stay out.
    assert!(!is_trusted_mod_cdn_host("cdn.mirror.example.org", &p));
    assert!(!is_trusted_mod_cdn_host("mirror.example.org.evil.com", &p));

    // Unsafe mirror pins are rejected at policy construction, not silently
    // dropped: schemes, ports, paths, credentials, IPs, internal names.
    for bad in [
        "https://mirror.example.org/",
        "mirror.example.org:8443",
        "mirror.example.org/path",
        "user@mirror.example.org",
        "127.0.0.1",
        "10.1.2.3",
        "localhost",
    ] {
        assert!(
            SyncPolicy::with_operator_mirrors(&[bad]).is_err(),
            "mirror pin {bad:?} must be rejected"
        );
    }
    // A trailing DNS root dot is a valid spelling of the same host.
    assert!(SyncPolicy::with_operator_mirrors(&["mirror.example.org."]).is_ok());
}

// ---------------------------------------------------------------------------
// .jar filename jail
// ---------------------------------------------------------------------------

#[test]
fn hostile_file_names_rejected() {
    let hostile = [
        // Traversal & separators.
        "../evil.jar",
        "sub/mod.jar",
        "sub\\mod.jar",
        "..\\evil.jar",
        "mod..jar",
        "..jar",
        // Non-.jar payloads — every dangerous executable/data suffix.
        "payload.exe",
        "run.sh",
        "x.bat",
        "x.dll",
        "x.so",
        "x.dylib",
        "Exploit.class",
        "x.jar.exe",
        "x.jar.sh",
        "jar",
        ".jar",
        // Shell injection & expansion.
        "${jndi:ldap://x}.jar",
        "$(id).jar",
        "`id`.jar",
        "a;b.jar",
        "a|b.jar",
        "a&b.jar",
        "a>b.jar",
        "a<b.jar",
        "a*b.jar",
        "a?b.jar",
        "a!b.jar",
        "a'b.jar",
        "a\"b.jar",
        "a(b).jar",
        "a{b}.jar",
        "a[b].jar",
        "~.jar",
        "a#b.jar",
        // Hidden dotfiles, ADS/drive colon, NUL & control bytes, non-ASCII.
        ".hidden.jar",
        "x.jar:stream",
        "C:x.jar",
        "mod\u{0}.jar",
        "mod\u{1F}.jar",
        "mod\u{7F}.jar",
        "möd.jar",
        "",
    ];
    for name in hostile {
        let e = validate_jar_file_name(name)
            .expect_err(&format!("hostile file name {name:?} must be rejected"));
        assert_eq!(e.kind(), ModSyncErrorKind::UnsafeFileName, "{name:?}");
    }
}

#[test]
fn legit_jar_file_names_accepted() {
    for name in [
        "create-1.21.1-6.0.6.jar",
        "sodium-neoforge-0.6.13+mc1.21.1.jar",
        "jei-1.21.1-neoforge-19.21.0.247.jar",
        "MOD.JAR",
        "My Mod-1.0.Jar",
        "a,b@c=d.jar",
        "x.jar",
    ] {
        validate_jar_file_name(name).unwrap_or_else(|e| panic!("{name} rejected: {e}"));
    }
}

#[test]
fn entry_validation_enforces_digest_and_size() {
    let (mut e, _) = good_entry();
    e.sha256_hex = "zz".repeat(32);
    assert_eq!(
        validate_sync_entry(&e, &policy()).unwrap_err().kind(),
        ModSyncErrorKind::InvalidEntry
    );
    let (mut e, _) = good_entry();
    e.size_bytes = 0;
    assert_eq!(
        validate_sync_entry(&e, &policy()).unwrap_err().kind(),
        ModSyncErrorKind::InvalidEntry
    );
    let (mut e, _) = good_entry();
    e.size_bytes = MAX_MOD_JAR_BYTES + 1;
    assert_eq!(
        validate_sync_entry(&e, &policy()).unwrap_err().kind(),
        ModSyncErrorKind::InvalidEntry
    );
    let (mut e, _) = good_entry();
    e.mod_id = "bad id!".to_string();
    assert_eq!(
        validate_sync_entry(&e, &policy()).unwrap_err().kind(),
        ModSyncErrorKind::InvalidEntry
    );
}

// ---------------------------------------------------------------------------
// Consent, digest/size gating, atomic install, re-plan verification
// ---------------------------------------------------------------------------

#[test]
fn consent_denied_writes_nothing() {
    let dir = fixture_dir("consent");
    let mods = dir.join("mods");
    let (e, bytes) = good_entry();

    let err =
        install_verified_bytes(&mods, &e, &bytes, &ConsentToken::denied(), &policy()).unwrap_err();
    assert_eq!(err.kind(), ModSyncErrorKind::ConsentRequired);
    // Not even the mods/ directory may be created.
    assert!(!mods.exists());
}

#[test]
fn digest_mismatch_writes_nothing_and_leaves_no_temp() {
    let dir = fixture_dir("digest");
    let mods = dir.join("mods");
    fs::create_dir_all(&mods).unwrap();
    let (mut e, bytes) = good_entry();
    // Corrupt exactly one digest nibble — constant-time compare must fail.
    e.sha256_hex.replace_range(0..1, "f");
    let err = install_verified_bytes(&mods, &e, &bytes, &ConsentToken::approved(), &policy())
        .unwrap_err();
    assert_eq!(err.kind(), ModSyncErrorKind::DigestMismatch);
    assert_eq!(
        dir_file_names(&mods),
        Vec::<String>::new(),
        "no file or temp may remain"
    );
}

#[test]
fn wrong_bytes_and_size_mismatch_write_nothing() {
    let dir = fixture_dir("size");
    let mods = dir.join("mods");
    fs::create_dir_all(&mods).unwrap();
    let (e, bytes) = good_entry();

    // Correct digest field but wrong payload bytes → digest mismatch.
    let wrong = jar_bytes(0x99, e.size_bytes as usize);
    let err = install_verified_bytes(&mods, &e, &wrong, &ConsentToken::approved(), &policy())
        .unwrap_err();
    assert_eq!(err.kind(), ModSyncErrorKind::DigestMismatch);

    // Right prefix, wrong length → size mismatch.
    let short = &bytes[..bytes.len() - 1];
    let err =
        install_verified_bytes(&mods, &e, short, &ConsentToken::approved(), &policy()).unwrap_err();
    assert_eq!(err.kind(), ModSyncErrorKind::SizeMismatch);

    assert_eq!(
        dir_file_names(&mods),
        Vec::<String>::new(),
        "no jar and no staging temp may remain"
    );
}

#[test]
fn approved_install_writes_atomically_and_replans_verified() {
    let dir = fixture_dir("install");
    let mods = dir.join("mods");
    let (e, bytes) = good_entry();

    let installed =
        install_verified_bytes(&mods, &e, &bytes, &ConsentToken::approved(), &policy()).unwrap();
    assert_eq!(installed, mods.join(&e.file_name));
    assert_eq!(fs::read(&installed).unwrap(), bytes);
    // No staging file may linger beside the installed jar.
    assert_eq!(dir_file_names(&mods), vec![e.file_name.clone()]);

    // Re-planning the same manifest verifies on-disk bytes — no download.
    let plan = plan_mod_sync(&manifest_of(vec![e.clone()]), &mods, &policy());
    assert_eq!(plan.already_installed_verified, vec![e.file_name.clone()]);
    assert!(plan.to_download.is_empty());
    assert!(plan.skipped_builtin_perf.is_empty());
    assert!(plan.rejected_entries.is_empty());

    // Idempotent reinstall over the verified file stays byte-exact.
    install_verified_bytes(&mods, &e, &bytes, &ConsentToken::approved(), &policy()).unwrap();
    assert_eq!(fs::read(&installed).unwrap(), bytes);
}

#[test]
fn tampered_on_disk_file_replans_as_download() {
    let dir = fixture_dir("replan");
    let mods = dir.join("mods");
    let (e, bytes) = good_entry();
    install_verified_bytes(&mods, &e, &bytes, &ConsentToken::approved(), &policy()).unwrap();

    // Corrupt the installed jar — the plan must demand a re-download.
    fs::write(
        mods.join(&e.file_name),
        jar_bytes(0xEE, e.size_bytes as usize),
    )
    .unwrap();
    let plan = plan_mod_sync(&manifest_of(vec![e.clone()]), &mods, &policy());
    assert!(plan.already_installed_verified.is_empty());
    assert_eq!(plan.to_download, vec![e]);
}

// ---------------------------------------------------------------------------
// Built-in optimization mod auto-skip
// ---------------------------------------------------------------------------

#[test]
fn superseded_perf_mods_auto_skip_by_id_case_and_filename() {
    let bytes = jar_bytes(0x55, 512);
    // Direct id match — even though the URL/digest are valid, the client
    // must never download a redundant perf mod.
    let sodium = entry(
        "sodium",
        "sodium-neoforge-0.6.13+mc1.21.1.jar",
        "https://cdn.modrinth.com/data/abc/sodium.jar",
        &bytes,
    );
    // Case-insensitive id match.
    let modernfix = entry(
        "ModernFix",
        "modernfix-neoforge-5.19.0.jar",
        "https://cdn.modrinth.com/data/abc/modernfix.jar",
        &bytes,
    );
    // Filename-inferred id when the advertised id lies.
    let sneaky = entry(
        "totally-not-sodium",
        "lithium-neoforge-0.15.0.jar",
        "https://cdn.modrinth.com/data/abc/lithium.jar",
        &bytes,
    );
    // `-`/`_` folding: `embeddium_extra` matches `embeddiumextra`-style ids.
    let emb = entry(
        "embeddium_extra",
        "embeddium-extra-1.0.jar",
        "https://cdn.modrinth.com/data/abc/emb.jar",
        &bytes,
    );
    for e in [&sodium, &modernfix, &sneaky, &emb] {
        assert!(
            is_superseded_sync_entry(e),
            "{} should be superseded",
            e.mod_id
        );
    }

    let (real, _) = good_entry();
    let dir = fixture_dir("superseded");
    let mods = dir.join("mods");
    let plan = plan_mod_sync(
        &manifest_of(vec![
            sodium.clone(),
            modernfix.clone(),
            sneaky.clone(),
            emb.clone(),
            real.clone(),
        ]),
        &mods,
        &policy(),
    );
    assert_eq!(
        plan.skipped_builtin_perf,
        vec![
            "sodium".to_string(),
            "ModernFix".to_string(),
            "totally-not-sodium".to_string(),
            "embeddium_extra".to_string()
        ]
    );
    assert_eq!(plan.to_download, vec![real]);
    assert!(plan.rejected_entries.is_empty());
    assert!(plan.already_installed_verified.is_empty());

    // Belt-and-suspenders: even an approved direct install refuses.
    let err = install_verified_bytes(&mods, &sodium, &bytes, &ConsentToken::approved(), &policy())
        .unwrap_err();
    assert_eq!(err.kind(), ModSyncErrorKind::InvalidEntry);
    assert!(!mods.join(&sodium.file_name).exists());
}

// ---------------------------------------------------------------------------
// Plan partitioning & symlink escape
// ---------------------------------------------------------------------------

#[test]
fn plan_partitions_mixed_manifest_fail_closed() {
    let dir = fixture_dir("partition");
    let mods = dir.join("mods");
    let (real, _) = good_entry();
    let http = entry(
        "evilhttp",
        "evil.jar",
        "http://cdn.modrinth.com/evil.jar",
        &[1, 2, 3],
    );
    let untrusted = entry(
        "evilhost",
        "evilhost.jar",
        "https://evil.example/evil.jar",
        &[1, 2, 3],
    );
    let traversal = entry(
        "trav",
        "../escape.jar",
        "https://cdn.modrinth.com/e.jar",
        &[1, 2, 3],
    );
    let badhash = ModSyncEntry {
        sha256_hex: "not-hex".to_string(),
        ..entry(
            "badhash",
            "badhash.jar",
            "https://cdn.modrinth.com/b.jar",
            &[1],
        )
    };
    let sodium = entry(
        "sodium",
        "sodium-x.jar",
        "https://cdn.modrinth.com/sodium.jar",
        &[1],
    );
    let plan = plan_mod_sync(
        &manifest_of(vec![
            real.clone(),
            http,
            untrusted,
            traversal,
            badhash,
            sodium,
        ]),
        &mods,
        &policy(),
    );
    assert_eq!(plan.to_download, vec![real]);
    assert_eq!(plan.skipped_builtin_perf, vec!["sodium".to_string()]);
    assert_eq!(plan.rejected_entries.len(), 4);
    assert!(plan.already_installed_verified.is_empty());
    // Every manifest entry lands in exactly one bucket.
    let total = plan.to_download.len()
        + plan.skipped_builtin_perf.len()
        + plan.rejected_entries.len()
        + plan.already_installed_verified.len();
    assert_eq!(total, 6);
}

#[test]
fn duplicate_file_names_rejected_after_first() {
    let dir = fixture_dir("dup");
    let (e, _) = good_entry();
    let mut dup = e.clone();
    // Same basename modulo case — collides on case-insensitive filesystems.
    dup.file_name = e.file_name.to_ascii_uppercase();
    let plan = plan_mod_sync(&manifest_of(vec![e, dup]), &dir, &policy());
    assert_eq!(plan.to_download.len(), 1);
    assert_eq!(plan.rejected_entries.len(), 1);
    assert!(plan.rejected_entries[0].reason.contains("duplicate"));
}

#[cfg(unix)]
#[test]
fn symlink_escape_refused_by_plan_and_install() {
    use std::os::unix::fs::symlink;

    let dir = fixture_dir("symlink");
    let mods = dir.join("mods");
    fs::create_dir_all(&mods).unwrap();
    let outside = dir.join("outside.jar");
    fs::write(&outside, b"precious-bytes").unwrap();

    // Plant a symlink inside mods/ pointing outside it.
    let (e, bytes) = good_entry();
    symlink(&outside, mods.join(&e.file_name)).unwrap();

    // The plan refuses to treat it as installable.
    let plan = plan_mod_sync(&manifest_of(vec![e.clone()]), &mods, &policy());
    assert!(plan.to_download.is_empty());
    assert!(plan.already_installed_verified.is_empty());
    assert_eq!(plan.rejected_entries.len(), 1);
    assert!(plan.rejected_entries[0].reason.contains("symlink"));

    // And even a forced, consented install refuses to write through it —
    // the outside target stays byte-identical and the link stays put.
    let err = install_verified_bytes(&mods, &e, &bytes, &ConsentToken::approved(), &policy())
        .unwrap_err();
    assert_eq!(err.kind(), ModSyncErrorKind::SymlinkEscape);
    assert_eq!(fs::read(&outside).unwrap(), b"precious-bytes");
    assert!(fs::symlink_metadata(mods.join(&e.file_name))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn entry_on_untrusted_host_rejected_even_with_valid_digest() {
    // A perfectly well-formed entry — right digest, right size, `.jar`
    // name — is still dead on arrival if the origin is untrusted.
    let bytes = jar_bytes(0x01, 128);
    let e = entry(
        "realmod",
        "realmod-1.0.jar",
        "https://cdn.modrinth.com.attacker.example/realmod.jar",
        &bytes,
    );
    let err = validate_sync_entry(&e, &policy()).unwrap_err();
    assert_eq!(err.kind(), ModSyncErrorKind::UnsafeUrl);
}

// ---------------------------------------------------------------------------
// NET-3c: 32-byte manifest identity and validated diff-only plans
// ---------------------------------------------------------------------------

#[test]
fn root_hash_is_order_independent_and_content_sensitive() {
    let (a, _) = good_entry();
    let b = entry(
        "create",
        "create.jar",
        "https://cdn.modrinth.com/create.jar",
        b"other",
    );
    let first = manifest_of(vec![a.clone(), b.clone()]);
    let backwards = manifest_of(vec![b.clone(), a.clone()]);
    assert_eq!(manifest_root_hash(&first), manifest_root_hash(&backwards));

    let mut changed_url = a.clone();
    changed_url.download_url = "https://edge.forgecdn.net/same.jar".into();
    assert_eq!(
        manifest_root_hash(&first),
        manifest_root_hash(&manifest_of(vec![changed_url, b.clone()]))
    );

    let mut changed_digest = a.clone();
    changed_digest.sha256_hex = sha256_hex(b"different");
    assert_ne!(
        manifest_root_hash(&first),
        manifest_root_hash(&manifest_of(vec![changed_digest, b.clone()]))
    );
    let mut changed_size = a.clone();
    changed_size.size_bytes += 1;
    assert_ne!(
        manifest_root_hash(&first),
        manifest_root_hash(&manifest_of(vec![changed_size, b.clone()]))
    );
    let mut changed_name = a;
    changed_name.file_name = "new-name.jar".into();
    assert_ne!(
        manifest_root_hash(&first),
        manifest_root_hash(&manifest_of(vec![changed_name, b]))
    );
}

#[test]
fn msh1_probe_exact_36_bytes_roundtrip_and_fail_closed() {
    let (a, _) = good_entry();
    let root = manifest_root_hash(&manifest_of(vec![a]));
    let packet = encode_root_hash_probe(&root);
    assert_eq!(packet.len(), 36);
    assert_eq!(&packet[..4], &MSH1_MAGIC);
    assert_eq!(decode_root_hash_probe(&packet).unwrap(), root);
    for invalid in [
        &packet[..0],
        &packet[..4],
        &packet[..35],
        &[packet.as_slice(), &[0]].concat(),
    ] {
        assert_eq!(
            decode_root_hash_probe(invalid).unwrap_err().kind(),
            ModSyncErrorKind::MalformedManifest
        );
    }
    let mut bad_magic = packet;
    bad_magic[0] = b'X';
    assert_eq!(
        decode_root_hash_probe(&bad_magic).unwrap_err().kind(),
        ModSyncErrorKind::MalformedManifest
    );
}

#[test]
fn diff_partitions_add_update_unchanged_remove_and_skips_perf() {
    let unchanged = entry(
        "jei",
        "jei.jar",
        "https://cdn.modrinth.com/jei.jar",
        b"unchanged",
    );
    let old_update = entry(
        "create",
        "old-create.jar",
        "https://cdn.modrinth.com/create.jar",
        b"old",
    );
    let new_update = entry(
        "create",
        "new-create.jar",
        "https://cdn.modrinth.com/create.jar",
        b"new",
    );
    let old_removed = entry(
        "retired",
        "retired.jar",
        "https://cdn.modrinth.com/retired.jar",
        b"retired",
    );
    let added = entry(
        "ae2",
        "ae2.jar",
        "https://cdn.modrinth.com/ae2.jar",
        b"added",
    );
    let sodium = entry(
        "sodium",
        "sodium.jar",
        "https://cdn.modrinth.com/sodium.jar",
        b"skip",
    );
    let local = vec![unchanged.clone(), old_update.clone(), old_removed];
    let manifest = manifest_of(vec![
        new_update.clone(),
        sodium,
        unchanged.clone(),
        added.clone(),
    ]);
    let diff = compute_manifest_diff(&local, &manifest).unwrap();
    assert_eq!(diff.unchanged, vec![unchanged]);
    assert_eq!(diff.added, vec![added]);
    assert_eq!(diff.updated, vec![new_update]);
    assert_eq!(diff.removed, vec!["old-create.jar", "retired.jar"]);
    assert_eq!(diff.skipped_builtin_perf, vec!["sodium"]);
    assert_eq!(local[1], old_update);
}

#[test]
fn malformed_server_manifest_rejected_before_removal_plan() {
    let local = vec![entry(
        "old",
        "old.jar",
        "https://cdn.modrinth.com/old.jar",
        b"old",
    )];
    let valid = entry("new", "new.jar", "https://cdn.modrinth.com/new.jar", b"new");
    let mut hostile = valid.clone();
    hostile.file_name = "../outside.jar".into();
    let mut bad_digest = valid.clone();
    bad_digest.sha256_hex = "not-a-digest".into();
    let mut bad_host = valid.clone();
    bad_host.download_url = "https://attacker.example/mod.jar".into();
    let mut duplicate = valid.clone();
    duplicate.mod_id = "another-id".into();
    let mut duplicate_id = valid.clone();
    duplicate_id.file_name = "another-file.jar".into();

    for bad in [hostile, bad_digest, bad_host, duplicate, duplicate_id] {
        let manifest = manifest_of(vec![valid.clone(), bad]);
        assert!(compute_manifest_diff(&local, &manifest).is_err());
    }
    let huge = manifest_of(vec![valid; MAX_SYNC_MODS + 1]);
    assert_eq!(
        compute_manifest_diff(&local, &huge).unwrap_err().kind(),
        ModSyncErrorKind::TooManyEntries
    );
}

#[test]
fn diff_accepts_only_explicitly_pinned_mirrors() {
    let from_mirror = entry(
        "example",
        "example.jar",
        "https://trusted.example/mod.jar",
        b"blob",
    );
    let manifest = manifest_of(vec![from_mirror.clone()]);
    assert!(compute_manifest_diff(&[], &manifest).is_err());
    let mirror = SyncPolicy::with_operator_mirrors(&["trusted.example"]).unwrap();
    assert_eq!(
        compute_manifest_diff_with_policy(&[], &manifest, &mirror)
            .unwrap()
            .added,
        vec![from_mirror]
    );
}

// ---------------------------------------------------------------------------
// NET-3c: shared content-addressed JAR cache and safe removal
// ---------------------------------------------------------------------------

#[test]
fn cache_stores_once_and_materializes_same_digest_into_two_profiles() {
    let dir = fixture_dir("cache-shared");
    let cache = ContentAddressedModCache::new(dir.join("cache"));
    let bytes = jar_bytes(0x17, 4096);
    let digest = sha256(&bytes);
    let blob = cache.store_verified_bytes(&digest, &bytes).unwrap();
    assert_eq!(
        blob.file_name().unwrap().to_str().unwrap(),
        format!("{}.jar", digest_hex(&digest))
    );
    assert_eq!(cache.store_verified_bytes(&digest, &bytes).unwrap(), blob);
    assert_eq!(dir_file_names(cache.root()).len(), 1);

    let first = cache
        .materialize_into_profile(
            &digest,
            &dir.join("profile-a/mods"),
            "first.jar",
            &ConsentToken::approved(),
        )
        .unwrap();
    let second = materialize_into_profile(
        cache.root(),
        &digest,
        &dir.join("profile-b/mods"),
        "second.jar",
        &ConsentToken::approved(),
    )
    .unwrap();
    assert_eq!(fs::read(&first).unwrap(), bytes);
    assert_eq!(fs::read(&second).unwrap(), bytes);
    assert_eq!(fs::read(&blob).unwrap(), bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&blob).unwrap().ino(),
            fs::metadata(&first).unwrap().ino()
        );
        assert_eq!(
            fs::metadata(&blob).unwrap().ino(),
            fs::metadata(&second).unwrap().ino()
        );
    }
}

#[test]
fn cache_rejects_tampered_bytes_and_never_overwrites_profile() {
    let dir = fixture_dir("cache-tamper");
    let cache = ContentAddressedModCache::new(dir.join("cache"));
    let mods = dir.join("mods");
    fs::create_dir_all(&mods).unwrap();
    let bytes = b"verified jar bytes";
    let digest = sha256(bytes);
    assert_eq!(
        cache
            .store_verified_bytes(&digest, b"wrong")
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::DigestMismatch
    );
    let blob = cache.store_verified_bytes(&digest, bytes).unwrap();
    fs::write(mods.join("existing.jar"), b"leave unchanged").unwrap();
    assert_eq!(
        cache
            .materialize_into_profile(&digest, &mods, "existing.jar", &ConsentToken::denied())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::ConsentRequired
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&blob).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&blob, permissions).unwrap();
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(&blob).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&blob, permissions).unwrap();
    }
    fs::write(&blob, b"tampered").unwrap();
    assert_eq!(
        cache
            .store_verified_bytes(&digest, bytes)
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::DigestMismatch
    );
    assert_eq!(
        cache
            .materialize_into_profile(&digest, &mods, "existing.jar", &ConsentToken::approved())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::DigestMismatch
    );
    assert_eq!(
        fs::read(mods.join("existing.jar")).unwrap(),
        b"leave unchanged"
    );
    assert_eq!(dir_file_names(&mods), vec!["existing.jar"]);
}

#[test]
fn long_jar_basename_keeps_staging_name_under_name_max() {
    let dir = fixture_dir("max-filename");
    let mods = dir.join("mods");
    let name = format!("{}.jar", "a".repeat(251));
    assert_eq!(name.len(), 255);
    validate_jar_file_name(&name).unwrap();
    let bytes = jar_bytes(0x27, 512);
    let digest = sha256(&bytes);
    let cache = ContentAddressedModCache::new(dir.join("cache"));
    cache.store_verified_bytes(&digest, &bytes).unwrap();
    let first = cache
        .materialize_into_profile(&digest, &mods, &name, &ConsentToken::approved())
        .unwrap();
    assert_eq!(fs::read(&first).unwrap(), bytes);
    let e = entry("long", &name, "https://cdn.modrinth.com/long.jar", &bytes);
    install_verified_bytes(&mods, &e, &bytes, &ConsentToken::approved(), &policy()).unwrap();
    assert_eq!(dir_file_names(&mods), vec![name]);
}

#[test]
fn prune_removed_requires_consent_and_prevalidates_every_name() {
    let dir = fixture_dir("prune");
    let mods = dir.join("mods");
    fs::create_dir_all(&mods).unwrap();
    fs::write(mods.join("one.jar"), b"one").unwrap();
    fs::write(mods.join("two.jar"), b"two").unwrap();
    fs::write(mods.join("notes.txt"), b"keep").unwrap();
    let names = vec!["one.jar".into(), "two.jar".into()];
    assert_eq!(
        prune_removed(&mods, &names, &ConsentToken::denied())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::ConsentRequired
    );
    let hostile = vec!["one.jar".into(), "../notes.txt".into()];
    assert_eq!(
        prune_removed(&mods, &hostile, &ConsentToken::approved())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::UnsafeFileName
    );
    assert!(mods.join("one.jar").exists());
    assert_eq!(
        prune_removed(&mods, &names, &ConsentToken::approved()).unwrap(),
        2
    );
    assert_eq!(
        prune_removed(&mods, &names, &ConsentToken::approved()).unwrap(),
        0
    );
    assert_eq!(dir_file_names(&mods), vec!["notes.txt"]);
}

#[cfg(unix)]
#[test]
fn cache_and_prune_refuse_symlinks_without_touching_outside_files() {
    use std::os::unix::fs::symlink;
    let dir = fixture_dir("cache-symlinks");
    let mods = dir.join("mods");
    fs::create_dir_all(&mods).unwrap();
    let outside = dir.join("outside.jar");
    fs::write(&outside, b"outside").unwrap();
    symlink(&outside, mods.join("linked.jar")).unwrap();
    fs::write(mods.join("safe.jar"), b"safe").unwrap();
    let paths = vec!["safe.jar".into(), "linked.jar".into()];
    assert_eq!(
        prune_removed(&mods, &paths, &ConsentToken::approved())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::SymlinkEscape
    );
    assert!(mods.join("safe.jar").exists());

    // A genuine mods directory below a symlinked parent is also unsafe.
    let alias_parent = dir.join("alias-parent");
    symlink(&dir, &alias_parent).unwrap();
    let nested_mods = alias_parent.join("mods");
    assert_eq!(
        prune_removed(
            &nested_mods,
            &["safe.jar".into()],
            &ConsentToken::approved()
        )
        .unwrap_err()
        .kind(),
        ModSyncErrorKind::SymlinkEscape
    );
    assert!(mods.join("safe.jar").exists());

    let digest = sha256(b"blob");
    let cache = ContentAddressedModCache::new(dir.join("cache"));
    fs::create_dir_all(cache.root()).unwrap();
    symlink(
        &outside,
        cache.root().join(format!("{}.jar", digest_hex(&digest))),
    )
    .unwrap();
    assert_eq!(
        cache
            .store_verified_bytes(&digest, b"blob")
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::SymlinkEscape
    );
    assert_eq!(
        cache
            .materialize_into_profile(&digest, &mods, "new.jar", &ConsentToken::approved())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::SymlinkEscape
    );
    assert_eq!(fs::read(&outside).unwrap(), b"outside");

    fs::remove_file(cache.root().join(format!("{}.jar", digest_hex(&digest)))).unwrap();
    cache.store_verified_bytes(&digest, b"blob").unwrap();
    assert_eq!(
        cache
            .materialize_into_profile(&digest, &nested_mods, "new.jar", &ConsentToken::approved())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::SymlinkEscape
    );
    assert_eq!(
        cache
            .materialize_into_profile(&digest, &mods, "linked.jar", &ConsentToken::approved())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::SymlinkEscape
    );
    assert_eq!(fs::read(&outside).unwrap(), b"outside");

    let linked_dir = dir.join("linked-mods");
    symlink(&mods, &linked_dir).unwrap();
    assert_eq!(
        prune_removed(&linked_dir, &["safe.jar".into()], &ConsentToken::approved())
            .unwrap_err()
            .kind(),
        ModSyncErrorKind::SymlinkEscape
    );
    assert!(mods.join("safe.jar").exists());
}
