// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Exercise the real standalone bootstrapper, including on-disk preservation.

use mc_spine_server_files::json::{self, Json};
use mc_spine_server_files::{check_eula, EulaStatus, OpsList, Whitelist};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mc-spine-bootstrap-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn server(&self) -> PathBuf {
        self.0.join("server")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mc-spine-init"))
        .args(args)
        .output()
        .expect("run mc-spine-init")
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mc-spine-init"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run mc-spine-init")
}

fn text(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn err(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json_output(out: &Output) -> Json {
    assert!(out.status.success(), "{}", err(out));
    json::parse(&text(out)).expect("valid JSON stdout")
}

fn field<'a>(value: &'a Json, key: &str) -> &'a Json {
    value
        .get(key)
        .unwrap_or_else(|| panic!("missing {key} in {value:?}"))
}

fn actions(output: &Json) -> Vec<(&str, &str)> {
    field(output, "actions")
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                field(entry, "path").as_str().unwrap(),
                field(entry, "action").as_str().unwrap(),
            )
        })
        .collect()
}

#[test]
fn fresh_init_creates_layout_and_valid_config_with_unaccepted_eula() {
    let scratch = Scratch::new();
    let server = scratch.server();
    let output = run(&["--dir", server.to_str().unwrap()]);
    assert!(output.status.success(), "{}", err(&output));
    assert!(text(&output).contains("eula=false"));

    for name in ["world", "mods", "datapacks", "backups"] {
        assert!(server.join(name).is_dir(), "{name}");
    }
    let toml = std::fs::read_to_string(server.join("server.toml")).unwrap();
    assert!(toml.contains("mc_version = \"1.21.1\""), "{toml}");
    assert!(toml.contains("port = 25565"), "{toml}");
    assert_eq!(check_eula(&server), EulaStatus::NotAccepted);
    assert!(std::fs::read_to_string(server.join("eula.txt"))
        .unwrap()
        .contains("eula=false"));
    assert!(OpsList::load_from(&server.join("ops.json"))
        .unwrap()
        .entries
        .is_empty());
    assert!(Whitelist::load_from(&server.join("whitelist.json"))
        .unwrap()
        .entries
        .is_empty());
}

#[test]
fn rerunning_without_force_preserves_operator_files_byte_for_byte() {
    let scratch = Scratch::new();
    let server = scratch.server();
    assert!(run(&["--dir", server.to_str().unwrap()]).status.success());
    let originals = [
        ("server.toml", b"# custom\nport = 12500\n".as_slice()),
        ("eula.txt", b"# operator acceptance\neula=true\n".as_slice()),
        ("ops.json", br#"[{"name":"Admin","level":4}]"#.as_slice()),
        (
            "whitelist.json",
            br#"[{"name":"Trusted","uuid":"u1"}]"#.as_slice(),
        ),
    ];
    for (name, bytes) in originals {
        std::fs::write(server.join(name), bytes).unwrap();
    }
    let output = run(&[
        "--dir",
        server.to_str().unwrap(),
        "--port",
        "30000",
        "--json",
    ]);
    let report = json_output(&output);
    assert_eq!(field(&report, "eula_accepted"), &Json::Bool(true));
    for (name, bytes) in originals {
        assert_eq!(std::fs::read(server.join(name)).unwrap(), bytes, "{name}");
        assert!(actions(&report).contains(&(name, "keep")), "{name}");
    }
}

#[test]
fn explicit_eula_acceptance_and_force_have_precise_semantics() {
    let scratch = Scratch::new();
    let server = scratch.server();
    let dir = server.to_str().unwrap();
    assert!(run(&["--dir", dir, "--accept-eula"]).status.success());
    assert_eq!(check_eula(&server), EulaStatus::Accepted);

    // A subsequent run without opt-in cannot silently revoke acceptance.
    assert!(run(&["--dir", dir]).status.success());
    assert_eq!(check_eula(&server), EulaStatus::Accepted);

    // Forced regeneration resets consent unless --accept-eula is explicit.
    assert!(run(&["--dir", dir, "--force"]).status.success());
    assert_eq!(check_eula(&server), EulaStatus::NotAccepted);
    assert!(run(&["--dir", dir, "--force", "--accept-eula"])
        .status
        .success());
    assert_eq!(check_eula(&server), EulaStatus::Accepted);
}

#[test]
fn consent_on_an_existing_file_requires_force_and_preserves_other_data() {
    let scratch = Scratch::new();
    let server = scratch.server();
    let dir = server.to_str().unwrap();
    assert!(run(&["--dir", dir]).status.success());
    assert!(run(&["--dir", dir, "--accept-eula"]).status.success());
    assert_eq!(check_eula(&server), EulaStatus::NotAccepted);

    std::fs::write(server.join("ops.json"), b"malformed JSON").unwrap();
    std::fs::write(server.join("server.properties"), b"# user-owned\n").unwrap();
    std::fs::write(server.join("mods/example.jar"), b"mod bytes").unwrap();
    let output = run(&["--dir", dir, "--force", "--accept-eula", "--json"]);
    let result = json_output(&output);
    assert_eq!(field(&result, "eula_accepted"), &Json::Bool(true));
    assert_eq!(check_eula(&server), EulaStatus::Accepted);
    assert!(OpsList::load_from(&server.join("ops.json"))
        .unwrap()
        .entries
        .is_empty());
    assert_eq!(
        std::fs::read(server.join("server.properties")).unwrap(),
        b"# user-owned\n"
    );
    assert_eq!(
        std::fs::read(server.join("mods/example.jar")).unwrap(),
        b"mod bytes"
    );
}

#[test]
fn target_26_3_and_custom_port_are_recorded() {
    let scratch = Scratch::new();
    let server = scratch.server();
    let result = json_output(&run(&[
        "--dir",
        server.to_str().unwrap(),
        "--mc-version",
        "26.3",
        "--port",
        "25570",
        "--json",
    ]));
    assert_eq!(field(&result, "mc_version"), &Json::String("26.3".into()));
    assert_eq!(field(&result, "port"), &Json::Number(25570.0));
    let contents = std::fs::read_to_string(server.join("server.toml")).unwrap();
    assert!(contents.contains("mc_version = \"26.3\""));
    assert!(contents.contains("port = 25570"));
}

#[test]
fn dry_run_has_no_side_effects_even_with_force_or_acceptance() {
    let scratch = Scratch::new();
    let server = scratch.server();
    let args = [
        "--dir",
        server.to_str().unwrap(),
        "--force",
        "--accept-eula",
        "--dry-run",
        "--json",
    ];
    let first = run(&args);
    let second = run(&args);
    assert_eq!(first.stdout, second.stdout, "JSON must be deterministic");
    assert_eq!(field(&json_output(&first), "dry_run"), &Json::Bool(true));
    assert!(actions(&json_output(&first))
        .iter()
        .all(|(_, action)| *action == "create"));
    assert!(!server.exists());

    // The default directory is resolved from the current working directory.
    let output = run_in(&scratch.0, &["--dry-run", "--json"]);
    assert_eq!(
        field(&json_output(&output), "dir"),
        &Json::String(".".into())
    );
    assert!(!scratch.0.join("server.toml").exists());
}

#[test]
fn json_reports_existing_and_new_actions_in_stable_order() {
    let scratch = Scratch::new();
    let server = scratch.server();
    std::fs::create_dir_all(server.join("mods")).unwrap();
    std::fs::write(server.join("ops.json"), "[]").unwrap();
    let result = json_output(&run(&["--dir", server.to_str().unwrap(), "--json"]));
    assert_eq!(field(&result, "eula_accepted"), &Json::Bool(false));
    assert!(field(&result, "eula_notice")
        .as_str()
        .unwrap()
        .contains("--accept-eula"));
    assert_eq!(
        actions(&result),
        vec![
            ("world", "create"),
            ("mods", "keep"),
            ("datapacks", "create"),
            ("backups", "create"),
            ("server.toml", "create"),
            ("eula.txt", "create"),
            ("ops.json", "keep"),
            ("whitelist.json", "create"),
        ]
    );
}

#[test]
fn invalid_flags_fail_without_creating_anything() {
    let scratch = Scratch::new();
    let server = scratch.server();
    let dir = server.to_str().unwrap();
    for suffix in [
        vec!["--mc-version", "1.20.4"],
        vec!["--mc-version"],
        vec!["--port", "0"],
        vec!["--port", "65536"],
        vec!["--port", "abc"],
        vec!["--port"],
        vec!["--nonsense"],
        vec!["--dir", ""],
        vec!["--json", "--json"],
    ] {
        let mut args = vec!["--dir", dir];
        args.extend_from_slice(&suffix);
        let output = run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(!server.exists(), "{args:?} created files");
    }
    // A missing --dir value must not turn the following flag into a path.
    let output = run_in(&scratch.0, &["--dir", "--force"]);
    assert!(!output.status.success());
    assert!(!scratch.0.join("--force").exists());
    assert!(run(&["--help"]).status.success());
    assert!(run(&["-h"]).status.success());

    // The OS rejects a NUL argument before the process can start.
    let nul = Command::new(env!("CARGO_BIN_EXE_mc-spine-init"))
        .args(["--dir", "nul\0path"])
        .output();
    assert_eq!(nul.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn invalid_targets_and_conflicting_entries_fail_closed() {
    let scratch = Scratch::new();
    let server = scratch.server();
    std::fs::write(&server, b"file instead of directory").unwrap();
    assert!(!run(&["--dir", server.to_str().unwrap(), "--force"])
        .status
        .success());
    assert_eq!(
        std::fs::read(&server).unwrap(),
        b"file instead of directory"
    );

    std::fs::remove_file(&server).unwrap();
    std::fs::create_dir(&server).unwrap();
    std::fs::write(server.join("world"), b"keep me").unwrap();
    assert!(!run(&["--dir", server.to_str().unwrap()]).status.success());
    assert!(!server.join("server.toml").exists());
    assert_eq!(std::fs::read(server.join("world")).unwrap(), b"keep me");
}

#[cfg(unix)]
#[test]
fn symlink_targets_are_rejected_to_prevent_external_writes() {
    let scratch = Scratch::new();
    let server = scratch.server();
    std::fs::create_dir(&server).unwrap();
    let outside = scratch.0.join("outside");
    std::fs::write(&outside, b"outside").unwrap();
    std::os::unix::fs::symlink(&outside, server.join("ops.json")).unwrap();
    assert!(!run(&["--dir", server.to_str().unwrap(), "--force"])
        .status
        .success());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    assert!(!server.join("server.toml").exists());

    // Reject symlinks in ancestors as well as at the target itself.
    let external_dir = scratch.0.join("external-dir");
    std::fs::create_dir(&external_dir).unwrap();
    let linked_dir = scratch.0.join("linked-dir");
    std::os::unix::fs::symlink(&external_dir, &linked_dir).unwrap();
    let nested_server = linked_dir.join("nested-server");
    assert!(!run(&["--dir", nested_server.to_str().unwrap()])
        .status
        .success());
    assert!(!external_dir.join("nested-server").exists());
}
