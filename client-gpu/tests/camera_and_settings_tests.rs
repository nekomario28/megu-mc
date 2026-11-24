// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use megu_client_gpu::window::{CameraController, CameraMode, GpuApp};
use megu_client_gpu::{Camera, GpuCliConfig};
use megu_ui::state::{UiSettings, VideoPreset};

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[test]
fn f5_cycles_all_three_modes_and_offsets_four_blocks() {
    let mut controller = CameraController::default();
    let player = Camera {
        position: [5.0, 10.0, -3.0],
        yaw_deg: 37.0,
        pitch_deg: -21.0,
        ..Camera::default()
    };
    assert_eq!(controller.mode(), CameraMode::FirstPerson);
    assert_eq!(controller.view_camera(&player).position, player.position);

    assert_eq!(controller.cycle_mode(), CameraMode::ThirdPersonBack);
    let back = controller.view_camera(&player);
    assert!((distance(back.position, player.position) - 4.0).abs() < 1e-5);
    let forward = player.forward_vector();
    let back_offset = [
        back.position[0] - player.position[0],
        back.position[1] - player.position[1],
        back.position[2] - player.position[2],
    ];
    assert!((dot(back_offset, forward) + 4.0).abs() < 1e-5);
    assert_eq!(back.yaw_deg, player.yaw_deg);
    assert_eq!(back.pitch_deg, player.pitch_deg);

    assert_eq!(controller.cycle_mode(), CameraMode::ThirdPersonFront);
    let front = controller.view_camera(&player);
    assert!((distance(front.position, player.position) - 4.0).abs() < 1e-5);
    let front_offset = [
        front.position[0] - player.position[0],
        front.position[1] - player.position[1],
        front.position[2] - player.position[2],
    ];
    assert!((dot(front_offset, forward) - 4.0).abs() < 1e-5);
    assert!((dot(front.forward_vector(), forward) + 1.0).abs() < 1e-5);
    assert!((front.pitch_deg + player.pitch_deg).abs() < f32::EPSILON);
    assert_eq!(player.position, [5.0, 10.0, -3.0]);

    assert_eq!(controller.cycle_mode(), CameraMode::FirstPerson);
    assert_eq!(controller.view_camera(&player).position, player.position);
}

#[test]
fn zoom_hold_release_and_mouse_sensitivity_interpolate_together() {
    let mut controller = CameraController::new(80.0, 1.5, 4.0);
    assert_eq!(controller.effective_fov_deg(), 80.0);
    assert_eq!(controller.effective_sensitivity(), 1.5);

    controller.set_zoomed(true);
    let first_zoom_frame = controller.tick(1.0 / 60.0, false);
    assert!(first_zoom_frame < 80.0 && first_zoom_frame > 20.0);
    assert!(controller.effective_sensitivity() < 1.5);
    for _ in 0..120 {
        controller.tick(1.0 / 60.0, false);
    }
    assert!((controller.effective_fov_deg() - 20.0).abs() < 0.01);
    assert!((controller.effective_sensitivity() - 0.375).abs() < 0.001);

    let mut camera = Camera::default();
    controller.apply_mouse_look(&mut camera, 20.0, 0.0);
    let zoomed_yaw = camera.yaw_deg;
    assert!(zoomed_yaw > 0.0);
    assert!(zoomed_yaw < 20.0 * 0.12 * 1.5);

    controller.set_zoomed(false);
    let release_frame = controller.tick(1.0 / 60.0, false);
    assert!(release_frame > 20.0 && release_frame < 80.0);
    for _ in 0..120 {
        controller.tick(1.0 / 60.0, false);
    }
    assert!((controller.effective_fov_deg() - 80.0).abs() < 0.01);
    assert!((controller.effective_sensitivity() - 1.5).abs() < 0.001);
}

#[test]
fn sprint_boost_is_smoothed_and_fov_is_finite_and_clamped() {
    let mut controller = CameraController::new(100.0, 1.0, 4.0);
    assert_eq!(controller.tick(f32::NAN, true), 100.0);
    let first = controller.tick(1.0 / 60.0, true);
    assert!(first > 100.0 && first < 110.0);
    for _ in 0..120 {
        controller.tick(1.0 / 60.0, true);
    }
    assert!((controller.effective_fov_deg() - 110.0).abs() < 0.01);
    let first_release = controller.tick(1.0 / 60.0, false);
    assert!(first_release < 110.0 && first_release > 100.0);

    controller.apply_settings(120.0, 1.0, 4.0);
    for _ in 0..120 {
        controller.tick(1.0 / 60.0, true);
    }
    assert_eq!(controller.effective_fov_deg(), 120.0);
    controller.apply_settings(10.0, 1.0, 10.0);
    controller.set_zoomed(true);
    for _ in 0..120 {
        controller.tick(1.0 / 60.0, false);
    }
    assert_eq!(controller.effective_fov_deg(), 10.0);

    controller.apply_settings(f32::NAN, f32::INFINITY, f32::NEG_INFINITY);
    assert_eq!(controller.base_fov_deg(), 70.0);
    assert_eq!(controller.base_sensitivity(), 1.0);
    assert_eq!(controller.zoom_divisor(), 4.0);
    for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let fov = controller.tick(dt, true);
        assert!(fov.is_finite());
        assert!((10.0..=120.0).contains(&fov));
    }
    controller.apply_settings(f32::INFINITY, f32::NAN, f32::NAN);
    assert!((10.0..=120.0).contains(&controller.effective_fov_deg()));
}

#[test]
fn settings_change_updates_active_camera_values_and_pacing_without_window() {
    let cfg = GpuCliConfig {
        offline_demo: true,
        ..GpuCliConfig::default()
    };
    let mut app = GpuApp::new(cfg);

    app.apply_camera_settings(95.0, 1.7, 6.0);
    assert_eq!(app.camera_controller().base_fov_deg(), 95.0);
    assert_eq!(app.camera_controller().base_sensitivity(), 1.7);
    assert_eq!(app.camera_controller().zoom_divisor(), 6.0);
    assert_eq!(app.render_config().fov_deg, 95.0);

    let mut settings = UiSettings::default();
    settings.video.fps_cap = 144;
    settings.video.background_fps = 0;
    settings.video.vsync = false;
    settings.video.view_distance = 24;
    settings.video.preset = VideoPreset::Fast;
    // `apply_settings` owns the camera too — the Options widgets feed
    // the same values the manual `apply_camera_settings` call set.
    settings.fov_deg = 95;
    settings.mouse_sensitivity_pct = 170;
    settings.zoom_divisor_tenths = 60;
    app.apply_settings(&settings);
    assert!(!app.render_config().vsync);
    assert_eq!(app.render_config().max_fps, Some(144));
    assert_eq!(app.render_config().background_fps_limit, 0);
    assert_eq!(app.render_config().view_distance, Some(24));
    assert_eq!(app.camera_controller().base_fov_deg(), 95.0);
    assert_eq!(app.camera_controller().zoom_divisor(), 6.0);

    settings.qol.smooth_zoom = false;
    app.apply_settings(&settings);
    let mut snap_zoom = CameraController::default();
    snap_zoom.set_smooth_zoom(settings.qol.smooth_zoom);
    snap_zoom.set_zoomed(true);
    assert!((snap_zoom.tick(1.0 / 60.0, false) - 17.5).abs() < 0.001);

    settings.video.fps_cap = 0;
    settings.video.vsync = true;
    app.apply_settings(&settings);
    assert!(app.render_config().vsync);
    assert_eq!(app.render_config().max_fps, None);
}
