#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Static Protocol 767 source parity audit, anchored to data_v767.rs.

Coverage means a specific production source-level dispatch, emission, decoder,
or registry mapping was found. It does not certify wire compatibility or that
a runtime path is reachable. Missing or ambiguous evidence remains a gap.
"""

import argparse
import ast
import json
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
REGISTRIES = {
    "block": "V767_BLOCKS",
    "item": "V767_ITEMS",
    "entity_type": "V767_ENTITY_TYPES",
    "menu": "V767_MENUS",
    "data_component_type": "V767_COMPONENTS",
}
PHASES = ("Handshake", "Status", "Login", "Configuration", "Play")
DIRECTIONS = ("Serverbound", "Clientbound")
STRING = r'"(?:\\.|[^"\\])*"'
RAW_STRING = re.compile(r'(?:b|c)?r(#{0,255})"')
CHARACTER = re.compile(r"'(?:[^'\\\n]|\\(?:.|u\{[A-Fa-f0-9_]+\}))'")
CFG_TEST = re.compile(r'\bcfg\s*\(\s*(?:test\b|all\s*\(\s*test\b)')
INTEGER = r'(?:0x[0-9a-fA-F_]+|[0-9][0-9_]*)'


class AuditError(ValueError):
    """An authoritative table is malformed or incomplete."""


def _blank(chars, start, end):
    for position in range(start, end):
        if chars[position] != "\n":
            chars[position] = " "


def _brace_end(code, start, opening="{", closing="}"):
    depth = 0
    for position in range(start, len(code)):
        if code[position] == opening:
            depth += 1
        elif code[position] == closing:
            depth -= 1
            if depth == 0:
                return position + 1
    raise AuditError(f"unbalanced {opening}{closing} at offset {start}")


def rust_code(source, keep_strings=False):
    """Strip Rust comments, literals for code checks, and cfg(test) items.

    Both output modes preserve offsets. In keep_strings mode string literals
    remain for name-based match arms, while comments and test items disappear.
    """
    comments = list(source)
    code = list(source)
    position = 0
    while position < len(source):
        start = position
        if source.startswith("//", position):
            end = source.find("\n", position)
            position = len(source) if end == -1 else end
        elif source.startswith("/*", position):
            position += 2
            depth = 1
            while position < len(source) and depth:
                if source.startswith("/*", position):
                    depth += 1
                    position += 2
                elif source.startswith("*/", position):
                    depth -= 1
                    position += 2
                else:
                    position += 1
            if depth:
                raise AuditError("unclosed Rust block comment")
        elif raw := RAW_STRING.match(source, position):
            end_marker = '"' + raw.group(1)
            end = source.find(end_marker, raw.end())
            if end == -1:
                raise AuditError("unclosed Rust raw string")
            position = end + len(end_marker)
            _blank(code, start, position)
            continue
        elif source[position] == '"':
            position += 1
            while position < len(source):
                if source[position] == "\\":
                    position += 2
                elif source[position] == '"':
                    position += 1
                    break
                else:
                    position += 1
            else:
                raise AuditError("unclosed Rust string")
            _blank(code, start, position)
            continue
        elif char := CHARACTER.match(source, position):
            position = char.end()
            _blank(code, start, position)
            continue
        else:
            position += 1
            continue
        _blank(comments, start, position)
        _blank(code, start, position)

    clean = "".join(code)
    for attr in re.finditer(r'#\s*\[', clean):
        if code[attr.start()] == " ":
            continue
        bracket = clean.index("[", attr.start())
        attr_end = _brace_end(clean, bracket, "[", "]")
        if not CFG_TEST.search(clean[attr.start():attr_end]):
            continue
        item_start = attr_end
        while True:
            while item_start < len(clean) and clean[item_start].isspace():
                item_start += 1
            if not clean.startswith("#[", item_start):
                break
            item_start = _brace_end(clean, item_start + 1, "[", "]")
        parens = brackets = 0
        item_end = len(clean)
        for pos in range(item_start, len(clean)):
            token = clean[pos]
            if token == "(":
                parens += 1
            elif token == ")":
                parens -= 1
            elif token == "[":
                brackets += 1
            elif token == "]":
                brackets -= 1
            elif not parens and not brackets and token == "{":
                item_end = _brace_end(clean, pos)
                break
            elif not parens and not brackets and token == ";":
                item_end = pos + 1
                break
        _blank(comments, attr.start(), item_end)
        _blank(code, attr.start(), item_end)
    return "".join(comments if keep_strings else code)


def _static_body(source, name):
    code = rust_code(source, keep_strings=True)
    pattern = (r'\b(?:static|const)\s+' + re.escape(name)
               + r'\s*:\s*\[[^\]]*;\s*(\w+)\s*\]\s*=\s*\[')
    found = list(re.finditer(pattern, code))
    if len(found) != 1:
        raise AuditError(f"expected exactly one {name} declaration, found {len(found)}")
    declaration = found[0]
    end = _brace_end(code, declaration.end() - 1, "[", "]")
    count = declaration.group(1)
    if not count.isdecimal():
        constant = re.search(r'\bconst\s+' + re.escape(count)
                             + r'\s*:\s*\w+\s*=\s*(\d+)\s*;', code)
        if not constant:
            raise AuditError(f"unable to resolve {name} length constant {count}")
        count = constant.group(1)
    return code[declaration.end():end - 1], int(count)


def _literal(token):
    value = ast.literal_eval(token)
    if not isinstance(value, str):
        raise AuditError(f"invalid string literal: {token}")
    return value


def _number(token):
    token = token.replace("_", "")
    return int(token, 16 if token.startswith("0x") else 10)


def _entries(source, name, entry_pattern, decode):
    body, declared = _static_body(source, name)
    found = []
    offset = 0
    for match in re.finditer(entry_pattern, body):
        if body[offset:match.start()].strip(" \r\n\t,"):
            raise AuditError(f"unexpected token in {name}: {body[offset:match.start()][:70]!r}")
        found.append(decode(match))
        offset = match.end()
    if body[offset:].strip(" \r\n\t,"):
        raise AuditError(f"unparsed entries in {name}: {body[offset:][:70]!r}")
    if len(found) != declared:
        raise AuditError(f"{name} declared {declared} entries, parsed {len(found)}")
    return found


def parse_authority(root):
    """Return the complete packet table and index-addressed registries."""
    source = (Path(root) / "proto/src/data_v767.rs").read_text(encoding="utf-8")
    packet_pattern = (r'\(\s*PacketPhase::(\w+)\s*,\s*PacketDir::(\w+)\s*,\s*'
                      + f'({STRING})' + r'\s*,\s*(' + INTEGER + r')\s*\)')
    packets = _entries(source, "V767_PACKETS", packet_pattern, lambda m: {
        "phase": m.group(1), "direction": m.group(2),
        "id": _number(m.group(4)), "name": _literal(m.group(3)),
    })
    registries = {}
    for registry, declaration in REGISTRIES.items():
        names = _entries(source, declaration, STRING, lambda m: _literal(m.group()))
        registries[registry] = [{"id": index, "name": name}
                                for index, name in enumerate(names)]
    return packets, registries


def validate_authority(packets, registries):
    errors = []
    ids = set()
    names = set()
    for packet in packets:
        phase, direction, number, name = (packet.get(key) for key in
                                          ("phase", "direction", "id", "name"))
        label = f"packet {phase}/{direction}/{number}/{name}"
        if phase not in PHASES or direction not in DIRECTIONS:
            errors.append(f"{label}: invalid phase or direction")
        if type(number) is not int or not 0 <= number <= 0xffffffff:
            errors.append(f"{label}: invalid protocol ID")
        if not isinstance(name, str) or not re.fullmatch(r'[a-z][a-z0-9_]*', name):
            errors.append(f"{label}: invalid packet name")
        key = (phase, direction, number)
        named = (phase, direction, name)
        if key in ids:
            errors.append(f"{label}: duplicate packet ID")
        if named in names:
            errors.append(f"{label}: duplicate packet name")
        ids.add(key)
        names.add(named)
    for registry, entries in registries.items():
        seen_ids = set()
        seen_names = set()
        for index, entry in enumerate(entries):
            number, name = entry.get("id"), entry.get("name")
            label = f"minecraft:{registry}[{index}]"
            if type(number) is not int or number != index or number in seen_ids:
                errors.append(f"{label}: noncontiguous or duplicate ID {number}")
            if not isinstance(name, str) or not re.fullmatch(r'minecraft:[a-z0-9_./-]+', name):
                errors.append(f"{label}: malformed name {name!r}")
            if name in seen_names:
                errors.append(f"{label}: duplicate name {name}")
            seen_ids.add(number)
            seen_names.add(name)
    return errors


def _read(root, relative):
    path = Path(root) / relative
    return rust_code(path.read_text(encoding="utf-8"), keep_strings=True) if path.exists() else ""


def _function(source, name):
    code = rust_code(source) if '//' in source or '/*' in source else source
    found = re.search(r'\bfn\s+' + re.escape(name) + r'\s*\(', code)
    if not found:
        return ""
    opening = code.find("{", found.end())
    if opening == -1:
        return ""
    end = _brace_end(code, opening)
    return source[found.start():end]


def _literal_arms(source, match_expression, kind="name"):
    """Read only direct match arms at brace depth one."""
    code = rust_code(source)
    found = re.search(r'\bmatch\s+' + match_expression + r'\s*\{', code)
    if not found:
        return set()
    start = found.end() - 1
    end = _brace_end(code, start)
    values = set()
    depth = 0
    arm = re.compile((INTEGER if kind == "id" else STRING) + r'\s*=>')
    for position in range(start, end):
        if code[position] == "{":
            depth += 1
        elif code[position] == "}":
            depth -= 1
        if depth != 1:
            continue
        if position > start and (code[position - 1].isalnum() or code[position - 1] == "_"):
            continue
        # The token must start at this position, and not inside an identifier.
        match = arm.match(code if kind == "id" else source, position)
        if match:
            token = match.group().split("=>", 1)[0].strip()
            values.add(_number(token) if kind == "id" else _literal(token))
    return values


def _send_ids(source):
    code = rust_code(source)
    ids = set()
    for match in re.finditer(r'\bsend_packet\s*\(', code):
        opening = match.end() - 1
        end = _brace_end(code, opening, "(", ")")
        stack = []
        arguments = []
        argument_start = opening + 1
        for position in range(opening, end):
            token = code[position]
            if token in "([{":
                stack.append(token)
            elif token in ")]}":
                if stack:
                    stack.pop()
            elif token == "," and stack == ["("]:
                arguments.append(code[argument_start:position].strip())
                argument_start = position + 1
        arguments.append(code[argument_start:end - 1].strip())
        if len(arguments) >= 3 and re.fullmatch(INTEGER, arguments[1]):
            ids.add(_number(arguments[1]))
    return ids


def _client_packets(source):
    client_sends = set()
    for match in re.finditer(r'\bsend_named\s*\(\s*PacketPhase::(\w+)\s*,\s*('
                             + STRING + r')\s*,', source):
        client_sends.add((match.group(1), _literal(match.group(2))))
    # Public encoder entry points return (packet name, encoded payload) for
    # callers that send through c2s_id/send_raw. Count a concrete encoder body.
    for found in re.finditer(r'\b(?:pub\s+)?fn\s+(encode_[a-z0-9_]+)\s*\(', rust_code(source)):
        encoder = _function(source, found.group(1))
        for item in re.finditer(r'\(\s*(' + STRING + r')\s*,', encoder):
            client_sends.add(("Play", _literal(item.group(1))))
    client_receives = set()
    for phase in PHASES:
        # A phase-scoped name comparison or match arm identifies a decoder.
        for found in re.finditer(r'\bs2c_name\s*\(\s*PacketPhase::' + phase
                                 + r'\s*,\s*pid\s*\)\.as_deref\s*\(\s*\)', source):
            tail = source[found.end():]
            if tail.startswith(" == Some(") or tail.startswith(" != Some("):
                named = re.match(r'\s*[!=]=\s*Some\s*\(\s*(' + STRING + r')\s*\)', tail)
                if named:
                    client_receives.add((phase, _literal(named.group(1))))
            elif re.search(r'\bmatch\s+(?:client|self)\.\s*$', source[max(0, found.start() - 50):found.start()]):
                opening = source.find("{", found.end())
                if opening != -1:
                    end = _brace_end(rust_code(source), opening)
                    for arm in re.finditer(r'\bSome\s*\(\s*(' + STRING + r')\s*\)\s*(?:\||=>)', source[opening:end]):
                        client_receives.add((phase, _literal(arm.group(1))))
    if len(re.findall(r'\bdecode_extended_play_event\s*\(', rust_code(source))) > 1:
        decoder = _function(source, "decode_extended_play_event")
        # The body has `Ok(Some(match name { ... }))`; only named arm cases count.
        client_receives.update(("Play", name) for name in _literal_arms(decoder, r'name'))
    return client_sends, client_receives


def _server_packets(root, source, main_source):
    code = rust_code(source)
    handle = _function(source, "handle_inner")
    play_arms = _literal_arms(handle, r'pid', kind="id")
    c2s = {("Play", number) for number in play_arms}
    if re.search(r'\bhandshake_parts\s*\(\s*&hs\s*\)', rust_code(handle)) and re.search(r'\bif\s+id\s*!=\s*0\b', rust_code(handle)):
        c2s.add(("Handshake", 0))
    status = handle.split("if next == 1", 1)[-1].split("if next != 2", 1)[0] if "if next == 1" in handle else ""
    for number in (0, 1):
        if re.search(r'\b(?:if\s+let|match)\s+Some\s*\(\s*\(\s*' + str(number) + r'\s*,', rust_code(status)):
            c2s.add(("Status", number))
    login = handle.split("if next != 2", 1)[-1].split("let cfg = load_config_packets()", 1)[0]
    if re.search(r'\blid\s*!=\s*0\b', rust_code(login)) and re.search(r'\blogin_identity\s*\(', rust_code(login)):
        c2s.add(("Login", 0))
    if re.search(r'\bSome\s*\(\s*\(\s*3\s*,', rust_code(login)):
        c2s.add(("Login", 3))
    config = handle.split("let cfg = load_config_packets()", 1)[-1].split("let player_uuid", 1)[0]
    for number in re.findall(r'\bSome\s*\(\s*\(\s*(' + INTEGER + r')\s*,', rust_code(config)):
        c2s.add(("Configuration", _number(number)))

    sent = set()
    for number in _send_ids(status):
        sent.add(("Status", number))
    for number in _send_ids(login):
        sent.add(("Login", number))
    for number in _send_ids(config):
        sent.add(("Configuration", number))
    # Other methods in play_protocol.rs, and the post-config part of handle_inner.
    body_start = code.find("fn handle_inner(")
    if body_start != -1:
        opening = code.find("{", body_start)
        handle_end = _brace_end(code, opening)
        prefix = source[:body_start]
        suffix = source[handle_end:]
        within_play = handle.split("let player_uuid", 1)[-1]
    else:
        prefix, suffix, within_play = source, "", ""
    for number in _send_ids(prefix) | _send_ids(suffix) | _send_ids(within_play) | _send_ids(main_source):
        sent.add(("Play", number))
    # Phase-specific helper writers are called from production paths.
    if re.search(r'\bnegotiate_compression\s*\(', rust_code(handle)) and re.search(r'\bsend_packet\s*\(\s*s\s*,\s*0x03\s*,', code):
        sent.add(("Login", 3))
    if re.search(r'\blogin_disconnect\s*\(', rust_code(handle)):
        sent.add(("Login", 0))
    # The config stream is dynamic. Attribute only IDs seen in concrete
    # production writes, leaving captured file contents unverified.
    for found in re.finditer(r'\bencode_play_([a-z0-9_]+)\s*\(', code):
        name = found.group(1)
        if code[max(0, found.start() - 4):found.start()].strip().endswith("fn"):
            continue
        sent.add(("PlayName", name))
    return c2s, sent


def _registry_evidence(root, registries):
    evidence = {key: {"server": set(), "client": set()} for key in registries}
    errors = []
    blocks = registries["block"]
    items = registries["item"]
    proto = _read(root, "proto/src/table.rs")
    server_items = _read(root, "src/menus_and_items.rs")
    if re.search(r'\bRegistry::from_dense_names\s*\(\s*&V767_BLOCKS\s*\)', rust_code(proto)):
        evidence["block"]["client"].update(range(len(blocks)))
    if re.search(r'\bRegistry::from_dense_names\s*\(\s*&V767_ITEMS\s*\)', rust_code(proto)):
        evidence["item"]["client"].update(range(len(items)))
    asset_path = Path(root) / "assets/blocks_1211.json"
    state_path = Path(root) / "assets/block_states_1211.json"
    if asset_path.exists() and state_path.exists():
        try:
            asset = json.loads(asset_path.read_text(encoding="utf-8"))
            states = json.loads(state_path.read_text(encoding="utf-8"))["blocks"]
            if len(asset) != len(blocks) or len(states) != len(blocks):
                errors.append(f"block asset count mismatch: v767={len(blocks)} blocks={len(asset)} states={len(states)}")
            next_state = 0
            for index, row in enumerate(blocks):
                if index >= len(asset) or index >= len(states):
                    continue
                block = asset[index]
                state = states[index]
                if block.get("id") != index or block.get("name") != row["name"].removeprefix("minecraft:"):
                    errors.append(f"block asset mismatch at {index}: {row['name']}")
                if not isinstance(state, list) or len(state) != 3 or state[0] != row["name"] or state[1] != next_state or type(state[2]) is not int or state[2] <= 0:
                    errors.append(f"block state asset mismatch at {index}: {row['name']}")
                else:
                    next_state += state[2]
                if block.get("id") == index and block.get("name") == row["name"].removeprefix("minecraft:") and state[0] == row["name"]:
                    evidence["block"]["server"].add(index)
        except (OSError, ValueError, TypeError, KeyError, IndexError) as exc:
            errors.append(f"invalid block assets: {exc}")
    else:
        errors.append("missing block assets/blocks_1211.json or assets/block_states_1211.json")
    try:
        names = _entries(server_items, "ITEM_NAMES", STRING, lambda m: _literal(m.group()))
        for item in items:
            if item["id"] < len(names) and item["name"] == "minecraft:" + names[item["id"]]:
                evidence["item"]["server"].add(item["id"])
            else:
                errors.append(f"item table mismatch at {item['id']}: {item['name']}")
    except AuditError as exc:
        errors.append(str(exc))

    entity_source = _read(root, "src/entity_registry.rs")
    client_entity = _read(root, "client-port/src/entity_rig.rs")
    for match in re.finditer(r'\bentity_spec_1211!\s*\(\s*(' + INTEGER + r')\s*,\s*(' + STRING + r')', entity_source):
        number = _number(match.group(1))
        if number < len(registries["entity_type"]) and registries["entity_type"][number]["name"] == "minecraft:" + _literal(match.group(2)):
            evidence["entity_type"]["server"].add(number)
    # Natural spawning handles a subset of entity types. Check its explicit
    # dispatch names against the authority without requiring every entity to
    # be naturally spawnable (e.g. projectiles and display entities).
    natural_spawner = _read(root, "src/natural_spawner.rs")
    if natural_spawner:
        spawn_names = _literal_arms(_function(natural_spawner, "from_id"), r'id')
        registered_names = {entry["name"] for entry in registries["entity_type"]}
        for name in sorted(spawn_names - registered_names):
            errors.append(f"natural spawner references unknown entity type: {name}")
    try:
        body, _ = _static_body(client_entity, "ENTITIES")
        for index, match in enumerate(re.finditer(r'\(\s*(' + STRING + r')\s*,\s*[-\d.]+\s*,\s*[-\d.]+\s*\)', body)):
            if index < len(registries["entity_type"]) and registries["entity_type"][index]["name"] == _literal(match.group(1)):
                evidence["entity_type"]["client"].add(index)
    except AuditError as exc:
        errors.append(str(exc))

    server_menus = server_items + "\n" + _read(root, "src/play_protocol.rs")
    # Menu IDs appear as the kind of a live-opened screen or as an explicitly
    # returned (kind, title) pair in a concrete menu handler.
    live = _function(server_items, "screen")
    server_menu_ids = {_number(number) for number in re.findall(r'=>\s*\(\s*(' + INTEGER + r')\s*,\s*' + STRING, live)}
    for match in re.finditer(r'\bfn\s+screen\s*\([^)]*\)\s*->\s*\(\s*i64\s*,', rust_code(server_items)):
        opening = rust_code(server_items).find("{", match.end())
        if opening != -1:
            section = server_items[opening:_brace_end(rust_code(server_items), opening)]
            for number in re.findall(r'\(\s*(' + INTEGER + r')\s*,\s*' + STRING, section):
                server_menu_ids.add(_number(number))
    workstation_source = _read(root, "src/workstations_and_mounts.rs")
    workstation_code = rust_code(workstation_source)
    menu_constants = {match.group(1): _number(match.group(2)) for match in re.finditer(
        r'\bconst\s+(WM_MENU_[A-Z_]+)\s*:\s*i64\s*=\s*(' + INTEGER + r')\s*;', workstation_code)}
    # LiveMenu dispatches to workstation screen() methods whose returned kind
    # is a named constant in the same production module.
    for helper in re.findall(r'\bSelf::\w+\s*\([^)]*\)\s*=>\s*(\w+)::screen\s*\(\)', live):
        implementation = re.search(r'\bimpl\s+' + re.escape(helper) + r'\s*\{', workstation_code)
        if not implementation:
            continue
        body = workstation_source[implementation.start():_brace_end(workstation_code, implementation.end() - 1)]
        screen = _function(body, "screen")
        for name in re.findall(r'\(\s*(WM_MENU_[A-Z_]+)\s*,\s*' + STRING, screen):
            if name in menu_constants:
                server_menu_ids.add(menu_constants[name])
    # Explicit, production menu-kind writes at the server open-screen boundary.
    for number in server_menu_ids:
        if number < len(registries["menu"]):
            evidence["menu"]["server"].add(number)
    ui = _read(root, "client-ui/src/container_screen.rs")
    try:
        body, _ = _static_body(ui, "ALL_VANILLA_MENUS")
        for match in re.finditer(r'\b(?:chest_schema|standard_schema)\s*\(\s*(' + INTEGER + r')\s*,\s*(' + STRING + r')', body):
            number = _number(match.group(1))
            if number < len(registries["menu"]) and registries["menu"][number]["name"] == _literal(match.group(2)):
                evidence["menu"]["client"].add(number)
    except AuditError as exc:
        errors.append(str(exc))

    comp_source = _read(root, "proto/src/item_components.rs")
    decoder = _function(comp_source, "decode_from_reader")
    decoded = {match.group(1) for match in re.finditer(r'(' + STRING + r')\s*=>', decoder)}
    decoded_names = {_literal(token) for token in decoded}
    for entry in registries["data_component_type"]:
        if entry["name"] in decoded_names:
            evidence["data_component_type"]["client"].add(entry["id"])
    server_code = rust_code(server_menus)
    for match in re.finditer(r'\bconst\s+((?:WM_)?COMP_[A-Z_0-9]+)\s*:\s*i64\s*=\s*(' + INTEGER + r')\s*;', server_code):
        number = _number(match.group(2))
        if number < len(registries["data_component_type"]) and len(re.findall(r'\b' + re.escape(match.group(1)) + r'\b', server_code)) > 1:
            evidence["data_component_type"]["server"].add(number)
    return evidence, errors


def audit(root=ROOT):
    root = Path(root)
    packets, registries = parse_authority(root)
    integrity = validate_authority(packets, registries)
    server = _read(root, "src/play_protocol.rs")
    main = _read(root, "src/main.rs")
    client = _read(root, "client-app/src/protocol.rs")
    server_recv, server_sent = _server_packets(root, server, main)
    client_sent, client_recv = _client_packets(client)
    evidence, asset_errors = _registry_evidence(root, registries)
    tables = {}
    for phase in PHASES:
        for direction in DIRECTIONS:
            key = f"{phase}/{direction}"
            entries = [packet for packet in packets if packet["phase"] == phase and packet["direction"] == direction]
            rows = []
            for packet in entries:
                number, name = packet["id"], packet["name"]
                packet_key = (phase, number)
                if direction == "Serverbound":
                    server_ok = packet_key in server_recv
                    client_ok = (phase, name) in client_sent
                else:
                    server_ok = packet_key in server_sent or ("PlayName", name) in server_sent and phase == "Play"
                    client_ok = (phase, name) in client_recv
                rows.append({**packet, "server_covered": server_ok, "client_covered": client_ok})
            tables[key] = _table(rows)
    for registry, entries in registries.items():
        rows = [{**row, "server_covered": row["id"] in evidence[registry]["server"],
                 "client_covered": row["id"] in evidence[registry]["client"]} for row in entries]
        tables[f"minecraft:{registry}"] = _table(rows)
    return {"protocol": 767, "source": "proto/src/data_v767.rs",
            "integrity_errors": integrity, "asset_errors": asset_errors, "tables": tables}


def _table(rows):
    total = len(rows)
    server = sum(row["server_covered"] for row in rows)
    client = sum(row["client_covered"] for row in rows)
    gaps = [row for row in rows if not row["server_covered"] or not row["client_covered"]]
    return {"total": total, "server_covered": server, "client_covered": client,
            "server_percent": round(100 * server / total, 2) if total else 100.0,
            "client_percent": round(100 * client / total, 2) if total else 100.0,
            "gap_count": len(gaps), "missing_server": [row for row in rows if not row["server_covered"]],
            "missing_client": [row for row in rows if not row["client_covered"]], "gaps": gaps}


def display(report):
    print("Table | Total in v767 | Server Covered | Client Covered | Gaps")
    print("--- | ---: | ---: | ---: | ---:")
    for name, table in report["tables"].items():
        print(f"{name} | {table['total']} | {table['server_covered']} ({table['server_percent']:.2f}%) | "
              f"{table['client_covered']} ({table['client_percent']:.2f}%) | {table['gap_count']}")
    for side in ("missing_server", "missing_client"):
        print(f"\n{side} (all entries)")
        for name, table in report["tables"].items():
            missing = table[side]
            if missing:
                print(f"{name}: " + ", ".join(f"{entry['id']:#04x} {entry['name']}" for entry in missing))
    for error in report["integrity_errors"] + report["asset_errors"]:
        print(f"INTEGRITY: {error}", file=sys.stderr)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help="repository root for audit")
    parser.add_argument("--json", action="store_true", help="full machine-readable coverage diff")
    parser.add_argument("--verify-registry-integrity", action="store_true", help="exit 1 on malformed canonical tables or block/item assets")
    args = parser.parse_args(argv)
    try:
        report = audit(args.root)
    except (AuditError, OSError, ValueError, SyntaxError, TypeError) as exc:
        print(json.dumps({"error": str(exc)}) if args.json else f"AUDIT FAILED: {exc}", file=sys.stdout if args.json else sys.stderr)
        return 1
    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
    else:
        display(report)
    return int(args.verify_registry_integrity and bool(report["integrity_errors"] or report["asset_errors"]))


if __name__ == "__main__":
    sys.exit(main())
