// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Native Minecraft 1.21.1 (Protocol 767) client executable for mc-spine.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use mc_spine_client::cli::{
    help_text, parse_args, parse_host_port, redact_from_argv, version_info_text, CliConfig,
};
use mc_spine_client::crash_reporter::{
    self, parse_report_receipt, parse_report_url, CrashSubmitPolicy, ReportTransport,
    SessionReportDeduper, StdHttpReportTransport, DEFAULT_REPORT_TIMEOUT, MAX_BODY_BYTES,
};
use mc_spine_client::frame_pacer::FrameBudgetPacer;
use mc_spine_client::friend_client::FriendGuestBridge;
use mc_spine_client::integrated;
use mc_spine_client::protocol::ping_server;
use mc_spine_client::session::ClientSession;
use mc_spine_client::settings::ClientSettings;
use mc_spine_client::sim::run_session;

/// Status ping gives the server ten seconds for the whole exchange —
/// generous on a loaded loopback, short enough for an operator's patience.
const PING_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match parse_args(&raw_args) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("mc-spine-client: {}", redact_from_argv(&err, &raw_args));
            return ExitCode::from(2);
        }
    };

    if cfg.show_help {
        print!("{}", help_text());
        return ExitCode::SUCCESS;
    }

    if cfg.show_version_info {
        println!("{}", version_info_text());
        return ExitCode::SUCCESS;
    }

    let mut cfg = cfg;

    // Resolve player settings: --preset base, settings-file keys, a Vanilla
    // options.txt import, then explicit flags. File problems are warnings,
    // not fatal, unless an explicitly named file cannot be read at all.
    match cfg.resolve_client_settings() {
        Ok(warnings) => {
            for warning in warnings {
                eprintln!("mc-spine-client: {}", cfg.redact(&warning));
            }
        }
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            return ExitCode::from(2);
        }
    }

    // Crash reporting: install the panic hook as early as possible — after
    // settings resolved the crash directory but before any network, server,
    // or session work. The hook only writes local redacted files; whether
    // anything is ever transmitted is governed by `crash_report_mode` and
    // `report_endpoint`.
    let crash_dir = cfg.resolved_crash_reports_dir();
    crash_reporter::install_panic_hook(crash_dir.clone(), crash_reporter::ReportEnv::detect());
    crash_reporter::set_active_stage("startup");

    // `--flush-crash-reports` is an offline-safe recovery command: it runs
    // before any session setup and exits.
    if cfg.flush_crash_reports {
        return flush_crash_reports(&cfg, &crash_dir);
    }

    // Settings write-out paths are settings-only actions: write the file and
    // exit unless a server/render action was also requested.
    if let Some(path) = cfg.write_default_settings.clone() {
        return match ClientSettings::default().write_toml(&path) {
            Ok(()) => {
                println!("wrote default settings to {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("mc-spine-client: {}", cfg.redact(&err));
                ExitCode::FAILURE
            }
        };
    }
    if let Some(path) = cfg.write_settings.clone() {
        match cfg.client_settings.write_toml(&path) {
            Ok(()) => println!("wrote settings to {}", path.display()),
            Err(err) => {
                eprintln!("mc-spine-client: {}", cfg.redact(&err));
                return ExitCode::FAILURE;
            }
        }
        if !cfg.wants_session_action() {
            return ExitCode::SUCCESS;
        }
    }

    // `--ping` is a server action like a session: SRV-aware, --mc-version
    // aware, JSON or human output, then exit.
    if let Some(target) = cfg.ping.clone() {
        crash_reporter::set_active_stage("ping");
        return run_status_ping(&cfg, &target);
    }

    // A local one-connection adapter lets the existing Minecraft Login/Play
    // state machine use FRL1 without replacing its TCP transport internals.
    let friend_bridge = if let Some(code) = cfg.join_code {
        crash_reporter::set_active_stage("friend_relay");
        let relay = cfg
            .relay
            .as_deref()
            .expect("CLI requires relay with join code");
        match FriendGuestBridge::start(relay, &code, &cfg.username, Duration::from_secs(15)) {
            Ok(bridge) => {
                cfg.server = Some(bridge.local_addr().ip().to_string());
                cfg.port = bridge.local_addr().port();
                Some(bridge)
            }
            Err(err) => {
                eprintln!("mc-spine-client: {}", cfg.redact(&err));
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    let endpoint = match cfg.target_endpoint() {
        Ok(ep) => ep,
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            return ExitCode::from(2);
        }
    };

    // Real --quickPlaySingleplayer loopback: launch the integrated mc-spine
    // server on an ephemeral port and keep the guard alive for the session.
    let mut integrated_server = None;
    if endpoint.is_none() {
        if cfg.quick_play_singleplayer.is_some() {
            crash_reporter::set_active_stage("integrated_server");
            match integrated::launch(&cfg) {
                Ok(server) => {
                    cfg.server = Some("127.0.0.1".to_string());
                    cfg.port = server.port;
                    integrated_server = Some(server);
                }
                Err(err) => {
                    eprintln!("mc-spine-client: {}", cfg.redact(&err));
                    return ExitCode::from(2);
                }
            }
        } else {
            eprintln!(
                "mc-spine-client: no server specified (pass --quickPlayMultiplayer <host[:port]>, --quickPlaySingleplayer <world>, or --server <host>; use --help for usage)"
            );
            return ExitCode::from(2);
        }
    }
    let _integrated_server = integrated_server;
    let _friend_bridge = friend_bridge;

    crash_reporter::set_active_stage("play_session");
    // Live sound path: decode->mixer->sink config picked up by
    // ProtocolClient when it connects. `off` leaves the runtime absent;
    // `auto`/`pipewire`/`alsa` fail closed to the null sink when no
    // playback tool exists or the child dies.
    mc_spine_client::audio_out::install_session_audio_config(
        mc_spine_client::audio_out::AudioSessionConfig {
            sink: cfg.audio_sink.clone(),
            assets_dir: cfg.assets_dir.clone(),
            asset_index: cfg.asset_index.clone(),
            game_dir: cfg.game_dir.clone(),
            max_volume_cap: cfg.client_settings.qol.max_volume_cap,
            max_concurrent_same_sound: cfg.client_settings.qol.max_concurrent_same_sound,
            subtitle_path: None,
            subtitles_to_stderr: true,
        },
    );
    let _client_session = ClientSession::new(crash_reporter::ReportEnv::detect(), crash_dir);
    let mut frame_pacer = FrameBudgetPacer::new();
    if cfg.render_png.is_some() {
        frame_pacer.begin_frame();
    }
    match run_session(&cfg) {
        Ok(summary) => {
            if cfg.render_png.is_some() {
                frame_pacer.end_frame((summary.render_ms * 1_000.0).round() as u64);
            }
            if cfg.json_summary {
                println!("{}", summary.to_json());
            } else {
                println!("{}", summary.to_human());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            ExitCode::FAILURE
        }
    }
}

/// `--ping <host[:port]>`: query the server's status in `cfg.mc_version`'s
/// protocol and print either a human summary or `--json` output.
fn run_status_ping(cfg: &CliConfig, target: &str) -> ExitCode {
    let (host, port) = match parse_host_port(target, cfg.port) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            return ExitCode::from(2);
        }
    };
    match ping_server(&host, port, cfg.mc_version, PING_TIMEOUT) {
        Ok(status) => {
            if cfg.json_summary {
                println!("{}", status.to_json());
            } else {
                println!("{}", status.to_human());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            ExitCode::FAILURE
        }
    }
}

/// One queued crash report and its flush verdict for the summary output.
struct FlushEntry {
    fingerprint: String,
    file_name: String,
    status: &'static str,
    detail: String,
}

/// The fingerprint a queued payload file belongs to, or `None` when `name`
/// is not a pending crash payload (`pending-<fingerprint>.json` or a
/// `<fingerprint>.pending.json` bundle).
fn pending_fingerprint(name: &str) -> Option<&str> {
    let fp = if let Some(rest) = name.strip_prefix("pending-") {
        rest.strip_suffix(".json")?
    } else {
        name.strip_suffix(".pending.json")?
    };
    (!fp.is_empty()).then_some(fp)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `--flush-crash-reports`: enumerate queued `pending-*` payloads in
/// `crash_dir`, print a redacted per-report summary, and transmit only
/// what the resolved consent policy allows — under `ask`, only when
/// `--confirm-send` was passed. Successfully relayed payloads are removed
/// from the queue; everything else stays on disk for the next attempt.
/// Returns `FAILURE` only when the crash directory itself is invalid or
/// unreadable — individual report problems are reported, never fatal.
fn flush_crash_reports(cfg: &CliConfig, crash_dir: &Path) -> ExitCode {
    let mut pending: Vec<(String, PathBuf)> = Vec::new();
    match std::fs::read_dir(crash_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if let Some(fingerprint) = pending_fingerprint(name) {
                    pending.push((fingerprint.to_string(), entry.path()));
                }
            }
        }
        Err(err) => {
            // A missing implicit directory just means "no reports yet"; an
            // explicit --crash-reports-dir that cannot be listed, or an
            // existing but unreadable one, is a real failure.
            if cfg.crash_reports_dir.is_some() || crash_dir.exists() {
                eprintln!(
                    "mc-spine-client: cannot read crash reports directory {}: {err}",
                    crash_dir.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }
    pending.sort();

    let policy = cfg.client_settings.crash_report_mode.submit_policy();
    // The TOML parser and the CLI already gate through parse_report_url;
    // re-validate here so a programmatically-broken config cannot reach
    // the transport either.
    let endpoint = match cfg.client_settings.report_endpoint.as_deref() {
        Some(url) => match parse_report_url(url) {
            Ok(_) => Some(url.to_string()),
            Err(err) => {
                eprintln!("mc-spine-client: configured report_endpoint rejected: {err}");
                None
            }
        },
        None => None,
    };

    let deduper = SessionReportDeduper::new();
    let transport = StdHttpReportTransport::new();
    let mut entries = Vec::new();
    let mut sent = 0usize;
    for (fingerprint, path) in pending {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut entry = FlushEntry {
            fingerprint,
            file_name,
            status: "pending",
            detail: String::new(),
        };
        let body = match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    entry.status = "skipped";
                    entry.detail = "payload is not UTF-8".to_string();
                    entries.push(entry);
                    continue;
                }
            },
            Err(err) => {
                entry.status = "error";
                entry.detail = format!("cannot read payload: {err}");
                entries.push(entry);
                continue;
            }
        };
        if body.len() > MAX_BODY_BYTES {
            entry.status = "skipped";
            entry.detail = format!("payload exceeds {MAX_BODY_BYTES}-byte cap");
            entries.push(entry);
            continue;
        }
        // Consent gate — mirrors submit_crash_report's ordering: policy
        // first, endpoint second, transport last.
        match policy {
            CrashSubmitPolicy::Off => {
                entry.status = "blocked";
                entry.detail = "submission disabled (crash_report_mode=off)".to_string();
                entries.push(entry);
                continue;
            }
            CrashSubmitPolicy::Ask if !cfg.confirm_send => {
                entry.detail = "awaiting consent — re-run with --confirm-send".to_string();
                entries.push(entry);
                continue;
            }
            _ => {}
        }
        if !deduper.claim(&entry.fingerprint) {
            entry.status = "skipped";
            entry.detail = "duplicate fingerprint already flushed".to_string();
            entries.push(entry);
            continue;
        }
        let Some(url) = endpoint.as_deref() else {
            entry.detail = "queued — no report endpoint configured".to_string();
            entries.push(entry);
            continue;
        };
        match transport.post_json(url, &body, DEFAULT_REPORT_TIMEOUT) {
            Ok(response) if (200..300).contains(&response.status) => {
                match parse_report_receipt(&response.body) {
                    Ok(receipt) => {
                        let _ = std::fs::remove_file(&path);
                        sent += 1;
                        entry.status = "sent";
                        entry.detail = receipt
                            .issue_url
                            .unwrap_or_else(|| "accepted by relay".to_string());
                    }
                    Err(err) => {
                        entry.status = "failed";
                        entry.detail = format!("relay ack could not be parsed: {err}");
                    }
                }
            }
            Ok(response) => {
                entry.status = "failed";
                entry.detail = format!("relay answered HTTP {}", response.status);
            }
            Err(err) => {
                entry.status = "failed";
                entry.detail = err.to_string();
            }
        }
        entries.push(entry);
    }

    if cfg.json_summary {
        let mut json = format!(
            "{{\"crash_reports_dir\":\"{}\",\"crash_report_mode\":\"{}\",\"endpoint_configured\":{},\"sent\":{sent},\"pending\":[",
            json_escape(&crash_dir.display().to_string()),
            cfg.client_settings.crash_report_mode.as_str(),
            endpoint.is_some(),
        );
        for (index, entry) in entries.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                "{{\"fingerprint\":\"{}\",\"file\":\"{}\",\"status\":\"{}\",\"detail\":\"{}\"}}",
                json_escape(&entry.fingerprint),
                json_escape(&entry.file_name),
                entry.status,
                json_escape(&entry.detail),
            ));
        }
        json.push_str("]}");
        println!("{json}");
    } else {
        println!(
            "crash reports in {} (mode: {}, endpoint: {}):",
            crash_dir.display(),
            cfg.client_settings.crash_report_mode.as_str(),
            if endpoint.is_some() {
                "configured"
            } else {
                "none"
            },
        );
        for entry in &entries {
            println!(
                "  {}  {:<8} {}  {}",
                entry.fingerprint, entry.status, entry.file_name, entry.detail
            );
        }
        println!("{sent} sent, {} kept", entries.len() - sent);
    }
    ExitCode::SUCCESS
}
