// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_kernel::{
    debug_chords::{
        build_chunk_border_lines, build_entity_hitbox_lines, finish_gamemode_switcher,
        format_advanced_tooltip_lines, handle_f3_chord, DebugChordAction, DebugChordContext,
        DebugChordState, DebugTarget, F3Chord, GameMode, PlayerDebugInfo, ALL_F3_CHORDS,
    },
    Aabb,
};

fn player(gamemode: GameMode) -> PlayerDebugInfo<'static> {
    PlayerDebugInfo {
        dimension: "minecraft:overworld",
        position: [12.345, 64.0, -8.125],
        yaw_deg: 90.25,
        pitch_deg: -35.5,
        gamemode,
    }
}

#[test]
fn all_fifteen_key_codes_and_help_entries_are_unique_and_reachable() {
    assert_eq!(ALL_F3_CHORDS.len(), 15);
    let mut keys = std::collections::HashSet::new();
    for chord in ALL_F3_CHORDS {
        assert!(keys.insert(chord.glfw_key()));
        assert_eq!(F3Chord::from_glfw_key(chord.glfw_key()), Some(chord));
        assert!(chord.help_line().contains("F3 +"));
    }
    assert_eq!(F3Chord::from_glfw_key(999), None);

    let mut state = DebugChordState::default();
    let mut ctx = DebugChordContext::new(&mut state);
    assert_eq!(
        handle_f3_chord(F3Chord::Q, &mut ctx),
        DebugChordAction::HelpLines(
            ALL_F3_CHORDS
                .iter()
                .map(|key| key.help_line().to_string())
                .collect()
        )
    );
    ctx.localized_help = Some(&|chord, fallback| format!("{fallback} [{}]", chord.glfw_key()));
    let DebugChordAction::HelpLines(localized) = handle_f3_chord(F3Chord::Q, &mut ctx) else {
        panic!("F3+Q must emit help lines");
    };
    assert_eq!(localized.len(), 15);
    assert!(localized.iter().all(|line| line.ends_with(']')));
}

#[test]
fn every_toggle_and_effect_matches_its_f3_binding() {
    let mut state = DebugChordState::default();
    let mut ctx = DebugChordContext::new(&mut state);

    assert_eq!(
        handle_f3_chord(F3Chord::A, &mut ctx),
        DebugChordAction::ReloadChunks
    );
    assert_eq!(
        handle_f3_chord(F3Chord::D, &mut ctx),
        DebugChordAction::ClearChat
    );
    assert_eq!(
        handle_f3_chord(F3Chord::T, &mut ctx),
        DebugChordAction::ReloadResourcePacks
    );

    for (key, expected) in [
        (F3Chord::B, "Hitboxes: shown"),
        (F3Chord::G, "Chunk borders: shown"),
        (F3Chord::H, "Advanced tooltips: shown"),
    ] {
        assert_eq!(
            handle_f3_chord(key, &mut ctx),
            DebugChordAction::ChatFeedback(expected.into())
        );
    }
    assert!(ctx.state.show_hitboxes);
    assert!(ctx.state.show_chunk_borders);
    assert!(ctx.state.advanced_tooltips);
    assert_eq!(
        handle_f3_chord(F3Chord::B, &mut ctx),
        DebugChordAction::ChatFeedback("Hitboxes: hidden".into())
    );
    assert_eq!(
        handle_f3_chord(F3Chord::G, &mut ctx),
        DebugChordAction::ChatFeedback("Chunk borders: hidden".into())
    );
    assert_eq!(
        handle_f3_chord(F3Chord::H, &mut ctx),
        DebugChordAction::ChatFeedback("Advanced tooltips: hidden".into())
    );
    assert!(!ctx.state.show_hitboxes);
    assert!(!ctx.state.show_chunk_borders);
    assert!(!ctx.state.advanced_tooltips);

    assert!(ctx.state.pause_on_lost_focus);
    assert_eq!(
        handle_f3_chord(F3Chord::P, &mut ctx),
        DebugChordAction::ChatFeedback("Pause on lost focus: hidden".into())
    );
    assert!(!ctx.state.pause_on_lost_focus);
    handle_f3_chord(F3Chord::P, &mut ctx);
    assert!(ctx.state.pause_on_lost_focus);

    assert!(!ctx.state.show_debug_overlay);
    handle_f3_chord(F3Chord::Key1, &mut ctx);
    assert!(ctx.state.pie_chart);
    assert!(ctx.state.show_debug_overlay);
    handle_f3_chord(F3Chord::Key1, &mut ctx);
    assert!(!ctx.state.pie_chart);
    handle_f3_chord(F3Chord::Key2, &mut ctx);
    assert!(ctx.state.fps_tps_chart);
    assert!(!ctx.state.net_chart);
    handle_f3_chord(F3Chord::Key3, &mut ctx);
    assert!(ctx.state.net_chart);
    assert!(!ctx.state.fps_tps_chart);
    handle_f3_chord(F3Chord::Key3, &mut ctx);
    assert!(!ctx.state.net_chart);
    ctx.state.toggle_debug_overlay();
    assert!(!ctx.state.show_debug_overlay);
    handle_f3_chord(F3Chord::Key2, &mut ctx);
    assert!(ctx.state.fps_tps_chart);
    assert!(ctx.state.show_debug_overlay);
}

#[test]
fn copy_location_and_target_recreation_commands_use_vanilla_formats() {
    let mut state = DebugChordState::default();
    let mut ctx = DebugChordContext::new(&mut state);
    ctx.player = Some(player(GameMode::Survival));

    assert_eq!(
        handle_f3_chord(F3Chord::C, &mut ctx),
        DebugChordAction::ClipboardCommand(
            "/execute in minecraft:overworld run tp @s 12.35 64.00 -8.12 90.25 -35.50".into()
        )
    );

    ctx.target = Some(DebugTarget::Block {
        position: [-1, 64, 20],
        block_state: "minecraft:oak_stairs[facing=east,half=bottom]",
        nbt: None,
    });
    assert_eq!(
        handle_f3_chord(F3Chord::I, &mut ctx),
        DebugChordAction::ClipboardCommand(
            "/setblock -1 64 20 minecraft:oak_stairs[facing=east,half=bottom]".into()
        )
    );
    ctx.target = Some(DebugTarget::Block {
        position: [2, -64, 3],
        block_state: "minecraft:chest",
        nbt: Some("{CustomName:'\"Storage\"'}"),
    });
    assert_eq!(
        handle_f3_chord(F3Chord::I, &mut ctx),
        DebugChordAction::ClipboardCommand(
            "/setblock 2 -64 3 minecraft:chest{CustomName:'\"Storage\"'}".into()
        )
    );
    ctx.target = Some(DebugTarget::Entity {
        entity_id: "minecraft:zombie",
        position: [1.25, 65.0, -3.75],
        nbt: Some("{NoAI:1b}"),
    });
    assert_eq!(
        handle_f3_chord(F3Chord::I, &mut ctx),
        DebugChordAction::ClipboardCommand(
            "/summon minecraft:zombie 1.25 65.00 -3.75 {NoAI:1b}".into()
        )
    );

    ctx.reduced_debug_info = true;
    assert_eq!(
        handle_f3_chord(F3Chord::C, &mut ctx),
        DebugChordAction::ChatFeedback("Location unavailable".into())
    );
    assert_eq!(
        handle_f3_chord(F3Chord::I, &mut ctx),
        DebugChordAction::ChatFeedback("Target data unavailable".into())
    );
    ctx.reduced_debug_info = false;
    ctx.target = None;
    assert_eq!(
        handle_f3_chord(F3Chord::I, &mut ctx),
        DebugChordAction::ChatFeedback("Target data unavailable".into())
    );
    ctx.player = Some(PlayerDebugInfo {
        yaw_deg: f64::NAN,
        ..player(GameMode::Survival)
    });
    assert_eq!(
        handle_f3_chord(F3Chord::C, &mut ctx),
        DebugChordAction::ChatFeedback("Location unavailable".into())
    );
}

#[test]
fn f4_cycles_all_four_modes_and_applies_only_on_release() {
    let mut state = DebugChordState::default();
    let mut ctx = DebugChordContext::new(&mut state);
    ctx.player = Some(player(GameMode::Survival));
    assert!(matches!(
        handle_f3_chord(F3Chord::F4, &mut ctx),
        DebugChordAction::ChatFeedback(_)
    ));
    assert_eq!(ctx.state.gamemode_switcher, None);

    ctx.has_permission_level_2 = true;
    for selected in [
        GameMode::Creative,
        GameMode::Survival,
        GameMode::Adventure,
        GameMode::Spectator,
        GameMode::Creative,
    ] {
        assert!(matches!(
            handle_f3_chord(F3Chord::F4, &mut ctx),
            DebugChordAction::ChatFeedback(_)
        ));
        assert_eq!(ctx.state.gamemode_switcher, Some(selected));
    }
    assert_eq!(
        finish_gamemode_switcher(ctx.state, GameMode::Survival),
        Some(DebugChordAction::SendGameModeCommand(GameMode::Creative))
    );
    assert_eq!(ctx.state.gamemode_switcher, None);
    handle_f3_chord(F3Chord::F4, &mut ctx);
    assert_eq!(
        finish_gamemode_switcher(ctx.state, GameMode::Creative),
        None
    );
}

#[test]
fn spectator_shortcut_uses_saved_mode_and_creative_fallback() {
    let mut state = DebugChordState::default();
    let mut ctx = DebugChordContext::new(&mut state);
    ctx.player = Some(player(GameMode::Adventure));
    assert!(matches!(
        handle_f3_chord(F3Chord::N, &mut ctx),
        DebugChordAction::ChatFeedback(_)
    ));
    ctx.has_permission_level_2 = true;
    assert_eq!(
        handle_f3_chord(F3Chord::N, &mut ctx),
        DebugChordAction::SendGameModeCommand(GameMode::Spectator)
    );
    assert_eq!(ctx.state.previous_gamemode, Some(GameMode::Adventure));
    ctx.player = Some(player(GameMode::Spectator));
    assert_eq!(
        handle_f3_chord(F3Chord::N, &mut ctx),
        DebugChordAction::SendGameModeCommand(GameMode::Adventure)
    );
    ctx.state.previous_gamemode = None;
    assert_eq!(
        handle_f3_chord(F3Chord::N, &mut ctx),
        DebugChordAction::SendGameModeCommand(GameMode::Creative)
    );
}

#[test]
fn chunk_borders_include_red_neighborhood_corners_two_block_grid_and_sections() {
    let lines = build_chunk_border_lines(-2, 3, -64, -32);
    assert!(!lines.is_empty());
    assert!(lines.iter().all(|line| line
        .start
        .iter()
        .chain(line.end.iter())
        .all(|n| n.is_finite())));
    assert!(lines.iter().all(|line| {
        [line.start, line.end].iter().all(|point| {
            (-48.0..=0.0).contains(&point[0])
                && (-64.0..=-32.0).contains(&point[1])
                && (32.0..=96.0).contains(&point[2])
        })
    }));
    let red: Vec<_> = lines
        .iter()
        .filter(|line| line.rgba == [255, 0, 0, 128])
        .collect();
    assert_eq!(red.len(), 16);
    assert!(red
        .iter()
        .any(|line| line.start == [-32.0, -64.0, 48.0] && line.end == [-32.0, -32.0, 48.0]));
    assert!(lines
        .iter()
        .any(|line| line.rgba == [255, 255, 0, 255] && line.start[1] == -62.0));
    assert!(lines
        .iter()
        .any(|line| line.rgba == [0, 155, 155, 255] && line.start[1] == -56.0));
    assert!(lines
        .iter()
        .any(|line| line.rgba == [64, 64, 255, 255] && line.start[1] == -48.0));
    assert!(build_chunk_border_lines(0, 0, 10, 9).is_empty());
    assert!(build_chunk_border_lines(i32::MAX, i32::MIN, 0, 16)
        .iter()
        .all(|line| {
            line.start
                .iter()
                .chain(line.end.iter())
                .all(|v| v.is_finite())
        }));
}

#[test]
fn entity_wireframe_has_twelve_white_four_red_and_one_blue_segment() {
    let box_ = Aabb {
        min: [1.0, 2.0, 3.0],
        max: [3.0, 4.0, 5.0],
    };
    let lines = build_entity_hitbox_lines(box_, 3.75, 0.0, 0.0, true);
    assert_eq!(lines.len(), 17);
    assert_eq!(
        lines.iter().filter(|line| line.rgba == [255; 4]).count(),
        12
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.rgba == [255, 0, 0, 255])
            .count(),
        4
    );
    let ray = lines
        .iter()
        .find(|line| line.rgba == [0, 0, 255, 255])
        .unwrap();
    assert_eq!(ray.start, [2.0, 3.75, 4.0]);
    assert_eq!(ray.end, [2.0, 3.75, 6.0]);

    let nan_lines = build_entity_hitbox_lines(box_, f64::NAN, f32::NAN, f32::INFINITY, true);
    assert_eq!(nan_lines.len(), 17);
    assert!(nan_lines.iter().all(|line| line
        .start
        .iter()
        .chain(line.end.iter())
        .all(|n| n.is_finite())));
    let nonliving = build_entity_hitbox_lines(box_, 3.0, 90.0, 0.0, false);
    assert_eq!(nonliving.len(), 13);
    let ray = nonliving.last().unwrap();
    assert!((ray.end[0] - ray.start[0] + 2.0).abs() < 0.0001);
    assert!(build_entity_hitbox_lines(
        Aabb {
            min: [0.0, f64::INFINITY, 0.0],
            max: [1.0, 2.0, 1.0]
        },
        0.0,
        0.0,
        0.0,
        true
    )
    .is_empty());
}

#[test]
fn advanced_tooltips_keep_exact_durability_and_dark_gray_metadata() {
    assert_eq!(
        format_advanced_tooltip_lines("minecraft:diamond_pickaxe", 10, 1561, 4),
        [
            "Durability: 1551 / 1561",
            "§8minecraft:diamond_pickaxe",
            "§84 component(s)"
        ]
    );
    assert_eq!(
        format_advanced_tooltip_lines("minecraft:stone", 0, 0, 0),
        ["§8minecraft:stone"]
    );
    assert_eq!(
        format_advanced_tooltip_lines("mod:item", 0, 20, 1),
        ["§8mod:item", "§81 component(s)"]
    );
    assert_eq!(
        format_advanced_tooltip_lines("minecraft:tool", 50, 20, 0),
        ["Durability: 0 / 20", "§8minecraft:tool"]
    );
}
