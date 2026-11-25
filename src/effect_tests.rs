use super::*;
use crate::effects::*;

#[test]
fn respawn_reset_clears_transient_state_and_retains_inventory() {
    let (client, _receiver) = client_pair();
    client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(45, (1, 1, Vec::new()));
    client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff { id: 1, amp: 0, left: 100 });
    *client.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 4.0;
    *client.air.lock().unwrap_or_else(|e| e.into_inner()) = 0;
    *client.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) = 10;
    *client.last_hurt.lock().unwrap_or_else(|e| e.into_inner()) = 5.0;
    *client.blocking.lock().unwrap_or_else(|e| e.into_inner()) = true;
    *client.food.lock().unwrap_or_else(|e| e.into_inner()) = 2;
    reset_player_food(&client);
    assert!(client.effs.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 0.0);
    assert_eq!(*client.air.lock().unwrap_or_else(|e| e.into_inner()), AIR_SUPPLY_GT as i32);
    assert_eq!(*client.invulnerable.lock().unwrap_or_else(|e| e.into_inner()), 0);
    assert_eq!(*client.last_hurt.lock().unwrap_or_else(|e| e.into_inner()), 0.0);
    assert!(!*client.blocking.lock().unwrap_or_else(|e| e.into_inner()));
    assert_eq!(*client.food.lock().unwrap_or_else(|e| e.into_inner()), 20);
    assert!(client.inv.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&45));
}

#[test]
fn attack_speed_effects_change_live_melee_charge() {
    let (client, _receiver) = client_pair();
    let client = Arc::new(client);
    let sword = name_to_item("diamond_sword").unwrap();
    *client.last_melee.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now() - Duration::from_millis(300);
    let normal = melee_charge(&client, sword);
    client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff { id: 2, amp: 1, left: 100 });
    *client.last_melee.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now() - Duration::from_millis(300);
    let haste = melee_charge(&client, sword);
    assert!(haste > normal * 1.1 && haste < 1.0);
}

#[test]
fn armor_damage_honors_custom_maximum_and_breaks() {
    let (client, _receiver) = client_pair();
    let client = Arc::new(client);
    let mut comps = Vec::new();
    comp_set_varint(&mut comps, COMP_MAX_DAMAGE, 2);
    client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(5, (name_to_item("iron_helmet").unwrap(), 1, comps));
    damage_armor(&client, 4.0);
    assert_eq!(comp_varint(&client.inv.lock().unwrap_or_else(|e| e.into_inner())[&5].2, COMP_DAMAGE), Some(1));
    damage_armor(&client, 4.0);
    assert!(!client.inv.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&5));
}

#[test]
fn player_attack_damage_id_uses_the_replayed_registry_order() {
    fn registry(entries: &[&str]) -> (i64, Vec<u8>) {
        fn name(packet: &mut Vec<u8>, value: &str) {
            write_varint(packet, value.len() as i64);
            packet.extend_from_slice(value.as_bytes());
        }
        let mut packet = vec![0x07];
        name(&mut packet, "minecraft:damage_type");
        write_varint(&mut packet, entries.len() as i64);
        for entry in entries {
            name(&mut packet, entry);
            packet.extend_from_slice(&[1, 10, 0]); // present, empty unnamed compound
        }
        (0x07, packet)
    }

    let attack = "minecraft:player_attack";
    let generic = "minecraft:generic";
    assert_eq!(
        player_attack_damage_wire_id(&[registry(&[attack, generic])]),
        Some(0)
    );
    assert_eq!(
        player_attack_damage_wire_id(&[registry(&[generic, attack])]),
        Some(1)
    );
    assert_eq!(player_attack_damage_wire_id(&[registry(&[generic])]), None);
    assert_eq!(
        player_attack_damage_wire_id(&[registry(&[attack, attack])]),
        None
    );

    let (id, mut truncated) = registry(&[attack, generic]);
    truncated.pop();
    assert_eq!(player_attack_damage_wire_id(&[(id, truncated)]), None);
}

pub(super) fn client_pair() -> (Client, TcpStream) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let receiver = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let (sender, _) = listener.accept().unwrap();
    let past = Instant::now() - Duration::from_secs(10);
    let client = Client {
        eid: 4242,
        uuid: [0; 16],
        name: "totem-test".into(),
        player_attack_damage_id: None,
        pos: Mutex::new((0.0, 70.0, 0.0, 0.0, 0.0, true)),
        teleport: Mutex::new(Default::default()),
        hp: Mutex::new(20.0),
        sock: Mutex::new(sender),
        inv: Mutex::new(HashMap::new()),
        cursor: Mutex::new((0, 0, Vec::new())),
        drag: Mutex::new((-1, Vec::new())),
        open_chest: Mutex::new(None),
        chest_sync: Mutex::new(None),
        chunk_ack: Mutex::new(ChunkFeedback::new(1)),
        open_furnace: Mutex::new(None),
        craft: Mutex::new(HashMap::new()),
        craft9: Mutex::new(HashMap::new()),
        open_table: Mutex::new(false),
        open_merchant: Mutex::new(false),
        trade_sel: Mutex::new(-1),
        merchant_in: Mutex::new([(0, 0, Vec::new()), (0, 0, Vec::new())]),
        bow_draw: Mutex::new(None),
        dig: Mutex::new(None),
        eat_start: Mutex::new(None),
        blocking: Mutex::new(false),
        sprinting: Mutex::new(false),
        sneaking: Mutex::new(false),
        last_melee: Mutex::new(past),
        fall_fly: Mutex::new(false),
        invulnerable: Mutex::new(0),
        last_hurt: Mutex::new(0.0),
        shield_cd: Mutex::new(past),
        absorb: Mutex::new(0.0),
        pearl_cd: Mutex::new(past),
        xp: Mutex::new(0),
        skin: Mutex::new(None),
        seen: Mutex::new(HashSet::new()),
        chunks: Mutex::new(HashSet::new()),
        gen_sources: Mutex::new(HashMap::new()),
        gm: Mutex::new(0),
        fall_from: Mutex::new(70.0),
        food: Mutex::new(20),
        air: Mutex::new(60),
        armor_pts: Mutex::new(0),
        sat: Mutex::new(5.0),
        walk_acc: Mutex::new(0.0),
        lastp: Mutex::new((0.0, 70.0, 0.0)),
        spawn_pt: Mutex::new(None),
        dim: Mutex::new(DimId::Overworld),
        spawn_dim: Mutex::new(DimId::Overworld),
        view: Mutex::new(2),
        in_portal: Mutex::new(false),
        portal_at: Mutex::new(None),
        held: Mutex::new(0),
        effs: Mutex::new(Vec::new()),
        lod_v1_enabled: AtomicBool::new(false),
        underground_revealed: AtomicBool::new(false),
        sleeping: Mutex::new(None),
        inv_state_id: AtomicI32::new(0),
        generic_kill_pending: AtomicBool::new(false),
        use_on_resend: Mutex::new(None),
    };
    (client, receiver)
}

#[test]
fn death_protection_updates_state_and_sends_effect_changes() {
    let (client, mut receiver) = client_pair();
    let totem = name_to_item("totem_of_undying").unwrap();
    client
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(45, (totem, 2, Vec::new()));
    *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
    client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
        id: 18,
        amp: 1,
        left: 80,
    });
    totem_check(&client);
    assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 1.0);
    assert_eq!(
        client.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45),
        Some(&(totem, 1, Vec::new()))
    );
    assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 8.0);
    assert_eq!(hurt_absorb(&client, 3.0), 0.0);
    assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 5.0);
    assert_eq!(hurt_absorb(&client, 7.0), 2.0);
    assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 0.0);

    client
        .sock
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    let mut changes = Vec::new();
    while let Some((packet, body)) =
        read_packet(&mut receiver, Instant::now() + Duration::from_secs(1))
    {
        if packet == 0x43 || packet == 0x76 {
            let mut payload = &body[..];
            let entity = read_varint(&mut payload).unwrap();
            let effect = read_varint(&mut payload).unwrap();
            if packet == 0x43 {
                assert!(payload.is_empty());
                changes.push((packet, entity, effect, None));
            } else {
                let amplifier = read_varint(&mut payload).unwrap();
                let duration = read_varint(&mut payload).unwrap();
                assert_eq!(payload, &[0x06]);
                changes.push((packet, entity, effect, Some((amplifier, duration))));
            }
        }
    }
    assert_eq!(
        changes,
        vec![
            (0x43, 4242, 18, None),
            (0x76, 4242, 9, Some((1, 900))),
            (0x76, 4242, 21, Some((1, 100))),
            (0x76, 4242, 11, Some((0, 800))),
        ]
    );
}

#[test]
fn totem_replaces_existing_effects_with_vanilla_grant() {
    let mut effects = vec![
        Eff {
            id: 18,
            amp: 2,
            left: 120,
        }, // poison must not survive death protection
        Eff {
            id: 9,
            amp: 4,
            left: 1,
        },
        Eff {
            id: 21,
            amp: 0,
            left: 40,
        },
        Eff {
            id: 21,
            amp: 0,
            left: 40,
        },
    ];
    // Stored durations are vanilla game ticks, which equal server ticks at 20TPS.
    let snapshot = |effects: &[Eff]| {
        let mut result: Vec<_> = effects.iter().map(|e| (e.id, e.amp, e.left)).collect();
        result.sort_unstable();
        result
    };
    let expected = vec![(9, 1, 900), (11, 0, 800), (21, 1, 100)];
    totem_effects(&mut effects);
    assert_eq!(snapshot(&effects), expected);
    totem_effects(&mut effects);
    assert_eq!(
        snapshot(&effects),
        expected,
        "repeated grants must not duplicate effects"
    );
}

#[test]
fn wither_death_protection_stops_old_effects_and_keeps_new_grants() {
    let (client, _receiver) = client_pair();
    client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(
        45,
        (name_to_item("totem_of_undying").unwrap(), 1, Vec::new()),
    );
    *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 1.0;
    *client.effs.lock().unwrap_or_else(|e| e.into_inner()) = vec![
        Eff {
            id: 21,
            amp: 0,
            left: 1,
        }, // expires in the old set
        Eff {
            id: 19,
            amp: 0,
            left: 1,
        }, // lethal wither
        Eff {
            id: 16,
            amp: 0,
            left: 81,
        }, // must not run after death protection removes it
    ];
    effects::tick_effects(&client);
    assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 1.0);
    assert!(!client.inv.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&45));
    assert_eq!(*client.food.lock().unwrap_or_else(|e| e.into_inner()), 20);
    assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 8.0);
    let mut actual: Vec<_> = client
        .effs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|e| (e.id, e.amp, e.left))
        .collect();
    actual.sort_unstable();
    assert_eq!(actual, vec![(9, 1, 900), (11, 0, 800), (21, 1, 100)]);
}

#[test]
fn absorption_expiry_removes_unspent_shield() {
    let (client, _receiver) = client_pair();
    *client.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 8.0;
    client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
        id: 21,
        amp: 1,
        left: 2,
    });
    effects::tick_effects(&client);
    assert_eq!(
        *client.absorb.lock().unwrap_or_else(|e| e.into_inner()),
        8.0,
        "still active before expiry"
    );
    effects::tick_effects(&client);
    assert!(client.effs.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    assert_eq!(
        hurt_absorb(&client, 3.0),
        3.0,
        "expired absorption must not intercept damage"
    );
    assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 0.0);
}

/// `LivingEntity.hurt`'s invulnerability rule, checked against what the 1.21.1
/// bytecode does: the full-immunity window is `invulnerableTime > 10`, an
/// equal-or-weaker hit is rejected, a stronger one applies only the difference,
/// and applying the difference does *not* refresh the timer.
#[test]
fn vulnerability_follows_vanillas_invulnerable_time_rule() {
    let (c, _sock) = client_pair();

    // a first hit applies in full and arms the twenty-tick window
    assert_eq!(damage_after_invulnerability(&c, 5.0), Some((5.0, true)));
    assert_eq!(*c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()), 20);
    assert_eq!(*c.last_hurt.lock().unwrap_or_else(|e| e.into_inner()), 5.0);

    // nine ticks later the window is still open (20 -> 11)
    *c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) = 11;
    assert_eq!(
        damage_after_invulnerability(&c, 5.0),
        None,
        "an equal hit is rejected"
    );
    assert_eq!(
        damage_after_invulnerability(&c, 3.0),
        None,
        "a weaker hit is rejected"
    );
    assert_eq!(
        damage_after_invulnerability(&c, 8.0),
        Some((3.0, false)),
        "a stronger hit applies only the difference without a new hurt event"
    );
    assert_eq!(
        *c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()),
        11,
        "the difference hit must not refresh the timer"
    );
    assert_eq!(
        *c.last_hurt.lock().unwrap_or_else(|e| e.into_inner()),
        8.0,
        "the difference hit moves the recorded amount up"
    );

    // at ten ticks the full-immunity window is over
    *c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) = 10;
    assert_eq!(damage_after_invulnerability(&c, 1.0), Some((1.0, true)));
    assert_eq!(*c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()), 20);
    assert_eq!(*c.last_hurt.lock().unwrap_or_else(|e| e.into_inner()), 1.0);
}

/// A rejected hit must leave the state it is measured against untouched - the
/// melee and explosion callers rely on that to skip shield and armour damage.
#[test]
fn a_rejected_hit_changes_nothing() {
    let (c, _sock) = client_pair();
    assert_eq!(damage_after_invulnerability(&c, 4.0), Some((4.0, true)));
    *c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) = 15;
    assert_eq!(damage_after_invulnerability(&c, 4.0), None);
    assert_eq!(*c.last_hurt.lock().unwrap_or_else(|e| e.into_inner()), 4.0);
    assert_eq!(*c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()), 15);
}

#[test]
fn vanilla_1211_effect_registry_names_and_ids_round_trip() {
    let names = [
        "speed",
        "slowness",
        "haste",
        "mining_fatigue",
        "strength",
        "instant_health",
        "instant_damage",
        "jump_boost",
        "nausea",
        "regeneration",
        "resistance",
        "fire_resistance",
        "water_breathing",
        "invisibility",
        "blindness",
        "night_vision",
        "hunger",
        "weakness",
        "poison",
        "wither",
        "health_boost",
        "absorption",
        "saturation",
        "glowing",
        "levitation",
        "luck",
        "unluck",
        "slow_falling",
        "conduit_power",
        "dolphins_grace",
        "bad_omen",
        "hero_of_the_village",
        "darkness",
        "trial_omen",
        "raid_omen",
        "wind_charged",
        "weaving",
        "oozing",
        "infested",
    ];
    assert_eq!(names.len(), 39);
    for (index, name) in names.iter().enumerate() {
        let id = index as i32 + 1;
        assert_eq!(effect_registry_name(id), Some(*name));
        assert_eq!(effect_registry_id(name), Some(id));
    }
    assert_eq!(effect_registry_name(0), None);
    assert_eq!(effect_registry_name(40), None);
    assert_eq!(effect_registry_id("minecraft:conduit_power"), Some(29));
    assert_eq!(effect_registry_id("missing"), None);
    assert_eq!(effect_internal_id(1), Some(0));
    assert_eq!(effect_internal_id(17), Some(HUNGER_EFFECT));
    assert_eq!(effect_internal_id(39), Some(38));
}

#[test]
fn vanilla_attribute_modifiers_cover_each_effect_level() {
    assert_eq!(
        effect_attribute_modifier(1, 0, EffectAttribute::MovementSpeed),
        1.2
    );
    assert_eq!(
        effect_attribute_modifier(1, 1, EffectAttribute::MovementSpeed),
        1.4
    );
    assert!((effect_attribute_modifier(2, 0, EffectAttribute::MovementSpeed) - 0.85).abs() < 1e-6);
    assert_eq!(
        effect_attribute_modifier(2, 8, EffectAttribute::MovementSpeed),
        0.0,
        "slowness cannot make the speed factor negative"
    );
    assert!((effect_attribute_modifier(3, 0, EffectAttribute::AttackSpeed) - 1.1).abs() < 1e-6);
    assert_eq!(
        effect_attribute_modifier(4, 0, EffectAttribute::AttackSpeed),
        0.9
    );
    assert_eq!(
        effect_attribute_modifier(5, 2, EffectAttribute::AttackDamage),
        9.0
    );
    assert_eq!(
        effect_attribute_modifier(18, 1, EffectAttribute::AttackDamage),
        -8.0
    );
    assert_eq!(
        effect_attribute_modifier(21, 3, EffectAttribute::MaxHealth),
        16.0
    );
    assert_eq!(
        effect_attribute_modifier(22, 1, EffectAttribute::MaxAbsorption),
        8.0
    );
    assert_eq!(
        effect_attribute_modifiers(23, 0),
        EffectAttributeModifiers::default(),
        "unrelated effects do not alter attributes"
    );
}

#[test]
fn vanilla_damage_mitigation_and_slow_falling_edges() {
    assert!((resistance_multiplier(0) - 0.8).abs() < 1e-6);
    assert!((resistance_multiplier(3) - 0.2).abs() < 1e-6);
    assert_eq!(resistance_multiplier(4), 0.0, "level five is immune");
    assert_eq!(
        incoming_damage_after_effects(10.0, DamageKind::Ordinary, Some(0), false, false),
        8.0
    );
    assert_eq!(
        incoming_damage_after_effects(10.0, DamageKind::Ordinary, Some(4), false, false),
        0.0
    );
    for kind in [DamageKind::Fire, DamageKind::Lava] {
        assert_eq!(
            incoming_damage_after_effects(10.0, kind, None, true, false),
            0.0
        );
    }
    assert_eq!(
        incoming_damage_after_effects(10.0, DamageKind::Ordinary, None, true, false),
        10.0,
        "Fire Resistance does not block ordinary damage"
    );
    assert_eq!(
        incoming_damage_after_effects(10.0, DamageKind::Fall, None, false, true),
        0.0
    );
    for bypass in [DamageKind::Void, DamageKind::Kill] {
        assert_eq!(
            incoming_damage_after_effects(10.0, bypass, Some(8), true, true),
            10.0,
            "Resistance does not block void or kill damage"
        );
    }
    assert_eq!(slow_falling_gravity_multiplier(true), 0.125);
    assert_eq!(slow_falling_gravity_multiplier(false), 1.0);
}

#[test]
fn vanilla_periodic_effect_rates_and_high_amplifiers() {
    assert_eq!(periodic_effect_interval(50, 0), 50);
    assert_eq!(periodic_effect_interval(50, 1), 25);
    assert_eq!(periodic_effect_interval(25, 1), 12);
    assert_eq!(periodic_effect_interval(40, 2), 10);
    assert_eq!(periodic_effect_interval(25, 31), 1);
    assert_eq!(periodic_effect_interval(50, 255), 1);
    assert_eq!(regeneration_heal(4), 1.0);
    assert_eq!(poison_damage(4), 1.0);
    assert_eq!(wither_damage(4), 1.0);
}

#[test]
fn vanilla_food_air_levitation_and_instant_effect_formulas() {
    assert_eq!(hunger_exhaustion_per_tick(0), 0.005);
    assert_eq!(hunger_exhaustion_per_tick(2), 0.015);
    assert_eq!(saturation_tick(10, 3.0), (11, 5.0));
    assert_eq!(saturation_tick(19, 19.0), (20, 20.0));
    assert_eq!(saturation_tick(20, 20.0), (20, 20.0));
    assert_eq!(saturation_tick(0, 0.0), (1, 1.0));
    assert!(!effect_prevents_drowning(&[1, 12]));
    assert!(effect_prevents_drowning(&[13]));
    assert!(effect_prevents_drowning(&[29]));
    assert!((levitation_velocity_delta(0.0, 0) - 0.01).abs() < 1e-12);
    assert!((levitation_velocity_delta(0.2, 0) + 0.03).abs() < 1e-12);
    assert_eq!(instant_effect_amount(true, 0), 4.0);
    assert_eq!(instant_effect_amount(true, 2), 16.0);
    assert_eq!(instant_effect_amount(false, 0), 6.0);
    assert_eq!(instant_effect_amount(false, 2), 24.0);
}

#[test]
fn tick_effects_apply_saturation_water_breathing_and_regeneration() {
    let (client, _receiver) = client_pair();
    *client.food.lock().unwrap_or_else(|e| e.into_inner()) = 18;
    *client.sat.lock().unwrap_or_else(|e| e.into_inner()) = 4.0;
    *client.air.lock().unwrap_or_else(|e| e.into_inner()) = 3;
    *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 10.0;
    *client.effs.lock().unwrap_or_else(|e| e.into_inner()) = vec![
        Eff {
            id: 22,
            amp: 0,
            left: 20,
        }, // Saturation
        Eff {
            id: 12,
            amp: 0,
            left: 20,
        }, // Water Breathing
        Eff {
            id: 9,
            amp: 0,
            left: 51,
        }, // Regeneration: remaining duration reaches 50
    ];

    effects::tick_effects(&client);

    assert_eq!(*client.food.lock().unwrap_or_else(|e| e.into_inner()), 19);
    assert_eq!(*client.sat.lock().unwrap_or_else(|e| e.into_inner()), 6.0);
    assert_eq!(*client.air.lock().unwrap_or_else(|e| e.into_inner()), AIR_SUPPLY_GT as i32);
    assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 11.0);
}

#[test]
fn tick_effects_keep_poison_nonlethal_and_wither_lethal() {
    let (client, _receiver) = client_pair();
    *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 1.0;
    client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
        id: 18,
        amp: 0,
        left: 26,
    });
    effects::tick_effects(&client);
    assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 1.0);

    client.effs.lock().unwrap_or_else(|e| e.into_inner()).clear();
    *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 1.0;
    client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
        id: 19,
        amp: 0,
        left: 41,
    });
    effects::tick_effects(&client);
    assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 0.0);
}
