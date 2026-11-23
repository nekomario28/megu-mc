# Live client perf investigation — 2026-11-23

Setup: `megu-client-gpu --connect 127.0.0.1:25599 --gameDir ~/.megu-mc --version 1.21.1`
against `target/release/megu` (vanilla datapack worldgen, seed 0, view distance 8),
vanilla client jar `~/.megu-mc/versions/1.21.1/1.21.1.jar` (sha1-pinned Mojang download).
Host: Ryzen 7 5700X + RTX 2080 SUPER, KDE Plasma Wayland. Reference baseline
(user screenshot): vanilla/Fabric 1.21.1 ≈ 81 fps @ rd12, tick ~5.2 ms.

## Finding P1 — server kicks the shipped client: chunk-batch ack timeout (repro x3)

`[spine] PerfProbe disconnected: chunk batch acknowledgement timeout` on every
join. Server side: after the first ack, each `sent_batch` arms a **15 s**
deadline (`src/chunk_ack.rs:24`). Client side: the ack is sent on the network
worker (`client-gpu/src/live_session.rs:3421`) immediately on
`ChunkBatchFinished`, BUT the same worker forwards every decoded event into the
net→render `sync_channel(256)` (live_session.rs:857). During join the render
thread is busy compiling the vanilla atlas + all WGSL pipelines and building
the first meshes, so the channel fills, `tx.send` blocks, the worker never
reaches `ChunkBatchFinished`, and the server kicks.

- Raising the channel to 4096 (diagnostic patch, worktree-only) let the client
  survive join — supports backpressure as the mechanism, though the deeper fix
  is not blocking the ack path (ack before the forwarding enqueue, or
  prioritize ack/keepalive writes).
- Client also echoed `count` as its requested rate (live_session.rs:3422),
  asking for the whole window per batch — vanilla asks for its measured
  consumption rate. Diagnostic patch caps it at 10.0 (worktree-only).
- `ACK_TIMEOUT` is hard-coded 15 s; no env override exists.

## Finding P2 — join collapses server TPS to ~6 (max tick work 1.87 s)

While streaming the initial chunk window: `[tick] observed_tps=5.99
avg_work_ms=130.39 max_work_ms=1871.92 over_budget=22/100`. After settle:
TPS 20, avg <5 ms. The per-chunk send path is the hot spot — existing
`MEGU_CHUNK_PROFILE=1` note in src/main.rs:13893 says one delivered chunk costs
21,619–29,536 `block_at` calls (one world lock each), 32–62 ms per chunk.
This is the dominant server perf defect observed end-to-end.

## Finding P3 — invisible window presents 0 frames (Wayland)

With the client window off the visible desktop (unfocused workspace), zero
`queue.present` calls were recorded over >60 s (fps meter instrumented at
window.rs:2309, worktree-only diagnostic). The JIT/frame-callback pacing
(`pre_present_notify`, `request_redraw` on deadline) waits for a
`wl_surface.frame` callback the compositor never sends for an invisible
surface — so the client does literally nothing instead of an offscreen-tick
fallback. Correct as a power feature, but it means the window *must* be
visible to measure; automated headless FPS measurement needs a
non-frame-callback mode.

## Finding P2b — CHUNKPROF breakdown (fresh world, 270 chunks streamed)

`MEGU_CHUNK_PROFILE=1` during the join storm:

```
CHUNKPROF n=270 map=49.6ms data=22.1 section=10.3 light=18.3 (sky=15.4 block=2.8)
             floors=11.1 x1.3  block_at=4778 miss=4499   (94% miss!)
  read/chunk: n=1747 ms=10.15   band_path n=78 ms=10.10
  encode/chunk: hidden=146 reads=1747   encode=10.69ms section_encode=0.02ms
  phases/chunk: biomes=0.89ms band_install=0.02ms relight 0.9/chunk 2.2ms/chunk
  floors: calls=1.27/chunk cells=860/chunk
```

Per-chunk total ≈ **100 ms** of work: worldgen `map=49.6ms` dominates, then
`data=22.1ms` (the 1,747 `block_at` reads at ~10 ms via the band path), then
`light=18.3ms` (mostly sky light), `section=10.3ms` encode. `block_at`
cache-miss rate is 94 % (4,499/4,778) — the fast path almost never hits.

Generation is running **inside the tick** — one join tick measured
`max_work_ms=1067` (single tick over 1 s). Vanilla generates on worker
threads off the tick loop; here serial generation+pump collapses TPS to ~6.
Steady-state tick after streaming settles: `avg_work_ms≈0.05`.

## Finding P4 — visual/audio parity gaps seen on the way in

- `.mcmeta` animations "not applied" for every animated block texture (fire,
  water, lava, portals, campfire, kelp, seagrass, sculk…) — all such textures
  render static.
- `megu-client audio: pipewire sink failed; continuing on null` — the PipeWire
  backend is wired but fails at runtime on this system.
- Server log spams `[fluid] deferred native spring tick: unsupported fluid
  shape minecraft:glow_lichen` repeatedly for the same positions — unhandled
  fluid shape + noisy logging.
- `mobs=0` at first join (later 17–20) — spawn warmup is very slow or
  spawn attempts fail silently early on.

## P1 fix — verified live (2026-11-23)

Root cause confirmed: the ack is written at read time, but the worker loop
blocks on `tx.send` for every *forwarded* event (chunk data, teleport, sounds)
when the net→render `sync_channel(256)` is full — which it is during join
while the render thread compiles the atlas/pipelines. The loop freezes, the
next `ChunkBatchFinished` is never read, and the server's 15 s ack deadline
expires.

Fix in `client-gpu/src/live_session.rs` `run_connection`: forwarding is now
non-blocking — `try_send` into the channel, overflow into a bounded
`VecDeque` (`PENDING_CAP=8192`), drained opportunistically at the top of
each loop. Channel resized 256→1024; batch-rate request clamped to
`min(count,10)`. Read/ack path is unchanged (was already prompt).

Live verification (`PerfProbe2`, same server/world that kicked 3/3 times):
**0 disconnects in >60 s**, server `[tick] observed_tps=20.00
avg_work_ms≈2`, no backlog-cap error. Previously every join ended in
`chunk batch acknowledgement timeout`.

## Finding P3 corrected — "zero presents" was a MEASUREMENT artifact + a real spin bug (2026-11-23)

Two separate phenomena were conflated earlier; disentangled:

1. **Spectacle/xdg-desktop-portal screenshots return pure WHITE for the
   client's Vulkan dmabuf surface** on this RADV+KWin setup. Ground-truth
   readback (`MEGU_FRAME_DUMP`, COPY_SRC surface + copy_texture_to_buffer +
   PPM) proves the presented frames contain real rendered content (warm
   gradient/grid scene), not white. The client renderer works on this
   machine (AMD RX 7800 XT, Ryzen 7 9800X3D — NOTE: the vanilla 81 fps
   baseline screenshot was a DIFFERENT machine: 5700X + RTX 2080 SUPER).
2. **Wayland frame-callback starvation is real and confirmed**: on
   `cap=0` (uncapped/vsync-off), `about_to_wait` calls
   `window.request_redraw()` + `ControlFlow::Poll` — winit 0.30.13 only
   emits `RedrawRequested` when `frame_callback_state() != Requested`
   (event_loop/mod.rs:486-496), i.e. only after the compositor returns a
   `wl_surface.frame` callback. While the surface is fully occluded KWin
   withholds callbacks → `RedrawRequested` never re-fires → the loop
   spins `about_to_wait` at ~1.5M iterations/sec (measured n=34,000,000
   in 22 s) burning a full core doing nothing. winit never delivers
   `WindowEvent::Occluded` on Wayland, so `self.occluded` stays false and
   the zero-GPU sleep path never engages. **On X11 (`env -u
   WAYLAND_DISPLAY`) the same client renders continuously at ~59 fps
   (vsync) / ~6000 fps (uncapped)** — X11 `request_redraw` is unconditional
   channel delivery.
3. `WINIT_UNIX_BACKEND` was REMOVED in winit 0.29 — setting it does
   nothing; every "x11" attempt earlier was still native Wayland. Force
   X11 by unsetting `WAYLAND_DISPLAY`.

Fix direction for the spin bug: track `last_redraw_delivered`; when
request_redraw has been pending > ~1 s without a RedrawRequested (the
occluded-on-Wayland signature), fall back to `WaitUntil` pacing instead
of Poll — restores the intended zero-GPU-ish sleep on hidden windows.

Measured perf (X11/XWayland, 1280x720): demo scene 59 fps vsync-capped;
live world connect 58.5 fps vsync-capped, 0 disconnects after the P1 fix.

**Spin-fix verified (native Wayland, occluded window):** about_to_wait now
falls back to `WaitUntil(33 ms)` + `live.poll` after 500 ms without a
delivered RedrawRequested — whole-process CPU dropped from ~113% of a
core to ~13.5%, connection stays up (clients=1), and the Poll fast path
returns automatically when callbacks resume.

## Diagnostic patches left in worktree (not committed)

- live_session.rs P1 fix — real fix (see above), ready to commit
- window.rs:2309 `[fps]` meter — present counter, still useful for the
  user to eyeball FPS on the visible desktop; remove or gate before commit
