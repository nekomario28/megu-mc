# `WARM=1` login-timeout root-cause diagnostic (2026-11-20)

This is a **measurement and triage receipt**. No server, driver, bench, or test code was
changed. It records the exact failure trace, the exact server-side log, and a controlled
reproduction with explicit timeouts and timestamped stderr capture.

## Identity and provenance

| item | value |
|---|---|
| Port / branch / HEAD | `t3/b6-structure-capture-triage` / `9a1cc1303cddaf5f8ebbdfcaf49a1b975c433d21` |
| Rebuilt binary | `target/debug/mc-spine`, `cargo build --offline --locked` **PASS** (12.45 s), SHA-256 `07d33d53742e687d64d9c4fb87f6285a497306ba142db583bd01ba2c7d7605b0` |
| Tools | `tools/load_client.py` SHA-256 `d79746c847873912686dd1adcffdc85bcc60b7d07fd1e1b91796535de4262c3b`; `tools/load_compare.sh`; `tools/perf_bench.py` SHA-256 `7c731c52c8c5b4e831af1cc1bae1a51638104b45892a066de2f26a49b267891f` |
| Temp policy | `mkdir -p target/tmp`; `TMPDIR="$PWD/target/tmp"`; `JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData"` where Java ran |
| Raw artifacts (outside Git) | `/home/yuu/mc-spine-local/captures/matrix-b6/warm-diag/` — `original-spine.log`, `original-spine-load.txt`, `original-van-load.txt`, `spine.log`, `loadwarm.txt`, `loada.txt`, `spine.stdout` |

**Correction to the task wording.** `tools/perf_bench.py --help` exposes only
`--binary/--arms/--jobs/--rounds/--dump-n/--profile-counters/--compare-binary/.../--json/--markdown`;
it has **no `WARM` switch** and takes no `server.jar`. `WARM=1` belongs exclusively to
`tools/load_compare.sh` (lines 63-66 of that script). The earlier failure was in the
`WARM=1` *load-compare* run, so that is the route reproduced here.

## Original observed failure (recorded in `m3-wave2-bench-20261120.md`)

Command: `WARM=1 TMPDIR="$PWD/target/tmp" bash tools/load_compare.sh 20 0`.
The vanilla arm was valid (`LOAD ok packets=2097 chunks_moved=90`, RSS 1169.0 MiB).
The mc-spine arm printed `LOAD failed:` followed by the traceback below. Its server log
(`original-spine.log`, SHA-256 `5ee7871457b1df9a865de9c366f25aca3cfc072593b521c545aa6f9fd9b72979`)
contains, in order:

```
mc-spine v0.6 listening 127.0.0.1:25602 (offline identities; no Mojang authentication)
[sz] t=21 ... clients=0
[sz] t=24 ... clients=0
[spine] LoadWarm joined from 127.0.0.1 (uuid 1d3224a8ce8e3a658a6738dbb7de1c00)
[spine] LoadWarm left (from 127.0.0.1)
[sz] t=52 ... inv=1 clients=0
```

There is **no `LoadA joined` line, no `[security] ... refused` / `duplicate login refused`
line, and no `LoadA disconnected` line**. The measured client died before the server
could finish admitting it.

## Exact client trace (both the original and the reproduction)

```
Traceback (most recent call last):
  File ".../tools/load_client.py", line 129, in <module>
    main()
  File ".../tools/load_client.py", line 121, in main
    client.login()
  File ".../tools/load_client.py", line 75, in login
    if self.read(deadline - time.monotonic())[0] == 0x0C:
       ~~~~~~~~~^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  File ".../tools/load_client.py", line 42, in read
    length = recv_vint(self.sock)
  File ".../tests/wire_smoke.py", line 51, in recv_vint
    b = recv_exact(sock, 1)[0]
  File ".../tests/wire_smoke.py", line 41, in recv_exact
    chunk = sock.recv(n - len(out))
TimeoutError: timed out
```

Reproduced byte-for-byte on this HEAD in `loadwarm.txt` and `loada.txt` (both SHA-256
`32425eb49953546b2dc206d4763f0c4556bb495b876512eac9db9e3c5f2a2355`).

## Controlled reproduction on `target/debug/mc-spine`

A fresh work dir under `matrix-b6/warm-diag/spine/` symlinked `datapacks`, `assets` and
`config_packets.bin` from the checkout. The debug server was started on port `25612` with
`MC_SPINE_SEED=0`, `MC_SPINE_RESOURCE_PACK_URL=""`; stderr was timestamped into
`spine.log` (SHA-256 `1eae60c9206e62c4afbff244e7b89b50db58e8b215971e694402a42c017541db`).
Drivers were bounded with explicit `timeout 60 ... LoadWarm` and `timeout 40 ... LoadA`.

| wall clock | event |
|---|---|
| 21:16:26.436 | `listening 127.0.0.1:25612` |
| 21:16:26.557 | `LoadWarm` driver started |
| 21:16:41.621 | `LoadWarm` driver exited rc=1 — line-75 `TimeoutError` (≈15 s client deadline) |
| 21:17:14.661 | `LoadA` driver started. The harness's `wait for "LoadWarm left"` loop ran its full 30 s (the server's stderr was block-buffered into `spine.log`, so the line was not visible yet), then `sleep 3`; the `joined`/`left` lines only reached the log at 21:17:28.350. |
| 21:17:28.126 | server `[sz] t=36 ... clients=0` |
| 21:17:28.350 | server `[spine] LoadWarm joined ...` — **≈62 s after connect, ≈47 s after the client gave up** |
| 21:17:28.350 | server `[spine] LoadWarm left ...` |
| 21:17:29.719 | `LoadA` driver exited rc=1 — same line-75 `TimeoutError` (15 s) |

Again: no refusal, duplicate-login, allowlist, or disconnect log line appeared for either
client. The debug binary is slow enough that even the *warm* client's first window outran
its 15 s deadline, while its server-side handler kept working for another ~47 s.

## Root cause (what the trace actually says)

1. **TCP was not the failure.** `RawClient.__init__` uses
   `socket.create_connection(("127.0.0.1", port), timeout=10).` Execution reached
   `login()` and then `read()`, so the TCP handshake and the handshake/login-success/config
   exchanges all completed. The phrase "TCP handshake timed out" is a description of the
   symptom, not the observed layer.
2. **The timeout is application-level.** `load_client.py:60` sets one
   `deadline = time.monotonic() + 15` for the whole login, and line 75 waits for packet
   `0x0C` (Chunk Batch Finished). The server sends `0x0C` at the end of the *first*
   `stream_chunks` batch (`src/main.rs:20233`), after generating and streaming the initial
   view-4 window (9×9 = 81 chunks) plus registry/config exchanges. Cold first-window
   worldgen/streaming exceeded that fixed 15 s budget.
3. **It is slowness, not a deadlock or a policy refusal.** The server-side handler
   continued and emitted `LoadWarm joined` ~47 s after the client abandoned it; the accept
   loop, connection cap (64), per-IP cap, `LOGINS` duplicate guard, and allowlist all behaved
   normally (no refusal log). In the original release build the warm client *did* join
   within 15 s, but after it logged out and saved (`inv=1`), the second client's cold first
   window again exceeded 15 s — so the `WARM=1` measured window never contained a valid
   `LoadA` login.
4. **Consequence.** The `mc-spine 31.900 270.2 6` line in the earlier bench is **not** a
   valid representative-load RSS/CPU data point. The only valid current-tree observation is
   that the second login times out; a corrected `WARM=1` RSS number is **UNKNOWN / NOT RUN**
   from these receipts. `receipt.json`'s 122.1 MiB was measured without `WARM=1`.

No server-side timing constant was changed, no driver deadline was relaxed, and no gate was
weakened. Determining and fixing the warm-first-window latency is a separate code task and
was deliberately out of scope here.
