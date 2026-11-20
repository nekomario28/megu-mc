use super::*;

fn entity_angle_cases() -> Vec<json::J> {
    let fixture = json::jparse(include_bytes!(
        "../evidence/gameplay/entity-angles/vanilla.json"
    ))
    .unwrap();
    assert_eq!(fixture.get("minecraft").unwrap().str(), "1.21.1");
    let cases = fixture.get("cases").unwrap().as_arr().unwrap().clone();
    assert!(!cases.is_empty(), "an empty oracle is not coverage");
    cases
}
fn rotations(case: &json::J) -> (f32, f32) {
    (
        f32::from_bits(case.get("yaw_bits").unwrap().num() as u32),
        f32::from_bits(case.get("pitch_bits").unwrap().num() as u32),
    )
}
fn expected_bytes(case: &json::J, key: &str) -> Vec<u8> {
    case.get("expected")
        .unwrap()
        .get(key)
        .unwrap()
        .as_arr()
        .unwrap()
        .iter()
        .map(|j| j.num() as u8)
        .collect()
}
fn spawn_rotation_bytes(packet: &[u8]) -> &[u8] {
    let mut rest = packet;
    assert_eq!(read_varint(&mut rest), Some(42));
    rest = &rest[16..]; // UUID
    read_varint(&mut rest).unwrap(); // type (not part of this angle oracle)
    assert_eq!(rest.len(), 24 + 3 + 1 + 6);
    &rest[24..27]
}

#[test]
fn entity_angles_spawn_matches_native_constructors() {
    let (mut client, _reader) = effect_tests::client_pair();
    client.eid = 42;
    let mob = Mob {
        dim: DimId::Overworld,
        eid: 42,
        uuid: [0; 16],
        etype: 124,
        pos: Mutex::new((1.0, 64.0, -2.0, 0.0, 0.0)),
        hp: Mutex::new(20.0),
        last_hit: Mutex::new(Instant::now()),
        hurt_at: Mutex::new(Instant::now()),
        last_hurt_tick: Mutex::new(None),
        vel: Mutex::new([0.0; 3]),
        fuse: Mutex::new(0),
        fire_ticks: Mutex::new(0),
        love: Mutex::new(0),
        air: Mutex::new(300),
    };
    for case in entity_angle_cases() {
        let (yaw, pitch) = rotations(&case);
        *client.pos.lock().unwrap() = (1.0, 64.0, -2.0, yaw, pitch, false);
        *mob.pos.lock().unwrap() = (1.0, 64.0, -2.0, yaw, pitch);
        let want = expected_bytes(&case, "spawn");
        for packet in [
            spawn_player(&client),
            spawn_mob(&mob),
            spawn_entity_pkt(42, 124, 1.0, 64.0, -2.0, yaw, pitch, 0),
        ] {
            assert_eq!(
                spawn_rotation_bytes(&packet),
                want,
                "{}",
                case.get("id").unwrap().str()
            );
        }
    }
}

#[test]
fn entity_angles_teleport_matches_native_constructor() {
    for case in entity_angle_cases() {
        let (yaw, pitch) = rotations(&case);
        let packet = entity_teleport(42, 1.0, 64.0, -2.0, yaw, pitch, false);
        let mut rest = packet.as_slice();
        assert_eq!(read_varint(&mut rest), Some(42));
        assert_eq!(rest.len(), 24 + 2 + 1);
        assert_eq!(
            &rest[24..26],
            expected_bytes(&case, "teleport"),
            "{}",
            case.get("id").unwrap().str()
        );
    }
}

#[test]
fn entity_angles_head_update_matches_native_floor() {
    for case in entity_angle_cases() {
        let (yaw, _) = rotations(&case);
        let want = case
            .get("expected")
            .unwrap()
            .get("head_update")
            .unwrap()
            .num() as u8;
        assert_eq!(
            head_rot(42, yaw),
            [42, want],
            "{}",
            case.get("id").unwrap().str()
        );
    }
}
