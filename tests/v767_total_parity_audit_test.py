#!/usr/bin/env python3
"""Regression tests for the Protocol 767 source-of-truth auditor."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("v767_total_parity_audit", ROOT / "tools/v767_total_parity_audit.py")
AUDIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUDIT)


def write(root, relative, text):
    target = root / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def fixture(root, *, duplicate_packet=False, commented_handler=False):
    packets = [
        '(PacketPhase::Play, PacketDir::Serverbound, "chat", 0x01)',
        '(PacketPhase::Play, PacketDir::Clientbound, "ping", 0x02)',
    ]
    if duplicate_packet:
        packets.append('(PacketPhase::Play, PacketDir::Serverbound, "conflict", 0x01)')
    write(root, "proto/src/data_v767.rs", "\n".join([
        'static V767_PACKETS: [(PacketPhase, PacketDir, &str, u32); '
        + str(len(packets)) + '] = [' + ', '.join(packets) + '];',
        'static V767_BLOCKS: [&str; 2] = ["minecraft:air", "minecraft:stone"];',
        'static V767_ITEMS: [&str; 2] = ["minecraft:air", "minecraft:stone"];',
        'static V767_ENTITY_TYPES: [&str; 1] = ["minecraft:allay"];',
        'static V767_MENUS: [&str; 1] = ["minecraft:generic_9x1"];',
        'static V767_COMPONENTS: [&str; 1] = ["minecraft:custom_data"];',
    ]))
    write(root, "proto/src/table.rs", '''
        table.insert_registry("minecraft:block", Registry::from_dense_names(&V767_BLOCKS));
        table.insert_registry("minecraft:item", Registry::from_dense_names(&V767_ITEMS));
    ''')
    write(root, "assets/blocks_1211.json", json.dumps([
        {"id": 0, "name": "air"}, {"id": 1, "name": "stone"},
    ]))
    write(root, "assets/block_states_1211.json", json.dumps({"blocks": [
        ["minecraft:air", 0, 1], ["minecraft:stone", 1, 1],
    ]}))
    write(root, "src/menus_and_items.rs", '''
        static ITEM_NAMES: [&str; 2] = ["air", "stone"];
    ''')
    write(root, "src/entity_registry.rs", '''
        static ENTITY_REGISTRY_1211: [EntitySpec; 1] = [
            entity_spec_1211!(0, "allay", 1, 2, 3)
        ];
    ''')
    write(root, "client-port/src/entity_rig.rs", '''
        const ENTITIES: [(&str, f32, f32); 1] = [("minecraft:allay", 1.0, 2.0)];
    ''')
    write(root, "client-ui/src/container_screen.rs", '''
        const VANILLA_MENU_COUNT: usize = 1;
        static ALL_VANILLA_MENUS: [MenuScreenSchema; VANILLA_MENU_COUNT] = [
            chest_schema(0, "minecraft:generic_9x1", 1, &SLOTS),
        ];
    ''')
    if commented_handler:
        server_code = '''
            fn handle_inner() {
                match pid {
                    // 0x01 => { handle_chat(); },
                    /* nested /* 0x01 => { handle_chat(); } */ */
                    _ => {}
                }
            }
            // fn emit() { send_packet(socket, 0x02, vec![]); }
            #[cfg(test)]
            fn test_emit() { send_packet(socket, 0x02, vec![]); }
            const COMMENT: &str = "send_packet(socket, 0x02, vec![])";
        '''
        client_code = '''
            // self.send_named(PacketPhase::Play, "chat", &payload);
            /* self.s2c_name(PacketPhase::Play, pid).as_deref() == Some("ping") */
            #[cfg(test)]
            fn test() { self.send_named(PacketPhase::Play, "chat", &payload); }
        '''
    else:
        server_code = '''
            fn handle_inner() {
                match pid { 0x01 => { handle_chat(); }, _ => {} }
                emit();
            }
            fn emit() { send_packet(socket, 0x02, vec![]); }
        '''
        client_code = '''
            fn send() { self.send_named(PacketPhase::Play, "chat", &payload); }
            fn receive() {
                if self.s2c_name(PacketPhase::Play, pid).as_deref() == Some("ping") {
                    decode_ping(body);
                }
            }
        '''
    write(root, "src/play_protocol.rs", server_code)
    write(root, "client-app/src/protocol.rs", client_code)


class AuthorityTests(unittest.TestCase):
    def test_real_repository_tables_have_no_registry_corruption(self):
        packets, registries = AUDIT.parse_authority(ROOT)
        self.assertEqual(AUDIT.validate_authority(packets, registries), [])
        self.assertEqual(len(packets), 226)
        self.assertEqual({name: len(rows) for name, rows in registries.items()}, {
            "block": 1060, "item": 1333, "entity_type": 130,
            "menu": 25, "data_component_type": 57,
        })

    def test_duplicate_packet_id_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root, duplicate_packet=True)
            report = AUDIT.audit(root)
            self.assertTrue(any("duplicate packet ID" in issue for issue in report["integrity_errors"]))
            with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                self.assertEqual(AUDIT.main(["--root", str(root), "--verify-registry-integrity"]), 1)

    def test_duplicate_registry_name_and_bad_length_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root)
            file = root / "proto/src/data_v767.rs"
            source = file.read_text(encoding="utf-8")
            file.write_text(source.replace('"minecraft:stone"];', '"minecraft:air"];', 1), encoding="utf-8")
            packets, registries = AUDIT.parse_authority(root)
            self.assertTrue(any("duplicate name" in error for error in AUDIT.validate_authority(packets, registries)))
            file.write_text(source.replace('V767_ITEMS: [&str; 2]', 'V767_ITEMS: [&str; 3]'), encoding="utf-8")
            with self.assertRaises(AUDIT.AuditError):
                AUDIT.parse_authority(root)

    def test_commented_and_test_only_handlers_never_count(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root, commented_handler=True)
            report = AUDIT.audit(root)
            sb = report["tables"]["Play/Serverbound"]
            cb = report["tables"]["Play/Clientbound"]
            self.assertEqual((sb["server_covered"], sb["client_covered"]), (0, 0))
            self.assertEqual((cb["server_covered"], cb["client_covered"]), (0, 0))

    def test_real_handlers_and_client_codec_count(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root)
            report = AUDIT.audit(root)
            self.assertEqual(report["integrity_errors"], [])
            self.assertEqual(report["asset_errors"], [])
            for direction in ("Serverbound", "Clientbound"):
                table = report["tables"][f"Play/{direction}"]
                self.assertEqual((table["server_covered"], table["client_covered"], table["gap_count"]), (1, 1, 0))
            output = io.StringIO()
            with redirect_stdout(output):
                self.assertEqual(AUDIT.main(["--root", str(root), "--json", "--verify-registry-integrity"]), 0)
            self.assertEqual(json.loads(output.getvalue())["tables"]["Play/Clientbound"]["gap_count"], 0)

    def test_rust_raw_strings_and_nested_comments_are_not_code(self):
        source = '''
            const A: &str = r###"send_packet(s, 0x02, p)"###;
            /* a /* send_packet(s, 0x02, p) */ end */
            #[cfg(test)] mod test { fn emit() { send_packet(s, 0x02, p); } }
            #[cfg(not(test))] fn emit() { send_packet(s, 0x03, p); }
        '''
        self.assertNotIn("0x02", AUDIT.rust_code(source))
        self.assertEqual(AUDIT._send_ids(source), {3})

    def test_nested_socket_expression_and_nested_payload_are_detected(self):
        source = '''
            fn deliver() {
                send_packet(
                    &mut peer.sock.lock().unwrap_or_else(|error| error.into_inner()),
                    0x05,
                    ack(seq),
                );
                send_packet(&mut sock, 0x09, vec![1, (2 + 3) as u8]);
                send_packet(&mut sock, dynamic_id, vec![]);
                // send_packet(&mut sock, 0x0f, vec![]);
                #[cfg(test)] fn bogus() { send_packet(&mut sock, 0x10, vec![]); }
            }
        '''
        self.assertEqual(AUDIT._send_ids(source), {5, 9})

    def emission(self, text, phase="Play", producer=False, consts=None):
        ids, _ = AUDIT._emission_ids(
            AUDIT.rust_code(text), phase, consts or {}, producer)
        return {number for _, number in ids}

    def test_packet_tuple_sinks_are_detected(self):
        source = '''
            fn seen(eid: i64) {
                broadcast_seen(eid, |_c| (0x70, entity_teleport(eid)));
                fanout_in(audience, |_| (0x08, body.clone()));
                broadcast(-1, move |_c| (0x75, pk.clone()));
                (*id, *payload) = match (pos, rot) {
                    (true, true) => (0x2f, move_pos_rot(eid)),
                    (true, false) => (0x2e, move_pos(eid)),
                    _ => return,
                };
            }
        '''
        self.assertEqual(self.emission(source), {0x08, 0x2e, 0x2f, 0x70, 0x75})

    def test_non_packet_tuples_and_patterns_are_rejected(self):
        source = '''
            fn data(inv: &mut Map, frame: &mut Frame) {
                inv.insert(k, (908, c - 1, Vec::new()));
                specs.push((0, "allay", 0.35));
                queue.insert(36 + held_slot, (908, cnt - 1, _hc));
                if let Some((0x1e, b)) = frame.next() { handle(b); }
                match packet {
                    Some((0x5e, body)) => deliver(body),
                    _ => {}
                }
                let (id, body) = produce();
                (x, y) = swapped;
            }
        '''
        self.assertEqual(self.emission(source), set())

    def test_frame_write_varint_and_send_closure_forms(self):
        source = '''
            const CLIENTBOUND_COMMANDS: i64 = 0x11;
            fn emit(sock: &mut TcpStream, targets: &[i32]) {
                let framed = frame(0x6d, &payload);
                write_raw_frame(sock, &framed);
                let mut body = Vec::new();
                write_varint(&mut body, 0x27);
                write_varint(&mut body, 512);
                send_packet(sock, i64::from(CLIENTBOUND_COMMANDS), commands_payload(level));
                let send_to = |packet: i64, payload: &Vec<u8>| -> usize {
                    let mut n = 0;
                    for e in targets { let _ = send_packet(sock, packet, payload.clone()); n += 1; }
                    n
                };
                send_to(0x0f, &vec![1u8]);
                let pkt_id: i64 = match mode { "title" => 0x65, _ => 0x4c };
                send_to(pkt_id, &payload);
            }
        '''
        expected = {0x0f, 0x11, 0x27, 0x4c, 0x65, 0x6d}
        self.assertEqual(
            self.emission(source, consts={"CLIENTBOUND_COMMANDS": 0x11}),
            expected)
        # The const-declared packet id also resolves through the crate table.
        self.assertEqual(self.emission(source) - {0x11}, expected - {0x11})

    def test_uncalled_and_dead_helpers_do_not_count(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root)
            write(root, "src/extra.rs", '''
                pub fn dead_emit(s: &mut TcpStream) { send_packet(s, 0x18, vec![]); }
                #[allow(dead_code)]
                fn live_encode() -> (i64, Vec<u8>) { (0x19, Vec::new()) }
                fn caller(c: &Client) { broadcast(-1, |_c| live_encode()); }
            ''')
            write(root, "src/play_protocol.rs", '''
                fn handle_inner() {
                    match pid { 0x01 => { handle_chat(); }, _ => {} }
                    caller(&me);
                }
            ''')
            c2s, sent = AUDIT._server_packets(
                root, (root / "src/play_protocol.rs").read_text(), "")
            self.assertIn(("Play", 0x19), sent)
            self.assertNotIn(("Play", 0x18), sent)

    def test_unknown_natural_spawn_dispatch_entity_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root)
            write(root, "src/natural_spawner.rs", '''
                fn from_id(id: &str) -> Option<Self> {
                    Some(match id {
                        "minecraft:unlisted_entity" => Self::Unknown,
                        _ => return None,
                    })
                }
            ''')
            report = AUDIT.audit(root)
            self.assertTrue(any("unknown entity type" in error for error in report["asset_errors"]))
            with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                self.assertEqual(AUDIT.main(["--root", str(root), "--verify-registry-integrity"]), 1)


if __name__ == "__main__":
    unittest.main()
