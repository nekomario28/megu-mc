// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_kernel::{
    evaluate_sky_and_fog, lerp_angle_deg, megu_interp_entities_batch,
    megu_interp_table_free, megu_interp_table_new, megu_sky_fog_eval,
    EntityInterpolationTable, Error, InterpEntityUpdate, InterpPoseOut, SkyFogInput,
    SkyFogInputC, SkyFogOutputC, INTERP_UPDATE_POSITION, INTERP_UPDATE_REMOVE,
    INTERP_UPDATE_ROTATION, INTERP_UPDATE_SNAP,
};
use std::ptr;

fn pose_bits(pose: &megu_kernel::InterpolatedPose) -> ([u64; 3], u32, u32, u32) {
    (
        [
            pose.position[0].to_bits(),
            pose.position[1].to_bits(),
            pose.position[2].to_bits(),
        ],
        pose.yaw.to_bits(),
        pose.pitch.to_bits(),
        pose.head_yaw.to_bits(),
    )
}

#[test]
fn lerp_angle_deg_takes_the_shortest_arc() {
    // 350° -> 10° is +20°, never the 340° spin backward through 180°.
    // Results are normalized to (-180, 180], so 350° reads as -10°.
    assert_eq!(lerp_angle_deg(350.0, 10.0, 0.0), -10.0);
    assert_eq!(lerp_angle_deg(350.0, 10.0, 0.5), 0.0);
    assert_eq!(lerp_angle_deg(350.0, 10.0, 1.0), 10.0);
    for i in 0..=8 {
        let t = i as f32 / 8.0;
        let angle = lerp_angle_deg(350.0, 10.0, t);
        assert!((-10.0 - 1e-6..=10.0 + 1e-6).contains(&angle), "t={t} angle={angle}");
    }
    // The reverse direction wraps the other way but still stays short.
    assert_eq!(lerp_angle_deg(10.0, 350.0, 0.5), 0.0);
}

#[test]
fn three_tick_catchup_converges_without_overshoot_and_teleport_snaps() {
    let mut table = EntityInterpolationTable::new();
    table
        .snap_entity(42, [0.0, 64.0, 0.0], 0.0, 0.0, 0.0)
        .unwrap();
    table.advance_tick();
    table.update_position(42, [3.0, 64.0, 0.0]).unwrap();
    table.update_rotation(42, 90.0, 0.0, -170.0).unwrap();

    // Fresh packet: catch-up spans exactly three ticks.
    let mut prev_x = table.sample(42, 0.0).unwrap().position[0];
    assert_eq!(prev_x, 0.0);
    for _ in 0..3 {
        for step in 0..=8 {
            let t = step as f32 / 8.0;
            let x = table.sample(42, t).unwrap().position[0];
            assert!(x >= prev_x - 1e-9 && x <= 3.0 + 1e-9, "overshoot at t={t}");
            prev_x = x;
        }
        table.advance_tick();
    }
    let done = table.sample(42, 0.37).unwrap();
    assert_eq!(done.position, [3.0, 64.0, 0.0]);
    assert_eq!(done.yaw, 90.0);
    assert_eq!(done.head_yaw, -170.0);

    // A >16 block move is a teleport: the next sample is already there.
    table.update_position(42, [40.0, 64.0, 0.0]).unwrap();
    assert_eq!(table.sample(42, 0.0).unwrap().position[0], 40.0);
    assert_eq!(table.sample(42, 0.99).unwrap().position[0], 40.0);

    // Despawn removes the entity cleanly.
    assert!(table.remove_entity(42));
    assert!(!table.remove_entity(42));
    assert!(table.sample(42, 0.5).is_none());
    assert!(matches!(
        table.update_rotation(42, 0.0, 0.0, 0.0),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        table.update_position(42, [f64::NAN; 3]),
        Err(Error::InvalidArgument)
    ));
}

fn sky_input() -> SkyFogInput {
    SkyFogInput {
        day_time: 6_000,
        rain_level: 0.0,
        thunder_level: 0.0,
        view_distance: 128.0,
        biome_fog_color: [0.62, 0.71, 0.83],
        biome_fog_color_next: [0.62, 0.71, 0.83],
        biome_blend: 0.0,
        underwater: false,
        in_lava: false,
        powder_snow: false,
        blindness: 0.0,
        darkness: 0.0,
    }
}

#[test]
fn sky_and_fog_evaluate_day_night_weather_and_mediums() {
    let noon = evaluate_sky_and_fog(&sky_input());
    assert_eq!(noon.celestial_angle, 0.25);
    assert_eq!(noon.star_brightness, 0.0);
    assert!(noon.sky_rgb[2] > noon.sky_rgb[0] && noon.sky_rgb[1] > noon.sky_rgb[0]);
    assert!(noon.sky_rgb.iter().sum::<f32>() > 1.5);
    assert_eq!(noon.fog_end, 128.0);

    let midnight = evaluate_sky_and_fog(&SkyFogInput {
        day_time: 18_000,
        ..sky_input()
    });
    assert_eq!(midnight.celestial_angle, 0.75);
    assert!(midnight.star_brightness > 0.0);
    assert!(midnight.sky_rgb.iter().sum::<f32>() < 0.2);

    let thunder = evaluate_sky_and_fog(&SkyFogInput {
        rain_level: 1.0,
        thunder_level: 1.0,
        ..sky_input()
    });
    assert!(thunder.sky_rgb.iter().sum::<f32>() < noon.sky_rgb.iter().sum::<f32>());
    assert!(thunder.star_brightness == 0.0);

    let water = evaluate_sky_and_fog(&SkyFogInput {
        underwater: true,
        ..sky_input()
    });
    assert!(water.fog_end < noon.fog_end);
    let lava = evaluate_sky_and_fog(&SkyFogInput {
        in_lava: true,
        ..sky_input()
    });
    assert!(lava.fog_end < water.fog_end);
    let snow = evaluate_sky_and_fog(&SkyFogInput {
        powder_snow: true,
        ..sky_input()
    });
    assert!(snow.fog_end < water.fog_end);

    let blind = evaluate_sky_and_fog(&SkyFogInput {
        blindness: 1.0,
        ..sky_input()
    });
    assert!(blind.fog_end < noon.fog_end);
    assert!(blind.fog_start <= blind.fog_end);

    // Biome border: fog colour blends linearly between the two biome samples.
    let a = evaluate_sky_and_fog(&SkyFogInput {
        biome_fog_color: [0.1, 0.2, 0.3],
        biome_fog_color_next: [0.7, 0.6, 0.5],
        biome_blend: 0.0,
        ..sky_input()
    })
    .fog_rgb;
    let b = evaluate_sky_and_fog(&SkyFogInput {
        biome_fog_color: [0.1, 0.2, 0.3],
        biome_fog_color_next: [0.7, 0.6, 0.5],
        biome_blend: 1.0,
        ..sky_input()
    })
    .fog_rgb;
    let mid = evaluate_sky_and_fog(&SkyFogInput {
        biome_fog_color: [0.1, 0.2, 0.3],
        biome_fog_color_next: [0.7, 0.6, 0.5],
        biome_blend: 0.5,
        ..sky_input()
    })
    .fog_rgb;
    for i in 0..3 {
        assert!((mid[i] - (a[i] + b[i]) * 0.5).abs() < 1e-6);
    }
}

fn update(id: i32, kind: u8, position: [f64; 3]) -> InterpEntityUpdate {
    InterpEntityUpdate {
        entity_id: id,
        kind,
        position,
        yaw: 15.0,
        pitch: -5.0,
        head_yaw: 350.0,
    }
}

fn empty_pose() -> InterpPoseOut {
    InterpPoseOut {
        position: [0.0; 3],
        yaw: 0.0,
        pitch: 0.0,
        head_yaw: 0.0,
        present: 0,
    }
}

#[test]
fn ffi_batch_validates_pointers_and_lengths() {
    let mut table: *mut EntityInterpolationTable = ptr::null_mut();
    let updates = [update(1, INTERP_UPDATE_SNAP, [0.0; 3])];
    let mut out = [empty_pose(); 2];
    unsafe {
        assert_eq!(megu_interp_table_new(ptr::null_mut()), Error::Null as i32);
        assert_eq!(megu_interp_table_new(&mut table), 0);
        assert!(!table.is_null());

        // Null table, undersized output, null inputs, bad partial tick, bad kind.
        assert_eq!(
            megu_interp_entities_batch(ptr::null_mut(), updates.as_ptr(), 1, 0.5, out.as_mut_ptr(), 2),
            Error::Null as i32
        );
        assert_eq!(
            megu_interp_entities_batch(table, updates.as_ptr(), 1, 0.5, out.as_mut_ptr(), 0),
            Error::BufferTooSmall as i32
        );
        assert_eq!(
            megu_interp_entities_batch(table, ptr::null(), 1, 0.5, out.as_mut_ptr(), 2),
            Error::Null as i32
        );
        assert_eq!(
            megu_interp_entities_batch(table, updates.as_ptr(), 1, 0.5, ptr::null_mut(), 2),
            Error::Null as i32
        );
        assert_eq!(
            megu_interp_entities_batch(table, updates.as_ptr(), 1, f32::NAN, out.as_mut_ptr(), 2),
            Error::InvalidArgument as i32
        );
        assert_eq!(
            megu_interp_entities_batch(table, updates.as_ptr(), 1, 0.5, out.as_mut_ptr(), 2),
            0
        );
        let bad = [update(2, 99, [0.0; 3])];
        assert_eq!(
            megu_interp_entities_batch(table, bad.as_ptr(), 1, 0.5, out.as_mut_ptr(), 2),
            Error::InvalidArgument as i32
        );
        // Rotation for an untracked entity fails closed.
        let rot = [update(77, INTERP_UPDATE_ROTATION, [0.0; 3])];
        assert_eq!(
            megu_interp_entities_batch(table, rot.as_ptr(), 1, 0.5, out.as_mut_ptr(), 2),
            Error::NotFound as i32
        );

        assert_eq!(megu_interp_table_free(table), 0);
        assert_eq!(megu_interp_table_free(ptr::null_mut()), Error::Null as i32);
    }
}

#[test]
fn ffi_batch_matches_the_rust_api_bit_for_bit() {
    let mut ffi_table: *mut EntityInterpolationTable = ptr::null_mut();
    let mut rust_table = EntityInterpolationTable::new();
    unsafe {
        assert_eq!(megu_interp_table_new(&mut ffi_table), 0);
    }

    // The batch applies each update and samples that entity immediately, so
    // mirror the same interleaving on the checked Rust API.
    let updates = [
        update(5, INTERP_UPDATE_SNAP, [1.0, 64.0, -2.0]),
        update(5, INTERP_UPDATE_POSITION, [2.5, 64.0, -2.0]),
        update(5, INTERP_UPDATE_ROTATION, [0.0; 3]),
        update(6, INTERP_UPDATE_POSITION, [10.0, 70.0, 10.0]),
        update(7, INTERP_UPDATE_SNAP, [-3.0, 60.0, 4.0]),
        update(7, INTERP_UPDATE_REMOVE, [0.0; 3]),
    ];
    let mut out = [empty_pose(); 6];
    unsafe {
        assert_eq!(
            megu_interp_entities_batch(
                ffi_table,
                updates.as_ptr(),
                updates.len(),
                0.25,
                out.as_mut_ptr(),
                out.len(),
            ),
            0
        );
    }
    for (i, u) in updates.iter().enumerate() {
        let rust_pose = match u.kind {
            INTERP_UPDATE_SNAP => rust_table
                .snap_entity(u.entity_id, u.position, u.yaw, u.pitch, u.head_yaw)
                .map(|()| rust_table.sample(u.entity_id, 0.25)),
            INTERP_UPDATE_POSITION => rust_table
                .update_position(u.entity_id, u.position)
                .map(|()| rust_table.sample(u.entity_id, 0.25)),
            INTERP_UPDATE_ROTATION => rust_table
                .update_rotation(u.entity_id, u.yaw, u.pitch, u.head_yaw)
                .map(|()| rust_table.sample(u.entity_id, 0.25)),
            INTERP_UPDATE_REMOVE => {
                rust_table.remove_entity(u.entity_id);
                Ok(None)
            }
            _ => unreachable!(),
        }
        .unwrap();
        let ffi = &out[i];
        match rust_pose {
            Some(rust) => {
                assert_eq!(ffi.present, 1, "entity {} missing", u.entity_id);
                assert_eq!(
                    (
                        [
                            ffi.position[0].to_bits(),
                            ffi.position[1].to_bits(),
                            ffi.position[2].to_bits()
                        ],
                        ffi.yaw.to_bits(),
                        ffi.pitch.to_bits(),
                        ffi.head_yaw.to_bits(),
                    ),
                    pose_bits(&rust),
                    "entity {} mismatch",
                    u.entity_id
                );
            }
            None => assert_eq!(ffi.present, 0, "entity {} should be absent", u.entity_id),
        }
    }
    unsafe {
        assert_eq!(megu_interp_table_free(ffi_table), 0);
    }
}

#[test]
fn ffi_sky_fog_eval_matches_rust_bit_for_bit() {
    let input = SkyFogInputC {
        day_time: -1,
        rain_level: 0.3,
        thunder_level: 0.7,
        view_distance: 96.0,
        biome_fog_color: [0.4, 0.5, 0.6],
        biome_fog_color_next: [0.55, 0.45, 0.35],
        biome_blend: 0.5,
        underwater: 1,
        in_lava: 0,
        powder_snow: 0,
        blindness: 0.25,
        darkness: 0.0,
    };
    let rust = evaluate_sky_and_fog(&SkyFogInput {
        day_time: input.day_time,
        rain_level: input.rain_level,
        thunder_level: input.thunder_level,
        view_distance: input.view_distance,
        biome_fog_color: input.biome_fog_color,
        biome_fog_color_next: input.biome_fog_color_next,
        biome_blend: input.biome_blend,
        underwater: true,
        in_lava: false,
        powder_snow: false,
        blindness: input.blindness,
        darkness: input.darkness,
    });

    let mut out = SkyFogOutputC {
        celestial_angle: 0.0,
        star_brightness: 0.0,
        sky_rgb: [0.0; 3],
        fog_rgb: [0.0; 3],
        fog_start: 0.0,
        fog_end: 0.0,
    };
    unsafe {
        assert_eq!(megu_sky_fog_eval(ptr::null(), &mut out), Error::Null as i32);
        assert_eq!(megu_sky_fog_eval(&input, ptr::null_mut()), Error::Null as i32);
        let mut bad = input;
        bad.underwater = 2;
        assert_eq!(
            megu_sky_fog_eval(&bad, &mut out),
            Error::InvalidArgument as i32
        );
        assert_eq!(megu_sky_fog_eval(&input, &mut out), 0);
    }
    assert_eq!(out.celestial_angle.to_bits(), rust.celestial_angle.to_bits());
    assert_eq!(out.star_brightness.to_bits(), rust.star_brightness.to_bits());
    for i in 0..3 {
        assert_eq!(out.sky_rgb[i].to_bits(), rust.sky_rgb[i].to_bits());
        assert_eq!(out.fog_rgb[i].to_bits(), rust.fog_rgb[i].to_bits());
    }
    assert_eq!(out.fog_start.to_bits(), rust.fog_start.to_bits());
    assert_eq!(out.fog_end.to_bits(), rust.fog_end.to_bits());
}
