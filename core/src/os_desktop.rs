// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_core::os_desktop` — zero-dependency Windows 10/11 and macOS
//! (Apple Silicon / Intel) native desktop integration, unified with
//! [`crate::linux_desktop`]'s fail-silent philosophy.
//!
//! Everything here is pure `std`, never panics, and degrades to `None`
//! (or a safe default) on any host where the integration source is
//! absent. A complete beginner on any OS sees no errors and no extra
//! setup; a power user gets first-class native behaviour for free:
//!
//! * [`OsThemePalette`] — a unified theme palette described by
//!   [`OsThemeSource`] (`Omarchy`, `MacOsSystem`, `WindowsSystem` or
//!   `Default`).
//! * macOS: [`parse_macos_appearance_and_accent`] decodes
//!   `AppleInterfaceStyle` / `AppleAccentColor` into a palette,
//!   [`format_macos_notification_script`] builds an injection-proof
//!   AppleScript `display notification` string, and
//!   [`macos_caffeinate_args`] keeps the Mac awake only while the Megu
//!   process PID lives.
//! * Windows: [`parse_windows_dwm_abgr`] decodes the little-endian
//!   `0xAABBGGRR` DWM accent DWORD, [`parse_windows_reg_theme`] reads
//!   `reg query` output for `AppsUseLightTheme` / `AccentColor`, and
//!   [`format_windows_toast_xml`] builds an injection-proof Toast
//!   notification XML document.
//! * [`detect_os_theme`] is the unified auto-detect entrypoint: it
//!   delegates to Omarchy detection on Linux, probes `defaults read` on
//!   macOS and `reg query` on Windows, and returns `None` silently
//!   everywhere else.

use std::path::PathBuf;

/// Maximum characters kept in one notification title or body before it
/// is embedded in a native script/XML payload.
pub const MAX_OS_NOTIFY_CHARS: usize = 160;

/// Light or dark desktop appearance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsAppearance {
    Dark,
    Light,
}

/// Where an [`OsThemePalette`] was resolved from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsThemeSource {
    /// Linux Omarchy `colors.toml` theme.
    Omarchy,
    /// macOS system appearance/accent (`AppleInterfaceStyle`,
    /// `AppleAccentColor`).
    MacOsSystem,
    /// Windows system personalization (`AppsUseLightTheme`,
    /// `AccentColor`).
    WindowsSystem,
    /// Built-in fallback palette.
    Default,
}

/// A desktop palette resolved from a native OS theme source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsThemePalette {
    /// Which integration produced this palette.
    pub source: OsThemeSource,
    /// Light or dark appearance.
    pub appearance: OsAppearance,
    /// Accent colour (links, focus rings, active tabs).
    pub accent: [u8; 3],
    /// Window/body background.
    pub background: [u8; 3],
    /// Primary text colour.
    pub foreground: [u8; 3],
    /// Raised surface colour (panels, selections).
    pub surface: [u8; 3],
}

/// macOS system accent colours keyed by the `AppleAccentColor`
/// `defaults` integer: `-1` Graphite, `0` Red, `1` Orange, `2` Yellow,
/// `3` Green, `4` Blue, `5` Purple, `6` Pink. Anything else (including
/// unset) resolves to the default macOS System Blue.
fn macos_accent_rgb(accent_code: Option<i32>) -> [u8; 3] {
    match accent_code {
        Some(-1) => [0x6e, 0x6e, 0x73], // Graphite
        Some(0) => [0xff, 0x3b, 0x30],  // Red
        Some(1) => [0xff, 0x95, 0x00],  // Orange
        Some(2) => [0xff, 0xcc, 0x00],  // Yellow
        Some(3) => [0x34, 0xc7, 0x59],  // Green
        Some(4) | None => [0x00, 0x7a, 0xff], // Blue / default System Blue
        Some(5) => [0xaf, 0x52, 0xde],  // Purple
        Some(6) => [0xff, 0x2d, 0x55],  // Pink
        Some(_) => [0x00, 0x7a, 0xff],
    }
}

/// Parse macOS `AppleInterfaceStyle` + `AppleAccentColor` probe output
/// into an [`OsThemePalette`].
///
/// `interface_style` containing `"Dark"` (case-insensitive) selects
/// dark appearance; anything else — including `None`, which is what
/// `defaults read -g AppleInterfaceStyle` yields in light mode because
/// the key simply does not exist — selects light. `accent_code` maps
/// the `AppleAccentColor` integer through [`macos_accent_rgb`]; an
/// absent or unknown code yields the default System Blue.
pub fn parse_macos_appearance_and_accent(
    interface_style: Option<&str>,
    accent_code: Option<i32>,
) -> OsThemePalette {
    let dark = interface_style
        .map(|s| s.to_ascii_lowercase().contains("dark"))
        .unwrap_or(false);
    let accent = macos_accent_rgb(accent_code);
    let (background, foreground, surface) = if dark {
        ([0x1e, 0x1e, 0x1e], [0xf5, 0xf5, 0xf7], [0x2c, 0x2c, 0x2e])
    } else {
        ([0xf5, 0xf5, 0xf7], [0x1c, 0x1c, 0x1e], [0xff, 0xff, 0xff])
    };
    OsThemePalette {
        source: OsThemeSource::MacOsSystem,
        appearance: if dark {
            OsAppearance::Dark
        } else {
            OsAppearance::Light
        },
        accent,
        background,
        foreground,
        surface,
    }
}

/// Escape one free-text field for embedding inside an AppleScript
/// double-quoted string: control characters are stripped outright (a
/// stray newline can never break out of the string literal), `\` and
/// `"` are backslash-escaped, and the result is capped at
/// [`MAX_OS_NOTIFY_CHARS`] characters. Untrusted player/server strings
/// can therefore never inject AppleScript.
fn escape_osascript_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().filter(|c| !c.is_control()).take(MAX_OS_NOTIFY_CHARS) {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Build a safe AppleScript `display notification` command for macOS.
///
/// Both fields pass through [`escape_osascript_string`], so quotes,
/// backslashes, newlines and control characters in untrusted input are
/// neutralised before the script is ever handed to `osascript`.
pub fn format_macos_notification_script(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        escape_osascript_string(body),
        escape_osascript_string(title)
    )
}

/// `caffeinate` arguments that keep the Mac awake only while the Megu
/// process identified by `pid` is alive: `-dimsu` blocks display,
/// idle-sleep, system-sleep and user-active assertions, and `-w <pid>`
/// makes caffeinate exit automatically when that process dies — so the
/// assertion can never outlive Megu itself.
pub fn macos_caffeinate_args(pid: u32) -> Vec<String> {
    vec!["-dimsu".to_owned(), "-w".to_owned(), pid.to_string()]
}

/// Decode a Windows `AccentColor`/`ColorizationColor` DWORD stored as
/// little-endian ABGR (`0xAABBGGRR`) into `[R, G, B]`.
///
/// Example: the Windows 11 default accent `0xff_d7_78_00` decodes to
/// `[0x00, 0x78, 0xd7]` (Windows blue).
pub fn parse_windows_dwm_abgr(dword: u32) -> [u8; 3] {
    [
        (dword & 0xff) as u8,
        ((dword >> 8) & 0xff) as u8,
        ((dword >> 16) & 0xff) as u8,
    ]
}

/// Parse one numeric token from `reg query` output: `0x`-prefixed hex
/// (the usual `REG_DWORD` rendering) or plain decimal.
fn parse_reg_dword_token(token: &str) -> Option<u32> {
    let t = token.trim().trim_end_matches(',');
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u32>().ok()
    }
}

/// Find a named `REG_DWORD` value in `reg query` output and return its
/// numeric value. `reg query` without `/v` prints a header line
/// (`HKEY_...\Path`) followed by indented
/// `<name>    REG_DWORD    0x...` rows; the value name is matched as a
/// whole whitespace-separated token so `AppsUseLightTheme` can never
/// match a longer name that merely contains it.
fn reg_dword_value(stdout: &str, name: &str) -> Option<u32> {
    for line in stdout.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 3 || tokens[0].starts_with("HKEY_") {
            continue;
        }
        if tokens.contains(&name) {
            return tokens.last().and_then(|v| parse_reg_dword_token(v));
        }
    }
    None
}

/// Default Windows accent (system blue) used when the registry probe
/// does not yield an accent colour.
const WINDOWS_DEFAULT_ACCENT: [u8; 3] = [0x00, 0x78, 0xd7];

/// Parse `reg query` output captured from
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
/// (`personalize_stdout`) and optionally the DWM key (`dwm_stdout`)
/// into an [`OsThemePalette`].
///
/// `AppsUseLightTheme` (`0x0` = Dark, `0x1` = Light) decides the
/// appearance; `AccentColor` — searched in the personalize output first,
/// then the DWM output — is decoded as little-endian ABGR via
/// [`parse_windows_dwm_abgr`]. Returns `None` when the light/dark value
/// is absent so a malformed or unrelated registry dump can never
/// masquerade as a theme.
pub fn parse_windows_reg_theme(
    personalize_stdout: &str,
    dwm_stdout: &str,
) -> Option<OsThemePalette> {
    let light = reg_dword_value(personalize_stdout, "AppsUseLightTheme")?;
    let appearance = if light == 0 {
        OsAppearance::Dark
    } else {
        OsAppearance::Light
    };
    let accent = reg_dword_value(personalize_stdout, "AccentColor")
        .or_else(|| reg_dword_value(dwm_stdout, "AccentColor"))
        .map(parse_windows_dwm_abgr)
        .unwrap_or(WINDOWS_DEFAULT_ACCENT);
    let (background, foreground, surface) = match appearance {
        OsAppearance::Dark => ([0x20, 0x20, 0x20], [0xe5, 0xe5, 0xe5], [0x28, 0x28, 0x28]),
        OsAppearance::Light => ([0xf3, 0xf3, 0xf3], [0x20, 0x20, 0x20], [0xff, 0xff, 0xff]),
    };
    Some(OsThemePalette {
        source: OsThemeSource::WindowsSystem,
        appearance,
        accent,
        background,
        foreground,
        surface,
    })
}

/// Escape one free-text field for embedding inside XML character data:
/// control characters are stripped, the five predefined entities
/// (`&`, `<`, `>`, `"`, `'`) are replaced, and the result is capped at
/// [`MAX_OS_NOTIFY_CHARS`] characters. Untrusted player/server strings
/// can therefore never break out of the Toast XML or inject markup.
fn escape_xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().filter(|c| !c.is_control()).take(MAX_OS_NOTIFY_CHARS) {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Build a Windows Toast Notification XML document (`ToastGeneric`
/// template) with both free-text fields strictly XML-escaped, so
/// untrusted strings can never break the document or inject elements.
pub fn format_windows_toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\">\
         <text>{}</text><text>{}</text></binding></visual></toast>",
        escape_xml_text(title),
        escape_xml_text(body)
    )
}

/// Run a probe command to completion and return its stdout on success.
/// Every failure mode — missing binary, non-zero exit, non-UTF-8
/// output — collapses to `None` so callers stay fail-silent.
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Detect the live Omarchy theme for the Linux fallback path, converting
/// [`crate::linux_desktop::OmarchyThemePalette`] into an
/// [`OsThemePalette`]. Appearance is derived from background luminance
/// since Omarchy themes carry no explicit light/dark flag.
fn detect_linux_theme() -> Option<OsThemePalette> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })?;
    let p = crate::linux_desktop::detect_omarchy_theme_in(&config_home)?;
    let [r, g, b] = p.background;
    let luminance = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
    Some(OsThemePalette {
        source: OsThemeSource::Omarchy,
        appearance: if luminance < 128.0 {
            OsAppearance::Dark
        } else {
            OsAppearance::Light
        },
        accent: p.accent,
        background: p.background,
        foreground: p.foreground,
        surface: p.surface,
    })
}

/// Probe macOS system appearance and accent via `defaults read -g`.
/// `AppleInterfaceStyle` only exists in dark mode, so a failed read
/// means light; a missing `AppleAccentColor` means the default blue.
/// `defaults` is a system binary on every supported macOS, so the
/// palette is always produced — worst case it is the light/default
/// palette, which is exactly what an un-customised Mac shows anyway.
fn detect_macos_theme() -> Option<OsThemePalette> {
    let style = command_stdout("defaults", &["read", "-g", "AppleInterfaceStyle"])
        .map(|s| s.trim().to_owned());
    let accent_code = command_stdout("defaults", &["read", "-g", "AppleAccentColor"])
        .and_then(|s| s.trim().parse::<i32>().ok());
    Some(parse_macos_appearance_and_accent(
        style.as_deref(),
        accent_code,
    ))
}

/// Probe Windows system theme via `reg query` on the Personalize and
/// DWM keys. Any failure returns `None`.
fn detect_windows_theme() -> Option<OsThemePalette> {
    let personalize = command_stdout(
        "reg",
        &[
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        ],
    )?;
    let dwm = command_stdout("reg", &["query", r"HKCU\Software\Microsoft\Windows\DWM"])
        .unwrap_or_default();
    parse_windows_reg_theme(&personalize, &dwm)
}

/// Unified OS theme auto-detection.
///
/// * Linux/Unix: delegates to Omarchy `colors.toml` detection via
///   [`crate::linux_desktop::detect_omarchy_theme_in`], converting the
///   result to an [`OsThemePalette`] with [`OsThemeSource::Omarchy`].
/// * macOS: probes `defaults read -g AppleInterfaceStyle` /
///   `AppleAccentColor`, failing silently to `None` if `defaults` is
///   unavailable.
/// * Windows: probes `reg query` for `AppsUseLightTheme` / `AccentColor`,
///   failing silently to `None` if `reg` is unavailable.
///
/// Every error path returns `None`: beginners never see an error.
pub fn detect_os_theme() -> Option<OsThemePalette> {
    match crate::linux_desktop::host_os() {
        crate::linux_desktop::TargetOs::MacOs => detect_macos_theme(),
        crate::linux_desktop::TargetOs::Windows => detect_windows_theme(),
        _ => detect_linux_theme(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- macOS appearance & accent ----------

    #[test]
    fn macos_dark_and_light_appearances() {
        let dark = parse_macos_appearance_and_accent(Some("Dark"), Some(4));
        assert_eq!(dark.appearance, OsAppearance::Dark);
        assert_eq!(dark.source, OsThemeSource::MacOsSystem);
        assert_eq!(dark.accent, [0x00, 0x7a, 0xff]);

        let light = parse_macos_appearance_and_accent(Some("Light"), Some(4));
        assert_eq!(light.appearance, OsAppearance::Light);

        // Case-insensitive matching.
        assert_eq!(
            parse_macos_appearance_and_accent(Some("dark aqua"), None).appearance,
            OsAppearance::Dark
        );
        assert_eq!(
            parse_macos_appearance_and_accent(Some("DARK"), None).appearance,
            OsAppearance::Dark
        );
        // Absent key (light mode) and unrelated strings are light.
        assert_eq!(
            parse_macos_appearance_and_accent(None, None).appearance,
            OsAppearance::Light
        );
        assert_eq!(
            parse_macos_appearance_and_accent(Some("Aqua"), None).appearance,
            OsAppearance::Light
        );
    }

    #[test]
    fn macos_all_accent_codes_map_to_exact_rgb() {
        let expected: [(Option<i32>, [u8; 3]); 9] = [
            (Some(-1), [0x6e, 0x6e, 0x73]), // Graphite
            (Some(0), [0xff, 0x3b, 0x30]),  // Red
            (Some(1), [0xff, 0x95, 0x00]),  // Orange
            (Some(2), [0xff, 0xcc, 0x00]),  // Yellow
            (Some(3), [0x34, 0xc7, 0x59]),  // Green
            (Some(4), [0x00, 0x7a, 0xff]),  // Blue
            (Some(5), [0xaf, 0x52, 0xde]),  // Purple
            (Some(6), [0xff, 0x2d, 0x55]),  // Pink
            (None, [0x00, 0x7a, 0xff]),     // default System Blue
        ];
        for (code, rgb) in expected {
            let p = parse_macos_appearance_and_accent(Some("Dark"), code);
            assert_eq!(p.accent, rgb, "accent code {code:?}");
        }
        // Unknown codes fall back to System Blue too.
        assert_eq!(
            parse_macos_appearance_and_accent(Some("Dark"), Some(42)).accent,
            [0x00, 0x7a, 0xff]
        );
    }

    // ---------- macOS AppleScript notification escaping ----------

    #[test]
    fn macos_notification_script_escapes_injection() {
        let script = format_macos_notification_script(
            "Player \"Admin\" joined",
            "path C:\\Users\\player\\world\nnext line",
        );
        // Quotes and backslashes inside the strings are escaped.
        assert!(script.contains("Player \\\"Admin\\\" joined"));
        assert!(script.contains("C:\\\\Users\\\\player\\\\world"));
        // The raw newline was stripped, never emitted literally.
        assert!(!script.contains('\n'));
        assert!(script.starts_with("display notification \""));
        assert!(script.ends_with("\" with title \"Player \\\"Admin\\\" joined\""));
        // Control characters are stripped entirely.
        let ctrl = format_macos_notification_script("a\u{0007}b\u{001b}c", "x\ry");
        assert!(ctrl.contains("abc") || ctrl.contains("a") && ctrl.contains("b") && ctrl.contains("c"));
        assert!(!ctrl.contains('\u{0007}'));
        assert!(!ctrl.contains('\u{001b}'));
        assert!(!ctrl.contains('\r'));
    }

    #[test]
    fn macos_notification_script_caps_length() {
        let long = "x".repeat(MAX_OS_NOTIFY_CHARS * 3);
        let script = format_macos_notification_script(&long, &long);
        // Each embedded field is capped at MAX_OS_NOTIFY_CHARS chars.
        assert!(script.len() < MAX_OS_NOTIFY_CHARS * 2 + 128);
        assert!(!script.contains(&"x".repeat(MAX_OS_NOTIFY_CHARS + 1)));
    }

    // ---------- macOS caffeinate ----------

    #[test]
    fn macos_caffeinate_args_tie_assertion_to_pid() {
        assert_eq!(
            macos_caffeinate_args(12345),
            vec!["-dimsu".to_string(), "-w".to_string(), "12345".to_string()]
        );
    }

    // ---------- Windows DWM ABGR decoding ----------

    #[test]
    fn windows_dwm_abgr_decodes_windows11_blue() {
        // 0xAABBGGRR little-endian: 0xff_d7_78_00 -> R=0x00 G=0x78 B=0xd7.
        assert_eq!(parse_windows_dwm_abgr(0xff_d7_78_00), [0x00, 0x78, 0xd7]);
        // Pure red stored as ABGR 0x000000ff -> [0xff, 0x00, 0x00].
        assert_eq!(parse_windows_dwm_abgr(0x00_00_00_ff), [0xff, 0x00, 0x00]);
        // Alpha byte is ignored entirely.
        assert_eq!(parse_windows_dwm_abgr(0x12_34_56_78), [0x78, 0x56, 0x34]);
    }

    // ---------- Windows reg query parsing ----------

    const REG_DARK: &str = "\
HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize
    ColorPrevalence    REG_DWORD    0x1
    AppsUseLightTheme    REG_DWORD    0x0
    SystemUsesLightTheme    REG_DWORD    0x0
    AccentColor    REG_DWORD    0xffd77800
";

    const REG_LIGHT: &str = "\
HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize
    ColorPrevalence    REG_DWORD    0x1
    AppsUseLightTheme    REG_DWORD    0x1
    SystemUsesLightTheme    REG_DWORD    0x1
    AccentColor    REG_DWORD    0xffc359c3
";

    #[test]
    fn windows_reg_theme_parses_dark_and_light() {
        let dark = parse_windows_reg_theme(REG_DARK, "").expect("dark theme");
        assert_eq!(dark.source, OsThemeSource::WindowsSystem);
        assert_eq!(dark.appearance, OsAppearance::Dark);
        assert_eq!(dark.accent, parse_windows_dwm_abgr(0xffd77800));

        let light = parse_windows_reg_theme(REG_LIGHT, "").expect("light theme");
        assert_eq!(light.appearance, OsAppearance::Light);
        assert_eq!(light.accent, parse_windows_dwm_abgr(0xffc359c3));
    }

    #[test]
    fn windows_reg_theme_falls_back_to_dwm_and_defaults() {
        // AccentColor only present in the DWM dump.
        let dwm = "\
HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\DWM
    AccentColor    REG_DWORD    0xff0078d7
";
        let no_accent = "\
HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize
    AppsUseLightTheme    REG_DWORD    0x0
";
        let p = parse_windows_reg_theme(no_accent, dwm).expect("theme");
        assert_eq!(p.accent, parse_windows_dwm_abgr(0xff0078d7));

        // No accent anywhere -> Windows system blue default.
        let p2 = parse_windows_reg_theme(no_accent, "").expect("theme");
        assert_eq!(p2.accent, WINDOWS_DEFAULT_ACCENT);

        // Missing AppsUseLightTheme -> not a theme dump.
        assert!(parse_windows_reg_theme("HKEY_CURRENT_USER\\Foo\n    Bar    REG_DWORD    0x1\n", "").is_none());
    }

    // ---------- Windows Toast XML escaping ----------

    #[test]
    fn windows_toast_xml_escapes_markup_injection() {
        let xml = format_windows_toast_xml(
            "<script>alert(1)</script>",
            "Tom & \"Jerry\" <b>'quoted'</b>",
        );
        assert!(xml.starts_with("<toast><visual><binding template=\"ToastGeneric\">"));
        assert!(xml.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(xml.contains("Tom &amp; &quot;Jerry&quot; &lt;b&gt;&apos;quoted&apos;&lt;/b&gt;"));
        // No raw injected tags or unescaped entities survive in text.
        assert!(!xml.contains("<script>"));
        assert!(!xml.contains("&lt;script&gt;alert(1)</script>"));
        // Control characters stripped.
        let ctrl = format_windows_toast_xml("a\u{0007}b", "x\ny");
        assert!(!ctrl.contains('\u{0007}'));
        assert!(!ctrl.contains('\n'));
    }

    // ---------- Unified entrypoint sanity ----------

    #[test]
    fn detect_os_theme_never_panics() {
        // Whatever the host, the result is either a palette or None.
        let _ = detect_os_theme();
    }
}
