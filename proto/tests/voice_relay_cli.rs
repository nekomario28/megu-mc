// SPDX-License-Identifier: LGPL-3.0-or-later
//! End-to-end tests for the `megu-voice-relay` binary: real process
//! invocations asserting exit codes (`0` help/self-test/bench, `2` bad args
//! or a refused public bind) and the machine-readable benchmark report.

use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_megu-voice-relay");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {BIN}: {e}"))
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

/// Pull `"key":<number>` out of the JSON report. The proto crate is
/// zero-dependency, so no JSON parser is available in tests either; a
/// bounded substring scan is enough for a shape the binary emits itself.
fn json_number(doc: &str, key: &str) -> f64 {
    let needle = format!("\"{key}\":");
    let pos = doc
        .find(&needle)
        .unwrap_or_else(|| panic!("key `{key}` missing from {doc}"));
    let tail = &doc[pos + needle.len()..];
    let end = tail
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(tail.len());
    tail[..end]
        .parse()
        .unwrap_or_else(|_| panic!("key `{key}` has a non-numeric value in {doc}"))
}

#[test]
fn help_exits_zero() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert_eq!(code(&out), 0, "{flag}: {}", stderr(&out));
        let text = stdout(&out);
        for needle in ["--bind", "--bench-packets", "--self-test", "--allow-public-bind"] {
            assert!(text.contains(needle), "help lacks {needle}:\n{text}");
        }
    }
}

#[test]
fn bad_args_exit_two() {
    for args in [
        vec!["--no-such-option"],
        vec!["positional"],
        vec!["--bind"],
        vec!["--bind", "not-an-addr"],
        vec!["--max-distance", "banana"],
        vec!["--max-distance", "-1.5"],
        vec!["--whisper-distance", "0"],
        vec!["--max-pps", "0"],
        vec!["--bench-packets", "0"],
    ] {
        let out = run(&args);
        assert_eq!(code(&out), 2, "{args:?} should be a usage error");
    }
}

#[test]
fn public_bind_requires_explicit_opt_in() {
    let out = run(&["--bind", "0.0.0.0:24454"]);
    assert_eq!(code(&out), 2, "public bind must be refused");
    assert!(
        stderr(&out).contains("--allow-public-bind"),
        "refusal should name the opt-in flag: {}",
        stderr(&out)
    );

    // The opt-in itself is accepted: --self-test exits 0 before serving.
    let out = run(&["--bind", "0.0.0.0:0", "--allow-public-bind", "--self-test"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn self_test_exits_zero() {
    let out = run(&["--self-test"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("PASS"), "{}", stdout(&out));
}

#[test]
fn bench_json_reports_counts_throughput_and_latency() {
    let out = run(&["--bench-packets", "200", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = stdout(&out);
    let trimmed = doc.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "not a JSON object: {doc}"
    );
    assert!(trimmed.contains("\"latency_ns\":{"), "{doc}");
    assert_eq!(json_number(trimmed, "frames"), 200.0);
    assert_eq!(json_number(trimmed, "routed"), 200.0);
    assert_eq!(json_number(trimmed, "players"), 32.0);
    assert_eq!(json_number(trimmed, "dimensions"), 3.0);
    assert!(json_number(trimmed, "deliveries") > 0.0, "{doc}");
    assert!(json_number(trimmed, "packets_per_second") > 0.0, "{doc}");
    let p50 = json_number(trimmed, "p50");
    let p99 = json_number(trimmed, "p99");
    let max = json_number(trimmed, "max");
    assert!(p50 <= p99 && p99 <= max, "p50={p50} p99={p99} max={max}");
}

#[test]
fn bench_human_output_mentions_throughput() {
    let out = run(&["--bench-packets", "100"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    for needle in ["throughput", "packets/s", "p50", "p99"] {
        assert!(text.contains(needle), "report lacks {needle}:\n{text}");
    }
}
