#!/usr/bin/env python3
"""Sculk trace workbench: read, sanity-check and compare cursor/placement traces.

    tools/sculk_trace.py summary <trace>...
    tools/sculk_trace.py sanity <trace> [--require KIND]...
    tools/sculk_trace.py names <class|type>...
    tools/sculk_trace.py passes <trace> [--json]
    tools/sculk_trace.py windows <trace> [--class CLS] [--arm leading|trailing]
    tools/sculk_trace.py draws <trace> [--class CLS] [--pass cx,cz] [--limit N]
    tools/sculk_trace.py extract <run.log> [--origin x,y,z] [--lines N] [--out FILE]
    tools/sculk_trace.py compare-passes <native> <port-trace>
    tools/sculk_trace.py compare-draws <native> <port-draw-log> [--class CLS] [--offset N]
    tools/sculk_trace.py patches <log> [--origins FILE] [--out FILE]

Why a tool and not a handful of greps: the same three analyses (per-pass attempt sets, per-patch
cursor trajectories, per-window RNG draws) were rewritten by hand for every capture in this work,
each time with a fresh chance of getting the grouping wrong. Two of those mistakes cost hours:

* **Line order is not program order** unless the trace came from `SculkLog`'s own file
  (`-Dsculktrace=<path>`). log4j's console appender does not preserve a thread's sequence, so any
  "between this marker and the next" attribution read off `run.log` is scrambled. `is_log_file`
  detects the `[hh:mm:ss] [thread/LEVEL]` prefix, and every positional subcommand refuses a log
  unless `--allow-unordered` is passed.

* **An armed window can be structurally unable to contain what you are looking for**: arming on
  `ConfiguredFeature.place` cannot see the placement's position draws, because the placement
  modifiers run in `PlacedFeature.placeWithBiomeCheck`, one level up. The trace looks healthy
  (windows, markers, thousands of draws) and the numbers are silently about something else.
  `sanity` fails on exactly that shape: draws present, and no window holding any.

Marker vocabulary (from `tools/carve-agent/SculkLog.java`):

    SPASS  cx cz                  decoration pass entered (ChunkGenerator.applyBiomeDecoration)
    SPLACE <feature-class> cx cz   a feature placement started (ConfiguredFeature.place)
    SRNG   <rng-class>             the RandomSource that placement was handed
    SDRAW  <kind> <value>          an RNG draw while armed
    SPATCH x y z                   a sculk patch's origin (SculkPatchFeature.place)
    SCURSOR r= i= x y z charge decayDelay updateDelay facingMask
                                   one ChargeCursor.update
    SCMARK <site> r= i=            a named call site inside the update
    SSPREAD / SSPREADALL ...       spread attempts
"""
import argparse
import collections
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLASS_TABLE = ROOT / 'tests/data/feature_classes_1211.tsv'

# `[21:49:44] [Worker-Main-7/INFO]: [STDOUT]: SPLACE edi ...` - a run.log line
LOG_PREFIX = re.compile(r"^\s*\[\d\d:\d\d:\d\d\] \[[^\]]+/(INFO|WARN|ERROR)\]: (?:\[STDOUT\]: )?")
THREAD = re.compile(r"\[([A-Za-z0-9\-_ ]+)/(?:INFO|WARN|ERROR)\]:")
CURSOR = re.compile(r"SCURSOR r=(-?\d+) i=(-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+)")
PATCH = re.compile(r"SPATCH (-?\d+) (-?\d+) (-?\d+)")
DRAW = re.compile(r"DRAW (\d+) f=(-?\d+),(-?\d+) ([^=]+)=(\S+)")


def trace_lines(path):
    """Marker lines only, with any `run.log` prefix stripped."""
    out = []
    for raw in Path(path).read_text(errors='replace').splitlines():
        line = LOG_PREFIX.sub('', raw).strip()
        if line and line.split()[0].isupper():
            out.append(line)
    return out


def reads_unordered(path):
    """True when the file is a game log, whose line order is not program order."""
    for raw in Path(path).read_text(errors='replace').splitlines():
        if raw.strip():
            return bool(LOG_PREFIX.match(raw))
    return False


def threads_of(path):
    """{line index: thread name} for a game log; empty for an ordered trace."""
    out = {}
    for i, raw in enumerate(Path(path).read_text(errors='replace').splitlines()):
        m = THREAD.search(raw)
        if m:
            out[i] = m.group(1)
    return out


def split_passes(lines):
    """[(pass chunk, [lines])] in order. A pass starts at `SPASS`."""
    out, cur, buf = [], None, []
    for line in lines:
        if line.startswith('SPASS '):
            if cur is not None:
                out.append((cur, buf))
            f = line.split()
            cur, buf = (int(f[1]), int(f[2])), []
        elif cur is not None:
            buf.append(line)
    if cur is not None:
        out.append((cur, buf))
    return out


def split_patches(lines):
    """[(origin, [cursor lines])] - `SPATCH` starts a patch, `SCURSOR` lines follow it."""
    out, origin, buf = [], None, []
    for line in lines:
        if line.startswith('SPATCH '):
            if origin is not None:
                out.append((origin, buf))
            f = line.split()
            origin, buf = (int(f[1]), int(f[2]), int(f[3])), []
        elif origin is not None and line.startswith('SCURSOR'):
            buf.append(line)
    if origin is not None:
        out.append((origin, buf))
    return out


def split_windows(lines, arm='trailing'):
    """[(kind, payload, [lines])] - one entry per `SPLACE` marker.

    `arm='trailing'`: a window holds the lines that *follow* its marker (what a hook armed on the
    same method it prints from sees).
    `arm='leading'`: a window holds the lines that *precede* its marker - the layout of a hook
    armed one level higher, e.g. the placement modifiers running inside
    `PlacedFeature.placeWithBiomeCheck` before it calls `ConfiguredFeature.place`. Which layout a
    trace has is a property of the agent build, so `sanity` reports which one it found rather than
    assuming one.
    """
    at = [i for i, line in enumerate(lines) if line.startswith('SPLACE ')]
    out = []
    for n, i in enumerate(at):
        fields = lines[i].split()
        if arm == 'trailing':
            end = at[n + 1] if n + 1 < len(at) else len(lines)
            body = lines[i + 1:end]
        else:
            start = at[n - 1] + 1 if n else 0
            body = lines[start:i]
        out.append(['SPLACE', {'cls': fields[1], 'src': (int(fields[2]), int(fields[3]))}, body])
    return out


def window_summary(windows):
    """(kind, cls, src, draws, patches, cursors) per window."""
    rows = []
    for kind, payload, body in windows:
        draws = [l.split()[2] for l in body if l.startswith('SDRAW ')]
        rows.append({
            'kind': kind,
            'cls': payload.get('cls'),
            'src': payload.get('src'),
            'draws': draws,
            'patches': [l for l in body if l.startswith('SPATCH ')],
            'cursors': sum(1 for l in body if l.startswith('SCURSOR')),
        })
    return rows


def pass_table(path):
    """pass chunk -> {'attempts': set(origins), 'spreads': set(origins)}."""
    lines = trace_lines(path)
    out = collections.OrderedDict()
    for chunk, body in split_passes(lines):
        entry = out.setdefault(chunk, {'attempts': set(), 'spreads': set()})
        last = None
        for line in body:
            m = PATCH.match(line)
            if m:
                last = tuple(int(m.group(k)) for k in range(1, 4))
                entry['attempts'].add(last)
            elif line.startswith('SCURSOR') and last is not None:
                entry['spreads'].add(last)
    return out


def class_names(cls):
    """Every feature *type* the class table maps to this class (empty when unknown)."""
    if not CLASS_TABLE.exists():
        return []
    out = []
    for line in CLASS_TABLE.read_text().splitlines():
        parts = line.split('\t')
        if len(parts) >= 2 and parts[1] == cls:
            out.append(parts[0])
    return out


def class_of_type(name):
    """The class the pinned jar's registry maps this feature type to."""
    name = name.split(':')[-1]
    if not CLASS_TABLE.exists():
        return None
    for line in CLASS_TABLE.read_text().splitlines():
        parts = line.split('\t')
        if len(parts) >= 2 and parts[0] == name:
            return parts[1]
    return None


def sanity_problems(path, required=()):
    """Structural checks that fail loudly instead of producing quiet nonsense."""
    problems = []
    lines = trace_lines(path)
    kinds = collections.Counter(l.split()[0] for l in lines)
    if not lines:
        return [f"{path}: no marker lines at all"]
    for kind in required:
        if kinds[kind] == 0:
            problems.append(f"{path}: no {kind} lines (required by --require)")
    if reads_unordered(path):
        problems.append(
            f"{path}: this is a game log - log4j does not preserve a thread's line order, so "
            f"every positional grouping below is meaningless (use the agent's own file, "
            f"-Dsculktrace=<path>)")
    if kinds['SPLACE'] and not kinds['SPASS']:
        problems.append(f"{path}: SPLACE without SPASS - the pass grouping has nothing to use")
    windows = window_summary(split_windows(lines, arm='trailing'))
    if kinds['SDRAW'] and not any(w['draws'] for w in windows):
        problems.append(
            f"{path}: {kinds['SDRAW']} draws but no SPLACE window holds any - the armed window "
            f"cannot see these draws (check the arm point, e.g. `eiv.b` vs `ebq.a`, and the "
            f"hooked class)")
    unknown = collections.Counter(w['cls'] for w in windows
                                 if w['cls'] and not class_names(w['cls']))
    if unknown:
        problems.append(f"{path}: SPLACE names classes absent from the feature table: "
                        f"{dict(unknown.most_common(5))}")

    # The shape that cost an hour, and the test that distinguishes it from a healthy trace.
    #
    # A patch's *position* draws are the placement modifiers' bounded draws, so they are small
    # integers with a `bound` in the kind. In a healthy trace they sit immediately before the
    # patch announces its origin (the modifiers run inside `PlacedFeature.placeWithBiomeCheck`,
    # one level above `ConfiguredFeature.place`, so their lines come first). What made the bad
    # traces look healthy was the *spreader's* draws inside the same placement - plenty of
    # `SDRAW` lines, and every one of them irrelevant to the question being asked. A value check
    # separates the two without hard-coding class names: the raw generator (`next(bits)`) returns
    # full-range values, the modifiers return < a few hundred.
    def position_like(line):
        if not line.startswith('SDRAW '):
            return False
        parts = line.split()
        if 'bound' not in parts[1] and 'nextInt' not in parts[1]:
            return False
        try:
            return abs(int(parts[2])) < 1024
        except ValueError:
            return False

    # The test that distinguishes a usable draw trace from a plausible-looking one, and the
    # structure it relies on (both measured, not assumed):
    #
    #   * the placement modifiers draw a *position list*, then the feature is placed at each
    #     position. So the modifier draws sit immediately before the **first** marker of a group;
    #     the rest are preceded by the previous `SPATCH` (measured: 1131 of 1231 `SPLACE edi`
    #     markers follow a `SPATCH`, 45 follow `nextInt(bound)@dyn`, 55 `nextFloat()@dyn`).
    #   * a sculk patch's position draws are small bounded integers (`nextInt(16)`, `nextInt(321)`
    #     in the port's own trace), while the raw generator (`next(bits)`) returns full-range
    #     values - which is what separates the placement's draws from the spreader's.
    #
    # So: at least one marker must be immediately preceded by a position-like draw. Measured:
    # healthy trace 100, armed-one-level-too-deep 0, no-draw-hooks 0.
    def position_like(line):
        if not line.startswith('SDRAW '):
            return False
        parts = line.split()
        if 'bound' not in parts[1] and 'nextInt' not in parts[1]:
            return False
        try:
            return abs(int(parts[2])) < 1024
        except ValueError:
            return False

    markers = [(i, l.split()[1]) for i, l in enumerate(lines) if l.startswith('SPLACE ')]
    preceded = [(i, cls) for i, cls in markers if i and position_like(lines[i - 1])]
    # In-window evidence must be *before* the window's first `SPATCH`: draws that come after it
    # belong to the spreader, which is what made the too-shallow traces look healthy.
    in_window = 0
    for i, cls in markers:
        j = i + 1
        hit = False
        while j < len(lines) and not lines[j].startswith('SPLACE '):
            if lines[j].startswith('SPATCH '):
                break
            hit = hit or position_like(lines[j])
            j += 1
        if hit:
            in_window += 1
    # The gate is about the *sculk* placements: a trace where some other feature's marker carries
    # position draws still cannot answer a question about a sculk patch's position.
    sculk_preceded = [c for _, c in preceded if class_names(c) == ['sculk_patch']]
    if kinds['SPATCH']:
        if sculk_preceded:
            print(f"sculk_trace: {path}: {len(preceded)} of {len(markers)} SPLACE markers are "
                  f"immediately preceded by a position-like draw, {len(sculk_preceded)} of them "
                  f"sculk_patch; use --arm leading")
        elif preceded:
            problems.append(
                f"{path}: {len(preceded)} SPLACE markers carry a position-like draw but none of "
                f"them is a sculk_patch marker - this trace cannot answer a question about a sculk "
                f"patch's placement position")
        elif in_window:
            print(f"sculk_trace: {path}: {in_window} windows hold a position-like draw in-window; "
                  f"use --arm trailing")
        elif kinds['SDRAW']:
            problems.append(
                f"{path}: no SPLACE marker is preceded by (or followed by) a position-like draw - a "
                f"small bounded `nextInt` - although the trace holds {kinds['SDRAW']} draws. The "
                f"placement's position draws are made in PlacedFeature.placeWithBiomeCheck "
                f"(`eiv.b`), one level above ConfiguredFeature.place (`ebq.a`); a trace armed on "
                f"the lower one looks healthy (markers, patches, thousands of spreader draws) and "
                f"cannot answer a question about placement positions")
        else:
            problems.append(
                f"{path}: {kinds['SPATCH']} sculk patches and no `SDRAW` lines at all - the RNG "
                f"draw hooks are not in this agent build")
    return problems


def cmd_summary(args):
    for path in args.traces:
        lines = trace_lines(path)
        kinds = collections.Counter(l.split()[0] for l in lines)
        windows = window_summary(split_windows(lines, arm='trailing'))
        sculk_cls = {w['cls'] for w in windows if w['cls'] and class_names(w['cls'])}
        print(f"{path}: {len(lines)} marker lines"
              + ("  (GAME LOG - line order is not program order)" if reads_unordered(path) else ""))
        print("  kinds: " + ", ".join(f"{k}={v}" for k, v in kinds.most_common()))
        print(f"  passes={kinds['SPASS']} windows={kinds['SPLACE']} patches={kinds['SPATCH']} "
              f"cursors={kinds['SCURSOR']} draws={kinds['SDRAW']}")
        with_draws = [w for w in windows if w['draws']]
        if kinds['SDRAW']:
            print(f"  windows holding draws: {len(with_draws)} of {len(windows)}"
                  f" (median draws {sorted(len(w['draws']) for w in windows)[len(windows) // 2] if windows else 0})")
        if sculk_cls:
            print("  sculk-capable classes in SPLACE: "
                  + ", ".join(f"{c}({','.join(class_names(c))})" for c in sorted(sculk_cls)))
    return 0


def cmd_sanity(args):
    bad = []
    for path in args.traces:
        problems = sanity_problems(path, args.require)
        if problems:
            bad += problems
        else:
            print(f"sculk_trace: {path} sane")
    for problem in bad:
        print(f"sculk_trace: PROBLEM {problem}", file=sys.stderr)
    return 1 if bad else 0


def cmd_names(args):
    rc = 0
    for needle in args.names:
        cls = needle if re.fullmatch(r'[a-z]{3}', needle) else class_of_type(needle)
        if cls is None:
            print(f"{needle}: not in {CLASS_TABLE.name}")
            rc = 1
            continue
        types = class_names(cls)
        if len(types) > 1:
            print(f"{needle} -> {cls} -> {len(types)} types: {', '.join(types)} (ambiguous: one "
                  f"class serves several placed feature types)")
        elif types:
            print(f"{needle} -> {cls} -> {types[0]}")
        else:
            print(f"{needle} -> {cls} -> (no type)")
    return rc


def cmd_passes(args):
    table = pass_table(args.trace)
    out = {f"{c[0]},{c[1]}": {'attempts': len(v['attempts']), 'spreads': len(v['spreads'])}
           for c, v in table.items()}
    if args.json:
        print(json.dumps(out, indent=2, sort_keys=True))
    else:
        for chunk, entry in table.items():
            print(f"pass {chunk[0]},{chunk[1]}: {len(entry['attempts'])} attempts, "
                  f"{len(entry['spreads'])} spread")
    return 0


def cmd_windows(args):
    lines = trace_lines(args.trace)
    rows = window_summary(split_windows(lines, arm=args.arm))
    shown = 0
    for row in rows:
        if args.cls and row['cls'] != args.cls:
            continue
        print(f"{row['cls']} src={row['src']} draws={len(row['draws'])} "
              f"patches={len(row['patches'])} cursors={row['cursors']}"
              + (f" first draws={row['draws'][:6]}" if row['draws'] else ""))
        shown += 1
        if args.limit and shown >= args.limit:
            break
    return 0


def cmd_draws(args):
    lines = trace_lines(args.trace)
    want_pass = tuple(int(v) for v in args.pass_chunk.split(',')) if args.pass_chunk else None
    chunks = split_passes(lines)
    for chunk, body in chunks:
        if want_pass and chunk != want_pass:
            continue
        lines = body
    rows = window_summary(split_windows(lines, arm=args.arm))
    printed = 0
    for row in rows:
        if args.cls and row['cls'] != args.cls:
            continue
        for value in row['draws']:
            print(value)
            printed += 1
            if args.limit and printed >= args.limit:
                return 0
    return 0


def cmd_extract(args):
    """Marker lines out of a game log, with the ordering caveat stated at the end."""
    lines = trace_lines(args.log)
    origin = tuple(int(v) for v in args.origin.split(',')) if args.origin else None
    if origin is not None:
        keep, started = [], False
        for i, line in enumerate(lines):
            m = PATCH.match(line)
            if m and tuple(int(m.group(k)) for k in range(1, 4)) == origin:
                started = True
                keep = lines[i:]
                break
        lines = keep if started else []
    if args.lines:
        lines = lines[:args.lines]
    text = "\n".join(lines) + ("\n" if lines else "")
    if args.out:
        Path(args.out).write_text(text)
        print(f"sculk_trace: {len(lines)} lines -> {args.out}")
    else:
        sys.stdout.write(text)
    if reads_unordered(args.log):
        print("sculk_trace: note: extracted from a game log - the line order is not program "
              "order; use it for set/membership questions only", file=sys.stderr)
    return 0


def cmd_compare_passes(args):
    native, port = pass_table(args.native), pass_table(args.port)
    shared = [c for c in native if c in port]
    identical = [c for c in shared if native[c]['attempts'] == port[c]['attempts']]
    buckets = collections.Counter()
    for c in shared:
        d = len(native[c]['attempts'] ^ port[c]['attempts'])
        buckets[0 if d == 0 else 1 if d <= 2 else 2 if d <= 8 else 3] += 1
    na = sum(len(native[c]['attempts']) for c in shared)
    pa = sum(len(port[c]['attempts']) for c in shared)
    sh = sum(len(native[c]['attempts'] & port[c]['attempts']) for c in shared)
    equal = [c for c in shared if len(native[c]['attempts']) == len(port[c]['attempts'])]
    print(f"passes: native {len(native)}, port {len(port)}, shared {len(shared)}")
    print(f"identical sculk attempt set: {len(identical)} of {len(shared)}")
    print(f"difference distribution: identical {buckets[0]}, 1-2 {buckets[1]}, 3-8 {buckets[2]}, "
          f">8 {buckets[3]}")
    print(f"attempts over shared passes: native {na}, port {pa}, shared {sh}")
    print(f"passes with equal attempt counts: {len(equal)}, identical positions: "
          f"{sum(1 for c in equal if native[c]['attempts'] == port[c]['attempts'])}")
    diverged = [c for c in shared if native[c]['attempts'] != port[c]['attempts']]
    for c in diverged[:args.limit]:
        print(f"divergent pass (trace order): {c[0]},{c[1]} native "
              f"{len(native[c]['attempts'])} / port {len(port[c]['attempts'])}, shared "
              f"{len(native[c]['attempts'] & port[c]['attempts'])}")
    ns = set().union(*(native[c]['spreads'] for c in shared)) if shared else set()
    ps = set().union(*(port[c]['spreads'] for c in shared)) if shared else set()
    print(f"spread origins over shared passes: native {len(ns)}, port {len(ps)}, agree "
          f"{len(ns & ps)}, native-only {len(ns - ps)}, port-only {len(ps - ns)}")
    return 0 if not diverged else 1


def parse_port_draws(path):
    """[(gi, step, kind, value)] from the port's `DRAW n f=gi,step kind=value` trace."""
    out = []
    for raw in Path(path).read_text(errors='replace').splitlines():
        m = DRAW.search(raw)
        if m:
            out.append((int(m.group(2)), int(m.group(3)), m.group(4).strip(), m.group(5)))
    return out


def native_bounded_draws(path):
    """[(bound, value)] from an ordered trace's `nextInt(bound)@cls(<bound>) <value>` lines."""
    out = []
    for line in trace_lines(path):
        if not line.startswith('SDRAW '):
            continue
        m = re.match(r'SDRAW (\S*bound\S*)?\((\d+)\) (\d+)$', line)
        if m:
            out.append((int(m.group(2)), int(m.group(3))))
    return out


def cmd_compare_draws(args):
    """Find where the port's draw sequence sits in the native stream, and where it parts.

    Joined on `(bound, value)`: the port logs `nextInt(16)=15`, the native logs
    `nextInt(bound)@dyn(16) 15`, so the *bound* is what makes the two sequences the same question
    rather than two streams of integers. The answer is a *position* in the native stream plus a
    match length, which is what turns "the placement differs" into "they agree for 24 draws and
    then ask different questions".
    """
    native = native_bounded_draws(args.native)
    port = [(int(m.group(1)), int(m.group(2)))
            for line in Path(args.port).read_text(errors='replace').splitlines()
            if (m := re.search(r'nextInt\((\d+)\)=(\d+)', line))]
    if not port:
        print("sculk_trace: the port trace has no bounded draws (run with FSEED_ARM=..., )",
              file=sys.stderr)
        return 2
    if not native:
        print(f"sculk_trace: {args.native} has no bounded draws with a bound - it was produced by "
              f"an agent build that did not log them", file=sys.stderr)
        return 2
    if args.window:
        probe = port[:args.window]
    else:
        probe = port[:min(6, len(port))]
    hits = [i for i in range(len(native) - len(probe) + 1) if native[i:i + len(probe)] == probe]
    print(f"native bounded draws {len(native)}, port bounded draws {len(port)}")
    print(f"probe (port's first {len(probe)}): {probe}")
    if not hits:
        print("the port's sequence does not occur in the native stream - the two placements are "
              "not drawing the same questions anywhere in this capture")
        return 1
    print(f"occurrences: {len(hits)} at native index {hits[:4]}")
    at = hits[0]
    same = 0
    for a, b in zip(port, native[at:]):
        if a != b:
            break
        same += 1
    print(f"identical prefix from native index {at}: {same} of {len(port)}")
    if same < len(port):
        print(f"  first divergence: port {port[same]} native {native[at + same]}")
    return 0 if same == len(port) else 1


def parse_biomef(path, feature=None):
    """[(x, y, z, ok)] from `BIOMEF x y z <ok> [biome=<id>] feature=<gi>,<step>` lines.

    Both sides emit this shape - the native via the `eii` hook (gated by `-Dbiomefchunk=cx,cz`,
    written through `SculkLog.line` so the order is program order) and the port under
    `BIOMEF_TRACE=cx,cz`. Comparing only the *verdicts* answers "does the filter agree"; comparing
    the *sequence* answers "are the same candidates visited in the same order", which is a
    different question and the one that matters when the draw streams part.
    """
    out = []
    pattern = re.compile(r'BIOMEF (-?\d+) (-?\d+) (-?\d+) (true|false)'
                         r'(?: biome=\S+)? feature=(\d+),(\d+)')
    for line in Path(path).read_text(errors='replace').splitlines():
        m = pattern.search(line)
        if not m:
            continue
        if feature is not None and (int(m.group(5)), int(m.group(6))) != feature:
            continue
        out.append((int(m.group(1)), int(m.group(2)), int(m.group(3)), m.group(4) == 'true'))
    return out


def cmd_compare_biomef(args):
    feature = tuple(int(v) for v in args.feature.split(','))
    native = parse_biomef(args.native, feature)
    port = parse_biomef(args.port, feature)
    print(f"feature={args.feature}: candidates logged native {len(native)}, port {len(port)}")
    if not native or not port:
        print("sculk_trace: one side logged no candidates - check -Dbiomefchunk / BIOMEF_TRACE and "
              "that the feature index is the same placed feature on both sides (the index is into "
              "the step list, and two different features can share it)", file=sys.stderr)
        return 2
    ns, ps = [c[:3] for c in native], [c[:3] for c in port]
    na, pa = [c[:3] for c in native if c[3]], [c[:3] for c in port if c[3]]
    print(f"  position set identical: {set(ns) == set(ps)};  sequence identical: {ns == ps}")
    print(f"  accepted: native {len(na)}, port {len(pa)}; set identical: {set(na) == set(pa)}; "
          f"sequence identical: {na == pa}")
    bad = sum(1 for a, b in zip(native, port) if a[:3] == b[:3] and a[3] != b[3])
    print(f"  verdict disagreements on aligned positions: {bad}")
    if ns != ps:
        i = next((i for i, (a, b) in enumerate(zip(ns, ps)) if a != b), min(len(ns), len(ps)))
        print(f"  first order difference at {i}: native {ns[i] if i < len(ns) else None} "
              f"port {ps[i] if i < len(ps) else None}")
    ok = set(ns) == set(ps) and ns == ps and na == pa and bad == 0
    return 0 if ok else 1


def cmd_patches(args):
    lines = trace_lines(args.log)
    keep = None
    if args.origins:
        keep = {tuple(int(v) for v in ln.split())
                for ln in Path(args.origins).read_text().splitlines() if ln.strip()}
    rows = []
    for origin, body in split_patches(lines):
        if keep is not None and origin not in keep:
            continue
        round_no, last = -1, None
        for line in body:
            m = CURSOR.search(line)
            if not m:
                continue
            rnd = int(m.group(1))
            if rnd != last:
                round_no += 1
                last = rnd
            state = [int(m.group(k)) for k in range(2, 10)]
            rows.append((origin, round_no) + tuple(state))
    text = "".join(" ".join(str(v) for v in row) + "\n" for row in rows)
    if args.out:
        Path(args.out).write_text(text)
        print(f"sculk_trace: {len(rows)} rows, {len({r[0] for r in rows})} patches -> {args.out}")
    else:
        sys.stdout.write(text)
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='cmd', required=True)

    s = sub.add_parser('summary', help='marker counts, windows and which classes are in the trace')
    s.add_argument('traces', nargs='+')
    s.set_defaults(func=cmd_summary)

    s = sub.add_parser('sanity', help='fail on a trace that cannot answer the questions asked of it')
    s.add_argument('traces', nargs='+')
    s.add_argument('--require', action='append', default=[],
                   help='a marker kind that must be present (repeatable)')
    s.set_defaults(func=cmd_sanity)

    s = sub.add_parser('names', help='feature class <-> type, from tests/data/feature_classes_1211.tsv')
    s.add_argument('names', nargs='+')
    s.set_defaults(func=cmd_names)

    s = sub.add_parser('passes', help='per decoration pass: sculk attempts and spread origins')
    s.add_argument('trace')
    s.add_argument('--json', action='store_true')
    s.set_defaults(func=cmd_passes)

    s = sub.add_parser('windows', help='per SPLACE window: draws, patches, cursors')
    s.add_argument('trace')
    s.add_argument('--class', dest='cls')
    s.add_argument('--arm', choices=['trailing', 'leading'], default='trailing')
    s.add_argument('--limit', type=int)
    s.set_defaults(func=cmd_windows)

    s = sub.add_parser('draws', help='the draw values of a window (optionally one pass)')
    s.add_argument('trace')
    s.add_argument('--class', dest='cls')
    s.add_argument('--pass', dest='pass_chunk', help='cx,cz')
    s.add_argument('--arm', choices=['trailing', 'leading'], default='trailing')
    s.add_argument('--limit', type=int)
    s.set_defaults(func=cmd_draws)

    s = sub.add_parser('extract', help='marker lines out of a game log')
    s.add_argument('log')
    s.add_argument('--origin', help='start at the patch with this origin x,y,z')
    s.add_argument('--lines', type=int)
    s.add_argument('--out')
    s.set_defaults(func=cmd_extract)

    s = sub.add_parser('compare-passes', help='native trace vs port trace, per decoration pass')
    s.add_argument('native')
    s.add_argument('port')
    s.add_argument('--limit', type=int, default=1,
                   help='how many divergent passes to list (in trace order)')
    s.set_defaults(func=cmd_compare_passes)

    s = sub.add_parser('compare-draws',
                       help="find the port's draw sequence in the native stream, joined on "
                            "(bound, value)")
    s.add_argument('native')
    s.add_argument('port')
    s.add_argument('--window', type=int, default=0,
                   help='probe length (default: the first 6 port draws, then extend)')
    s.set_defaults(func=cmd_compare_draws)

    s = sub.add_parser('compare-biomef',
                       help='candidate positions and biome-filter verdicts, native vs port')
    s.add_argument('native')
    s.add_argument('port')
    s.add_argument('--feature', required=True, help='gi,step as both sides log it')
    s.set_defaults(func=cmd_compare_biomef)

    s = sub.add_parser('patches', help='patch-scoped cursor rows (needs an ordered trace)')
    s.add_argument('log')
    s.add_argument('--origins')
    s.add_argument('--out')
    s.set_defaults(func=cmd_patches)

    args = ap.parse_args()
    positional = ['trace', 'traces', 'native', 'port', 'log']
    for name in positional:
        for value in ([getattr(args, name)] if isinstance(getattr(args, name, None), str)
                      else getattr(args, name, []) or []):
            pass
    for name in ['trace', 'native']:
        path = getattr(args, name, None)
        if isinstance(path, str) and reads_unordered(path) and args.cmd in (
                'passes', 'windows', 'draws', 'patches', 'compare-passes', 'compare-biomef'):
            print(f"sculk_trace: refusing {args.cmd} on {path}: it is a game log and its line order "
                  f"is not program order (pass --allow-unordered only for set questions)",
                  file=sys.stderr)
            if not getattr(args, 'allow_unordered', False):
                return 2
    return args.func(args)


if __name__ == '__main__':
    sys.exit(main())
