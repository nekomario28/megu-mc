// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_server_files::sd_notify` — zero-dependency systemd `sd_notify`
//! and local control-socket status helpers.
//!
//! Pure `std`, never panics, and strictly fail-silent: a host without
//! `$NOTIFY_SOCKET` (every non-systemd launch, every container, every
//! beginner double-clicking the binary) sees no error and no behavioural
//! change, while a `Type=notify` systemd unit automatically gets
//! `READY=1`, watchdog pings and live `STATUS=` lines.
//!
//! * [`sd_notify`] / [`sd_notify_to_socket`] — a single `SOCK_DGRAM`
//!   datagram to the notification socket systemd passes in
//!   `$NOTIFY_SOCKET`, filesystem-path or Linux abstract (`@name`) form.
//!   Non-blocking and `false` on any failure.
//! * [`notify_ready`], [`notify_stopping`], [`notify_watchdog`] and
//!   [`notify_status`] — the convenience wrappers a server main loop
//!   actually calls; [`format_sd_status`] builds the sanitised status
//!   text.
//! * [`CtlStatusSnapshot`] + [`format_ctl_status_json`] +
//!   [`parse_ctl_line`] — the wire helpers for the local Unix control
//!   socket (`megu ctl`, Waybar, shell scripts) so status queries never
//!   need a TCP RCON port open. [`default_ctl_socket_path`] picks the
//!   canonical socket location under the session runtime dir when one
//!   exists.

use std::path::{Path, PathBuf};

/// Sanitised one-line human-readable status, e.g.
/// `Megu: 20.0 TPS, 3/20 players, RSS 512 MiB`.
///
/// Non-finite or negative TPS collapses to `0.0` and absurd values clamp,
/// so a NaN stall or a garbage metric can never leak `NaN`/`inf` into a
/// systemd status line.
pub fn format_sd_status(tps: f32, players: usize, max_players: usize, rss_mib: Option<u64>) -> String {
    let tps = if tps.is_finite() {
        tps.clamp(0.0, 9_999.0)
    } else {
        0.0
    };
    match rss_mib {
        Some(mib) => format!("Megu: {tps:.1} TPS, {players}/{max_players} players, RSS {mib} MiB"),
        None => format!("Megu: {tps:.1} TPS, {players}/{max_players} players"),
    }
}

/// Send one datagram carrying `payload` to the notification socket at
/// `socket_path`. `true` only when the datagram was handed to the kernel.
///
/// `socket_path` is either a filesystem path (`/run/systemd/notify`) or —
/// on Linux — an abstract socket written `@name` (or with a leading NUL,
/// the on-wire form). Any other Unix returns `false` for abstract names,
/// and every error anywhere returns `false`: notification is best-effort
/// by contract.
#[cfg(unix)]
pub fn sd_notify_to_socket(socket_path: &str, payload: &str) -> bool {
    use std::os::unix::net::UnixDatagram;
    let socket = match UnixDatagram::unbound() {
        Ok(socket) => socket,
        Err(_) => return false,
    };
    let _ = socket.set_nonblocking(true);
    if socket_path.starts_with('@') || socket_path.starts_with('\0') {
        return send_to_abstract(&socket, socket_path, payload.as_bytes());
    }
    socket.send_to(payload.as_bytes(), socket_path).is_ok()
}

/// Non-Unix stub: no notification socket exists, so the call cleanly
/// reports "not delivered".
#[cfg(not(unix))]
pub fn sd_notify_to_socket(_socket_path: &str, _payload: &str) -> bool {
    false
}

/// Linux abstract-socket delivery (`@name` / `\0name` forms).
#[cfg(target_os = "linux")]
fn send_to_abstract(
    socket: &std::os::unix::net::UnixDatagram,
    socket_path: &str,
    payload: &[u8],
) -> bool {
    use std::os::linux::net::SocketAddrExt;
    let name = socket_path.trim_start_matches(['@', '\0']);
    match std::os::unix::net::SocketAddr::from_abstract_name(name) {
        Ok(addr) => socket.send_to_addr(payload, &addr).is_ok(),
        Err(_) => false,
    }
}

/// Abstract sockets are a Linux extension; other Unixes report a clean
/// failure rather than mangling the name into a filesystem path.
#[cfg(all(unix, not(target_os = "linux")))]
fn send_to_abstract(
    _socket: &std::os::unix::net::UnixDatagram,
    _socket_path: &str,
    _payload: &[u8],
) -> bool {
    false
}

/// Send `payload` to `$NOTIFY_SOCKET` when systemd handed us one.
/// Absent, empty or unreachable sockets all return `false` — running
/// outside systemd is the normal case, never an error.
pub fn sd_notify(payload: &str) -> bool {
    match std::env::var_os("NOTIFY_SOCKET") {
        Some(v) if !v.is_empty() => sd_notify_to_socket(&v.to_string_lossy(), payload),
        _ => false,
    }
}

/// Tell systemd the server finished starting (`READY=1`).
pub fn notify_ready() -> bool {
    sd_notify("READY=1")
}

/// Tell systemd the server is shutting down (`STOPPING=1`).
pub fn notify_stopping() -> bool {
    sd_notify("STOPPING=1")
}

/// Answer the `WatchdogSec=` keep-alive ping (`WATCHDOG=1`).
pub fn notify_watchdog() -> bool {
    sd_notify("WATCHDOG=1")
}

/// Publish a live `STATUS=` line (`systemctl status` shows it).
pub fn notify_status(tps: f32, players: usize, max_players: usize, rss_mib: Option<u64>) -> bool {
    sd_notify(&format!(
        "STATUS={}",
        format_sd_status(tps, players, max_players, rss_mib)
    ))
}

/// One status snapshot served over the local control socket.
///
/// Serialised by [`format_ctl_status_json`] into a single deterministic
/// JSON line for `megu ctl`, Waybar `custom/*` modules and shell scripts —
/// no TCP RCON port required.
#[derive(Clone, Debug, PartialEq)]
pub struct CtlStatusSnapshot {
    /// Whether the server is healthy enough to report at all.
    pub ok: bool,
    /// Measured ticks per second (non-finite serialises as `null`).
    pub tps: f32,
    /// Online player count.
    pub players: u32,
    /// Configured player cap.
    pub max_players: u32,
    /// Loaded dimension count.
    pub dimension_count: u32,
    /// Resident memory in MiB when the platform reports one.
    pub rss_mib: Option<u64>,
    /// Message of the day — arbitrary text, always JSON-escaped.
    pub motd: String,
}

/// Serialise `snap` as one canonical JSON object:
/// `{"ok":true,"tps":20.0,"players":3,"max_players":20,"dimension_count":3,"rss_mib":512,"motd":"..."}`.
///
/// Key order is fixed so consumers can string-compare, the MOTD goes
/// through [`megu_core::json::quote_json_string`] for canonical escaping,
/// and non-finite TPS emits `null` (never `NaN`, which is invalid JSON).
pub fn format_ctl_status_json(snap: &CtlStatusSnapshot) -> String {
    let tps = if snap.tps.is_finite() {
        format!("{:?}", snap.tps)
    } else {
        "null".to_owned()
    };
    let rss = snap
        .rss_mib
        .map_or_else(|| "null".to_owned(), |m| m.to_string());
    format!(
        "{{\"ok\":{},\"tps\":{},\"players\":{},\"max_players\":{},\"dimension_count\":{},\"rss_mib\":{},\"motd\":{}}}",
        snap.ok,
        tps,
        snap.players,
        snap.max_players,
        snap.dimension_count,
        rss,
        megu_core::json::quote_json_string(&snap.motd),
    )
}

/// A parsed command line arriving on the control socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CtlCommand {
    /// Report a status snapshot (`status`, `tps`, `ping`).
    Status,
    /// Begin a clean shutdown (`stop`).
    Stop,
    /// Broadcast a chat message (`say <text>`).
    Say(String),
}

/// Parse one control-socket command line.
///
/// Whitespace-trimmed, command name case-insensitive, payload preserved.
/// Empty lines and unknown verbs return `None` so the listener can ignore
/// stray bytes without dropping the connection.
pub fn parse_ctl_line(line: &str) -> Option<CtlCommand> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (verb, rest) = match line.split_once(|c: char| c.is_whitespace()) {
        Some((v, r)) => (v, r.trim()),
        None => (line, ""),
    };
    if verb.eq_ignore_ascii_case("status")
        || verb.eq_ignore_ascii_case("tps")
        || verb.eq_ignore_ascii_case("ping")
    {
        Some(CtlCommand::Status)
    } else if verb.eq_ignore_ascii_case("stop") {
        Some(CtlCommand::Stop)
    } else if verb.eq_ignore_ascii_case("say") && !rest.is_empty() {
        Some(CtlCommand::Say(rest.to_owned()))
    } else {
        None
    }
}

/// The canonical control-socket path.
///
/// With a live session runtime dir (the resolved `$XDG_RUNTIME_DIR/megu`
/// from `megu_core::linux_desktop::MeguPaths`) the socket sits inside it
/// as `ctl.sock` and dies with the login session; otherwise it falls back
/// to `megu-ctl.sock` beside the server's persistent data so bare-metal
/// and service installs still work.
pub fn default_ctl_socket_path(runtime_dir: Option<&Path>, data_dir: &Path) -> PathBuf {
    match runtime_dir {
        Some(dir) => dir.join("ctl.sock"),
        None => data_dir.join("megu-ctl.sock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "megu-sd-notify-test-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn format_sd_status_sanitizes() {
        assert_eq!(
            format_sd_status(20.0, 3, 20, Some(512)),
            "Megu: 20.0 TPS, 3/20 players, RSS 512 MiB"
        );
        assert_eq!(
            format_sd_status(19.94, 0, 100, None),
            "Megu: 19.9 TPS, 0/100 players"
        );
        // NaN/Inf/negative can never leak into the status line.
        assert!(format_sd_status(f32::NAN, 0, 0, None).contains("0.0 TPS"));
        assert!(format_sd_status(f32::INFINITY, 0, 0, None).contains("0.0 TPS"));
        assert!(format_sd_status(-3.5, 0, 0, None).contains("0.0 TPS"));
        assert!(!format_sd_status(f32::NAN, 0, 0, None).contains("NaN"));
    }

    #[cfg(unix)]
    #[test]
    fn socket_delivers_exact_bytes() {
        use std::os::unix::net::UnixDatagram;
        use std::time::Duration;
        let dir = temp_dir("sock");
        let path = dir.join("notify.sock");
        let recv = UnixDatagram::bind(&path).unwrap();
        recv.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        assert!(sd_notify_to_socket(path.to_str().unwrap(), "READY=1"));
        let mut buf = [0u8; 128];
        let n = recv.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");

        assert!(sd_notify_to_socket(
            path.to_str().unwrap(),
            "STATUS=Megu: 20.0 TPS"
        ));
        let n = recv.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"STATUS=Megu: 20.0 TPS");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn missing_socket_returns_false_without_panicking() {
        let dir = temp_dir("missing");
        let absent = dir.join("nope.sock");
        assert!(!sd_notify_to_socket(absent.to_str().unwrap(), "READY=1"));
        // An empty socket name fails quietly too.
        assert!(!sd_notify_to_socket("", "READY=1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sd_notify_without_notify_socket_is_false() {
        // NOTIFY_SOCKET is set by systemd only; on a plain shell launch
        // the wrapper must report "not delivered" and move on.
        if std::env::var_os("NOTIFY_SOCKET").is_none() {
            assert!(!sd_notify("READY=1"));
            assert!(!notify_ready());
            assert!(!notify_watchdog());
        }
    }

    #[test]
    fn ctl_status_json_is_deterministic_and_escapes() {
        let snap = CtlStatusSnapshot {
            ok: true,
            tps: 20.0,
            players: 3,
            max_players: 20,
            dimension_count: 3,
            rss_mib: Some(512),
            motd: "A \"quoted\" \\ MOTD\nwith emoji \u{1f64f}".to_owned(),
        };
        let json = format_ctl_status_json(&snap);
        // Round-trips through the canonical parser: valid, strict JSON.
        let parsed = megu_core::json::parse_json(&json).unwrap();
        let megu_core::json::JsonValue::Object(obj) = &parsed else {
            panic!("expected object, got {json}");
        };
        assert_eq!(obj.len(), 7);
        assert_eq!(
            parsed.get("motd").and_then(|v| v.as_str()),
            Some(snap.motd.as_str())
        );
        assert_eq!(parsed.get("tps").and_then(|v| v.as_f64()), Some(20.0));
        assert_eq!(
            parsed.get("players").and_then(|v| v.as_i64()),
            Some(3)
        );
        assert_eq!(
            parsed.get("rss_mib").and_then(|v| v.as_i64()),
            Some(512)
        );
        // Byte-identical output for identical input (stable key order).
        assert_eq!(format_ctl_status_json(&snap), json);
    }

    #[test]
    fn ctl_status_json_nulls_non_finite_and_missing() {
        for tps in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let json = format_ctl_status_json(&CtlStatusSnapshot {
                ok: false,
                tps,
                players: 0,
                max_players: 0,
                dimension_count: 0,
                rss_mib: None,
                motd: String::new(),
            });
            let parsed = megu_core::json::parse_json(&json).unwrap();
            assert!(matches!(
                parsed.get("tps"),
                Some(megu_core::json::JsonValue::Null)
            ));
            assert!(matches!(
                parsed.get("rss_mib"),
                Some(megu_core::json::JsonValue::Null)
            ));
        }
    }

    #[test]
    fn parse_ctl_line_understands_verbs() {
        assert_eq!(parse_ctl_line("status"), Some(CtlCommand::Status));
        assert_eq!(parse_ctl_line("  TPS \n"), Some(CtlCommand::Status));
        assert_eq!(parse_ctl_line("ping"), Some(CtlCommand::Status));
        assert_eq!(parse_ctl_line("stop"), Some(CtlCommand::Stop));
        assert_eq!(
            parse_ctl_line("say Hello  there"),
            Some(CtlCommand::Say("Hello  there".to_owned()))
        );
        assert_eq!(
            parse_ctl_line("SAY keep case"),
            Some(CtlCommand::Say("keep case".to_owned()))
        );
        for bad in ["", "   ", "say", "explode"] {
            assert_eq!(parse_ctl_line(bad), None, "{bad}");
        }
    }

    #[test]
    fn default_ctl_socket_path_prefers_runtime_dir() {
        let runtime = Path::new("/run/user/1000/megu");
        let data = Path::new("/var/lib/megu");
        assert_eq!(
            default_ctl_socket_path(Some(runtime), data),
            PathBuf::from("/run/user/1000/megu/ctl.sock")
        );
        assert_eq!(
            default_ctl_socket_path(None, data),
            PathBuf::from("/var/lib/megu/megu-ctl.sock")
        );
    }
}
