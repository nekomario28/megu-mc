# CF-1b Headless Differential Client Parity — 2026-11-20

## Tested inputs and method

- Repository source HEAD before this harness commit was `5759cbef5e5bf9f809365c6b2a4cc2fd9d9d57a9`, branch `t3/vanilla-client-parity`. `cargo build --offline --locked` produced `target/debug/mc-spine`.
- Official Minecraft Java 1.21.1 `server.jar` at `/home/yuu/tmpx/carverfresh/server.jar`, verified SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`, ran under `/usr/lib/jvm/java-21-openjdk/bin/java` with `-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData`.
- Both targets ran on loopback using Protocol **767**, an offline `ParityProbe` account, seed **4242**, creative mode, isolated disposable worlds, the same scripted player packets, and the same RCON arena commands. Both were set to peaceful difficulty and creative game mode through RCON before arena actions. The Rust server used `MC_SPINE_WG=old` flat terrain; Vanilla used `level-type=minecraft:flat`. Their initial terrain/spawn generation differs. The controlled arena was placed at matching absolute coordinates before action testing.
- Client frames, login/configuration handshake, chunk/light palette decoding and block-state IDs reuse the repository's Protocol 767 test implementation. The harness handles Vanilla's compression and relative-position flags, acknowledges teleports, and queries each server's authoritative player position over RCON after every scripted movement packet. No desktop client, authenticated Minecraft account, or externally hosted server was used.
- Comparison is deliberately strict. Each target must independently satisfy the scripted acceptance invariants. Any observed bootstrap packet-content or initial chunk-delivery difference marks its scenario `MISMATCH` even when required packets and decoding are present. Self-consistency runs two separate Rust server instances.

## Observed scenario results

| Scenario | Live mc-spine | Official Vanilla 1.21.1 | Result |
| --- | --- | --- | --- |
| Login → Configuration → Play, initial border `0x25`, advancements `0x74`, player position `0x40` | All required packets present; border parsed | All required packets present; border parsed | **MISMATCH** — advancement content, compression/login sequence, initial position |
| Chunk and light `0x27` decode | 25 decoded/active chunks, 24 sections, 4096 states per section, origin present | 45 decoded/active chunks, 24 sections, 4096 states per section, origin present | **MISMATCH** — 25 versus 45 initial chunks |
| Grounded flat walk | 6/6 accepted; end `(2.94, 240, 1.5)` | 6/6 accepted; same end | **PASS** — 0 correction `0x40` packets; max server-position delta `0.0` |
| Sprint | 6/6 accepted; end `(4.74, 240, 1.5)` | 6/6 accepted; same end | **PASS** — 0 corrections; delta `0.0` |
| Jump arc | 12/12 accepted; apex Y `241.252203` | 12/12 accepted; same apex | **PASS** — 0 corrections; delta `0.0` |
| Place stone | `0x09` state ID `1` at `(6, 240, 1)` | `0x09` state ID `1` at same position | **PASS** |
| Break stone | `0x09` state ID `0` at `(6, 240, 1)` | `0x09` state ID `0` at same position | **PASS** |
| Enter still-water column | 3/3 accepted; water state ID `80`; end `(9.5, 244.8, 2.5)` | 3/3 accepted; identical state/end | **PASS** — 0 corrections; delta `0.0` |
| Submerged scripted tick positions | 8/8 accepted; Y `243.5` → `243.40402847798026`; water state ID `80` | 8/8 accepted; same positions/state | **PASS** — 0 corrections; max per-step coordinate delta `0.0`, tolerance `0.03` blocks |

**Dual-target result — 7/9 PASS, 2 MISMATCH.** Both targets independently accepted all 35 movement packets (6 + 6 + 12 + 3 + 8) with zero server correction teleports and `0.0` maximum difference between requested and authoritative coordinates. The two block operations emitted the expected `0x09` updates on both servers. The Rust-only two-server self-consistency run passed **9/9**.

## Exact observed Vanilla divergences

**Bootstrap / `MISMATCH`**

| Observed field | mc-spine (all runs) | Vanilla run 1 | Vanilla run 2 | Vanilla run 3 (after common difficulty setup) |
| --- | --- | --- | --- | --- |
| Login-phase packet IDs | `0x02` | `0x03`, `0x02` | `0x03`, `0x02` | `0x03`, `0x02` |
| Compression threshold | absent (`null`) | `256` bytes (`0x03` Set Compression) | `256` bytes | `256` bytes |
| First advancements `0x74` body size | `1417` bytes | `274` bytes | `274` bytes | `274` bytes |
| First advancements `0x74` body SHA-256 | `6c1301ff4ed5d0b1dbb1f6b137f317b2cf84cee35160fc03a8d6275c5239e2b5` | `7b01b49088b6c686e4bb8fd22047681345e7043ad1510ae85ac0e342726800b9` | `bcfd147f49a497cc6913fcb6188314172c7813a32a4472acff89ba1ba8882bb0` | `4a4b85ee0fb5a559fc54549bf50f24bdfc345cd5fd9044300fb6887206562f54` |
| First initial player position `0x40` | `(0.5, 16.0, 0.5)` | `(7.5, -60.0, 7.5)` | `(3.5, -60.0, -3.5)` | `(-9.5, -60.0, 0.5)` |

Both `0x25` border payloads decode identically to center `(0,0)`, size `59999968`, maximum size `29999984`, warning distance `5`, warning time `15`, and zero interpolation duration. Both initial `0x40` frames had raw flags `0`. Both targets completed login and configuration. The three independently executed Vanilla sessions differed in their spawn coordinates and advancement-body hashes. The different initial player locations reflect the different flat-world generation configurations and spawn selection; later controlled movement uses identical arena coordinates. The advancement bodies genuinely differ in length and hash, and these runs do not establish whether their contents are semantically equivalent.

**Chunks / `MISMATCH`**

At the post-login observation point (after the origin chunk and an additional `0.6` seconds of packet reception), mc-spine had **25** decoded and active chunks while Vanilla had **45**. Each reported **24** decoded sections with **4096** states per section, a decoded `(0,0)` origin chunk, and **0** unmatched unloads. Chunk count is time- and initial-spawn-sensitive; the observation captures a concrete difference in delivered chunk counts at this point, without attributing its cause to the chunk codec.

## Reproduction and verification

The following commands were run from the worktree. `target/tmp` was configured before the build and both scripts, and it was used for isolated worlds and JVM temporary files.

```bash
mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"
cargo build --offline --locked
python3 tests/client_vanilla_parity.py
TMPDIR="$PWD/target/tmp" python3 tests/client_vanilla_parity.py --with-vanilla
bash tools/layers_check.sh
bash tools/debt_ratchet.sh
python3 tools/provenance_check.py
```

| Verification | Observed outcome |
| --- | --- |
| Offline locked Rust build | **PASS** |
| Rust self-consistency | **PASS**, 9/9 |
| Pinned official JVM comparison, three runs | **7/9 PASS, 2 MISMATCH** on each run, exit code `1` as designed for detected differences |
| `tools/layers_check.sh` | **PASS**, 44 modules, 111 edges, 0 named errors; existing named debt remains |
| `tools/debt_ratchet.sh` | **PASS**, 0 over limit |
| `tools/provenance_check.py` | **PASS**, provenance clean |

The fluid test checks that both live servers accept the same prescribed eight-step underwater displacement and remain within the **0.03 block** cross-server tolerance. It does not independently simulate full player fluid physics or prove that an unattended real client would generate that displacement. Likewise, matching movement acceptance after RCON fixture placement does not establish equivalence of the servers' original terrain, spontaneous spawn location, advancement definitions, or initial chunk-delivery cadence.
