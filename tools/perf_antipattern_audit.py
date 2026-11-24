#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Performance anti-pattern and unwired-optimizer CI guard.

Two jobs, both permanent regression gates:

* Anti-pattern ratchets over the client frame path and the server
  tick/worldgen hot modules. Counts are per-file ceilings kept in
  ``audit/perf_antipattern_baseline.json``; a file whose count grows past
  its ceiling fails with an exact ``file:line``, a file that improves
  prints a "tighten the baseline" tip (or fails under ``--fail-on-stale``).
* Required-wired optimizer registry: every optimizer Megu ships is dead
  code if nothing production calls it — the kernel survives, tests stay
  green, and the feature silently unwires. Each named optimizer must keep
  at least one non-test, non-declaration use in its production files.

All checks run on masked Rust source: comments, string/char literals and
``#[cfg(test)]``/``#[test]`` items are blanked (offsets and newlines are
preserved), so a call site that only exists under test or inside a comment
can never satisfy — or inflate — a check. Stdlib only, byte-level scan;
the whole audit is designed to finish in well under a second.

Usage:
    tools/perf_antipattern_audit.py                      # human PASS/FAIL summary
    tools/perf_antipattern_audit.py --json               # machine-readable report
    tools/perf_antipattern_audit.py --fail-on-stale      # also fail when a ceiling can drop
    tools/perf_antipattern_audit.py --update-baseline    # lower ceilings to current counts
    tools/perf_antipattern_audit.py --root DIR
Exit: 0 pass, 1 regression (or stale baseline under --fail-on-stale), 2 invalid input.
"""

import argparse
import bisect
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
BASELINE = Path('audit/perf_antipattern_baseline.json')

CLIENT_FILES = (
    'client-gpu/src/atmosphere.rs',
    'client-gpu/src/live_session.rs',
    'client-gpu/src/renderer.rs',
    'client-gpu/src/window.rs',
)
CUBE_SCAN_FILES = (
    'client-gpu/src/live_session.rs',
    'client-gpu/src/window.rs',
)
SERVER_FILES = (
    'src/feature.rs',
    'src/jigsaw_structures.rs',
    'src/main.rs',
    'src/play_protocol.rs',
    'src/worldgen.rs',
)
# main.rs legitimately paces the master tick with a short sleep; the worldgen
# and protocol modules have no blocking work at all and must stay that way.
SLEEP_ZERO_FILES = (
    'src/feature.rs',
    'src/jigsaw_structures.rs',
    'src/play_protocol.rs',
    'src/worldgen.rs',
)

# Per-frame / per-draw entry points. The render/compose/draw/encode prefixes
# cover render_frame, render_to_view*, the encode_* command-buffer helpers and
# the draw_* helpers; the two named live_session fns run every frame.
FRAME_FN = re.compile(
    rb'^(?:render|compose|draw|encode)(?:_|$)|^(?:collect_dynamic_lights|refresh_voxel_clouds)$')
BLOCKING = re.compile(
    rb'(?:std::)?thread::sleep'
    rb'|std::fs::(?:read|read_to_string|read_dir|write|create_dir_all)'
    rb'|(?:std::fs::)?File::(?:open|create)'
    rb'|\.sync_all\s*\(')
GPU_CREATE = re.compile(
    rb'\.create_(?:buffer_init|buffer|texture|render_pipeline|compute_pipeline)\s*\(')
REMOVE_ZERO = re.compile(rb'\.remove\(\s*0\s*\)')
THREAD_SLEEP = re.compile(rb'(?:std::)?thread::sleep')
# `.clone()` is anchored literally (fast); the receiver chain is recovered by a
# bounded look-back — a single full-chain pattern backtracks quadratically on
# long `a.b.c.d` member chains in these files.
CLONE_CALL = re.compile(rb'\.clone\s*\(\s*\)')
CLONE_RECEIVER = re.compile(rb'([A-Za-z_]\w*(?:\s*\.\s*[A-Za-z_]\w*)*)\s*$')
HEAVY_RECEIVER = re.compile(
    rb'(?:map|nbt|palette|columns|sections|blocks|states|compound|entries|registry'
    rb'|snapshot|table|chunks?|world|index)', re.I)
SYSTEM_TIME = re.compile(rb'SystemTime::now')
GET_BLOCK = re.compile(rb'\bget_block_state\b')
CACHE_HINT = re.compile(rb'cache', re.I)

# (rule name, files, human explanation for findings)
RULES = (
    ('frame_blocking', CLIENT_FILES,
     'blocking disk I/O or sleep inside a frame/render path'),
    ('frame_gpu_create', CLIENT_FILES,
     'GPU resource creation inside a steady-state draw/encode path'),
    ('uncached_cube_scan', CUBE_SCAN_FILES,
     'uncached triple-nested get_block_state scan (route through PlacedLightScanCache)'),
    ('vec_remove_zero', SERVER_FILES,
     'O(n) Vec::remove(0) in a server hotpath (use VecDeque::pop_front or swap_remove)'),
    ('server_thread_sleep', SLEEP_ZERO_FILES,
     'blocking thread::sleep in a server tick/worldgen module'),
    ('hot_loop_map_clone', SERVER_FILES,
     'heavy NBT/block-map .clone() inside a hot loop'),
    ('hot_loop_systemtime', SERVER_FILES,
     'SystemTime::now() inside a hot loop (wall clock can jump; use Instant)'),
)

# Required-wired optimizers: (label, symbol regex, candidate paths/globs, mode).
# 'any' — at least one candidate file must hold a non-test use; 'each' — every
# listed file must hold one. A "use" is any occurrence outside comments,
# literals, cfg(test) items, `use` import lines and declaration heads
# (struct/enum/impl/fn/type/static/const). Imports alone do not count: an
# imported optimizer nothing calls is still unwired.
WIRED = (
    ('PlacedLightScanCache', rb'\bPlacedLightScanCache\b',
     ('client-gpu/src/live_session.rs',), 'any'),
    ('MeshJobPool', rb'\bMeshJobPool\b',
     ('client-gpu/src/*.rs',), 'any'),
    ('DirtyOnlyShadowCache', rb'\bDirtyOnlyShadowCache\b',
     ('client-gpu/src/renderer.rs',), 'any'),
    ('ChunkUploadBudget', rb'\bChunkUploadBudget\b',
     ('client-gpu/src/renderer.rs', 'client-gpu/src/window.rs'), 'each'),
    ('MAP_CHUNK_CACHE', rb'\bMAP_CHUNK_CACHE\b',
     ('src/main.rs',), 'any'),
    ('SpatialGrid', rb'\b\w*SpatialGrid\b',
     ('src/main.rs', 'src/play_protocol.rs'), 'any'),
    ('DfProg', rb'\bDfProg\b',
     ('src/worldgen.rs',), 'any'),
    ('AutoRecipeMemoizer', rb'\bAutoRecipeMemoizer\b',
     ('src/live_mod_bridge.rs',), 'any'),
    ('AdaptivePollBackoff', rb'\bAdaptivePollBackoff\b',
     ('src/live_mod_bridge.rs',), 'any'),
    # First-launch hardware auto-tune: the entry call must exist in BOTH
    # the CLI path (client-app --auto-tune) and the client boot path.
    ('recommend_tuning', rb'\brecommend_tuning\b',
     ('client-app/src/cli.rs', 'client-gpu/src/window.rs'), 'each'),
    # Bounded async chunk-frame build pool behind play_protocol delivery.
    ('async_chunk_delivery', rb'\basync_chunk_delivery::',
     ('src/play_protocol.rs',), 'any'),
    # Shared compressed chunk-packet cache on the encode hot path.
    ('ChunkPacketCache', rb'\bChunkPacketCache\b',
     ('src/play_protocol.rs',), 'any'),
    # Coordinate-keyed hash maps/sets in the worldgen hot loops.
    ('CoordHasher', rb'\bCoord(?:Hasher|Map|Set|Build)\b',
     ('src/worldgen.rs',), 'any'),
    # Per-rotation jigsaw template cache inside structure placement.
    ('rotated_jigsaws', rb'\brotated_jigsaws\b',
     ('src/jigsaw_structures.rs',), 'any'),
)

DECL_HEAD = re.compile(
    rb'(?:struct|enum|trait|impl|mod|type|static|const|fn)\s*$')
# A symbol inside a `use ...;` import statement — including braced multi-line
# imports — is not a call site.
USE_STMT = re.compile(rb'\buse\s+[\w:,\s{}*]*$')
_WORD = b'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_'

# ---------------------------------------------------------------------------
# Byte-level Rust masking. Comments, literals and cfg(test) items are replaced
# with spaces, preserving byte offsets and newlines, so structure walks and
# `file:line` reporting stay exact.
# ---------------------------------------------------------------------------

# `(?<![\w])` guards only the raw-string opener: `foo_r#"x"#`-style
# juxtapositions and `r#ident` raw identifiers must not swallow code. A `"`
# cannot legally abut a word char, so the plain-string alternative skips the
# lookbehind (measured ~10% faster over the corpus).
TOKEN = re.compile(
    rb'//[^\n]*'
    rb'|/\*'
    rb'|b?"(?:\\.|[^"\\\n])*"'
    rb'|(?<![\w])[bc]?r(#*)"'
    rb"|'(?:\\.|[^'\\\n])'")
# One-shot masking pattern for files where the careful loop is unnecessary:
# consumes each literal/comment whole — raw strings via the `#*`-backreference.
# Left-to-right alternation makes a `//`/`"`/`/*` inside an earlier token
# unreachable, exactly like the sequential lexer. It cannot express *nested*
# block comments, so callers verify no `/*`/`*/` survives before trusting it.
FAST_TOKEN = re.compile(
    rb'//[^\n]*'
    rb'|/\*(?s:.*?)\*/'
    rb'|b?"(?:\\.|[^"\\\n])*"'
    rb'|(?<![\w])[bc]?r(#*)"(?s:.*?)"\1'
    rb"|'(?:\\.|[^'\\\n])'")
BLOCK = re.compile(rb'/\*|\*/')
BLANK = bytearray(256)
for _i in range(256):
    BLANK[_i] = 32
BLANK[10] = 10  # keep newlines so file:line stays exact

DELIM = re.compile(rb'[{}\[\]();,]')
ATTR = re.compile(rb'#\s*\[')
# Mirrors tools/opt_wiring_audit.py: cfg(test) and cfg(all(test, ...)) are
# test-only; cfg(any(test, ...)) / cfg(not(test)) keep production meaning.
CFG_TEST = re.compile(rb'cfg\s*\(\s*(?:test\b|all\s*\(\s*test\b)')
BARE_TEST = re.compile(rb'(?:\w+::)*test\s*$')
# One keyword pass feeds both the fn-range and loop-body indexes: group(1)
# is a fn name, group(2) a loop keyword.
KW = re.compile(rb'\bfn\s+([A-Za-z_]\w*)|\b(for|while|loop)\b')
IMPL_FOR = re.compile(rb'\bimpl\b[^{};]*$')
ITEM_KEYWORDS = frozenset((
    b'fn', b'mod', b'impl', b'struct', b'enum', b'trait', b'use', b'const',
    b'static', b'type', b'pub', b'unsafe', b'extern', b'async', b'macro',
    b'let', b'union', b'macro_rules', b'auto', b'default', b'crate'))


def _blank(arr, start, end):
    arr[start:end] = arr[start:end].translate(BLANK)


def mask_literals(raw):
    """Blank comments and string/char/raw literals in `raw` bytes.

    Fast path: one `FAST_TOKEN.sub` pass replaces every token with its
    newline-preserving blanked form. It is then *verified* — if a `/*`/`*/`
    marker survives (nested block comments, which the flat pattern cannot
    balance), the result is discarded for the careful `finditer` loop below,
    which tracks `/* */` depth explicitly.
    """
    blanked = raw.translate(BLANK)
    if b'/*' not in raw:
        return bytearray(FAST_TOKEN.sub(
            lambda m: blanked[m.start():m.end()], raw))
    out = FAST_TOKEN.sub(lambda m: blanked[m.start():m.end()], raw)
    if b'/*' not in out and b'*/' not in out:
        return bytearray(out)
    arr = bytearray(raw)
    skip_to = 0
    for match in TOKEN.finditer(arr):
        start = match.start()
        if start < skip_to:
            continue
        first = arr[start]
        if first == 47:  # /
            if arr[start + 1] == 47:  # //
                arr[start:match.end()] = blanked[start:match.end()]
                continue
            # Nested block comment: walk /* */ pairs directly so a string-shaped
            # match inside cannot swallow a `*/` terminator.
            depth = 1
            end = match.end()
            while depth:
                tok = BLOCK.search(arr, end)
                if tok is None:
                    end = len(arr)
                    break
                depth += 1 if tok.group() == b'/*' else -1
                end = tok.end()
            arr[start:end] = blanked[start:end]
            skip_to = end
        elif first == 114 or (first in (98, 99) and arr[start + 1] == 114):
            # r / br / cr raw string — consume to `"` + hashes.
            hashes = match.group(1) or b''
            close = arr.find(b'"' + hashes, match.end())
            end = len(arr) if close < 0 else close + 1 + len(hashes)
            arr[start:end] = blanked[start:end]
            skip_to = end
        else:  # "…" / b"…" / '…' — the regex already spans the whole literal
            arr[start:match.end()] = blanked[start:match.end()]
    return arr


def delimiters(arr):
    """Byte offsets of structural punctuation in masked code."""
    return [m.start() for m in DELIM.finditer(arr)]


def _balanced_end(arr, toks, i):
    """Offset just past the `}` closing the `{` at toks[i]."""
    depth = 0
    while i < len(toks):
        byte = arr[toks[i]]
        if byte == 123:  # {
            depth += 1
        elif byte == 125:  # }
            depth -= 1
            if depth == 0:
                return toks[i] + 1
        i += 1
    return len(arr)


def mask_test_items(arr, toks):
    """Blank `#[cfg(test)]` / `#[test]` items so test code cannot satisfy a check."""
    for match in ATTR.finditer(arr):
        # match.end()-1 is the attr's own `[`; balance brackets from there.
        j = bisect.bisect_left(toks, match.end() - 1)
        depth = 0
        inner_end = None
        while j < len(toks):
            byte = arr[toks[j]]
            if byte == 91:  # [
                depth += 1
            elif byte == 93:  # ]
                depth -= 1
                if depth == 0:
                    inner_end = toks[j]
                    break
            j += 1
        if inner_end is None:
            continue
        inner = arr[match.end():inner_end]
        if not (CFG_TEST.search(inner) or BARE_TEST.fullmatch(inner.strip())):
            continue
        # Skip whitespace and any further attributes to reach the item.
        cursor = inner_end + 1
        while True:
            while cursor < len(arr) and arr[cursor] in b' \t\n\r':
                cursor += 1
            if arr[cursor:cursor + 2] != b'#[':
                break
            q = bisect.bisect_left(toks, cursor)
            depth = 0
            while q < len(toks):
                byte = arr[toks[q]]
                if byte == 91:
                    depth += 1
                elif byte == 93:
                    depth -= 1
                    if depth == 0:
                        cursor = toks[q] + 1
                        break
                q += 1
            else:
                break
        # Item end: first `{` -> balanced close, or `;`/`}`/`,` (declaration,
        # statement, or a cfg'd match arm/expression inside another item).
        # An item keyword (`fn`/`mod`/`impl`/...) may carry `<A, B>` generics
        # whose commas sit at depth 0, so `,` only aborts non-item positions.
        q = bisect.bisect_left(toks, cursor)
        depth = 0
        end = len(arr)
        word = re.match(rb'[A-Za-z_]+', arr[cursor:])
        is_item = word is not None and word.group() in ITEM_KEYWORDS
        while q < len(toks):
            byte = arr[toks[q]]
            if byte in (40, 91):  # ( [
                depth += 1
            elif byte in (41, 93):  # ) ]
                depth -= 1
            elif byte == 123 and depth == 0:  # {
                end = _balanced_end(arr, toks, q)
                break
            elif byte in (59, 125) and depth == 0:  # ; }
                end = toks[q] + (1 if byte == 59 else 0)
                break
            elif byte == 44 and depth == 0 and not is_item:  # ,
                end = toks[q]
                break
            q += 1
        _blank(arr, match.start(), end)


def masked_source(path):
    """Fully masked source plus delimiter index for one Rust file."""
    arr = mask_literals(path.read_bytes())
    toks = delimiters(arr)
    mask_test_items(arr, toks)
    return arr, toks


def _header_brace(arr, toks, start_index, abort_commas=False):
    """Token index of the `{` opening an item header, or -1.

    `;`/`}` at depth 0 always abort (declaration or a keyword that was not a
    header at all). `,` aborts only for loop scans: it ends a struct-literal
    field such as `Foo { loop: 1, }`, while fn signatures legitimately carry
    `<A, B>` generics whose commas sit at depth 0.
    """
    depth = 0
    j = start_index
    while j < len(toks):
        byte = arr[toks[j]]
        if byte in (40, 91):
            depth += 1
        elif byte in (41, 93):
            depth -= 1
        elif depth == 0:
            if byte == 123:
                return j
            if byte in (59, 125) or (abort_commas and byte == 44):
                return -1
        j += 1
    return -1


def fn_ranges(arr, toks, kw):
    """`(name, body_start, body_end)` for every non-test `fn` with a body."""
    out = []
    for match in kw:
        name = match.group(1)
        if name is None:
            continue
        i = bisect.bisect_left(toks, match.end())
        brace = _header_brace(arr, toks, i)
        if brace < 0:
            continue
        out.append((name, toks[brace], _balanced_end(arr, toks, brace)))
    return out


def loop_bodies(arr, toks, kw):
    """`(body_start, body_end)` spans of for/while/loop bodies."""
    spans = []
    for match in kw:
        kw_kind = match.group(2)
        if kw_kind is None:
            continue
        if kw_kind == b'for':
            tail = arr[match.end():match.end() + 16].lstrip()
            if tail[:1] == b'<':  # `for<'a>` higher-ranked bound, not a loop
                continue
            # `impl X for Y` is a trait impl, not a loop.
            if IMPL_FOR.search(arr[max(0, match.start() - 200):match.start()]):
                continue
        i = bisect.bisect_left(toks, match.end())
        brace = _header_brace(arr, toks, i, abort_commas=True)
        if brace >= 0:
            spans.append((toks[brace], _balanced_end(arr, toks, brace)))
    return spans


def loop_depth(index, offset):
    """Enclosing-loop count via interval stabbing on sorted span edges."""
    starts, ends = index
    return bisect.bisect_right(starts, offset) - bisect.bisect_right(ends, offset)


def span_index(spans):
    return (sorted(start for start, _ in spans), sorted(end for _, end in spans))


def _line(arr, offset):
    return arr.count(b'\n', 0, offset) + 1


# ---------------------------------------------------------------------------
# Rule evaluation
# ---------------------------------------------------------------------------


def rule_counts(ctx, rule, path):
    """`(count, [(line, excerpt), ...])` for one rule on one masked file.

    `ctx` caches the per-file `fn`/`loop` analysis so a file checked by
    several rules pays for its structure pass once.
    """
    arr = ctx['arr']
    lines = []

    def at(offset):
        if not lines:
            lines.extend(arr.decode('utf-8', 'replace').splitlines())
        line = _line(arr, offset)
        return line, lines[line - 1].strip()[:110]

    if 'kw' not in ctx:
        ctx['kw'] = list(KW.finditer(arr))
    if rule in ('frame_blocking', 'frame_gpu_create'):
        if 'fns' not in ctx:
            ctx['fns'] = fn_ranges(arr, ctx['toks'], ctx['kw'])
        pattern = BLOCKING if rule == 'frame_blocking' else GPU_CREATE
        hits = []
        for name, start, end in ctx['fns']:
            if FRAME_FN.match(name):
                hits.extend(at(m.start()) for m in pattern.finditer(arr, start, end))
        return len(hits), hits
    if rule == 'uncached_cube_scan':
        if 'fns' not in ctx:
            ctx['fns'] = fn_ranges(arr, ctx['toks'], ctx['kw'])
        if 'loops' not in ctx:
            ctx['loops'] = span_index(loop_bodies(arr, ctx['toks'], ctx['kw']))
        hits = []
        for match in GET_BLOCK.finditer(arr):
            if loop_depth(ctx['loops'], match.start()) < 3:
                continue
            body = next((arr[s:e] for n, s, e in ctx['fns'] if s <= match.start() < e), b'')
            if CACHE_HINT.search(body):
                continue  # inside a named cache miss path — the intended shape
            hits.append(at(match.start()))
        return len(hits), hits
    if rule == 'vec_remove_zero':
        hits = [at(m.start()) for m in REMOVE_ZERO.finditer(arr)]
        return len(hits), hits
    if rule == 'server_thread_sleep':
        hits = [at(m.start()) for m in THREAD_SLEEP.finditer(arr)]
        return len(hits), hits
    if rule in ('hot_loop_map_clone', 'hot_loop_systemtime'):
        if 'loops' not in ctx:
            ctx['loops'] = span_index(loop_bodies(arr, ctx['toks'], ctx['kw']))
        if rule == 'hot_loop_systemtime':
            hits = [at(m.start()) for m in SYSTEM_TIME.finditer(arr)
                    if loop_depth(ctx['loops'], m.start()) >= 1]
            return len(hits), hits
        hits = []
        for match in CLONE_CALL.finditer(arr):
            if loop_depth(ctx['loops'], match.start()) < 1:
                continue
            recv = CLONE_RECEIVER.search(arr, max(0, match.start() - 120), match.start())
            if recv is not None and HEAVY_RECEIVER.search(recv.group(1)):
                hits.append(at(match.start()))
        return len(hits), hits
    raise ValueError(f'unknown rule {rule}')


def non_decl_uses(arr, symbol, literal=None):
    """Count non-test, non-import, non-declaration occurrences of `symbol`.

    `literal` is a plain-bytes fast path for `\bX\b` patterns: `bytearray.find`
    hits C speed and word boundaries are checked on the surrounding bytes.
    """
    uses = 0
    if literal is not None:
        pos = 0
        n = len(literal)
        while True:
            pos = arr.find(literal, pos)
            if pos < 0:
                return uses
            before = arr[pos - 1] if pos else 0
            after = arr[pos + n] if pos + n < len(arr) else 0
            if not (before in _WORD or after in _WORD):
                if not USE_STMT.search(arr[max(0, pos - 200):pos]) and not DECL_HEAD.search(
                        arr[max(0, pos - 24):pos]):
                    uses += 1
            pos += n
    for match in re.finditer(symbol, arr):
        if USE_STMT.search(arr[max(0, match.start() - 200):match.start()]):
            continue
        if DECL_HEAD.search(arr[max(0, match.start() - 24):match.start()]):
            continue
        uses += 1
    return uses


def audit(root, baseline):
    masked = {}
    findings = []
    improvements = []
    counts = {}

    def ctx_of(path):
        if path not in masked:
            arr, toks = masked_source(root / path)
            masked[path] = {'arr': arr, 'toks': toks}
        return masked[path]

    bootstrap = not baseline
    expected = {rule: set(files) for rule, files, _ in RULES}
    for rule, files, message in RULES:
        table = baseline.get(rule)
        if not bootstrap and (not isinstance(table, dict) or set(table) != expected[rule]):
            raise ValueError(f'baseline table {rule!r} missing or has wrong file set')
        counts[rule] = {}
        for path in files:
            count, hits = rule_counts(ctx_of(path), rule, path)
            counts[rule][path] = count
            limit = count if bootstrap else table[path]
            if count > limit:
                for line, excerpt in hits[limit:]:
                    findings.append(dict(
                        rule=rule, file=path, line=line, excerpt=excerpt, hint=message))
            elif count < limit:
                improvements.append(dict(rule=rule, file=path, current=count, baseline=limit))

    for label, symbol, candidates, mode in WIRED:
        resolved = sorted({
            p.relative_to(root).as_posix()
            for pattern in candidates
            for p in (sorted(root.glob(pattern)) if '*' in pattern
                      else [root / pattern])
            if p.is_file()})
        literal = re.fullmatch(rb'\\b([A-Za-z_]\w*)\\b', symbol)
        literal = literal.group(1) if literal else None
        uses = {}
        for path in resolved:
            if path in masked:
                uses[path] = non_decl_uses(masked[path]['arr'], symbol, literal)
                continue
            raw = (root / path).read_bytes()
            present = (literal in raw) if literal is not None else (
                re.search(symbol, raw) is not None)
            if not present:
                uses[path] = 0
                continue
            uses[path] = non_decl_uses(ctx_of(path)['arr'], symbol, literal)
        if mode == 'any':
            if not any(uses.values()):
                findings.append(dict(
                    rule='unwired_optimizer', file=' | '.join(candidates), line=0,
                    excerpt=label,
                    hint=f'{label} has no non-test production call site — optimizer is dead code'))
        else:
            for path in resolved:
                if not uses.get(path):
                    findings.append(dict(
                        rule='unwired_optimizer', file=path, line=0, excerpt=label,
                        hint=f'{label} lost its non-test production call site in {path}'))
    findings.sort(key=lambda f: (f['file'], f['line'], f['rule']))
    return dict(ok=not findings, files_scanned=len(masked), findings=findings,
                improvements=improvements, counts=counts)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--json', action='store_true')
    parser.add_argument('--update-baseline', action='store_true')
    parser.add_argument('--fail-on-stale', '--strict', dest='fail_on_stale',
                        action='store_true',
                        help='also fail when a baseline ceiling can be tightened')
    args = parser.parse_args(argv)
    root = args.root.resolve()
    baseline_path = root / BASELINE
    try:
        if not root.is_dir() or not (root / 'src').is_dir():
            raise ValueError('root must contain a src/ directory')
        if baseline_path.exists():
            data = json.loads(baseline_path.read_text(encoding='utf-8'))
            baseline = {rule: data[rule] for rule, _, _ in RULES}
            for rule, table in baseline.items():
                if (not isinstance(table, dict)
                        or any(not isinstance(k, str) or type(v) is not int or v < 0
                               for k, v in table.items())):
                    raise ValueError(f'baseline {rule} must be nonnegative ints by file')
        elif args.update_baseline:
            baseline = {}
        else:
            raise ValueError(f'missing baseline: {baseline_path}')
        report = audit(root, baseline)
        if args.update_baseline and report['ok']:
            new = {}
            for rule, files, _ in RULES:
                old = baseline.get(rule, {})
                new[rule] = {p: min(report['counts'][rule][p], old.get(p, report['counts'][rule][p]))
                             for p in files}
            baseline_path.parent.mkdir(parents=True, exist_ok=True)
            baseline_path.write_text(
                json.dumps(new, indent=2, sort_keys=True) + '\n', encoding='utf-8')
            report['improvements'] = []
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        print(f'perf antipattern guard: {error}', file=sys.stderr)
        return 2
    stale = bool(report['improvements']) and args.fail_on_stale
    ok = report['ok'] and not stale
    if args.json:
        report['ok'] = ok
        print(json.dumps(report, indent=2))
    else:
        print(f"perf antipattern guard: {'PASS' if ok else 'FAIL'} "
              f"({report['files_scanned']} Rust files, {len(report['findings'])} findings)")
        for finding in report['findings']:
            where = f"{finding['file']}:{finding['line']}" if finding['line'] else finding['file']
            print(f"  {where}: {finding['rule']}: {finding['excerpt']}\n    {finding['hint']}")
        for item in report['improvements']:
            print(f"  DOWN {item['file']} [{item['rule']}]: "
                  f"{item['current']} < {item['baseline']}; tighten the baseline")
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
