fn play_c2s_change_difficulty(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // Serverbound change difficulty: difficulty byte + locked flag.
    // A `lock_difficulty`-locked world refuses further changes —
    // vanilla `LevelData.setDifficulty` does the same refusal.
    if DIFFICULTY_LOCKED.load(Ordering::Relaxed) == 0
        && b.len() == 2
        && b[0] <= 3
        && b[1] <= 1
        && client_is_admin(&me)
    {
        set_difficulty(b[0]);
    }
    PlayPacketControl::Handled
}

fn play_c2s_client_information(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let Some(requested) = client_info_view_distance(b) else {
        return PlayPacketControl::Break;
    };
    let effective = requested.clamp(2, live_view_distance());
    if effective != *cx.view {
        *cx.view = effective;
        *me.view.lock().unwrap_or_else(|e| e.into_inner()) = *cx.view;
        send_view_distance(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            *cx.view,
        );
        connection_limits::set_chunk_feedback_window(
            &mut me.chunk_ack.lock().unwrap_or_else(|e| e.into_inner()),
            window_batch(*cx.view),
        );
        *cx.retry_chunks = pump_client_chunks(&me, true);
    }
    PlayPacketControl::Handled
}

fn play_c2s_command_suggestion(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    reply_to_command_suggestion(&me, b);
    PlayPacketControl::Handled
}

fn play_c2s_chat_command(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // chat_command(+signed): command string (no leading /)
    let mut sl = b;
    if let Some(mlen) = read_varint(&mut sl) {
        let m = String::from_utf8_lossy(&sl[..(mlen as usize).min(sl.len())]).to_string();
        if !connection_limits::chat_text_valid(&m) {
            eprintln!("[spine] {} disconnected: illegal chat command", me.name);
            play_disconnect(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                "Illegal characters in chat",
            );
            return PlayPacketControl::Break;
        }
        if !cx.chat_throttle.allow(Instant::now()) {
            eprintln!("[spine] {} disconnected: chat spam", me.name);
            play_disconnect(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                "Kicked for spamming!",
            );
            return PlayPacketControl::Break;
        }

        run_command(Some(&me), &format!("/{}", m), &mut Some(&me));
    }
    PlayPacketControl::Handled
}

fn play_c2s_chat_message(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // chat_message: the protocol-767 signed envelope runs the live
    // admission + relay in `live_chat_message` (P7a / #132).
    if let Err(text) = live_chat_message(&me, b, cx.chat_throttle) {
        eprintln!("[spine] {} disconnected: {}", me.name, text);
        play_disconnect(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), text);
        return PlayPacketControl::Break;
    }
    PlayPacketControl::Handled
}

fn play_c2s_client_command(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let view = *cx.view;
    // client_command: 0 = perform respawn, 1 = request stats
    // (`ServerboundClientCommandPacket` actions).
    let mut sl = b;
    let action = read_varint(&mut sl);
    if action == Some(1) && sl.is_empty() {
        let body = {
            let mut stats = player_stats().lock().unwrap_or_else(|e| e.into_inner());
            let stats = stats.entry(me.uuid).or_default();
            // P8b-2: the protocol-767 `award_stats` payload comes
            // from the P8b-2 codec over the live counter book —
            // same `(category, stat, value)` triples the save codec
            // encodes, in `BTreeMap` order.
            let entries: Vec<(u8, i32, i32)> = stats
                .counts
                .iter()
                .map(|(&(category, stat_id), &value)| (category.id() as u8, stat_id, value))
                .collect();
            encode_award_stats_0x04(&entries)
        };
        let _ = send_packet(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x04,
            body,
        );
    } else if action == Some(0) && sl.is_empty() {
        drop_player_items_on_death(&me);
        let restored_health = max_hp_of(&me.name);
        {
            let mut health = me.hp.lock().unwrap_or_else(|e| e.into_inner());
            if *health > 0.0 {
                return PlayPacketControl::Skip;
            }
            *health = restored_health;
        }
        // `ServerPlayer.stopRiding` on death: the respawn starts
        // unmounted wherever the death happened.
        player_dismount(&me, false);
        death_drop_marks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&me.eid);
        close_live_menu(&me, true, true);
        let bed_respawn = saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&me.uuid)
            .and_then(|save| save.respawn)
            .filter(|target| target.kind == SpawnTargetKind::Bed);
        if let Some(target) = bed_respawn {
            let head = (
                target.position.0.floor() as i32,
                target.position.1.floor() as i32 - 1,
                target.position.2.floor() as i32,
            );
            let bed_exists = block_registry::breg().is_some_and(|registry| {
                let state = block_at_dim(target.dim, head.0, head.1, head.2);
                let name = registry.name_of(state);
                (registry.in_tag(state, "minecraft:beds") || name.ends_with("_bed"))
                    && registry.get_prop(state, "part").as_deref() == Some("head")
            });
            if !bed_exists || bed_stand_position(target.dim, head).is_none() {
                *me.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *me.spawn_dim.lock().unwrap_or_else(|e| e.into_inner()) = DimId::Overworld;
                if let Some(save) = saved_players()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&me.uuid)
                {
                    save.respawn = None;
                }
                let _ = send_packet(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    0x4c,
                    translate_component_nbt("block.minecraft.spawn.not_valid"),
                );
            } else if let Some((sx, sy, sz)) = bed_stand_position(target.dim, head) {
                *me.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((sx, sy, sz, target.angle));
            }
        }
        let anchor_respawn = saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&me.uuid)
            .and_then(|save| save.respawn)
            .filter(|target| target.kind == SpawnTargetKind::Anchor);
        if let Some(target) = anchor_respawn {
            let anchor = (
                target.position.0.floor() as i32,
                target.position.1.floor() as i32 - 1,
                target.position.2.floor() as i32,
            );
            let state = block_at_dim(target.dim, anchor.0, anchor.1, anchor.2);
            let charges = block_registry::breg().and_then(|registry| {
                (registry.name_of(state) == "minecraft:respawn_anchor")
                    .then(|| registry.get_prop(state, "charges")?.parse::<u8>().ok())
                    .flatten()
            });
            let anchor_works = target.dim == DimId::Nether
                || crate::worldgen::dimension_spec(target.dim)
                    .is_some_and(|spec| spec.respawn_anchor_works);
            let valid = anchor_works && charges.is_some_and(|charges| charges > 0);
            if !valid || respawn_anchor_stand_position(target.dim, anchor).is_none() {
                *me.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *me.spawn_dim.lock().unwrap_or_else(|e| e.into_inner()) = DimId::Overworld;
                if let Some(save) = saved_players()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&me.uuid)
                {
                    save.respawn = None;
                }
                let _ = send_packet(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    0x4c,
                    translate_component_nbt("block.minecraft.spawn.not_valid"),
                );
            } else if let (Some(charges), Some((sx, sy, sz))) =
                (charges, respawn_anchor_stand_position(target.dim, anchor))
            {
                // The typed target still names the anchor block;
                // only the live respawn coordinate is replaced by
                // the resolved stand position. A successful Nether
                // anchor respawn spends exactly one charge.
                if let Some(reg) = block_registry::breg() {
                    let new_state = reg.with_prop(
                        state,
                        "charges",
                        &respawn_anchor_consume(charges).to_string(),
                    );
                    if set_block_if_dim(
                        target.dim,
                        anchor.0,
                        anchor.1,
                        anchor.2,
                        new_state,
                        Some(state),
                    ) {
                        note_block_update_dim(target.dim, anchor.0, anchor.1, anchor.2, new_state);
                    }
                }
                *me.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((sx, sy, sz, target.angle));
            }
        }
        // P2-3-S4: respawn in the dimension the spawn point belongs to. A
        // death in the Nether returns to the Overworld bed/world spawn when that
        // is where the spawn point was set, with a full window rebuild.
        let (sdim, (rx, ry, rz, yaw)) = {
            let sdim = me.spawn_dim.lock().unwrap_or_else(|e| e.into_inner());
            let point = *me.spawn_pt.lock().unwrap_or_else(|e| e.into_inner());
            match point {
                Some(point) => (*sdim, point),
                None => {
                    // `spawnRadius`: a bedless respawn scatters
                    // inside the radius square around world spawn.
                    let w = *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner());
                    let [bx, by, bz] = resolve_world_spawn_pos(
                        [w.0.floor() as i32, w.1.floor() as i32, w.2.floor() as i32],
                        spawn_radius_blocks(),
                        world_rng::next_u64(),
                    );
                    (
                        DimId::Overworld,
                        (bx as f64 + 0.5, by as f64, bz as f64 + 0.5, w.3),
                    )
                }
            }
        };
        switch_client_dimension(&me, sdim, (rx, ry, rz, yaw, 0.0), view);
        reset_player_food(&me);
        let _ = send_player_xp(&me, |_| Ok(()));
        {
            let iv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&iv);
        }
        let _ = send_packet(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x5d,
            health_pkt_c(max_hp_of(&me.name), &me),
        );
        send_attrs(&me);
        // vanilla resyncs window 0 after respawn — without it
        // the client keeps showing the pre-death inventory.
        // Respawn builds a fresh InventoryMenu server- and
        // client-side, so the shared counter restarts too.
        me.inv_state_id.store(0, Ordering::Relaxed);
        let mut q = vec![0u8]; // window 0 = player inventory
        write_varint(&mut q, i64::from(inv_state_next(&me)));
        write_varint(&mut q, 46);
        {
            let iv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            for i in 0..46i16 {
                let (it, c, _cc) = iv.get(&i).cloned().unwrap_or((0, 0, Vec::new()));
                q.extend_from_slice(&slot_wire(&(it, c, _cc)));
            }
        }
        q.extend_from_slice(&slot_wire(&(0, 0, Vec::new()))); // carried
        let _ = send_packet(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x13,
            q,
        );
        let inventory = me.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
        save_live_player_snapshot(&me, client_dim_pos(&me), &inventory);
    }
    PlayPacketControl::Handled
}

fn play_c2s_chat_ack(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // chat_ack: fold the acknowledged offset into the connection's
    // last-seen counter — saturating so a hostile offset cannot
    // wrap it backwards.
    if let Some(offset) = decode_chat_ack(b) {
        let mut offsets = chat_ack_offsets().lock().unwrap_or_else(|e| e.into_inner());
        let entry = offsets.entry(me.eid).or_insert(0);
        *entry = entry.saturating_add(offset);
    }
    PlayPacketControl::Handled
}

fn play_c2s_chat_session_update(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // chat_session_update: record the signed-chat session metadata
    // for the P7a lane; strict bounds refuse oversized keys.
    if let Some(meta) = decode_chat_session_update(b) {
        chat_sessions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(me.eid, meta);
    }
    PlayPacketControl::Handled
}

fn play_c2s_debug_sample_subscription(
    cx: &mut PlayPacketContext<'_>,
    b: &[u8],
) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // debug_sample_subscription: OP>=2 subscribes to the per-tick
    // `debug_sample` push; everyone else is refused fail-closed.
    if let Some(sample_type) = decode_debug_sample_subscription(b) {
        if client_permission_level(&me) >= 2 {
            debug_sample_subscribers()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(me.eid, sample_type);
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_lock_difficulty(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // lock_difficulty: OP>=2 only — locks the world difficulty and
    // broadcasts `ChangeDifficulty` with the locked flag.
    if let Some(locked) = decode_lock_difficulty(b) {
        c2s_apply_lock_difficulty(&me, locked);
    }
    PlayPacketControl::Handled
}

fn play_c2s_seen_advancements(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // seen_advancements: OpenedTab carries the tab id,
    // ClosedScreen carries nothing.
    if let Some(state) = decode_seen_advancements(b) {
        seen_advancement_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(me.eid, state.clone());
        // vanilla `PlayerAdvancements.setSelectedTab` answers the
        // request: OpenedTab echoes the tab, ClosedScreen sends
        // an empty selection.
        let _ = send_packet(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x4a,
            encode_play_select_advancements_tab(state.1.as_deref()).1,
        );
    }
    PlayPacketControl::Handled
}
