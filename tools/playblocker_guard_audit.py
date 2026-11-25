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
        if f'b"{magic}"' not in owner_visible:
            findings.append(f'{owner}: missing {magic} wire magic')
        if channel == 'megu:mod_registry_sync_v1' and f'b"{magic}"' not in peer_visible:
            findings.append(f'{peer}: missing {magic} decoder')
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
        # and v2, while the other three versioned frames use one u8 v1.
        versions = {
            'MSP1': ('SEAMLESS_PORTAL_VERSION',),
            'SW6D': ('SUBWORLD_SYNC_VERSION',),
            'SHLM': ('VERSION',),
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
