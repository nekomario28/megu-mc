// SPDX-License-Identifier: LGPL-3.0-or-later
//! Client-host map fair-play (`minimap_fair_play` / `--fair-play` /
//! `--allow-cave-map` / `--allow-entity-radar` / `--allow-waypoint-teleport`)
//! and `--auto-tune` hardware auto-tune CLI wiring tests.
//!
//! Determinism contract: the Xaero control signal and the
//! `MEGU_FAIR_PLAY*` env pairs are pure functions of the resolved
//! settings — no test here depends on a live server spawn. The only
//! host-dependent part is `--auto-tune`, which is compared against the same
//! `recommend_tuning(probe_host_hardware())` the resolver computes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use megu_client::cli::{help_text, parse_args};
use megu_client::hardware_profile::{probe_host_hardware, recommend_tuning};
use megu_client::integrated::{
    IntegratedServerConfig, FAIR_PLAY_ENV_ALLOW_CAVE_MAP, FAIR_PLAY_ENV_ALLOW_ENTITY_RADAR,
    FAIR_PLAY_ENV_ALLOW_MOB_RADAR, FAIR_PLAY_ENV_ALLOW_WAYPOINT_TELEPORT,
    FAIR_PLAY_ENV_ENABLED, FAIR_PLAY_ENV_MINIMAP_MODE, FAIR_PLAY_ENV_PLAYER_RADAR_SCOPE,
    FAIR_PLAY_ENV_XAERO_SIGNAL,
};
use megu_client::settings::{
    ClientSettings, HostMapFairPlaySettings, HostMinimapMode, HostPlayerRadarScope,
    XAERO_FAIR_PLAY_SIGNAL, XAERO_NO_ENTITY_RADAR_SIGNAL, XAERO_RESET_SIGNAL,
};
use megu_client_mesh::gpu_pipeline::RenderBackendPreference;

/// A scratch directory guaranteed not to contain a `megu-client.toml`
/// auto-load file (the resolver only reads one when the file exists).
fn empty_scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("megu-fairplay-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn env_map(config: &IntegratedServerConfig) -> BTreeMap<&'static str, String> {
    config.env_pairs().into_iter().collect()
}

// ---------------------------------------------------------------------------
// TOML roundtrip & backward compatibility
// ---------------------------------------------------------------------------

#[test]
fn fair_play_toml_roundtrip_is_lossless() {
    for fp in [
        HostMapFairPlaySettings::default(),
        HostMapFairPlaySettings {
            minimap_fair_play: true,
            allow_cave_map: false,
            allow_entity_radar: false,
            allow_waypoint_teleport: false,
            minimap_mode: HostMinimapMode::FairPlay,
            allow_mob_radar: false,
            player_radar_scope: HostPlayerRadarScope::None,
        },
        HostMapFairPlaySettings {
            minimap_fair_play: true,
            allow_cave_map: true,
            allow_entity_radar: false,
            allow_waypoint_teleport: true,
            minimap_mode: HostMinimapMode::FairPlay,
            allow_mob_radar: false,
            player_radar_scope: HostPlayerRadarScope::PartyOnly,
        },
        HostMapFairPlaySettings {
            minimap_fair_play: true,
            allow_cave_map: true,
            allow_entity_radar: true,
            allow_waypoint_teleport: true,
            minimap_mode: HostMinimapMode::Disabled,
            allow_mob_radar: true,
            player_radar_scope: HostPlayerRadarScope::All,
        },
    ] {
        let s = ClientSettings {
            map_fair_play: fp,
            ..ClientSettings::default()
        };
        let (back, warnings) = ClientSettings::from_toml(&s.to_toml());
        assert_eq!(back, s, "{fp:?}");
        assert_eq!(back.map_fair_play, fp);
        assert!(warnings.is_empty(), "{fp:?}: {warnings:?}");
    }
}

#[test]
fn fair_play_omitted_keys_keep_permissive_defaults() {
    // A file written before the fair-play keys existed stays valid and
    // defaults to fair play off with every feature allowed.
    let (s, warnings) = ClientSettings::from_toml("view_distance = 20\n");
    assert_eq!(s.map_fair_play, HostMapFairPlaySettings::default());
    assert!(!s.map_fair_play.minimap_fair_play);
    assert!(s.map_fair_play.allow_cave_map);
    assert!(s.map_fair_play.allow_entity_radar);
    assert!(s.map_fair_play.allow_waypoint_teleport);
    // The granular switches default permissive too: unrestricted minimap,
    // mob radar on, every player on the radar.
    assert_eq!(s.map_fair_play.minimap_mode, HostMinimapMode::Unrestricted);
    assert!(s.map_fair_play.allow_mob_radar);
    assert_eq!(s.map_fair_play.player_radar_scope, HostPlayerRadarScope::All);
    assert!(warnings.is_empty());

    // apply_toml overlays only the keys a file defines.
    let mut s = ClientSettings {
        map_fair_play: HostMapFairPlaySettings {
            minimap_fair_play: true,
            allow_cave_map: false,
            allow_entity_radar: false,
            allow_waypoint_teleport: true,
            minimap_mode: HostMinimapMode::FairPlay,
            allow_mob_radar: false,
            player_radar_scope: HostPlayerRadarScope::PartyOnly,
        },
        ..ClientSettings::default()
    };
    let warnings = s.apply_toml("view_distance = 14\n");
    assert!(warnings.is_empty());
    assert!(!s.map_fair_play.allow_entity_radar);
    s.apply_toml("allow_entity_radar = true\n");
    assert!(s.map_fair_play.allow_entity_radar);
    // The mob-radar mirror moved with the legacy spelling.
    assert!(s.map_fair_play.allow_mob_radar);
    // Keys the file never defined are untouched.
    assert!(s.map_fair_play.minimap_fair_play);
    assert_eq!(s.map_fair_play.minimap_mode, HostMinimapMode::FairPlay);
    assert!(!s.map_fair_play.allow_cave_map);
    assert!(s.map_fair_play.allow_waypoint_teleport);
    assert_eq!(
        s.map_fair_play.player_radar_scope,
        HostPlayerRadarScope::PartyOnly
    );
}

#[test]
fn fair_play_toml_keys_parse_flat_and_under_section() {
    // Flat top-level keys (what hand-edited files may use).
    let (s, warnings) = ClientSettings::from_toml(
        "minimap_fair_play = true\n\
         allow_cave_map = false\n\
         allow_entity_radar = false\n\
         allow_waypoint_teleport = false\n",
    );
    assert!(warnings.is_empty());
    assert!(s.map_fair_play.minimap_fair_play);
    assert!(!s.map_fair_play.allow_cave_map);
    assert!(!s.map_fair_play.allow_entity_radar);
    assert!(!s.map_fair_play.allow_waypoint_teleport);

    // The same keys under the [host_map_fair_play] header to_toml emits.
    let (s, warnings) = ClientSettings::from_toml(
        "[host_map_fair_play]\n\
         minimap_fair_play = true\n\
         allow_entity_radar = false\n",
    );
    assert!(warnings.is_empty());
    assert!(s.map_fair_play.minimap_fair_play);
    assert!(!s.map_fair_play.allow_entity_radar);
    // Keys the section left out keep defaults.
    assert!(s.map_fair_play.allow_cave_map);
    assert!(s.map_fair_play.allow_waypoint_teleport);

    // Invalid boolean tokens warn and keep the default, like every other
    // bool key (file parsing degrades gracefully; the CLI fails closed).
    let (s, warnings) = ClientSettings::from_toml("minimap_fair_play = maybe\n");
    assert!(!s.map_fair_play.minimap_fair_play);
    assert!(warnings.iter().any(|w| w.contains("minimap_fair_play")));
}

// ---------------------------------------------------------------------------
// Xaero control signal
// ---------------------------------------------------------------------------

#[test]
fn xaero_control_string_is_deterministic() {
    // Disabled policy always resets, whatever the allow flags say.
    for allow in [true, false] {
        let fp = HostMapFairPlaySettings {
            minimap_fair_play: false,
            allow_cave_map: allow,
            allow_entity_radar: allow,
            allow_waypoint_teleport: allow,
            minimap_mode: HostMinimapMode::Unrestricted,
            allow_mob_radar: allow,
            player_radar_scope: HostPlayerRadarScope::All,
        };
        assert_eq!(fp.xaero_control_string(), XAERO_RESET_SIGNAL);
    }

    // Full fair play (cave map + radar both locked) emits the exact
    // fair-play signal the task pins.
    let fp = HostMapFairPlaySettings {
        minimap_fair_play: true,
        allow_cave_map: false,
        allow_entity_radar: false,
        allow_waypoint_teleport: false,
        minimap_mode: HostMinimapMode::FairPlay,
        allow_mob_radar: false,
        player_radar_scope: HostPlayerRadarScope::None,
    };
    assert_eq!(fp.xaero_control_string(), "§f§a§i§r§x§a§e§r§o");
    assert_eq!(fp.xaero_control_string(), XAERO_FAIR_PLAY_SIGNAL);

    // Partial lock: reset followed by the selective lock codes for the
    // blocked features.
    let fp = HostMapFairPlaySettings {
        minimap_fair_play: true,
        allow_cave_map: true,
        allow_entity_radar: false,
        allow_waypoint_teleport: false,
        minimap_mode: HostMinimapMode::FairPlay,
        allow_mob_radar: false,
        player_radar_scope: HostPlayerRadarScope::All,
    };
    let signal = fp.xaero_control_string();
    assert!(signal.starts_with(XAERO_RESET_SIGNAL));
    assert!(signal.contains(XAERO_NO_ENTITY_RADAR_SIGNAL));
    assert!(!signal.contains(XAERO_FAIR_PLAY_SIGNAL));

    // `Disabled` mode emits the strongest lock the Xaero protocol can
    // express; the granular mode itself rides the env var.
    let fp = HostMapFairPlaySettings {
        minimap_mode: HostMinimapMode::Disabled,
        ..HostMapFairPlaySettings::default()
    };
    assert_eq!(fp.xaero_control_string(), XAERO_FAIR_PLAY_SIGNAL);
}

// ---------------------------------------------------------------------------
// --fair-play / --allow-* CLI flags + IntegratedServerConfig env propagation
// ---------------------------------------------------------------------------

#[test]
fn fair_play_flag_locks_all_features_and_emits_fairxaero() {
    let dir = empty_scratch_dir("fp-all");
    let mut cfg = parse_args(&["--fair-play"]).unwrap();
    assert!(cfg.fair_play);
    cfg.resolve_client_settings_from(&dir).unwrap();

    let fp = cfg.client_settings.map_fair_play;
    assert!(fp.minimap_fair_play);
    assert!(!fp.allow_cave_map);
    assert!(!fp.allow_entity_radar);
    assert!(!fp.allow_waypoint_teleport);
    // The granular switches lock too: fair_play mode, mob radar off,
    // player radar fully hidden.
    assert_eq!(fp.minimap_mode, HostMinimapMode::FairPlay);
    assert!(!fp.allow_mob_radar);
    assert_eq!(fp.player_radar_scope, HostPlayerRadarScope::None);

    // The hosted-server config carries the exact policy: the pinned Xaero
    // control string plus deterministic env pairs.
    let config = IntegratedServerConfig::from_settings(&cfg.client_settings);
    assert_eq!(config.map_fair_play, fp);
    assert_eq!(config.xaero_control, "§f§a§i§r§x§a§e§r§o");

    let env = env_map(&config);
    assert_eq!(env.len(), 8);
    assert_eq!(env["MEGU_FAIR_PLAY"], "true");
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_CAVE_MAP"], "false");
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_ENTITY_RADAR"], "false");
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_WAYPOINT_TELEPORT"], "false");
    assert_eq!(env["MEGU_FAIR_PLAY_XAERO_SIGNAL"], "§f§a§i§r§x§a§e§r§o");
    assert_eq!(env["MEGU_MINIMAP_MODE"], "fair_play");
    assert_eq!(env["MEGU_ALLOW_MOB_RADAR"], "false");
    assert_eq!(env["MEGU_PLAYER_RADAR_SCOPE"], "none");
    // The public constant names line up with the emitted keys.
    assert_eq!(FAIR_PLAY_ENV_ENABLED, "MEGU_FAIR_PLAY");
    assert_eq!(FAIR_PLAY_ENV_ALLOW_CAVE_MAP, "MEGU_FAIR_PLAY_ALLOW_CAVE_MAP");
    assert_eq!(
        FAIR_PLAY_ENV_ALLOW_ENTITY_RADAR,
        "MEGU_FAIR_PLAY_ALLOW_ENTITY_RADAR"
    );
    assert_eq!(
        FAIR_PLAY_ENV_ALLOW_WAYPOINT_TELEPORT,
        "MEGU_FAIR_PLAY_ALLOW_WAYPOINT_TELEPORT"
    );
    assert_eq!(
        FAIR_PLAY_ENV_XAERO_SIGNAL,
        "MEGU_FAIR_PLAY_XAERO_SIGNAL"
    );
    assert_eq!(FAIR_PLAY_ENV_MINIMAP_MODE, "MEGU_MINIMAP_MODE");
    assert_eq!(FAIR_PLAY_ENV_ALLOW_MOB_RADAR, "MEGU_ALLOW_MOB_RADAR");
    assert_eq!(
        FAIR_PLAY_ENV_PLAYER_RADAR_SCOPE,
        "MEGU_PLAYER_RADAR_SCOPE"
    );

    // A default (non-fair-play) run resets the client's map-mod features.
    let mut cfg = parse_args::<&str>(&[]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let config = IntegratedServerConfig::from_settings(&cfg.client_settings);
    assert_eq!(config.xaero_control, "§r§e§s§e§t§x§a§e§r§o");
    let env = env_map(&config);
    assert_eq!(env["MEGU_FAIR_PLAY"], "false");
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_CAVE_MAP"], "true");
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_ENTITY_RADAR"], "true");
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_WAYPOINT_TELEPORT"], "true");
    assert_eq!(env["MEGU_MINIMAP_MODE"], "unrestricted");
    assert_eq!(env["MEGU_ALLOW_MOB_RADAR"], "true");
    assert_eq!(env["MEGU_PLAYER_RADAR_SCOPE"], "all");
}

#[test]
fn fair_play_allow_cave_map_selective_override() {
    let dir = empty_scratch_dir("fp-cave");
    let mut cfg = parse_args(&["--fair-play", "--allow-cave-map", "true"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();

    let fp = cfg.client_settings.map_fair_play;
    assert!(fp.minimap_fair_play);
    // Cave map selectively re-enabled...
    assert!(fp.allow_cave_map);
    // ...while entity radar and waypoint teleport stay locked.
    assert!(!fp.allow_entity_radar);
    assert!(!fp.allow_waypoint_teleport);

    let config = IntegratedServerConfig::from_settings(&cfg.client_settings);
    let env = env_map(&config);
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_CAVE_MAP"], "true");
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_ENTITY_RADAR"], "false");
    assert_eq!(env["MEGU_FAIR_PLAY_ALLOW_WAYPOINT_TELEPORT"], "false");
    // Partial lock no longer emits the blanket fair-play signal.
    assert_ne!(config.xaero_control, XAERO_FAIR_PLAY_SIGNAL);
    assert!(config.xaero_control.contains(XAERO_NO_ENTITY_RADAR_SIGNAL));

    // The inline `--flag=value` form resolves identically.
    let mut cfg = parse_args(&["--fair-play", "--allow-entity-radar=on"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert!(fp.allow_entity_radar);
    assert!(!fp.allow_cave_map);
    assert!(!fp.allow_waypoint_teleport);

    // An --allow-* flag alone does not switch fair play on.
    let mut cfg = parse_args(&["--allow-cave-map", "false"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert!(!fp.minimap_fair_play);
    assert!(!fp.allow_cave_map);
    assert_eq!(
        IntegratedServerConfig::from_settings(&cfg.client_settings).xaero_control,
        XAERO_RESET_SIGNAL
    );
}

#[test]
fn fair_play_toml_file_and_cli_override_chain() {
    let dir = empty_scratch_dir("fp-chain");
    let file = dir.join("custom.toml");
    std::fs::write(
        &file,
        "minimap_fair_play = true\nallow_cave_map = true\nallow_entity_radar = true\n",
    )
    .unwrap();

    // TOML alone drives the resolved policy.
    let mut cfg = parse_args(&["--settings", file.to_str().unwrap()]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert!(fp.minimap_fair_play);
    assert!(fp.allow_cave_map);
    assert!(fp.allow_entity_radar);
    assert!(fp.allow_waypoint_teleport);

    // --fair-play re-locks the three allowances the TOML had allowed...
    let mut cfg = parse_args(&[
        "--settings",
        file.to_str().unwrap(),
        "--fair-play",
    ])
    .unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert!(fp.minimap_fair_play);
    assert!(!fp.allow_cave_map);
    assert!(!fp.allow_entity_radar);
    assert!(!fp.allow_waypoint_teleport);

    // ...and an --allow-* CLI flag re-enables its own feature on top.
    let mut cfg = parse_args(&[
        "--settings",
        file.to_str().unwrap(),
        "--fair-play",
        "--allow-waypoint-teleport",
        "true",
    ])
    .unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert!(fp.allow_waypoint_teleport);
    assert!(!fp.allow_cave_map);
    assert!(!fp.allow_entity_radar);
}

#[test]
fn fair_play_flags_fail_closed_on_bad_booleans() {
    for argv in [
        vec!["--allow-cave-map", "maybe"],
        vec!["--allow-entity-radar", "2"],
        vec!["--allow-entity-radar", "onfire"],
        vec!["--allow-waypoint-teleport", "yesn't"],
    ] {
        let err = parse_args(&argv).unwrap_err();
        assert!(err.contains("must be"), "{argv:?} -> {err}");
    }
    // A missing value fails closed too.
    for argv in [
        vec!["--allow-cave-map"],
        vec!["--allow-entity-radar"],
        vec!["--allow-waypoint-teleport"],
    ] {
        assert!(parse_args(&argv).is_err(), "{argv:?} must fail");
    }
    // The accepted token set matches the other on/off player flags.
    for (token, want) in [
        ("true", true),
        ("on", true),
        ("yes", true),
        ("1", true),
        ("false", false),
        ("off", false),
        ("no", false),
        ("0", false),
    ] {
        let cfg = parse_args(&["--allow-cave-map", token]).unwrap();
        assert_eq!(cfg.allow_cave_map, Some(want), "{token}");
    }
}

// ---------------------------------------------------------------------------
// Granular 5-switch policy (--minimap-mode / --allow-mob-radar / --player-radar)
// ---------------------------------------------------------------------------

#[test]
fn fair_play_with_player_radar_party_only() {
    // `--fair-play --player-radar party_only`: minimap ON (fair_play
    // mode) + cave/mob radar OFF + party/team player radar ON.
    let dir = empty_scratch_dir("fp-party");
    let mut cfg = parse_args(&["--fair-play", "--player-radar", "party_only"]).unwrap();
    assert_eq!(cfg.player_radar, Some(HostPlayerRadarScope::PartyOnly));
    cfg.resolve_client_settings_from(&dir).unwrap();

    let fp = cfg.client_settings.map_fair_play;
    assert!(fp.minimap_fair_play);
    assert_eq!(fp.minimap_mode, HostMinimapMode::FairPlay);
    assert!(!fp.allow_cave_map);
    assert!(!fp.allow_entity_radar);
    assert!(!fp.allow_mob_radar);
    assert_eq!(fp.player_radar_scope, HostPlayerRadarScope::PartyOnly);
    assert!(!fp.allow_waypoint_teleport);

    let config = IntegratedServerConfig::from_settings(&cfg.client_settings);
    let env = env_map(&config);
    assert_eq!(env["MEGU_FAIR_PLAY"], "true");
    assert_eq!(env["MEGU_MINIMAP_MODE"], "fair_play");
    assert_eq!(env["MEGU_ALLOW_MOB_RADAR"], "false");
    assert_eq!(env["MEGU_PLAYER_RADAR_SCOPE"], "party_only");
    // Cave map + mob radar both locked -> the blanket fair-play signal.
    assert_eq!(env["MEGU_FAIR_PLAY_XAERO_SIGNAL"], XAERO_FAIR_PLAY_SIGNAL);

    // Every player-radar token parses, case-insensitively.
    for (token, want) in [
        ("all", HostPlayerRadarScope::All),
        ("party_only", HostPlayerRadarScope::PartyOnly),
        ("none", HostPlayerRadarScope::None),
        ("PARTY_ONLY", HostPlayerRadarScope::PartyOnly),
    ] {
        let cfg = parse_args(&["--player-radar", token]).unwrap();
        assert_eq!(cfg.player_radar, Some(want), "{token}");
    }
}

#[test]
fn minimap_mode_flag_parses_and_disables() {
    let dir = empty_scratch_dir("fp-mode");
    // `--minimap-mode disabled` blocks map mods entirely: the master
    // switch mirrors the mode and the hosted server gets the strongest
    // Xaero lock available plus the granular env var.
    let mut cfg = parse_args(&["--minimap-mode", "disabled"]).unwrap();
    assert_eq!(cfg.minimap_mode, Some(HostMinimapMode::Disabled));
    cfg.resolve_client_settings_from(&dir).unwrap();

    let fp = cfg.client_settings.map_fair_play;
    assert_eq!(fp.minimap_mode, HostMinimapMode::Disabled);
    assert!(fp.minimap_fair_play);
    // The allow switches keep their permissive defaults — the mode alone
    // carries the restriction.
    assert!(fp.allow_cave_map);
    assert!(fp.allow_mob_radar);

    let config = IntegratedServerConfig::from_settings(&cfg.client_settings);
    assert_eq!(config.xaero_control, XAERO_FAIR_PLAY_SIGNAL);
    let env = env_map(&config);
    assert_eq!(env["MEGU_FAIR_PLAY"], "true");
    assert_eq!(env["MEGU_MINIMAP_MODE"], "disabled");

    // `unrestricted` is the explicit no-restriction spelling.
    let mut cfg = parse_args(&["--minimap-mode", "unrestricted"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert_eq!(fp.minimap_mode, HostMinimapMode::Unrestricted);
    assert!(!fp.minimap_fair_play);
    assert_eq!(
        IntegratedServerConfig::from_settings(&cfg.client_settings).xaero_control,
        XAERO_RESET_SIGNAL
    );

    // `fair_play` alone gates the map features by the allow switches,
    // which keep their permissive defaults — mirroring the server, where
    // minimap-mode=fair_play without allow-* keys restricts nothing.
    let mut cfg = parse_args(&["--minimap-mode", "fair_play"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert_eq!(fp.minimap_mode, HostMinimapMode::FairPlay);
    assert!(fp.minimap_fair_play);
    assert!(fp.allow_cave_map);
    assert!(fp.allow_mob_radar);
    assert_eq!(fp.player_radar_scope, HostPlayerRadarScope::All);
    assert_eq!(
        IntegratedServerConfig::from_settings(&cfg.client_settings).xaero_control,
        XAERO_RESET_SIGNAL
    );

    // Every token parses, case-insensitively; the inline form works.
    for (token, want) in [
        ("unrestricted", HostMinimapMode::Unrestricted),
        ("fair_play", HostMinimapMode::FairPlay),
        ("disabled", HostMinimapMode::Disabled),
        ("FAIR_PLAY", HostMinimapMode::FairPlay),
    ] {
        let cfg = parse_args(&["--minimap-mode", token]).unwrap();
        assert_eq!(cfg.minimap_mode, Some(want), "{token}");
    }
    let cfg = parse_args(&["--minimap-mode=disabled"]).unwrap();
    assert_eq!(cfg.minimap_mode, Some(HostMinimapMode::Disabled));
}

#[test]
fn mob_radar_flag_aliases_entity_radar() {
    let dir = empty_scratch_dir("fp-mob");
    // `--allow-mob-radar` and `--allow-entity-radar` write the same
    // switch; both mirrors stay equal no matter which flag is used.
    let mut cfg = parse_args(&["--allow-mob-radar", "false"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert!(!fp.allow_mob_radar);
    assert!(!fp.allow_entity_radar);

    let mut cfg = parse_args(&["--allow-entity-radar", "true"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert!(fp.allow_mob_radar);
    assert!(fp.allow_entity_radar);

    // Both spellings in one invocation: the server-parity flag wins.
    let mut cfg =
        parse_args(&["--allow-entity-radar", "true", "--allow-mob-radar", "false"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let fp = cfg.client_settings.map_fair_play;
    assert!(!fp.allow_mob_radar);
    assert!(!fp.allow_entity_radar);
}

#[test]
fn granular_toml_keys_parse_and_overlay() {
    // The granular spellings parse flat and under the section header.
    let (s, warnings) = ClientSettings::from_toml(
        "[host_map_fair_play]\n\
         minimap_mode = \"disabled\"\n\
         allow_mob_radar = false\n\
         player_radar_scope = \"party_only\"\n",
    );
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    let fp = s.map_fair_play;
    assert_eq!(fp.minimap_mode, HostMinimapMode::Disabled);
    // A non-unrestricted mode mirrors into the legacy master switch.
    assert!(fp.minimap_fair_play);
    assert!(!fp.allow_mob_radar);
    assert!(!fp.allow_entity_radar);
    assert_eq!(fp.player_radar_scope, HostPlayerRadarScope::PartyOnly);
    // Untouched keys keep permissive defaults.
    assert!(fp.allow_cave_map);
    assert!(fp.allow_waypoint_teleport);

    // The legacy `minimap_fair_play` bool maps onto the mode: `true`
    // reads as fair_play, `false` as unrestricted.
    let (s, warnings) = ClientSettings::from_toml("minimap_fair_play = true\n");
    assert!(warnings.is_empty());
    assert!(s.map_fair_play.minimap_fair_play);
    assert_eq!(s.map_fair_play.minimap_mode, HostMinimapMode::FairPlay);
    let (s, warnings) = ClientSettings::from_toml(
        "minimap_mode = \"disabled\"\nminimap_fair_play = false\n",
    );
    assert!(warnings.is_empty());
    // Last key wins, exactly like server.properties.
    assert_eq!(s.map_fair_play.minimap_mode, HostMinimapMode::Unrestricted);
    assert!(!s.map_fair_play.minimap_fair_play);

    // Invalid enum tokens warn and keep the permissive default, like
    // every other named-option key in this file.
    let (s, warnings) = ClientSettings::from_toml(
        "minimap_mode = \"bogus\"\nplayer_radar_scope = \"everyone\"\n",
    );
    assert_eq!(s.map_fair_play.minimap_mode, HostMinimapMode::Unrestricted);
    assert_eq!(s.map_fair_play.player_radar_scope, HostPlayerRadarScope::All);
    assert!(warnings.iter().any(|w| w.contains("minimap_mode")));
    assert!(warnings.iter().any(|w| w.contains("player_radar_scope")));
}

#[test]
fn granular_flags_fail_closed_on_bad_values() {
    for argv in [
        vec!["--minimap-mode", "bogus"],
        vec!["--minimap-mode", "on"],
        vec!["--player-radar", "friends"],
        vec!["--player-radar", "2"],
        vec!["--allow-mob-radar", "maybe"],
    ] {
        let err = parse_args(&argv).unwrap_err();
        assert!(
            err.contains("invalid") || err.contains("must be"),
            "{argv:?} -> {err}"
        );
    }
    // A missing value fails closed too.
    for argv in [
        vec!["--minimap-mode"],
        vec!["--player-radar"],
        vec!["--allow-mob-radar"],
    ] {
        assert!(parse_args(&argv).is_err(), "{argv:?} must fail");
    }
}

// ---------------------------------------------------------------------------
// --auto-tune
// ---------------------------------------------------------------------------

#[test]
fn auto_tune_seeds_settings_from_probed_hardware() {
    let dir = empty_scratch_dir("auto-tune");
    let plan = recommend_tuning(&probe_host_hardware());
    let mut expected = ClientSettings::default();
    plan.apply_to(&mut expected);

    let mut cfg = parse_args(&["--auto-tune"]).unwrap();
    assert!(cfg.auto_tune);
    cfg.resolve_client_settings_from(&dir).unwrap();

    let s = &cfg.client_settings;
    assert_eq!(s.view_distance, expected.view_distance);
    assert_eq!(s.gpu_backend, expected.gpu_backend);
    assert_eq!(s.distant_horizon, expected.distant_horizon);
    // The tuned view distance folds back into the session chunk window the
    // same way a settings file's view_distance does.
    assert_eq!(cfg.view_distance, expected.view_distance as i8);
    assert!(cfg.settings_source_present);
}

#[test]
fn auto_tune_explicit_flags_and_toml_always_win() {
    let dir = empty_scratch_dir("auto-tune-override");
    let plan = recommend_tuning(&probe_host_hardware());
    let mut expected = ClientSettings::default();
    plan.apply_to(&mut expected);

    // Explicit flags strictly override the auto-tuned fields.
    let mut cfg = parse_args(&[
        "--auto-tune",
        "--view-distance",
        "9",
        "--gpu-backend",
        "classic",
        "--background-fps",
        "33",
    ])
    .unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    let s = &cfg.client_settings;
    assert_eq!(s.view_distance, 9);
    assert_eq!(s.gpu_backend, RenderBackendPreference::Classic);
    assert_eq!(s.background_fps_limit, 33);
    assert_eq!(cfg.view_distance, 9);
    // Fields the explicit flags do not cover keep the tuned values.
    assert_eq!(s.distant_horizon, expected.distant_horizon);

    // A TOML key overrides the auto-tuned value too; keys the file omits
    // keep the tuned defaults.
    let file = dir.join("custom.toml");
    std::fs::write(&file, "view_distance = 24\n").unwrap();
    let mut cfg = parse_args(&[
        "--auto-tune",
        "--settings",
        file.to_str().unwrap(),
    ])
    .unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(cfg.client_settings.view_distance, 24);
    assert_eq!(cfg.client_settings.gpu_backend, expected.gpu_backend);

    // Auto-tune composes with fair-play flags: the tuned graphics fields
    // apply and the host policy resolves independently.
    let mut cfg = parse_args(&["--auto-tune", "--fair-play"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(cfg.client_settings.view_distance, expected.view_distance);
    assert!(cfg.client_settings.map_fair_play.minimap_fair_play);
    assert!(!cfg.client_settings.map_fair_play.allow_entity_radar);
}

// ---------------------------------------------------------------------------
// Help text
// ---------------------------------------------------------------------------

#[test]
fn help_text_documents_fair_play_and_auto_tune_flags() {
    let help = help_text();
    for needle in [
        "--fair-play",
        "--minimap-mode",
        "--allow-cave-map",
        "--allow-entity-radar",
        "--allow-mob-radar",
        "--player-radar",
        "--allow-waypoint-teleport",
        "--auto-tune",
        "MEGU_FAIR_PLAY",
        "MEGU_MINIMAP_MODE",
        "MEGU_ALLOW_MOB_RADAR",
        "MEGU_PLAYER_RADAR_SCOPE",
    ] {
        assert!(help.contains(needle), "help text missing {needle}");
    }
}
