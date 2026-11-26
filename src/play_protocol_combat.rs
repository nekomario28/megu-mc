fn play_c2s_arm_animation(cx: &mut PlayPacketContext<'_>, _b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    // arm_animation: relay swing to others
    broadcast_seen(me.eid, |_c| {
        (0x03, {
            let mut p = Vec::new();
            write_varint(&mut p, me.eid as i64);
            p.push(0);
            p
        })
    });
    PlayPacketControl::Handled
}

fn play_c2s_use_item(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let held_slot = *cx.held_slot;
    // use_item: ack + eat + bow draw
    let mut sl = b;
    let _h = read_varint(&mut sl);
    let seq = read_varint(&mut sl).unwrap_or(0);
    // vanilla `WrittenBookItem.use` → `openItemGui` sends
    // `ClientboundOpenBookPacket` for the used hand.
    let held_item = {
        let inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let slot = if _h == Some(1) {
            45
        } else {
            held_player_menu_slot(held_slot)
        };
        inv.get(&slot).map(|v| v.0).unwrap_or(0)
    };
    if held_item == name_to_item("written_book").unwrap_or(-1) {
        if let Some((_, pl)) = encode_play_open_book(_h.unwrap_or(0) as u8) {
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x32,
                pl,
            );
        }
    }
    if !try_use_ender_eye(&me, held_slot, seq) {
        note_item_use_start(&me);
        air_use_item(&me, held_slot, seq, _h, false);
    }
    PlayPacketControl::Handled
}

fn play_c2s_use_entity_interact(
    me: &Arc<Client>,
    target: i32,
    held_slot: i16,
) -> PlayPacketControl {
    let cart_uuid = generated_content()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .carts
        .iter()
        .find(|c| c.eid == target)
        .and_then(|c| c.uuid().ok());
    if let Some(uuid) = cart_uuid {
        if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 3 {
            unpack_cart_loot_if_needed(uuid);
            let key = ContainerKey::Cart(uuid);
            let (audience, before) =
                if let Some((prev, prev_anchor)) = client_open_window_and_anchor(me) {
                    let aud = client_snapshot();
                    let cdim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
                    let halves = client_window_halves_dim(prev, prev_anchor, cdim);
                    let snap = chest_opener_snapshot_dim(&aud, &halves);
                    (aud, snap)
                } else {
                    (Vec::new(), Vec::new())
                };
            close_live_menu(me, false, true);
            let inv = me.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
            open_chest_for_client(me, (key, None), None, &inv, "Minecart with Chest");
            if !before.is_empty() {
                sync_chest_openers_dim(&audience, &before);
            }
        }
        return PlayPacketControl::Skip;
    }
    // `LeashFenceKnotEntity.interact` runs `LeadItem.bindPlayerMobs`:
    // every mob leashed to this player re-anchors onto the knot.
    if let Some(&(kdim, kpos)) = leash_knots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&target)
    {
        if kdim == *me.dim.lock().unwrap_or_else(|e| e.into_inner()) {
            hitch_player_tethers(me.eid, kdim, kpos);
        }
        return PlayPacketControl::Skip;
    }
    // `LeadItem`/`NameTagItem` `mobInteract`: leash toggle and
    // custom-name application before the pet paths.
    if leash_name_tag_interact(me, target, held_player_menu_slot(held_slot)) {
        return PlayPacketControl::Skip;
    }
    if pet_collar_or_parrot_interact(me, target, held_player_menu_slot(held_slot)) {
        return PlayPacketControl::Skip;
    }
    // `IronGolem`/`SnowGolem`/`Allay` `mobInteract` — ingot
    // repair, pumpkin shear, and the allay's sample give/take
    // ride the same lane as the pet paths.
    if utility_mob_interact(me, target, held_player_menu_slot(held_slot)) {
        return PlayPacketControl::Skip;
    }
    if live_mob_interact(me, target, held_player_menu_slot(held_slot)) {
        return PlayPacketControl::Skip;
    }
    // Boats, minecarts and the rideable mobs: `interact` on a
    // free seat starts the ride and updates every tracker with
    // the new SetPassengers list.
    if try_mount_vehicle(me, target) {
        return PlayPacketControl::Skip;
    }
    // Interact with merchants, or use the held item on an animal.
    let merchant_types = [name_to_etype("villager"), name_to_etype("wandering_trader")];
    let merchant = mobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|m| m.eid == target && merchant_types.contains(&Some(m.etype)))
        .cloned();
    let is_merchant = merchant.is_some();
    let held_it = me
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&held_player_menu_slot(held_slot))
        .cloned()
        .unwrap_or((0, 0, Vec::new()))
        .0;
    if held_it == 983 && !is_merchant {
        // shears on sheep -> wool
        let is_sheep = {
            let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
            ms.iter().any(|m| m.eid == target && m.etype == 87)
        };
        if is_sheep
            && !sheared()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&target)
        {
            sheared()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(target);
            // `Sheep.shear` plays `entity.sheep.shear` via
            // `playSound` on the sheep — entity-attached, in
            // the `neutral` source, for every tracker.
            broadcast_entity_sound(target, "minecraft:entity.sheep.shear", 6);
            let (mx, my, mz) = {
                let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
                ms.iter()
                    .find(|m| m.eid == target)
                    .map(|m| *m.pos.lock().unwrap_or_else(|e| e.into_inner()))
                    .map(|p| (p.0, p.1, p.2))
                    .unwrap_or((0.0, 0.0, 0.0))
            };
            let n = 1 + ((target as u64).wrapping_mul(2654435761) % 3) as u8;
            for _ in 0..n {
                let deid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                let d = Drop {
                    eid: deid,
                    x: mx,
                    y: my + 0.5,
                    z: mz,
                    item: 202,
                    cnt: 1,
                    comps: Vec::new(),
                    age: 0,
                    target: None,
                    thrower: None,
                }
                .world_scatter(DimId::Overworld);
                drops().lock().unwrap_or_else(|e| e.into_inner()).push(d);
            }
            return PlayPacketControl::Skip;
        }
    }
    if held_it == 908 && !is_merchant {
        // bucket on cow/sheep -> milk
        let milkable = {
            let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
            ms.iter()
                .any(|m| m.eid == target && [22i64, 77].contains(&m.etype))
        };
        if milkable {
            let k = held_player_menu_slot(held_slot);
            let c = {
                let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                let c = inv.get(&k).map(|v| v.1).unwrap_or(0);
                if c <= 1 {
                    inv.insert(k, (914, 1, Vec::new()));
                } else {
                    inv.insert(k, (908, c - 1, Vec::new()));
                }
                c
            };
            let hand_stack = me
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&k)
                .cloned()
                .unwrap_or((0, 0, Vec::new()));
            let mut p = vec![0u8];
            write_varint(&mut p, i64::from(inv_state_next(me)));
            p.extend_from_slice(&k.to_be_bytes());
            p.extend_from_slice(&slot_wire(&hand_stack));
            let _ = send_packet(
                &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                0x15,
                p,
            );
            if c > 1 {
                player_give_or_drop_dim(me, (914, 1, Vec::new()));
            }
            return PlayPacketControl::Skip;
        }
    }
    if matches!(held_it, 853 | 854 | 1097) && !is_merchant {
        // Keep the legacy wheat interaction and accept the GP-1 temptation items.
        let mut loved = false;
        {
            let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(m) = ms.iter().find(|m| {
                m.eid == target
                    && match m.etype {
                        19 => matches!(held_it, 853 | 854),
                        77 => matches!(held_it, 854 | 1097),
                        22 | 87 => held_it == 854,
                        _ => false,
                    }
            }) {
                let mut love = m.love.lock().unwrap_or_else(|e| e.into_inner());
                // `AnimalMob.canFallInLove`: an animal already
                // in love (and vanilla's breed/age cooldowns,
                // which this live model represents by that same
                // timer) passes the interaction before the item
                // is consumed. This also makes the client's
                // INTERACT_AT + INTERACT pair idempotent.
                if *love == 0 {
                    *love = 80;
                    loved = true;
                }
            }
        }
        if loved {
            if *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
                let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                let k = held_player_menu_slot(held_slot);
                if let Some((it, c, _kc)) = inv.get(&k).cloned() {
                    if c > 1 {
                        inv.insert(k, (it, c - 1, _kc));
                    } else {
                        inv.remove(&k);
                    }
                }
                let (it, c, _cc) = inv.get(&k).cloned().unwrap_or((0, 0, Vec::new()));
                drop(inv);
                let mut p = vec![0u8];
                write_varint(&mut p, i64::from(inv_state_next(me)));
                p.extend_from_slice(&k.to_be_bytes());
                p.extend_from_slice(&slot_wire(&(it, c, _cc)));
                let _ = send_packet(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    0x15,
                    p,
                );
            }
            return PlayPacketControl::Skip;
        }
    }
    if let Some(merchant) = merchant {
        open_live_merchant(me, &merchant);
        return PlayPacketControl::Skip;
    }
    PlayPacketControl::Handled
}

fn play_c2s_use_entity_attack(me: &Arc<Client>, target: i32, held_slot: i16) -> PlayPacketControl {
    // attack
    // `LeashFenceKnotEntity.hurt`: the knot breaks and every
    // tether it anchored detaches with its lead.
    if break_leash_knot(target) {
        return PlayPacketControl::Skip;
    }
    // `Player.attack` on a large fireball: melee contact does
    // no damage; it reflects the projectile along the player's
    // look vector and transfers ownership to that player.
    let reflected_fireball = {
        let mut projectiles = live_mob_projectiles()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        projectiles.iter_mut().find_map(|projectile| {
            if projectile.eid != target || projectile.kind != MobProjectileKind::GhastFireball {
                return None;
            }
            let speed = mob_behavior_dot(projectile.velocity, projectile.velocity)
                .sqrt()
                .max(0.5);
            let (_, _, _, yaw, pitch, _) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
            let yr = f64::from(yaw).to_radians();
            let pr = f64::from(pitch).to_radians();
            projectile.velocity = [
                -yr.sin() * pr.cos() * speed,
                -pr.sin() * speed,
                yr.cos() * pr.cos() * speed,
            ];
            projectile.owner = me.eid;
            Some(projectile.velocity)
        })
    };
    if let Some([vx, vy, vz]) = reflected_fireball {
        let packet = velocity_pkt(
            target,
            (vx * 8000.0) as i16,
            (vy * 8000.0) as i16,
            (vz * 8000.0) as i16,
        );
        broadcast_seen_packet(target, 0x5a, &packet);
        return PlayPacketControl::Skip;
    }
    // PvP: target may be another player
    if target != me.eid {
        let gm = *me.gm.lock().unwrap_or_else(|e| e.into_inner());
        if gm != 3 && break_generated_cart(target, gm != 1) {
            return PlayPacketControl::Skip;
        }
        let mut found: Option<(i32, String, f32, f32)> = None;
        {
            // LOCK ORDER: snapshot, never hold clients() while
            // broadcasting or touching per-client locks
            // (self-deadlock: broadcast re-locks clients()).
            let vic: Option<Arc<Client>> = {
                clients()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|c| c.eid == target)
                    .cloned()
            };
            if let Some(v) = vic.as_ref() {
                if *v.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1 {
                } else if !live_pvp_enabled() {
                    // `pvp=false`: a player's melee against
                    // another player does nothing, the same
                    // way Vanilla's `Player.attack` returns
                    // early on `!isPvpAllowed`.
                    return PlayPacketControl::Skip;
                } else {
                    // vanilla: attack_damage attribute (fist=1) + held item bonus + sharpness
                    let held = {
                        let i2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                        i2.get(&held_player_menu_slot(held_slot))
                            .cloned()
                            .unwrap_or((0, 0, Vec::new()))
                    };
                    let band = if (818..=847).contains(&held.0) {
                        (held.0 - 818) / 5
                    } else {
                        -1
                    };
                    let kind = if (818..=847).contains(&held.0) {
                        (held.0 - 818) % 5
                    } else {
                        -1
                    };
                    // vanilla tool attack damages by kind (sword,shovel,pick,axe,hoe)
                    // bands: wood,stone,gold,iron,diamond,netherite
                    let wep = match kind {
                        0 => [4.0, 5.0, 4.0, 6.0, 7.0, 8.0],  // swords
                        1 => [2.5, 3.5, 2.5, 4.5, 5.5, 6.5],  // shovels
                        2 => [2.0, 3.0, 2.0, 4.0, 5.0, 6.0],  // pickaxes
                        3 => [7.0, 9.0, 7.0, 9.0, 9.0, 10.0], // axes
                        4 => [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],  // hoes
                        _ => [1.0; 6],
                    }[band.clamp(0, 5) as usize];
                    let wep = if held.0 == 1188 {
                        9.0
                    } else if held.0 == 1093 {
                        5.0
                    } else {
                        wep
                    };
                    let sharp = enchant_level(&held.2, 32) as f32 * 0.5;
                    // attr default 1 = fist; an attribute-set value adds on top
                    let chg = melee_charge(me, held.0);
                    let mace_fall = mace_attack_fall(me, held.0);
                    let smash_bonus =
                        mace_smash_damage_bonus(mace_fall, enchant_level(&held.2, 6) as i32);
                    let crt = {
                        let (_px, py, _pz, _, _, g) =
                            *me.pos.lock().unwrap_or_else(|e| e.into_inner());
                        let ff = *me.fall_from.lock().unwrap_or_else(|e| e.into_inner());
                        !g && py < ff - 0.1
                    };
                    let (strb, wkb) = {
                        let ef = me.effs.lock().unwrap_or_else(|e| e.into_inner());
                        (
                            ef.iter()
                                .find(|e| e.id == 4)
                                .map(|e| e.amp + 1)
                                .unwrap_or(0) as f32
                                * 3.0,
                            ef.iter()
                                .find(|e| e.id == 17)
                                .map(|e| e.amp + 1)
                                .unwrap_or(0) as f32
                                * -4.0,
                        )
                    };
                    let dmg = ((attr_of(&format!("p:{}", me.name), "generic.attack_damage")
                        as f32
                        - 1.0)
                        .max(0.0)
                        + wep
                        + sharp
                        + strb
                        + wkb)
                        .max(0.0)
                        * (0.2 + 0.8 * chg * chg);
                    let dmg = (dmg + smash_bonus) * if crt { 1.5 } else { 1.0 };
                    let Some((accepted, starts_hurt_window)) = damage_after_invulnerability(v, dmg)
                    else {
                        return PlayPacketControl::Skip;
                    };
                    let dmg = accepted;
                    // vanilla `CombatTracker.recordDamage`:
                    // opens or refreshes the victim's combat
                    // window (`0x3b` on the first hit).
                    combat_note_damage(v);
                    *me.walk_acc.lock().unwrap_or_else(|e| e.into_inner()) += 0.1;
                    // vanilla: an axe hit disables a raised shield for 5s
                    if kind == 3 && *v.blocking.lock().unwrap_or_else(|e| e.into_inner()) {
                        *v.blocking.lock().unwrap_or_else(|e| e.into_inner()) = false;
                        *v.shield_cd.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                    }
                    let vb = kind != 3
                        && *v.blocking.lock().unwrap_or_else(|e| e.into_inner())
                        && v.shield_cd
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .elapsed()
                            > Duration::from_secs(5);
                    if vb {
                        damage_shield(v, dmg);
                    }
                    let mut h = v.hp.lock().unwrap_or_else(|e| e.into_inner());
                    *h = (*h
                        - if vb {
                            0.0
                        } else {
                            hurt_absorb(v, dmg_after_breach(v, dmg, 0, enchant_level(&held.2, 4)))
                        })
                    .max(0.0);
                    if *h <= 0.0 {
                        drop(h);
                        totem_check(v);
                        h = v.hp.lock().unwrap_or_else(|e| e.into_inner());
                    }
                    let hh = *h;
                    drop(h);
                    if !vb {
                        damage_armor(v, dmg);
                        if dmg > 0.0 {
                            let (x, y, z, ..) = *v.pos.lock().unwrap_or_else(|e| e.into_inner());
                            queue_mace_smash(me, &held, mace_fall, v.eid, [x, y, z]);
                        }
                    }
                    if damage_held(me) {
                        let (pxx, pyy, pzz, ..) = {
                            let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                            *p
                        };
                        let snd = sound_pkt_named("entity.item.break", pxx, pyy, pzz);
                        broadcast_near_packet(
                            *me.dim.lock().unwrap_or_else(|e| e.into_inner()),
                            pxx,
                            pyy,
                            pzz,
                            -1,
                            0x68,
                            &snd,
                        );
                    }
                    held_slot_resync(me);
                    if starts_hurt_window && !vb && dmg > 0.0 {
                        let attacker_dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
                        let victim_dim = *v.dim.lock().unwrap_or_else(|e| e.into_inner());
                        if attacker_dim == victim_dim {
                            broadcast_player_attack_damage(v.eid, me.eid, victim_dim);
                        }
                    }
                    let (pp, vp) = (
                        {
                            let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                            *p
                        },
                        {
                            let p = v.pos.lock().unwrap_or_else(|e| e.into_inner());
                            *p
                        },
                    );
                    // vanilla LivingEntity.hurt → indicateDamage:
                    // hurtDir = atan2(dz, dx) * 57.2957763671875 - victimYaw
                    // where (dx, dz) is attacker minus victim position.
                    let hurt_yaw = {
                        let dx = pp.0 - vp.0;
                        let dz = pp.2 - vp.2;
                        (dz.atan2(dx) * 57.2957763671875 - f64::from(vp.3)) as f32
                    };
                    let hurt_pkt = hurt_anim_yaw(v.eid, hurt_yaw);
                    if let Ok(mut w) = v.sock.lock() {
                        let _ = send_packet(&mut w, 0x24, hurt_pkt.clone());
                        let _ = send_packet(&mut w, 0x5d, health_pkt_c(hh, v));
                    }
                    // vanilla LivingEntity.knockback(0.4, dx, dz): the
                    // strength is scaled by (1 - knockback_resistance),
                    // the direction is the normalized attacker-minus-
                    // victim offset, and SetEntityMotion clamps each
                    // axis to +-3.9 and truncates velocity * 8000.
                    let kb = vanilla_knockback(&vp, &pp);
                    let vel = velocity_pkt(v.eid, kb.0, kb.1, kb.2);
                    if let Ok(mut w) = v.sock.lock() {
                        let _ = send_packet(&mut w, 0x5a, vel.clone());
                    }
                    found = Some((v.eid, v.name.clone(), hh, hurt_yaw));
                }
            }
        }
        if let Some((veid, vname, hh, hurt_yaw)) = found {
            let hurt_pkt = hurt_anim_yaw(veid, hurt_yaw);
            broadcast_seen_ex_packet(veid, veid, 0x24, &hurt_pkt);
            if hh <= 0.0 {
                let victim = {
                    clients()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .find(|c| c.eid == veid)
                        .cloned()
                };
                if let Some(victim) = &victim {
                    drop_player_items_on_death(victim);
                }
                if GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) != 0 {
                    // Vanilla `player_attack`: the `.item`
                    // variant only when the killer's held
                    // item carries a custom name.
                    let weapon = {
                        let i2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                        i2.get(&held_player_menu_slot(held_slot))
                            .and_then(|st| renamed_custom_name(&st.2))
                    };
                    let message = format_vanilla_death_message(
                        &vname,
                        "player_attack",
                        Some(me.name.as_str()),
                        weapon.as_deref(),
                    );
                    let line = system_chat(&message);
                    broadcast_packet(-1, 0x6c, &line);
                    // vanilla `ServerPlayer.die`: under
                    // showDeathMessages the combat tracker's
                    // death message also rides
                    // `player_combat_kill` to the dying client.
                    if let Some(victim) = &victim {
                        let _ = send_packet(
                            &mut victim.sock.lock().unwrap_or_else(|e| e.into_inner()),
                            0x3c,
                            encode_play_player_combat_kill(veid, &message).1,
                        );
                    }
                }
            }
            return PlayPacketControl::Skip;
        }
    }
    let mut dead: Vec<(i32, i64, f64, f64, f64)> = Vec::new();
    let mut swept: Vec<i32> = Vec::new();
    {
        let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = ms.iter().find(|m| m.eid == target) {
            if m.dim != *me.dim.lock().unwrap_or_else(|e| e.into_inner()) {
                return PlayPacketControl::Skip;
            }
            // vanilla hurtTime: 0.5s of invulnerability after a hit
            let fresh = m
                .hurt_at
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .elapsed()
                >= Duration::from_millis(500);
            if fresh {
                if !live_mob_hit_allowed(m, me.eid, BossHit::Other) {
                    return PlayPacketControl::Skip;
                }
                *m.hurt_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            }
            if fresh {
                *me.walk_acc.lock().unwrap_or_else(|e| e.into_inner()) += 0.1;
                let held = {
                    let i2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                    i2.get(&held_player_menu_slot(held_slot))
                        .cloned()
                        .unwrap_or((0, 0, Vec::new()))
                };
                // vanilla melee: weapon total by tool kind; sharpness adds 0.5*lvl
                let band = if (818..=847).contains(&held.0) {
                    (held.0 - 818) / 5
                } else {
                    -1
                };
                let kind = if (818..=847).contains(&held.0) {
                    (held.0 - 818) % 5
                } else {
                    -1
                };
                let wep = match kind {
                    0 => [4.0, 5.0, 4.0, 6.0, 7.0, 8.0],
                    1 => [2.5, 3.5, 2.5, 4.5, 5.5, 6.5],
                    2 => [2.0, 3.0, 2.0, 4.0, 5.0, 6.0],
                    3 => [7.0, 9.0, 7.0, 9.0, 9.0, 10.0],
                    4 => [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
                    _ => [1.0; 6],
                }[band.clamp(0, 5) as usize];
                let wep = if held.0 == 1188 {
                    9.0
                } else if held.0 == 1093 {
                    5.0
                } else {
                    wep
                };
                let sharp = enchant_level(&held.2, 32) as f32 * 0.5;
                // smite (+2.5/lvl vs undead) / bane (+2.5/lvl vs arthropods)
                let undead = enchantment_targets_undead(m.etype);
                let arthro = enchantment_targets_arthropod(m.etype);
                let spec = if undead {
                    enchant_level(&held.2, 34)
                } else if arthro {
                    enchant_level(&held.2, 1)
                } else {
                    0
                } as f32
                    * 2.5;
                // vanilla attack cooldown: charge<1 scales damage 0.2..1.0
                let charge = melee_charge(me, held.0);
                let scale = 0.2 + 0.8 * charge * charge;
                let mace_fall = mace_attack_fall(me, held.0);
                let smash_bonus =
                    mace_smash_damage_bonus(mace_fall, enchant_level(&held.2, 6) as i32);
                // Vanilla criticals require a charged attack and exclude
                // sprinting, water, climbable blocks, and blindness.
                let (px, py, pz, _, _, grounded) =
                    *me.pos.lock().unwrap_or_else(|e| e.into_inner());
                let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
                let (bx, by, bz) = (px.floor() as i32, py.floor() as i32, pz.floor() as i32);
                let reg = crate::block_registry::breg();
                let in_water = reg.is_some_and(|r| {
                    r.has_water(block_at_dim(dim, bx, by, bz))
                        || r.has_water(block_at_dim(dim, bx, by + 1, bz))
                });
                let climbing = reg.is_some_and(|r| {
                    [
                        block_at_dim(dim, bx, by, bz),
                        block_at_dim(dim, bx, by + 1, bz),
                    ]
                    .into_iter()
                    .any(|state| {
                        matches!(
                            r.name_of(state),
                            "minecraft:ladder"
                                | "minecraft:vine"
                                | "minecraft:scaffolding"
                                | "minecraft:weeping_vines"
                                | "minecraft:weeping_vines_plant"
                                | "minecraft:twisting_vines"
                                | "minecraft:twisting_vines_plant"
                                | "minecraft:cave_vines"
                                | "minecraft:cave_vines_plant"
                        )
                    })
                });
                let blind = me
                    .effs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .any(|effect| effect.id == 14);
                let ff = *me.fall_from.lock().unwrap_or_else(|e| e.into_inner());
                let crit = melee_critical_allowed(
                    charge,
                    grounded,
                    py < ff - 0.1,
                    *me.fall_fly.lock().unwrap_or_else(|e| e.into_inner()),
                    *me.sprinting.lock().unwrap_or_else(|e| e.into_inner()),
                    in_water,
                    climbing,
                    blind,
                );
                let (strb, wkb) = {
                    let ef = me.effs.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        ef.iter()
                            .find(|e| e.id == 4)
                            .map(|e| e.amp + 1)
                            .unwrap_or(0) as f32
                            * 3.0,
                        ef.iter()
                            .find(|e| e.id == 17)
                            .map(|e| e.amp + 1)
                            .unwrap_or(0) as f32
                            * -4.0,
                    )
                };
                // Registered multi-part layouts (mod
                // colossals): the melee ray is tested
                // against the part AABBs and the struck
                // part's damage multiplier scales the hit —
                // `1.0` for every ordinary mob.
                let (mx2, my2, mz2, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
                let (_ew, eh2) = entity_dimensions_1211(m.etype);
                let dir = [
                    mx2 - px,
                    (my2 + f64::from(eh2) / 2.0) - (py + 1.62),
                    mz2 - pz,
                ];
                let ray_len = (dir[0].powi(2) + dir[1].powi(2) + dir[2].powi(2)).sqrt();
                let colossal_scale = if ray_len > 1e-6 {
                    live_colossal_attack_scale(
                        [px, py + 1.62, pz],
                        [dir[0] / ray_len, dir[1] / ray_len, dir[2] / ray_len],
                        ray_len + 1.0,
                        m,
                    )
                } else {
                    1.0
                };
                let mut h = m.hp.lock().unwrap_or_else(|e| e.into_inner());
                let damage = ((wep + sharp + spec + strb + wkb).max(0.0) * scale + smash_bonus)
                    * colossal_scale
                    * if crit { 1.5 } else { 1.0 };
                if *h <= 0.0 || damage <= 0.0 {
                    return PlayPacketControl::Skip;
                }
                *h -= damage;
                drop(h);
                queue_mace_smash(me, &held, mace_fall, m.eid, [mx2, my2, mz2]);
                // Vanilla swords sweep even without Sweeping Edge.
                if kind == 0 {
                    let (mx, my, mz, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
                    let (width, height) = entity_dimensions_1211(m.etype);
                    let target_box = PlayerAabb {
                        min: [mx - f64::from(width) / 2.0, my, mz - f64::from(width) / 2.0],
                        max: [
                            mx + f64::from(width) / 2.0,
                            my + f64::from(height),
                            mz + f64::from(width) / 2.0,
                        ],
                    };
                    for other in ms
                        .iter()
                        .filter(|other| other.eid != m.eid && other.dim == dim)
                    {
                        let (ox, oy, oz, ..) = *other.pos.lock().unwrap_or_else(|e| e.into_inner());
                        let (ow, oh) = entity_dimensions_1211(other.etype);
                        let candidate = PlayerAabb {
                            min: [ox - f64::from(ow) / 2.0, oy, oz - f64::from(ow) / 2.0],
                            max: [
                                ox + f64::from(ow) / 2.0,
                                oy + f64::from(oh),
                                oz + f64::from(ow) / 2.0,
                            ],
                        };
                        let sweep = sweeping_attack(
                            charge,
                            grounded,
                            *me.sprinting.lock().unwrap_or_else(|e| e.into_inner()),
                            crit,
                            [px, py, pz],
                            target_box,
                            candidate,
                            (wep + sharp + strb + wkb).max(0.0),
                            enchant_level(&held.2, 36) as i32,
                        );
                        if !sweep.eligible || !live_mob_hit_allowed(other, me.eid, BossHit::Other) {
                            continue;
                        }
                        let mut hurt_at = other.hurt_at.lock().unwrap_or_else(|e| e.into_inner());
                        if hurt_at.elapsed() < Duration::from_millis(500) {
                            continue;
                        }
                        *hurt_at = Instant::now();
                        drop(hurt_at);
                        let mut health = other.hp.lock().unwrap_or_else(|e| e.into_inner());
                        if *health <= 0.0 {
                            continue;
                        }
                        *health -= sweep.damage;
                        if *health <= 0.0 {
                            dead.push((other.eid, other.etype, ox, oy, oz));
                        }
                        drop(health);
                        swept.push(other.eid);
                    }
                }
                // Fire Aspect adds four seconds of independent burn time per level.
                let fa = enchant_level(&held.2, 10);
                if fa > 0 && !mob_fire_immune(m.etype) {
                    *m.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) =
                        fire_aspect_duration_ticks(fa);
                }
                if damage_held(me) {
                    let (pxx, pyy, pzz, ..) = {
                        let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                        *p
                    };
                    let snd = sound_pkt_named("entity.item.break", pxx, pyy, pzz);
                    broadcast_near_packet(
                        *me.dim.lock().unwrap_or_else(|e| e.into_inner()),
                        pxx,
                        pyy,
                        pzz,
                        -1,
                        0x68,
                        &snd,
                    );
                }
                held_slot_resync(me);
                let h = m.hp.lock().unwrap_or_else(|e| e.into_inner());
                if *h <= 0.0 {
                    let (mx, my, mz, ..) = {
                        let p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
                        *p
                    };
                    dead.push((m.eid, m.etype, mx, my, mz));
                }
                let ha = hurt_anim(m.eid);
                let _ = send_packet(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    0x24,
                    ha.clone(),
                );
                broadcast_seen_ex_packet(me.eid, me.eid, 0x24, &ha);
                // knockback away from attacker
                let (pp, mp) = (
                    {
                        let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
                        *p
                    },
                    {
                        let p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
                        *p
                    },
                );
                let (mut kx, mut kz) = (mp.0 - pp.0, mp.2 - pp.2);
                let kl = (kx * kx + kz * kz).sqrt().max(0.01);
                kx /= kl;
                kz /= kl;
                let vp = velocity_pkt(m.eid, (kx * 3200.0) as i16, 2800, (kz * 3200.0) as i16);
                broadcast_seen_packet(m.eid, 0x5a, &vp);
                // vanilla: sprint attacks add knockback and reset the sprint
                let sprint_hit = *me.sprinting.lock().unwrap_or_else(|e| e.into_inner());
                if sprint_hit {
                    *me.sprinting.lock().unwrap_or_else(|e| e.into_inner()) = false;
                }
                let kbs = 0.6
                    + 0.6 * enchant_level(&held.2, 17) as f64
                    + if sprint_hit { 0.5 } else { 0.0 };
                let (nx, nz) = (mp.0 + kx * kbs, mp.2 + kz * kbs);
                {
                    let mut p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
                    p.0 = nx;
                    p.2 = nz;
                }
                let tp = entity_teleport(m.eid, nx, mp.1, nz, mp.3, mp.4, true);
                broadcast_seen_packet(m.eid, 0x70, &tp);
            }
        }
    }
    for target_eid in swept {
        broadcast_seen(target_eid, |_c| (0x24, hurt_anim(target_eid)));
    }
    for (deid, etype, mx, my, mz) in dead {
        adv_event(&me.name, "player_killed_entity", Some(etype));
        stat_increment(me, StatCategory::Killed, etype as i32, 1);
        let mn = match etype {
            124 => "Zombie",
            22 => "Cow",
            23 => "Creeper",
            91 => "Skeleton",
            19 => "Chicken",
            77 => "Pig",
            87 => "Sheep",
            100 => "Spider",
            _ => "mob",
        };
        if GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) != 0 {
            let line = system_chat(&format!("{} was slain by {}", mn, me.name));
            broadcast_packet(-1, 0x6c, &line);
        }
        let Some((mob, state)) = remove_live_mob(deid) else {
            continue;
        };
        let burn_kill = *mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) > 0;
        live_mob_death(&mob, state, [mx, my, mz]);
        broadcast_seen_destroy(deid);
        let was_sheared = sheared()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&deid);
        // A dead vehicle drops its riders, a dead rider leaves
        // its seat, and the mob's equipment record dies with it.
        live_mount_forget(deid);
        mob_equipment()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&deid);
        // looting: +0..level extra count per drop
        let held2 = {
            let i2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
            i2.get(&held_player_menu_slot(held_slot))
                .cloned()
                .unwrap_or((0, 0, Vec::new()))
        };
        let loot = enchant_level(&held2.2, 18).clamp(0, 3) as u64;
        for (item, cnt, comps) in mob_loot(etype, burn_kill)
            .into_iter()
            .filter(|st| !(was_sheared && st.0 == 202))
            .filter(|_| GR_MOB_LOOT.load(Ordering::Relaxed) == 1)
        {
            let extra = if loot > 0 {
                (world_rng::next_u64() % (loot + 1)) as u8
            } else {
                0
            };
            let d = Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: mx,
                y: my,
                z: mz,
                item,
                cnt: cnt + extra,
                comps,
                age: 0,
                target: None,
                thrower: None,
            }
            .world_scatter(mob.dim);
            drops().lock().unwrap_or_else(|e| e.into_inner()).push(d);
        }
        xp_spawn_dim(mob.dim, mx, my, mz, mob_xp(etype));
        // spawn streams via interest_tick
    }
    PlayPacketControl::Handled
}

fn play_c2s_use_entity(cx: &mut PlayPacketContext<'_>, b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let held_slot = *cx.held_slot;
    // use_entity: attack mobs
    let mut sl = b;
    let target = read_varint(&mut sl).unwrap_or(-1) as i32;
    let mouse = read_varint(&mut sl).unwrap_or(-1);
    // vanilla entity_interaction_range = 3.0; gate all attack/interact
    // uses so distant entities can't be hit/sheared/bred cross-map.
    {
        let (px, py, pz, ..) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
        let vt: Option<Arc<Client>> = {
            clients()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|c| c.eid == target)
                .cloned()
        };
        let tp = vt
            .map(|v| {
                let p = *v.pos.lock().unwrap_or_else(|e| e.into_inner());
                (p.0, p.1, p.2, false)
            })
            .or_else(|| {
                let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
                ms.iter().find(|m| m.eid == target).map(|m| {
                    let p = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
                    (p.0, p.1, p.2, false)
                })
            })
            .or_else(|| {
                let gen = generated_content()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                gen.carts
                    .iter()
                    .find(|c| c.eid == target)
                    .and_then(|c| c.pos().ok())
                    .map(|p| (p[0], p[1], p[2], true))
            })
            .or_else(|| {
                leash_knots()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&target)
                    .map(|&(_, p)| (p.0 as f64 + 0.5, p.1 as f64 + 0.5, p.2 as f64 + 0.5, false))
            });
        let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
        let eye_y = client_eye_y(&me, dim, px, py, pz);
        let ok = tp
            .map(|(tx, ty, tz, is_cart)| {
                let center_y = ty + if is_cart { 0.35 } else { 0.9 };
                let (dx, dy, dz) = (tx - px, center_y - eye_y, tz - pz);
                dx * dx + dy * dy + dz * dz <= 3.2 * 3.2
            })
            .unwrap_or(false);
        if !ok {
            return PlayPacketControl::Skip;
        }
        if mouse == 1 {
            if let Some((tx, ty, tz, is_cart)) = tp {
                let target_center = [tx, ty + if is_cart { 0.35 } else { 0.9 }, tz];
                if !ac_check_attack(&me, dim, target_center, target) {
                    return PlayPacketControl::Skip;
                }
                // Persistent claims: damaging an entity inside a
                // claim needs the entity-attack permission.
                if !can_player_damage_entity(
                    dim,
                    tx.floor() as i32,
                    tz.floor() as i32,
                    &uuid_text(&me.uuid),
                    &me.name,
                    client_permission_level(&me) >= 4,
                ) {
                    return PlayPacketControl::Skip;
                }
            }
        }
        if mouse == 0 || mouse == 2 {
            if let Some((tx, _ty, tz, _is_cart)) = tp {
                // Persistent claims: interacting with an entity
                // inside a claim needs the entity-interact
                // permission.
                if !can_player_interact_entity(
                    *me.dim.lock().unwrap_or_else(|e| e.into_inner()),
                    tx.floor() as i32,
                    tz.floor() as i32,
                    &uuid_text(&me.uuid),
                    &me.name,
                    client_permission_level(&me) >= 4,
                ) {
                    return PlayPacketControl::Skip;
                }
            }
        }
    }
    if mouse == 0 || mouse == 2 {
        return play_c2s_use_entity_interact(&me, target, held_slot);
    }
    if mouse == 1 {
        return play_c2s_use_entity_attack(&me, target, held_slot);
    }
    PlayPacketControl::Handled
}
