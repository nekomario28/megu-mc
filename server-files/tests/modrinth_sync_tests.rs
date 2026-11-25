// SPDX-License-Identifier: LGPL-3.0-or-later
//! Compliance coverage for `modrinth_sync`: the Modrinth ToS identity and
//! rate-limit contract, zero-redistribution URL trust, `.mrpack` schema
//! v1 parsing/writing, the fail-closed path + Mojang/secret purity
//! filter, the server-push sync diff, the dependency/conflict resolver
//! and the SHA-256 pre-commit gate.

use megu_server_files::modrinth_sync::{
    crate_user_agent, diff_installed_vs_server, is_compliant_user_agent, project_page_url,
    required_user_agent, resolve_dependencies, validate_download_url, validate_mrpack_path,
    verify_bytes_sha256, verify_file_sha256, DependencyKind, EnvSupport, InstalledMod,
    MissingDependency, ModConflict, ModDependencyEdge, ModNode, ModSyncPlan, ModrinthSearchHit,
    ModrinthSyncError, MrpackIndex, PathRejection, PurityRule, RateLimitState, ResolvedModSet,
    ServerModEntry, ServerModSyncManifest, Sha256Verification, SyncRejectReason, UrlTrustError,
    RATE_LIMIT_MAX_CACHE_ENTRIES,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "megu-modsync-{}-{}-{tag}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(std::fs::canonicalize(&root).unwrap())
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA512: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\
                     cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const SHA1: &str = "dddddddddddddddddddddddddddddddddddddddd";
const CDN: &str = "https://cdn.modrinth.com/data/AAAA1111/versions/x/mod-a.jar";

fn entry(project: &str, version: &str, file: &str, sha256: &str, url: &str) -> ServerModEntry {
    ServerModEntry {
        project_id: project.to_owned(),
        version_id: version.to_owned(),
        file_name: file.to_owned(),
        sha256: sha256.to_owned(),
        sha512: None,
        download_url: url.to_owned(),
        required: true,
        dependencies: Vec::new(),
    }
}

fn manifest(entries: Vec<ServerModEntry>) -> ServerModSyncManifest {
    ServerModSyncManifest {
        format_version: 1,
        server_name: "srv".to_owned(),
        minecraft_version: "1.21.1".to_owned(),
        loader: Some("fabric".to_owned()),
        entries,
    }
}

fn inst_mod(
    file: &str,
    project: Option<&str>,
    sha256: Option<&str>,
    enabled: bool,
) -> InstalledMod {
    InstalledMod {
        file_name: file.to_owned(),
        project_id: project.map(str::to_owned),
        sha256: sha256.map(str::to_owned),
        enabled,
    }
}

trait WithSha512 {
    fn with_sha512(self, s: &str) -> Self;
}
impl WithSha512 for ServerModEntry {
    fn with_sha512(mut self, s: &str) -> Self {
        self.sha512 = Some(s.to_owned());
        self
    }
}

// -------------------------------------------------------------- ToS identity

#[test]
fn required_user_agent_shape_and_compliance() {
    let ua = required_user_agent("1.2.3");
    assert_eq!(
        ua,
        "nekomario28/megu-mc/1.2.3 (https://github.com/nekomario28/megu-mc)"
    );
    assert!(is_compliant_user_agent(&ua));
    assert!(is_compliant_user_agent(&crate_user_agent()));
}

#[test]
fn generic_user_agents_are_rejected() {
    for bad in [
        "curl/8.5.0",
        "reqwest/0.12.9",
        "megu-mc",
        "Java/21",
        "Mozilla/5.0",
        "nekomario28/megu-mc",
        "nekomario28/megu-mc/ (https://github.com/nekomario28/megu-mc)",
        "nekomario28/megu-mc/1.0 (https://github.com/other/repo)",
        "nekomario28/megu-mc/1.0",
        "megu-mc/1.0 (https://github.com/nekomario28/megu-mc)",
        // whitespace/comment injection inside the version token
        "nekomario28/megu-mc/1.0 x (https://github.com/nekomario28/megu-mc)",
        "nekomario28/megu-mc/1.0(bad) (https://github.com/nekomario28/megu-mc)",
    ] {
        assert!(!is_compliant_user_agent(bad), "{bad}");
    }
}

#[test]
fn project_page_url_derives_from_validated_slug() {
    assert_eq!(
        project_page_url("fabric-api").as_deref(),
        Some("https://modrinth.com/mod/fabric-api")
    );
    for bad in ["", "a b", "../x", "sodium/extra", "sodium?x=1"] {
        assert_eq!(project_page_url(bad), None, "{bad}");
    }
}

#[test]
fn search_hit_preserves_attribution() {
    let hit =
        ModrinthSearchHit::new("AAAA1111", "sodium", "Sodium", "jellysquid3", "LGPL-3.0").unwrap();
    assert_eq!(hit.author, "jellysquid3");
    assert_eq!(hit.license_id, "LGPL-3.0");
    assert_eq!(hit.project_url, "https://modrinth.com/mod/sodium");

    // From a wire-shaped search hit: attribution fields are mandatory.
    let json = megu_server_files::json::parse(
        r#"{"project_id":"BBBB2222","slug":"lithium","title":"Lithium","author":"2No2Name","license":"LGPL-3.0"}"#,
    )
    .unwrap();
    let hit = ModrinthSearchHit::from_hit_json(&json).unwrap();
    assert_eq!(hit.author, "2No2Name");
    assert_eq!(hit.license_id, "LGPL-3.0");
    assert_eq!(hit.project_url, "https://modrinth.com/mod/lithium");

    // Missing author or license: an unattributable result is an error.
    for body in [
        r#"{"project_id":"X","slug":"ok","license":"MIT"}"#,
        r#"{"project_id":"X","slug":"ok","author":"a"}"#,
        r#"{"project_id":"X","slug":"bad slug","author":"a","license":"MIT"}"#,
    ] {
        let json = megu_server_files::json::parse(body).unwrap();
        assert!(ModrinthSearchHit::from_hit_json(&json).is_err(), "{body}");
    }
}

// -------------------------------------------------------------- URL trust

#[test]
fn only_official_cdn_urls_are_trusted() {
    assert_eq!(validate_download_url(CDN), Ok(()));

    for (url, reason) in [
        (
            "http://cdn.modrinth.com/data/x/y.jar",
            UrlTrustError::NotHttps,
        ),
        ("cdn.modrinth.com/data/x/y.jar", UrlTrustError::NotHttps),
        ("ftp://cdn.modrinth.com/x", UrlTrustError::NotHttps),
        // Lookalike hosts and authority tricks fail closed.
        (
            "https://cdn.modrinth.com.evil.test/x.jar",
            UrlTrustError::UntrustedHost,
        ),
        (
            "https://cdn.modrinth.com@evil.test/x.jar",
            UrlTrustError::UntrustedHost,
        ),
        (
            "https://api.modrinth.com/v2/x",
            UrlTrustError::UntrustedHost,
        ),
        ("https://modrinth.com/x", UrlTrustError::UntrustedHost),
        ("https://evil.test/x.jar", UrlTrustError::UntrustedHost),
        // Malformed remainders.
        ("https://cdn.modrinth.com/", UrlTrustError::Malformed),
        (
            "https://cdn.modrinth.com/data/../escape",
            UrlTrustError::Malformed,
        ),
        ("https://cdn.modrinth.com/a//b", UrlTrustError::Malformed),
        ("https://cdn.modrinth.com/a b.jar", UrlTrustError::Malformed),
        (
            "https://cdn.modrinth.com/a\\b.jar",
            UrlTrustError::Malformed,
        ),
    ] {
        assert_eq!(validate_download_url(url), Err(reason), "{url}");
    }
}

// -------------------------------------------------------------- rate limit

#[test]
fn rate_limit_headers_gate_requests() {
    let mut rl = RateLimitState::new();
    let now = 2_000_000_000u64;
    assert!(rl.may_request(now));

    // Duration-style reset: remaining=0 blocks until now+reset.
    rl.observe_headers(
        &[
            ("X-Ratelimit-Remaining".to_owned(), "0".to_owned()),
            ("X-Ratelimit-Reset".to_owned(), "120".to_owned()),
        ],
        now,
    );
    assert_eq!(rl.requests_remaining(), Some(0));
    assert!(!rl.may_request(now));
    assert_eq!(rl.retry_after_secs(now), 120);
    assert_eq!(rl.retry_after_secs(now + 119), 1);
    assert!(rl.may_request(now + 120));

    // Epoch-style reset (> now) is honored the same way.
    rl.observe_headers(
        &[
            ("x-ratelimit-remaining".to_owned(), "0".to_owned()),
            ("x-ratelimit-reset".to_owned(), "2000000300".to_owned()),
        ],
        now,
    );
    assert_eq!(rl.retry_after_secs(now), 300);

    // A fresh window re-opens requests.
    rl.observe_headers(
        &[("X-Ratelimit-Remaining".to_owned(), "42".to_owned())],
        now,
    );
    assert!(rl.may_request(now));
}

#[test]
fn rate_limit_backoff_is_exponential_and_capped() {
    let mut rl = RateLimitState::new();
    let now = 1_000u64;
    for expected in [1, 2, 4, 8] {
        rl.record_failure(now);
        assert_eq!(rl.retry_after_secs(now), expected);
        assert!(!rl.may_request(now));
        assert!(rl.backoff_until_unix().is_some());
    }
    // Failures keep growing but the delay caps at 300 s.
    for _ in 0..40 {
        rl.record_failure(now);
    }
    assert_eq!(rl.retry_after_secs(now), 300);
    // A success clears the backoff entirely.
    rl.record_success();
    assert!(rl.may_request(now));
    assert_eq!(rl.backoff_until_unix(), None);
}

#[test]
fn rate_limit_cache_is_bounded_and_ttl_checked() {
    let mut rl = RateLimitState::new();
    let url = "https://api.modrinth.com/v2/search?query=x";
    rl.cache_put(url, "body".to_owned(), 100, 60);
    assert_eq!(rl.cache_get(url, 100), Some("body"));
    assert_eq!(rl.cache_get(url, 159), Some("body"));
    assert_eq!(rl.cache_get(url, 160), None);
    assert_eq!(rl.cache_purge_expired(160), 1);
    assert_eq!(rl.cache_len(), 0);

    // Bounded: the oldest entry is evicted at capacity.
    let mut rl = RateLimitState::new();
    for i in 0..RATE_LIMIT_MAX_CACHE_ENTRIES {
        rl.cache_put(&format!("u{i}"), "b".to_owned(), i as u64, 10_000);
    }
    assert_eq!(rl.cache_len(), RATE_LIMIT_MAX_CACHE_ENTRIES);
    rl.cache_put("newest", "b".to_owned(), 999_999, 10_000);
    assert_eq!(rl.cache_len(), RATE_LIMIT_MAX_CACHE_ENTRIES);
    assert_eq!(rl.cache_get("u0", 999_999), None);
    assert_eq!(rl.cache_get("newest", 999_999), Some("b"));
}

// -------------------------------------------------------------- mrpack index

fn index_body() -> String {
    format!(
        r#"{{
  "formatVersion": 1,
  "game": "minecraft",
  "versionId": "1.0.0",
  "name": "Test Pack",
  "summary": "s",
  "files": [
    {{
      "path": "mods/fabric-api.jar",
      "hashes": {{"sha1": "{SHA1}", "sha512": "{SHA512}"}},
      "env": {{"client": "required", "server": "required"}},
      "downloads": ["{CDN}"],
      "fileSize": 12345
    }},
    {{
      "path": "config/sodium-options.json",
      "hashes": {{"sha1": "{SHA1}", "sha512": "{SHA512}"}},
      "env": {{"client": "optional", "server": "unsupported"}},
      "downloads": ["{CDN}"],
      "fileSize": 42
    }}
  ],
  "dependencies": {{"minecraft": "1.21.1", "fabric-loader": "0.16.5"}}
}}"#
    )
}

#[test]
fn mrpack_index_parses_and_round_trips() {
    let index = MrpackIndex::parse_index(&index_body()).unwrap();
    assert_eq!(index.format_version, 1);
    assert_eq!(index.game, "minecraft");
    assert_eq!(index.version_id, "1.0.0");
    assert_eq!(index.name, "Test Pack");
    assert_eq!(index.summary.as_deref(), Some("s"));
    assert_eq!(index.dependencies["minecraft"], "1.21.1");
    assert_eq!(index.dependencies["fabric-loader"], "0.16.5");
    assert_eq!(index.files.len(), 2);
    assert_eq!(index.files[0].path, "mods/fabric-api.jar");
    assert_eq!(index.files[0].hashes.sha1, SHA1);
    assert_eq!(index.files[0].hashes.sha512, SHA512);
    assert_eq!(
        index.files[0].env,
        Some(megu_server_files::modrinth_sync::MrpackEnv {
            client: EnvSupport::Required,
            server: EnvSupport::Required
        })
    );
    assert_eq!(index.files[1].env.unwrap().server, EnvSupport::Unsupported);
    assert_eq!(index.files[0].downloads, vec![CDN.to_owned()]);
    assert_eq!(index.files[0].file_size, 12345);

    // Writer emits schema-v1 shape; a second parse proves round-trip.
    let reparsed = MrpackIndex::parse_index(&index.to_index_string()).unwrap();
    assert_eq!(reparsed, index);
}

#[test]
fn mrpack_index_fails_closed_on_schema_violations() {
    // formatVersion != 1
    let bad = index_body().replace("\"formatVersion\": 1", "\"formatVersion\": 2");
    assert!(matches!(
        MrpackIndex::parse_index(&bad),
        Err(ModrinthSyncError::UnsupportedFormatVersion(2))
    ));
    // game != minecraft
    let bad = index_body().replace("\"game\": \"minecraft\"", "\"game\": \"terraria\"");
    assert!(matches!(
        MrpackIndex::parse_index(&bad),
        Err(ModrinthSyncError::NotMinecraftGame(_))
    ));
    // untrusted download URL inside files[]
    let bad = index_body().replace(CDN, "https://evil-mirror.test/x.jar");
    assert!(matches!(
        MrpackIndex::parse_index(&bad),
        Err(ModrinthSyncError::UntrustedDownloadUrl { .. })
    ));
    // unsafe path inside files[]
    let bad = index_body().replace("mods/fabric-api.jar", "../evil.jar");
    assert!(matches!(
        MrpackIndex::parse_index(&bad),
        Err(ModrinthSyncError::UnsafePath { .. })
    ));
    // malformed hash
    let bad = index_body().replace(SHA1, "zzzz");
    assert!(matches!(
        MrpackIndex::parse_index(&bad),
        Err(ModrinthSyncError::InvalidHash { field: "sha1", .. })
    ));
}

// -------------------------------------------------------------- path safety

#[test]
fn mrpack_paths_fail_closed() {
    for path in [
        "mods/fabric-api.jar",
        "config/sodium-options.json",
        "overrides/options.txt",
        "overrides/config/x.toml",
        "resourcepacks/pack.zip",
        "shaderpacks/s.zip",
        "datapacks/custom/thing.json",
    ] {
        assert_eq!(validate_mrpack_path(path), Ok(()), "{path}");
    }

    for (path, reason) in [
        ("", PathRejection::Empty),
        ("/etc/passwd", PathRejection::AbsolutePath),
        ("C:\\windows\\x", PathRejection::Backslash),
        ("C:/windows/x", PathRejection::AbsolutePath),
        ("//server/share/x", PathRejection::AbsolutePath),
        ("a/../b.jar", PathRejection::ParentTraversal),
        ("..", PathRejection::ParentTraversal),
        ("a/./b.jar", PathRejection::DotSegment),
        ("a//b.jar", PathRejection::EmptySegment),
        ("a/b.jar/", PathRejection::EmptySegment),
        ("a\0b.jar", PathRejection::NulByte),
        ("a\\b.jar", PathRejection::Backslash),
        // Mojang/secret purity filter, all case-insensitive.
        (
            "overrides/sodium.jar",
            PathRejection::Purity(PurityRule::JarInOverrides),
        ),
        (
            "overrides/deep/nested/Create.JAR",
            PathRejection::Purity(PurityRule::JarInOverrides),
        ),
        (
            "config/config_packets.bin",
            PathRejection::Purity(PurityRule::ConfigPacketsBin),
        ),
        (
            "datapacks/vanilla/data/minecraft/x.json",
            PathRejection::Purity(PurityRule::VanillaDatapack),
        ),
        (
            "world/datapacks/VANILLA/recipes.json",
            PathRejection::Purity(PurityRule::VanillaDatapack),
        ),
        (
            "world_overlay_nether.bin",
            PathRejection::Purity(PurityRule::WorldOverlayBin),
        ),
        (
            "cache/World_Overlay2.bin.gz",
            PathRejection::Purity(PurityRule::WorldOverlayBin),
        ),
        (
            "launcher_accounts.json",
            PathRejection::Purity(PurityRule::LauncherAccountsJson),
        ),
        (
            "config/launcher_accounts.JSON",
            PathRejection::Purity(PurityRule::LauncherAccountsJson),
        ),
        (
            "accounts.json",
            PathRejection::Purity(PurityRule::AccountsJson),
        ),
        (
            "x/accounts.json",
            PathRejection::Purity(PurityRule::AccountsJson),
        ),
        (
            "session.token",
            PathRejection::Purity(PurityRule::TokenFile),
        ),
        (
            "cache/auth.TOKEN",
            PathRejection::Purity(PurityRule::TokenFile),
        ),
    ] {
        assert_eq!(validate_mrpack_path(path), Err(reason), "{path}");
    }
}

// -------------------------------------------------------------- manifest + diff

#[test]
fn manifest_round_trips_json() {
    let mut e = entry("A", "v1", "a.jar", SHA, CDN);
    e.sha512 = Some(SHA512.to_owned());
    e.required = false;
    e.dependencies = vec![
        ModDependencyEdge {
            project_id: "B".to_owned(),
            kind: DependencyKind::Required,
        },
        ModDependencyEdge {
            project_id: "C".to_owned(),
            kind: DependencyKind::Incompatible,
        },
    ];
    let m = manifest(vec![e]);
    let reparsed = ServerModSyncManifest::parse_manifest(&m.to_manifest_string()).unwrap();
    assert_eq!(reparsed, m);

    // Unsupported format version fails closed.
    let bad = m
        .to_manifest_string()
        .replace("\"formatVersion\":1", "\"formatVersion\":9");
    assert!(matches!(
        ServerModSyncManifest::parse_manifest(&bad),
        Err(ModrinthSyncError::UnsupportedFormatVersion(9))
    ));
}

#[test]
fn diff_produces_download_enable_disable_and_consent() {
    let m = manifest(vec![
        entry("A", "v1", "a.jar", SHA, CDN), // already installed+enabled: no-op
        entry("B", "v2", "b.jar", SHA_B, CDN), // installed but disabled: enable
        entry("C", "v1", "c.jar", SHA, CDN), // absent: download
        entry("D", "v2", "d-2.0.jar", SHA_B, CDN), // version change for project D
    ]);
    let installed = vec![
        inst_mod("a.jar", Some("A"), Some(SHA), true),
        inst_mod("b.jar", Some("B"), Some(SHA_B), false),
        inst_mod("d-1.0.jar", Some("D"), Some(SHA), true),
        inst_mod("foreign.jar", Some("ZZZ"), Some(SHA), true),
    ];
    let plan = diff_installed_vs_server(&installed, &m);

    // New C and upgraded D download, in manifest order.
    assert_eq!(
        plan.to_download
            .iter()
            .map(|e| e.file_name.as_str())
            .collect::<Vec<_>>(),
        vec!["c.jar", "d-2.0.jar"]
    );
    assert_eq!(plan.to_enable, vec!["b.jar"]);
    // Old D version plus the foreign extra are disabled, sorted.
    assert_eq!(plan.to_disable, vec!["d-1.0.jar", "foreign.jar"]);
    assert!(plan.untrusted_rejected.is_empty());
    assert!(plan.requires_user_consent);
}

#[test]
fn diff_redownloads_on_hash_mismatch_and_needs_no_consent_when_clean() {
    let m = manifest(vec![entry("A", "v1", "a.jar", SHA_B, CDN)]);
    let installed = vec![inst_mod("a.jar", Some("A"), Some(SHA), true)];
    let plan = diff_installed_vs_server(&installed, &m);
    assert_eq!(plan.to_download.len(), 1);
    assert!(plan.requires_user_consent);

    // Already in sync and enabled: empty plan, no consent gate.
    let m = manifest(vec![entry("A", "v1", "a.jar", SHA_B, CDN)]);
    let installed = vec![inst_mod("a.jar", Some("A"), Some(SHA_B), true)];
    let plan = diff_installed_vs_server(&installed, &m);
    assert_eq!(
        plan,
        ModSyncPlan {
            requires_user_consent: false,
            ..ModSyncPlan::default()
        }
    );
}

#[test]
fn diff_fails_closed_on_untrusted_entries() {
    let m = manifest(vec![
        entry("BAD1", "v1", "evil.jar", SHA, "https://evil.test/e.jar"),
        entry("BAD2", "v1", "evil2.jar", "not-hex", CDN),
        entry("BAD3", "v1", "evil3.jar", SHA, CDN).with_sha512("short"),
        entry("BAD4", "v1", "../escape.jar", SHA, CDN),
        entry("BAD5", "v1", "ok.jar", SHA, "http://cdn.modrinth.com/x"),
        entry("OK", "v1", "ok.jar", SHA, CDN),
    ]);
    let plan = diff_installed_vs_server(&[], &m);

    assert_eq!(
        plan.untrusted_rejected
            .iter()
            .map(|r| r.reason)
            .collect::<Vec<_>>(),
        vec![
            SyncRejectReason::UntrustedDownloadUrl,
            SyncRejectReason::MalformedSha256,
            SyncRejectReason::MalformedSha512,
            SyncRejectReason::UnsafeFileName,
            SyncRejectReason::UntrustedDownloadUrl,
        ]
    );
    // Only the trusted entry may ever reach the download list.
    assert_eq!(plan.to_download.len(), 1);
    assert_eq!(plan.to_download[0].project_id, "OK");
    assert!(plan.requires_user_consent);
}

#[test]
fn diff_determinism_and_untrusted_disable_safety() {
    // An installed mod whose only manifest claimant is untrusted is still
    // disabled — an untrusted entry never protects a foreign jar.
    let m = manifest(vec![entry(
        "A",
        "v1",
        "a.jar",
        SHA,
        "https://evil.test/a.jar",
    )]);
    let installed = vec![inst_mod("a.jar", Some("A"), Some(SHA), true)];
    let plan = diff_installed_vs_server(&installed, &m);
    assert_eq!(plan.to_disable, vec!["a.jar"]);
    assert!(plan.to_download.is_empty());
    assert!(!plan.requires_user_consent);
    assert_eq!(plan.untrusted_rejected.len(), 1);
}

// -------------------------------------------------------------- resolver

fn node(project: &str, deps: &[(&str, DependencyKind)]) -> ModNode {
    ModNode {
        project_id: project.to_owned(),
        version_id: "v".to_owned(),
        dependencies: deps
            .iter()
            .map(|(p, k)| ModDependencyEdge {
                project_id: (*p).to_owned(),
                kind: *k,
            })
            .collect(),
    }
}

#[test]
fn resolver_orders_dependencies_first() {
    let set = resolve_dependencies(&[
        node("app", &[("lib", DependencyKind::Required)]),
        node("lib", &[("core", DependencyKind::Required)]),
        node("core", &[]),
    ]);
    assert_eq!(set.install_order, vec!["core", "lib", "app"]);
    assert!(set.missing_required.is_empty());
    assert!(set.conflicts.is_empty());
    assert!(set.cyclic.is_empty());
}

#[test]
fn resolver_reports_missing_required_ignores_missing_optional() {
    let set = resolve_dependencies(&[
        node(
            "app",
            &[
                ("gone", DependencyKind::Required),
                ("maybe", DependencyKind::Optional),
            ],
        ),
        node("core", &[]),
    ]);
    assert_eq!(
        set.missing_required,
        vec![MissingDependency {
            project_id: "app".to_owned(),
            missing_project_id: "gone".to_owned(),
        }]
    );
    // Orderable nodes still resolve; missing deps do not block them.
    assert_eq!(set.install_order, vec!["app", "core"]);
}

#[test]
fn resolver_detects_incompatible_and_duplicates() {
    let set = resolve_dependencies(&[
        node("sodium", &[("optifine", DependencyKind::Incompatible)]),
        node("optifine", &[]),
        node("sodium", &[]),
    ]);
    assert_eq!(
        set.conflicts,
        vec![ModConflict {
            project_id: "optifine".to_owned(),
            incompatible_with: "sodium".to_owned(),
        }]
    );
    assert_eq!(set.duplicates, vec!["sodium"]);
}

#[test]
fn resolver_embedded_satisfies_required_and_orders_provider_first() {
    let set = resolve_dependencies(&[
        node("app", &[("bundled-lib", DependencyKind::Required)]),
        node("host", &[("bundled-lib", DependencyKind::Embedded)]),
    ]);
    assert!(set.missing_required.is_empty());
    // app's requirement on bundled-lib resolves to the embedder `host`.
    assert_eq!(set.install_order, vec!["host", "app"]);
}

#[test]
fn resolver_reports_cycles_without_hanging() {
    let set = resolve_dependencies(&[
        node("a", &[("b", DependencyKind::Required)]),
        node("b", &[("a", DependencyKind::Required)]),
        node("free", &[]),
    ]);
    assert_eq!(set.install_order, vec!["free"]);
    assert_eq!(set.cyclic, vec!["a", "b"]);
}

#[test]
fn resolver_empty_set_is_a_clean_resolution() {
    assert_eq!(resolve_dependencies(&[]), ResolvedModSet::default());
}

// -------------------------------------------------------------- sha256 gate

#[test]
fn sha256_gate_matches_known_vectors_and_rejects_tampering() {
    // SHA-256("abc") per FIPS 180-4.
    let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(
        verify_bytes_sha256(b"abc", expected).unwrap(),
        Sha256Verification::Match
    );
    match verify_bytes_sha256(b"abd", expected).unwrap() {
        Sha256Verification::Mismatch {
            expected: e,
            actual,
        } => {
            assert_eq!(e, expected);
            assert_ne!(actual, expected);
        }
        other => panic!("expected mismatch, got {other:?}"),
    }
    // Malformed expected hashes are errors, not mismatches.
    assert!(verify_bytes_sha256(b"abc", "xyz").is_err());
    assert!(verify_bytes_sha256(b"abc", &expected[..40]).is_err());
    // Case-insensitive on the expected side.
    assert_eq!(
        verify_bytes_sha256(b"abc", &expected.to_ascii_uppercase()).unwrap(),
        Sha256Verification::Match
    );
}

#[test]
fn sha256_gate_verifies_files_before_commit() {
    let scratch = Scratch::new("sha256");
    let file = scratch.path("download.jar");
    std::fs::write(&file, b"abc").unwrap();
    let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(
        verify_file_sha256(&file, expected).unwrap(),
        Sha256Verification::Match
    );
    std::fs::write(&file, b"abd").unwrap();
    assert!(matches!(
        verify_file_sha256(&file, expected),
        Ok(Sha256Verification::Mismatch { .. })
    ));
    assert!(verify_file_sha256(&scratch.path("missing.jar"), expected).is_err());
}

// -------------------------------------------------------------- misc errors

#[test]
fn untrusted_and_path_errors_display() {
    let err = ModrinthSyncError::UntrustedDownloadUrl {
        url: "https://evil.test/x".to_owned(),
        reason: UrlTrustError::UntrustedHost,
    };
    assert!(err.to_string().contains("untrusted download url"));
    let err = ModrinthSyncError::UnsafePath {
        path: "../x".to_owned(),
        reason: PathRejection::ParentTraversal,
    };
    assert!(err.to_string().contains("unsafe path"));
}
