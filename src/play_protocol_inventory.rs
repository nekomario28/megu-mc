fn play_c2s_held_item_slot(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let mut held_slot = *cx.held_slot;
    // held_item_slot: i16 — `EntityEquipment.setChanged` for the
    // new main hand is emitted by the loop-head equipment diff.
    if let Some(raw) = b.get(..2).and_then(|s| <[u8; 2]>::try_from(s).ok()) {
        held_slot = i16::from_be_bytes(raw).clamp(0, 8);
        *me.held.lock().unwrap_or_else(|e| e.into_inner()) = held_slot;
        // `MapItem.inventoryTick`: a filled_map in the new main
        // hand pushes its `map_item_data` once per map.
        send_held_map_data(&me, held_slot);
    }
    *cx.held_slot = held_slot;
    PlayPacketControl::Handled
}

fn play_c2s_pick_item(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let mut held_slot = *cx.held_slot;
    // ServerboundPickItemPacket: one VarInt `slot` — an
    // `Inventory.items` index the client already resolved
    // (0..=8 hotbar, 9..=35 main inventory). The server runs
    // `Inventory.pickSlot`; out-of-range values drop fail-closed.
    let mut sl = b;
    if let Some(slot) = read_varint(&mut sl) {
        pick_item(&me, &mut held_slot, slot);
    }
    *cx.held_slot = held_slot;
    PlayPacketControl::Handled
}

fn play_c2s_recipe_book_change_settings(
    cx: &mut PlayPacketContext<'_>,
    b: &[u8],
) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // ServerboundRecipeBookChangeSettingsPacket
    let mut sl = b;
    if let Some(s) = decode_recipe_book_change_settings(&mut sl) {
        recipe_books()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(me.uuid)
            .or_default()
            .apply_book_settings(s.book_type, s.book_open, s.filter_active);
    }
    PlayPacketControl::Handled
}

fn play_c2s_recipe_book_seen_recipe(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // ServerboundRecipeBookSeenRecipePacket
    let mut sl = b;
    if let Some(id) = decode_recipe_book_seen_recipe(&mut sl) {
        recipe_books()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(me.uuid)
            .or_default()
            .seen_recipe(&id);
    }
    PlayPacketControl::Handled
}

fn play_c2s_place_recipe(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // ServerboundPlaceRecipePacket
    let mut sl = b;
    if let Some(place) = decode_place_recipe(&mut sl) {
        place_live_recipe(&me, &place);
    }
    PlayPacketControl::Handled
}

fn play_c2s_set_creative_slot(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // set_creative_slot: slot i16 + slot data (creative only)
    if b.len() >= 3 && *me.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1 {
        let Some((slot, mut st)) = slot_read_wire(b) else {
            return PlayPacketControl::Skip;
        };
        // Player inventory menu slots are crafting output/input, armor,
        // inventory, hotbar and offhand (0..=45).
        if !creative_inventory_slot(slot) {
            return PlayPacketControl::Skip;
        }
        // fail-closed bomb guard: a stack whose added components
        // exceed the wire bound is dropped before it can be
        // stored, echoed, or re-broadcast through drops/pickups
        if !creative_slot_acceptable(&st) {
            return PlayPacketControl::Skip;
        }
        creative_slot_clamp(&mut st);
        // MEGA-W2 §3.1: `ac_sanitize_creative_item` clamps the
        // count for everyone and strips dangerous NBT-bearing
        // components (command/spawner/spawn data, attribute
        // modifiers) from non-operator submissions.
        ac_sanitize_creative_item(&mut st, client_permission_level(&me));
        if st.1 > 0 {
            me.inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(slot, st.clone());
        } else {
            me.inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&slot);
        }
        {
            let iv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&iv);
        }
        // echo container set_slot (0x15): window 0 echoes carry the
        // shared inventory counter (`incrementStateId`), not 0.
        let mut p = vec![0u8];
        write_varint(&mut p, i64::from(inv_state_next(&me)));
        p.extend_from_slice(&slot.to_be_bytes());
        p.extend_from_slice(&slot_wire(&st));
        let _ = send_packet(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x15,
            p,
        );
    }
    PlayPacketControl::Handled
}

fn play_c2s_container_button_click(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let mut payload = b;
    if let (Some(window), Some(button)) = (
        read_protocol_i32(&mut payload),
        read_protocol_i32(&mut payload),
    ) {
        if payload.is_empty() {
            with_save_guard(|| live_enchant_button(&me, window, button));
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_anvil_rename(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let mut payload = b;
    if let Some(length) = read_protocol_i32(&mut payload) {
        if (0..=150).contains(&length) && payload.len() == length as usize {
            if let Ok(name) = std::str::from_utf8(payload) {
                with_save_guard(|| live_anvil_rename(&me, name));
            }
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_select_trade(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    if let Some(index) = decode_select_trade(b) {
        with_save_guard(|| live_select_trade(&me, index));
    }
    PlayPacketControl::Handled
}

fn play_c2s_set_beacon_effect(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // set_beacon_effect: Optional<varint> primary, Optional<varint>
    // secondary — mob-effect registry ids, not BeaconEffect ordinals.
    let mut sl = b;
    let opt = |sl: &mut &[u8]| -> Option<Option<i64>> {
        match *sl.first()? {
            0 => {
                *sl = &sl[1..];
                Some(None)
            }
            1 => {
                *sl = &sl[1..];
                read_varint(sl).map(Some)
            }
            _ => None,
        }
    };
    let Some(primary) = opt(&mut sl) else {
        return PlayPacketControl::Skip;
    };
    let Some(secondary) = opt(&mut sl) else {
        return PlayPacketControl::Skip;
    };
    let primary = primary.and_then(beacon_effect_from_wire);
    let secondary = secondary.and_then(beacon_effect_from_wire);
    let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((bdim, bx, by, bz)) = live_beacon_anchor(&me, dim) {
        let applied = with_save_guard(|| {
            let Some(reg) = crate::block_registry::breg() else {
                return false;
            };
            let state = block_at_dim(bdim, bx, by, bz);
            let mut store = live_block_entities()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(LiveBlockEntity::Beacon(beacon)) = store.at(reg, (bdim, bx, by, bz), state)
            else {
                return false;
            };
            let held_slot =
                held_player_menu_slot(*me.held.lock().unwrap_or_else(|e| e.into_inner()));
            let stack = me
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&held_slot)
                .cloned()
                .unwrap_or_default();
            let mut payment = BlockEntityItem::from_stack(&stack);
            if !beacon.confirm(primary, secondary, &mut payment) {
                return false;
            }
            map_take(
                &mut me.inv.lock().unwrap_or_else(|e| e.into_inner()),
                held_slot,
                1,
            );
            true
        });
        if applied {
            // The beacon's changed state is the confirmation: the
            // updated block-entity data goes out to every holder of
            // its chunk, matching sendBlockUpdated.
            if let Some((_, tag)) = live_block_entity_snapshot((bdim, bx, by, bz)) {
                if let Ok(nbt) = crate::nbt::write_network_nbt(&tag) {
                    let mut pkt = Vec::new();
                    write_pos(&mut pkt, bx, by, bz);
                    write_varint(&mut pkt, 14); // block entity type: beacon
                    pkt.extend_from_slice(&nbt);
                    broadcast_chunk_dim_packet(bdim, bx >> 4, bz >> 4, 0x07, &pkt);
                    held_slot_resync(&me);
                }
            }
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_close_window(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    if let Some(id) = live_menu_id(&me) {
        if b == [id] {
            close_live_menu_id(&me, Some(id), false, true);
        }
        return PlayPacketControl::Skip;
    }
    if b.first().is_some_and(|&id| id >= 4) {
        return PlayPacketControl::Skip;
    }
    // close_window
    // `AbstractContainerMenu.removed` -> `Container.stopOpen`: the opener
    // counters drop and each half that moved broadcasts its BlockEvent.
    let chest_window = client_open_window_and_anchor(&me);
    if let Some((open, anchor)) = chest_window {
        let audience = client_snapshot();
        let cdim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
        let halves = client_window_halves_dim(open, anchor, cdim);
        let before = chest_opener_snapshot_dim(&audience, &halves);
        *me.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *me.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
        sync_chest_openers_dim(&audience, &before);
    }
    *me.open_chest.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *me.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *me.open_furnace.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *me.open_table.lock().unwrap_or_else(|e| e.into_inner()) = false;
    beacon_menu_anchors()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&me.eid);
    {
        // vanilla containerMenu.removed: the carried stack is
        // thrown into the world, crafting grid goes back to inv
        let cur = std::mem::take(&mut *me.cursor.lock().unwrap_or_else(|e| e.into_inner()));
        if cur.1 > 0 {
            let (px, py, pz, ..) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
            drops()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Drop {
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    x: px,
                    y: py + 1.3,
                    z: pz,
                    item: cur.0,
                    cnt: cur.1,
                    comps: cur.2,
                    age: -(server_ticks(40) as i32), // pickup delay: 2s already elapsed,
                    target: None,
                    thrower: None,
                });
        }
        let mut leftovers = Vec::new();
        {
            let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            for g in [&me.craft, &me.craft9] {
                leftovers.extend(return_crafting_grid_to_inventory(
                    &mut inv,
                    &mut g.lock().unwrap_or_else(|e| e.into_inner()),
                ));
            }
        }
        return_live_inputs(&me, leftovers);
        *me.drag.lock().unwrap_or_else(|e| e.into_inner()) = (-1, Vec::new());
    }
    PlayPacketControl::Handled
}

fn play_c2s_window_click_mode0(
    me: &Arc<Client>,
    slot: i16,
    btn: u8,
    click_tail: &[u8],
) -> PlayPacketControl {
    let chest_pos = *me.open_chest.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cp, cp2)) = chest_pos {
        if !window_live_for(me, (cp, cp2)) {
            invalidate_client_chest_window(&client_snapshot(), me, (cp, cp2));
            return PlayPacketControl::Skip;
        }
        // chest window: single 0-26 chest,27-62 inv | double 0-26 A,27-53 B,54-89 inv
        let rows = if cp2.is_some() { 54i16 } else { 27i16 };
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let mut cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
        if slot == -999 {
            if cur.1 > 0 {
                // vanilla: left-click outside = throw whole stack, right-click = one
                let n = if btn == 1 { 1 } else { cur.1 };
                let it = cur.clone();
                if n >= cur.1 {
                    *cur = (0, 0, Vec::new());
                } else {
                    cur.1 -= n;
                }
                let (px, py, pz, ..) = {
                    let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                    *p
                };
                let d = Drop {
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    x: px,
                    y: py + 1.3,
                    z: pz,
                    item: it.0,
                    cnt: n,
                    comps: it.2.clone(),
                    age: 0,
                    target: None,
                    thrower: Some(me.uuid),
                };
                drops().lock().unwrap_or_else(|e| e.into_inner()).push(d);
            }
        } else if slot >= 0 && slot < rows + 36 {
            let (gslot, gmap): (i16, &mut HashMap<i16, Stack>) = if slot < 27 {
                (slot, all.entry(cp).or_default())
            } else if slot < rows {
                let Some(partner) = cp2 else {
                    return PlayPacketControl::Skip;
                };
                (slot - 27, all.entry(partner).or_default())
            } else {
                (slot - rows + 9, &mut inv)
            };
            click_slot(gmap, &mut cur, gslot, btn as i8);
            let st = gmap.get(&gslot).cloned().unwrap_or_default();
            let cursor = cur.clone();
            drop(all);
            drop(inv);
            drop(cur);
            send_chest_slot(me, slot, &st);
            resync_slot(me, 255, -1, &cursor);
            return PlayPacketControl::Skip;
        }
        if slot == -999 {
            let cursor = cur.clone();
            drop(cur);
            drop(inv);
            drop(all);
            resync_slot(me, 255, -1, &cursor);
        }
        return PlayPacketControl::Skip;
    }
    let furnace_pos = *me.open_furnace.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(fp) = furnace_pos {
        let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
        let f = fs.entry(fp).or_insert_with(Furnace::empty);
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let mut cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
        if slot == -999 {
            if cur.1 > 0 {
                // vanilla: left-click outside = throw whole stack, right-click = one
                let n = if btn == 1 { 1 } else { cur.1 };
                let it = cur.clone();
                if n >= cur.1 {
                    *cur = (0, 0, Vec::new());
                } else {
                    cur.1 -= n;
                }
                let (px, py, pz, ..) = {
                    let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                    *p
                };
                let d = Drop {
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    x: px,
                    y: py + 1.3,
                    z: pz,
                    item: it.0,
                    cnt: n,
                    comps: it.2.clone(),
                    age: 0,
                    target: None,
                    thrower: Some(me.uuid),
                };
                drops().lock().unwrap_or_else(|e| e.into_inner()).push(d);
            }
        } else if (0..39).contains(&slot) {
            if slot == 2 {
                // output: take only, merge onto cursor
                if f.output.1 > 0
                    && (cur.1 == 0
                        || (cur.0 == f.output.0
                            && cur.2 == f.output.2
                            && cur.1 + f.output.1 <= item_stack_max(f.output.0, &f.output.2) as u8))
                {
                    let got = f.output.1;
                    *cur = (f.output.0, cur.1 + got, f.output.2.clone());
                    f.output = (0, 0, Vec::new());
                    f.dirty = true;
                    let (px, py, pz, ..) = {
                        let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                        *p
                    };
                    furnace_xp_pop(f, px, py, pz);
                }
            } else if slot == 0 || slot == 1 {
                let m = if slot == 0 { &mut f.input } else { &mut f.fuel };
                let sv = if m.1 == 0 { None } else { Some(m.clone()) };
                let cv = cur.clone();
                if btn == 0 {
                    if cv.1 == 0 {
                        if let Some(s) = sv {
                            *cur = s;
                            *m = (0, 0, Vec::new());
                        }
                    } else {
                        match sv {
                            None => {
                                *m = cv;
                                *cur = (0, 0, Vec::new());
                            }
                            Some((si, sc, _cc)) => {
                                if si == cv.0
                                    && _cc == cv.2
                                    && sc as i64 + cv.1 as i64 <= item_stack_max(si, &_cc)
                                {
                                    *m = (si, sc + cv.1, _cc);
                                    *cur = (0, 0, Vec::new());
                                } else {
                                    *m = cv;
                                    *cur = (si, sc, _cc);
                                }
                            }
                        }
                    }
                } else if btn == 1 {
                    if cv.1 == 0 {
                        if let Some((si, sc, _cc)) = sv {
                            let take = sc - sc / 2;
                            *cur = (si, take, _cc.clone());
                            if sc - take == 0 {
                                *m = (0, 0, Vec::new());
                            } else {
                                *m = (si, sc - take, _cc);
                            }
                        }
                    } else {
                        match sv {
                            None => {
                                *m = (cv.0, 1, cv.2.clone());
                                *cur = (cv.0, cv.1 - 1, cv.2);
                            }
                            Some((si, sc, _cc)) => {
                                if si == cv.0 && _cc == cv.2 && sc < item_stack_max(si, &_cc) as u8
                                {
                                    *m = (si, sc + 1, _cc);
                                    *cur = (cv.0, cv.1 - 1, cv.2);
                                }
                            }
                        }
                    }
                }
                f.dirty = true;
            } else {
                let islot = slot + 6;
                click_slot(&mut inv, &mut cur, islot, btn as i8);
                *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
            }
        }
        // Snapshot the changed slots before acquiring the socket mutex.
        let slots = [f.input.clone(), f.fuel.clone(), f.output.clone()];
        let player_slot = (3..39).contains(&slot).then(|| {
            (
                slot,
                inv.get(&(slot + 6)).cloned().unwrap_or((0, 0, Vec::new())),
            )
        });
        let cursor = cur.clone();
        drop(cur);
        drop(inv);
        drop(fs);
        if (0..39).contains(&slot) {
            let mut sock = me.sock.lock().unwrap_or_else(|e| e.into_inner());
            for (wsl, st) in slots.iter().enumerate() {
                let mut p = vec![2u8];
                write_varint(&mut p, i64::from(inv_state_next(me)));
                p.extend_from_slice(&(wsl as i16).to_be_bytes());
                p.extend_from_slice(&slot_wire(st));
                let _ = send_packet(&mut sock, 0x15, p);
            }
            if let Some((slot, st)) = player_slot {
                let mut p = vec![2u8];
                write_varint(&mut p, i64::from(inv_state_next(me)));
                p.extend_from_slice(&slot.to_be_bytes());
                p.extend_from_slice(&slot_wire(&st));
                let _ = send_packet(&mut sock, 0x15, p);
            }
        }
        if slot == -999 || (0..39).contains(&slot) {
            resync_slot(me, 255, -1, &cursor);
        }
        wake_furnace_at(fp.0, fp.1, fp.2, fp.3);
        return PlayPacketControl::Skip;
    }
    if *me.open_table.lock().unwrap_or_else(|e| e.into_inner()) {
        let mut grid = me.craft9.lock().unwrap_or_else(|e| e.into_inner());
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let mut cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
        if slot == -999 {
            if cur.1 > 0 {
                let n = if btn == 1 { 1 } else { cur.1 };
                let it = cur.clone();
                if n >= cur.1 {
                    *cur = (0, 0, Vec::new());
                } else {
                    cur.1 -= n;
                }
                let (px, py, pz, ..) = {
                    let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                    *p
                };
                let d = Drop {
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    x: px,
                    y: py + 1.3,
                    z: pz,
                    item: it.0,
                    cnt: n,
                    comps: it.2.clone(),
                    age: 0,
                    target: None,
                    thrower: Some(me.uuid),
                };
                drops().lock().unwrap_or_else(|e| e.into_inner()).push(d);
                drop(cur);
                drop(grid);
                drop(inv);
                // spawn streams via interest_tick
                // drop meta streams via interest_tick
                return PlayPacketControl::Skip;
            }
        } else if slot == 0 {
            if let Some((ri, rc, _rc)) = craft3_result(&grid) {
                if cur.1 == 0
                    || (cur.0 == ri && cur.2 == _rc && cur.1 + rc <= item_stack_max(ri, &_rc) as u8)
                {
                    let overflow = consume_crafting_grid(&mut grid, 1..=9);
                    let (px, py, pz, ..) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
                    for remainder in overflow {
                        let remainder = move_to_slots(&mut inv, 9..=44, remainder);
                        if remainder.1 > 0 {
                            drop_live_stack((px, py + 1.3, pz), remainder);
                        }
                    }
                    *cur = (ri, cur.1 + rc, _rc.clone());
                }
            }
        } else if (1..=9).contains(&slot) {
            click_slot(&mut grid, &mut cur, slot, btn as i8);
        } else if (10..=45).contains(&slot) {
            click_slot(&mut inv, &mut cur, slot - 1, btn as i8);
            *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
        }
        // echo changed slot + result
        let (it, c, _ce) = if slot == 0 {
            (0, 0, Vec::new())
        } else if (1..=9).contains(&slot) {
            grid.get(&slot).cloned().unwrap_or((0, 0, Vec::new()))
        } else if slot >= 10 {
            inv.get(&(slot - 1)).cloned().unwrap_or((0, 0, Vec::new()))
        } else {
            (0, 0, Vec::new())
        };
        let out = craft3_result(&grid).unwrap_or((0, 0, Vec::new()));
        let grid_snapshot = (slot == 0).then(|| grid.clone());
        drop(grid);
        drop(inv);
        drop(cur);
        if slot >= 0 {
            let mut p = vec![3u8];
            write_varint(&mut p, i64::from(inv_state_next(me)));
            p.extend_from_slice(&slot.to_be_bytes());
            p.extend_from_slice(&slot_wire(&(it, c, _ce)));
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x15,
                p,
            );
        }
        let mut p0 = vec![3u8];
        write_varint(&mut p0, i64::from(inv_state_next(me)));
        p0.extend_from_slice(&0i16.to_be_bytes());
        p0.extend_from_slice(&slot_wire(&(out.0, out.1, Vec::new())));
        let _ = send_packet(
            &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x15,
            p0,
        );
        if let Some(grid) = grid_snapshot {
            for grid_slot in 1..=9i16 {
                let mut p = vec![3u8];
                write_varint(&mut p, i64::from(inv_state_next(me)));
                p.extend_from_slice(&grid_slot.to_be_bytes());
                p.extend_from_slice(&slot_wire(
                    &grid.get(&grid_slot).cloned().unwrap_or_default(),
                ));
                let _ = send_packet(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    0x15,
                    p,
                );
            }
        }
        return PlayPacketControl::Skip;
    }
    // Window 0 (player inventory + 2x2 craft grid). Vanilla's
    // post-click broadcast emits only the slots whose content
    // changed, in menu order (result, grid, armor, inventory,
    // offhand), then the carried stack when it differs from the
    // stack the client acknowledged in this click.
    let pre_grid = me.craft.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let pre_inv = me.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let pre_result = craft_result(&pre_grid).unwrap_or((0, 0, Vec::new()));
    if slot == -999 {
        let cur = {
            let c = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
            c.clone()
        };
        if cur.1 > 0 {
            let n = if btn == 1 { 1 } else { cur.1 };
            if n >= cur.1 {
                *me.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (0, 0, Vec::new());
            } else {
                me.cursor.lock().unwrap_or_else(|e| e.into_inner()).1 -= n;
            }
            let (px, py, pz, ..) = {
                let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                *p
            };
            let d = Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: px,
                y: py + 1.3,
                z: pz,
                item: cur.0,
                cnt: n,
                comps: cur.2.clone(),
                age: 0,
                target: None,
                thrower: Some(me.uuid),
            };
            drops().lock().unwrap_or_else(|e| e.into_inner()).push(d);
        }
    } else if slot == 0 {
        // craft output: take result, consume one of each ingredient
        let mut cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
        let mut grid = me.craft.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((ri, rc, _rc)) = craft_result(&grid) {
            if cur.1 == 0
                || (cur.0 == ri && cur.2 == _rc && cur.1 + rc <= item_stack_max(ri, &_rc) as u8)
            {
                let overflow = consume_crafting_grid(&mut grid, 1..=4);
                drop(grid);
                return_live_inputs(me, overflow);
                *cur = (ri, cur.1 + rc, _rc.clone());
            }
        }
    } else if (1..=4).contains(&slot) {
        let mut grid = me.craft.lock().unwrap_or_else(|e| e.into_inner());
        let mut cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
        click_slot(&mut grid, &mut cur, slot, btn as i8);
    } else if (5..=8).contains(&slot) || slot == 45 {
        // armor + offhand: vanilla restricts armor
        // slots to their matching piece type
        let armor_slot_ok = |it: i64, sl: i16| -> bool {
            if it == 0 {
                return true;
            }
            let n = |x: &str| name_to_item(x).unwrap_or(-1);
            match sl {
                5 => {
                    it == n("leather_helmet")
                        || it == n("golden_helmet")
                        || it == n("chainmail_helmet")
                        || it == n("iron_helmet")
                        || it == n("diamond_helmet")
                        || it == n("netherite_helmet")
                        || it == n("turtle_helmet")
                }
                6 => {
                    it == n("leather_chestplate")
                        || it == n("golden_chestplate")
                        || it == n("chainmail_chestplate")
                        || it == n("iron_chestplate")
                        || it == n("diamond_chestplate")
                        || it == n("netherite_chestplate")
                        || it == n("elytra")
                }
                7 => {
                    it == n("leather_leggings")
                        || it == n("golden_leggings")
                        || it == n("chainmail_leggings")
                        || it == n("iron_leggings")
                        || it == n("diamond_leggings")
                        || it == n("netherite_leggings")
                }
                8 => {
                    it == n("leather_boots")
                        || it == n("golden_boots")
                        || it == n("chainmail_boots")
                        || it == n("iron_boots")
                        || it == n("diamond_boots")
                        || it == n("netherite_boots")
                }
                _ => true,
            }
        };
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let mut cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
        if cur.1 > 0 && armor_slot_ok(cur.0, slot) || cur.1 == 0 {
            click_slot(&mut inv, &mut cur, slot, btn as i8);
            *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
        }
    } else if (9..=44).contains(&slot) {
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let mut cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
        click_slot(&mut inv, &mut cur, slot, btn as i8);
        *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
    }
    let post_grid = me.craft.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let post_inv = me.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let post_cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let post_result = craft_result(&post_grid).unwrap_or((0, 0, Vec::new()));
    let carried = click_carried_stack(click_tail);
    let mut sock = me.sock.lock().unwrap_or_else(|e| e.into_inner());
    if post_result != pre_result {
        let mut p = vec![0u8];
        write_varint(&mut p, i64::from(inv_state_next(me)));
        p.extend_from_slice(&0i16.to_be_bytes());
        p.extend_from_slice(&slot_wire(&post_result));
        let _ = send_packet(&mut sock, 0x15, p);
    }
    for i in 1..=4i16 {
        let now = post_grid.get(&i).cloned().unwrap_or((0, 0, Vec::new()));
        let was = pre_grid.get(&i).cloned().unwrap_or((0, 0, Vec::new()));
        if now != was {
            let mut p = vec![0u8];
            write_varint(&mut p, i64::from(inv_state_next(me)));
            p.extend_from_slice(&i.to_be_bytes());
            p.extend_from_slice(&slot_wire(&now));
            let _ = send_packet(&mut sock, 0x15, p);
        }
    }
    for i in 5..=45i16 {
        let now = post_inv.get(&i).cloned().unwrap_or((0, 0, Vec::new()));
        let was = pre_inv.get(&i).cloned().unwrap_or((0, 0, Vec::new()));
        if now != was {
            let mut p = vec![0u8];
            write_varint(&mut p, i64::from(inv_state_next(me)));
            p.extend_from_slice(&i.to_be_bytes());
            p.extend_from_slice(&slot_wire(&now));
            let _ = send_packet(&mut sock, 0x15, p);
        }
    }
    let cursor_differs = match carried {
        Some(st) => st != post_cur,
        None => post_cur.1 > 0,
    };
    if cursor_differs {
        let mut p = vec![0xFFu8];
        write_varint(&mut p, i64::from(inv_state_next(me)));
        p.extend_from_slice(&(-1i16).to_be_bytes());
        p.extend_from_slice(&slot_wire(&post_cur));
        let _ = send_packet(&mut sock, 0x15, p);
    }
    PlayPacketControl::Handled
}

fn swap_inventory_slots(inv: &mut HashMap<i16, Stack>, a: i16, b: i16) {
    if a == b {
        return;
    }
    let first = inv.remove(&a);
    let second = inv.remove(&b);
    inv.extend(second.filter(|s| s.1 > 0).map(|s| (a, s)));
    inv.extend(first.filter(|s| s.1 > 0).map(|s| (b, s)));
}

fn play_c2s_window_click_mode2(me: &Arc<Client>, slot: i16, btn: u8) -> PlayPacketControl {
    let Some(hand_slot) = inventory_swap_slot(btn as i8) else {
        return PlayPacketControl::Skip;
    };
    let chest_pos = *me.open_chest.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(window @ (first, second)) = chest_pos {
        if !window_live_for(me, window) {
            invalidate_client_chest_window(&client_snapshot(), me, window);
            return PlayPacketControl::Skip;
        }
        let rows = if second.is_some() { 54 } else { 27 };
        if !(0..rows + 36).contains(&slot) {
            return PlayPacketControl::Skip;
        }
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let clicked = if slot < rows {
            let (pos, index) = if slot < 27 {
                (first, slot)
            } else {
                (second.expect("double chest's second half"), slot - 27)
            };
            let chest = all.entry(pos).or_default();
            let hand = inv.remove(&hand_slot).unwrap_or_default();
            if let Some(stack) = chest.remove(&index).filter(|s| s.1 > 0) {
                inv.insert(hand_slot, stack);
            }
            if hand.1 > 0 {
                chest.insert(index, hand.clone());
            }
            hand
        } else {
            let index = slot - rows + 9;
            swap_inventory_slots(&mut inv, index, hand_slot);
            inv.get(&index).cloned().unwrap_or_default()
        };
        let hand = inv.get(&hand_slot).cloned().unwrap_or_default();
        drop(inv);
        drop(all);
        resync_slot(me, 1, slot, &clicked);
        if hand_slot == OFFHAND_MENU_SLOT {
            resync_slot(me, 0, hand_slot, &hand);
        } else if slot != rows + hand_slot - 9 {
            resync_slot(me, 1, rows + hand_slot - 9, &hand);
        }
        return PlayPacketControl::Handled;
    }

    let furnace_pos = *me.open_furnace.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(fp) = furnace_pos {
        if !(0..39).contains(&slot) {
            return PlayPacketControl::Skip;
        }
        let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let clicked = if slot >= 3 {
            let index = slot + 6;
            swap_inventory_slots(&mut inv, index, hand_slot);
            inv.get(&index).cloned().unwrap_or_default()
        } else {
            let furnace = fs.entry(fp).or_insert_with(Furnace::empty);
            let hand = inv.get(&hand_slot).cloned().unwrap_or_default();
            let target = match slot {
                0 => &mut furnace.input,
                1 => &mut furnace.fuel,
                _ => &mut furnace.output,
            };
            let can_swap = match slot {
                1 => hand.1 == 0 || fuel_ticks(hand.0) > 0 || (hand.1 == 1 && hand.0 == 908),
                2 => hand.1 == 0,
                _ => true,
            };
            if can_swap {
                if target.1 > 0 {
                    inv.insert(hand_slot, target.clone());
                } else {
                    inv.remove(&hand_slot);
                }
                *target = hand;
                let clicked = target.clone();
                furnace.dirty = true;
                if slot == 2 && inv.contains_key(&hand_slot) {
                    let (x, y, z, ..) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
                    furnace_xp_pop(furnace, x, y, z);
                }
                clicked
            } else {
                target.clone()
            }
        };
        let hand = inv.get(&hand_slot).cloned().unwrap_or_default();
        drop(inv);
        drop(fs);
        resync_slot(me, 2, slot, &clicked);
        if hand_slot == OFFHAND_MENU_SLOT {
            resync_slot(me, 0, hand_slot, &hand);
        } else if slot != hand_slot - 6 {
            resync_slot(me, 2, hand_slot - 6, &hand);
        }
        wake_furnace_at(fp.0, fp.1, fp.2, fp.3);
        return PlayPacketControl::Handled;
    }

    // Window 0 slots that accept either hand without a placement restriction.
    if ((9..=44).contains(&slot) || slot == OFFHAND_MENU_SLOT) && slot != hand_slot {
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        swap_inventory_slots(&mut inv, slot, hand_slot);
        let clicked = inv.get(&slot).cloned().unwrap_or_default();
        let hand = inv.get(&hand_slot).cloned().unwrap_or_default();
        drop(inv);
        resync_slot(me, 0, slot, &clicked);
        resync_slot(me, 0, hand_slot, &hand);
    }
    PlayPacketControl::Handled
}

fn play_c2s_crafting_table_click(me: &Arc<Client>, slot: i16, button: u8, mode: i64) {
    if !(0..=45).contains(&slot) {
        return;
    }
    let mut grid = me.craft9.lock().unwrap_or_else(|e| e.into_inner());
    let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
    let before = inv.clone();
    let mut outgoing = Vec::new();
    match (mode, slot) {
        (1, 0) => outgoing.extend(quick_move_crafting_result(&mut inv, &mut grid, 1..=9, craft3_result)),
        (1, 1..=9) => {
            if let Some(stack) = map_take(&mut grid, slot, u8::MAX) {
                let remainder = move_to_slots(&mut inv, 9..=44, stack);
                if remainder.1 > 0 {
                    grid.insert(slot, remainder);
                }
            }
        }
        (1, 10..=45) => {
            let index = slot - 1;
            let destination = if slot <= 36 { 36..=44 } else { 9..=35 };
            if let Some(stack) = map_take(&mut inv, index, u8::MAX) {
                let remainder = move_to_slots(&mut inv, destination, stack);
                if remainder.1 > 0 {
                    inv.insert(index, remainder);
                }
            }
        }
        (2, _) => {
            let Some(hand) = inventory_swap_slot(button as i8) else {
                return;
            };
            match slot {
                0 if inv.get(&hand).is_none_or(|stack| stack.1 == 0) => {
                    if let Some(result) = craft3_result(&grid) {
                        inv.insert(hand, result);
                        outgoing.extend(consume_crafting_grid(&mut grid, 1..=9));
                    }
                }
                1..=9 => {
                    let held = map_take(&mut inv, hand, u8::MAX);
                    if let Some(stack) = map_take(&mut grid, slot, u8::MAX) {
                        inv.insert(hand, stack);
                    }
                    if let Some(stack) = held {
                        grid.insert(slot, stack);
                    }
                }
                10..=45 => swap_inventory_slots(&mut inv, slot - 1, hand),
                _ => {}
            }
        }
        (4, 0) if button <= 1 => {
            if let Some(result) = craft3_result(&grid) {
                outgoing.push(result);
                outgoing.extend(consume_crafting_grid(&mut grid, 1..=9));
            }
        }
        (4, _) if button <= 1 => {
            let (slots, index) = if slot <= 9 { (&mut *grid, slot) } else { (&mut *inv, slot - 1) };
            outgoing.extend(map_take(slots, index, if button == 1 { u8::MAX } else { 1 }));
        }
        _ => return,
    }
    let changed: Vec<_> = (9..=OFFHAND_MENU_SLOT)
        .filter(|index| before.get(index) != inv.get(index))
        .map(|index| (index, inv.get(&index).cloned().unwrap_or_default()))
        .collect();
    drop(inv);
    drop(grid);
    if !outgoing.is_empty() {
        let (dim, x, y, z, ..) = client_dim_pos(me);
        for stack in outgoing {
            drop_live_stack_dim(dim, (x, y + 1.3, z), stack);
        }
    }
    for (index, stack) in changed {
        resync_slot(me, 0, index, &stack);
    }
    resend_container_content(me, 3);
}

fn play_c2s_window_click_mode6(me: &Arc<Client>, creative: bool) -> PlayPacketControl {
    // double-click: gather all matching stacks in the
    // window onto the cursor (up to stack cap)
    let chest_pos = *me.open_chest.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cp, cp2)) = chest_pos {
        if !window_live_for(me, (cp, cp2)) {
            invalidate_client_chest_window(&client_snapshot(), me, (cp, cp2));
            return PlayPacketControl::Skip;
        }
    }
    let mut cur = me.cursor.lock().unwrap_or_else(|e| e.into_inner());
    let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
    if cur.1 > 0 {
        let cap = item_stack_max(cur.0, &cur.2);
        if let Some((cp, cp2)) = chest_pos {
            let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
            for pos in [cp].into_iter().chain(cp2) {
                let m = all.entry(pos).or_default();
                let ks: Vec<i16> = m
                    .iter()
                    .filter(|(_, v)| v.0 == cur.0 && v.2 == cur.2)
                    .map(|(&k, _)| k)
                    .collect();
                for k in ks {
                    if cur.1 as i64 >= cap {
                        break;
                    }
                    let mut remove = false;
                    if let Some(entry) = m.get_mut(&k) {
                        let take = (cap - cur.1 as i64).min(entry.1 as i64) as u8;
                        entry.1 -= take;
                        cur.1 += take;
                        remove = entry.1 == 0;
                    }
                    if remove {
                        m.remove(&k);
                    }
                }
            }
        }
        if (cur.1 as i64) < cap {
            let ks: Vec<i16> = inv
                .iter()
                .filter(|(&slot, stack)| {
                    stack.0 == cur.0
                        && stack.2 == cur.2
                        && !armor_removal_blocked(slot, Some(stack), creative)
                })
                .map(|(&k, _)| k)
                .collect();
            for k in ks {
                if cur.1 as i64 >= cap {
                    break;
                }
                let mut remove = false;
                if let Some(entry) = inv.get_mut(&k) {
                    let take = (cap - cur.1 as i64).min(entry.1 as i64) as u8;
                    entry.1 -= take;
                    cur.1 += take;
                    remove = entry.1 == 0;
                }
                if remove {
                    inv.remove(&k);
                }
            }
        }
    }
    let cp2 = cur.clone();
    drop(inv);
    drop(cur);
    // cursor echo (window slot -1) + whole-inventory refresh is cheapest
    let mut pc = vec![0u8];
    write_varint(&mut pc, i64::from(inv_state_next(me)));
    pc.extend_from_slice(&(-1i16).to_be_bytes());
    pc.extend_from_slice(&slot_wire(&cp2));
    if let Ok(mut w) = me.sock.lock() {
        let _ = send_packet(&mut w, 0x15, pc);
    }
    PlayPacketControl::Handled
}

fn play_c2s_window_click_mode14(
    me: &Arc<Client>,
    slot: i16,
    btn: u8,
    mode: i64,
) -> PlayPacketControl {
    // vanilla click modes: 4=throw hovered slot (btn 0=one,
    // 1=whole stack); 1=shift-move between regions
    let chest_pos = *me.open_chest.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cp, cp2)) = chest_pos {
        if !window_live_for(me, (cp, cp2)) {
            invalidate_client_chest_window(&client_snapshot(), me, (cp, cp2));
            return PlayPacketControl::Skip;
        }
    }
    let furnace_pos = *me.open_furnace.lock().unwrap_or_else(|e| e.into_inner());
    let (px, py, pz, ..) = {
        let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
        *p
    };
    macro_rules! throw {
        ($st:expr) => {{
            let st: Stack = $st;
            if st.1 > 0 {
                drops()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Drop {
                        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                        x: px,
                        y: py + 1.3,
                        z: pz,
                        item: st.0,
                        cnt: st.1,
                        comps: st.2,
                        age: 0,
                        target: None,
                        thrower: Some(me.uuid),
                    });
            }
        }};
    }
    if let Some((cp, cp2)) = chest_pos {
        // window slots: 0..rows chest | rows..rows+36 inv(key=slot-rows+9)
        let rows = if cp2.is_some() { 54i16 } else { 27i16 };
        let mut all = chests().lock().unwrap_or_else(|e| e.into_inner());
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        if slot >= 0 && slot < rows + 36 {
            let in_cont = slot < rows;
            if mode == 4 {
                let n = if btn == 1 { 255 } else { 1 };
                let got = if in_cont {
                    let (pos, gslot) = if slot < 27 {
                        (cp, slot)
                    } else if let Some(partner) = cp2 {
                        (partner, slot - 27)
                    } else {
                        return PlayPacketControl::Skip;
                    };
                    map_take(all.entry(pos).or_default(), gslot, n)
                } else {
                    map_take(&mut inv, slot - rows + 9, n)
                };
                if let Some(st) = got {
                    throw!(st);
                }
            } else {
                let get_slot = |index, all: &HashMap<ContainerKey, HashMap<i16, Stack>>,
                                inv: &HashMap<i16, Stack>| {
                    if index >= rows {
                        inv.get(&(index - rows + 9)).cloned().unwrap_or_default()
                    } else {
                        let (key, index) = if index < 27 {
                            (cp, index)
                        } else {
                            (cp2.expect("double chest's second half"), index - 27)
                        };
                        all.get(&key).and_then(|map| map.get(&index)).cloned().unwrap_or_default()
                    }
                };
                let before: Vec<_> = (0..rows + 36).map(|i| get_slot(i, &all, &inv)).collect();
                quick_move_chest(&mut all, &mut inv, cp, cp2, slot);
                let changed: Vec<_> = (0..rows + 36).zip(before)
                    .filter_map(|(i, old)| {
                        let stack = get_slot(i, &all, &inv);
                        (i == slot || old != stack).then_some((i, stack))
                    })
                    .collect();
                drop(inv);
                drop(all);
                for (index, stack) in changed {
                    resync_slot(me, 1, index, &stack);
                }
                return PlayPacketControl::Skip;
            }
            // resync touched slot + inventory/chest echo kept cheap
            let cur_st = if in_cont {
                let (pos, gslot) = if slot < 27 {
                    (cp, slot)
                } else if let Some(partner) = cp2 {
                    (partner, slot - 27)
                } else {
                    return PlayPacketControl::Skip;
                };
                all.get(&pos)
                    .and_then(|m| m.get(&gslot))
                    .cloned()
                    .unwrap_or((0, 0, Vec::new()))
            } else {
                inv.get(&(slot - rows + 9))
                    .cloned()
                    .unwrap_or((0, 0, Vec::new()))
            };
            resync_slot(me, 1, slot, &cur_st);
        }
        return PlayPacketControl::Skip;
    }
    if let Some(fp) = furnace_pos {
        // window: 0 input,1 fuel,2 output | 3..38 inv(key=slot+6)
        let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        if (0..39).contains(&slot) {
            if mode == 4 {
                if slot >= 3 {
                    if let Some(st) = map_take(&mut inv, slot + 6, if btn == 1 { 255 } else { 1 }) {
                        throw!(st);
                    }
                } else {
                    let f = fs.entry(fp).or_insert(Furnace {
                        input: (0, 0, Vec::new()),
                        fuel: (0, 0, Vec::new()),
                        output: (0, 0, Vec::new()),
                        burn_left: 0,
                        progress: 0,
                        dirty: false,
                        xp: 0.0,
                        recipe_cache: CachedLookup::default(),
                        fuel_cache: CachedLookup::default(),
                    });
                    let m = if slot == 0 {
                        &mut f.input
                    } else if slot == 1 {
                        &mut f.fuel
                    } else {
                        &mut f.output
                    };
                    let n = if btn == 1 { m.1 } else { 1.min(m.1) };
                    if n > 0 {
                        let st = (m.0, n, m.2.clone());
                        m.1 -= n;
                        if m.1 == 0 {
                            *m = (0, 0, Vec::new());
                        }
                        f.dirty = true;
                        if slot == 2 {
                            let (px, py, pz, ..) = {
                                let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                                *p
                            };
                            furnace_xp_pop(f, px, py, pz);
                        }
                        throw!(st);
                    }
                }
            } else if slot == 2 {
                // shift-take output into inventory
                let f = fs.entry(fp).or_insert(Furnace {
                    input: (0, 0, Vec::new()),
                    fuel: (0, 0, Vec::new()),
                    output: (0, 0, Vec::new()),
                    burn_left: 0,
                    progress: 0,
                    dirty: false,
                    xp: 0.0,
                    recipe_cache: CachedLookup::default(),
                    fuel_cache: CachedLookup::default(),
                });
                if f.output.1 > 0 {
                    let st = std::mem::take(&mut f.output);
                    f.dirty = true;
                    let (px, py, pz, ..) = {
                        let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                        *p
                    };
                    furnace_xp_pop(f, px, py, pz);
                    let rem = move_to_slots(&mut inv, 9..=44, st);
                    if rem.1 > 0 {
                        if let Some(f2) = fs.get_mut(&fp) {
                            f2.output = rem;
                        }
                    }
                }
            } else if slot <= 1 {
                // shift input/fuel -> inv
                let f = fs.entry(fp).or_insert(Furnace {
                    input: (0, 0, Vec::new()),
                    fuel: (0, 0, Vec::new()),
                    output: (0, 0, Vec::new()),
                    burn_left: 0,
                    progress: 0,
                    dirty: false,
                    xp: 0.0,
                    recipe_cache: CachedLookup::default(),
                    fuel_cache: CachedLookup::default(),
                });
                let m = if slot == 0 { &mut f.input } else { &mut f.fuel };
                if m.1 > 0 {
                    let st = std::mem::take(m);
                    f.dirty = true;
                    let rem = move_to_slots(&mut inv, 9..=44, st);
                    if rem.1 > 0 {
                        if let Some(f2) = fs.get_mut(&fp) {
                            if slot == 0 {
                                f2.input = rem
                            } else {
                                f2.fuel = rem
                            }
                        }
                    }
                }
            } else {
                // shift inv -> furnace input/fuel by type, else stays
                if let Some(st) = inv.get(&(slot + 6)).cloned() {
                    let f = fs.entry(fp).or_insert(Furnace {
                        input: (0, 0, Vec::new()),
                        fuel: (0, 0, Vec::new()),
                        output: (0, 0, Vec::new()),
                        burn_left: 0,
                        progress: 0,
                        dirty: false,
                        xp: 0.0,
                        recipe_cache: CachedLookup::default(),
                        fuel_cache: CachedLookup::default(),
                    });
                    let dst: i16 = if smelt_out(st.0).is_some() {
                        0
                    } else if fuel_ticks(st.0) > 0 {
                        1
                    } else {
                        -1
                    };
                    if dst >= 0 {
                        let m = if dst == 0 { &mut f.input } else { &mut f.fuel };
                        let fits = m.1 == 0
                            || (m.0 == st.0
                                && m.2 == st.2
                                && (m.1 as i64) < item_stack_max(m.0, &m.2));
                        if fits {
                            inv.remove(&(slot + 6));
                            let room = item_stack_max(st.0, &st.2).min(255) as u8;
                            let t = if m.1 == 0 {
                                room.min(st.1)
                            } else {
                                (room - m.1).min(st.1)
                            };
                            if m.1 == 0 {
                                *m = (st.0, t, st.2.clone());
                            } else {
                                m.1 += t;
                            }
                            f.dirty = true;
                            if t < st.1 {
                                inv.insert(slot + 6, (st.0, st.1 - t, st.2));
                            }
                        }
                    }
                }
            }
            let cur_st = if slot >= 3 {
                inv.get(&(slot + 6)).cloned().unwrap_or((0, 0, Vec::new()))
            } else if let Some(f2) = fs.get(&fp) {
                match slot {
                    0 => f2.input.clone(),
                    1 => f2.fuel.clone(),
                    _ => f2.output.clone(),
                }
            } else {
                (0, 0, Vec::new())
            };
            resync_slot(me, 2, slot, &cur_st);
            wake_furnace_at(fp.0, fp.1, fp.2, fp.3);
        }
        return PlayPacketControl::Skip;
    }
    // inventory window: 5-8 armor | 45 offhand | 1-4 craft | 9-44 inv
    {
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let mut grid = me.craft.lock().unwrap_or_else(|e| e.into_inner());
        if mode == 4 {
            let got = if (5..=8).contains(&slot) || (9..=44).contains(&slot) || slot == 45 {
                map_take(&mut inv, slot, if btn == 1 { 255 } else { 1 })
            } else if (1..=4).contains(&slot) {
                map_take(&mut grid, slot, if btn == 1 { 255 } else { 1 })
            } else {
                None
            };
            if let Some(st) = got {
                throw!(st);
            }
            resync_slot(
                me,
                0,
                slot,
                &inv.get(&slot).cloned().unwrap_or((0, 0, Vec::new())),
            );
        } else if slot == 0 {
            let overflow = quick_move_crafting_result(&mut inv, &mut grid, 1..=4, craft_result);
            let (px, py, pz, ..) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
            for remainder in overflow {
                drop_live_stack((px, py + 1.3, pz), remainder);
            }
        } else if (9..=35).contains(&slot) {
            // shift from main inv: armor equips, else -> hotbar
            if let Some(st) = inv.get(&slot).cloned() {
                let asl = armor_slot_for(st.0);
                if asl >= 0 && !inv.contains_key(&asl) {
                    inv.remove(&slot);
                    resync_slot(me, 0, asl, &st);
                    inv.insert(asl, st);
                } else {
                    inv.remove(&slot);
                    let rem = move_to_slots(&mut inv, player_hotbar_menu_slots(), st);
                    if rem.1 > 0 {
                        inv.insert(slot, rem);
                    }
                }
            }
        } else if hotbar_slot_from_menu(slot).is_some() {
            if let Some(st) = inv.get(&slot).cloned() {
                inv.remove(&slot);
                let rem = move_to_slots(&mut inv, 9..=35, st);
                if rem.1 > 0 {
                    inv.insert(slot, rem);
                }
            }
        } else if (5..=8).contains(&slot) || slot == 45 {
            // unequip into inventory
            if let Some(st) = inv.get(&slot).cloned() {
                inv.remove(&slot);
                let rem = move_to_slots(&mut inv, 9..=44, st);
                if rem.1 > 0 {
                    inv.insert(slot, rem);
                }
            }
        } else if (1..=4).contains(&slot) {
            if let Some(st) = grid.get(&slot).cloned() {
                grid.remove(&slot);
                let rem = move_to_slots(&mut inv, 9..=44, st);
                if rem.1 > 0 {
                    grid.insert(slot, rem);
                }
            }
        }
        *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
        drop(grid);
        drop(inv);
        if slot == 0 {
            let grid = me.craft.lock().unwrap_or_else(|e| e.into_inner()).clone();
            resync_slot(me, 0, 0, &craft_result(&grid).unwrap_or_default());
            for craft_slot in 1..=4i16 {
                resync_slot(
                    me,
                    0,
                    craft_slot,
                    &grid.get(&craft_slot).cloned().unwrap_or_default(),
                );
            }
            let inventory = me.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for inventory_slot in 5..=45i16 {
                resync_slot(
                    me,
                    0,
                    inventory_slot,
                    &inventory.get(&inventory_slot).cloned().unwrap_or_default(),
                );
            }
            let cursor = me.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
            resync_slot(me, 0, -1, &cursor);
        } else {
            resync_slot(me, 0, slot, &{
                let i2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                i2.get(&slot).cloned().unwrap_or((0, 0, Vec::new()))
            });
        }
        // armor/offhand moves need armor slot echo too
        if (5..=8).contains(&slot) || slot == 45 || mode == 1 {
            for asl in 5..=8i16 {
                let st2 = me
                    .inv
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&asl)
                    .cloned()
                    .unwrap_or((0, 0, Vec::new()));
                resync_slot(me, 0, asl, &st2);
            }
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_window_click(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let Some((&wid, mut sl)) = b.split_first() else {
        return PlayPacketControl::Skip;
    };
    let Some(click_state_id) = read_protocol_i32(&mut sl) else {
        return PlayPacketControl::Skip;
    };
    if sl.len() >= 4 {
        let Some(slot_bytes) = sl.get(..2).and_then(|s| <[u8; 2]>::try_from(s).ok()) else {
            return PlayPacketControl::Skip;
        };
        let slot = i16::from_be_bytes(slot_bytes);
        let btn = sl[2];
        sl = &sl[3..];
        let Some(mode) = read_protocol_i32(&mut sl).map(i64::from) else {
            return PlayPacketControl::Skip;
        };
        let click_tail = sl;
        if me.is_spectator() {
            resend_container_content(&me, wid);
            return PlayPacketControl::Skip;
        }
        if let Some(active) = live_menu_id(&me) {
            if wid == active {
                // Vanilla never rejects on a stale stateId: it
                // pushes the authoritative contents first, then
                // processes the click anyway.
                let _ = live_menu_connection(&me).is_some_and(|conn| {
                    let mut session = conn.active.lock().unwrap_or_else(|e| e.into_inner());
                    match session.as_mut() {
                        Some(s) if s.id == wid && s.state_id != click_state_id => {
                            sync_live_menu(&me, s, false);
                            true
                        }
                        _ => false,
                    }
                });
                with_save_guard(|| live_menu_click(&me, wid, slot, btn as i8, mode));
            }
            return PlayPacketControl::Skip;
        }
        let active = if me
            .open_chest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            1
        } else if me
            .open_furnace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            2
        } else if *me.open_table.lock().unwrap_or_else(|e| e.into_inner()) {
            3
        } else {
            0
        };
        if wid != active {
            return PlayPacketControl::Skip;
        }
        // Vanilla never rejects on a stale stateId (verified
        // in 1.21.1 ServerGamePacketListenerImpl.
        // handleContainerClick bytecode): the click applies
        // unconditionally; the flag only selects
        // broadcastFullState over broadcastChanges. We push
        // the authoritative SetContainerContent snapshot with
        // a fresh id first, then process the click anyway.
        if container_state_stale(&me, wid, click_state_id) {
            resend_container_content(&me, wid);
        }
        let creative = *me.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
        if wid == 0 && (5..=8).contains(&slot) && matches!(mode, 0 | 1 | 4) {
            let stack = me
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&slot)
                .cloned();
            if armor_removal_blocked(slot, stack.as_ref(), creative) {
                if let Some(blocked) = stack.as_ref() {
                    resync_slot(&me, 0, slot, blocked);
                }
                let cursor = me.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
                resync_slot(&me, 0, -1, &cursor);
                return PlayPacketControl::Skip;
            }
        }
        if wid == 3 && matches!(mode, 1 | 2 | 4) {
            play_c2s_crafting_table_click(&me, slot, btn, mode);
            return PlayPacketControl::Handled;
        }
        if mode == 0 {
            return play_c2s_window_click_mode0(&me, slot, btn, click_tail);
        } else if mode == 2 {
            return play_c2s_window_click_mode2(&me, slot, btn);
        } else if mode == 6 {
            return play_c2s_window_click_mode6(&me, creative);
        } else if mode == 5 {
            // drag-paint: btn 0/4 start (left/right), 1/5 add slot, 2/6 end
            if wid == 3 && matches!(btn, 1 | 5) && !(1..=45).contains(&slot) {
                return PlayPacketControl::Skip;
            }
            drag_paint_click(&me, slot, btn as i32);
            if wid == 3 && matches!(btn, 2 | 6) {
                resend_container_content(&me, 3);
            }
        } else if mode == 4 || mode == 1 {
            return play_c2s_window_click_mode14(&me, slot, btn, mode);
        }
    }
    PlayPacketControl::Handled
}

fn play_c2s_container_slot_state_changed(
    cx: &mut PlayPacketContext<'_>,
    b: &[u8],
) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // container_slot_state_changed: crafter slot toggles land on
    // the live Crafter block entity the menu anchors to.
    if let Some((slot_id, window_id, new_state)) = decode_container_slot_state(b) {
        c2s_apply_container_slot_state(&me, slot_id, window_id, new_state);
    }
    PlayPacketControl::Handled
}
