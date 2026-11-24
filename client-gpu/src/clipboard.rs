// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Cross-platform OS clipboard through the tools every desktop already
//! ships: `wl-copy`/`wl-paste` on Wayland, `xclip`/`xsel` on X11,
//! `pbcopy`/`pbpaste` on macOS and `clip.exe`/PowerShell on Windows.
//!
//! No clipboard crate links into this binary, so every backend is a
//! bounded [`std::process::Command`] child: stdin/stdout are piped
//! through helper threads and the wait loop kills a wedged tool after
//! [`TOOL_TIMEOUT`] — nothing here can block the event loop or panic on
//! a headless box. When the display server or the tools are missing,
//! [`SystemClipboard::in_memory`] state still round-trips
//! `set_text`/`get_text` cleanly, and [`ClipboardOutcome`] tells the UI
//! status line which store actually holds the text.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Hard cap on clipboard payloads both ways (stdin and stdout): 64 KiB
/// of UTF-8 is far beyond the 58-char `megu-id:` friend IDs, 128-bit
/// guest links, server addresses and seeds this exists for — anything
/// larger is truncated (`set_text`) or refused (`get_text`).
pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;

/// Longest a clipboard tool may run before it is killed and the call
/// falls back to the in-process buffer.
const TOOL_TIMEOUT: Duration = Duration::from_secs(2);

/// Grace period for the reader thread to deliver its buffered output
/// after the child exits successfully.
const OUTPUT_DRAIN: Duration = Duration::from_millis(250);

/// Which store actually accepted a `set_text` — the UI reports this
/// honestly (`"Copied to clipboard"` vs `"Copied (in-app buffer)"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardOutcome {
    /// A real OS clipboard tool accepted the text.
    System,
    /// Only the in-process buffer holds the text (headless, missing
    /// tools, or the system write failed).
    Fallback,
}

/// OS clipboard with a guaranteed in-process fallback. Constructed once
/// per app; the backend probe is environment-only (`WAYLAND_DISPLAY` /
/// `DISPLAY` / `MEGU_CLIPBOARD=off`), tool presence is proven lazily by
/// actually spawning it.
pub struct SystemClipboard {
    backend: Backend,
    /// Last text this process stored, and the buffer reads fall back to
    /// when no system tool answers. Always updated on `set_text`, so a
    /// session that loses its display mid-run keeps working.
    buffer: String,
}

impl std::fmt::Debug for SystemClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemClipboard")
            .field("backend", &self.backend)
            .field("buffer_len", &self.buffer.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    /// `wl-copy` / `wl-paste --no-newline` (`WAYLAND_DISPLAY` set).
    Wayland,
    /// `xclip` / `xsel` on the X11 clipboard selection (`DISPLAY` set).
    X11,
    /// `pbcopy` / `pbpaste`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    MacOs,
    /// `clip.exe` write, PowerShell `Get-Clipboard` read.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Windows,
    /// In-process buffer only.
    None,
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemClipboard {
    /// Autodetect the platform backend from the environment. Never
    /// panics and never spawns a probe: a missing binary simply makes
    /// the first real call fail into [`ClipboardOutcome::Fallback`].
    /// `MEGU_CLIPBOARD=off|none|fallback` forces the in-process buffer
    /// (CI and test hooks).
    pub fn new() -> Self {
        Self {
            backend: detect_backend(),
            buffer: String::new(),
        }
    }

    /// Force the in-process buffer — tests and environments where the
    /// system clipboard must never be touched.
    pub fn in_memory() -> Self {
        Self {
            backend: Backend::None,
            buffer: String::new(),
        }
    }

    /// Human-readable backend label for diagnostics.
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Wayland => "wayland",
            Backend::X11 => "x11",
            Backend::MacOs => "macos",
            Backend::Windows => "windows",
            Backend::None => "fallback",
        }
    }

    /// Store `text` on the OS clipboard when a backend is live, and
    /// always in the in-process buffer. Oversized payloads are truncated
    /// to [`MAX_CLIPBOARD_BYTES`] on a char boundary rather than
    /// refused, so a paste of a wall of text still leaves something
    /// sane on the clipboard.
    pub fn set_text(&mut self, text: &str) -> ClipboardOutcome {
        let text = truncate_utf8(text, MAX_CLIPBOARD_BYTES).to_owned();
        self.buffer = text.clone();
        let system = match self.backend {
            Backend::Wayland => tool_write("wl-copy", &[], text.as_bytes()),
            Backend::X11 => {
                tool_write("xclip", &["-selection", "clipboard"], text.as_bytes())
                    || tool_write("xsel", &["--clipboard", "--input"], text.as_bytes())
            }
            Backend::MacOs => tool_write("pbcopy", &[], text.as_bytes()),
            Backend::Windows => windows_write(&text),
            Backend::None => false,
        };
        if system {
            ClipboardOutcome::System
        } else {
            ClipboardOutcome::Fallback
        }
    }

    /// Read the clipboard. A live system tool wins; on failure (or a
    /// headless backend) the in-process buffer answers, so in-app
    /// copy->paste round-trips everywhere. `None` means nothing is
    /// stored at all.
    pub fn get_text(&mut self) -> Option<String> {
        let system = match self.backend {
            Backend::Wayland => tool_read("wl-paste", &["--no-newline"]),
            Backend::X11 => tool_read("xclip", &["-selection", "clipboard", "-o"])
                .or_else(|| tool_read("xsel", &["--clipboard", "--output"])),
            Backend::MacOs => tool_read("pbpaste", &[]),
            Backend::Windows => windows_read(),
            Backend::None => None,
        };
        if let Some(text) = system {
            if text.len() > MAX_CLIPBOARD_BYTES {
                return None;
            }
            self.buffer = text.clone();
            return Some(text);
        }
        if self.buffer.is_empty() {
            None
        } else {
            Some(self.buffer.clone())
        }
    }
}

/// Pick the backend purely from the environment — never spawns.
fn detect_backend() -> Backend {
    match std::env::var("MEGU_CLIPBOARD") {
        Ok(v) if matches!(v.as_str(), "off" | "none" | "fallback") => {
            return Backend::None;
        }
        _ => {}
    }
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Backend::Wayland
        } else if std::env::var_os("DISPLAY").is_some() {
            Backend::X11
        } else {
            Backend::None
        }
    }
    #[cfg(target_os = "macos")]
    {
        return Backend::MacOs;
    }
    #[cfg(target_os = "windows")]
    {
        return Backend::Windows;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Backend::None
    }
}

/// Truncate `text` to at most `max` bytes on a UTF-8 char boundary.
fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Strip characters a single-line text field cannot hold: every control
/// char (`\r`, `\n`, `\t`, C0/C1) — and for `single_line = false` keep
/// `\n` as the only allowed break (`\r` is normalized away first so a
/// CRLF pair never becomes two newlines).
pub fn sanitize_paste_text(text: &str, single_line: bool) -> String {
    if single_line {
        return text.chars().filter(|c| !c.is_control()).collect();
    }
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}

/// Write `data` to `tool`'s stdin within [`TOOL_TIMEOUT`]. The stdin
/// write rides a helper thread so a child that never drains its pipe
/// cannot wedge the event loop; the wait loop polls `try_wait` and
/// kills the child on expiry. `true` = the tool exited 0.
fn tool_write(tool: &str, args: &[&str], data: &[u8]) -> bool {
    let mut child = match spawn(tool, args, Stdio::piped(), Stdio::null()) {
        Some(child) => child,
        None => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
            let _ = stdin.flush();
            // Dropping `stdin` closes the pipe — the child's EOF.
        });
    }
    wait_success(&mut child)
}

/// Run `tool` and return its stdout (UTF-8 lossy), bounded by
/// [`TOOL_TIMEOUT`] + [`OUTPUT_DRAIN`]. Output beyond
/// [`MAX_CLIPBOARD_BYTES`] is read but reported as over-cap by the
/// caller, which refuses it.
fn tool_read(tool: &str, args: &[&str]) -> Option<String> {
    let mut child = spawn(tool, args, Stdio::null(), Stdio::piped())?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take((MAX_CLIPBOARD_BYTES + 1) as u64)
            .read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    if !wait_success(&mut child) {
        return None;
    }
    let buf = rx.recv_timeout(OUTPUT_DRAIN).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn spawn(tool: &str, args: &[&str], stdin: Stdio, stdout: Stdio) -> Option<Child> {
    Command::new(tool)
        .args(args)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// Poll `try_wait` until the child exits or [`TOOL_TIMEOUT`] expires;
/// on expiry the child is killed and reaped. `true` = exit status 0.
fn wait_success(child: &mut Child) -> bool {
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// `clip.exe` covers the common ASCII payloads (IDs, addresses, seeds)
/// with a fast native write; non-ASCII text goes through PowerShell
/// base64 so `clip`'s OEM-codepage stdin can never mangle CJK.
#[cfg(target_os = "windows")]
fn windows_write(text: &str) -> bool {
    if text.is_ascii() {
        if tool_write("clip", &[], text.as_bytes()) {
            return true;
        }
    }
    powershell_write(text)
}

/// `powershell -NoProfile -NonInteractive -Command <base64 decode ->
/// Set-Clipboard>`: the payload crosses stdin as base64 ASCII, so the
/// console codepage is irrelevant and CJK/emoji round-trip intact.
#[cfg(target_os = "windows")]
fn powershell_write(text: &str) -> bool {
    let script = "[Console]::InputEncoding=[Text.Encoding]::ASCII;\
                  Set-Clipboard -Value ([Text.Encoding]::UTF8.GetString(\
                  [Convert]::FromBase64String([Console]::In.ReadToEnd())))";
    tool_write(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
        base64_encode(text.as_bytes()).as_bytes(),
    )
}

/// `Get-Clipboard` text out as base64 — the same codepage-proof
/// channel [`powershell_write`] uses in reverse.
#[cfg(target_os = "windows")]
fn windows_read() -> Option<String> {
    let script = "$t=Get-Clipboard -Raw; if($null -ne $t){[Console]::Out.Write(\
                  [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($t)))}";
    let out = tool_read(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
    )?;
    let bytes = base64_decode(out.trim()).ok()?;
    if bytes.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(not(target_os = "windows"))]
fn windows_write(_text: &str) -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn windows_read() -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// Minimal RFC 4648 base64 (Windows PowerShell channel) — zero crates.
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "windows", test))]
const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg(any(target_os = "windows", test))]
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(any(target_os = "windows", test))]
fn base64_decode(text: &str) -> Result<Vec<u8>, ()> {
    let mut table = [0xffu8; 256];
    for (i, &c) in B64_ALPHABET.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for &b in text.as_bytes() {
        if b == b'=' {
            break;
        }
        let v = table[b as usize];
        if v == 0xff {
            return Err(());
        }
        acc = (acc << 6) | u32::from(v);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_roundtrip_without_display() {
        let mut clip = SystemClipboard::in_memory();
        assert_eq!(clip.backend_name(), "fallback");
        assert_eq!(clip.get_text(), None);
        assert_eq!(
            clip.set_text("megu-id:ABCDEF-0123456789"),
            ClipboardOutcome::Fallback
        );
        assert_eq!(
            clip.get_text().as_deref(),
            Some("megu-id:ABCDEF-0123456789")
        );
        // Unicode survives the in-process path byte-for-byte.
        assert_eq!(clip.set_text("こんにちは"), ClipboardOutcome::Fallback);
        assert_eq!(clip.get_text().as_deref(), Some("こんにちは"));
    }

    #[test]
    fn oversized_set_text_truncates_on_char_boundary() {
        let mut clip = SystemClipboard::in_memory();
        let mut big = "a".repeat(MAX_CLIPBOARD_BYTES);
        big.push('日'); // 3-byte char straddling the cap
        big.push('b');
        assert_eq!(clip.set_text(&big), ClipboardOutcome::Fallback);
        let stored = clip.get_text().unwrap();
        assert!(stored.len() <= MAX_CLIPBOARD_BYTES);
        assert!(stored.ends_with('a'), "truncated mid-char: {stored:?}");
        assert_eq!(stored.len(), MAX_CLIPBOARD_BYTES);
    }

    #[test]
    fn single_line_paste_strips_controls_and_newlines() {
        assert_eq!(
            sanitize_paste_text("megu.example.com\r\n:25565\ttab\x07bell", true),
            "megu.example.com:25565tabbell"
        );
        // Multi-line keeps \n but normalizes CRLF and drops other controls.
        assert_eq!(
            sanitize_paste_text("line1\r\nline2\rline3\tend", false),
            "line1\nline2\nline3end"
        );
        // Japanese text is untouched.
        assert_eq!(
            sanitize_paste_text("こんにちは世界", true),
            "こんにちは世界"
        );
    }

    #[test]
    fn base64_roundtrip_ascii_and_cjk() {
        for text in [
            "",
            "f",
            "fo",
            "foo",
            "megu-id:XYZ",
            "こんにちは",
            "a".repeat(1024).as_str(),
        ] {
            let enc = base64_encode(text.as_bytes());
            let dec = base64_decode(&enc).unwrap();
            assert_eq!(dec, text.as_bytes(), "roundtrip failed for {text:?}");
        }
        assert!(base64_decode("!!!").is_err());
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
    }
}
