// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! End-to-end tests for the `mc-spine-init` binary: real process invocations
//! against real server directories, asserting stdout content, on-disk results
//! and exit codes (`0` success, `2` bad args or EULA-not-accepted doctor
//! verdict, `1` corrupt/unreadable files). `rcon.password` must never appear
//! in any output.

use mc_spine_core::version::TargetVersion;
use mc_spine_server_files::json::{self, Json};
use mc_spine_server_files::{
    offline_uuid, AdmissionDecision, ClaimChunkKey, ClaimOwner, ClaimsWorld, EulaStatus,
    MinimapMode, PlayerRadarScope, ServerDirectory, ServerFairPlayPolicy, SubClaimId,
    BANNED_IPS_FILE, BANNED_PLAYERS_FILE, CLAIMS_FILE, EULA_FILE, OPS_FILE, PARTIES_FILE,
    PROPERTIES_FILE, USERCACHE_FILE, WHITELIST_FILE,
};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mc-spine-init"))
}

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-test scratch dir under the system temp dir (TMPDIR-honoring).
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-init-cli-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to launch mc-spine-init")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The JSON of a `--json` run, parsed by the crate's own reader so an
/// invalid document is a hard failure here.
fn json_stdout(out: &Output) -> Json {
    json::parse(&stdout(out)).expect("stdout is not valid JSON")
}

fn get<'a>(doc: &'a Json, key: &str) -> &'a Json {
    doc.get(key).unwrap_or_else(|| panic!("no {key:?} in {doc:?}"))
}

fn str_field<'a>(doc: &'a Json, key: &str) -> &'a str {
    get(doc, key).as_str().unwrap_or_else(|| panic!("{key:?} not a string"))
}

/// Initialize a directory and accept the EULA the operator's way — by
/// editing the file — so admin-command tests start from a ready directory.
fn ready_dir(scratch: &TestDir) -> PathBuf {
    let dir = scratch.path("srv");
    let out = run(&["init", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    scratch.write("srv/eula.txt", b"eula=true\n");
    dir
}

const UUID_A: &str = "00000000-0000-0000-0000-0000000000aa";

#[test]
fn help_and_usage_errors() {
    for args in [&["--help"][..], &["-h"][..], &["help"][..]] {
        let out = run(args);
        assert_eq!(code(&out), 0, "{args:?}: {}", stderr(&out));
        assert!(stdout(&out).contains("USAGE"), "{args:?}");
    }
    let scratch = TestDir::new("usage");
    let dir = scratch.path("srv").to_str().unwrap().to_string();
    let d = dir.as_str();
    for args in [
        &[][..],
        &["bogus"][..],
        &["init"][..],                                     // missing --dir
        &["init", "--dir"][..],                            // --dir without value
        &["init", "--dir", d, "--mc-version", "9.9"][..],
        &["init", "--dir", d, "--port", "abc"][..],
        &["init", "--dir", d, "--port", "70000"][..],
        &["init", "--dir", d, "--online-mode", "maybe"][..],
        &["init", "--dir", d, "--max-players", "-1"][..],
        &["init", "--dir", d, "extra"][..],
        &["doctor"][..],
        &["op", "--dir", d][..],                           // no player
        &["op", "--dir", d, "not a name!"][..],            // invalid name
        &["op", "--dir", d, "Steve", "--level", "5"][..],
        &["op", "--dir", d, "Steve", "--level", "0"][..],
        &["deop", "--dir", d, "A", "B"][..],               // two players
        &["ban", "--dir", d][..],
        &["pardon", "--dir", d][..],
        &["whitelist", "--dir", d][..],                    // no action
        &["whitelist", "--dir", d, "bogus"][..],
        &["whitelist", "--dir", d, "add"][..],             // no player
        &["whitelist", "--dir", d, "on", "extra"][..],
        &["init", "--dir", d, "--wat"][..],                // unknown option
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> out={} err={}", stdout(&out), stderr(&out));
    }
}

#[test]
fn init_fresh_dir_writes_the_full_skeleton() {
    let scratch = TestDir::new("init-fresh");
    let dir = scratch.path("srv");

    let out = run(&["init", "--dir", dir.to_str().unwrap(), "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json_stdout(&out);
    assert_eq!(str_field(&doc, "action"), "init");
    assert_eq!(str_field(&doc, "mc_version"), "26.3"); // default = latest vanilla
    assert_eq!(get(&doc, "eula_accepted"), &Json::Bool(false));
    let created: Vec<&str> = get(&doc, "created")
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j.as_str().unwrap())
        .collect();
    for name in [
        EULA_FILE,
        PROPERTIES_FILE,
        OPS_FILE,
        WHITELIST_FILE,
        BANNED_PLAYERS_FILE,
        BANNED_IPS_FILE,
        USERCACHE_FILE,
    ] {
        assert!(created.contains(&name), "{name} not in {created:?}");
        assert!(dir.join(name).is_file(), "{name} missing on disk");
    }

    // The EULA is written for the operator to accept — never pre-accepted.
    let eula = std::fs::read_to_string(dir.join(EULA_FILE)).unwrap();
    assert!(eula.contains("eula=false"), "{eula}");
    assert!(!eula.contains("eula=true"), "{eula}");

    // server.properties materializes the Vanilla defaults.
    let props = std::fs::read_to_string(dir.join(PROPERTIES_FILE)).unwrap();
    for line in [
        "server-port=25565",
        "motd=A Minecraft Server",
        "max-players=20",
        "online-mode=true",
        "white-list=false",
        "gamemode=survival",
    ] {
        assert!(props.contains(line), "{line} missing from:\n{props}");
    }

    // The admin JSON files are present and parse as empty Vanilla arrays.
    for name in [OPS_FILE, WHITELIST_FILE, BANNED_PLAYERS_FILE, BANNED_IPS_FILE, USERCACHE_FILE] {
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(json::parse(&text).unwrap(), Json::Array(vec![]), "{name}: {text}");
    }

    // A reload through the facade agrees.
    let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
    assert_eq!(sd.eula_status(), EulaStatus::NotAccepted);
    assert_eq!(sd.properties.server_port(), 25565);
}

#[test]
fn init_applies_flags_and_preserves_existing_files() {
    let scratch = TestDir::new("init-preserve");
    let dir = scratch.path("srv");
    std::fs::create_dir_all(&dir).unwrap();
    // An operator's own files: accepted EULA, commented properties, an op.
    scratch.write("srv/eula.txt", b"# agreed\neula=true\n");
    scratch.write(
        "srv/server.properties",
        b"# my server\nserver-port=25560\nmodded-key=keep me\n",
    );
    let ops_bytes = br#"[{"uuid":"u1","name":"Root","level":4,"bypassesPlayerLimit":true}]"#;
    scratch.write("srv/ops.json", ops_bytes);

    let out = run(&[
        "init",
        "--dir",
        dir.to_str().unwrap(),
        "--port",
        "25571",
        "--motd",
        "Fresh Coat",
        "--online-mode",
        "false",
        "--max-players",
        "8",
        "--mc-version",
        "1.21.1",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("target 1.21.1"), "{text}");

    // Explicit flags landed on top of the existing document; comments and
    // unknown keys survived untouched.
    let props = std::fs::read_to_string(dir.join(PROPERTIES_FILE)).unwrap();
    assert!(props.contains("server-port=25571"), "{props}");
    assert!(props.contains("motd=Fresh Coat"), "{props}");
    assert!(props.contains("online-mode=false"), "{props}");
    assert!(props.contains("max-players=8"), "{props}");
    assert!(props.contains("# my server"), "{props}");
    assert!(props.contains("modded-key=keep me"), "{props}");

    // The accepted EULA and the existing ops.json were never clobbered —
    // byte-identical, not just semantically equal.
    assert_eq!(
        std::fs::read(dir.join(EULA_FILE)).unwrap(),
        b"# agreed\neula=true\n"
    );
    assert_eq!(std::fs::read(dir.join(OPS_FILE)).unwrap(), ops_bytes);

    // Missing files were still materialized around the existing ones.
    for name in [WHITELIST_FILE, BANNED_PLAYERS_FILE, BANNED_IPS_FILE, USERCACHE_FILE] {
        assert!(dir.join(name).is_file(), "{name} missing");
    }

    // A bare re-run changes nothing at all (idempotent, no clobber).
    let before: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap())
        })
        .collect();
    let out = run(&["init", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("kept"), "{}", stdout(&out));
    let after: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap())
        })
        .collect();
    let mut before = before;
    before.sort();
    let mut after = after;
    after.sort();
    assert_eq!(before, after, "a flag-less re-init must not rewrite a byte");
}

#[test]
fn doctor_exit_2_until_eula_accepted_then_0() {
    let scratch = TestDir::new("doctor-eula");

    // A directory that does not exist at all: exit 2 with the plain-language
    // action, and doctor stays read-only — it creates nothing.
    let missing = scratch.path("nope");
    let out = run(&["doctor", "--dir", missing.to_str().unwrap()]);
    assert_eq!(code(&out), 2, "{}", stdout(&out));
    assert!(stdout(&out).contains("eula"), "{}", stdout(&out));
    assert!(!missing.exists(), "doctor must not create the directory");

    let dir = ready_dir(&scratch);
    // Reject the EULA again: doctor goes back to exit 2 with the action.
    scratch.write("srv/eula.txt", b"eula=false\n");
    let out = run(&["doctor", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 2, "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("NOT ACCEPTED"), "{text}");
    assert!(text.contains("eula=true"), "{text}");
    assert!(text.contains("NOT READY"), "{text}");

    // The JSON verdict agrees.
    let out = run(&["doctor", "--dir", dir.to_str().unwrap(), "--json"]);
    assert_eq!(code(&out), 2);
    let doc = json_stdout(&out);
    assert_eq!(get(&doc, "ready"), &Json::Bool(false));
    assert_eq!(get(&doc, "exit_code"), &Json::Number(2.0));
    assert_eq!(str_field(get(&doc, "eula"), "status"), "not_accepted");
    assert!(!str_field(get(&doc, "eula"), "action").is_empty());

    // Operator accepts -> ready.
    scratch.write("srv/eula.txt", b"eula=true\n");
    let out = run(&["doctor", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));
    assert!(stdout(&out).contains("READY"), "{}", stdout(&out));

    let out = run(&["doctor", "--dir", dir.to_str().unwrap(), "--json"]);
    assert_eq!(code(&out), 0);
    let doc = json_stdout(&out);
    assert_eq!(get(&doc, "ready"), &Json::Bool(true));
    assert_eq!(str_field(get(&doc, "eula"), "status"), "accepted");
}

#[test]
fn doctor_exit_1_on_corrupt_files() {
    let scratch = TestDir::new("doctor-corrupt");
    let dir = ready_dir(&scratch);

    scratch.write("srv/ops.json", b"not json");
    let out = run(&["doctor", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(stdout(&out).contains("ops.json"), "{}", stdout(&out));
    assert!(stdout(&out).contains("ERROR"), "{}", stdout(&out));

    // JSON mode reports the error per file and keeps exit 1.
    let out = run(&["doctor", "--dir", dir.to_str().unwrap(), "--json"]);
    assert_eq!(code(&out), 1);
    let doc = json_stdout(&out);
    assert_eq!(get(&doc, "ready"), &Json::Bool(false));
    assert_eq!(get(&doc, "exit_code"), &Json::Number(1.0));
    let ops = get(get(&doc, "files"), OPS_FILE);
    assert_eq!(str_field(ops, "status"), "error");
    assert!(!get(&doc, "errors").as_array().unwrap().is_empty());

    // A corrupt usercache is just as fatal (fail closed, never a repaired
    // empty cache reported as healthy).
    scratch.write("srv/ops.json", b"[]");
    scratch.write("srv/usercache.json", b"[{\"uuid\":\"u\"}]");
    let out = run(&["doctor", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
}

#[test]
fn doctor_never_prints_the_rcon_password() {
    let scratch = TestDir::new("doctor-rcon");
    let dir = ready_dir(&scratch);
    let secret = "s3cr3t-rcon-pw-9f4e";
    scratch.write(
        "srv/server.properties",
        format!("server-port=25565\nenable-rcon=true\nrcon.password={secret}\n").as_bytes(),
    );

    for args in [
        &["doctor", "--dir", dir.to_str().unwrap()][..],
        &["doctor", "--dir", dir.to_str().unwrap(), "--json"][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 0, "{args:?}: {}", stdout(&out));
        let text = stdout(&out);
        assert!(!text.contains(secret), "{args:?} leaked the password:\n{text}");
        assert!(text.contains("REDACTED") || text.contains("rcon_password_set"), "{text}");
    }

    // The JSON shape carries only the boolean.
    let out = run(&["doctor", "--dir", dir.to_str().unwrap(), "--json"]);
    let doc = json_stdout(&out);
    let props = get(get(&doc, "files"), PROPERTIES_FILE);
    assert_eq!(get(props, "rcon_password_set"), &Json::Bool(true));
    assert!(props.get("rcon_password").is_none(), "{props:?}");
}

#[test]
fn admin_commands_roundtrip_through_admit_login() {
    let scratch = TestDir::new("admin");
    let dir = ready_dir(&scratch);
    let d = || dir.to_str().unwrap().to_string();
    let eula_before = std::fs::read(dir.join(EULA_FILE)).unwrap();

    // op / whitelist on / whitelist add / ban, each a real process.
    let out = run(&["op", "--dir", &d(), "Admin", "--level", "4", "--bypass-limit"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("level 4"), "{}", stdout(&out));

    let out = run(&["whitelist", "--dir", &d(), "on"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(&["whitelist", "--dir", &d(), "add", "Friend"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let out = run(&["whitelist", "--dir", &d(), "list", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json_stdout(&out);
    assert_eq!(get(&doc, "enabled"), &Json::Bool(true));
    let entries = get(&doc, "entries").as_array().unwrap();
    assert_eq!(str_field(&entries[0], "name"), "Friend");

    let out = run(&["ban", "--dir", &d(), "Griefer", "--reason", "griefing"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // The persisted state answers logins exactly like Vanilla's
    // PlayerList.canPlayerLogin.
    let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
    assert_eq!(sd.ops.op_level_of("admin"), 4);
    assert!(sd.ops.bypasses_player_limit("ADMIN"));
    // Ops bypass the whitelist.
    assert_eq!(
        sd.admit_login("Admin", UUID_A, "1.2.3.4", 0),
        AdmissionDecision::Allowed
    );
    // A whitelisted player joins; a stranger does not.
    assert_eq!(
        sd.admit_login("friend", UUID_A, "1.2.3.4", 0),
        AdmissionDecision::Allowed
    );
    assert_eq!(
        sd.admit_login("Newbie", UUID_A, "1.2.3.4", 0),
        AdmissionDecision::Denied("You are not white-listed on this server!".to_owned())
    );
    // The ban beats the whitelist check entirely.
    let griefer_uuid = offline_uuid("Griefer");
    assert_eq!(
        sd.admit_login("Griefer", &griefer_uuid, "1.2.3.4", 0),
        AdmissionDecision::Denied("You are banned from this server.\nReason: griefing".to_owned())
    );
    // Vanilla ban fields landed in the file.
    let ban = &sd.banned_players.entries[0];
    assert_eq!(ban.expires, "forever");
    assert_eq!(ban.source, "mc-spine-init");
    assert_eq!(ban.uuid, griefer_uuid);

    // pardon -> no longer banned, but the whitelist still gates him.
    let out = run(&["pardon", "--dir", &d(), "Griefer"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
    assert!(sd.banned_players.find("griefer").is_none());
    assert_eq!(
        sd.admit_login("Griefer", &griefer_uuid, "1.2.3.4", 0),
        AdmissionDecision::Denied("You are not white-listed on this server!".to_owned())
    );

    // whitelist remove -> Friend is a stranger now; off -> everyone joins.
    let out = run(&["whitelist", "--dir", &d(), "remove", "Friend"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(&["whitelist", "--dir", &d(), "off"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
    assert!(!sd.properties.white_list());
    for player in ["Newbie", "Griefer", "Friend"] {
        assert_eq!(
            sd.admit_login(player, UUID_A, "1.2.3.4", 0),
            AdmissionDecision::Allowed,
            "{player} should join once the whitelist is off"
        );
    }

    // deop -> the operator entry is gone.
    let out = run(&["deop", "--dir", &d(), "Admin"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
    assert_eq!(sd.ops.op_level_of("Admin"), 0);
    assert!(sd.ops.entries.is_empty());

    // A no-op admin command still exits 0 and says so.
    let out = run(&["deop", "--dir", &d(), "Nobody"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("no change"), "{}", stdout(&out));

    // Through all of it eula.txt was never rewritten.
    assert_eq!(std::fs::read(dir.join(EULA_FILE)).unwrap(), eula_before);
}

#[test]
fn admin_commands_persist_atomically_and_report_json() {
    let scratch = TestDir::new("admin-json");
    let dir = ready_dir(&scratch);
    let d = dir.to_str().unwrap().to_string();

    let out = run(&["op", "--dir", &d, "Mod", "--level", "2", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json_stdout(&out);
    assert_eq!(str_field(&doc, "action"), "op");
    assert_eq!(str_field(&doc, "player"), "Mod");
    assert_eq!(get(&doc, "level"), &Json::Number(2.0));
    assert_eq!(get(&doc, "bypasses_player_limit"), &Json::Bool(false));
    assert_eq!(get(&doc, "changed"), &Json::Bool(true));

    // Every file on disk parses; no atomic-write temp files leaked.
    for name in [OPS_FILE, WHITELIST_FILE, BANNED_PLAYERS_FILE, BANNED_IPS_FILE, USERCACHE_FILE] {
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        json::parse(&text).unwrap_or_else(|e| panic!("{name} is corrupt: {e}"));
    }
    let strays: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(strays.is_empty(), "{strays:?}");

    // Re-op with a lower level: the explicit flag applies, the stored
    // bypass flag is preserved when --bypass-limit is omitted.
    let out = run(&["op", "--dir", &d, "Mod", "--bypass-limit"]);
    assert_eq!(code(&out), 0);
    let out = run(&["op", "--dir", &d, "Mod", "--level", "1"]);
    assert_eq!(code(&out), 0);
    let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
    assert_eq!(sd.ops.op_level_of("mod"), 1);
    assert!(sd.ops.bypasses_player_limit("mod"), "omitted --bypass-limit must keep the stored value");
}

#[test]
fn corrupt_admin_json_fails_the_command_with_exit_1() {
    let scratch = TestDir::new("admin-corrupt");
    let dir = ready_dir(&scratch);
    let d = dir.to_str().unwrap().to_string();

    // A corrupt ops.json must not be silently replaced with an empty list —
    // that would de-op everyone. The command fails closed.
    scratch.write("srv/ops.json", b"{ not an array");
    let out = run(&["op", "--dir", &d, "Steve"]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert_eq!(std::fs::read(dir.join(OPS_FILE)).unwrap(), b"{ not an array");

    let out = run(&["whitelist", "--dir", &d, "list"]);
    assert_eq!(code(&out), 1);
    let out = run(&["deop", "--dir", &d, "Steve"]);
    assert_eq!(code(&out), 1);

    // An init over the broken file also refuses rather than clobbering.
    let out = run(&["init", "--dir", &d]);
    assert_eq!(code(&out), 1);
}

#[test]
fn claims_admin_roundtrip_and_fail_closed() {
    let scratch = TestDir::new("claims");
    let dir = ready_dir(&scratch);
    let d = || dir.to_str().unwrap().to_string();
    let eula_before = std::fs::read(dir.join(EULA_FILE)).unwrap();

    // Bad command lines exit 2: no action, an unknown action, missing or
    // malformed coordinates, an invalid dimension, and misplaced flags.
    for args in [
        &["claims", "--dir", &d()][..],
        &["claims", "--dir", &d(), "bogus"][..],
        &["claims", "--dir", &d(), "list", "extra"][..],
        &["claims", "--dir", &d(), "parties", "extra"][..],
        &["claims", "--dir", &d(), "server-claim"][..],
        &["claims", "--dir", &d(), "server-claim", "minecraft:overworld", "0"][..],
        &["claims", "--dir", &d(), "unclaim", "minecraft:overworld", "0", "0", "extra"][..],
        &["claims", "--dir", &d(), "server-claim", "bad dim", "0", "0"][..],
        &["claims", "--dir", &d(), "server-claim", "minecraft:overworld", "x", "0"][..],
        &["claims", "--dir", &d(), "server-claim", "minecraft:overworld", "99999999999", "0"][..],
        &["claims", "--dir", &d(), "list", "--forceload"][..],
        &["claims", "--dir", &d(), "parties", "--dimension", "overworld"][..],
        &["claims", "--dir", &d(), "unclaim", "minecraft:overworld", "0", "0", "--forceload"][..],
        &["claims", "--dir", &d(), "server-claim", "overworld", "0", "0", "--dimension", "overworld"][..],
        &["claims", "--dir", &d(), "list", "--dimension", "BAD DIM"][..],
        &["claims", "--dir", &d(), "list", "--wat"][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> out={} err={}", stdout(&out), stderr(&out));
    }

    // Unclaiming wilderness is an engine failure, not a bad command line.
    let out = run(&["claims", "--dir", &d(), "unclaim", "minecraft:overworld", "0", "0"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));

    // An empty store lists cleanly in both renderers.
    let out = run(&["claims", "--dir", &d(), "list", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json_stdout(&out);
    assert_eq!(get(&doc, "claim_count"), &Json::Number(0.0));
    assert_eq!(get(&doc, "dimension"), &Json::Null);

    // Server-claim a spawn chunk with a forceload ticket; the dimension is
    // normalized to its canonical spelling in the report and on disk.
    let out = run(&[
        "claims", "--dir", &d(), "server-claim", "overworld", "3", "-2", "--forceload",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("server-claimed minecraft:overworld chunk (3, -2)"), "{text}");
    assert!(text.contains("forceloaded"), "{text}");

    // Both claims files exist; eula.txt was never rewritten.
    assert!(dir.join(CLAIMS_FILE).is_file());
    assert!(dir.join(PARTIES_FILE).is_file());
    assert_eq!(std::fs::read(dir.join(EULA_FILE)).unwrap(), eula_before);

    // `list --json` reports it: owner server, forceload counted.
    let out = run(&["claims", "--dir", &d(), "list", "--json"]);
    assert_eq!(code(&out), 0);
    let doc = json_stdout(&out);
    assert_eq!(get(&doc, "claim_count"), &Json::Number(1.0));
    assert_eq!(get(&doc, "forceload_count"), &Json::Number(1.0));
    let rows = get(&doc, "claims").as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(str_field(&rows[0], "dimension"), "minecraft:overworld");
    assert_eq!(get(&rows[0], "cx"), &Json::Number(3.0));
    assert_eq!(get(&rows[0], "cz"), &Json::Number(-2.0));
    assert_eq!(str_field(&rows[0], "owner"), "server");
    assert_eq!(str_field(&rows[0], "sub_claim"), "main");
    assert_eq!(get(&rows[0], "forceloaded"), &Json::Bool(true));

    // --dimension filters, normalizing the same way the key parser does.
    let out = run(&["claims", "--dir", &d(), "list", "--dimension", "overworld", "--json"]);
    assert_eq!(get(&json_stdout(&out), "claim_count"), &Json::Number(1.0));
    let out = run(&["claims", "--dir", &d(), "list", "--dimension", "the_nether", "--json"]);
    assert_eq!(get(&json_stdout(&out), "claim_count"), &Json::Number(0.0));

    // The engine itself sees the persisted claim: server-owned, force-loaded.
    let world = ClaimsWorld::load_from_dir(&dir).unwrap();
    let key = ClaimChunkKey::new("overworld", 3, -2).unwrap();
    let claim = world.claim_at(&key).unwrap();
    assert_eq!(claim.owner, ClaimOwner::Server);
    assert!(claim.forceloaded);

    // Re-claiming the same chunk fails closed (exit 1), no half-write.
    let out = run(&["claims", "--dir", &d(), "server-claim", "overworld", "3", "-2"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));

    // A second chunk in a different dimension, then the filtered counts.
    let out = run(&["claims", "--dir", &d(), "server-claim", "the_nether", "0", "0"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(&["claims", "--dir", &d(), "list", "--json"]);
    assert_eq!(get(&json_stdout(&out), "claim_count"), &Json::Number(2.0));
    let out = run(&["claims", "--dir", &d(), "list", "--dimension", "overworld", "--json"]);
    assert_eq!(get(&json_stdout(&out), "claim_count"), &Json::Number(1.0));

    // Unclaim drops it as admin, --json report agrees.
    let out = run(&["claims", "--dir", &d(), "unclaim", "overworld", "3", "-2", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json_stdout(&out);
    assert_eq!(str_field(&doc, "subaction"), "unclaim");
    assert_eq!(str_field(&doc, "dimension"), "minecraft:overworld");
    assert_eq!(get(&doc, "removed"), &Json::Bool(true));

    let world = ClaimsWorld::load_from_dir(&dir).unwrap();
    assert!(world.claim_at(&key).is_none());
    assert_eq!(world.claims().len(), 1);

    // Both files still parse; no atomic-write temp files leaked.
    for name in [CLAIMS_FILE, PARTIES_FILE] {
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        json::parse(&text).unwrap_or_else(|e| panic!("{name} is corrupt: {e}"));
    }
    let strays: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(strays.is_empty(), "{strays:?}");
    assert_eq!(std::fs::read(dir.join(EULA_FILE)).unwrap(), eula_before);
}

#[test]
fn claims_listing_reads_player_claims_and_parties() {
    let scratch = TestDir::new("claims-read");
    let dir = scratch.path("srv");
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap().to_string();

    // A store built by the engine itself: a party, two members, a player
    // claim in a named sub-claim — the CLI only reads it.
    let mut world = ClaimsWorld::new();
    world.create_party(UUID_A, "Raiders").unwrap();
    world.invite_player(UUID_A, "00000000-0000-0000-0000-0000000000bb").unwrap();
    world.accept_invite("00000000-0000-0000-0000-0000000000bb", None).unwrap();
    world
        .claim_chunk(UUID_A, ClaimChunkKey::new("overworld", 1, 1).unwrap(), &SubClaimId::main())
        .unwrap();
    world
        .claim_chunk(
            UUID_A,
            ClaimChunkKey::new("aeronautics:space", -4, 7).unwrap(),
            &SubClaimId::parse("farm").unwrap(),
        )
        .unwrap();
    world.save_to_dir(&dir).unwrap();

    // list --json sees both dimensions and the party-derived owner.
    let out = run(&["claims", "--dir", &d, "list", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json_stdout(&out);
    assert_eq!(get(&doc, "claim_count"), &Json::Number(2.0));
    let rows = get(&doc, "claims").as_array().unwrap();
    // Claims iterate in key order: aeronautics:space sorts first.
    assert_eq!(str_field(&rows[0], "dimension"), "aeronautics:space");
    assert_eq!(str_field(&rows[0], "owner"), &format!("player:{UUID_A}"));
    assert_eq!(str_field(&rows[0], "sub_claim"), "farm");
    assert_eq!(str_field(&rows[1], "dimension"), "minecraft:overworld");
    assert_eq!(str_field(&rows[1], "sub_claim"), "main");

    // parties --json lists the party, its ranks and its empty ally/invite
    // sets (the accepted invite left pending_invites).
    let out = run(&["claims", "--dir", &d, "parties", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json_stdout(&out);
    assert_eq!(get(&doc, "party_count"), &Json::Number(1.0));
    let parties = get(&doc, "parties").as_array().unwrap();
    assert_eq!(str_field(&parties[0], "id"), "party-1");
    assert_eq!(str_field(&parties[0], "name"), "Raiders");
    assert_eq!(str_field(&parties[0], "owner"), UUID_A);
    let members = get(&parties[0], "members").as_array().unwrap();
    assert_eq!(members.len(), 2);
    // Members sort by UUID: ...aa (owner) precedes ...bb (member).
    assert_eq!(str_field(&members[0], "uuid"), UUID_A);
    assert_eq!(str_field(&members[0], "rank"), "owner");
    assert_eq!(str_field(&members[1], "rank"), "member");
    assert!(get(&parties[0], "allies").as_array().unwrap().is_empty());
    assert!(get(&parties[0], "invites").as_array().unwrap().is_empty());

    // Read-only commands never rewrite the store.
    let before = std::fs::read(dir.join(CLAIMS_FILE)).unwrap();
    let out = run(&["claims", "--dir", &d, "list"]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("2 claims"), "{}", stdout(&out));
    assert_eq!(std::fs::read(dir.join(CLAIMS_FILE)).unwrap(), before);

    // A corrupt claims store fails closed with exit 1, never a repaired
    // empty listing.
    scratch.write("srv/openpac_claims.json", b"{ definitely not json");
    let out = run(&["claims", "--dir", &d, "list"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    let out = run(&["claims", "--dir", &d, "parties"]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        std::fs::read(dir.join(CLAIMS_FILE)).unwrap(),
        b"{ definitely not json"
    );
}

// ------------------------------------------------------------- fair play

/// The five strictly opt-in `server.properties` keys the `--minimap-mode` /
/// `--player-radar` / `--allow-*` init flags write.
const FAIR_PLAY_KEYS: [&str; 5] = [
    "minimap-mode",
    "allow-cave-map",
    "allow-mob-radar",
    "player-radar-scope",
    "allow-waypoint-teleport",
];

#[test]
fn init_fair_play_flags_are_strictly_opt_in() {
    let scratch = TestDir::new("init-fp-optin");
    let dir = scratch.path("srv");

    let out = run(&["init", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // Without the flags the file carries none of the five keys — minimaps
    // stay fully unrestricted by contract.
    let props = std::fs::read_to_string(dir.join(PROPERTIES_FILE)).unwrap();
    for key in FAIR_PLAY_KEYS {
        assert!(!props.contains(key), "{key} must not be written:\n{props}");
    }
    // And the loaded policy is the permissive default.
    let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
    assert_eq!(sd.properties.fair_play_policy(), ServerFairPlayPolicy::default());
    assert_eq!(
        sd.properties.fair_play_policy().minimap_mode,
        MinimapMode::Unrestricted
    );
    assert_eq!(
        sd.properties.fair_play_policy().player_radar_scope,
        PlayerRadarScope::All
    );
}

#[test]
fn init_fair_play_flags_persist_and_pass_doctor() {
    let scratch = TestDir::new("init-fp");
    let dir = scratch.path("srv");

    let out = run(&[
        "init",
        "--dir",
        dir.to_str().unwrap(),
        "--minimap-mode",
        "fair_play",
        "--player-radar",
        "party_only",
        "--allow-cave-map",
        "false",
        "--allow-mob-radar",
        "false",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = json_stdout(&out);
    let applied = get(&doc, "applied");
    for (key, value) in [
        ("minimap-mode", "fair_play"),
        ("player-radar-scope", "party_only"),
        ("allow-cave-map", "false"),
        ("allow-mob-radar", "false"),
    ] {
        assert_eq!(str_field(applied, key), value, "{applied:?}");
    }

    // Each flag landed as exactly its own key; the unpassed waypoint flag
    // stayed absent.
    let props = std::fs::read_to_string(dir.join(PROPERTIES_FILE)).unwrap();
    for line in [
        "minimap-mode=fair_play",
        "player-radar-scope=party_only",
        "allow-cave-map=false",
        "allow-mob-radar=false",
    ] {
        assert!(props.contains(line), "{line} missing:\n{props}");
    }
    assert!(!props.contains("allow-waypoint-teleport"), "{props}");

    // A reload reads the typed policy; the absent waypoint key defaults
    // permissive.
    let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
    let policy = sd.properties.fair_play_policy();
    assert_eq!(policy.minimap_mode, MinimapMode::FairPlay);
    assert!(!policy.allow_cave_map);
    assert!(!policy.allow_mob_radar);
    assert_eq!(policy.player_radar_scope, PlayerRadarScope::PartyOnly);
    assert!(policy.allow_waypoint_teleport);

    // doctor still passes once the EULA is accepted.
    scratch.write("srv/eula.txt", b"eula=true\n");
    let out = run(&["doctor", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));

    // A flag-less re-init keeps the operator's fair-play keys byte-identical.
    let before = std::fs::read(dir.join(PROPERTIES_FILE)).unwrap();
    let out = run(&["init", "--dir", dir.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.join(PROPERTIES_FILE)).unwrap(), before);
}

#[test]
fn init_fair_play_partial_flags_write_only_their_keys() {
    let scratch = TestDir::new("init-fp-partial");
    let dir = scratch.path("srv");

    // "minimap ON + show party members only" — the friend-server opt-in.
    let out = run(&[
        "init",
        "--dir",
        dir.to_str().unwrap(),
        "--minimap-mode",
        "unrestricted",
        "--player-radar",
        "party_only",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let props = std::fs::read_to_string(dir.join(PROPERTIES_FILE)).unwrap();
    assert!(props.contains("minimap-mode=unrestricted"), "{props}");
    assert!(props.contains("player-radar-scope=party_only"), "{props}");
    for key in ["allow-cave-map", "allow-mob-radar", "allow-waypoint-teleport"] {
        assert!(!props.contains(key), "{key} must not be written:\n{props}");
    }
}

#[test]
fn init_rejects_invalid_fair_play_values() {
    let scratch = TestDir::new("init-fp-bad");
    let d = scratch.path("srv").to_str().unwrap().to_string();
    for args in [
        &["init", "--dir", &d, "--minimap-mode", "bogus"][..],
        &["init", "--dir", &d, "--minimap-mode", "fair-play"][..], // dash, not _
        &["init", "--dir", &d, "--player-radar", "bogus"][..],
        &["init", "--dir", &d, "--player-radar", "party-only"][..],
        &["init", "--dir", &d, "--allow-cave-map", "maybe"][..],
        &["init", "--dir", &d, "--allow-mob-radar", "1"][..],
        &["init", "--dir", &d, "--allow-waypoint-teleport", "yes"][..],
        &["init", "--dir", &d, "--minimap-mode"][..], // missing value
        &["init", "--dir", &d, "--player-radar"][..],
        &["init", "--dir", &d, "--allow-cave-map"][..],
    ] {
        let out = run(args);
        assert_eq!(
            code(&out),
            2,
            "{args:?} -> out={} err={}",
            stdout(&out),
            stderr(&out)
        );
    }

    // The failure names the flag, and nothing was materialized on disk for
    // the rejected invocations.
    let out = run(&["init", "--dir", &d, "--minimap-mode", "bogus"]);
    assert!(stderr(&out).contains("--minimap-mode"), "{}", stderr(&out));
    assert!(!scratch.path("srv").join(PROPERTIES_FILE).exists());
}
