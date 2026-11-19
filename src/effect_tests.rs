use super::*;

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
        pos: Mutex::new((0.0, 70.0, 0.0, 0.0, 0.0, true)),
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
        .unwrap()
        .insert(45, (totem, 2, Vec::new()));
    *client.hp.lock().unwrap() = 0.0;
    client.effs.lock().unwrap().push(Eff {
        id: 18,
        amp: 1,
        left: 80,
    });
    totem_check(&client);
    assert_eq!(*client.hp.lock().unwrap(), 1.0);
    assert_eq!(
        client.inv.lock().unwrap().get(&45),
        Some(&(totem, 1, Vec::new()))
    );
    assert_eq!(*client.absorb.lock().unwrap(), 8.0);
    assert_eq!(hurt_absorb(&client, 3.0), 0.0);
    assert_eq!(*client.absorb.lock().unwrap(), 5.0);
    assert_eq!(hurt_absorb(&client, 7.0), 2.0);
    assert_eq!(*client.absorb.lock().unwrap(), 0.0);

    client
        .sock
        .lock()
        .unwrap()
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
    client.inv.lock().unwrap().insert(
        45,
        (name_to_item("totem_of_undying").unwrap(), 1, Vec::new()),
    );
    *client.hp.lock().unwrap() = 1.0;
    *client.effs.lock().unwrap() = vec![
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
    assert_eq!(*client.hp.lock().unwrap(), 1.0);
    assert!(!client.inv.lock().unwrap().contains_key(&45));
    assert_eq!(*client.food.lock().unwrap(), 20);
    assert_eq!(*client.absorb.lock().unwrap(), 8.0);
    let mut actual: Vec<_> = client
        .effs
        .lock()
        .unwrap()
        .iter()
        .map(|e| (e.id, e.amp, e.left))
        .collect();
    actual.sort_unstable();
    assert_eq!(actual, vec![(9, 1, 900), (11, 0, 800), (21, 1, 100)]);
}

#[test]
fn absorption_expiry_removes_unspent_shield() {
    let (client, _receiver) = client_pair();
    *client.absorb.lock().unwrap() = 8.0;
    client.effs.lock().unwrap().push(Eff {
        id: 21,
        amp: 1,
        left: 2,
    });
    effects::tick_effects(&client);
    assert_eq!(
        *client.absorb.lock().unwrap(),
        8.0,
        "still active before expiry"
    );
    effects::tick_effects(&client);
    assert!(client.effs.lock().unwrap().is_empty());
    assert_eq!(
        hurt_absorb(&client, 3.0),
        3.0,
        "expired absorption must not intercept damage"
    );
    assert_eq!(*client.absorb.lock().unwrap(), 0.0);
}

/// `LivingEntity.hurt`'s invulnerability rule, checked against what the 1.21.1
/// bytecode does: the full-immunity window is `invulnerableTime > 10`, an
/// equal-or-weaker hit is rejected, a stronger one applies only the difference,
/// and applying the difference does *not* refresh the timer.
#[test]
fn vulnerability_follows_vanillas_invulnerable_time_rule() {
    let (c, _sock) = client_pair();

    // a first hit applies in full and arms the twenty-tick window
    assert_eq!(damage_after_invulnerability(&c, 5.0), Some(5.0));
    assert_eq!(*c.invulnerable.lock().unwrap(), 20);
    assert_eq!(*c.last_hurt.lock().unwrap(), 5.0);

    // nine ticks later the window is still open (20 -> 11)
    *c.invulnerable.lock().unwrap() = 11;
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
        Some(3.0),
        "a stronger hit applies only the difference"
    );
    assert_eq!(
        *c.invulnerable.lock().unwrap(),
        11,
        "the difference hit must not refresh the timer"
    );
    assert_eq!(
        *c.last_hurt.lock().unwrap(),
        8.0,
        "the difference hit moves the recorded amount up"
    );

    // at ten ticks the full-immunity window is over
    *c.invulnerable.lock().unwrap() = 10;
    assert_eq!(damage_after_invulnerability(&c, 1.0), Some(1.0));
    assert_eq!(*c.invulnerable.lock().unwrap(), 20);
    assert_eq!(*c.last_hurt.lock().unwrap(), 1.0);
}

/// A rejected hit must leave the state it is measured against untouched - the
/// melee and explosion callers rely on that to skip shield and armour damage.
#[test]
fn a_rejected_hit_changes_nothing() {
    let (c, _sock) = client_pair();
    assert_eq!(damage_after_invulnerability(&c, 4.0), Some(4.0));
    *c.invulnerable.lock().unwrap() = 15;
    assert_eq!(damage_after_invulnerability(&c, 4.0), None);
    assert_eq!(*c.last_hurt.lock().unwrap(), 4.0);
    assert_eq!(*c.invulnerable.lock().unwrap(), 15);
}
