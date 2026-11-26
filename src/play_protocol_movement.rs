fn play_c2s_teleport_confirm(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let mut payload = b;
    if let Some(id) = read_protocol_i32(&mut payload).filter(|_| payload.is_empty()) {
        me.teleport
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .confirm(id);
        // v2: the confirm echoes the matching transaction —
        // pending velocity/teleport grants activate and the
        // confirmed RTT folds into lag compensation.
        ac_note_tx_confirm(&me, i64::from(id));
        *cx.retry_chunks = pump_client_chunks(&me, false);
    }
    PlayPacketControl::Handled
}

fn play_c2s_player_movement(
    cx: &mut PlayPacketContext<'_>,
    pid: i64,
    b: &[u8],
) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let view = *cx.view;
    // position / position_look: f64 x,y,z first
    if b.len() >= 24 {
        if !cx.move_guard.allow_tick(TICK.load(Ordering::Relaxed)) {
            return PlayPacketControl::Skip;
        }
        let px = f64::from_be_bytes(b[..8].try_into().unwrap());
        let py = f64::from_be_bytes(b[8..16].try_into().unwrap());
        let pz = f64::from_be_bytes(b[16..24].try_into().unwrap());
        if !me
            .teleport
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .movement_allowed()
        {
            return PlayPacketControl::Skip;
        }
        // Anti-cheat runs before any lock is taken: it reads attributes, effects
        // and blocks around the player, and may pull the player back.
        let before = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
        let claimed = if pid == 0x1b && b.len() >= 33 {
            b[32] != 0
        } else if pid == 0x1a && b.len() >= 25 {
            b[24] != 0
        } else {
            before.5
        };
        // `disableElytraMovementCheck`: a gliding player's position
        // packets skip the anti-cheat envelope when the rule is on —
        // vanilla skips the same moved-too-quickly check for
        // `isFallFlying` under the gamerule.
        let checked_gnd = if *me.fall_fly.lock().unwrap_or_else(|e| e.into_inner())
            && gamerule_live_value("disableElytraMovementCheck") != 0
        {
            claimed
        } else {
            match ac_check_move(&me, (before.0, before.1, before.2), (px, py, pz), claimed) {
                AcMoveVerdict::Accept { ground } => ground,
                AcMoveVerdict::Setback => {
                    ac_setback(&me, before);
                    return PlayPacketControl::Skip;
                }
            }
        };
        let teleport = me.teleport.lock().unwrap_or_else(|e| e.into_inner());
        if !teleport.movement_allowed() {
            return PlayPacketControl::Skip;
        }
        let mut p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
        let (mut yaw, mut pitch, gnd) = (p.3, p.4, checked_gnd);
        if pid == 0x1b && b.len() >= 33 {
            yaw = f32::from_be_bytes(b[24..28].try_into().unwrap());
            pitch = f32::from_be_bytes(b[28..32].try_into().unwrap());
        }
        let prev_gnd = p.5;
        *p = (px, py, pz, yaw, pitch, gnd);
        drop(p);
        // Vanilla wakes on an actual positional displacement. Idle
        // position/look keepalives that reassert the same coordinate
        // must not cancel sleep or hide the Leave Bed UI.
        let moved =
            (px - before.0).powi(2) + (py - before.1).powi(2) + (pz - before.2).powi(2) > 1.0e-6;
        if moved {
            wake_if_sleeping(&me);
            dispatch_player_shrieker_vibration(&me, (px, py, pz));
        }
        drop(teleport);
        landing::player_move(&me, prev_gnd);
        // Portal waits advance on the game tick (vanilla
        // `handleInsidePortal`), not on movement packets — a player
        // standing still in a portal must still transit, and a
        // single caller keeps the arrival guard race-free.
        // A vertical move inside the same chunk window still crosses the
        // occlusion hysteresis (descending into a cave); the lane flip
        // re-sends held chunks in the new lane. The window pump below
        // re-runs the same check as a no-op when nothing changed.
        if before.1 != py {
            update_underground_lane(&me);
        }
        let window = ChunkWindow::at(px, pz, view);
        let current = with_play_wire(&me.sock.lock().unwrap_or_else(|e| e.into_inner()), |play| {
            play.chunk_delivery.window()
        })
        .flatten();
        if current != Some(window) {
            if let Ok(pending) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pump_client_chunks(&me, true)
            })) {
                *cx.retry_chunks = pending;
            }
        }
        if ingress_guard::movement_pose_changed(before, (px, py, pz, yaw, pitch, gnd)) {
            broadcast_seen(me.eid, |_c| {
                (0x70, entity_teleport(me.eid, px, py, pz, yaw, pitch, gnd))
            });
            if pid == 0x1b {
                broadcast_seen(me.eid, |_c| (0x48, head_rot(me.eid, yaw)));
            }
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_player_rotation(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // look: yaw,pitch only
    if b.len() >= 9 {
        let yaw = f32::from_be_bytes(b[..4].try_into().unwrap());
        let pitch = f32::from_be_bytes(b[4..8].try_into().unwrap());
        // The same rule `valid_movement_frame` applies to the
        // position packets: a non-finite angle must not reach `pos`,
        // the rotation broadcasts, or the projectile math that turns
        // yaw/pitch into velocities.
        if !yaw.is_finite() || !pitch.is_finite() {
            return PlayPacketControl::Skip;
        }
        let teleport = me.teleport.lock().unwrap_or_else(|e| e.into_inner());
        if !teleport.movement_allowed() {
            return PlayPacketControl::Skip;
        }
        let at = {
            let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
            (p.0, p.1, p.2)
        };
        let gnd = ac_effective_ground(&me, at, b[8] != 0);
        let mut p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
        let prev_gnd = p.5;
        p.3 = yaw;
        p.4 = pitch;
        p.5 = gnd;
        let (px, py, pz, g) = (p.0, p.1, p.2, p.5);
        drop(p);
        drop(teleport);
        landing::player_move(&me, prev_gnd);
        broadcast_seen(me.eid, |_c| {
            (0x70, entity_teleport(me.eid, px, py, pz, yaw, pitch, g))
        });
        broadcast_seen(me.eid, |_c| (0x48, head_rot(me.eid, yaw)));
    }
    PlayPacketControl::Handled
}

fn play_c2s_entity_action(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // entity_action: sneak/sprint start/stop -> entity flags+pose
    let mut sl = b;
    let _eid = read_varint(&mut sl);
    if let Some(act) = read_varint(&mut sl) {
        match act {
            0 => *me.sneaking.lock().unwrap_or_else(|e| e.into_inner()) = true,
            1 => *me.sneaking.lock().unwrap_or_else(|e| e.into_inner()) = false,
            2 => {
                wake_if_sleeping(&me);
            }
            3 | 4 => {
                *me.sprinting.lock().unwrap_or_else(|e| e.into_inner()) = act == 3;
                // v2: sprint-while-using-item is impossible for
                // a vanilla client — the FSM keeps the proof.
                ac_v2_note_sprint(&me, act == 3);
            }
            7 => {
                // OPEN_INVENTORY: riding an equine opens its
                // inventory screen (`openHorseInventory`).
                open_horse_screen(&me);
            }
            // START_FALL_FLYING: only an intact Elytra in the
            // chest slot while airborne may enter pose 1.
            8 if can_start_fall_flying(&me) => {
                *me.fall_fly.lock().unwrap_or_else(|e| e.into_inner()) = true;
            }
            _ => {}
        }
        if (0..=1).contains(&act) || (3..=4).contains(&act) || act == 8 {
            let sneak = *me.sneaking.lock().unwrap_or_else(|e| e.into_inner());
            let sprint = *me.sprinting.lock().unwrap_or_else(|e| e.into_inner());
            let glide = *me.fall_fly.lock().unwrap_or_else(|e| e.into_inner());
            // flags: sneak=0x02 sprint=0x08 fall_flying=0x80 ; pose: sneak=5 fall_fly=1 else standing=0
            let mut p = Vec::new();
            write_varint(&mut p, me.eid as i64);
            p.push(0);
            p.push(0);
            p.push(((sneak as u8) * 2) | ((sprint as u8) * 8) | ((glide as u8) * 0x80));
            p.push(6);
            write_varint(&mut p, 21);
            write_varint(
                &mut p,
                if glide {
                    1
                } else if sneak {
                    5
                } else {
                    0
                },
            );
            p.push(0xff);
            broadcast_seen_packet(me.eid, 0x58, &p);
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_player_flying(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // flying: onGround only
    if !b.is_empty() {
        let teleport = me.teleport.lock().unwrap_or_else(|e| e.into_inner());
        if !teleport.movement_allowed() {
            return PlayPacketControl::Skip;
        }
        let at = {
            let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
            (p.0, p.1, p.2)
        };
        let gnd = ac_effective_ground(&me, at, b[0] != 0);
        let mut p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
        let prev_gnd = p.5;
        p.5 = gnd;
        drop(p);
        drop(teleport);
        landing::player_move(&me, prev_gnd);
    }
    PlayPacketControl::Handled
}

fn play_c2s_move_vehicle(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // ServerboundMoveVehiclePacket: the riding client drives the
    // vehicle. `step_live_vehicle` runs the vanilla speed gate and
    // credits the carry to anti-cheat, so riding never trips the
    // mover checks.
    if let Some(pkt) = decode_move_vehicle(b) {
        step_live_vehicle(&me, &pkt);
    }
    PlayPacketControl::Handled
}

fn play_c2s_paddle_boat(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // ServerboundPaddleBoatPacket: remembered per vehicle for the
    // boat-paddle animation state; it moves no world state itself.
    if let Some(paddle) = decode_paddle_boat(b) {
        if let Some(vehicle) = vehicle_of(me.eid) {
            mounts()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .paddling
                .insert(vehicle, paddle);
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_player_input(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // ServerboundPlayerInputPacket: the shift bit dismounts the
    // rider (`Entity.stopRiding`), updating the vehicle's
    // SetPassengers for every tracker. The rest of the input is
    // the driver's steering for the live vehicle physics tick.
    if let Some(input) = decode_player_input(b) {
        if let Some(vehicle) = vehicle_of(me.eid) {
            if input.unmount {
                mounts()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .steering
                    .remove(&vehicle);
            } else {
                mounts()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .steering
                    .insert(vehicle, input);
            }
        }
        if input.unmount {
            player_dismount(&me, true);
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_player_abilities(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // player_abilities: honour the flying bit only under `mayfly` —
    // a survival fly-hack request is refused, never stored.
    if let Some(flags) = decode_player_abilities(b) {
        c2s_apply_player_abilities(&me, flags);
    }
    PlayPacketControl::Handled
}

fn play_c2s_teleport_to_entity(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // teleport_to_entity: spectator-only — vanilla refuses anyone
    // else outright before resolving the target.
    if let Some(uuid) = decode_teleport_to_entity(b) {
        if me.is_spectator() {
            c2s_spectate_teleport(&me, &uuid);
        }
    }
    PlayPacketControl::Handled
}
