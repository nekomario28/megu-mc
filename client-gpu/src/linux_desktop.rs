// SPDX-License-Identifier: LGPL-3.0-or-later
//! Linux desktop integration policies that stay independent of windowing APIs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MEGU_WAYLAND_APP_ID: &str = "dev.megu.ClientGpu";
pub const MEGU_X11_WM_CLASS: &str = "megu-client-gpu";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinuxCompositor {
    Hyprland,
    Sway,
    Niri,
    Cosmic,
    KdePlasma,
    Gnome,
    Gamescope,
    GenericWayland,
    X11Tiling,
    X11Floating,
    Headless,
}

/// Detect a session from supplied environment pairs, allowing portable tests.
pub fn detect_compositor<I, K, V>(environment: I) -> LinuxCompositor
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let pairs: Vec<(String, String)> = environment
        .into_iter()
        .map(|(key, value)| (key.as_ref().to_owned(), value.as_ref().to_owned()))
        .collect();
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    let desktop = get("XDG_CURRENT_DESKTOP")
        .unwrap_or("")
        .to_ascii_lowercase();
    let session = get("XDG_SESSION_TYPE").unwrap_or("").to_ascii_lowercase();
    let compositor = get("XDG_SESSION_DESKTOP")
        .unwrap_or("")
        .to_ascii_lowercase();

    if session.is_empty() && get("WAYLAND_DISPLAY").is_none() && get("DISPLAY").is_none() {
        return LinuxCompositor::Headless;
    }
    if session == "wayland" || get("WAYLAND_DISPLAY").is_some() {
        if get("HYPRLAND_INSTANCE_SIGNATURE").is_some() || desktop.contains("hyprland") {
            return LinuxCompositor::Hyprland;
        }
        if get("SWAYSOCK").is_some() || desktop.contains("sway") {
            return LinuxCompositor::Sway;
        }
        if desktop.contains("niri") || compositor.contains("niri") {
            return LinuxCompositor::Niri;
        }
        if desktop.contains("cosmic") || compositor.contains("cosmic") {
            return LinuxCompositor::Cosmic;
        }
        if desktop.contains("gamescope") || compositor.contains("gamescope") {
            return LinuxCompositor::Gamescope;
        }
        if desktop.contains("kde") || desktop.contains("plasma") {
            return LinuxCompositor::KdePlasma;
        }
        if desktop.contains("gnome") {
            return LinuxCompositor::Gnome;
        }
        return LinuxCompositor::GenericWayland;
    }
    if session == "x11" || get("DISPLAY").is_some() {
        if desktop.contains("i3")
            || desktop.contains("bspwm")
            || desktop.contains("awesome")
            || desktop.contains("xmonad")
        {
            return LinuxCompositor::X11Tiling;
        }
        return LinuxCompositor::X11Floating;
    }
    LinuxCompositor::Headless
}

impl LinuxCompositor {
    /// `true` for sessions where winit drives the native Wayland backend:
    /// `WindowAttributesExtWayland::with_name`'s `general` argument is
    /// read as `app_id`. X11 and headless sessions read `WM_CLASS`
    /// through the shared `platform_specific.name` slot instead.
    pub fn is_wayland(self) -> bool {
        matches!(
            self,
            Self::Hyprland
                | Self::Sway
                | Self::Niri
                | Self::Cosmic
                | Self::KdePlasma
                | Self::Gnome
                | Self::Gamescope
                | Self::GenericWayland
        )
    }
}

/// Detect the compositor from this process's live environment — the
/// runtime counterpart of [`detect_compositor`] used by `window.rs`.
/// Non-UTF-8 pairs are skipped rather than aborting (`std::env::vars`
/// panics on them; `vars_os` does not).
pub fn detect_compositor_from_env() -> LinuxCompositor {
    detect_compositor(std::env::vars_os().filter_map(|(key, value)| {
        Some((key.into_string().ok()?, value.into_string().ok()?))
    }))
}

/// `(general, instance)` name pair for `winit 0.30`'s platform
/// `with_name` extension. Both backends share one
/// `platform_specific.name` slot, so the detected compositor picks the
/// pair the active backend actually consumes: Wayland reads `general`
/// as `app_id` (`instance` is a no-op but still carries the X11 name in
/// case of a misdetect), while X11 emits
/// `WM_CLASS(STRING) = "instance", "general"` — `res_class` stays
/// pinned to [`MEGU_X11_WM_CLASS`] so `StartupWMClass` matching works.
pub fn window_name_hints(compositor: LinuxCompositor) -> (&'static str, &'static str) {
    if compositor.is_wayland() {
        (MEGU_WAYLAND_APP_ID, MEGU_X11_WM_CLASS)
    } else {
        (MEGU_X11_WM_CLASS, MEGU_X11_WM_CLASS)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImeCompositionState {
    pub active: bool,
    pub preedit: String,
    pub cursor_byte_range: Option<(usize, usize)>,
    pub ime_cursor_area: (i32, i32, u32, u32),
}

impl ImeCompositionState {
    pub fn on_enabled(&mut self) {
        self.active = true;
    }

    /// Store preedit and clamp its cursor/selection range to valid UTF-8 boundaries.
    pub fn on_preedit(
        &mut self,
        text: impl Into<String>,
        cursor_byte_range: Option<(usize, usize)>,
    ) {
        self.preedit = text.into();
        self.cursor_byte_range = cursor_byte_range.map(|(start, end)| {
            let start = floor_char_boundary(&self.preedit, start);
            let end = floor_char_boundary(&self.preedit, end).max(start);
            (start, end)
        });
    }

    pub fn on_commit(&mut self, text: impl Into<String>) -> String {
        let committed = text.into();
        self.preedit.clear();
        self.cursor_byte_range = None;
        committed
    }

    pub fn on_disabled(&mut self) {
        self.active = false;
        self.preedit.clear();
        self.cursor_byte_range = None;
    }

    pub fn render_composed_line(&self, committed_prefix: &str, committed_suffix: &str) -> String {
        format!("{committed_prefix}{}{committed_suffix}", self.preedit)
    }

    pub fn set_ime_cursor_area(&mut self, area: (i32, i32, u32, u32)) {
        self.ime_cursor_area = area;
    }
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerGrab {
    Locked,
    Confined,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecorationHint {
    ClientSide,
    ServerSide,
    Either,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TilingGrabPolicy {
    compositor: LinuxCompositor,
    focused: bool,
    pointer_inside: bool,
    requested: bool,
}

impl TilingGrabPolicy {
    pub fn new(compositor: LinuxCompositor) -> Self {
        Self {
            compositor,
            focused: true,
            pointer_inside: false,
            requested: false,
        }
    }
    /// The compositor this policy was built for — `window.rs` reads it
    /// back for the `with_name` attribute hints at window creation.
    pub fn compositor(&self) -> LinuxCompositor {
        self.compositor
    }
    pub fn focus_lost(&mut self) {
        self.focused = false;
        self.requested = false;
    }
    pub fn focus_gained(&mut self) {
        self.focused = true;
    }
    pub fn pointer_entered(&mut self) {
        self.pointer_inside = true;
        if self.focused {
            self.requested = true;
        }
    }
    pub fn pointer_left(&mut self) {
        self.pointer_inside = false;
        self.requested = false;
    }
    pub fn request_grab(&mut self) -> bool {
        self.requested = self.focused && self.pointer_inside;
        self.requested
    }
    pub fn should_grab(&self) -> bool {
        self.requested && self.focused && self.pointer_inside
    }
    pub fn preferred_grabs(&self) -> &'static [PointerGrab] {
        match self.compositor {
            LinuxCompositor::Gamescope => &[PointerGrab::Confined, PointerGrab::Locked],
            LinuxCompositor::GenericWayland
            | LinuxCompositor::Hyprland
            | LinuxCompositor::Sway
            | LinuxCompositor::Niri
            | LinuxCompositor::Cosmic
            | LinuxCompositor::KdePlasma
            | LinuxCompositor::Gnome => &[PointerGrab::Locked, PointerGrab::Confined],
            _ => &[PointerGrab::Confined, PointerGrab::Locked],
        }
    }
    pub fn decoration_hint(&self) -> DecorationHint {
        match self.compositor {
            LinuxCompositor::Hyprland
            | LinuxCompositor::Sway
            | LinuxCompositor::Niri
            | LinuxCompositor::Cosmic => DecorationHint::ClientSide,
            LinuxCompositor::KdePlasma | LinuxCompositor::Gnome => DecorationHint::ServerSide,
            _ => DecorationHint::Either,
        }
    }
}

pub fn render_desktop_entry(exec_path: &Path, icon_name: &str) -> String {
    let exec = desktop_exec_quote(&exec_path.to_string_lossy());
    let icon = desktop_value_escape(icon_name);
    format!("[Desktop Entry]\nVersion=1.0\nType=Application\nName=Megu Client GPU\nName[ja]=Megu クライアント GPU\nComment=Play Megu\nComment[ja]=Megu をプレイ\nExec={exec}\nIcon={icon}\nTerminal=false\nCategories=Game;\nStartupWMClass={MEGU_X11_WM_CLASS}\nStartupNotify=true\n")
}

fn desktop_value_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}
fn desktop_exec_quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$");
    format!("\"{escaped}\"")
}

/// Install under `$XDG_DATA_HOME/applications` without elevated privileges.
pub fn install_xdg_desktop_entry(xdg_data_home: &Path, exec_path: &Path) -> io::Result<PathBuf> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let applications = xdg_data_home.join("applications");
    fs::create_dir_all(&applications)?;
    let destination = applications.join("megu-client-gpu.desktop");
    let temporary = applications.join(format!(
        ".megu-client-gpu.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let contents = render_desktop_entry(exec_path, "megu-client-gpu");
    let result = (|| {
        fs::write(&temporary, contents)?;
        fs::rename(&temporary, &destination)?;
        Ok(destination.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
