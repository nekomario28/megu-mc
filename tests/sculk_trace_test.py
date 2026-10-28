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
        return type('A', (), {'native': str(native), 'port': str(port), 'window': window})

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
            self.assertIn("identical prefix from native index 0: 4 of 4", out.getvalue())
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
            self.assertIn("identical prefix from native index 0: 4 of 5", out.getvalue())
            self.assertIn("first divergence: port (16, 7) native (49, 6)", out.getvalue())
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


if __name__ == "__main__":
    unittest.main()
