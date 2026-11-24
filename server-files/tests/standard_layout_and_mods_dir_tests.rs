// SPDX-License-Identifier: LGPL-3.0-or-later
//! Cross-OS hardening coverage for the folder layer: `eula.txt` parsing
//! against real-world editor quirks, the `mods/` scanner's typed skip
//! classification, and the standard dedicated-server / client `<gameDir>`
//! folder layouts.

use megu_core::version::TargetVersion;
use megu_server_files::{
    check_eula, ensure_client_game_dir_layout, ensure_server_game_dir_layout, scan_mods_folder,
    EulaStatus, FolderOutcome, ModsDirSkipReason, ModsFolderScan, ServerDirectory, EULA_FILE,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "megu-layout-{}-{}-{tag}",
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

#[test]
fn eula_accepts_windows_notepad_bom_and_crlf_and_rejects_false_or_garbage() {
    let scratch = Scratch::new("eula");
    let eula = scratch.path(EULA_FILE);

    // Accepted: Windows Notepad's UTF-8 BOM (EF BB BF), CRLF and classic-CR
    // line endings, whitespace around the key/separator/value, `:` as a
    // separator, case-insensitive `true`, and comment/blank lines.
    for body in [
        b"\xEF\xBB\xBF#By changing the setting below to TRUE you are indicating your agreement.\r\neula=true\r\n".as_slice(),
        b"\xEF\xBB\xBFeula=true\r\n".as_slice(),
        b"\xEF\xBB\xBF  eula  =  TRUE  \r\n".as_slice(),
        b"eula=true\r\n".as_slice(),
        b"eula=true\r".as_slice(),
        b"eula=true".as_slice(),
        b"eula : True\n".as_slice(),
        b"# comment above\n\neula\t=\tTrUe\n# comment below\n".as_slice(),
    ] {
        std::fs::write(&eula, body).unwrap();
        assert_eq!(check_eula(&scratch.0), EulaStatus::Accepted, "{body:?}");
    }

    // Rejected: anything that is not the exact token `true` fails closed,
    // including partial tokens and `#` glued onto the value (a `#` only
    // comments when it starts the line).
    for body in [
        b"\xEF\xBB\xBFeula=false\r\n".as_slice(),
        b"eula=false\n".as_slice(),
        b"eula=trueish\n".as_slice(),
        b"eula=true#comment\n".as_slice(),
        b"eula=\n".as_slice(),
        b"eul=true\n".as_slice(),
        b"eula=\\u00zz\n".as_slice(),
        b"eula=true\neula=false\n".as_slice(), // last wins, like Java
    ] {
        std::fs::write(&eula, body).unwrap();
        assert_eq!(check_eula(&scratch.0), EulaStatus::NotAccepted, "{body:?}");
    }

    // Missing stays its own status — never silently NotAccepted.
    std::fs::remove_file(&eula).unwrap();
    assert_eq!(check_eula(&scratch.0), EulaStatus::Missing);
}

#[test]
fn mods_dir_scanner_handles_disabled_appledouble_dotfiles_version_subdir_and_symlinks() {
    let scratch = Scratch::new("mods");
    let mods = scratch.path("mods");
    std::fs::create_dir_all(mods.join("1.21.1")).unwrap();
    std::fs::create_dir_all(mods.join("1.20.1")).unwrap();
    std::fs::create_dir_all(mods.join("memory_repo")).unwrap();
    for (rel, bytes) in [
        ("b.jar", b"jar bytes".as_slice()),
        ("A.JAR", b"jar bytes".as_slice()), // Windows-style uppercase suffix
        ("mod.jar.disabled", b"jar bytes".as_slice()),
        ("toggle.disabled", b"jar bytes".as_slice()),
        ("._create.jar", b"AppleDouble".as_slice()),
        (".DS_Store", b"metadata".as_slice()),
        ("README.txt", b"docs".as_slice()),
        ("notes.zip", b"zip".as_slice()),
        ("config.toml", b"toml".as_slice()),
        ("1.21.1/extra.jar", b"jar bytes".as_slice()),
        ("1.20.1/old.jar", b"jar bytes".as_slice()),
    ] {
        std::fs::write(mods.join(rel), bytes).unwrap();
    }

    // With a matching version, `1.21.1/` descends and every other entry is
    // accounted for exactly once — nothing silently dropped.
    let scan = scan_mods_folder(&mods, Some("1.21.1")).unwrap();
    assert_eq!(scan.root, mods);
    assert_eq!(
        scan.relative_active_mods(),
        vec!["1.21.1/extra.jar", "A.JAR", "b.jar"],
        "jars sorted by normalized relative path"
    );
    let reason_for = |name: &str| -> ModsDirSkipReason {
        scan.skipped
            .iter()
            .find(|s| s.path.file_name().unwrap() == name)
            .unwrap_or_else(|| panic!("{name} was dropped instead of recorded"))
            .reason
    };
    assert_eq!(reason_for("mod.jar.disabled"), ModsDirSkipReason::DisabledMod);
    assert_eq!(reason_for("toggle.disabled"), ModsDirSkipReason::DisabledMod);
    assert_eq!(
        reason_for("._create.jar"),
        ModsDirSkipReason::DotfileOrAppleDouble
    );
    assert_eq!(
        reason_for(".DS_Store"),
        ModsDirSkipReason::DotfileOrAppleDouble
    );
    assert_eq!(reason_for("README.txt"), ModsDirSkipReason::NonJarFile);
    assert_eq!(reason_for("notes.zip"), ModsDirSkipReason::NonJarFile);
    assert_eq!(reason_for("config.toml"), ModsDirSkipReason::NonJarFile);
    assert_eq!(reason_for("1.20.1"), ModsDirSkipReason::OtherSubdirectory);
    assert_eq!(
        reason_for("memory_repo"),
        ModsDirSkipReason::OtherSubdirectory
    );
    // The other-version folder is reported, never descended: old.jar is
    // nowhere in the report.
    assert!(scan
        .skipped
        .iter()
        .all(|s| s.path.file_name().unwrap() != "old.jar"));

    // Without a version, `1.21.1/` is just another subdirectory.
    let scan = scan_mods_folder(&mods, None).unwrap();
    assert_eq!(scan.relative_active_mods(), vec!["A.JAR", "b.jar"]);
    assert_eq!(reason_of(&scan, "1.21.1"), ModsDirSkipReason::OtherSubdirectory);

    // Bad version names are refused before anything is read.
    for bad in ["..", ".", "a/b", "a\\b", "", "1.2\0", ".1.21.1"] {
        assert!(
            scan_mods_folder(&mods, Some(bad)).is_err(),
            "{bad:?} must be rejected"
        );
    }

    // A missing mods/ folder is a clean empty scan, not an error.
    let scan = scan_mods_folder(&scratch.path("no-such-mods"), Some("1.21.1")).unwrap();
    assert!(scan.active_mods.is_empty() && scan.skipped.is_empty());

    // Symlinks are refused — even ones pointing at a perfectly good jar —
    // so nothing can redirect the loader outside `mods/`.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(mods.join("b.jar"), mods.join("alias.jar")).unwrap();
        std::os::unix::fs::symlink("/etc", mods.join("escape.jar")).unwrap();
        let scan = scan_mods_folder(&mods, Some("1.21.1")).unwrap();
        assert_eq!(
            scan.relative_active_mods(),
            vec!["1.21.1/extra.jar", "A.JAR", "b.jar"],
            "a symlinked jar must never activate"
        );
        let reason_for = |name: &str| -> ModsDirSkipReason {
            scan.skipped
                .iter()
                .find(|s| s.path.file_name().unwrap() == name)
                .unwrap_or_else(|| panic!("{name} was dropped instead of recorded"))
                .reason
        };
        assert_eq!(reason_for("alias.jar"), ModsDirSkipReason::SymlinkRefused);
        assert_eq!(reason_for("escape.jar"), ModsDirSkipReason::SymlinkRefused);
    }
}

fn reason_of(scan: &ModsFolderScan, name: &str) -> ModsDirSkipReason {
    scan.skipped
        .iter()
        .find(|s| s.path.file_name().unwrap() == name)
        .unwrap_or_else(|| panic!("{name} was dropped instead of recorded"))
        .reason
}

#[test]
fn server_and_client_standard_layouts_create_all_expected_folders_and_reject_bad_level_name() {
    let scratch = Scratch::new("layout");
    let server = scratch.path("server");
    std::fs::create_dir_all(&server).unwrap();
    std::fs::write(server.join(EULA_FILE), b"# agreed\neula=true\n").unwrap();
    std::fs::write(
        server.join("server.properties"),
        b"level-name=world\nmotd=keep me\n",
    )
    .unwrap();

    // All seven dedicated-server folders are created in one pass.
    let sd = ServerDirectory::load_or_init(&server, TargetVersion::V1_21_1).unwrap();
    let report = sd.ensure_standard_folders().unwrap();
    assert_eq!(report.entries.len(), 7);
    for name in [
        "mods",
        "config",
        "defaultconfigs",
        "logs",
        "crash-reports",
        "backups",
    ] {
        assert!(server.join(name).is_dir(), "{name} missing");
    }
    assert!(server.join("world").join("datapacks").is_dir());
    assert!(report
        .entries
        .iter()
        .all(|e| e.outcome == FolderOutcome::Created));

    // Idempotent: a second pass creates nothing and reports it.
    let again = sd.ensure_standard_folders().unwrap();
    assert!(again
        .entries
        .iter()
        .all(|e| e.outcome == FolderOutcome::AlreadyPresent));

    // The operator's own files were never clobbered.
    assert_eq!(
        std::fs::read(server.join(EULA_FILE)).unwrap(),
        b"# agreed\neula=true\n"
    );
    assert_eq!(
        std::fs::read(server.join("server.properties")).unwrap(),
        b"level-name=world\nmotd=keep me\n"
    );

    // A level-name that escapes the server directory fails closed.
    for bad in ["../escape", "/abs/escape", "world/../../etc", ".."] {
        std::fs::write(
            server.join("server.properties"),
            format!("level-name={bad}\n"),
        )
        .unwrap();
        let sd = ServerDirectory::load_or_init(&server, TargetVersion::V1_21_1).unwrap();
        assert!(sd.ensure_standard_folders().is_err(), "{bad} must fail closed");
        assert!(ensure_server_game_dir_layout(&server).is_err(), "{bad}");
    }
    std::fs::write(server.join("server.properties"), b"level-name=world\n").unwrap();

    // A symlink at any target folder path is refused outright.
    #[cfg(unix)]
    {
        let outside = scratch.path("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let evil = scratch.path("evil-server");
        std::fs::create_dir_all(&evil).unwrap();
        std::os::unix::fs::symlink(&outside, evil.join("mods")).unwrap();
        assert!(ensure_server_game_dir_layout(&evil).is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }

    // The client game directory mirrors .minecraft / Prism / Modrinth.
    let client = scratch.path("game-dir");
    let report = ensure_client_game_dir_layout(&client).unwrap();
    assert_eq!(report.entries.len(), 8);
    for name in [
        "mods",
        "config",
        "resourcepacks",
        "shaderpacks",
        "saves",
        "screenshots",
        "logs",
        "crash-reports",
    ] {
        assert!(client.join(name).is_dir(), "{name} missing");
    }
    let again = ensure_client_game_dir_layout(&client).unwrap();
    assert!(again
        .entries
        .iter()
        .all(|e| e.outcome == FolderOutcome::AlreadyPresent));
}
