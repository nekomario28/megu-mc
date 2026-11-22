// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane UX-CRASH-1: panic-hook capture, redaction + 4 KiB caps, the
//! `CrashSubmitPolicy` consent gate, loopback `/api/report` relay
//! transport against a mock that mirrors `website/functions/api/report.js`,
//! and the endpoint URL allowlist.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use mc_spine_client::crash_reporter::{
    capture_panic_context, crash_file_path, install_panic_hook, parse_report_url,
    pending_file_path, set_active_stage, submit_crash_report, CrashSubmitPolicy, PanicSite,
    ReportEnv, ReportKind, ReportTransport, SessionReportDeduper, StdHttpReportTransport,
    SubmitOutcome, SubmitRequest, TransportError, TransportResponse, MAX_BODY_BYTES,
    CRASH_MARKDOWN_MAX_BYTES,
};

const TIMEOUT: Duration = Duration::from_secs(5);

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mc-spine-crash-{tag}-{}",
        std::process::id()
    ));
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

fn crash_report(env: &ReportEnv, message: &str) -> mc_spine_client::crash_reporter::CrashReport {
    capture_panic_context(env, ReportKind::Crash, "panic", message, None)
}

fn request<'a>(
    policy: CrashSubmitPolicy,
    url: Option<&'a str>,
    dir: &'a std::path::Path,
    confirmed: bool,
) -> SubmitRequest<'a> {
    SubmitRequest {
        policy,
        report_api_url: url,
        crash_dir: dir,
        user_confirmed: confirmed,
        timeout: TIMEOUT,
    }
}

/// Transport stub counting invocations and replaying a canned result.
struct CountingTransport {
    calls: Arc<AtomicUsize>,
    result: Result<TransportResponse, String>,
}

impl ReportTransport for CountingTransport {
    fn post_json(
        &self,
        url: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<TransportResponse, TransportError> {
        let _ = (url, body, timeout);
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.result {
            Ok(resp) => Ok(resp.clone()),
            Err(e) => Err(TransportError::Io(e.clone())),
        }
    }
}

fn counting_transport(
    result: Result<TransportResponse, String>,
) -> (CountingTransport, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        CountingTransport {
            calls: Arc::clone(&calls),
            result,
        },
        calls,
    )
}

// --- Mock /api/report relay over real loopback TCP -------------------------

struct MockRelay {
    url: String,
    bodies: Arc<Mutex<Vec<String>>>,
}

fn read_request_body(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
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

/// Spawn a loopback HTTP server answering `expected` requests with the
/// given status/body — mirroring `report.js`'s `{ok,...}` JSON contract.
fn spawn_mock_relay(status: u16, body: String, expected: usize) -> MockRelay {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&bodies);
    thread::spawn(move || {
        for _ in 0..expected {
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
                 Cache-Control: no-store\r\n\
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

// --- Redaction & caps -------------------------------------------------------

#[test]
fn capture_redacts_secrets_paths_and_ips() {
    let mut env = test_env();
    env.subsystem = Some("relay 10.0.0.5".to_string());
    env.stage = Some("world-join /home/alice/save".to_string());
    let report = capture_panic_context(
        &env,
        ReportKind::Crash,
        "panic",
        "NPE at /home/alice/mods/x.jar auth Bearer tok_live123 \
         peer 192.168.1.10 rcon.password=hunter2 C:\\Users\\Bob\\x",
        Some(&PanicSite {
            file: "client-app/src/world.rs".to_string(),
            line: 512,
            column: 17,
        }),
    );
    for secret in [
        "alice",
        "Bob",
        "tok_live123",
        "192.168.1.10",
        "10.0.0.5",
        "hunter2",
    ] {
        assert!(
            !report.markdown.contains(secret),
            "markdown leaked {secret:?}: {}",
            report.markdown
        );
        assert!(
            !report.payload_json.contains(secret),
            "payload leaked {secret:?}: {}",
            report.payload_json
        );
    }
    assert!(report.markdown.contains("[REDACTED]"));
    assert!(report.markdown.contains("client-app/src/world.rs:512:17"));
    assert!(report.fingerprint.starts_with("MEGU-E-"));
    assert_eq!(report.fingerprint.len(), "MEGU-E-".len() + 8);
}

#[test]
fn fingerprint_is_stable_across_players() {
    let env = test_env();
    let a = crash_report(&env, "NPE at /home/alice/mods/x.jar");
    let b = crash_report(&env, "NPE at /home/bob/mods/x.jar");
    assert_eq!(a.fingerprint, b.fingerprint);
}

#[test]
fn markdown_and_payload_stay_under_4kib() {
    let mut env = test_env();
    env.top_frames = (0..12).map(|i| format!("frame{i} {}", "f".repeat(400))).collect();
    let report = capture_panic_context(
        &env,
        ReportKind::Crash,
        "panic",
        &"very long detail line with path /home/alice/x ".repeat(500),
        Some(&PanicSite {
            file: "src/sim.rs".to_string(),
            line: 1,
            column: 1,
        }),
    );
    assert!(report.markdown.len() <= CRASH_MARKDOWN_MAX_BYTES);
    assert!(report.markdown.contains("[truncated]"));
    assert!(report.payload_json.len() <= MAX_BODY_BYTES);
    assert!(report.payload_json.contains("\"schema\":\"megu-report/1\""));
}

// --- Consent gate ------------------------------------------------------------

#[test]
fn policy_off_never_touches_the_network() {
    let dir = scratch_dir("off");
    let report = crash_report(&test_env(), "panic for off-policy test");
    let (transport, calls) = counting_transport(Ok(TransportResponse {
        status: 200,
        body: r#"{"ok":true,"issue_url":"https://x/1"}"#.to_string(),
    }));
    let outcome = submit_crash_report(
        &report,
        &request(
            CrashSubmitPolicy::Off,
            Some("https://reports.example.com/api/report"),
            &dir,
            true,
        ),
        &transport,
        &SessionReportDeduper::new(),
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "Off touched transport");
    match outcome {
        SubmitOutcome::Blocked { local_path, reason } => {
            assert!(reason.contains("disabled"));
            assert!(local_path.exists());
            let md = std::fs::read_to_string(&local_path).unwrap();
            assert!(md.contains("## Crash Report `MEGU-E-"));
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[test]
fn policy_ask_refuses_then_sends_on_confirm() {
    let dir = scratch_dir("ask");
    let env = test_env();
    let report = crash_report(&env, "panic for ask-policy test");
    let deduper = SessionReportDeduper::new();
    let (transport, calls) = counting_transport(Ok(TransportResponse {
        status: 200,
        body: r#"{"ok":true,"issue_url":"https://github.com/o/r/issues/3","duplicate":false,"total_reports":1}"#.to_string(),
    }));

    // Refuses without explicit confirmation — and never hits the wire.
    let outcome = submit_crash_report(
        &report,
        &request(
            CrashSubmitPolicy::Ask,
            Some("https://reports.example.com/api/report"),
            &dir,
            false,
        ),
        &transport,
        &deduper,
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    match outcome {
        SubmitOutcome::AwaitingUserConsent {
            local_path,
            preview_markdown,
        } => {
            assert!(local_path.exists());
            assert_eq!(preview_markdown, report.markdown);
        }
        other => panic!("expected AwaitingUserConsent, got {other:?}"),
    }

    // The same report sends once the user confirms.
    let outcome = submit_crash_report(
        &report,
        &request(
            CrashSubmitPolicy::Ask,
            Some("https://reports.example.com/api/report"),
            &dir,
            true,
        ),
        &transport,
        &deduper,
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    match outcome {
        SubmitOutcome::Sent { receipt, .. } => {
            assert_eq!(
                receipt.issue_url.as_deref(),
                Some("https://github.com/o/r/issues/3")
            );
            assert!(!receipt.deduped);
        }
        other => panic!("expected Sent, got {other:?}"),
    }
}

#[test]
fn policy_auto_crash_only_gates_bug_reports() {
    let dir = scratch_dir("auto");
    let env = test_env();
    let deduper = SessionReportDeduper::new();
    let (transport, calls) = counting_transport(Ok(TransportResponse {
        status: 200,
        body: r#"{"ok":true,"issue_url":"https://x/9"}"#.to_string(),
    }));

    // A manual bug report is never sent without confirmation.
    let bug = capture_panic_context(
        &env,
        ReportKind::Bug,
        "manual-report",
        "blocks disappear on chunk reload",
        None,
    );
    let outcome = submit_crash_report(
        &bug,
        &request(
            CrashSubmitPolicy::AutoCrashOnly,
            Some("https://reports.example.com/api/report"),
            &dir,
            false,
        ),
        &transport,
        &deduper,
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(outcome, SubmitOutcome::AwaitingUserConsent { .. }));

    // Confirmed bug reports do go out.
    let outcome = submit_crash_report(
        &bug,
        &request(
            CrashSubmitPolicy::AutoCrashOnly,
            Some("https://reports.example.com/api/report"),
            &dir,
            true,
        ),
        &transport,
        &deduper,
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(outcome, SubmitOutcome::Sent { .. }));
}

// --- Loopback TCP relay (mock matching report.js) ----------------------------

#[test]
fn auto_crash_only_sends_crash_over_loopback_and_parses_receipt() {
    let dir = scratch_dir("send");
    let env = test_env();
    let report = crash_report(&env, "panic over loopback at /home/alice/x");
    let relay = spawn_mock_relay(
        200,
        format!(
            "{{\"ok\":true,\"fingerprint\":\"{fp}\",\"issue_url\":\"https://github.com/nekomario28/mc-spine/issues/42\",\"duplicate\":false,\"total_reports\":1}}",
            fp = report.fingerprint
        ),
        1,
    );

    let outcome = submit_crash_report(
        &report,
        &request(
            CrashSubmitPolicy::AutoCrashOnly,
            Some(relay.url.as_str()),
            &dir,
            false, // crashes send without asking under AutoCrashOnly
        ),
        &StdHttpReportTransport::new(),
        &SessionReportDeduper::new(),
    );

    match outcome {
        SubmitOutcome::Sent {
            local_path,
            receipt,
        } => {
            assert_eq!(
                receipt.issue_url.as_deref(),
                Some("https://github.com/nekomario28/mc-spine/issues/42")
            );
            assert_eq!(receipt.fingerprint.as_deref(), Some(report.fingerprint.as_str()));
            assert!(!receipt.deduped);
            assert_eq!(receipt.total_reports, Some(1));
            let md = std::fs::read_to_string(&local_path).unwrap();
            assert!(md.contains(&report.fingerprint));
        }
        other => panic!("expected Sent, got {other:?}"),
    }

    // The relay saw exactly one body honouring the megu-report/1 contract.
    let bodies = relay.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let wire = &bodies[0];
    assert!(wire.len() <= MAX_BODY_BYTES);
    let fp_field = format!("\"fingerprint\":\"{}\"", report.fingerprint);
    for field in [
        "\"schema\":\"megu-report/1\"",
        "\"kind\":\"crash\"",
        fp_field.as_str(),
        "\"mc_version\":\"26.3\"",
        "\"engine_version\":\"mc-spine-client 0.1.0\"",
        "\"os\":\"linux-x86_64\"",
        "\"summary\":\"",
        "\"culprit\":\"create\"",
    ] {
        assert!(wire.contains(field), "payload missing {field}: {wire}");
    }
}

#[test]
fn http_500_preserves_payload_in_pending_file() {
    let dir = scratch_dir("offline");
    let report = crash_report(&test_env(), "panic for offline queue test");
    let relay = spawn_mock_relay(500, r#"{"ok":false,"error":"GITHUB_TOKEN is not configured"}"#.to_string(), 1);

    let outcome = submit_crash_report(
        &report,
        &request(
            CrashSubmitPolicy::AutoCrashOnly,
            Some(relay.url.as_str()),
            &dir,
            false,
        ),
        &StdHttpReportTransport::new(),
        &SessionReportDeduper::new(),
    );

    match outcome {
        SubmitOutcome::SavedOffline {
            local_path,
            pending_path,
            reason,
        } => {
            assert!(reason.contains("HTTP 500"), "reason: {reason}");
            assert!(local_path.exists());
            let pending = std::fs::read_to_string(&pending_path).unwrap();
            assert_eq!(pending, report.payload_json);
            assert!(pending.contains("\"schema\":\"megu-report/1\""));
        }
        other => panic!("expected SavedOffline, got {other:?}"),
    }
}

#[test]
fn unreachable_endpoint_queues_offline() {
    let dir = scratch_dir("unreachable");
    let report = crash_report(&test_env(), "panic for unreachable test");
    // Bind-then-release a port so nothing listens on it.
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{dead_port}/api/report");
    let outcome = submit_crash_report(
        &report,
        &request(CrashSubmitPolicy::AutoCrashOnly, Some(&url), &dir, false),
        &StdHttpReportTransport::new(),
        &SessionReportDeduper::new(),
    );
    match outcome {
        SubmitOutcome::SavedOffline {
            pending_path, reason, ..
        } => {
            assert!(reason.contains("connect"), "reason: {reason}");
            assert_eq!(
                pending_path,
                pending_file_path(&dir, &report.fingerprint)
            );
            assert!(pending_path.exists());
        }
        other => panic!("expected SavedOffline, got {other:?}"),
    }
}

#[test]
fn missing_and_unsafe_endpoints_still_preserve_the_report() {
    let dir = scratch_dir("no-endpoint");
    let env = test_env();
    let deduper = SessionReportDeduper::new();

    // No URL configured under AutoCrashOnly: no send, but nothing lost.
    let report = crash_report(&env, "panic with no endpoint");
    match submit_crash_report(
        &report,
        &request(CrashSubmitPolicy::AutoCrashOnly, None, &dir, false),
        &StdHttpReportTransport::new(),
        &deduper,
    ) {
        SubmitOutcome::SavedOffline {
            pending_path, reason, ..
        } => {
            assert!(reason.contains("endpoint"), "reason: {reason}");
            assert!(pending_path.exists());
        }
        other => panic!("expected SavedOffline, got {other:?}"),
    }

    // Confirmed Ask with a non-https public URL: also queued, never sent.
    let report = crash_report(&env, "panic with unsafe endpoint");
    match submit_crash_report(
        &report,
        &request(
            CrashSubmitPolicy::Ask,
            Some("http://reports.example.com/api/report"),
            &dir,
            true,
        ),
        &StdHttpReportTransport::new(),
        &deduper,
    ) {
        SubmitOutcome::SavedOffline { reason, .. } => {
            assert!(reason.contains("endpoint"), "reason: {reason}");
        }
        other => panic!("expected SavedOffline, got {other:?}"),
    }
}

#[test]
fn session_deduper_skips_repeat_fingerprints() {
    let dir = scratch_dir("dedup");
    let env = test_env();
    let report = crash_report(&env, "panic for dedup test");
    let relay = spawn_mock_relay(
        200,
        r#"{"ok":true,"issue_url":"https://x/1","duplicate":false,"total_reports":1}"#.to_string(),
        1,
    );
    let deduper = SessionReportDeduper::new();
    assert!(deduper.claim("unrelated-fingerprint"));
    assert!(!deduper.claim("unrelated-fingerprint"));

    let req = |confirmed| {
        request(
            CrashSubmitPolicy::AutoCrashOnly,
            Some(relay.url.as_str()),
            &dir,
            confirmed,
        )
    };
    let first = submit_crash_report(&report, &req(false), &StdHttpReportTransport::new(), &deduper);
    assert!(matches!(first, SubmitOutcome::Sent { .. }));

    // Same fingerprint again: blocked locally, second request never fires.
    let second = submit_crash_report(&report, &req(false), &StdHttpReportTransport::new(), &deduper);
    match second {
        SubmitOutcome::Blocked { reason, .. } => {
            assert!(reason.contains("already submitted"), "reason: {reason}");
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
    assert_eq!(relay.bodies.lock().unwrap().len(), 1);
}

// --- URL allowlist ------------------------------------------------------------

#[test]
fn unsafe_urls_are_rejected() {
    for bad in [
        "file:///etc/passwd",
        "file://C:/Windows/System32",
        "http://public.example.com/api/report",
        "http://10.0.0.8/api/report",
        "http://192.168.1.1/api/report",
        "ftp://127.0.0.1/x",
        "gopher://127.0.0.1/x",
        "javascript:alert(1)",
        "https://user:hunter2@example.com/api/report",
        "https://example.com/x#frag",
        "https://exa mple.com/x",
        "https://example.com/`touch /tmp/pwn`",
        "https://example.com/$(touch /tmp/pwn)",
        "https://example.com/;touch /tmp/pwn",
        "https://example.com/ -O /tmp/pwn --insecure",
        "http://127.0.0.1:0/x",
        "https://例え.jp/api/report",
        "https://[2001:db8::42]/api/report",
        "example.com/no-scheme",
    ] {
        assert!(parse_report_url(bad).is_err(), "must reject {bad:?}");
    }
    assert!(parse_report_url("https://example.com/\r\nX-Injected: y").is_err());
    assert!(parse_report_url("https://example.com/\u{0007}bell").is_err());
    for good in [
        "https://reports.example.com/api/report",
        "https://example.com:8443/api/report?src=client",
        "http://127.0.0.1:8787/api/report",
        "http://localhost:8787/api/report",
        "http://[::1]:8787/api/report",
    ] {
        assert!(parse_report_url(good).is_ok(), "must accept {good:?}");
    }
}

// --- Panic hook ----------------------------------------------------------------

#[test]
fn panic_hook_writes_redacted_file_and_survives_io_failure() {
    // First: an unwritable crash_dir must not make the hook panic. A path
    // under a regular file can never become a directory.
    let blocker = scratch_dir("hook-blocker").join("not-a-dir");
    std::fs::write(&blocker, b"occupied").unwrap();
    let bad_dir = blocker.join("crash-reports");
    install_panic_hook(bad_dir, test_env());
    let caught = std::panic::catch_unwind(|| panic!("first panic on unwritable dir"));
    let payload = caught.unwrap_err();
    assert!(
        payload
            .downcast_ref::<&str>()
            .map(|s| s.contains("unwritable"))
            .unwrap_or(false),
        "hook must propagate the original panic"
    );

    // Second: a writable dir gets a fully redacted crash-<fp>.md. The
    // earlier hook still runs (it writes to the unwritable dir and is
    // ignored), so only this dir gets a file.
    let good_dir = scratch_dir("hook");
    install_panic_hook(good_dir.clone(), test_env());
    set_active_stage("world-join");
    let secret_msg = "second panic at /home/alice/x Bearer tok_zzz 192.168.9.9";
    let _ = std::panic::catch_unwind(|| panic!("{secret_msg}"));

    let files: Vec<PathBuf> = std::fs::read_dir(&good_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("crash-MEGU-E-") && n.ends_with(".md"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(files.len(), 1, "expected one crash file in {files:?}");
    let md = std::fs::read_to_string(&files[0]).unwrap();
    assert!(md.contains("## Crash Report `MEGU-E-"));
    assert!(md.contains("**Stage**: world-join"));
    assert!(md.contains("panic"));
    for secret in ["alice", "tok_zzz", "192.168.9.9"] {
        assert!(!md.contains(secret), "crash file leaked {secret:?}");
    }
}

#[test]
fn local_files_are_fingerprint_addressed() {
    let dir = scratch_dir("names");
    let report = crash_report(&test_env(), "panic for filename test");
    assert_eq!(
        crash_file_path(&dir, &report.fingerprint),
        dir.join(format!("crash-{}.md", report.fingerprint))
    );
    assert_eq!(
        pending_file_path(&dir, &report.fingerprint),
        dir.join(format!("pending-{}.json", report.fingerprint))
    );
}
