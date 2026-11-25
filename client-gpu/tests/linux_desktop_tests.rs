// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_gpu::linux_desktop::{
    detect_compositor, detect_compositor_from_env, install_xdg_desktop_entry,
    render_desktop_entry, window_name_hints, DecorationHint, ImeCompositionState,
    LinuxCompositor, PointerGrab, TilingGrabPolicy, MEGU_WAYLAND_APP_ID,
    MEGU_X11_WM_CLASS,
};
use std::fs;
use std::path::Path;

#[test]
fn compositor_detection_is_injected_and_handles_common_desktops() {
    assert_eq!(
        detect_compositor([
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("HYPRLAND_INSTANCE_SIGNATURE", "x")
        ]),
        LinuxCompositor::Hyprland
    );
    assert_eq!(
        detect_compositor([
            ("XDG_SESSION_TYPE", "wayland"),
            ("XDG_CURRENT_DESKTOP", "KDE")
        ]),
        LinuxCompositor::KdePlasma
    );
    assert_eq!(
        detect_compositor([("XDG_SESSION_TYPE", "x11"), ("XDG_CURRENT_DESKTOP", "i3")]),
        LinuxCompositor::X11Tiling
    );
    assert_eq!(
        detect_compositor([("DISPLAY", ":0")]),
        LinuxCompositor::X11Floating
    );
    assert_eq!(
        detect_compositor([] as [(&str, &str); 0]),
        LinuxCompositor::Headless
    );
}

#[test]
fn ime_composition_preserves_committed_text_and_resets_state() {
    let mut ime = ImeCompositionState::default();
    ime.on_enabled();
    ime.on_preedit("にほん", Some((1, 4)));
    assert_eq!(ime.render_composed_line("say: ", "!"), "say: にほん!");
    assert_eq!(ime.cursor_byte_range, Some((0, 3)));
    assert_eq!(ime.on_commit("日本"), "日本");
    assert_eq!(ime.render_composed_line("say: 日本", "!"), "say: 日本!");
    assert_eq!(ime.preedit, "");
    ime.on_preedit("かな", Some((99, 100)));
    assert_eq!(ime.cursor_byte_range, Some((6, 6)));
    ime.set_ime_cursor_area((12, 34, 1, 20));
    assert_eq!(ime.ime_cursor_area, (12, 34, 1, 20));
    ime.on_disabled();
    assert!(!ime.active);
    assert_eq!(ime.preedit, "");
}

#[test]
fn tiling_focus_loss_releases_and_pointer_reentry_rearms_only_on_request() {
    let mut policy = TilingGrabPolicy::new(LinuxCompositor::Sway);
    policy.pointer_entered();
    assert!(policy.request_grab());
    assert!(policy.should_grab());
    policy.focus_lost();
    assert!(!policy.should_grab());
    policy.focus_gained();
    assert!(!policy.should_grab());
    policy.pointer_left();
    assert!(!policy.should_grab());
    policy.pointer_entered();
    assert!(policy.should_grab());
    assert!(policy.request_grab());
    assert_eq!(
        policy.preferred_grabs(),
        &[PointerGrab::Locked, PointerGrab::Confined]
    );
    assert_eq!(policy.decoration_hint(), DecorationHint::ClientSide);
    assert_eq!(
        TilingGrabPolicy::new(LinuxCompositor::Gamescope).preferred_grabs(),
        &[PointerGrab::Confined, PointerGrab::Locked]
    );
}

#[test]
fn desktop_entry_is_valid_and_installs_atomically_under_xdg_data_home() {
    assert_eq!(MEGU_WAYLAND_APP_ID, "dev.megu.ClientGpu");
    assert_eq!(MEGU_X11_WM_CLASS, "megu-client-gpu");
    let text = render_desktop_entry(
        Path::new("/opt/Megu Client/megu-client-gpu"),
        "megu-client-gpu",
    );
    assert!(text.contains("Exec=\"/opt/Megu Client/megu-client-gpu\""));
    assert!(text.contains("StartupWMClass=megu-client-gpu"));
    assert!(text.contains("Categories=Game;"));
    assert!(text.contains("Comment[ja]="));

    let root = std::env::temp_dir().join(format!("megu-desktop-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let installed =
        install_xdg_desktop_entry(&root, Path::new("/usr/bin/megu-client-gpu")).unwrap();
    assert_eq!(installed, root.join("applications/megu-client-gpu.desktop"));
    assert!(fs::read_to_string(installed)
        .unwrap()
        .contains("StartupWMClass=megu-client-gpu"));
    fs::remove_dir_all(root).unwrap();
}

/// `window_name_hints` feeds winit 0.30's shared `platform_specific.name`
/// slot: Wayland sessions put `MEGU_WAYLAND_APP_ID` in `general` (read as
/// `app_id`), while X11/headless sessions keep `MEGU_X11_WM_CLASS` in both
/// slots so `WM_CLASS(STRING)` res_class matches `StartupWMClass`.
#[test]
fn window_name_hints_pick_backend_specific_pairs() {
    for wayland in [
        LinuxCompositor::Hyprland,
        LinuxCompositor::Sway,
        LinuxCompositor::Niri,
        LinuxCompositor::Cosmic,
        LinuxCompositor::KdePlasma,
        LinuxCompositor::Gnome,
        LinuxCompositor::Gamescope,
        LinuxCompositor::GenericWayland,
    ] {
        assert!(wayland.is_wayland(), "{wayland:?} must report Wayland");
        assert_eq!(
            window_name_hints(wayland),
            (MEGU_WAYLAND_APP_ID, MEGU_X11_WM_CLASS),
            "{wayland:?} must publish the reverse-DNS app_id"
        );
    }
    for x11 in [
        LinuxCompositor::X11Tiling,
        LinuxCompositor::X11Floating,
        LinuxCompositor::Headless,
    ] {
        assert!(!x11.is_wayland(), "{x11:?} must not report Wayland");
        assert_eq!(
            window_name_hints(x11),
            (MEGU_X11_WM_CLASS, MEGU_X11_WM_CLASS),
            "{x11:?} must pin WM_CLASS to the binary name"
        );
    }
}

/// `detect_compositor_from_env` must agree with the injected-pair
/// detector over the same live environment — whatever the host session
/// is, both views of it are identical.
#[test]
fn detect_compositor_from_env_matches_injected_pairs() {
    let from_env = detect_compositor_from_env();
    let injected = detect_compositor(std::env::vars());
    assert_eq!(from_env, injected);
    // The policy built from it reports the same compositor back.
    assert_eq!(TilingGrabPolicy::new(from_env).compositor(), from_env);
}
