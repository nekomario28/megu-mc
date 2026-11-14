#!/usr/bin/env python3
"""The sculk trace workbench has to refuse the traces that cannot answer the question.

Every one of these cases is a shape that actually occurred while porting `sculk_patch`, and each
cost real time because the trace *looked* healthy: markers present, thousands of patch lines,
hundreds of thousands of `SDRAW` lines. The failure mode is never "no data" - it is data about
something else.
"""
import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("sculk_trace", ROOT / "tools/sculk_trace.py")
st = importlib.util.module_from_spec(spec)
spec.loader.exec_module(st)

# A healthy ordered trace, in the layout the agent's own writer produces: the placement modifiers
# draw the position list immediately *before* the marker of the first position they fill.
HEALTHY = """\
SPASS -16 112
SDRAW nextLong()@dzz -8209572278615141403
SDRAW next(bits)@dzx 1191763649
SDRAW nextInt(bound)@dyn 8
SDRAW nextLong()@dzz -3078376962530789668
SDRAW next(bits)@dzx 1789113403
SDRAW nextInt(bound)@dyn 1
SPLACE edi -16 112
SRNG dzx
SPATCH -248 -63 1816
SDRAW nextInt(bound)@dyn 3
SCURSOR r=0 i=0 -248 -63 1816 32 1 0 -1
SPLACE edi -16 112
SRNG dzx
SPATCH -244 -23 1819
SCURSOR r=0 i=0 -244 -23 1819 32 1 0 -1
SPASS -15 112
SDRAW nextInt(bound)@dyn 5
SPLACE edi -15 112
SRNG dzx
SPATCH -231 -40 1820
SCURSOR r=0 i=0 -231 -40 1820 32 1 0 -1
"""

# The shape that cost an hour: armed one level too deep (`ConfiguredFeature.place` instead of
# `PlacedFeature.placeWithBiomeCheck`), so the windows hold the spreader's draws - plenty of
# `SDRAW` lines, and none of them the position draws the comparison needed.
ARMED_TOO_DEEP = """\
SPASS -16 112
SPLACE edi -16 112
SRNG dzx
SPATCH -248 -63 1816
SDRAW nextLong()@dzz -8209572278615141403
SDRAW next(bits)@dzx 1191763649
SDRAW nextInt(bound)@dyn 3
SCURSOR r=0 i=0 -248 -63 1816 32 1 0 -1
SPLACE edi -16 112
SRNG dzx
SPATCH -244 -23 1819
SCURSOR r=0 i=0 -244 -23 1819 32 1 0 -1
"""

# No draw hooks in the build at all.
NO_DRAWS = """\
SPASS -16 112
SPLACE edi -16 112
SRNG dzx
SPATCH -248 -63 1816
SCURSOR r=0 i=0 -248 -63 1816 32 1 0 -1
"""

GAME_LOG = """\
[21:49:44] [Worker-Main-7/INFO]: [STDOUT]: SPASS -16 112
[21:49:44] [Worker-Main-7/INFO]: [STDOUT]: SPLACE edi -16 112
[21:49:44] [Worker-Main-7/INFO]: [STDOUT]: SPATCH -248 -63 1816
"""


def write(tmp, name, text):
    path = Path(tmp) / name
    path.write_text(text)
    return path


class ReadingTraces(unittest.TestCase):
    def test_game_log_prefix_is_stripped_and_flagged(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "run.log", GAME_LOG)
            self.assertTrue(st.reads_unordered(path))
            self.assertEqual(st.trace_lines(path),
                             ["SPASS -16 112", "SPLACE edi -16 112", "SPATCH -248 -63 1816"])

    def test_ordered_trace_is_not_flagged(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "trace.txt", HEALTHY)
            self.assertFalse(st.reads_unordered(path))


class Grouping(unittest.TestCase):
    def test_passes_split_on_spass(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", HEALTHY)
            table = st.pass_table(path)
            self.assertEqual(list(table), [(-16, 112), (-15, 112)])
            self.assertEqual(len(table[(-16, 112)]['attempts']), 2)
            self.assertEqual(len(table[(-16, 112)]['spreads']), 2)

    def test_windows_leading_and_trailing(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", HEALTHY)
            lines = st.trace_lines(path)
            trailing = st.window_summary(st.split_windows(lines, arm='trailing'))
            leading = st.window_summary(st.split_windows(lines, arm='leading'))
            # trailing: the window holds what follows its marker - here, the patch it announced
            self.assertEqual(trailing[0]['cls'], 'edi')
            self.assertEqual(len(trailing[0]['patches']), 1)
            # ... and the draws after the marker, which for a sculk placement are the spreader's
            self.assertEqual(trailing[0]['draws'], ['3'])
            # leading: shifted by one, so the window holds the draws made *before* its marker -
            # for this layout those are the placement modifiers' position draws
            self.assertEqual(leading[0]['patches'], [])
            # every draw made before the first marker: three logical draws, each reported by the
            # three hooks they pass through (raw generator, legacy mixin, and the parallel stream)
            self.assertEqual(leading[0]['draws'], ['-8209572278615141403', '1191763649', '8',
                                                   '-3078376962530789668', '1789113403', '1'])

    def test_patch_splitting_and_round_indexing(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", HEALTHY)
            patches = st.split_patches(st.trace_lines(path))
            self.assertEqual(len(patches), 3)
            self.assertEqual(patches[0][0], (-248, -63, 1816))
            self.assertEqual(len(patches[0][1]), 1)


class ThreadTags(unittest.TestCase):
    """The agent tags each line with its thread: the file order is lock order, not program order."""

    TAGGED = """\
[w=Worker-Main-5] SPASS -16 112
[w=Worker-Main-5] SPLACE edi -16 112
[w=Worker-Main-7] SPASS -15 112
[w=Worker-Main-5] SPATCH -248 -63 1816
"""

    def test_tag_is_stripped_from_marker_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", self.TAGGED)
            self.assertEqual(st.trace_lines(path)[:2], ["SPASS -16 112", "SPLACE edi -16 112"])

    def test_lines_group_per_thread_in_file_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", self.TAGGED)
            by_thread = st.lines_by_thread(path)
            self.assertEqual(list(by_thread), ['Worker-Main-5', 'Worker-Main-7'])
            self.assertEqual(by_thread['Worker-Main-5'],
                             ["SPASS -16 112", "SPLACE edi -16 112", "SPATCH -248 -63 1816"])
            self.assertEqual(by_thread['Worker-Main-7'], ["SPASS -15 112"])

    def test_untagged_lines_have_no_thread(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", "SPASS -16 112\n")
            self.assertIsNone(st.thread_of("SPASS -16 112"))
            self.assertEqual(st.lines_by_thread(path), {})


class Sanity(unittest.TestCase):
    """The gate: a trace must be refused when it cannot answer the question asked of it."""

    def test_healthy_trace_is_sane(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", HEALTHY)
            self.assertEqual(st.sanity_problems(path), [])

    def test_armed_one_level_too_deep_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", ARMED_TOO_DEEP)
            problems = st.sanity_problems(path)
            self.assertTrue(problems, "a trace whose windows hold only spreader draws is unusable")
            self.assertIn("PlacedFeature.placeWithBiomeCheck", " ".join(problems))

    def test_missing_draw_hooks_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", NO_DRAWS)
            problems = st.sanity_problems(path)
            self.assertTrue(problems)
            self.assertIn("no `SDRAW` lines", problems[0])

    def test_game_log_is_refused_for_positional_questions(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "run.log", GAME_LOG)
            problems = st.sanity_problems(path)
            self.assertTrue(any("this is a game log" in p for p in problems))

    def test_unknown_feature_class_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            text = HEALTHY.replace("edi", "zzz")
            path = write(tmp, "t", text)
            problems = st.sanity_problems(path)
            self.assertTrue(any("absent from the feature table" in p for p in problems))

    def test_required_marker_is_enforced(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", NO_DRAWS)
            problems = st.sanity_problems(path, required=['SPASS', 'SSPREADALL'])
            self.assertTrue(any("SSPREADALL" in p for p in problems))


class PortPhaseMixing(unittest.TestCase):
    def test_two_port_phases_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            text = "SPASS -16 112\n" + NO_DRAWS[NO_DRAWS.index('SPLACE'):] + \
                   "[wgprof] 1x1 chunks in 1ms\nSPATCH -1 -2 3\nDRAW 0 f=1,7 nextInt(16)=15\n" \
                   "[wgdump] 1 chunks\n"
            path = write(tmp, "port.log", text)
            problems = st.sanity_problems(path)
            self.assertTrue(any("two port phases" in p for p in problems), problems)


class Names(unittest.TestCase):
    def test_class_maps_to_its_types(self):
        self.assertEqual(st.class_names('edi'), ['sculk_patch'])
        self.assertEqual(st.class_of_type('minecraft:sculk_patch'), 'edi')

    def test_ambiguous_class_is_reported_as_such(self):
        # one class serves several feature types; that is why a trace's class column cannot be
        # turned back into a placed feature id on its own
        self.assertGreater(len(st.class_names('edc')), 1)

    def test_unknown_class_is_empty(self):
        self.assertEqual(st.class_names('zzz'), [])


class PortDraws(unittest.TestCase):
    def test_port_draw_lines_parse(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "port.log", "DRAW 0 f=1,7 nextInt(16)=15\nDRAW 1 f=1,7 nextInt(321)=280\n")
            self.assertEqual(st.parse_port_draws(path), [(1, 7, 'nextInt(16)', '15'),
                                                         (1, 7, 'nextInt(321)', '280')])


def args(native, port, limit=1):
    return type('A', (), {'native': str(native), 'port': str(port), 'limit': limit})


class CompareDraws(unittest.TestCase):
    """Joining on (bound, value): the bound is what makes two draws the same question."""

    NATIVE = """\
SPASS -16 112
SDRAW nextInt(bound)@dyn(16) 15
SDRAW nextInt(bound)@dyn(16) 11
SDRAW nextInt(bound)@dyn(321) 280
SDRAW nextInt(bound)@dyn(16) 12
SDRAW nextInt(bound)@dyn(49) 6
SPLACE edi -16 112
SRNG dzx
SPATCH -248 -63 1816
"""

    def _args(self, native, port, window=0):
        return type('A', (), {'native': str(native), 'port': str(port), 'window': window,
                             'split': 'thread'})

    def test_float_comparison_uses_the_native_type_not_an_absolute_tolerance(self):
        self.assertTrue(st.same_draw(('nextFloat', True, 0.107397735),
                                     ('nextFloat', True, 0.10739773511886597)))
        self.assertFalse(st.same_draw(('nextFloat', True, 0.5),
                                      ('nextFloat', True, 0.5000000596046448)))
        self.assertFalse(st.same_draw(('nextDouble', True, 0.5),
                                      ('nextDouble', True, 0.5000000000000001)))

    def test_raw_source_draws_are_separate_and_reported_not_silently_dropped(self):
        with tempfile.TemporaryDirectory() as tmp:
            native = write(tmp, 'native.txt', self.NATIVE)
            port = write(tmp, 'port.log', 'DRAW 0 f=1,7 nextInt(16)=15\n'
                         'RAW_DRAW 1 f=1,7 nextFloat=0.8254984021186829\n'
                         'DRAW 2 f=1,7 nextInt(16)=11\n'
                         'DRAW 3 f=1,7 nextInt(321)=280\n')
            self.assertEqual(st.draws_of_port(port, source='RAW_DRAW'),
                             [('nextFloat', True, 0.8254984021186829)])
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_draws(self._args(native, port))
            self.assertEqual(rc, 0)
            self.assertIn("excluded raw-source draws: 1 {'nextFloat': 1}", out.getvalue())
            self.assertIn('identical prefix there: 3 of 3', out.getvalue())

    def test_matching_prefix_is_found_and_its_length_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            native = write(tmp, "native.txt", self.NATIVE)
            port = write(tmp, "port.log", "DRAW 0 f=1,7 nextInt(16)=15\n"
                                          "DRAW 1 f=1,7 nextInt(16)=11\n"
                                          "DRAW 2 f=1,7 nextInt(321)=280\n"
                                          "DRAW 3 f=1,7 nextInt(16)=12\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_draws(self._args(native, port, window=3))
            self.assertIn("at that unit's draw 0", out.getvalue())
            self.assertIn("identical prefix there: 4 of 4", out.getvalue())
            self.assertEqual(rc, 0)

    def test_divergence_reports_the_two_draws(self):
        with tempfile.TemporaryDirectory() as tmp:
            native = write(tmp, "native.txt", self.NATIVE)
            port = write(tmp, "port.log", "DRAW 0 f=1,7 nextInt(16)=15\n"
                                          "DRAW 1 f=1,7 nextInt(16)=11\n"
                                          "DRAW 2 f=1,7 nextInt(321)=280\n"
                                          "DRAW 3 f=1,7 nextInt(16)=12\n"
                                          "DRAW 4 f=1,7 nextInt(16)=7\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_draws(self._args(native, port, window=3))
            self.assertIn("at that unit's draw 0", out.getvalue())
            self.assertIn("identical prefix there: 4 of 5", out.getvalue())
            self.assertIn("first divergence: port ('nextInt(16)', False, 7) "
                          "native ('nextInt(49)', False, 6)", out.getvalue())
            self.assertEqual(rc, 1)

    def test_absent_sequence_is_reported_as_absent(self):
        with tempfile.TemporaryDirectory() as tmp:
            native = write(tmp, "native.txt", self.NATIVE)
            port = write(tmp, "port.log", "DRAW 0 f=1,7 nextInt(64)=63\n"
                                          "DRAW 1 f=1,7 nextInt(64)=1\n"
                                          "DRAW 2 f=1,7 nextInt(64)=2\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_draws(self._args(native, port, window=3))
            self.assertIn("does not occur", out.getvalue())
            self.assertEqual(rc, 1)


class CompareBiomef(unittest.TestCase):
    """Candidate sequence and verdicts: the set can match while the order does not."""

    NATIVE = """\
BIOMEF -241 216 1803 false feature=1,7
BIOMEF -244 92 1798 false feature=1,7
BIOMEF -256 134 1806 true feature=1,7
BIOMEF -248 81 1795 true feature=1,7
BIOMEF -230 10 1799 false feature=1,7
"""

    def _args(self, native, port, feature='1,7'):
        return type('A', (), {'native': str(native), 'port': str(port), 'feature': feature})

    def test_identical_sides_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            native = write(tmp, "native.txt", self.NATIVE)
            port = write(tmp, "port.log",
                         self.NATIVE.replace("feature=1,7", "biome=minecraft:forest feature=1,7"))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_biomef(self._args(native, port))
            self.assertEqual(rc, 0)
            self.assertIn("sequence identical: True", out.getvalue())

    def test_reordered_candidates_are_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            lines = self.NATIVE.splitlines()
            lines[0], lines[3] = lines[3], lines[0]
            native = write(tmp, "native.txt", "\n".join(lines) + "\n")
            port = write(tmp, "port.log", self.NATIVE)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_biomef(self._args(native, port))
            self.assertEqual(rc, 1)
            self.assertIn("sequence identical: False", out.getvalue())
            self.assertIn("first order difference", out.getvalue())

    def test_a_different_verdict_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            native = write(tmp, "native.txt", self.NATIVE)
            port = write(tmp, "port.log",
                         self.NATIVE.replace("-256 134 1806 true", "-256 134 1806 false"))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_biomef(self._args(native, port))
            self.assertEqual(rc, 1)
            self.assertIn("verdict disagreements on aligned positions: 1", out.getvalue())


class ComparePasses(unittest.TestCase):
    def test_identical_passes_report_as_identical(self):
        with tempfile.TemporaryDirectory() as tmp:
            native = write(tmp, "native.txt", HEALTHY)
            port = write(tmp, "port.txt", HEALTHY)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_passes(args(native, port))
            self.assertIn("identical sculk attempt set: 2 of 2", out.getvalue())
            self.assertEqual(rc, 0)

    def test_divergence_sets_exit_code(self):
        with tempfile.TemporaryDirectory() as tmp:
            native = write(tmp, "native.txt", HEALTHY)
            port = write(tmp, "port.txt", HEALTHY.replace("SPATCH -244 -23 1819",
                                                          "SPATCH -245 -23 1819"))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = st.cmd_compare_passes(args(native, port))
            self.assertEqual(rc, 1)
            self.assertIn("identical sculk attempt set: 1 of 2", out.getvalue())
            self.assertIn("divergent pass (trace order)", out.getvalue())


class CliRefusals(unittest.TestCase):
    """The command line must refuse a game log for a positional question, not answer anyway."""

    def test_passes_refuses_a_game_log(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "run.log", GAME_LOG)
            done = subprocess.run([sys.executable, str(ROOT / "tools/sculk_trace.py"), "passes",
                                   str(path)], capture_output=True, text=True)
            self.assertEqual(done.returncode, 2)
            self.assertIn("not program order", done.stderr)

    def test_sanity_exits_nonzero_on_a_broken_trace(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "t", ARMED_TOO_DEEP)
            done = subprocess.run([sys.executable, str(ROOT / "tools/sculk_trace.py"), "sanity",
                                   str(path)], capture_output=True, text=True)
            self.assertEqual(done.returncode, 1)
            self.assertIn("PROBLEM", done.stderr)


class PopOrder(unittest.TestCase):
    def test_extracts_spass_and_pop_chunk_with_bbox_and_refuses_game_log(self):
        with tempfile.TemporaryDirectory() as tmp:
            trace = write(tmp, "trace.txt",
                          "[w=Worker-1] SPASS 3 -1\n"
                          "[w=Worker-2] POP_CHUNK 3 -2\n"
                          "[w=Worker-1] POP_CHUNK 9 9\n"
                          "[w=Worker-2] POP_CHUNK 3 -1\n"
                          "[w=Worker-2] POP_CHUNK 3 -2\n")
            self.assertEqual(st.extract_pop_order(trace), [(3, -2), (9, 9), (3, -1)])
            self.assertEqual(st.extract_pop_order(trace, bbox=(2, -3, 4, -1)), [(3, -2), (3, -1)])

            spass_only = write(tmp, "spass.txt", HEALTHY)
            self.assertEqual(st.extract_pop_order(spass_only), [(-16, 112), (-15, 112)])

            log = write(tmp, "run.log", GAME_LOG)
            refused = subprocess.run([sys.executable, str(ROOT / "tools/sculk_trace.py"),
                                      "pop-order", str(log)], capture_output=True, text=True)
            self.assertEqual(refused.returncode, 2)
            self.assertIn("not program order", refused.stderr)

            allowed = subprocess.run([sys.executable, str(ROOT / "tools/sculk_trace.py"),
                                      "pop-order", str(log), "--allow-unordered"],
                                     capture_output=True, text=True)
            self.assertEqual(allowed.returncode, 0)
            self.assertEqual(allowed.stdout, "-16\t112\n")


if __name__ == "__main__":
    unittest.main()
