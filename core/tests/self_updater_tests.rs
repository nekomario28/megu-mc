// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unit, integration and end-to-end tests for `megu_core::self_updater`.

use std::path::{Path, PathBuf};

use megu_core::crypto::sha256_hex;
use megu_core::hash::crc32_ieee;
use megu_core::self_updater::{
    cleanup_stale_files, expected_sha256, extract_tar_gz_member, failed_path, is_sha256_hex,
    is_trusted_download_url, launcher_mc_version, mc_version_compatible, parse_release_list,
    parse_sha256sums, previous_path, rollback, rollback_launcher_sha256_pin, select_update,
    staged_path, sync_launcher_sha256_pin, verify_bytes_sha256, verify_file_sha256,
    AtomicBinarySwap, AtomicBundleSwap, PinSyncOutcome, ReleaseArch, ReleaseOs, SemVer,
    TargetPlatform, UpdateChannel, UpdateComponent, UpdateError, MAX_MEMBER_BYTES, MAX_TAR_BYTES,
};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "megu-self-updater-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `aaa…` / `bbb…` style deterministic digests for fixture construction.
fn fixed_sha(byte: u8) -> String {
    std::iter::repeat_n(char::from_digit(byte as u32, 16).unwrap(), 64).collect()
}

const OLD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A GitHub Releases API asset object as it appears in the wild.
fn asset_json(tag: &str, name: &str) -> String {
    format!(
        "{{\"name\":\"{name}\",\"browser_download_url\":\
         \"https://github.com/nekomario28/megu-mc/releases/download/{tag}/{name}\"}}"
    )
}

fn release_json(tag: &str, draft: bool, prerelease: bool, assets: &[&str]) -> String {
    let assets = assets
        .iter()
        .map(|name| asset_json(tag, name))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"tag_name\":\"{tag}\",\"draft\":{draft},\"prerelease\":{prerelease},\
         \"published_at\":\"2026-11-01T00:00:00Z\",\"assets\":[{assets}]}}"
    )
}

fn platform(os: ReleaseOs, arch: ReleaseArch, musl: bool) -> TargetPlatform {
    TargetPlatform {
        os,
        arch,
        prefer_musl: musl,
    }
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn read_string(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

// ---------------------------------------------------------------------------
// SemVer
// ---------------------------------------------------------------------------

#[test]
fn semver_orders_prereleases_per_semver2() {
    let ordered = [
        "0.1.0-alpha.3",
        "0.1.0-alpha.4",
        "0.1.0-alpha.10",
        "0.1.0-beta.1",
        "0.1.0-rc.1",
        "0.1.0",
        "0.2.0-alpha.1",
        "0.2.0",
    ];
    let versions: Vec<SemVer> = ordered.iter().map(|s| SemVer::parse(s).unwrap()).collect();
    for pair in versions.windows(2) {
        assert!(pair[0] < pair[1], "{} !< {}", pair[0], pair[1]);
    }
    // Numeric identifiers compare numerically, not lexically.
    assert!(SemVer::parse("0.1.0-alpha.10").unwrap() > SemVer::parse("0.1.0-alpha.9").unwrap());
    // A release outranks all of its own prereleases.
    assert!(SemVer::parse("0.1.0").unwrap() > SemVer::parse("0.1.0-rc.99").unwrap());
    // Numeric identifiers sort below alphanumeric ones.
    assert!(SemVer::parse("0.1.0-1").unwrap() < SemVer::parse("0.1.0-alpha").unwrap());
    // Longer prerelease list wins over its strict prefix.
    assert!(SemVer::parse("0.1.0-alpha.1.1").unwrap() > SemVer::parse("0.1.0-alpha.1").unwrap());
    // Cross-major/minor ordering.
    assert!(SemVer::parse("1.0.0-alpha.1").unwrap() > SemVer::parse("0.99.99").unwrap());
}

#[test]
fn semver_parses_v_prefix_and_build_metadata() {
    let v = SemVer::parse("v0.1.0-alpha.3").unwrap();
    assert_eq!(v, SemVer::parse("0.1.0-alpha.3").unwrap());
    assert!(v.is_prerelease());
    assert_eq!(v.to_string(), "0.1.0-alpha.3");
    let built = SemVer::parse("0.1.0-rc.1+build.42").unwrap();
    assert_eq!(built, SemVer::parse("0.1.0-rc.1").unwrap());
    assert!(!SemVer::parse("0.1.0").unwrap().is_prerelease());
    assert_eq!(SemVer::parse("v1.21.4").unwrap().to_string(), "1.21.4");
}

#[test]
fn semver_rejects_malformed_input() {
    for bad in [
        "",
        "v",
        "0.1",
        "0.1.0.0",
        "0.1.0-",
        "0.1.0-alpha..1",
        "0.1.0-alpha_1",
        "0.1.0+build..x",
        "1.02.3",
        "01.2.3",
        "0.1.0-01",
        "x.1.0",
        "0.1.x",
        "0.1.0-alpha.18446744073709551616",
        "vv1.0.0",
    ] {
        assert!(
            matches!(SemVer::parse(bad), Err(UpdateError::MalformedSemVer(_))),
            "{bad:?} must be rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// update channel filtering
// ---------------------------------------------------------------------------

#[test]
fn stable_channel_skips_prereleases() {
    let current = SemVer::parse("0.1.0").unwrap();
    let releases = parse_release_list(&format!(
        "[{},{}]",
        release_json(
            "v0.1.1",
            false,
            false,
            &["megu-v0.1.1-linux-x86_64.tar.gz", "SHA256SUMS-all.txt",]
        ),
        release_json(
            "v0.2.0-alpha.1",
            false,
            true,
            &[
                "megu-v0.2.0-alpha.1-linux-x86_64.tar.gz",
                "SHA256SUMS-all.txt",
            ]
        ),
    ))
    .unwrap();
    let linux = platform(ReleaseOs::Linux, ReleaseArch::X86_64, false);
    let plan = select_update(
        &releases,
        &current,
        UpdateChannel::Stable,
        linux,
        UpdateComponent::Server,
    )
    .unwrap()
    .expect("a stable update must be offered");
    assert_eq!(plan.tag, "v0.1.1");
}

#[test]
fn prerelease_channel_picks_newest_alpha() {
    let current = SemVer::parse("0.1.0-alpha.3").unwrap();
    let releases = parse_release_list(&format!(
        "[{},{},{}]",
        release_json(
            "v0.1.0-alpha.4",
            false,
            true,
            &[
                "megu-v0.1.0-alpha.4-linux-x86_64.tar.gz",
                "SHA256SUMS-all.txt",
            ]
        ),
        // alpha.10 must beat alpha.4 (numeric, not lexical, ordering).
        release_json(
            "v0.1.0-alpha.10",
            false,
            true,
            &[
                "megu-v0.1.0-alpha.10-linux-x86_64.tar.gz",
                "SHA256SUMS-all.txt",
            ]
        ),
        release_json(
            "v0.1.0-alpha.2",
            false,
            true,
            &[
                "megu-v0.1.0-alpha.2-linux-x86_64.tar.gz",
                "SHA256SUMS-all.txt",
            ]
        ),
    ))
    .unwrap();
    let linux = platform(ReleaseOs::Linux, ReleaseArch::X86_64, false);
    let plan = select_update(
        &releases,
        &current,
        UpdateChannel::Prerelease,
        linux,
        UpdateComponent::Server,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.tag, "v0.1.0-alpha.10");
}

#[test]
fn stable_channel_on_prerelease_host_tracks_prereleases() {
    // Users already on an alpha are on the alpha train: Stable still offers
    // the newer prerelease AND the final stable, picking the newest.
    let current = SemVer::parse("0.1.0-alpha.3").unwrap();
    let releases = parse_release_list(&format!(
        "[{},{}]",
        release_json(
            "v0.1.0-alpha.4",
            false,
            true,
            &[
                "megu-v0.1.0-alpha.4-linux-x86_64.tar.gz",
                "SHA256SUMS-all.txt",
            ]
        ),
        release_json(
            "v0.1.0",
            false,
            false,
            &["megu-v0.1.0-linux-x86_64.tar.gz", "SHA256SUMS-all.txt",]
        ),
    ))
    .unwrap();
    let linux = platform(ReleaseOs::Linux, ReleaseArch::X86_64, false);
    let plan = select_update(
        &releases,
        &current,
        UpdateChannel::Stable,
        linux,
        UpdateComponent::Server,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.tag, "v0.1.0");
}

#[test]
fn drafts_and_older_releases_are_never_selected() {
    let current = SemVer::parse("0.1.0-alpha.3").unwrap();
    let releases = parse_release_list(&format!(
        "[{},{}]",
        // A draft at a much higher version must never win.
        release_json(
            "v9.9.9",
            true,
            false,
            &["megu-v9.9.9-linux-x86_64.tar.gz", "SHA256SUMS-all.txt",]
        ),
        release_json(
            "v0.1.0-alpha.2",
            false,
            true,
            &[
                "megu-v0.1.0-alpha.2-linux-x86_64.tar.gz",
                "SHA256SUMS-all.txt",
            ]
        ),
    ))
    .unwrap();
    let linux = platform(ReleaseOs::Linux, ReleaseArch::X86_64, false);
    assert!(select_update(
        &releases,
        &current,
        UpdateChannel::Prerelease,
        linux,
        UpdateComponent::Server,
    )
    .unwrap()
    .is_none());
}

// ---------------------------------------------------------------------------
// release JSON parsing, asset mapping, URL allowlist
// ---------------------------------------------------------------------------

fn full_asset_release(tag: &str) -> String {
    release_json(
        tag,
        false,
        tag.contains('-'),
        &[
            "megu-vTAG-linux-x86_64.tar.gz",
            "megu-vTAG-linux-x86_64-musl.tar.gz",
            "megu-vTAG-linux-arm64.tar.gz",
            "megu-vTAG-macos-arm64.tar.gz",
            "megu-vTAG-windows-x86_64.tar.gz",
            "megu-installer-windows-x86_64.exe",
            "megu-vTAG-release-manifest.json",
            "SHA256SUMS-all.txt",
        ],
    )
    .replace("vTAG", tag)
}

#[test]
fn select_update_maps_real_release_asset_names() {
    let releases =
        parse_release_list(&format!("[{}]", full_asset_release("v0.1.0-alpha.4"))).unwrap();
    let current = SemVer::parse("0.1.0-alpha.3").unwrap();

    let linux = platform(ReleaseOs::Linux, ReleaseArch::X86_64, false);
    let plan = select_update(
        &releases,
        &current,
        UpdateChannel::Prerelease,
        linux,
        UpdateComponent::Server,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.asset_name, "megu-v0.1.0-alpha.4-linux-x86_64.tar.gz");
    assert_eq!(
        plan.asset_url,
        "https://github.com/nekomario28/megu-mc/releases/download/\
         v0.1.0-alpha.4/megu-v0.1.0-alpha.4-linux-x86_64.tar.gz"
    );
    assert_eq!(plan.checksums_name, "SHA256SUMS-all.txt");
    assert_eq!(plan.archive_member, "megu-v0.1.0-alpha.4-linux-x86_64/megu");
    assert_eq!(plan.binary_name, "megu");
    assert_eq!(
        plan.manifest_name.as_deref(),
        Some("megu-v0.1.0-alpha.4-release-manifest.json")
    );
    assert_eq!(
        plan.summary(),
        "v0.1.0-alpha.3 -> v0.1.0-alpha.4 (linux-x86_64, megu)"
    );
}

#[test]
fn select_update_covers_every_release_target() {
    let tag = "v0.1.0-alpha.4";
    let assets = [
        "megu-v0.1.0-alpha.4-linux-x86_64.tar.gz",
        "megu-v0.1.0-alpha.4-linux-x86_64-musl.tar.gz",
        "megu-v0.1.0-alpha.4-linux-arm64.tar.gz",
        "megu-v0.1.0-alpha.4-linux-arm64-musl.tar.gz",
        "megu-v0.1.0-alpha.4-linux-x86_64-musl-client.tar.gz",
        "megu-v0.1.0-alpha.4-linux-arm64-musl-client.tar.gz",
        "megu-v0.1.0-alpha.4-freebsd-x86_64.tar.gz",
        "megu-v0.1.0-alpha.4-linux-riscv64.tar.gz",
        "megu-v0.1.0-alpha.4-macos-x86_64.tar.gz",
        "megu-v0.1.0-alpha.4-macos-arm64.tar.gz",
        "megu-v0.1.0-alpha.4-windows-x86_64.tar.gz",
        "megu-v0.1.0-alpha.4-windows-arm64.tar.gz",
        "megu-installer-linux-x86_64",
        "megu-installer-linux-x86_64-musl",
        "megu-installer-freebsd-x86_64",
        "megu-installer-linux-riscv64",
        "megu-installer-windows-arm64.exe",
        "SHA256SUMS-all.txt",
    ];
    let releases =
        parse_release_list(&format!("[{}]", release_json(tag, false, true, &assets))).unwrap();
    let current = SemVer::parse("0.1.0-alpha.3").unwrap();
    let cases = [
        (
            (ReleaseOs::Linux, ReleaseArch::X86_64, false),
            "linux-x86_64",
        ),
        (
            (ReleaseOs::Linux, ReleaseArch::X86_64, true),
            "linux-x86_64-musl",
        ),
        ((ReleaseOs::Linux, ReleaseArch::Arm64, false), "linux-arm64"),
        (
            (ReleaseOs::Linux, ReleaseArch::Arm64, true),
            "linux-arm64-musl",
        ),
        (
            (ReleaseOs::Macos, ReleaseArch::X86_64, false),
            "macos-x86_64",
        ),
        ((ReleaseOs::Macos, ReleaseArch::Arm64, false), "macos-arm64"),
        (
            (ReleaseOs::Windows, ReleaseArch::X86_64, false),
            "windows-x86_64",
        ),
        (
            (ReleaseOs::Windows, ReleaseArch::Arm64, false),
            "windows-arm64",
        ),
    ];
    for ((os, arch, musl), target) in cases {
        let plan = select_update(
            &releases,
            &current,
            UpdateChannel::Prerelease,
            platform(os, arch, musl),
            UpdateComponent::Server,
        )
        .unwrap()
        .unwrap_or_else(|| panic!("no plan for {target}"));
        assert_eq!(plan.release_target, target);
        assert_eq!(plan.asset_name, format!("megu-{tag}-{target}.tar.gz"));
    }
    // Windows client binary carries the .exe suffix inside the archive.
    let win = select_update(
        &releases,
        &current,
        UpdateChannel::Prerelease,
        platform(ReleaseOs::Windows, ReleaseArch::X86_64, false),
        UpdateComponent::Client,
    )
    .unwrap()
    .unwrap();
    assert_eq!(win.binary_name, "megu-client-gpu.exe");
    assert_eq!(
        win.archive_member,
        "megu-v0.1.0-alpha.4-windows-x86_64/megu-client-gpu.exe"
    );
    // The standalone installer is a raw download, not an archive member.
    let inst = select_update(
        &releases,
        &current,
        UpdateChannel::Prerelease,
        platform(ReleaseOs::Windows, ReleaseArch::Arm64, false),
        UpdateComponent::Installer,
    )
    .unwrap()
    .unwrap();
    assert_eq!(inst.asset_name, "megu-installer-windows-arm64.exe");
    assert_eq!(inst.archive_member, "");
    // musl clients now use the dedicated dynamic-musl GPU archives, never
    // the static server archive or a glibc fallback.
    let musl_client = select_update(
        &releases,
        &current,
        UpdateChannel::Prerelease,
        platform(ReleaseOs::Linux, ReleaseArch::X86_64, true),
        UpdateComponent::Client,
    )
    .unwrap()
    .unwrap();
    assert_eq!(musl_client.release_target, "linux-x86_64-musl-client");
    assert_eq!(
        musl_client.archive_member,
        "megu-v0.1.0-alpha.4-linux-x86_64-musl-client/megu-client-gpu"
    );

    // FreeBSD and RISC-V publish server/installer artifacts, but no GPU
    // client; unsupported client selection fails closed.
    for (os, arch, target) in [
        (ReleaseOs::FreeBsd, ReleaseArch::X86_64, "freebsd-x86_64"),
        (ReleaseOs::Linux, ReleaseArch::Riscv64, "linux-riscv64"),
    ] {
        let server = select_update(
            &releases,
            &current,
            UpdateChannel::Prerelease,
            platform(os, arch, false),
            UpdateComponent::Server,
        )
        .unwrap()
        .unwrap_or_else(|| panic!("no server plan for {target}"));
        assert_eq!(server.release_target, target);
        assert!(select_update(
            &releases,
            &current,
            UpdateChannel::Prerelease,
            platform(os, arch, false),
            UpdateComponent::Client,
        )
        .unwrap()
        .is_none());
    }
    // RISC-V musl is not one of the published lanes (the matrix ships
    // riscv64gc-unknown-linux-gnu), so it must not receive the glibc artifact.
    assert!(select_update(
        &releases,
        &current,
        UpdateChannel::Prerelease,
        platform(ReleaseOs::Linux, ReleaseArch::Riscv64, true),
        UpdateComponent::Server,
    )
    .unwrap()
    .is_none());
}

#[test]
fn non_semver_and_draftless_entries_parse_safely() {
    let releases = parse_release_list(
        "[{\"tag_name\":\"nightly-2026\",\"assets\":[]},\
          {\"tag_name\":\"v0.1.0-alpha.1\",\"assets\":[]}]",
    )
    .unwrap();
    // The non-SemVer tag is dropped entirely — it can never be newer.
    assert_eq!(releases.len(), 1);
    assert!(releases[0].draft, "missing draft flag defaults to true");
    // `latest` style objects are tolerated; a non-array top level is not.
    assert!(parse_release_list("{\"a\":1}").is_err());
    assert!(parse_release_list("not json").is_err());
}

#[test]
fn untrusted_download_urls_fail_closed() {
    for url in [
        "http://github.com/nekomario28/megu-mc/releases/download/v1/x",
        "https://evil.example.com/megu.tar.gz",
        "https://github.com/nekomario28/megu-mc.evil/releases/download/v1/x",
        "https://api.github.com.evil.com/repos/nekomario28/megu-mc/x",
        "https://github.com/nekomario28/megu-mc/releases/download/",
        "https://github.com:443@evil.com/nekomario28/megu-mc/releases/download/v1/x",
        "",
        "https://objects.githubusercontent.com/x y",
    ] {
        assert!(!is_trusted_download_url(url), "{url} must be refused");
    }
    for url in [
        "https://api.github.com/repos/nekomario28/megu-mc/releases/123",
        "https://github.com/nekomario28/megu-mc/releases/download/v0.1.0/SHA256SUMS-all.txt",
        "https://objects.githubusercontent.com/github-production-release-asset/x?sig=abc",
    ] {
        assert!(is_trusted_download_url(url), "{url} must be trusted");
    }
    // A release whose only asset URL is off-allowlist is refused, not used.
    let current = SemVer::parse("0.1.0-alpha.3").unwrap();
    let bad = "[{\"tag_name\":\"v0.1.0-alpha.4\",\"draft\":false,\
        \"prerelease\":true,\"assets\":[\
        {\"name\":\"megu-v0.1.0-alpha.4-linux-x86_64.tar.gz\",\"browser_download_url\":\"https://evil.example.com/x\"},\
        {\"name\":\"SHA256SUMS-all.txt\",\"browser_download_url\":\"https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/SHA256SUMS-all.txt\"}]}]";
    let releases = parse_release_list(bad).unwrap();
    let err = select_update(
        &releases,
        &current,
        UpdateChannel::Prerelease,
        platform(ReleaseOs::Linux, ReleaseArch::X86_64, false),
        UpdateComponent::Server,
    );
    assert!(matches!(err, Err(UpdateError::UntrustedUrl(_))));
}

// ---------------------------------------------------------------------------
// SHA256SUMS
// ---------------------------------------------------------------------------

#[test]
fn sha256sums_parses_text_and_binary_modes() {
    let text = format!(
        "{a}  megu-v1.0.0-linux-x86_64.tar.gz\n{b} *megu-installer-windows-x86_64.exe\n",
        a = fixed_sha(0xa),
        b = fixed_sha(0xb),
    );
    let sums = parse_sha256sums(&text).unwrap();
    assert_eq!(sums.len(), 2);
    assert_eq!(
        expected_sha256(&sums, "megu-v1.0.0-linux-x86_64.tar.gz").unwrap(),
        fixed_sha(0xa)
    );
    assert_eq!(
        expected_sha256(&sums, "megu-installer-windows-x86_64.exe").unwrap(),
        fixed_sha(0xb)
    );
    assert!(matches!(
        expected_sha256(&sums, "missing.tar.gz"),
        Err(UpdateError::MissingChecksumEntry(_))
    ));
}

#[test]
fn sha256sums_rejects_malformed_lines() {
    let good = format!("{}  file.bin\n", fixed_sha(0xc));
    for bad in [
        "not-hex  file.bin",
        "abcd  file.bin",
        &format!("{} file.bin", fixed_sha(0xc)),
        &format!("{}   ", fixed_sha(0xc)),
        &format!("{}ZZ file.bin", &fixed_sha(0xc)[..62]),
        &format!("{0}  a.bin\n{0}  a.bin", fixed_sha(0xc)),
        &format!("{}  spaced name.bin", fixed_sha(0xc)),
    ] {
        assert!(
            matches!(
                parse_sha256sums(bad),
                Err(UpdateError::MalformedSums { .. })
            ),
            "{bad:?} must be rejected"
        );
    }
    // A good file with blank lines still parses.
    assert_eq!(parse_sha256sums(&format!("\n{good}\n")).unwrap().len(), 1);
}

#[test]
fn checksum_verification_rejects_single_bit_corruption() {
    let payload = b"megu release binary v2";
    let sums = format!("{}  megu\n", sha256_hex(payload));
    verify_bytes_sha256(payload, &sums, "megu").unwrap();
    let mut corrupted = payload.to_vec();
    corrupted[0] ^= 0x01;
    assert!(matches!(
        verify_bytes_sha256(&corrupted, &sums, "megu"),
        Err(UpdateError::ChecksumMismatch { .. })
    ));
    assert!(matches!(
        verify_bytes_sha256(&[], &sums, "megu"),
        Err(UpdateError::EmptyPayload(_))
    ));
    // On-disk variant: matching passes, zero-length and mismatch fail.
    let dir = TestDir::new("sha-file");
    let file = dir.path("megu");
    std::fs::write(&file, payload).unwrap();
    verify_file_sha256(&file, &sums, "megu").unwrap();
    std::fs::write(&file, corrupted).unwrap();
    assert!(matches!(
        verify_file_sha256(&file, &sums, "megu"),
        Err(UpdateError::ChecksumMismatch { .. })
    ));
    std::fs::write(&file, b"").unwrap();
    assert!(matches!(
        verify_file_sha256(&file, &sums, "megu"),
        Err(UpdateError::EmptyPayload(_))
    ));
}

// ---------------------------------------------------------------------------
// tar.gz member extraction (hand-built stored-deflate gzip + USTAR)
// ---------------------------------------------------------------------------

fn ustar_header(name: &str, size: usize, mode: u32, typeflag: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    let put_octal = |field: &mut [u8], value: u64| {
        let text = format!("{:0>width$o}", value, width = field.len() - 1);
        field[..text.len()].copy_from_slice(text.as_bytes());
    };
    put_octal(&mut h[100..108], u64::from(mode));
    put_octal(&mut h[124..136], size as u64);
    h[156] = typeflag;
    h[257..262].copy_from_slice(b"ustar");
    h[263..265].copy_from_slice(b"00");
    // POSIX checksum: field treated as eight spaces.
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    let text = format!("{:06o}\0 ", sum);
    h[148..156].copy_from_slice(text.as_bytes());
    h
}

fn make_tar_gz(member_name: &str, member: &[u8]) -> Vec<u8> {
    let mut tar = Vec::new();
    tar.extend_from_slice(&ustar_header(member_name, member.len(), 0o755, b'0'));
    tar.extend_from_slice(member);
    tar.resize(tar.len().next_multiple_of(512), 0);
    tar.extend_from_slice(&[0u8; 1024]);
    let mut gz = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0, 0xff];
    // One stored deflate block (payload stays well under 65535).
    assert!(tar.len() <= 65535);
    gz.push(0x01);
    let len = tar.len() as u16;
    gz.extend_from_slice(&len.to_le_bytes());
    gz.extend_from_slice(&(!len).to_le_bytes());
    gz.extend_from_slice(&tar);
    gz.extend_from_slice(&crc32_ieee(&tar).to_le_bytes());
    gz.extend_from_slice(&(tar.len() as u32).to_le_bytes());
    gz
}

#[test]
fn extract_tar_gz_member_round_trips_real_layout() {
    let member_path = "megu-v0.1.0-alpha.4-linux-x86_64/megu";
    let binary = b"\x7fELF fake megu v2 binary";
    let gz = make_tar_gz(member_path, binary);
    let out = extract_tar_gz_member(&gz, member_path, MAX_MEMBER_BYTES, MAX_TAR_BYTES).unwrap();
    assert_eq!(out, binary);
    assert!(extract_tar_gz_member(
        &gz,
        "megu-v0.1.0-alpha.4-linux-x86_64/megu-client",
        MAX_MEMBER_BYTES,
        MAX_TAR_BYTES
    )
    .is_err());
    // Truncated / corrupt archives fail closed.
    assert!(extract_tar_gz_member(
        &gz[..gz.len() - 4],
        member_path,
        MAX_MEMBER_BYTES,
        MAX_TAR_BYTES
    )
    .is_err());
    let mut bad_crc = gz.clone();
    let n = bad_crc.len();
    bad_crc[n - 8] ^= 0x01;
    assert!(extract_tar_gz_member(&bad_crc, member_path, MAX_MEMBER_BYTES, MAX_TAR_BYTES).is_err());
    // A smuggled second gzip member after the trailer is refused.
    let mut extra = gz.clone();
    extra.extend_from_slice(&gz);
    assert!(extract_tar_gz_member(&extra, member_path, MAX_MEMBER_BYTES, MAX_TAR_BYTES).is_err());
}

// ---------------------------------------------------------------------------
// atomic binary swap & rollback
// ---------------------------------------------------------------------------

#[test]
fn atomic_swap_installs_verified_binary_and_rolls_back() {
    let dir = TestDir::new("swap");
    let bin = dir.path("megu");
    let v1 = b"megu v1 binary";
    let v2 = b"megu v2 binary with more bytes";
    std::fs::write(&bin, v1).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    AtomicBinarySwap::install(&bin, v2, &sha256_hex(v2)).unwrap();
    assert_eq!(read(&bin), v2);
    assert_eq!(read(&previous_path(&bin)), v1);
    assert!(!staged_path(&bin).exists());
    assert!(AtomicBinarySwap::has_rollback(&bin));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o111, 0o111, "installed binary must stay executable");
        assert_eq!(mode, 0o755);
    }

    rollback(&bin).unwrap();
    assert_eq!(read(&bin), v1, "rollback must restore v1");
    assert_eq!(
        read(&failed_path(&bin)),
        v2,
        "failed binary kept for diagnosis"
    );
    assert!(!previous_path(&bin).exists());
    assert!(!AtomicBinarySwap::has_rollback(&bin));
    assert!(matches!(
        rollback(&bin),
        Err(UpdateError::NoPreviousVersion(_))
    ));

    cleanup_stale_files(&bin).unwrap();
    assert!(!failed_path(&bin).exists());
    assert_eq!(read(&bin), v1);
}

#[test]
fn failed_checksum_aborts_with_zero_filesystem_mutation() {
    let dir = TestDir::new("reject");
    let bin = dir.path("megu");
    let v1 = b"megu v1 binary";
    std::fs::write(&bin, v1).unwrap();
    let wrong = fixed_sha(0xd);
    assert!(is_sha256_hex(&wrong));
    assert!(matches!(
        AtomicBinarySwap::install(&bin, b"evil v2", &wrong),
        Err(UpdateError::ChecksumMismatch { .. })
    ));
    // Active binary untouched, no staged/backup remnants left behind.
    assert_eq!(read(&bin), v1);
    assert!(!staged_path(&bin).exists());
    assert!(!previous_path(&bin).exists());
    assert!(!failed_path(&bin).exists());
}

#[test]
fn swap_replaces_stale_prev_and_cleanup_only_touches_stale() {
    let dir = TestDir::new("restage");
    let bin = dir.path("megu");
    std::fs::write(&bin, b"v1").unwrap();
    AtomicBinarySwap::install(&bin, b"v2", &sha256_hex(b"v2")).unwrap();
    AtomicBinarySwap::install(&bin, b"v3", &sha256_hex(b"v3")).unwrap();
    assert_eq!(read(&bin), b"v3");
    assert_eq!(read(&previous_path(&bin)), b"v2");
    // Stale staged + failed files from an interrupted run are removed;
    // the rollback target survives.
    std::fs::write(staged_path(&bin), b"partial").unwrap();
    std::fs::write(failed_path(&bin), b"broken").unwrap();
    cleanup_stale_files(&bin).unwrap();
    assert!(!staged_path(&bin).exists());
    assert!(!failed_path(&bin).exists());
    assert!(previous_path(&bin).exists());
    assert_eq!(read(&bin), b"v3");
}

#[test]
fn end_to_end_verified_archive_update() {
    // The real pipeline: sums entry -> verify archive -> extract member ->
    // verified swap -> rollback. All in pure std.
    let dir = TestDir::new("e2e");
    let bin = dir.path("megu");
    std::fs::write(&bin, b"megu v1").unwrap();
    let member_path = "megu-v0.1.0-alpha.4-linux-x86_64/megu";
    let v2 = b"megu v0.1.0-alpha.4 binary";
    let archive = make_tar_gz(member_path, v2);
    let sums = format!(
        "{}  megu-v0.1.0-alpha.4-linux-x86_64.tar.gz\n",
        sha256_hex(&archive)
    );
    verify_bytes_sha256(&archive, &sums, "megu-v0.1.0-alpha.4-linux-x86_64.tar.gz").unwrap();
    let payload =
        extract_tar_gz_member(&archive, member_path, MAX_MEMBER_BYTES, MAX_TAR_BYTES).unwrap();
    AtomicBinarySwap::install_verified(&bin, &payload).unwrap();
    assert_eq!(read(&bin), v2);
    rollback(&bin).unwrap();
    assert_eq!(read(&bin), b"megu v1");
}

// ---------------------------------------------------------------------------
// launcher -Dmegu.clientSha256= pin synchronisation
// ---------------------------------------------------------------------------

fn official_launcher_json(sha: &str) -> String {
    format!(
        "{{\n  \"id\": \"megu-1.21.1\",\n  \"inheritsFrom\": \"1.21.1\",\n  \
         \"type\": \"release\",\n  \"mainClass\": \"net.megu.launcher.Main\",\n  \
         \"arguments\": {{\"jvm\": [\n    \
         \"-Dmegu.clientBin=/opt/megu/bin/megu-client-gpu\",\n    \
         \"-Dmegu.clientSha256={sha}\"\n  ]}},\n  \
         \"libraries\": [{{\"name\": \"net.megu:bootstrapper:1.0.0\"}}]\n}}\n"
    )
}

fn prism_patch_json(sha: &str) -> String {
    format!(
        "{{\"formatVersion\":1,\"uid\":\"net.megu.bootstrapper\",\
         \"version\":\"1.0.0\",\"name\":\"megu bootstrapper\",\
         \"mainClass\":\"net.megu.launcher.Main\",\"inheritsFrom\":\"1.21.1\",\
         \"libraries\":[{{\"name\":\"net.megu:bootstrapper:1.0.0\",\"MMC-hint\":\"local\"}}],\
         \"+jvmArgs\":[\"-Dmegu.clientSha256={sha}\"]}}\n"
    )
}

#[test]
fn launcher_pin_syncs_and_rolls_back_atomically() {
    let dir = TestDir::new("pin");
    let json = dir.path("megu-1.21.1.json");
    let original = official_launcher_json(OLD_SHA);
    std::fs::write(&json, &original).unwrap();

    let outcome = sync_launcher_sha256_pin(&json, OLD_SHA, NEW_SHA).unwrap();
    assert_eq!(outcome, PinSyncOutcome::Updated);
    let updated = read_string(&json);
    assert!(updated.contains(&format!("-Dmegu.clientSha256={NEW_SHA}")));
    assert!(!updated.contains(OLD_SHA));
    // Untouched formatting: only the digest digits changed.
    assert_eq!(updated, official_launcher_json(NEW_SHA));
    assert_eq!(read_string(&previous_path(&json)), original);

    rollback_launcher_sha256_pin(&json).unwrap();
    assert_eq!(read_string(&json), original);
    // The superseded pin file is kept for diagnosis, not deleted silently.
    assert_eq!(
        read_string(&failed_path(&json)),
        official_launcher_json(NEW_SHA)
    );
}

#[test]
fn prism_patch_jvm_args_pin_syncs() {
    let dir = TestDir::new("prism-pin");
    let patch = dir.path("net.megu.bootstrapper.json");
    let original = prism_patch_json(OLD_SHA);
    std::fs::write(&patch, &original).unwrap();
    sync_launcher_sha256_pin(&patch, OLD_SHA, NEW_SHA).unwrap();
    assert_eq!(read_string(&patch), prism_patch_json(NEW_SHA));
    rollback_launcher_sha256_pin(&patch).unwrap();
    assert_eq!(read_string(&patch), original);
}

#[test]
fn launcher_pin_mismatch_and_missing_pin_fail_closed() {
    let dir = TestDir::new("pin-mismatch");
    let json = dir.path("megu-1.21.1.json");
    let original = official_launcher_json(&fixed_sha(0xe));
    std::fs::write(&json, &original).unwrap();
    // Pin holds a different digest than expected: refuse, touch nothing.
    assert!(matches!(
        sync_launcher_sha256_pin(&json, OLD_SHA, NEW_SHA),
        Err(UpdateError::PinMismatch { .. })
    ));
    assert_eq!(read_string(&json), original);
    assert!(!staged_path(&json).exists());
    assert!(!previous_path(&json).exists());
    // Valid JSON without the property is reported, not rewritten.
    let nopin = dir.path("unpinned.json");
    std::fs::write(
        &nopin,
        "{\"id\":\"megu-1.21.1\",\"inheritsFrom\":\"1.21.1\"}",
    )
    .unwrap();
    assert_eq!(
        sync_launcher_sha256_pin(&nopin, OLD_SHA, NEW_SHA).unwrap(),
        PinSyncOutcome::NotPinned
    );
    // Malformed JSON and malformed digests are rejected before any write.
    let broken = dir.path("broken.json");
    std::fs::write(&broken, "{not json").unwrap();
    assert!(sync_launcher_sha256_pin(&broken, OLD_SHA, NEW_SHA).is_err());
    assert!(sync_launcher_sha256_pin(&json, "not-a-sha", NEW_SHA).is_err());
    assert!(sync_launcher_sha256_pin(&json, OLD_SHA, "short").is_err());
}

#[test]
fn launcher_mc_version_reads_every_profile_shape() {
    assert_eq!(
        launcher_mc_version(&official_launcher_json(OLD_SHA)).as_deref(),
        Some("1.21.1")
    );
    assert_eq!(
        launcher_mc_version(&prism_patch_json(OLD_SHA)).as_deref(),
        Some("1.21.1")
    );
    assert_eq!(
        launcher_mc_version(
            "{\"formatVersion\":1,\"components\":[{\"uid\":\"net.minecraft\",\"version\":\"1.21.1\",\"important\":true}]}"
        )
        .as_deref(),
        Some("1.21.1")
    );
    assert_eq!(
        launcher_mc_version("{\"requires\":[{\"uid\":\"net.minecraft\",\"equals\":\"1.21.4\"}]}")
            .as_deref(),
        Some("1.21.4")
    );
    assert_eq!(launcher_mc_version("{\"id\":\"x\"}"), None);
}

// ---------------------------------------------------------------------------
// multi-binary atomic bundle swap
// ---------------------------------------------------------------------------

#[test]
fn bundle_swaps_binaries_and_pin_as_one_transaction() {
    let dir = TestDir::new("bundle");
    let client = dir.path("megu-client-gpu");
    let server = dir.path("megu");
    let json = dir.path("megu-1.21.1.json");
    std::fs::write(&client, b"client v1").unwrap();
    std::fs::write(&server, b"server v1").unwrap();
    std::fs::write(&json, official_launcher_json(OLD_SHA)).unwrap();

    let mut bundle = AtomicBundleSwap::new();
    bundle.add_binary(client.clone(), b"client v2".to_vec());
    bundle.add_binary(server.clone(), b"server v2".to_vec());
    bundle.add_launcher_pin(json.clone(), OLD_SHA, NEW_SHA);
    let receipt = bundle.apply().unwrap();
    assert_eq!(receipt.paths.len(), 3);
    assert_eq!(read(&client), b"client v2");
    assert_eq!(read(&server), b"server v2");
    assert!(read_string(&json).contains(NEW_SHA));
    assert_eq!(read(&previous_path(&client)), b"client v1");

    AtomicBundleSwap::rollback(&receipt).unwrap();
    assert_eq!(read(&client), b"client v1");
    assert_eq!(read(&server), b"server v1");
    assert_eq!(read_string(&json), official_launcher_json(OLD_SHA));
    AtomicBundleSwap::cleanup(&receipt).unwrap();
    assert!(!failed_path(&client).exists());
}

#[test]
fn bundle_failure_mid_swap_unwinds_everything() {
    let dir = TestDir::new("bundle-fail");
    let first = dir.path("megu-client-gpu");
    let second = dir.path("megu");
    std::fs::write(&first, b"client v1").unwrap();
    std::fs::write(&second, b"server v1").unwrap();
    // A directory where `.megu-prev` must be a file forces the second
    // swap to fail after the first already committed.
    std::fs::create_dir(previous_path(&second)).unwrap();

    let mut bundle = AtomicBundleSwap::new();
    bundle.add_binary(first.clone(), b"client v2".to_vec());
    bundle.add_binary(second.clone(), b"server v2".to_vec());
    assert!(bundle.apply().is_err());
    // First binary rolled back to v1, second never touched, nothing staged.
    assert_eq!(read(&first), b"client v1");
    assert_eq!(read(&second), b"server v1");
    assert!(!staged_path(&first).exists());
    assert!(!staged_path(&second).exists());
    std::fs::remove_dir(previous_path(&second)).unwrap();
    cleanup_stale_files(&first).unwrap();
    cleanup_stale_files(&second).unwrap();
    assert_eq!(read(&first), b"client v1");
}

#[test]
fn bundle_pin_drift_aborts_before_any_write() {
    let dir = TestDir::new("bundle-pin-drift");
    let bin = dir.path("megu-client-gpu");
    let json = dir.path("megu-1.21.1.json");
    std::fs::write(&bin, b"client v1").unwrap();
    let original = official_launcher_json(&fixed_sha(0xe));
    std::fs::write(&json, &original).unwrap();

    let mut bundle = AtomicBundleSwap::new();
    bundle.add_binary(bin.clone(), b"client v2".to_vec());
    bundle.add_launcher_pin(json.clone(), OLD_SHA, NEW_SHA);
    assert!(matches!(
        bundle.apply(),
        Err(UpdateError::PinMismatch { .. })
    ));
    // Phase-0 abort: no staged files, no .megu-prev, both files original.
    assert_eq!(read(&bin), b"client v1");
    assert_eq!(read_string(&json), original);
    assert!(!staged_path(&bin).exists());
    assert!(!previous_path(&bin).exists());
    assert!(!previous_path(&json).exists());
}

// ---------------------------------------------------------------------------
// minecraft version boundary guard
// ---------------------------------------------------------------------------

#[test]
fn mc_version_boundary_blocks_silent_cross_version_update() {
    mc_version_compatible("1.21.1", "1.21.1", false).unwrap();
    mc_version_compatible("1.21.1", "1.21.4", true).unwrap();
    assert!(matches!(
        mc_version_compatible("1.21.1", "1.21.4", false),
        Err(UpdateError::CrossMcVersion {
            ref installed,
            ref candidate
        }) if installed == "1.21.1" && candidate == "1.21.4"
    ));
    // Through the plan: a release targeting a different MC version is
    // refused in-place even when every other check passes.
    let releases =
        parse_release_list(&format!("[{}]", full_asset_release("v0.2.0-alpha.1"))).unwrap();
    let mut plan = select_update(
        &releases,
        &SemVer::parse("0.1.0").unwrap(),
        UpdateChannel::Prerelease,
        platform(ReleaseOs::Linux, ReleaseArch::X86_64, false),
        UpdateComponent::Client,
    )
    .unwrap()
    .unwrap();
    plan.mc_version = Some("1.21.4".to_owned());
    let installed = launcher_mc_version(&official_launcher_json(OLD_SHA)).unwrap();
    assert!(matches!(
        plan.check_mc_compat(&installed, false),
        Err(UpdateError::CrossMcVersion { .. })
    ));
    plan.mc_version = Some("1.21.1".to_owned());
    plan.check_mc_compat(&installed, false).unwrap();
    plan.check_mc_compat("1.21.4", true).unwrap();
}
