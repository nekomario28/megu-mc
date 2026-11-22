// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane UX-CRASH-2: end-to-end wiring of the crash reporter into the
//! `mc-spine-client` executable — the `--crash-report-mode` /
//! `--report-endpoint` / `--crash-reports-dir` / `--flush-crash-reports` /
//! `--confirm-send` CLI surface, the `crash_report_mode` / `report_endpoint`
//! TOML keys, and a loopback `/api/report` flush driven through the real
//! binary.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use mc_spine_client::cli::parse_args;
use mc_spine_client::crash_reporter::{
    capture_panic_context, write_pending_payload, ReportEnv, ReportKind,
};
use mc_spine_client::settings::{ClientSettings, CrashReportMode, QualityPreset};

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mc-spine-crash-cli-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_env() -> ReportEnv {
    ReportEnv {
        mc_version: "26.3".to_string(),
        engine_version: "mc-spine-client 0.1.0".to_string(),
        os: "linux-x86_64".to_string(),
        culprit: "create".to_string(),
        subsystem: None,
        stage: None,
        gpu_class: None,
        top_frames: Vec::new(),
        mod_count: None,
    }
}

/// Write a real `megu-report/1` pending payload plus its sibling markdown
/// file into `crash_dir`, returning the fingerprint and payload path.
fn queue_pending_report(crash_dir: &Path, message: &str) -> (String, PathBuf) {
    let report = capture_panic_context(&test_env(), ReportKind::Crash, "panic", message, None);
    let pending = write_pending_payload(crash_dir, &report).unwrap();
    std::fs::write(
        crash_dir.join(format!("crash-{}.md", report.fingerprint)),
        &report.markdown,
    )
    .unwrap();
    (report.fingerprint, pending)
}

// --- CLI parsing -------------------------------------------------------------

#[test]
fn crash_report_mode_flag_parses_all_tokens_and_fails_closed() {
    for (token, want) in [
        ("ask", CrashReportMode::Ask),
        ("auto_crash_only", CrashReportMode::AutoCrashOnly),
        ("off", CrashReportMode::Off),
    ] {
        let cfg = parse_args(&["--crash-report-mode", token]).unwrap();
        assert_eq!(cfg.crash_report_mode, Some(want), "{token}");
    }
    let cfg = parse_args(&["--crash-report-mode=off"]).unwrap();
    assert_eq!(cfg.crash_report_mode, Some(CrashReportMode::Off));

    let err = parse_args(&["--crash-report-mode", "always"]).unwrap_err();
    assert!(err.contains("--crash-report-mode"), "unexpected: {err}");
    assert!(parse_args(&["--crash-report-mode"]).is_err());
}

#[test]
fn report_endpoint_flag_enforces_url_allowlist() {
    let cfg = parse_args(&[
        "--report-endpoint",
        "https://reports.example.com/api/report",
    ])
    .unwrap();
    assert_eq!(
        cfg.report_endpoint.as_deref(),
        Some("https://reports.example.com/api/report")
    );
    // Loopback http is the only plain-http exception.
    let cfg = parse_args(&["--report-endpoint", "http://127.0.0.1:8787/api/report"]).unwrap();
    assert_eq!(
        cfg.report_endpoint.as_deref(),
        Some("http://127.0.0.1:8787/api/report")
    );
    // Anything else is rejected at parse time — never reaches the wire.
    for bad in [
        "http://example.com/api/report",
        "file:///etc/passwd",
        "ftp://127.0.0.1/x",
        "noscheme.example.com/x",
    ] {
        let err = parse_args(&["--report-endpoint", bad]).unwrap_err();
        assert!(
            err.contains("--report-endpoint"),
            "{bad} must be rejected: {err}"
        );
    }
}

#[test]
fn crash_reports_dir_flush_and_confirm_flags_parse() {
    let cfg = parse_args(&[
        "--crash-reports-dir",
        "/tmp/reports",
        "--flush-crash-reports",
        "--confirm-send",
    ])
    .unwrap();
    assert_eq!(
        cfg.crash_reports_dir.as_deref(),
        Some(Path::new("/tmp/reports"))
    );
    assert!(cfg.flush_crash_reports);
    assert!(cfg.confirm_send);

    // --confirm-send without --flush-crash-reports is a parse error.
    let err = parse_args(&["--confirm-send"]).unwrap_err();
    assert!(err.contains("--confirm-send"), "unexpected: {err}");

    // Directory resolution: flag > --gameDir > cwd.
    let cfg = parse_args::<&str>(&[]).unwrap();
    assert_eq!(
        cfg.resolved_crash_reports_dir(),
        PathBuf::from("crash-reports")
    );
    let cfg = parse_args(&["--gameDir", "/games/mc"]).unwrap();
    assert_eq!(
        cfg.resolved_crash_reports_dir(),
        PathBuf::from("/games/mc/crash-reports")
    );
    let cfg = parse_args(&[
        "--gameDir",
        "/games/mc",
        "--crash-reports-dir",
        "/elsewhere",
    ])
    .unwrap();
    assert_eq!(
        cfg.resolved_crash_reports_dir(),
        PathBuf::from("/elsewhere")
    );
}

#[test]
fn crash_flags_fold_into_resolved_settings_without_marking_custom() {
    let dir = scratch_dir("fold");
    let mut cfg = parse_args(&[
        "--crash-report-mode",
        "auto_crash_only",
        "--report-endpoint",
        "http://127.0.0.1:8787/api/report",
    ])
    .unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let s = &cfg.client_settings;
    assert_eq!(s.crash_report_mode, CrashReportMode::AutoCrashOnly);
    assert_eq!(
        s.report_endpoint.as_deref(),
        Some("http://127.0.0.1:8787/api/report")
    );
    // Consent/egress policy is not a graphics-preset dimension.
    assert_eq!(s.preset, QualityPreset::Balanced);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- TOML roundtrip ------------------------------------------------------------

#[test]
fn crash_keys_round_trip_through_toml() {
    let s = ClientSettings::default();
    assert_eq!(s.crash_report_mode, CrashReportMode::Ask);
    assert_eq!(s.report_endpoint, None);
    // The default file declares the mode but configures no egress.
    let text = s.to_toml();
    assert!(text.contains("crash_report_mode = \"ask\""), "{text}");
    assert!(!text.contains("report_endpoint"), "{text}");

    let s = ClientSettings {
        crash_report_mode: CrashReportMode::AutoCrashOnly,
        report_endpoint: Some("https://reports.example.com/api/report".to_owned()),
        ..ClientSettings::default()
    };
    let (back, warnings) = ClientSettings::from_toml(&s.to_toml());
    assert_eq!(back, s);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // apply_toml overlays only the keys the file defines.
    let mut s = ClientSettings::default();
    s.apply_toml("crash_report_mode = \"off\"\n");
    assert_eq!(s.crash_report_mode, CrashReportMode::Off);
    assert_eq!(s.report_endpoint, None);
}

#[test]
fn crash_keys_fail_closed_in_toml() {
    let (s, warnings) = ClientSettings::from_toml(
        "crash_report_mode = \"always\"\nreport_endpoint = \"http://example.com/api/report\"\n",
    );
    // Invalid mode and unsafe URL are refused: defaults hold, warnings name
    // both keys, and the endpoint stays unset (no egress).
    assert_eq!(s.crash_report_mode, CrashReportMode::Ask);
    assert_eq!(s.report_endpoint, None);
    assert!(warnings.iter().any(|w| w.contains("crash_report_mode")));
    assert!(warnings.iter().any(|w| w.contains("report_endpoint")));

    // Loopback http is accepted from TOML just like from the CLI.
    let (s, warnings) =
        ClientSettings::from_toml("report_endpoint = \"http://127.0.0.1:9/api/report\"\n");
    assert_eq!(
        s.report_endpoint.as_deref(),
        Some("http://127.0.0.1:9/api/report")
    );
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // An explicit empty value clears a previously configured endpoint.
    let mut s = ClientSettings {
        report_endpoint: Some("https://reports.example.com/api/report".to_owned()),
        ..ClientSettings::default()
    };
    s.apply_toml("report_endpoint = \"\"\n");
    assert_eq!(s.report_endpoint, None);
}

// --- End-to-end --flush-crash-reports over loopback --------------------------

fn client_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mc-spine-client")
}

struct MockRelay {
    url: String,
    bodies: Arc<Mutex<Vec<String>>>,
}

fn read_request_body(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let content_length = head
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = buf.split_off(head_end);
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    Some(String::from_utf8_lossy(&body).to_string())
}

/// Loopback HTTP server answering up to `expected` requests with the given
/// status/body, mirroring `report.js`'s `{ok,...}` JSON contract. The
/// listener stays bound so an unexpected request is still recorded.
fn spawn_mock_relay(status: u16, body: String, expected: usize) -> MockRelay {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&bodies);
    thread::spawn(move || {
        for _ in 0..expected.max(1) {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let Some(request_body) = read_request_body(&mut stream) else {
                break;
            };
            sink.lock().unwrap().push(request_body);
            let reason = match status {
                200 => "OK",
                400 => "Bad Request",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\n\
                 Content-Type: application/json; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n{body}",
                body.len()
            );
            if stream.write_all(response.as_bytes()).is_err() {
                break;
            }
        }
    });
    MockRelay {
        url: format!("http://127.0.0.1:{port}/api/report"),
        bodies,
    }
}

/// Run the real binary in an empty working directory (no stray
/// `mc-spine-client.toml` can leak in) and return its captured output.
fn run_client(run_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(client_bin())
        .current_dir(run_dir)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn flush_sends_pending_report_over_loopback_when_confirmed() {
    let dir = scratch_dir("flush-send");
    let run_dir = dir.join("run");
    let crash_dir = dir.join("crash-reports");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir_all(&crash_dir).unwrap();
    let (fingerprint, pending_path) =
        queue_pending_report(&crash_dir, "e2e flush panic at /home/alice/world");
    let relay = spawn_mock_relay(
        200,
        format!(
            "{{\"ok\":true,\"fingerprint\":\"{fingerprint}\",\"issue_url\":\"https://github.com/nekomario28/mc-spine/issues/7\",\"duplicate\":false,\"total_reports\":1}}"
        ),
        1,
    );
    let crash_dir_arg = crash_dir.to_str().unwrap().to_string();

    let output = run_client(
        &run_dir,
        &[
            "--flush-crash-reports",
            "--confirm-send",
            "--crash-reports-dir",
            &crash_dir_arg,
            "--report-endpoint",
            &relay.url,
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The relay received exactly one well-formed megu-report/1 payload.
    let bodies = relay.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1, "expected one report on the wire");
    assert!(bodies[0].contains("\"schema\":\"megu-report/1\""));
    assert!(bodies[0].contains("\"kind\":\"crash\""));
    assert!(bodies[0].contains(&format!("\"fingerprint\":\"{fingerprint}\"")));
    drop(bodies);

    // The delivered payload was consumed; the local markdown stays on disk.
    assert!(!pending_path.exists(), "sent payload must be dequeued");
    assert!(crash_dir.join(format!("crash-{fingerprint}.md")).exists());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&fingerprint), "stdout: {stdout}");
    assert!(stdout.contains("sent"), "stdout: {stdout}");
    assert!(stdout.contains("issues/7"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flush_under_ask_without_confirm_sends_nothing() {
    let dir = scratch_dir("flush-ask");
    let run_dir = dir.join("run");
    let crash_dir = dir.join("crash-reports");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir_all(&crash_dir).unwrap();
    let (fingerprint, pending_path) = queue_pending_report(&crash_dir, "unconfirmed flush panic");
    // The listener stays bound so a policy violation would be recorded.
    let relay = spawn_mock_relay(200, "{}".to_string(), 0);
    let crash_dir_arg = crash_dir.to_str().unwrap().to_string();

    let output = run_client(
        &run_dir,
        &[
            "--flush-crash-reports",
            "--crash-reports-dir",
            &crash_dir_arg,
            "--report-endpoint",
            &relay.url,
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Ask mode without --confirm-send: summary printed, nothing sent,
    // payload still queued.
    assert_eq!(
        relay.bodies.lock().unwrap().len(),
        0,
        "Ask mode transmitted without consent"
    );
    assert!(pending_path.exists(), "report must stay queued");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&fingerprint), "stdout: {stdout}");
    assert!(stdout.contains("--confirm-send"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flush_json_summary_and_policy_off() {
    let dir = scratch_dir("flush-json");
    let run_dir = dir.join("run");
    let crash_dir = dir.join("crash-reports");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir_all(&crash_dir).unwrap();
    let (fingerprint, pending_path) = queue_pending_report(&crash_dir, "json summary panic");
    let relay = spawn_mock_relay(200, "{}".to_string(), 0);
    let crash_dir_arg = crash_dir.to_str().unwrap().to_string();

    // crash_report_mode=off blocks transmission even with --confirm-send.
    let output = run_client(
        &run_dir,
        &[
            "--flush-crash-reports",
            "--confirm-send",
            "--json",
            "--crash-report-mode",
            "off",
            "--crash-reports-dir",
            &crash_dir_arg,
            "--report-endpoint",
            &relay.url,
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"crash_report_mode\":\"off\""), "stdout: {stdout}");
    assert!(stdout.contains(&fingerprint), "stdout: {stdout}");
    assert!(stdout.contains("\"status\":\"blocked\""), "stdout: {stdout}");
    assert!(stdout.contains("\"sent\":0"), "stdout: {stdout}");
    assert!(pending_path.exists(), "Off mode must keep the payload");
    assert_eq!(relay.bodies.lock().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flush_missing_explicit_dir_and_unsafe_url_fail_closed() {
    let dir = scratch_dir("flush-fail");
    let run_dir = dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let missing = dir.join("no-such-dir").to_str().unwrap().to_string();

    // An explicitly named crash directory that does not exist is a hard
    // failure (exit code 1).
    let output = run_client(
        &run_dir,
        &["--flush-crash-reports", "--crash-reports-dir", &missing],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // An unsafe report endpoint is rejected at parse time (exit code 2).
    for bad in ["http://example.com/api/report", "file:///etc/passwd"] {
        let output = run_client(
            &run_dir,
            &["--flush-crash-reports", "--report-endpoint", bad],
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{bad} must exit 2: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
