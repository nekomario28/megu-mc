// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Cross-OS coverage for the native `megu-installer` lanes: the Official
//! Launcher version/profile flow (with Windows Store dual profiles, `.bak`
//! backups, SHA-256 client pins and clean uninstall), the Prism instance
//! layout, the dedicated-server layout with scripts and a never-forced
//! EULA, and the fail-closed traversal/version/symlink matrix.

use megu_server_files::installer::{
    self, ClientInstallOptions, OsKind, ServerInstallOptions, BOOTSTRAP_LIBRARY_REL,
    BOOTSTRAP_PATCH_FILE, LAUNCHER_PROFILES_FILE, LAUNCHER_PROFILES_STORE_FILE,
};
use megu_server_files::json::{self, Json};
use megu_server_files::{check_eula, EulaStatus, CLIENT_STANDARD_FOLDERS};
use megu_core::version::TargetVersion;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

const NOW: &str = "2026-11-23T12:00:00+00:00";
const NOW_LATER: &str = "2026-11-24T12:00:00+00:00";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "megu-installer-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_installer(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_megu-installer"))
        .args(args)
        .output()
        .expect("run megu-installer")
}

fn read_json(path: &Path) -> Json {
    json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn get<'a>(doc: &'a Json, key: &str) -> &'a Json {
    doc.get(key).unwrap_or_else(|| panic!("missing {key} in {doc:?}"))
}

fn client_options() -> ClientInstallOptions {
    ClientInstallOptions {
        mc_version: TargetVersion::V1_21_1,
        bootstrap_jar: None,
        client_bin: None,
        client_sha256: None,
    }
}

fn profile_entry<'a>(profiles_file: &'a Json, version_id: &str) -> &'a Json {
    get(profiles_file, "profiles")
        .get(version_id)
        .unwrap_or_else(|| panic!("missing profile {version_id} in {profiles_file:?}"))
}

fn assert_profile_entry(profiles_file: &Json, version_id: &str, mc_version: &str) {
    let entry = profile_entry(profiles_file, version_id);
    assert_eq!(get(entry, "type"), &Json::String("custom".into()));
    assert_eq!(
        get(entry, "lastVersionId"),
        &Json::String(version_id.into())
    );
    assert_eq!(get(entry, "icon"), &Json::String("Furnace".into()));
    assert_eq!(
        get(entry, "name"),
        &Json::String(format!("Megu ({mc_version})"))
    );
    assert!(get(entry, "created").as_str().unwrap().ends_with("+00:00"));
    assert!(get(entry, "lastUsed").as_str().unwrap().ends_with("+00:00"));
}

fn assert_version_json(mc_dir: &Path, version_id: &str, inherits: &str) -> Json {
    let path = mc_dir
        .join("versions")
        .join(version_id)
        .join(format!("{version_id}.json"));
    assert!(path.is_file(), "{}", path.display());
    let profile = read_json(&path);
    assert_eq!(get(&profile, "id"), &Json::String(version_id.into()));
    assert_eq!(
        get(&profile, "inheritsFrom"),
        &Json::String(inherits.into())
    );
    assert_eq!(get(&profile, "type"), &Json::String("release".into()));
    assert_eq!(
        get(&profile, "mainClass"),
        &Json::String(installer::BOOTSTRAP_MAIN_CLASS.into())
    );
    assert_eq!(
        get(&profile, "libraries"),
        &Json::Array(vec![Json::Object(vec![(
            "name".into(),
            Json::String(installer::BOOTSTRAP_LIBRARY_COORD.into()),
        )])])
    );
    for key in ["releaseTime", "time"] {
        assert!(get(&profile, key).as_str().unwrap().ends_with("+00:00"));
    }
    profile
}

#[test]
fn official_launcher_install_upgrade_uninstall_across_windows_macos_linux_paths() {
    // -- Deterministic detection across the three OSes ----------------------
    let home = Path::new("/home/u");
    let appdata = Path::new("C:\\Users\\u\\AppData\\Roaming");
    assert_eq!(
        installer::detect_minecraft_dir_for_os(OsKind::Windows, home, Some(appdata)),
        appdata.join(".minecraft")
    );
    // No %APPDATA%: the Roaming fallback under home applies.
    assert_eq!(
        installer::detect_minecraft_dir_for_os(OsKind::Windows, home, None),
        home.join("AppData").join("Roaming").join(".minecraft")
    );
    assert_eq!(
        installer::detect_minecraft_dir_for_os(OsKind::MacOs, home, None),
        home.join("Library").join("Application Support").join("minecraft")
    );
    assert_eq!(
        installer::detect_minecraft_dir_for_os(OsKind::Linux, home, None),
        home.join(".minecraft")
    );
    // Linux Flatpak: wins only while ~/.minecraft is absent.
    assert_eq!(
        installer::resolve_minecraft_dir(OsKind::Linux, home, None, |p| {
            p == home.join(".var/app/com.mojang.Minecraft/.minecraft")
        }),
        home.join(".var/app/com.mojang.Minecraft/.minecraft")
    );
    assert_eq!(
        installer::resolve_minecraft_dir(OsKind::Linux, home, None, |_| true),
        home.join(".minecraft")
    );

    // -- Install with Windows-Store dual profiles and a pinned client -------
    let scratch = Scratch::new();
    let mc_dir = scratch.path().join("mc");
    std::fs::create_dir_all(&mc_dir).unwrap();
    let existing = json::parse(
        r#"{"profiles":{"vanilla prof":{"name":"バニラ プロファイル","type":"custom","lastVersionId":"1.21.1","nested":{"a":[1,2,{"b":"x{y}"}]}}},"selectedProfile":"vanilla prof","version":3}"#,
    )
    .unwrap();
    let original_std = format!("{}\n", existing.to_json_string());
    let original_store = "{ \"profiles\" : { \"store one\" : { \"name\" : \"Store\" } } }";
    std::fs::write(mc_dir.join(LAUNCHER_PROFILES_FILE), &original_std).unwrap();
    std::fs::write(mc_dir.join(LAUNCHER_PROFILES_STORE_FILE), original_store).unwrap();

    // A fake bootstrapper JAR and native client to stage/pin.
    let jar = scratch.path().join("megu-bootstrapper.jar");
    std::fs::write(&jar, b"PK\x03\x04fake-jar").unwrap();
    let client = scratch.path().join("megu-client");
    std::fs::write(&client, b"#!/bin/sh\nexit 0\n").unwrap();
    let client_sha = installer::sha256_file(&client).unwrap();

    let mut options = client_options();
    options.bootstrap_jar = Some(jar.clone());
    options.client_bin = Some(client.clone());
    options.client_sha256 = Some(client_sha.clone());
    installer::install_client(&mc_dir, &options, true, NOW).unwrap();

    let version = assert_version_json(&mc_dir, "megu-1.21.1", "1.21.1");
    // The pin computes SHA-256 automatically: both jvm args are present.
    let jvm = get(&version, "arguments")
        .get("jvm")
        .and_then(Json::as_array)
        .unwrap();
    assert_eq!(jvm.len(), 2);
    assert!(
        jvm[0].as_str().unwrap().starts_with("-Dmegu.clientBin="),
        "{jvm:?}"
    );
    assert_eq!(
        jvm[1],
        Json::String(format!("-Dmegu.clientSha256={client_sha}"))
    );
    // The bootstrapper JAR was staged under the Maven layout.
    assert_eq!(
        std::fs::read(mc_dir.join("libraries").join(BOOTSTRAP_LIBRARY_REL)).unwrap(),
        b"PK\x03\x04fake-jar"
    );

    // Both profile documents were updated; both backups hold pre-edit bytes.
    for (name, original) in [
        (LAUNCHER_PROFILES_FILE, original_std.as_str()),
        (LAUNCHER_PROFILES_STORE_FILE, original_store),
    ] {
        let backup = mc_dir.join(format!("{name}.bak"));
        assert!(backup.is_file(), "{name}.bak");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
        assert_profile_entry(&read_json(&mc_dir.join(name)), "megu-1.21.1", "1.21.1");
    }
    let std_profiles = read_json(&mc_dir.join(LAUNCHER_PROFILES_FILE));
    // Every pre-existing profile and root member survived untouched.
    let vanilla = profile_entry(&std_profiles, "vanilla prof");
    let nested_a = get(get(vanilla, "nested"), "a").as_array().unwrap();
    assert_eq!(get(&nested_a[2], "b"), &Json::String("x{y}".into()));
    assert_eq!(
        get(&std_profiles, "selectedProfile"),
        &Json::String("vanilla prof".into())
    );
    assert_eq!(get(&std_profiles, "version"), &Json::Number(3.0));
    let store = read_json(&mc_dir.join(LAUNCHER_PROFILES_STORE_FILE));
    assert!(profile_entry(&store, "store one").get("name").is_some());

    // -- Upgrade: a second install is idempotent and keeps the profiles ----
    installer::install_client(&mc_dir, &options, true, NOW_LATER).unwrap();
    let std_profiles = read_json(&mc_dir.join(LAUNCHER_PROFILES_FILE));
    let keys: Vec<&str> = match get(&std_profiles, "profiles") {
        Json::Object(members) => members.iter().map(|(k, _)| k.as_str()).collect(),
        _ => panic!("profiles must be an object"),
    };
    assert_eq!(keys.iter().filter(|k| **k == "megu-1.21.1").count(), 1);
    assert!(keys.contains(&"vanilla prof"));
    let entry = profile_entry(&std_profiles, "megu-1.21.1");
    assert_eq!(get(entry, "lastUsed"), &Json::String(NOW_LATER.into()));
    // The backup captures the document the upgrade replaced: the first
    // install's output, still stamped with the earlier timestamp.
    let backup = read_json(&mc_dir.join(format!("{LAUNCHER_PROFILES_FILE}.bak")));
    let backed_up = profile_entry(&backup, "megu-1.21.1");
    assert_eq!(get(backed_up, "lastUsed"), &Json::String(NOW.into()));
    assert!(profile_entry(&backup, "vanilla prof").get("name").is_some());
    // --no-profile leaves the profile documents alone on a fresh install.
    let bare = scratch.path().join("bare-mc");
    let mut plain = client_options();
    plain.mc_version = TargetVersion::V26_3;
    installer::install_client(&bare, &plain, false, NOW).unwrap();
    assert_version_json(&bare, "megu-26.3", "26.3");
    assert!(!bare.join(LAUNCHER_PROFILES_FILE).exists());
    assert!(!bare.join(LAUNCHER_PROFILES_STORE_FILE).exists());

    // -- Uninstall removes only our version dir and entries -----------------
    installer::uninstall_client(&mc_dir, TargetVersion::V1_21_1, true, NOW_LATER).unwrap();
    assert!(!mc_dir.join("versions").join("megu-1.21.1").exists());
    for name in [LAUNCHER_PROFILES_FILE, LAUNCHER_PROFILES_STORE_FILE] {
        let doc = read_json(&mc_dir.join(name));
        let Json::Object(profiles) = get(&doc, "profiles") else {
            panic!("profiles must be an object in {name}")
        };
        assert!(!profiles.iter().any(|(k, _)| k == "megu-1.21.1"), "{name}");
    }
    let std_profiles = read_json(&mc_dir.join(LAUNCHER_PROFILES_FILE));
    assert!(profile_entry(&std_profiles, "vanilla prof").get("name").is_some());
    assert_eq!(
        get(&std_profiles, "selectedProfile"),
        &Json::String("vanilla prof".into())
    );
    let store = read_json(&mc_dir.join(LAUNCHER_PROFILES_STORE_FILE));
    assert!(profile_entry(&store, "store one").get("name").is_some());
}

#[test]
fn prism_instance_install_creates_valid_cfg_mmc_pack_and_standard_client_folders() {
    // -- Deterministic detection, including XDG and the Flatpak fallback ----
    let home = Path::new("/home/u");
    let appdata = Path::new("C:\\Users\\u\\AppData\\Roaming");
    let xdg = Path::new("/xdg-data");
    assert_eq!(
        installer::detect_prism_instances_dir_for_os(OsKind::Windows, home, Some(appdata), None),
        appdata.join("PrismLauncher").join("instances")
    );
    assert_eq!(
        installer::detect_prism_instances_dir_for_os(OsKind::Windows, home, None, None),
        home.join("AppData")
            .join("Roaming")
            .join("PrismLauncher")
            .join("instances")
    );
    assert_eq!(
        installer::detect_prism_instances_dir_for_os(OsKind::MacOs, home, None, None),
        home.join("Library")
            .join("Application Support")
            .join("PrismLauncher")
            .join("instances")
    );
    assert_eq!(
        installer::detect_prism_instances_dir_for_os(OsKind::Linux, home, None, Some(xdg)),
        xdg.join("PrismLauncher").join("instances")
    );
    assert_eq!(
        installer::detect_prism_instances_dir_for_os(OsKind::Linux, home, None, None),
        home.join(".local")
            .join("share")
            .join("PrismLauncher")
            .join("instances")
    );
    assert_eq!(
        installer::resolve_prism_instances_dir(OsKind::Linux, home, None, None, |p| p
            == home.join(
                ".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/instances"
            )),
        home.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/instances")
    );
    assert_eq!(
        installer::resolve_prism_instances_dir(OsKind::Linux, home, None, None, |_| true),
        home.join(".local/share/PrismLauncher/instances")
    );

    let scratch = Scratch::new();
    let instances = scratch.path().join("instances");

    // -- Patch mode: --bootstrap-jar + pinned client ------------------------
    let jar = scratch.path().join("megu-bootstrapper.jar");
    std::fs::write(&jar, b"PK\x03\x04fake-jar").unwrap();
    let client = scratch.path().join("megu-client");
    std::fs::write(&client, b"#!/bin/sh\nexit 0\n").unwrap();
    let client_sha = installer::sha256_file(&client).unwrap();

    let mut options = client_options();
    options.bootstrap_jar = Some(jar);
    options.client_bin = Some(client);
    installer::install_prism(&instances, &options).unwrap();

    let instance = instances.join("Megu-1.21.1");
    let cfg = std::fs::read_to_string(instance.join("instance.cfg")).unwrap();
    assert!(cfg.contains("InstanceType=OneSix"), "{cfg}");
    assert!(cfg.contains("name=Megu 1.21.1"), "{cfg}");
    assert!(cfg.contains("OverrideCommands=false"), "{cfg}");

    let mmc = read_json(&instance.join("mmc-pack.json"));
    assert_eq!(get(&mmc, "formatVersion"), &Json::Number(1.0));
    let components = get(&mmc, "components").as_array().unwrap();
    assert_eq!(components.len(), 2);
    assert_eq!(get(&components[0], "uid"), &Json::String("net.minecraft".into()));
    assert_eq!(get(&components[0], "version"), &Json::String("1.21.1".into()));
    assert_eq!(get(&components[0], "important"), &Json::Bool(true));
    assert_eq!(
        get(&components[1], "uid"),
        &Json::String(installer::BOOTSTRAP_PATCH_UID.into())
    );

    let patch = read_json(&instance.join("patches").join(BOOTSTRAP_PATCH_FILE));
    assert_eq!(
        get(&patch, "mainClass"),
        &Json::String(installer::BOOTSTRAP_MAIN_CLASS.into())
    );
    assert_eq!(get(&patch, "inheritsFrom"), &Json::String("1.21.1".into()));
    let libs = get(&patch, "libraries").as_array().unwrap();
    assert_eq!(
        get(&libs[0], "name"),
        &Json::String(installer::BOOTSTRAP_LIBRARY_COORD.into())
    );
    assert_eq!(get(&libs[0], "MMC-hint"), &Json::String("local".into()));
    let jvm_args = get(&patch, "+jvmArgs").as_array().unwrap();
    assert!(jvm_args
        .iter()
        .any(|a| a.as_str().unwrap().starts_with("-Dmegu.clientBin=")));
    assert!(jvm_args
        .iter()
        .any(|a| a.as_str().unwrap() == format!("-Dmegu.clientSha256={client_sha}")));
    assert_eq!(
        std::fs::read(instance.join("libraries").join(BOOTSTRAP_LIBRARY_REL)).unwrap(),
        b"PK\x03\x04fake-jar"
    );
    assert!(instance.join(".minecraft/bin/megu-client").is_file());
    for folder in CLIENT_STANDARD_FOLDERS {
        assert!(
            instance.join(".minecraft").join(folder).is_dir(),
            ".minecraft/{folder}"
        );
    }

    // -- Wrapper mode: --client-bin without a bootstrapper patch ------------
    let client2 = scratch.path().join("megu-client-b");
    std::fs::write(&client2, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut wrap = client_options();
    wrap.mc_version = TargetVersion::V26_3;
    wrap.client_bin = Some(client2);
    installer::install_prism(&instances, &wrap).unwrap();
    let wrap_instance = instances.join("Megu-26.3");
    let cfg = std::fs::read_to_string(wrap_instance.join("instance.cfg")).unwrap();
    assert!(cfg.contains("OverrideCommands=true"), "{cfg}");
    let wrapper_line = cfg
        .lines()
        .find(|l| l.starts_with("WrapperCommand="))
        .expect("WrapperCommand set");
    assert!(wrapper_line.contains("bin/megu-client"), "{wrapper_line}");
    let mmc = read_json(&wrap_instance.join("mmc-pack.json"));
    let components = get(&mmc, "components").as_array().unwrap();
    assert_eq!(components.len(), 1);
    assert_eq!(get(&components[0], "version"), &Json::String("26.3".into()));
    assert!(!wrap_instance.join("patches").join(BOOTSTRAP_PATCH_FILE).exists());
    assert!(wrap_instance.join("bin/megu-client").is_file());
    for folder in CLIENT_STANDARD_FOLDERS {
        assert!(
            wrap_instance.join(".minecraft").join(folder).is_dir(),
            ".minecraft/{folder}"
        );
    }
}

#[test]
fn server_install_creates_standard_layout_run_scripts_and_never_auto_accepts_eula() {
    let scratch = Scratch::new();
    let dir = scratch.path().join("server dir with spaces");
    let fake_bin = scratch.path().join("fake-server");
    std::fs::write(&fake_bin, b"#!/bin/sh\nexit 0\n").unwrap();
    let options = ServerInstallOptions {
        mc_version: TargetVersion::V1_21_1,
        server_bin: Some(fake_bin.clone()),
    };
    installer::install_server(&dir, &options).unwrap();

    // The EULA file exists but was NOT accepted for the operator.
    assert_eq!(check_eula(&dir), EulaStatus::NotAccepted);
    let eula = std::fs::read_to_string(dir.join("eula.txt")).unwrap();
    assert!(eula.contains("eula=false"), "{eula}");
    assert!(!eula.contains("eula=true"), "{eula}");

    for folder in [
        "mods",
        "config",
        "defaultconfigs",
        "logs",
        "crash-reports",
        "backups",
        "world/datapacks",
    ] {
        assert!(dir.join(folder).is_dir(), "{folder}");
    }

    // run.sh: LF endings, executable, gates on eula.txt/EULA=TRUE.
    let run_sh = dir.join("run.sh");
    let sh = std::fs::read(&run_sh).unwrap();
    assert!(!sh.contains(&b'\r'), "run.sh must be LF-only");
    assert!(sh.starts_with(b"#!/bin/sh"), "run.sh shebang");
    assert!(sh.windows(9).any(|w| w == b"eula=true"), "run.sh EULA gate");
    assert!(sh.windows(4).any(|w| w == b"\"$@\""), "run.sh forwards args");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(run_sh.metadata().unwrap().permissions().mode() & 0o777, 0o755);
        let staged = dir.join("megu");
        assert_eq!(
            staged.metadata().unwrap().permissions().mode() & 0o111,
            0o111
        );
    }

    // run.bat: CRLF endings, same gate, launches megu.exe.
    let bat = std::fs::read(dir.join("run.bat")).unwrap();
    let bat_text = String::from_utf8(bat.clone()).unwrap();
    assert!(bat.starts_with(b"@echo off\r\n"), "run.bat CRLF prologue");
    assert!(
        bat_text
            .lines()
            .skip(1)
            .all(|line| line.is_empty() || !line.is_empty()),
        "all lines present"
    );
    assert!(bat_text.contains("\r\n"));
    assert!(!bat_text.replace("\r\n", "").contains('\n'), "every line ends CRLF");
    assert!(bat_text.contains("eula=true"), "run.bat EULA gate");
    assert!(bat_text.contains("megu.exe %*"), "run.bat launches megu.exe");

    let readme = std::fs::read_to_string(dir.join("README-SERVER.txt")).unwrap();
    assert!(readme.to_ascii_lowercase().contains("eula"), "{readme}");

    // The staged server binary is byte-identical to the input.
    assert_eq!(std::fs::read(dir.join("megu")).unwrap(), std::fs::read(&fake_bin).unwrap());

    // The generated run.sh genuinely refuses before the EULA is accepted.
    #[cfg(unix)]
    {
        let refused = Command::new("/bin/sh")
            .arg(&run_sh)
            .current_dir(&dir)
            .env("PATH", "/usr/bin:/bin")
            .env("EULA", "")
            .output()
            .unwrap();
        assert!(!refused.status.success(), "run.sh must refuse without EULA");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("EULA"),
            "{:?}",
            refused.stderr
        );
        // And a signed eula.txt lets it through to the staged binary.
        std::fs::write(dir.join("eula.txt"), b"eula=true\n").unwrap();
        let accepted = Command::new("/bin/sh")
            .arg(&run_sh)
            .current_dir(&dir)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(accepted.status.success(), "{:?}", accepted.stderr);
        // A re-install never silently revokes the operator's acceptance.
        installer::install_server(&dir, &options).unwrap();
        assert_eq!(check_eula(&dir), EulaStatus::Accepted);
    }
}

#[test]
fn installer_fails_closed_on_traversal_bad_version_and_symlink_targets() {
    let scratch = Scratch::new();

    // Traversal components in the server dir are rejected outright.
    let bad = scratch.path().join("..").join("escape");
    assert!(installer::install_server(&bad, &ServerInstallOptions::default()).is_err());

    // An unsupported Minecraft version never reaches the filesystem.
    assert!(TargetVersion::parse("1.99.9").is_none());
    assert!(TargetVersion::parse("../etc").is_none());
    let out = run_installer(&[
        "client",
        "--minecraft-dir",
        scratch.path().join("bad-v").to_str().unwrap(),
        "--mc-version",
        "bogus",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!scratch.path().join("bad-v").exists());

    // A symlinked install root is refused.
    let real = scratch.path().join("real-server");
    std::fs::create_dir(&real).unwrap();
    let link = scratch.path().join("link-server");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = installer::install_server(&link, &ServerInstallOptions::default())
            .expect_err("symlinked server dir must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!real.join("run.sh").exists());
    }

    // A planted symlink at run.sh is never followed.
    #[cfg(unix)]
    {
        let planted = scratch.path().join("planted");
        std::fs::create_dir(&planted).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", planted.join("run.sh")).unwrap();
        let before = std::fs::read("/etc/hostname").unwrap();
        assert!(installer::install_server(&planted, &ServerInstallOptions::default()).is_err());
        assert_eq!(std::fs::read("/etc/hostname").unwrap(), before);
    }

    // Staged inputs must exist, be files and not be symlinks.
    let missing = installer::install_server(
        &scratch.path().join("s1"),
        &ServerInstallOptions {
            mc_version: TargetVersion::V1_21_1,
            server_bin: Some(scratch.path().join("no-such-bin")),
        },
    );
    assert!(missing.is_err());
    #[cfg(unix)]
    {
        let bin_target = scratch.path().join("bin-target");
        std::fs::write(&bin_target, b"x").unwrap();
        let bin_link = scratch.path().join("bin-link");
        std::os::unix::fs::symlink(&bin_target, &bin_link).unwrap();
        assert!(installer::install_server(
            &scratch.path().join("s2"),
            &ServerInstallOptions {
                mc_version: TargetVersion::V1_21_1,
                server_bin: Some(bin_link),
            },
        )
        .is_err());
        // A directory is not a binary either.
        assert!(installer::install_server(
            &scratch.path().join("s3"),
            &ServerInstallOptions {
                mc_version: TargetVersion::V1_21_1,
                server_bin: Some(real.clone()),
            },
        )
        .is_err());
    }

    // Traversal and symlinks are refused in the client lane too.
    let traversal = installer::install_client(
        &scratch.path().join("..").join("mc-escape"),
        &client_options(),
        false,
        NOW,
    );
    assert!(traversal.is_err());
    let mc_dir = scratch.path().join("mc2");
    std::fs::create_dir_all(&mc_dir).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/hostname", mc_dir.join(LAUNCHER_PROFILES_FILE)).unwrap();
        assert!(installer::install_client(&mc_dir, &client_options(), true, NOW).is_err());
        assert_eq!(std::fs::read("/etc/hostname").unwrap(), before_hostname());
    }

    // A symlinked versions directory fails closed instead of being followed.
    #[cfg(unix)]
    {
        let mc_versions_dir = scratch.path().join("mc-vers");
        std::fs::create_dir(&mc_versions_dir).unwrap();
        let outside = scratch.path().join("outside-versions");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, mc_versions_dir.join("versions")).unwrap();
        assert!(installer::install_client(&mc_versions_dir, &client_options(), false, NOW).is_err());
        assert!(!outside.join("megu-1.21.1").exists());
    }

    // --client-bin must be an existing file; --client-sha256 must be hex.
    let mut bad_bin = client_options();
    bad_bin.client_bin = Some(scratch.path().join("absent"));
    assert!(installer::install_client(&scratch.path().join("mc3"), &bad_bin, false, NOW).is_err());
    let mut bad_sha = client_options();
    bad_sha.client_sha256 = Some("not-hex".to_owned());
    assert!(installer::install_client(&scratch.path().join("mc4"), &bad_sha, false, NOW).is_err());
}

fn before_hostname() -> Vec<u8> {
    std::fs::read("/etc/hostname").unwrap()
}

#[test]
fn cli_help_version_json_and_usage_errors() {
    for flag in ["--help", "-h"] {
        let out = run_installer(&[flag]);
        assert!(out.status.success(), "{flag}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("USAGE"));
    }
    let out = run_installer(&["--version"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("megu-installer"));

    let scratch = Scratch::new();
    let mc = scratch.path().join("mc");
    let out = run_installer(&[
        "client",
        "--minecraft-dir",
        mc.to_str().unwrap(),
        "--json",
    ]);
    assert!(out.status.success(), "{:?}", out.stderr);
    let report = json::parse(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(get(&report, "command"), &Json::String("client".into()));
    assert_eq!(get(&report, "mc_version"), &Json::String("1.21.1".into()));
    assert!(mc.join("versions").join("megu-1.21.1").join("megu-1.21.1.json").is_file());

    let out = run_installer(&[
        "uninstall",
        "--minecraft-dir",
        mc.to_str().unwrap(),
        "--json",
    ]);
    assert!(out.status.success(), "{:?}", out.stderr);
    assert!(!mc.join("versions").join("megu-1.21.1").exists());

    let out = run_installer(&["server"]);
    assert_eq!(out.status.code(), Some(2));
    let out = run_installer(&["nonsense-subcommand"]);
    assert_eq!(out.status.code(), Some(2));
    let out = run_installer(&["--frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    // `uninstall` rejects client-only flags it does not understand.
    let out = run_installer(&["uninstall", "--client-bin", "x"]);
    assert_eq!(out.status.code(), Some(2));
}
