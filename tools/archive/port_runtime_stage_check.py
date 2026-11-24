#!/usr/bin/env python3
"""Check frozen, uninterrupted and restarted port dumps against native saves."""
import argparse
import hashlib
import json
from pathlib import Path
import re

from carver_diff import load_states
from forest_stage_check import raw, require, world
from vanilla_java import JAR_SHA1


def digest(path, algorithm='sha256'):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, algorithm).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path, help='completed frozen native fixture')
    parser.add_argument('resume', type=Path, help='completed resumed native fixture')
    parser.add_argument('uninterrupted', type=Path, help='port runtime output directory')
    parser.add_argument('early', type=Path, help='port five-tick save directory')
    parser.add_argument('restarted', type=Path, help='port 5+695 restart output directory')
    parser.add_argument('prior_native', type=Path, help='native dump previously compared to canonical')
    parser.add_argument('prior_receipt', type=Path, help='receipt for the canonical comparison')
    parser.add_argument('binary', type=Path)
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    for path in (args.source, args.resume):
        require((path / 'finished.txt').exists(), f'fixture unfinished: {path}')
    require(digest(args.source / 'server.jar', 'sha1') == JAR_SHA1, 'native server hash mismatch')
    boundary = json.loads((args.source / 'boundary.json').read_text())
    require(boundary['mode'] == 'frozen' and boundary['game_times'][0] == boundary['game_times'][1]
            and boundary['runtime_writes'] == 0, 'invalid native frozen boundary')
    states = load_states()
    frozen = world(args.source / 'world', states)
    require(frozen == raw(args.source / 'native.bin'), 'native frozen dump stale')
    require(frozen == world(args.resume / 'snapshot-frozen', states), 'resumed native pre-state mismatch')
    ticked = world(args.resume / 'snapshot-ticked', states)
    require(ticked == raw(args.prior_native), 'resumed native differs from prior canonical-matched control')
    prior = json.loads(args.prior_receipt.read_text())
    require(prior['cells_compared'] == len(frozen) and prior['fresh_ticked_native_vs_canonical'] == 0
            and prior['sha256']['native_ticked_dump'] == digest(args.prior_native),
            'prior native dump lacks a matching canonical comparison receipt')
    resume_log = (args.resume / 'run.log').read_text()
    times = list(map(int, re.findall(r'The time is (\d+)', resume_log)))
    require(len(times) == 2 and times[1] - times[0] == 700, f'wrong native tick interval: {times}')
    require(raw(args.uninterrupted / 'start.bin') == frozen, 'port generation mismatch')
    require(raw(args.uninterrupted / 'end.bin') == ticked, 'uninterrupted runtime mismatch')
    require(raw(args.restarted / 'end.bin') == ticked, 'restarted runtime mismatch')
    for directory, steps in ((args.uninterrupted, 700), (args.early, 5), (args.restarted, 695)):
        path = directory / 'run.log'
        log = path.read_text()
        require('[fluid] deferred' not in log, f'unsupported fluid encountered: {path}')
        require(f'runtime diagnostic: {steps} steps;' in log and 'full_tick=true' in log,
                f'wrong runtime stage: {path}')
        require(f'population plan {args.source / "order.tsv"}: 25 source chunks executed' in log,
                f'wrong population order: {path}')
    require((args.early / 'world_overlay.bin').read_bytes() == (args.restarted / 'input-world.bin').read_bytes(),
            'restarted input does not match the five-tick save')
    paths = {'native_frozen': args.source / 'native.bin',
             'native_ticked_region': args.resume / 'snapshot-ticked/region/r.-2.1.mca',
             'prior_native': args.prior_native,
             'prior_canonical_receipt': args.prior_receipt,
             'native_resume_log': args.resume / 'run.log',
             'population_order': args.source / 'order.tsv',
             'port_binary': args.binary,
             'uninterrupted_start': args.uninterrupted / 'start.bin',
             'uninterrupted_end': args.uninterrupted / 'end.bin',
             'uninterrupted_log': args.uninterrupted / 'run.log',
             'early_save': args.early / 'world_overlay.bin',
             'early_log': args.early / 'run.log',
             'restart_end': args.restarted / 'end.bin',
             'restart_log': args.restarted / 'run.log'}
    sources = sorted(Path('src').glob('*.rs')) + [Path('Cargo.toml'), Path('Cargo.lock')]
    require(all(path.stat().st_mtime_ns <= args.binary.stat().st_mtime_ns for path in sources),
            'port binary predates source')
    result = {'region': [-64, 32, 3], 'y': [-64, 319], 'cells_compared': len(frozen),
              'native_resume_times': times,
              'port_generation_vs_frozen': 0, 'port_700_ticks_vs_native': 0,
              'port_save_after_5_then_695_ticks_vs_native': 0,
              'resumed_native_vs_prior_canonical_matched_native': 0,
              'sha256': {name: digest(path) for name, path in paths.items()},
              'source_sha256': {str(path): digest(path) for path in sources},
              'scope': 'seed 0, no structures, explicit native population order, full game_tick, forced 3x3; not general ticket scheduling or other worlds'}
    text = json.dumps(result, indent=2) + '\n'
    if args.out:
        args.out.write_text(text)
    print(text, end='')


if __name__ == '__main__':
    main()
