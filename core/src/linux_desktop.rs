// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_core::linux_desktop` — zero-dependency Linux/Unix desktop and
//! launcher integration primitives.
//!
//! Everything here is pure `std`, never panics, and degrades to a clean
//! fallback on any host where the integration source is absent. A complete
//! beginner on Windows, macOS or a launcher-managed install sees no errors
//! and no extra setup; a power user on an XDG/systemd/Omarchy desktop gets
//! first-class native behaviour for free:
//!
//! * [`MeguPaths`] — the XDG + cross-OS path resolver.
//!   [`resolve_paths_with_env`] is the pure, testable core (the OS and the
//!   environment are injected); [`resolve_paths`] binds it to the host.
//!   A launcher `--gameDir` override keeps every megu file inside the
//!   instance folder so Prism/Modrinth/official-launcher users are never
//!   surprised by stray dotfiles.
//! * [`OmarchyThemePalette`] — live Omarchy theme auto-sync.
//!   [`parse_omarchy_colors_toml`] reads Omarchy's `colors.toml` (the
//!   alacritty-style `key = "#rrggbb"` shape) with zero external crates,
//!   and [`detect_omarchy_theme_in`] resolves the active
//!   `omarchy/current/theme` symlink so the client can match the desktop
//!   theme at startup. Missing Omarchy installs return `None` silently.
//! * Beginner-friendly shell helpers: [`folder_open_command`] picks the
//!   right per-OS "show this folder" opener, and
//!   [`try_send_desktop_notification`] fires `notify-send` non-blocking
//!   only when a graphical session actually exists, failing silently in
//!   every headless or helperless environment.

use std::path::{Path, PathBuf};

/// Wayland `app_id` / X11 `WM_CLASS` the GPU client window registers as.
/// Desktop files (`StartupWMClass`) and compositor rules (Hyprland
/// `match:class`) key on this exact string.
pub const MEGU_WAYLAND_APP_ID: &str = "megu-client";

/// Operating system family a path/lookup decision runs against — an
/// explicit parameter so every layout is unit-testable without
/// cross-compiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetOs {
    /// Linux and other freedesktop Unix-likes (XDG directories).
    Linux,
    /// macOS (`~/Library/...` layout).
    MacOs,
    /// Windows (`%APPDATA%`/`%LOCALAPPDATA%` layout).
    Windows,
    /// Anything else — treated like Linux for path purposes.
    Other,
}

/// The OS this build targets.
pub fn host_os() -> TargetOs {
    if cfg!(target_os = "macos") {
        TargetOs::MacOs
    } else if cfg!(target_os = "windows") {
        TargetOs::Windows
    } else if cfg!(unix) {
        TargetOs::Linux
    } else {
        TargetOs::Other
    }
}

/// The resolved directory set every megu component stores under.
///
/// `runtime_dir` is `Some` only where a real per-user runtime directory
/// exists (`$XDG_RUNTIME_DIR` on Linux/Unix); it is meant for sockets and
/// other files that must not outlive the session. `None` on macOS/Windows
/// and on any XDG-less login — callers fall back to `data_dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeguPaths {
    /// User configuration root (`$XDG_CONFIG_HOME/megu`).
    pub config_dir: PathBuf,
    /// Persistent data root (`$XDG_DATA_HOME/megu`).
    pub data_dir: PathBuf,
    /// Disposable cache root (`$XDG_CACHE_HOME/megu`).
    pub cache_dir: PathBuf,
    /// Session-scoped runtime root (`$XDG_RUNTIME_DIR/megu`) when available.
    pub runtime_dir: Option<PathBuf>,
}

/// XDG-aware absolute-path env lookup: per the freedesktop basedir spec a
/// relative or empty value is invalid and must be ignored entirely.
///
/// "Absolute" here is the spec's POSIX rule — the value begins with `/` —
/// not `Path::is_absolute`, which answers for the *host* OS. The resolver
/// models the *target* OS's layout, so a test exercising `TargetOs::Linux`
/// semantics on a Windows host still honours `/`-rooted XDG values.
fn xdg_env_dir<F>(env_get: &F, name: &str) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_get(name)
        .filter(|v| !v.is_empty() && v.starts_with('/'))
        .map(PathBuf::from)
}

/// Any non-empty home-style env value, used as the last-resort anchor.
fn env_home_dir<F>(env_get: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = env_get(key).filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(value));
        }
    }
    None
}

/// Resolve every megu directory for an injected OS and environment.
///
/// `env_get` reads a process environment variable (returning `None` when
/// unset), so tests inject a map and production passes `std::env::var`.
///
/// Layout contract:
///
/// * `game_dir_override = Some(dir)` (Prism/Modrinth/official `--gameDir`):
///   everything the user could care about stays inside the instance —
///   `config` lands in `<dir>/config/megu` beside the other mod configs,
///   data in `<dir>/megu`, cache in `<dir>/megu/cache`. Only
///   `runtime_dir` still follows the session runtime dir, since sockets
///   are not portable data.
/// * Linux/Unix: `$XDG_CONFIG_HOME/megu`, `$XDG_DATA_HOME/megu`,
///   `$XDG_CACHE_HOME/megu`, `$XDG_RUNTIME_DIR/megu`, with the spec's
///   `~/.config` / `~/.local/share` / `~/.cache` fallbacks when a variable
///   is unset, empty or relative.
/// * macOS: `~/Library/Application Support/megu` for config and data,
///   `~/Library/Caches/megu` for cache.
/// * Windows: `%APPDATA%/megu` for config and data, `%LOCALAPPDATA%/megu`
///   for cache.
///
/// A host with no home variable at all degrades to relative paths rather
/// than panicking; every real desktop sets one.
pub fn resolve_paths_with_env<F>(
    os: TargetOs,
    game_dir_override: Option<&Path>,
    env_get: F,
) -> MeguPaths
where
    F: Fn(&str) -> Option<String>,
{
    let runtime_dir = match os {
        TargetOs::Linux | TargetOs::Other => {
            xdg_env_dir(&env_get, "XDG_RUNTIME_DIR").map(|d| d.join("megu"))
        }
        _ => None,
    };
    if let Some(dir) = game_dir_override {
        return MeguPaths {
            config_dir: dir.join("config").join("megu"),
            data_dir: dir.join("megu"),
            cache_dir: dir.join("megu").join("cache"),
            runtime_dir,
        };
    }
    match os {
        TargetOs::MacOs => {
            let home = env_home_dir(&env_get).unwrap_or_default();
            let app_support = home.join("Library").join("Application Support").join("megu");
            MeguPaths {
                config_dir: app_support.clone(),
                data_dir: app_support,
                cache_dir: home.join("Library").join("Caches").join("megu"),
                runtime_dir,
            }
        }
        TargetOs::Windows => {
            let home = env_home_dir(&env_get).unwrap_or_default();
            let roaming = env_get("APPDATA")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData").join("Roaming"));
            let local = env_get("LOCALAPPDATA")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData").join("Local"));
            MeguPaths {
                config_dir: roaming.join("megu"),
                data_dir: roaming.join("megu"),
                cache_dir: local.join("megu"),
                runtime_dir,
            }
        }
        _ => {
            let home = env_home_dir(&env_get).unwrap_or_default();
            MeguPaths {
                config_dir: xdg_env_dir(&env_get, "XDG_CONFIG_HOME")
                    .unwrap_or_else(|| home.join(".config"))
                    .join("megu"),
                data_dir: xdg_env_dir(&env_get, "XDG_DATA_HOME")
                    .unwrap_or_else(|| home.join(".local").join("share"))
                    .join("megu"),
                cache_dir: xdg_env_dir(&env_get, "XDG_CACHE_HOME")
                    .unwrap_or_else(|| home.join(".cache"))
                    .join("megu"),
                runtime_dir,
            }
        }
    }
}

/// Resolve every megu directory for the host OS and live environment.
pub fn resolve_paths(game_dir_override: Option<&Path>) -> MeguPaths {
    resolve_paths_with_env(host_os(), game_dir_override, |k| std::env::var(k).ok())
}

/// A desktop palette resolved from an Omarchy `colors.toml`.
///
/// All colours are guaranteed present: `background` and `foreground` are
/// required for a palette to exist at all, and every accent slot falls
/// back to another parsed colour rather than to a hardcoded guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OmarchyThemePalette {
    /// Theme name resolved from the `omarchy/current/theme` symlink target
    /// (or `theme.name` file); `None` when parsed from raw text.
    pub name: Option<String>,
    /// Window/body background.
    pub background: [u8; 3],
    /// Primary text colour.
    pub foreground: [u8; 3],
    /// Accent colour (links, focus rings, active tabs).
    pub accent: [u8; 3],
    /// Raised surface colour (panels, selections).
    pub surface: [u8; 3],
    /// Error/danger colour.
    pub error: [u8; 3],
    /// Success colour.
    pub success: [u8; 3],
    /// Warning colour.
    pub warning: [u8; 3],
}

/// Parse one `#rrggbb` colour, tolerating `0x` prefixes and the quote
/// styles TOML writers use (`"#1a1b26"`, `'#1a1b26'`).
pub fn parse_hex_rgb(s: &str) -> Option<[u8; 3]> {
    let t = s
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim();
    let hex = t
        .strip_prefix('#')
        .or_else(|| t.strip_prefix("0x"))
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// Strip a TOML `#` comment from a raw line without cutting `#` characters
/// inside a quoted value (hex colours are written `"#rrggbb"`).
fn strip_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    for (i, c) in line.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Parse Omarchy's `colors.toml` into a [`OmarchyThemePalette`].
///
/// The format is the flat/`[colors.*]`-sectioned alacritty-style TOML
/// Omarchy ships under `omarchy/current/theme/colors.toml`: plain
/// `key = "#rrggbb"` assignments where sections carry no meaning for the
/// palette (the same key name in a later section never overrides an
/// earlier one — first assignment wins).
///
/// Required keys are `background` and `foreground`; a file missing either
/// is not a theme and yields `None`. Optional slots resolve as:
/// `accent` = `accent` | `color4` | `color5` | `blue` | foreground,
/// `surface` = `selection_background` | `color0` | `black` | background,
/// `error` = `color1` | `red` | accent,
/// `success` = `color2` | `green` | accent,
/// `warning` = `color3` | `yellow` | accent.
pub fn parse_omarchy_colors_toml(toml_text: &str) -> Option<OmarchyThemePalette> {
    let mut entries: Vec<(&str, [u8; 3])> = Vec::new();
    for raw_line in toml_text.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || entries.iter().any(|(k, _)| *k == key) {
            continue;
        }
        if let Some(rgb) = parse_hex_rgb(value) {
            entries.push((key, rgb));
        }
    }
    let get = |name: &str| entries.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
    let background = get("background")?;
    let foreground = get("foreground")?;
    let accent = get("accent")
        .or_else(|| get("color4"))
        .or_else(|| get("color5"))
        .or_else(|| get("blue"))
        .unwrap_or(foreground);
    Some(OmarchyThemePalette {
        name: None,
        background,
        foreground,
        accent,
        surface: get("selection_background")
            .or_else(|| get("color0"))
            .or_else(|| get("black"))
            .unwrap_or(background),
        error: get("color1").or_else(|| get("red")).unwrap_or(accent),
        success: get("color2").or_else(|| get("green")).unwrap_or(accent),
        warning: get("color3").or_else(|| get("yellow")).unwrap_or(accent),
    })
}

/// Best-effort active-theme name for an `omarchy/current/theme` dir: the
/// symlink target's last component (Omarchy links `current/theme` to
/// `themes/<name>`), then the sibling `theme.name` record.
fn omarchy_theme_name(theme_dir: &Path) -> Option<String> {
    if let Ok(target) = std::fs::read_link(theme_dir) {
        if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    let name_file = theme_dir.parent()?.join("theme.name");
    let name = std::fs::read_to_string(name_file).ok()?;
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Detect the live Omarchy theme under an injected XDG config home.
///
/// Reads `<config_home>/omarchy/current/theme/colors.toml` and resolves
/// the theme name from the `current/theme` symlink target when it is one.
/// Any host without Omarchy — or a partial/removed theme — returns `None`
/// silently.
pub fn detect_omarchy_theme_in(config_home: &Path) -> Option<OmarchyThemePalette> {
    let theme_dir = config_home.join("omarchy").join("current").join("theme");
    let text = std::fs::read_to_string(theme_dir.join("colors.toml")).ok()?;
    let mut palette = parse_omarchy_colors_toml(&text)?;
    palette.name = omarchy_theme_name(&theme_dir);
    Some(palette)
}

/// `(program, args)` that opens `path` in the OS file manager.
///
/// Linux/Unix use `xdg-open`, macOS `open`, Windows `explorer`. The
/// returned program is a bare command name; callers spawn it detached and
/// ignore failures (a missing opener is not an error for a beginner).
pub fn folder_open_command(os: TargetOs, path: &Path) -> (&'static str, Vec<String>) {
    let arg = path.as_os_str().to_string_lossy().into_owned();
    match os {
        TargetOs::Windows => ("explorer", vec![arg]),
        TargetOs::MacOs => ("open", vec![arg]),
        _ => ("xdg-open", vec![arg]),
    }
}

/// Maximum characters kept in a `notify-send` summary line.
pub const MAX_NOTIFY_TITLE_CHARS: usize = 96;
/// Maximum characters kept in a `notify-send` body.
pub const MAX_NOTIFY_BODY_CHARS: usize = 256;

/// Collapse all control/whitespace runs to single spaces and cap the
/// length so a stray newline or megabyte-long body can never break the
/// `notify-send` argument vector or spam the notification daemon.
fn sanitize_notify_text(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    }
}

/// Build the `notify-send` argument vector for one desktop notification:
/// `notify-send -t <expire_ms> <title> <body>` with both free-text fields
/// sanitised to a single bounded line.
pub fn format_notify_send_args(title: &str, body: &str, expire_ms: u32) -> Vec<String> {
    vec![
        "-t".to_owned(),
        expire_ms.to_string(),
        sanitize_notify_text(title, MAX_NOTIFY_TITLE_CHARS),
        sanitize_notify_text(body, MAX_NOTIFY_BODY_CHARS),
    ]
}

/// Fire a desktop notification through `notify-send` without ever
/// blocking or failing the caller.
///
/// Only attempted on a Linux/Unix host that actually advertises a
/// graphical session (`WAYLAND_DISPLAY` or `DISPLAY`); headless servers,
/// Windows/macOS and systems without `notify-send` all return `false`
/// silently. The child is reaped on a helper thread so a finished
/// notifier never lingers as a zombie.
pub fn try_send_desktop_notification(title: &str, body: &str, expire_ms: u32) -> bool {
    if host_os() != TargetOs::Linux {
        return false;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    let mut cmd = std::process::Command::new("notify-send");
    cmd.args(format_notify_send_args(title, body, expire_ms))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            let _ = std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn env_with(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "megu-linux-desktop-test-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn linux_xdg_absolute_paths_are_respected() {
        let env = env_with(&[
            ("HOME", "/home/player"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_DATA_HOME", "/data"),
            ("XDG_CACHE_HOME", "/cache"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        let p = resolve_paths_with_env(TargetOs::Linux, None, env);
        assert_eq!(p.config_dir, PathBuf::from("/cfg/megu"));
        assert_eq!(p.data_dir, PathBuf::from("/data/megu"));
        assert_eq!(p.cache_dir, PathBuf::from("/cache/megu"));
        assert_eq!(p.runtime_dir, Some(PathBuf::from("/run/user/1000/megu")));
    }

    #[test]
    fn linux_relative_and_empty_xdg_values_fall_back() {
        let env = env_with(&[
            ("HOME", "/home/player"),
            ("XDG_CONFIG_HOME", "relative/cfg"),
            ("XDG_DATA_HOME", ""),
            ("XDG_CACHE_HOME", "./cache"),
            ("XDG_RUNTIME_DIR", "relative/run"),
        ]);
        let p = resolve_paths_with_env(TargetOs::Linux, None, env);
        assert_eq!(p.config_dir, PathBuf::from("/home/player/.config/megu"));
        assert_eq!(p.data_dir, PathBuf::from("/home/player/.local/share/megu"));
        assert_eq!(p.cache_dir, PathBuf::from("/home/player/.cache/megu"));
        assert_eq!(p.runtime_dir, None);
    }

    #[test]
    fn game_dir_override_wins_and_stays_inside_the_instance() {
        let env = env_with(&[
            ("HOME", "/home/player"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        let dir = Path::new("/instance/.minecraft");
        let p = resolve_paths_with_env(TargetOs::Linux, Some(dir), env);
        assert_eq!(p.config_dir, dir.join("config").join("megu"));
        assert_eq!(p.data_dir, dir.join("megu"));
        assert_eq!(p.cache_dir, dir.join("megu").join("cache"));
        // The runtime dir is session state, not instance data.
        assert_eq!(p.runtime_dir, Some(PathBuf::from("/run/user/1000/megu")));
        // The override wins on every OS, not just Linux.
        let pw = resolve_paths_with_env(
            TargetOs::Windows,
            Some(dir),
            env_with(&[("APPDATA", "/roaming")]),
        );
        assert_eq!(pw.data_dir, dir.join("megu"));
        assert_eq!(pw.runtime_dir, None);
    }

    #[test]
    fn macos_and_windows_layouts() {
        let mac = resolve_paths_with_env(
            TargetOs::MacOs,
            None,
            env_with(&[("HOME", "/Users/yuu")]),
        );
        let support = PathBuf::from("/Users/yuu/Library/Application Support/megu");
        assert_eq!(mac.config_dir, support);
        assert_eq!(mac.data_dir, support);
        assert_eq!(mac.cache_dir, PathBuf::from("/Users/yuu/Library/Caches/megu"));
        assert_eq!(mac.runtime_dir, None);

        let win = resolve_paths_with_env(
            TargetOs::Windows,
            None,
            env_with(&[
                ("USERPROFILE", "/C/Users/yuu"),
                ("APPDATA", "/C/Users/yuu/AppData/Roaming"),
                ("LOCALAPPDATA", "/C/Users/yuu/AppData/Local"),
            ]),
        );
        assert_eq!(
            win.config_dir,
            PathBuf::from("/C/Users/yuu/AppData/Roaming/megu")
        );
        assert_eq!(
            win.cache_dir,
            PathBuf::from("/C/Users/yuu/AppData/Local/megu")
        );
        // Missing %LOCALAPPDATA% falls back under the profile.
        let win_fallback = resolve_paths_with_env(
            TargetOs::Windows,
            None,
            env_with(&[
                ("USERPROFILE", "/C/Users/yuu"),
                ("APPDATA", "/C/Users/yuu/AppData/Roaming"),
            ]),
        );
        assert_eq!(
            win_fallback.cache_dir,
            PathBuf::from("/C/Users/yuu/AppData/Local/megu")
        );
    }

    #[test]
    fn hex_rgb_parses_all_spellings() {
        assert_eq!(parse_hex_rgb("#1a1b26"), Some([0x1a, 0x1b, 0x26]));
        assert_eq!(parse_hex_rgb("0xC0CAF5"), Some([0xc0, 0xca, 0xf5]));
        assert_eq!(parse_hex_rgb("\"#7aa2f7\""), Some([0x7a, 0xa2, 0xf7]));
        assert_eq!(parse_hex_rgb(" 'f7768e' "), Some([0xf7, 0x76, 0x8e]));
        for bad in ["", "#fff", "#1234567", "zzzzzz", "#12 456", "0x"] {
            assert_eq!(parse_hex_rgb(bad), None, "{bad}");
        }
    }

    const TOKYO_NIGHT_TOML: &str = r##"
# Omarchy Tokyo Night palette
background = "#1a1b26"
foreground = "#c0caf5"
accent = "#7aa2f7"
selection_background = "#33467c"
color0 = "#15161e"
color1 = "#f7768e"
color2 = "#9ece6a"
color3 = "#e0af68"
color4 = "#7aa2f7"
color5 = "#bb9af7"
"##;

    #[test]
    fn omarchy_colors_toml_parses_tokyo_night() {
        let p = parse_omarchy_colors_toml(TOKYO_NIGHT_TOML).unwrap();
        assert_eq!(p.name, None);
        assert_eq!(p.background, [0x1a, 0x1b, 0x26]);
        assert_eq!(p.foreground, [0xc0, 0xca, 0xf5]);
        assert_eq!(p.accent, [0x7a, 0xa2, 0xf7]);
        assert_eq!(p.surface, [0x33, 0x46, 0x7c]);
        assert_eq!(p.error, [0xf7, 0x76, 0x8e]);
        assert_eq!(p.success, [0x9e, 0xce, 0x6a]);
        assert_eq!(p.warning, [0xe0, 0xaf, 0x68]);
    }

    #[test]
    fn omarchy_colors_toml_parses_alacritty_named_sections() {
        let catppuccin = r##"
[colors.primary]
background = "#1e1e2e"
foreground = "#cdd6f4"

[colors.normal]
black = "#45475a"
red = "#f38ba8"
green = "#a6e3a1"
yellow = "#f9e2af"
blue = "#89b4fa"
"##;
        let p = parse_omarchy_colors_toml(catppuccin).unwrap();
        assert_eq!(p.background, [0x1e, 0x1e, 0x2e]);
        assert_eq!(p.accent, [0x89, 0xb4, 0xfa]);
        assert_eq!(p.surface, [0x45, 0x47, 0x5a]);
        assert_eq!(p.error, [0xf3, 0x8b, 0xa8]);
        assert_eq!(p.success, [0xa6, 0xe3, 0xa1]);
        assert_eq!(p.warning, [0xf9, 0xe2, 0xaf]);
    }

    #[test]
    fn omarchy_colors_toml_rejects_garbage() {
        for bad in [
            "",
            "not toml at all {{{",
            "foreground = \"#c0caf5\"",
            "background = \"oops\"",
            "[colors]\nbackground = \"#1a1b26\"",
        ] {
            assert_eq!(parse_omarchy_colors_toml(bad), None, "{bad}");
        }
        // A comment inside a quoted hex string must not truncate the value.
        let with_hash_comment = "background = \"#1a1b26\" # dark\nforeground = '#c0caf5'\n";
        assert!(parse_omarchy_colors_toml(with_hash_comment).is_some());
    }

    #[test]
    fn detect_omarchy_theme_in_absent_returns_none() {
        let dir = temp_dir("absent");
        assert_eq!(detect_omarchy_theme_in(&dir), None);
        // A colors.toml that is not a valid theme is also a silent None.
        let theme = dir.join("omarchy").join("current").join("theme");
        std::fs::create_dir_all(&theme).unwrap();
        std::fs::write(theme.join("colors.toml"), "garbage").unwrap();
        assert_eq!(detect_omarchy_theme_in(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn detect_omarchy_theme_in_reads_symlinked_theme() {
        let dir = temp_dir("symlink");
        let theme_dir = dir.join("omarchy").join("themes").join("tokyo-night");
        std::fs::create_dir_all(&theme_dir).unwrap();
        std::fs::write(theme_dir.join("colors.toml"), TOKYO_NIGHT_TOML).unwrap();
        let current = dir.join("omarchy").join("current");
        std::fs::create_dir_all(&current).unwrap();
        std::os::unix::fs::symlink(&theme_dir, current.join("theme")).unwrap();
        let p = detect_omarchy_theme_in(&dir).unwrap();
        assert_eq!(p.name.as_deref(), Some("tokyo-night"));
        assert_eq!(p.background, [0x1a, 0x1b, 0x26]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detect_omarchy_theme_in_reads_theme_name_file() {
        let dir = temp_dir("namefile");
        let theme_dir = dir.join("omarchy").join("current").join("theme");
        std::fs::create_dir_all(&theme_dir).unwrap();
        std::fs::write(theme_dir.join("colors.toml"), TOKYO_NIGHT_TOML).unwrap();
        std::fs::write(
            theme_dir.parent().unwrap().join("theme.name"),
            "catppuccin-mocha\n",
        )
        .unwrap();
        let p = detect_omarchy_theme_in(&dir).unwrap();
        assert_eq!(p.name.as_deref(), Some("catppuccin-mocha"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folder_open_command_maps_per_os() {
        let dir = Path::new("/srv/world");
        let (prog, args) = folder_open_command(TargetOs::Linux, dir);
        assert_eq!(prog, "xdg-open");
        assert_eq!(args, vec!["/srv/world".to_owned()]);
        let (prog, args) = folder_open_command(TargetOs::MacOs, dir);
        assert_eq!(prog, "open");
        assert_eq!(args, vec!["/srv/world".to_owned()]);
        let (prog, _) = folder_open_command(TargetOs::Windows, dir);
        assert_eq!(prog, "explorer");
        let (prog, _) = folder_open_command(TargetOs::Other, dir);
        assert_eq!(prog, "xdg-open");
    }

    #[test]
    fn notify_send_args_sanitize_and_cap() {
        let args = format_notify_send_args("Hello", "World", 5000);
        assert_eq!(args, vec!["-t", "5000", "Hello", "World"]);

        // Control characters collapse to spaces; runs are coalesced.
        let args = format_notify_send_args("a\nb\rc\u{7}d", "x\ty\nz", 0);
        assert_eq!(args[2], "a b c d");
        assert_eq!(args[3], "x y z");

        // Length caps hold and the ellipsis keeps the boundary valid UTF-8.
        let long_title = "t".repeat(10_000);
        let long_body = "b".repeat(10_000);
        let args = format_notify_send_args(&long_title, &long_body, 1);
        assert_eq!(args[2].chars().count(), MAX_NOTIFY_TITLE_CHARS);
        assert!(args[2].ends_with('…'));
        assert_eq!(args[3].chars().count(), MAX_NOTIFY_BODY_CHARS);
    }
}
