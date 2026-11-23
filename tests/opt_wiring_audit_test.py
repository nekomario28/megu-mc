#!/usr/bin/env python3
"""Fixture tests for tools/opt_wiring_audit.py.

Drives the real entry point against synthetic Rust trees to prove the gate
fails on the defect class it exists for: an optimization whose only remaining
call sites are test-only or commented out — dead code that still compiles.
"""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
AUDIT = ROOT / "tools/opt_wiring_audit.py"


def put(root, name, content):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def run(root, *args):
    return subprocess.run(
        [sys.executable, str(AUDIT), "--root", str(root), *args],
        text=True, capture_output=True, check=False, timeout=60,
    )


WINDOW = """\
fn drive(live: &mut Live, r: &mut R, u: &mut U) {
    live.apply_compute_strategy(r);
    u.render_into(1, 2, &mut v);
    r.sync_external_sections_from(live);
    r.set_entity_and_ber_meshes(vec![]);
    r.set_particle_billboards(&[]);
}
"""

RENDERER = """\
fn cull() {
    megu_core::simd_voxel::batch_aabb_frustum_cull_soa(a, b, c);
}
fn depth(p: bool, h: bool) -> u8 {
    let _ = select_depth_store_op(p, h);
    wgpu::StoreOp::Discard as u8
}
fn probe(f: wgpu::Features) -> bool {
    f.contains(wgpu::Features::SHADER_F16) && f.contains(wgpu::Features::SUBGROUP)
}
struct A {
    force_fallback_adapter: bool,
}
"""

LIVE_SESSION = """\
fn drain() {
    let _job = MeshJob::SectionMesh {};
    let _res = MeshResult::SectionMesh {};
}
"""

MESH = """\
fn pack_strips4(rows: &[u16], base: usize) -> u64 {
    base as u64
}
fn mesher(rows: &[u16]) -> u64 {
    pack_strips4(rows, 0)
}
"""

SIMD_FILE = """\
#[target_feature(enable = "avx2")]
unsafe fn kernel_avx2(x: &[f32]) -> f32 {
    x[0]
}

fn dispatch(x: &[f32]) -> f32 {
    if std::arch::is_x86_feature_detected!("avx2") {
        return unsafe { kernel_avx2(x) };
    }
    x[0]
}
"""

PLAY_PROTOCOL = """\
enum SpawnDescriptor {
    Typed { eid: i32 },
}
struct Tracked {
    spawn: SpawnDescriptor,
}
struct InterestChunkIndex;
impl InterestChunkIndex {
    fn new(_: &[Tracked]) -> Self {
        InterestChunkIndex
    }
}
struct SharedBroadcastFrame;
impl SharedBroadcastFrame {
    fn new(_: i64, _: Vec<u8>) -> Self {
        SharedBroadcastFrame
    }
}
mod stream_gen {
    pub fn request(_d: i32, _x: i32, _z: i32) -> u8 {
        0
    }
}
fn sync(ents: &[Tracked]) {
    let _d = SpawnDescriptor::Typed { eid: 1 };
    for e in ents {
        let _ = e.spawn.packets();
    }
    let _i = InterestChunkIndex::new(ents);
    let _f = SharedBroadcastFrame::new(1, vec![]);
    let _t = stream_gen::request(0, 0, 0);
}
impl SpawnDescriptor {
    fn packets(&self) -> u8 {
        0
    }
}
"""

MAIN = """\
mod tick_workers {}
fn tick() {
    let _ = tick_workers::mob_plans();
}
"""


def make_root(root):
    put(root, "client-gpu/src/window.rs", WINDOW)
    put(root, "client-gpu/src/renderer.rs", RENDERER)
    put(root, "client-gpu/src/live_session.rs", LIVE_SESSION)
    put(root, "client-port/src/mesh.rs", MESH)
    put(root, "core/src/simd_voxel.rs", SIMD_FILE)
    put(root, "src/worldgen.rs", SIMD_FILE)
    put(root, "src/generation_math.rs", SIMD_FILE)
    put(root, "src/play_protocol.rs", PLAY_PROTOCOL)
    put(root, "src/main.rs", MAIN)


def check_status(root, check_id):
    result = run(root, "--json")
    assert result.returncode in (0, 1), result.stdout + result.stderr
    data = json.loads(result.stdout)
    return data["checks"][check_id]["ok"], result


class FixtureTests(unittest.TestCase):
    def test_fully_wired_fixture_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            result = run(root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("20/20 invariants wired", result.stdout)

    def test_call_moved_into_cfg_test_fails(self):
        """apply_compute_strategy surviving only under #[cfg(test)] must FAIL."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            put(root, "client-gpu/src/window.rs", """\
fn drive(live: &mut Live, r: &mut R, u: &mut U) {
    u.render_into(1, 2, &mut v);
    r.sync_external_sections_from(live);
    r.set_entity_and_ber_meshes(vec![]);
    r.set_particle_billboards(&[]);
}
#[cfg(test)]
mod tests {
    fn t(live: &mut Live, r: &mut R) {
        live.apply_compute_strategy(r);
    }
}
""")
            ok, _ = check_status(root, "opt-gpu-compute-strategy")
            self.assertFalse(ok)
            human = run(root)
            self.assertEqual(human.returncode, 1)
            self.assertIn("FAIL opt-gpu-compute-strategy", human.stdout)

    def test_commented_out_call_fails(self):
        """A commented-out pack_strips4 call must FAIL the meshing gate."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            put(root, "client-port/src/mesh.rs", """\
fn pack_strips4(rows: &[u16], base: usize) -> u64 {
    base as u64
}
fn mesher(rows: &[u16]) -> u64 {
    // pack_strips4(rows, 0)
    0
}
""")
            ok, result = check_status(root, "opt-pack-strips4-bitboard")
            self.assertFalse(ok)
            self.assertEqual(result.returncode, 1)

    def test_string_literal_call_does_not_count(self):
        """The call name inside a string literal must not satisfy the gate."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            put(root, "client-port/src/mesh.rs", """\
fn mesher(rows: &[u16]) -> u64 {
    let _doc = "pack_strips4(rows, 0)";
    0
}
""")
            ok, _ = check_status(root, "opt-pack-strips4-bitboard")
            self.assertFalse(ok)

    def test_unguarded_target_feature_call_fails(self):
        """A #[target_feature] kernel called without a runtime probe FAILs."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            unguarded = """\
#[target_feature(enable = "avx2")]
unsafe fn kernel_avx2(x: &[f32]) -> f32 {
    x[0]
}

fn dispatch(x: &[f32]) -> f32 {
    unsafe { kernel_avx2(x) }
}
"""
            put(root, "core/src/simd_voxel.rs", unguarded)
            ok, result = check_status(root, "opt-simd-guard-simd-voxel")
            self.assertFalse(ok)
            self.assertEqual(result.returncode, 1)

    def test_test_only_kernel_call_fails(self):
        """A kernel whose only caller is #[cfg(test)] is dead code — FAIL."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            test_only = """\
#[target_feature(enable = "avx2")]
unsafe fn kernel_avx2(x: &[f32]) -> f32 {
    x[0]
}

fn dispatch(x: &[f32]) -> f32 {
    x[0]
}

#[cfg(test)]
mod tests {
    fn t(x: &[f32]) -> f32 {
        unsafe { kernel_avx2(x) }
    }
}
"""
            put(root, "src/worldgen.rs", test_only)
            ok, _ = check_status(root, "opt-simd-guard-worldgen")
            self.assertFalse(ok)

    def test_guarded_transitive_dispatch_passes(self):
        """Kernel <- unsafe helper <- detected dispatch is a legal shape."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            transitive = """\
#[target_feature(enable = "avx2")]
unsafe fn kernel_avx2(x: &[f32]) -> f32 {
    x[0]
}

unsafe fn helper(x: &[f32]) -> f32 {
    kernel_avx2(x)
}

fn dispatch(x: &[f32]) -> f32 {
    if std::arch::is_x86_feature_detected!("avx2") {
        return unsafe { helper(x) };
    }
    x[0]
}
"""
            put(root, "src/generation_math.rs", transitive)
            ok, _ = check_status(root, "opt-simd-guard-generation-math")
            self.assertTrue(ok)


class RealRepoTests(unittest.TestCase):
    def test_real_repository_passes_100_percent(self):
        result = subprocess.run(
            [sys.executable, str(AUDIT)],
            text=True, capture_output=True, check=False, timeout=120,
            cwd=ROOT)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("20/20 invariants wired", result.stdout)

    def test_json_report_covers_every_check(self):
        result = subprocess.run(
            [sys.executable, str(AUDIT), "--json"],
            text=True, capture_output=True, check=False, timeout=120,
            cwd=ROOT)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(result.stdout)
        self.assertEqual(data["failed"], 0)
        self.assertEqual(data["passed"], 20)
        for check_id, entry in data["checks"].items():
            with self.subTest(check=check_id):
                self.assertTrue(entry["ok"], entry["detail"])


if __name__ == "__main__":
    unittest.main()
