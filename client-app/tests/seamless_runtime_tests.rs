// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_client::seamless_runtime::{
    AudioAction, AudioDeviceHotSwapMonitor, AudioDeviceId, AudioSink, InstantResumeStore,
    ReloadChannel, ReloadState, ResumeStatus, ResumeTarget, ZeroFreezeReloadCoordinator,
};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir() -> std::path::PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("megu-seamless-{n}"));
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn resume_store_round_trips_and_never_persists_credentials() {
    let dir = temp_dir();
    let store = InstantResumeStore::new(dir.join("last_session.json"));
    let world = dir.join("world");
    fs::create_dir(&world).unwrap();
    let saved = store
        .save(
            ResumeTarget::SingleplayerWorld {
                world_dir: world.clone(),
                display_name: "Local world".into(),
            },
            Some("profile-7".into()),
        )
        .unwrap();
    assert!(matches!(store.resolve_resume(true), ResumeStatus::Ready(_)));
    assert!(matches!(
        store.resolve_resume(false),
        ResumeStatus::TitleScreen(_)
    ));
    let raw = fs::read_to_string(store.path()).unwrap();
    for secret in [
        "password",
        "access_token",
        "auth_token",
        "refresh_token",
        "client_token",
    ] {
        assert!(!raw.contains(secret));
    }
    assert_eq!(saved.mod_profile_id.as_deref(), Some("profile-7"));
    assert!(store
        .save(
            ResumeTarget::MultiplayerServer {
                host: "ok.example".into(),
                port: 25565,
                display_name: "passWORD".into()
            },
            None
        )
        .is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resume_fails_closed_for_missing_world_and_malformed_server() {
    let dir = temp_dir();
    let store = InstantResumeStore::new(dir.join("last_session.json"));
    store
        .save(
            ResumeTarget::SingleplayerWorld {
                world_dir: dir.join("missing"),
                display_name: "Missing".into(),
            },
            None,
        )
        .unwrap();
    assert!(matches!(
        store.resolve_resume(true),
        ResumeStatus::TitleScreen(_)
    ));
    fs::write(store.path(),r#"{"last_timestamp":1,"mod_profile_id":null,"target":{"kind":"server","host":"user@example.com","port":25565,"display_name":"bad"}}"#).unwrap();
    assert!(matches!(
        store.resolve_resume(true),
        ResumeStatus::TitleScreen(_)
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audio_rebind_recovers_after_stall_and_fades_in() {
    let sink = AudioSink {
        id: AudioDeviceId("headphones".into()),
        fingerprint: "usb-1".into(),
    };
    let mut monitor = AudioDeviceHotSwapMonitor::default();
    assert!(matches!(
        monitor.tick(Some(sink.clone()), false, false, false, 48_000),
        AudioAction::Rebind { .. }
    ));
    for _ in 0..2 {
        monitor.tick(Some(sink.clone()), true, false, false, 48_000);
    }
    assert!(matches!(
        monitor.tick(Some(sink.clone()), true, false, false, 48_000),
        AudioAction::Rebind { .. }
    ));
    let mut pcm = [i16::MAX; 960];
    monitor.apply_rebind_fade_in(&mut pcm);
    assert!(pcm[0] > 0 && pcm[0] < i16::MAX);
    assert_eq!(pcm[959], i16::MAX);
    let mut no_device = AudioDeviceHotSwapMonitor::default();
    for _ in 0..100 {
        assert_eq!(
            no_device.tick(None, false, false, false, 48_000),
            AudioAction::None
        );
    }
}

#[test]
fn reload_generations_supersede_stale_payloads_and_swap_at_frame_boundary() {
    let mut reload = ZeroFreezeReloadCoordinator::<String>::default();
    let old = reload.begin(ReloadChannel::Resources);
    let new = reload.begin(ReloadChannel::Resources);
    assert!(!reload.prepare(ReloadChannel::Resources, old, "stale".into()));
    assert!(reload.prepare(ReloadChannel::Resources, new, "fresh".into()));
    assert!(
        matches!(reload.state(ReloadChannel::Resources),Some(ReloadState::ReadyToSwap{generation,..}) if *generation==new)
    );
    assert_eq!(
        reload.swap_at_frame(ReloadChannel::Resources, 80),
        Some("fresh".into())
    );
    assert!(
        matches!(reload.state(ReloadChannel::Resources),Some(ReloadState::Swapped{generation}) if *generation==new)
    );
    assert_eq!(reload.swap_metrics(), (1, 80));
}
