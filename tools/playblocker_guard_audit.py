#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Fast production hotpath crash ratchet and cross-crate wire identity guard."""
import argparse
import json
from pathlib import Path
import re
import sys

from hardcode_guard_audit import production, blank

ROOT = Path(__file__).resolve().parents[1]
BASELINE = 'audit/playblocker_guard_baseline.json'
HOTPATHS = (
    'proto/src/seamless_portal.rs', 'server-files/src/claims.rs',
    'server-files/src/custom_portal.rs', 'server-files/src/sd_notify.rs',
    'core/src/linux_desktop.rs', 'core/src/os_desktop.rs',
    'src/ingress_guard.rs', 'src/play_protocol.rs', 'client-gpu/src/live_session.rs',
)
CRASH = re.compile(r'\b(?:todo|unimplemented|panic)\s*!\s*\(|\.\s*unwrap\s*\(\s*\)')
# Each entry names the canonical owner and the actual second wire implementation.
WIRES = (
    ('megu:portal_seamless_v1', 'MSP1', 'proto/src/seamless_portal.rs', 'client-gpu/src/live_session.rs'),
    ('megu:subworld_sync_v1', 'SW6D', 'create-adapter/src/aeronautics.rs', 'client-gpu/src/subworld_sync.rs'),
    ('megu:subworld_helm_v1', 'SHLM', 'client-gpu/src/subworld_sync.rs', 'create-adapter/src/aeronautics.rs'),
    ('megu:mod_sync_v1', 'MSY1', 'mod-adapter-core/src/mod_sync.rs', 'src/live_mod_bridge.rs'),
    ('megu:mod_registry_sync_v1', 'MRS1', 'mod-adapter-core/src/live_registry_bridge.rs', 'client-app/src/mod_assets_meta.rs'),
)
MAGIC_DEF = re.compile(r'\b(?:pub\s+)?const\s+\w*MAGIC\s*:\s*(?:&)?\s*\[u8\s*;\s*4\]\s*=\s*\*?\s*b"([A-Za-z0-9]{4})"')


def syntax_only(text):
    """Use the same comment/test removal as the existing hardcode guard, then mask strings."""
    visible, literals = production(text)
    chars = list(visible)
    for start, end, _ in literals:
        chars[start:end] = blank(visible[start:end])
    return ''.join(chars), visible


def audit(root, baseline):
    findings = []
    counts = {}
    source = {}
    parsed = {}
    needed = set(HOTPATHS)
    canonical_subworld = 'proto/src/subworld_wire.rs'
    canonical_mod_wire = 'proto/src/mod_wire.rs'
    needed.add(canonical_subworld)
    needed.add(canonical_mod_wire)
    for _channel, _magic, owner, peer in WIRES:
        needed.update((owner, peer))
    for path in sorted(needed):
        source[path] = (root / path).read_text(encoding='utf-8')
        parsed[path] = syntax_only(source[path])
    for path in HOTPATHS:
        text = source[path]
        syntax, _ = parsed[path]
        matches = list(CRASH.finditer(syntax))
        counts[path] = len(matches)
        if len(matches) > baseline[path]:
            for match in matches[baseline[path]:]:
                findings.append(f'{path}:{text.count(chr(10), 0, match.start()) + 1}: new live-hotpath crash primitive')
    # Wire definitions are scoped to the producer/consumer implementations, not
    # README examples or Rust test modules. Identical copies are tolerated; a
    # conflicting definition or a missing consumer is not.
    for channel, magic, owner, peer in WIRES:
        owner_syntax, owner_visible = parsed[owner]
        peer_syntax, peer_visible = parsed[peer]
        if f'"{channel}"' not in owner_visible:
            findings.append(f'{owner}: missing {channel} channel definition')
        if channel == 'megu:portal_seamless_v1' and (
                'SEAMLESS_PORTAL_CHANNEL' not in peer_syntax or
                'SEAMLESS_PORTAL_CHANNEL' not in owner_syntax):
            findings.append(f'{peer}: missing seamless portal client consumer/codec binding')
        # The peer may import the canonical definition instead of copying it.
        if channel.startswith('megu:subworld_') and f'"{channel}"' not in peer_visible:
            findings.append(f'{peer}: missing {channel} peer channel')
        if not channel.startswith('megu:subworld_') and f'b"{magic}"' not in owner_visible:
            findings.append(f'{owner}: missing {magic} wire magic')
        if channel == 'megu:mod_registry_sync_v1' and (
                f'b"{magic}"' not in peer_visible
                and 'megu_proto::mod_wire::decode_mrs1_frame' not in peer_syntax):
            findings.append(f'{peer}: missing {magic} decoder or canonical mod_wire delegation')
        # SW6D/SHLM are canonicalized in proto/src/subworld_wire.rs. Their
        # endpoint sharing and binary layout are checked by the dedicated
        # contract assertions below rather than by copied per-endpoint magic.
        if channel.startswith('megu:subworld_'):
            continue
        # Check all four-byte magic definitions in both crates for collisions
        # with this protocol's symbol, including an accidentally changed copy.
        names = {
            'MSP1': ('SEAMLESS_PORTAL_MAGIC',), 'SW6D': ('SUBWORLD_SYNC_MAGIC',),
            'SHLM': ('SUBWORLD_HELM_MAGIC',), 'MSY1': ('MSY1_MAGIC',),
            'MRS1': ('WIRE_MAGIC',),
        }[magic]
        definition = re.compile(r'\b(?:pub\s+)?const\s+(?:' + '|'.join(names) +
                                r')\s*:\s*(?:&)?\s*\[u8\s*;\s*4\]\s*=\s*\*?\s*b"([A-Za-z0-9]{4})"')
        for path in (owner, peer):
            _, visible = parsed[path]
            for found in definition.finditer(visible):
                if found.group(1) != magic:
                    findings.append(f'{path}: conflicting {magic} magic: {found.group(1)}')
        if not definition.search(owner_visible):
            findings.append(f'{owner}: missing canonical {magic} definition')
        # Pin the known frame versions; MRS1 deliberately supports both v1
        # and v2. SW6D/SHLM versions live in the canonical proto module and
        # are pinned by the dedicated checks below.
        versions = {
            'MSP1': ('SEAMLESS_PORTAL_VERSION',),
            'MRS1': ('WIRE_VERSION', 'WIRE_VERSION_V2'),
        }.get(magic, ())
        for index, name in enumerate(versions, 1):
            if not re.search(r'\bconst\s+' + name + r'\s*:\s*u(?:8|16)\s*=\s*' + str(index) + r'\s*;', owner_syntax):
                findings.append(f'{owner}: missing canonical {magic} {name}={index}')
        # A different constant with the same four bytes is another protocol
        # claiming this identity (but same-named cross-crate copies are valid).
        for path in (owner, peer):
            _, visible = parsed[path]
            for found in MAGIC_DEF.finditer(visible):
                if found.group(1) == magic and not definition.fullmatch(found.group()):
                    findings.append(f'{path}: duplicate {magic} magic definition')

    # Dedicated cross-endpoint contract checks for the alpha5 subworld codec.
    # The server includes the canonical implementation by path and the GPU
    # client imports the same proto module; requiring those bindings prevents
    # a return to two superficially similar but byte-incompatible codecs.
    canonical_path = 'proto/src/subworld_wire.rs'
    canonical_syntax, canonical_visible = parsed[canonical_path]
    server_path = 'create-adapter/src/aeronautics.rs'
    client_path = 'client-gpu/src/subworld_sync.rs'
    server_syntax, server_visible = parsed[server_path]
    client_syntax, client_visible = parsed[client_path]
    subworld_checks = (
        (r'pub\s+const\s+SW6D_MAGIC\s*:\s*\[u8\s*;\s*4\]\s*=\s*\*?\s*b"SW6D"', 'missing canonical SW6D magic'),
        (r'pub\s+const\s+SW6D_VERSION\s*:\s*u8\s*=\s*2\s*;', 'missing canonical SW6D version 2'),
        (r'pub\s+const\s+SHLM_MAGIC\s*:\s*\[u8\s*;\s*4\]\s*=\s*\*?\s*b"SHLM"', 'missing canonical SHLM magic'),
        (r'pub\s+const\s+SHLM_VERSION\s*:\s*u8\s*=\s*2\s*;', 'missing canonical SHLM version 2'),
    )
    for pattern, message in subworld_checks:
        if not re.search(pattern, canonical_visible):
            findings.append(f'{canonical_path}: {message}')
    # Pin the wire-owned semantic fields, not merely the magic bytes.
    for field in ('dimension_id', 'revision', 'pivot', 'voxels'):
        if not re.search(r'pub\s+' + field + r'\s*:', canonical_syntax):
            findings.append(f'{canonical_path}: SW6D record missing {field}')
    for token in ('encode_sw6d_frame', 'decode_sw6d_frame', 'encode_shlm_command', 'decode_shlm_command'):
        if token not in canonical_syntax:
            findings.append(f'{canonical_path}: missing {token}')
    if '#[path = "../../proto/src/subworld_wire.rs"]' not in server_visible:
        findings.append(f'{server_path}: server no longer shares proto/src/subworld_wire.rs')
    for token in ('subworld_wire::encode_sw6d_frame', 'subworld_wire::decode_sw6d_frame',
                  'subworld_wire::encode_shlm_command', 'subworld_wire::decode_shlm_command'):
        if token not in server_syntax:
            findings.append(f'{server_path}: server does not use canonical {token}')
    if 'use megu_proto::subworld_wire' not in client_visible:
        findings.append(f'{client_path}: client does not import canonical proto subworld module')
    for token in ('decode_sw6d_frame', 'encode_shlm_command', 'decode_shlm_command'):
        if token not in client_syntax:
            findings.append(f'{client_path}: client does not use canonical {token}')
    if 'pivot_adjusted_position' not in client_syntax:
        findings.append(f'{client_path}: client does not apply SW6D pivot semantics')

    # MRS1 v2 acceptance is part of the wire contract: the client must delegate
    # to the canonical proto/src/mod_wire.rs decoder (which accepts both v1 and
    # v2 and preserves/derives lighting facts) rather than a v1-only parser.
    registry_peer = 'client-app/src/mod_assets_meta.rs'
    registry_syntax, _ = parsed[registry_peer]
    mod_wire_syntax, mod_wire_visible = parsed[canonical_mod_wire]
    if 'megu_proto::mod_wire::decode_mrs1_frame' not in registry_syntax:
        findings.append(f'{registry_peer}: client does not delegate MRS1 decoding to canonical proto/src/mod_wire.rs')
    if 'facts_byte' not in registry_syntax or 'derive_mrs1_facts_byte' not in registry_syntax:
        findings.append(f'{registry_peer}: MRS1 client wrapper does not preserve/derive lighting facts')
    for pattern, message in (
        (r'pub\s+const\s+MRS1_MAGIC\s*:\s*\[u8\s*;\s*4\]\s*=\s*\*?\s*b"MRS1"', 'missing canonical MRS1 magic'),
        (r'pub\s+const\s+MRS1_VERSION_V1\s*:\s*u16\s*=\s*1\s*;', 'missing canonical MRS1 v1 constant'),
        (r'pub\s+const\s+MRS1_VERSION_V2\s*:\s*u16\s*=\s*2\s*;', 'missing canonical MRS1 v2 constant'),
    ):
        if not re.search(pattern, mod_wire_visible):
            findings.append(f'{canonical_mod_wire}: {message}')
    for token in ('decode_mrs1_frame', 'derive_mrs1_facts_byte', 'facts_bytes'):
        if token not in mod_wire_syntax:
            findings.append(f'{canonical_mod_wire}: MRS1 canonical codec missing {token}')
    return counts, findings


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--update-baseline', action='store_true')
    args = parser.parse_args(argv)
    path = args.root / BASELINE
    try:
        if path.exists():
            data = json.loads(path.read_text(encoding='utf-8'))
            baseline = data['counts']
            if set(baseline) != set(HOTPATHS) or any(type(v) is not int or v < 0 for v in baseline.values()):
                raise ValueError('invalid baseline counts or missing hotpath')
        elif args.update_baseline:
            baseline = {p: len(CRASH.findall(syntax_only((args.root / p).read_text(encoding='utf-8'))[0]))
                        for p in HOTPATHS}
        else:
            raise ValueError(f'missing baseline: {path}')
        counts, findings = audit(args.root, baseline)
        if args.update_baseline and not findings:
            # Bootstrap existing debt only on first creation; updates cannot raise ceilings.
            new = {p: min(counts[p], baseline[p]) for p in HOTPATHS}
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({'counts': new}, indent=2, sort_keys=True) + '\n', encoding='utf-8')
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print(f'playblocker guard: {exc}', file=sys.stderr)
        return 2
    print(f"playblocker guard: {'FAIL' if findings else 'PASS'} ({len(HOTPATHS)} hotpaths, {len(findings)} findings)")
    for finding in findings:
        print('  ' + finding)
    return 1 if findings else 0


if __name__ == '__main__':
    sys.exit(main())
