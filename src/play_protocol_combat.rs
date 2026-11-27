fn play_c2s_arm_animation(cx: &mut PlayPacketContext<'_>, mut b: &[u8]) -> PlayPacketControl {
    let me = Arc::clone(cx.me);
    let animation = match read_varint(&mut b) {
        Some(0) => 0,
        Some(1) => 3,
        _ => return PlayPacketControl::Skip,
    };
    broadcast_seen(me.eid, |_c| {
        (0x03, {
            let mut p = Vec::new();
            write_varint(&mut p, me.eid as i64);
            p.push(animation);
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
    hand_slot: i16,
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
    if leash_name_tag_interact(me, target, hand_slot) {
        return PlayPacketControl::Skip;
    }
    if pet_collar_or_parrot_interact(me, target, hand_slot) {
        return PlayPacketControl::Skip;
    }
    // `IronGolem`/`SnowGolem`/`Allay` `mobInteract` — ingot
    // repair, pumpkin shear, and the allay's sample give/take
    // ride the same lane as the pet paths.
    if utility_mob_interact(me, target, hand_slot) {
        return PlayPacketControl::Skip;
    }
    if live_mob_interact(me, target, hand_slot) {
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
        .get(&hand_slot)
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
            let k = hand_slot;
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
                let k = hand_slot;
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

struct MeleeAttackCalc {
    kind: i64,
    charge: f32,
    critical: bool,
    base: f32,
    enchant_bonus: f32,
    mace_fall: f32,
    smash_bonus: f32,
}

fn melee_effect_attack_bonus(me: &Client) -> f32 {
    let ef = lk(&me.effs);
    let strength = ef
        .iter()
        .find(|e| e.id == 4)
        .map(|e| e.amp + 1)
        .unwrap_or(0) as f32
        * 3.0;
    let weakness = ef
        .iter()
        .find(|e| e.id == 17)
        .map(|e| e.amp + 1)
        .unwrap_or(0) as f32
        * -4.0;
    strength + weakness
}

fn player_melee_critical(me: &Client, charge: f32) -> bool {
    let (px, py, pz, _, _, grounded) = *lk(&me.pos);
    let dim = *lk(&me.dim);
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
    let blind = lk(&me.effs).iter().any(|effect| effect.id == 14);
    let fall_from = *lk(&me.fall_from);
    melee_critical_allowed(
        charge,
        grounded,
        py < fall_from,
        *lk(&me.fall_fly),
        *lk(&me.sprinting),
        in_water,
        climbing,
        blind,
    ) && vehicle_of(me.eid).is_none()
}

fn melee_attack_calc(me: &Arc<Client>, held: &Stack, target_etype: Option<i64>) -> MeleeAttackCalc {
    let kind = vanilla_tool_kind(held.0);
    let charge = melee_charge(me, held.0);
    let mace_fall = mace_attack_fall(me, held.0);
    let smash_bonus = mace_smash_damage_bonus(mace_fall, enchant_level(&held.2, 6) as i32);
    let sharp = vanilla_sharpness_damage(enchant_level(&held.2, 32));
    let typed = target_etype.map_or(0.0, |etype| {
        if enchantment_targets_undead(etype) {
            enchant_level(&held.2, 34) as f32 * 2.5
        } else if enchantment_targets_arthropod(etype) {
            enchant_level(&held.2, 1) as f32 * 2.5
        } else {
            0.0
        }
    });
    let attribute = attr_of(&format!("p:{}", me.name), "generic.attack_damage") as f32;
    // An explicit `attribute_modifiers` component replaces the item's vanilla
    // default modifier; otherwise the table supplies the item contribution.
    let item_contribution = if comp_get(&held.2, COMP_ATTRIBUTE_MODIFIERS).is_some() {
        0.0
    } else {
        vanilla_weapon_attack_damage(held.0) - 1.0
    };
    let base = (attribute + item_contribution + melee_effect_attack_bonus(me)).max(0.0);
    MeleeAttackCalc {
        kind,
        charge,
        critical: player_melee_critical(me, charge),
        base,
        enchant_bonus: sharp + typed,
        mace_fall,
        smash_bonus,
    }
}

fn player_combat_box(x: f64, y: f64, z: f64) -> PlayerAabb {
    PlayerAabb {
        min: [x - 0.3, y, z - 0.3],
        max: [x + 0.3, y + 1.8, z + 0.3],
    }
}

struct MeleeSweepEnv<'a> {
    calc: &'a MeleeAttackCalc,
    held: &'a Stack,
    attacker_pos: [f64; 3],
    on_ground: bool,
    sprinting: bool,
    target_box: PlayerAabb,
}

/// Vanilla's post-charge sweep damage: Sweeping Edge scales the already
/// charge-scaled base through `generic.sweeping_damage_ratio`; target enchant
/// damage is added and the total is scaled by the bare attack charge.
fn melee_sweep_damage(
    env: &MeleeSweepEnv<'_>,
    candidate_box: PlayerAabb,
    target_etype: Option<i64>,
) -> Option<f32> {
    let calc = env.calc;
    if !sweeping_attack_gate(calc.charge, env.on_ground, env.sprinting, calc.critical) {
        return None;
    }
    let scaled_base = calc.base * (0.2 + 0.8 * calc.charge * calc.charge) + calc.smash_bonus;
    let probe = sweeping_attack(
        calc.charge,
        env.on_ground,
        env.sprinting,
        calc.critical,
        env.attacker_pos,
        env.target_box,
        candidate_box,
        scaled_base,
        enchant_level(&env.held.2, 36) as i32,
    );
    if !probe.eligible {
        return None;
    }
    let sharp = vanilla_sharpness_damage(enchant_level(&env.held.2, 32));
    let typed = target_etype.map_or(0.0, |etype| {
        if enchantment_targets_undead(etype) {
            enchant_level(&env.held.2, 34) as f32 * 2.5
        } else if enchantment_targets_arthropod(etype) {
            enchant_level(&env.held.2, 1) as f32 * 2.5
        } else {
            0.0
        }
    });
    Some((probe.damage + sharp + typed) * calc.charge)
}

fn sweep_mobs_for_melee(
    me: &Arc<Client>,
    env: &MeleeSweepEnv<'_>,
    dim: DimId,
    primary_eid: i32,
    mobs: &[Arc<Mob>],
    dead: &mut Vec<(i32, i64, f64, f64, f64)>,
    swept: &mut Vec<i32>,
) {
    for other in mobs
        .iter()
        .filter(|other| other.eid != primary_eid && other.dim == dim)
    {
        let (ox, oy, oz, ..) = *other.pos.lock().unwrap_or_else(|e| e.into_inner());
        let (ow, oh) = entity_dimensions_1211(other.etype);
        let candidate = PlayerAabb {
            min: [ox - f64::from(ow) / 2.0, oy, oz - f64::from(ow) / 2.0],
            max: [ox + f64::from(ow) / 2.0, oy + f64::from(oh), oz + f64::from(ow) / 2.0],
        };
        let Some(raw) = melee_sweep_damage(env, candidate, Some(other.etype)) else {
            continue;
        };
        if !live_mob_hit_allowed(other, me.eid, BossHit::Other) {
            continue;
        }
        let Some(accepted) = mob_melee_hurt_gate(other, raw) else {
            continue;
        };
        let damage = mob_melee_mitigate(other, accepted, 0);
        let mut health = other.hp.lock().unwrap_or_else(|e| e.into_inner());
        if *health <= 0.0 {
            continue;
        }
        *health -= damage;
        if *health <= 0.0 {
            dead.push((other.eid, other.etype, ox, oy, oz));
        }
        drop(health);
        swept.push(other.eid);
    }
}

fn sweep_players_for_melee(
    me: &Arc<Client>,
    env: &MeleeSweepEnv<'_>,
    dim: DimId,
    primary_eid: i32,
) {
    if !live_pvp_enabled() {
        return;
    }
    for other in client_snapshot() {
        if other.eid == me.eid
            || other.eid == primary_eid
            || *lk(&other.dim) != dim
            || !matches!(*lk(&other.gm), 0 | 2)
        {
            continue;
        }
        let (ox, oy, oz, ..) = *lk(&other.pos);
        let Some(raw) = melee_sweep_damage(env, player_combat_box(ox, oy, oz), None) else {
            continue;
        };
        let shield = player_melee_shield_block(me, &other, env.calc, env.held, raw);
        if shield.blocked {
            damage_shield_attempts(&other, shield.durability_attempts);
            continue;
        }
        let Some((accepted, _)) = damage_after_invulnerability(&other, raw) else {
            continue;
        };
        let source = DamageSource::new("player_attack").with_cause(me.eid);
        player_hurt_apply(&other, accepted, &source, Some(raw));
    }
}

fn player_melee_shield_block(me: &Client, victim: &Arc<Client>, calc: &MeleeAttackCalc, held: &Stack, damage: f32) -> ShieldBlock {
    let miss = ShieldBlock {
        blocked: false,
        durability_attempts: 0,
        disable_ticks: None,
    };
    if !*lk(&victim.blocking) {
        return miss;
    }
    let (ax, ay, az, ..) = *lk(&me.pos);
    let mut ctx = shield_hit_ctx(victim, [ax, ay, az], damage);
    ctx.attacker_can_disable = calc.kind == 3;
    ctx.attacker_is_player = true;
    ctx.attacker_sprinting = *lk(&me.sprinting);
    ctx.attacker_efficiency = enchant_level(&held.2, 8);
    ctx.disable_roll = (world_rng::next_u64() >> 40) as f32 / (1u32 << 24) as f32;
    evaluate_shield_block(&ctx)
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
    let mut dead: Vec<(i32, i64, f64, f64, f64)> = Vec::new();
    let mut swept: Vec<i32> = Vec::new();
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
                let attacker_dim = *lk(&me.dim);
                let victim_dim = *lk(&v.dim);
                if attacker_dim != victim_dim {
                    return PlayPacketControl::Skip;
                }
                if matches!(*v.gm.lock().unwrap_or_else(|e| e.into_inner()), 1 | 3) {
                } else if !live_pvp_enabled() {
                    // `pvp=false`: a player's melee against
                    // another player does nothing, the same
                    // way Vanilla's `Player.attack` returns
                    // early on `!isPvpAllowed`.
                    return PlayPacketControl::Skip;
                } else {
                    // Vanilla `Player.attack`: resolve the weapon and attack_damage
                    // attribute into the base, keep enchantment damage separate,
                    // scale/crit the base only, then add the scaled enchantment bonus.
                    let held = {
                        let i2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                        i2.get(&held_player_menu_slot(held_slot))
                            .cloned()
                            .unwrap_or((0, 0, Vec::new()))
                    };
                    let calc = melee_attack_calc(me, &held, None);
                    let dmg = if calc.smash_bonus == 0.0 {
                        melee_hit_damage(calc.base, calc.enchant_bonus, calc.charge, calc.critical)
                    } else {
                        melee_hit_damage_with_bonus(
                            calc.base,
                            calc.enchant_bonus,
                            calc.charge,
                            calc.critical,
                            calc.smash_bonus,
                        )
                    };
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
                    let shield = player_melee_shield_block(me, v, &calc, &held, dmg);
                    if shield.disable_ticks.is_some() {
                        apply_shield_disable(v);
                    }
                    if shield.blocked {
                        damage_shield_attempts(v, shield.durability_attempts);
                    }
                    let vb = shield.blocked;
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
                            queue_mace_smash(me, &held, calc.mace_fall, v.eid, [x, y, z]);
                        }
                    }
                    // A sword attack that lands on a player uses the same
                    // vanilla sweep as a mob target: nearby mobs and players
                    // inside the inflated victim box take the post-charge hit.
                    if calc.kind == 0 {
                        let (px, py, pz, _, _, grounded) = *lk(&me.pos);
                        let (vx, vy, vz, ..) = *lk(&v.pos);
                        let sprinting = *lk(&me.sprinting);
                        let env = MeleeSweepEnv {
                            calc: &calc,
                            held: &held,
                            attacker_pos: [px, py, pz],
                            on_ground: grounded,
                            sprinting,
                            target_box: player_combat_box(vx, vy, vz),
                        };
                        let mob_snapshot: Vec<Arc<Mob>> = lk(mobs()).iter().cloned().collect();
                        sweep_mobs_for_melee(
                            me,
                            &env,
                            victim_dim,
                            v.eid,
                            &mob_snapshot,
                            &mut dead,
                            &mut swept,
                        );
                        sweep_players_for_melee(me, &env, victim_dim, v.eid);
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
                        broadcast_player_attack_damage(v.eid, me.eid, victim_dim);
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
    {
        let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = ms.iter().find(|m| m.eid == target) {
            if m.dim != *me.dim.lock().unwrap_or_else(|e| e.into_inner()) {
                return PlayPacketControl::Skip;
            }
            let fresh = true;
            if !live_mob_hit_allowed(m, me.eid, BossHit::Other) {
                return PlayPacketControl::Skip;
            }
            if fresh {
                *me.walk_acc.lock().unwrap_or_else(|e| e.into_inner()) += 0.1;
                let held = {
                    let i2 = me.inv.lock().unwrap_or_else(|e| e.into_inner());
                    i2.get(&held_player_menu_slot(held_slot))
                        .cloned()
                        .unwrap_or((0, 0, Vec::new()))
                };
                let calc = melee_attack_calc(me, &held, Some(m.etype));
                let kind = calc.kind;
                let (px, py, pz, _, _, grounded) =
                    *me.pos.lock().unwrap_or_else(|e| e.into_inner());
                let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
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
                let raw_damage = if calc.smash_bonus == 0.0 {
                    melee_hit_damage(calc.base, calc.enchant_bonus, calc.charge, calc.critical)
                } else {
                    melee_hit_damage_with_bonus(
                        calc.base,
                        calc.enchant_bonus,
                        calc.charge,
                        calc.critical,
                        calc.smash_bonus,
                    )
                } * colossal_scale;
                let Some(accepted_damage) = mob_melee_hurt_gate(m, raw_damage) else {
                    return PlayPacketControl::Skip;
                };
                let damage = mob_melee_mitigate(
                    m,
                    accepted_damage,
                    enchant_level(&held.2, 4),
                );
                if *h <= 0.0 || damage <= 0.0 {
                    return PlayPacketControl::Skip;
                }
                *h -= damage;
                drop(h);
                queue_mace_smash(me, &held, calc.mace_fall, m.eid, [mx2, my2, mz2]);
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
                    let sprinting = *me.sprinting.lock().unwrap_or_else(|e| e.into_inner());
                    let env = MeleeSweepEnv {
                        calc: &calc,
                        held: &held,
                        attacker_pos: [px, py, pz],
                        on_ground: grounded,
                        sprinting,
                        target_box,
                    };
                    sweep_mobs_for_melee(me, &env, dim, m.eid, &ms[..], &mut dead, &mut swept);
                    sweep_players_for_melee(me, &env, dim, m.eid);
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
        let dim = *lk(&me.dim);
        if vt.as_ref().is_some_and(|v| *lk(&v.dim) != dim) {
            return PlayPacketControl::Skip;
        }
        let tp = vt
            .map(|v| {
                let p = *v.pos.lock().unwrap_or_else(|e| e.into_inner());
                (p.0, p.1, p.2, false)
            })
            .or_else(|| {
                let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
                ms.iter().find(|m| m.eid == target && m.dim == dim).map(|m| {
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
        // INTERACT_AT prefixes its hand with three hit-location floats.
        if mouse == 2 {
            let Some(rest) = sl.get(12..) else {
                return PlayPacketControl::Skip;
            };
            sl = rest;
        }
        let slot = match read_varint(&mut sl) {
            Some(0) => held_player_menu_slot(held_slot),
            Some(1) => 45,
            _ => return PlayPacketControl::Skip,
        };
        return play_c2s_use_entity_interact(&me, target, slot);
    }
    if mouse == 1 {
        return play_c2s_use_entity_attack(&me, target, held_slot);
    }
    PlayPacketControl::Handled
}

#[cfg(test)]
mod melee_dimension_tests {
    use super::*;

    #[test]
    fn pvp_nether_attacker_cannot_damage_overworld_player_at_same_coordinates() {
        let _guard = world_guard();
        let (attacker, _a) = crate::effect_tests::client_pair();
        let (mut victim, _v) = crate::effect_tests::client_pair();
        victim.eid = 899_011;
        let attacker = Arc::new(attacker);
        let victim = Arc::new(victim);
        *lk(&attacker.dim) = DimId::Nether;
        lk(clients()).push(Arc::clone(&victim));
        let hp = *lk(&victim.hp);
        play_c2s_use_entity_attack(&attacker, victim.eid, 0);
        let cross_dim = (*lk(&victim.hp), *lk(&victim.invulnerable));
        *lk(&attacker.dim) = DimId::Overworld;
        play_c2s_use_entity_attack(&attacker, victim.eid, 0);
        lk(clients()).retain(|c| c.eid != victim.eid);
        assert_eq!(cross_dim, (hp, 0));
        assert!(*lk(&victim.hp) < hp);
    }
}
