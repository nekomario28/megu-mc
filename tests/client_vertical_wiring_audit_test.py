#!/usr/bin/env python3
"""Fixture tests for tools/client_vertical_wiring_audit.py.

Drives the real entry point against synthetic Rust trees to prove the gate
actually fails on the defect class it exists for: half-wired vertical slices
(S2C events dropped by `_ => {}`, C2S commands only ever sent from tests,
HUD fields nobody writes, screens nothing can reach).
"""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
AUDIT = ROOT / "tools/client_vertical_wiring_audit.py"
WAIVERS = ROOT / "specs/client-vertical-wiring-waivers.json"


def put(root, name, content):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def run(root, *args):
    return subprocess.run(
        [sys.executable, str(AUDIT), "--root", str(root), *args],
        text=True, capture_output=True, check=False, timeout=60,
    )


def waivers_file(root, entries):
    path = root / "waivers.json"
    path.write_text(json.dumps({"version": 1, "waivers": entries}))
    return ["--waivers", str(path)]


def empty_waivers(root):
    return waivers_file(root, [])


PROTOCOL = """\
pub enum PlayEvent {
    Handled { x: i32 },
    Dropped,
}
"""

WORLD = """\
use crate::protocol::PlayEvent;
impl WorldState {
    pub fn apply_play_event(&mut self, event: &PlayEvent) {
        match event {
            PlayEvent::Handled { .. } => {}
            _ => {}
        }
    }
}
"""

LIVE_SESSION = """\
enum WorkerCommand {
    Ping,
    Place,
}
struct LiveSession { outgoing: Sender<WorkerCommand> }
impl LiveSession {
    pub fn send_ping(&mut self) {
        let _ = self.outgoing.try_send(WorkerCommand::Ping);
    }
    pub fn place(&mut self) {
        let _ = self.outgoing.try_send(WorkerCommand::Place { pos: [0; 3] });
    }
    fn apply_play(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::Handled { .. } => {}
            _ => {}
        }
    }
    fn worker(&mut self, cmd: WorkerCommand) {
        match cmd {
            WorkerCommand::Ping => {}
            WorkerCommand::Place { .. } => {}
        }
    }
}
#[cfg(test)]
mod tests {
    fn drives_place() {
        session.place();
    }
}
"""

WINDOW = """\
fn drive(live: &mut LiveSession, ui: &mut UiState) {
    live.send_ping();
    ui.screen = Screen::Title;
    ui.hud.health = 20.0;
    live.hud.health = 20.0;
}
"""

UI_STATE = """\
pub enum Screen {
    Title,
    Dead,
}
pub struct HudState {
    pub health: f32,
    pub armor: u8,
}
"""

DEATH_SCREEN = "pub struct DeadScreenState;\n"


def make_root(root):
    put(root, "client-app/src/protocol.rs", PROTOCOL)
    put(root, "client-app/src/world.rs", WORLD)
    put(root, "client-gpu/src/live_session.rs", LIVE_SESSION)
    put(root, "client-gpu/src/window.rs", WINDOW)
    put(root, "client-ui/src/state.rs", UI_STATE)
    put(root, "client-ui/src/death_screen.rs", DEATH_SCREEN)


def gaps(report):
    out = {}
    for check, entry in report["checks"].items():
        out[check] = {item["id"] for item in entry["unwaived"]}
    return out


def report(root, *extra):
    result = run(root, *extra, "--json")
    assert result.returncode in (0, 1, 2), result.stderr
    return result, json.loads(result.stdout)


class FixtureTests(unittest.TestCase):
    def test_every_half_wired_surface_is_reported(self):
        """The fixture has exactly one gap per check; all four must surface."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            result, data = report(root, *empty_waivers(root), "--report-only")
            found = gaps(data)
            self.assertEqual(found["s2c_play_event_consumption"],
                             {"PlayEvent::Dropped"})
            self.assertEqual(found["worker_command_producers"],
                             {"WorkerCommand::Place"})
            self.assertEqual(found["hud_state_writers"], {"HudState::armor"})
            self.assertEqual(found["screen_state_reachability"],
                             {"Screen::Dead", "DeadScreenState"})

    def test_unwaived_gaps_fail_by_default(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            result = run(root, *empty_waivers(root))
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("UNWIRED PlayEvent::Dropped", result.stdout)
            self.assertIn("UNWIRED WorkerCommand::Place", result.stdout)
            self.assertIn("UNWIRED HudState::armor", result.stdout)
            self.assertIn("UNWIRED DeadScreenState", result.stdout)

    def test_test_only_producer_call_does_not_count(self):
        """place() is only invoked under #[cfg(test)] — it must still fail."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            result, data = report(root, *empty_waivers(root), "--report-only")
            item = data["checks"]["worker_command_producers"]
            ids = {w["id"] for w in item["unwaived"]}
            self.assertIn("WorkerCommand::Place", ids)
            self.assertNotIn("WorkerCommand::Ping", ids)

    def test_fully_waived_fixture_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            entries = [
                {"check": "s2c_play_event_consumption", "id": "PlayEvent::Dropped",
                 "priority": "P1", "reason": "fixture gap"},
                {"check": "worker_command_producers", "id": "WorkerCommand::Place",
                 "priority": "P0", "reason": "fixture gap"},
                {"check": "hud_state_writers", "id": "HudState::armor",
                 "priority": "P1", "reason": "fixture gap"},
                {"check": "screen_state_reachability", "id": "Screen::Dead",
                 "priority": "P1", "reason": "fixture gap"},
                {"check": "screen_state_reachability", "id": "DeadScreenState",
                 "priority": "P0", "reason": "fixture gap"},
            ]
            result = run(root, *waivers_file(root, entries))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_fixed_item_reports_stale_waiver(self):
        """Waiving a wired item is stale: reported, fatal only on --fail-on-stale."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            entries = [
                {"check": "s2c_play_event_consumption", "id": "PlayEvent::Handled",
                 "priority": "P1", "reason": "stale: actually wired"},
            ]
            result = run(root, *waivers_file(root, entries))
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("STALE WAIVER", result.stdout)
            # --fail-on-stale also fails (there are other unwaived gaps, so
            # isolate the stale path by waiving everything else too).
            entries += [
                {"check": "s2c_play_event_consumption", "id": "PlayEvent::Dropped",
                 "priority": "P1", "reason": "gap"},
                {"check": "worker_command_producers", "id": "WorkerCommand::Place",
                 "priority": "P0", "reason": "gap"},
                {"check": "hud_state_writers", "id": "HudState::armor",
                 "priority": "P1", "reason": "gap"},
                {"check": "screen_state_reachability", "id": "Screen::Dead",
                 "priority": "P1", "reason": "gap"},
                {"check": "screen_state_reachability", "id": "DeadScreenState",
                 "priority": "P0", "reason": "gap"},
            ]
            result = run(root, *waivers_file(root, entries))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("STALE WAIVER", result.stdout)
            result = run(root, *waivers_file(root, entries), "--fail-on-stale")
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_malformed_waiver_entry_rejects(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            entries = [{"check": "nope", "id": "x", "priority": "P9"}]
            result = run(root, *waivers_file(root, entries))
            self.assertEqual(result.returncode, 2, result.stdout + result.stderr)


class RealRepoTests(unittest.TestCase):
    def test_current_tree_passes_with_committed_waivers(self):
        result = subprocess.run(
            [sys.executable, str(AUDIT)],
            text=True, capture_output=True, check=False, timeout=120, cwd=ROOT)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("0 unwaived", result.stdout)

    def test_json_counts_cover_every_item(self):
        result = subprocess.run(
            [sys.executable, str(AUDIT), "--json"],
            text=True, capture_output=True, check=False, timeout=120, cwd=ROOT)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(result.stdout)
        for check, entry in data["checks"].items():
            with self.subTest(check=check):
                counts = entry["counts"]
                self.assertEqual(counts["unwaived"], 0)
                self.assertGreater(counts["wired"], 0)


if __name__ == "__main__":
    unittest.main()
