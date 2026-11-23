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
fn pick_shader(shader_f16: bool) -> u32 {
    let _ = lit_wgsl(shader_f16);
    0
}
"""

LIVE_SESSION = """\
fn drain() {
    let _job = MeshJob::SectionMesh {};
    let _res = MeshResult::SectionMesh {};
}
fn poll() {
    let _ = dispatch_kernel_mode();
    let _ = select_arm();
    let _ = prefetch_boost_map();
    push_tick();
}
"""

SUBWORLD_RENDER = """\
struct TerrainVertex {
    packed_normal: u32,
    packed_albedo: u32,
}
const _: () = assert!(
    std::mem::size_of::<TerrainVertex>() == 32,
    "TerrainVertex must stay 32 bytes"
);
fn pack_normal_octahedral(n: [f32; 3]) -> u32 { 0 }
fn unpack_normal_octahedral(p: u32) -> [f32; 3] { [0.0; 3] }
fn pack_albedo_unorm8x4(a: [f32; 3], f: u8) -> u32 { 0 }
fn unpack_albedo_unorm8x4(p: u32) -> [f32; 3] { [0.0; 3] }
const LIT_FRAGMENT_F16_WGSL: &str = "";
fn lit_wgsl_f16() -> String { String::new() }
fn lit_wgsl(shader_f16: bool) -> String {
    if shader_f16 {
        lit_wgsl_f16()
    } else {
        String::new()
    }
}
fn vertex() -> TerrainVertex {
    TerrainVertex {
        packed_normal: pack_normal_octahedral([0.0; 3]),
        packed_albedo: pack_albedo_unorm8x4([0.0; 3], 0),
    }
}
fn decode(v: &TerrainVertex) -> ([f32; 3], [f32; 3]) {
    (unpack_normal_octahedral(v.packed_normal), unpack_albedo_unorm8x4(v.packed_albedo))
}
"""

MESH = """\
fn pack_strips4(rows: &[u16], base: usize) -> u64 {
    base as u64
}
const AO_LUT: [u16; 256] = [0; 256];
unsafe fn shade_fast(idx: usize) -> u16 {
    AO_LUT[idx]
}
fn mesher(rows: &[u16]) -> u64 {
    pack_strips4(rows, 0) + unsafe { shade_fast(0) } as u64
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
fn nbt_wire_skip(data: &[u8], p: &mut usize, depth: usize) -> bool {
    true
}
fn nbt_wire_value(data: &[u8], p: &mut usize, tag: u8, depth: usize) -> bool {
    true
}
fn live_scaled_tick_budget(base: usize, scale: f32) -> usize {
    base
}
fn live_server_budget_scale() -> f32 {
    1.0
}
fn wire_consumer(body: &[u8]) {
    let mut p = 0usize;
    let _ = nbt_wire_skip(body, &mut p, 0);
    let _ = nbt_wire_value(body, &mut p, 1, 0);
    let _ = megu_core::nbt_wire::skip_nbt_payload(
        1,
        body,
        megu_core::nbt_wire::NbtWireLimits::default(),
    );
    let _ = live_scaled_tick_budget(10, live_server_budget_scale());
}
"""

WORLDGEN = """\
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

struct MeguTaskPool;
fn bg_task_pool() -> MeguTaskPool {
    MeguTaskPool
}
macro_rules! bg_scope {
    ($f:expr) => {{
        let _ = bg_task_pool();
        $f
    }};
}
fn prefetch_cols_with() {
    bg_scope!(|s| {
        let _ = s;
    });
}
"""

MAIN = """\
mod tick_workers {}
const MAP_SHARDS: usize = 64;
struct ShardedChunks<V> {
    shards: [u8; MAP_SHARDS],
}
impl<V> ShardedChunks<V> {
    fn new() -> Self {
        Self { shards: [0; MAP_SHARDS] }
    }
}
fn chunk_shard_of(key: u32) -> usize {
    (key as usize) & (MAP_SHARDS - 1)
}
struct HotSig {
    warm_radius: i32,
    centers: u32,
    forced: u32,
}
struct Region {
    sig: Option<HotSig>,
}
static HOT_REBUILDS: u64 = 0;
fn refresh_hot(r: &mut Region, warm_radius: i32, centers: u32, forced_set: u32) {
    if let Some(sig) = &r.sig {
        if sig.warm_radius == warm_radius
            && sig.centers == centers
            && sig.forced == forced_set
        {
            return;
        }
    }
    HOT_REBUILDS.wrapping_add(1);
}
fn world() -> ShardedChunks<u32> {
    ShardedChunks::new()
}
fn shard_read(k: u32) -> usize {
    chunk_shard_of(k)
}
fn prefetch_group() {
    let _ = worldgen::prefetch_cols_with();
    let _ = worldgen::prefetch_terrain();
}
fn tick() {
    let _ = tick_workers::mob_plans();
    refresh_hot(&mut Region { sig: None }, 5, 0, 0);
}
"""

CONNECTION_LIMITS = """\
struct ZlibCompressScratch;
struct ZlibDecompressScratch;
impl ZlibCompressScratch {
    fn compress_at(&mut self) {}
}
impl ZlibDecompressScratch {
    fn inflate_exact(&mut self) {}
}
fn with_zlib_scratch() {}
fn with_zlib_inflate() {}
fn encode() {
    let mut c = ZlibCompressScratch;
    c.compress_at();
    with_zlib_scratch();
}
fn decode() {
    let mut d = ZlibDecompressScratch;
    d.inflate_exact();
    with_zlib_inflate();
}
"""

LIVE_MOD_BRIDGE = """\
use std::sync::atomic::{AtomicU32, Ordering};
static LIVE_SERVER_BUDGET_SCALE: AtomicU32 = AtomicU32::new(0);
fn live_server_budget_scale() -> f32 {
    1.0
}
fn live_observe_server_tick_mspt(_mspt: f64) {
    LIVE_SERVER_BUDGET_SCALE.store(1.0f32.to_bits(), Ordering::Relaxed);
}
struct Factory;
impl Factory {
    fn step_tick(&mut self, _tick: u64, _steps: usize, _us: Option<u64>) {}
}
struct Rt {
    factory: Factory,
}
fn live_create_tick_body(rt: &mut Rt, tick: u64) {
    rt.factory.step_tick(tick, 1_024, None);
}
"""

NBT_SKIP_CALLER = """\
fn skip_payload(id: u8, remaining: &[u8]) -> usize {
    megu_core::nbt_wire::skip_nbt_payload(id, remaining, megu_core::nbt_wire::NbtWireLimits::default())
        .unwrap_or(0)
}
"""

INFLATE_CALLER = """\
fn inflate_member(payload: &[u8]) -> usize {
    megu_core::zip_archive::inflate_raw_rfc1951_with_consumed(payload, 8)
        .map(|(out, n)| out.len() + n)
        .unwrap_or(0)
}
"""

REGION_SCHEDULER = """\
struct RegionScheduler {
    awake: std::collections::BTreeSet<u32>,
    sleeping: std::collections::BTreeMap<u32, u64>,
}
struct WorkerPool;
impl WorkerPool {
    fn new() -> Self {
        WorkerPool
    }
}
impl RegionScheduler {
    fn refresh_region_sleep(&mut self, _key: u32) {}
    fn advance_active_parallel_for(
        &mut self,
        _keys: &std::collections::BTreeSet<u32>,
        _tick: u64,
        _budget: u32,
        _threads: usize,
    ) -> u32 {
        0
    }
    fn step(&mut self) {
        self.refresh_region_sleep(0);
        let keys = self.awake.clone();
        let _ = self.advance_active_parallel_for(&keys, 0, 0, 1);
        let _ = WorkerPool::new();
        let _ = self.sleeping.len();
    }
}
"""

FACTORY = """\
struct RegionScheduler;
impl RegionScheduler {
    fn advance_active_parallel_for(
        &mut self,
        _keys: u32,
        _tick: u64,
        _budget: u32,
        _threads: usize,
    ) -> u32 {
        0
    }
}
struct CreateFactory {
    scheduler: RegionScheduler,
    idle_export_skips: u64,
}
impl CreateFactory {
    fn step_tick_parallel(&mut self, tick: u64) -> u32 {
        let _ = self
            .scheduler
            .advance_active_parallel_for(0, tick, 0, 1);
        self.idle_export_skips += 1;
        0
    }
    fn step_tick(&mut self, tick: u64) -> u32 {
        self.step_tick_parallel(tick)
    }
}
"""


def make_root(root):
    put(root, "client-gpu/src/window.rs", WINDOW)
    put(root, "client-gpu/src/renderer.rs", RENDERER)
    put(root, "client-gpu/src/live_session.rs", LIVE_SESSION)
    put(root, "client-gpu/src/subworld_render.rs", SUBWORLD_RENDER)
    put(root, "client-port/src/mesh.rs", MESH)
    put(root, "core/src/simd_voxel.rs", SIMD_FILE)
    put(root, "src/worldgen.rs", WORLDGEN)
    put(root, "src/generation_math.rs", SIMD_FILE)
    put(root, "src/play_protocol.rs", PLAY_PROTOCOL)
    put(root, "src/main.rs", MAIN)
    put(root, "src/connection_limits.rs", CONNECTION_LIMITS)
    put(root, "src/live_mod_bridge.rs", LIVE_MOD_BRIDGE)
    put(root, "proto/src/wire.rs", NBT_SKIP_CALLER)
    put(root, "state-store/src/world_interop.rs", NBT_SKIP_CALLER)
    put(root, "mod-offload/src/region_scheduler.rs", REGION_SCHEDULER)
    put(root, "create-adapter/src/factory.rs", FACTORY)
    put(root, "client-kernel/src/gzip.rs", INFLATE_CALLER)
    put(root, "server-files/src/pack_loader.rs", INFLATE_CALLER)
    put(root, "state-store/src/anvil_inspect.rs", INFLATE_CALLER)


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
            self.assertIn("44/44 invariants wired", result.stdout)

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

    def test_dispatch_kernel_mode_test_only_fails(self):
        """Challenger probing surviving only under #[cfg(test)] must FAIL."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            put(root, "client-gpu/src/live_session.rs", """\
fn drain() {
    let _job = MeshJob::SectionMesh {};
    let _res = MeshResult::SectionMesh {};
}
fn poll() {
    let _ = select_arm();
    let _ = prefetch_boost_map();
    push_tick();
}
#[cfg(test)]
mod tests {
    fn t() {
        let _ = dispatch_kernel_mode();
    }
}
""")
            ok, result = check_status(root, "opt-kernel-challenger-probe")
            self.assertFalse(ok)
            self.assertEqual(result.returncode, 1)

    def test_sharded_chunks_removed_fails(self):
        """Deleting the ShardedChunks wiring must FAIL the server gate."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            put(root, "src/main.rs", """\
mod tick_workers {}
struct HotSig {
    warm_radius: i32,
}
struct Region {
    sig: Option<HotSig>,
}
static HOT_REBUILDS: u64 = 0;
fn tick() {
    let _ = tick_workers::mob_plans();
    let _ = HOT_REBUILDS;
}
""")
            ok, _ = check_status(root, "opt-sharded-chunks")
            self.assertFalse(ok)
            ok, _ = check_status(root, "opt-refresh-hot-sig-skip")
            self.assertFalse(ok)

    def test_zlib_scratch_test_only_fails(self):
        """Zlib scratch pool calls only under #[cfg(test)] must FAIL."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            put(root, "src/connection_limits.rs", """\
struct ZlibCompressScratch;
struct ZlibDecompressScratch;
fn encode() {}
#[cfg(test)]
mod tests {
    fn t() {
        with_zlib_scratch();
        with_zlib_inflate();
    }
}
""")
            ok, result = check_status(root, "opt-zlib-scratch-pool")
            self.assertFalse(ok)
            self.assertEqual(result.returncode, 1)

    def test_nbt_skip_delegation_removed_fails(self):
        """Dropping the megu_core::nbt_wire delegation must FAIL."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_root(root)
            put(root, "src/play_protocol.rs", """\
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
fn nbt_wire_skip(data: &[u8], p: &mut usize, depth: usize) -> bool {
    true
}
fn nbt_wire_value(data: &[u8], p: &mut usize, tag: u8, depth: usize) -> bool {
    true
}
fn live_scaled_tick_budget(base: usize, scale: f32) -> usize {
    base
}
fn live_server_budget_scale() -> f32 {
    1.0
}
fn wire_consumer(body: &[u8]) {
    let mut p = 0usize;
    let _ = nbt_wire_skip(body, &mut p, 0);
    let _ = nbt_wire_value(body, &mut p, 1, 0);
    let _ = live_scaled_tick_budget(10, live_server_budget_scale());
}
""")
            ok, _ = check_status(root, "opt-nbt-wire-skip-play")
            self.assertFalse(ok)


class RealRepoTests(unittest.TestCase):
    def test_real_repository_passes_100_percent(self):
        result = subprocess.run(
            [sys.executable, str(AUDIT)],
            text=True, capture_output=True, check=False, timeout=120,
            cwd=ROOT)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("44/44 invariants wired", result.stdout)

    def test_json_report_covers_every_check(self):
        result = subprocess.run(
            [sys.executable, str(AUDIT), "--json"],
            text=True, capture_output=True, check=False, timeout=120,
            cwd=ROOT)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(result.stdout)
        self.assertEqual(data["failed"], 0)
        self.assertEqual(data["passed"], 44)
        for check_id, entry in data["checks"].items():
            with self.subTest(check=check_id):
                self.assertTrue(entry["ok"], entry["detail"])


if __name__ == "__main__":
    unittest.main()
