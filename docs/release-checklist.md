# Alpha release checklist

One end-to-end pass from a green dev tree to a public Alpha bundle and
container image. Every step is fail-closed: a red gate means stop, not
"ship anyway".

## 1. Green tree

Run the full gate on the release commit:

```sh
bash tests/verify.sh
```

[tests/verify.sh](../tests/verify.sh) runs the whole suite plus the
release-packaging self-tests under `== gate exit contracts`:
`tools/parity_cache.py --self-test`, `tools/release_bundle.py --self-test`
and [tests/dev_check_test.py](../tests/dev_check_test.py).
`--without-mutations` skips only the mutation campaign; every gate above
still runs.

The pre-release gate also runs [tests/verify_gate_coverage_test.py](../tests/verify_gate_coverage_test.py),
[tools/debt_ratchet.sh](../tools/debt_ratchet.sh) (including the
`.lock().unwrap()` ceiling), adversarial decoder/parser fuzz tests in
`proto`, `state-store`, `mod-adapter-core` and `server-files`, and
[tests/live_parity_e2e_wire.py](../tests/live_parity_e2e_wire.py) both as an
offline self-test and against the built server. It checks the `mod-suite`
feature as well.

The gate invokes `cargo test --offline --locked` for the **11 zero-third-party-dependency
standalone crates**: `core`, `proto`, `state-store`, `server-files`, `mod-offload`,
`mod-adapter-core`, `create-adapter`, `client-port`, `client-kernel`,
`client-app`, and `client-ui`. The twelfth standalone crate, `client-gpu`,
uses an isolated `client-gpu/Cargo.toml` and `client-gpu/Cargo.lock` for
`winit`/`wgpu` and their dependencies. Test it separately with
`cargo test --manifest-path client-gpu/Cargo.toml --locked`.
Clippy is a separate release check; `tests/verify.sh` does not run it:

```sh
for crate in core proto state-store server-files mod-offload mod-adapter-core \
    create-adapter client-port client-kernel client-app client-ui; do
    cargo clippy --manifest-path "$crate/Cargo.toml" --offline --locked \
        --all-targets -- -D warnings || exit 1
done
cargo clippy --manifest-path client-gpu/Cargo.toml --locked \
    --all-targets -- -D warnings
```

## 2. Clean public export

Produce the Mojang-free export and let `--verify` rehearse it:

```sh
bash tools/export_public_branch.sh --out ../megu-public --verify
```

[tools/export_public_branch.sh](../tools/export_public_branch.sh) creates a
fresh single-commit repository: `datapacks/vanilla`, `config_packets.bin`
and every generated `*_1211*` file are removed and ignored.

Re-check provenance on the exact directory you will ship:

```sh
cd ../megu-public
python3 tools/provenance_check.py --strict-public
```

`--verify` already ran it; `--strict-public` in
[tools/provenance_check.py](../tools/provenance_check.py) also proves the
stripped paths are untracked *and* ignored, so a release commit cannot pick
them up again by accident.

T1-2 removed four non-test generated-data `include_bytes!` calls. A stripped
public export now compiles its binaries without any Mojang data files. Check
this separately in the exported directory:

```sh
cargo check --bins --offline --locked
```

This check establishes buildability of the stripped binaries, not playable
world generation or runtime data availability.

## 3. Regenerate runtime data for playable worlds

For actual world generation and gameplay, regenerate the local 1.21.1 data
from the operator's own server jar before building a playable release:

```sh
bash tools/vanilla_data/gen_vanilla_data.sh \
    --jar /path/to/1.21.1-server.jar \
    --fetch-fabric-mappings --accept-eula
cargo build --release --locked
```

[tools/vanilla_data/gen_vanilla_data.sh](../tools/vanilla_data/gen_vanilla_data.sh)
pins the jar by sha1 and refetches only CC0 Fabric mappings; the regenerated
outputs are gitignored, so the export stays clean while remaining
reproducible.

## 4. Release bundle

```sh
python3 tools/release_bundle.py build \
    --binary target/release/megu \
    --out-dir dist \
    --version 0.1.0-alpha.1 \
    --target x86_64-unknown-linux-gnu
python3 tools/release_bundle.py verify \
    --bundle dist/megu-0.1.0-alpha.1-x86_64-unknown-linux-gnu.tar.gz \
    --manifest dist/release-manifest.json \
    --sums dist/SHA256SUMS
```

[tools/release_bundle.py](../tools/release_bundle.py) builds a deterministic
tarball (sorted members, mtime 0, normalised modes) and `verify` re-reads
the tarball bytes: manifest, `SHA256SUMS` and per-member records must all
agree, and the purity guard rejects any Mojang-derived or
repository-internal member.
The payload includes `tools/megu_start.py` and `tools/megu_translate.py` as
executable helpers. Separately built `megu-client`, `megu-mod-check`
`megu-world-migrate` and `megu-init` binaries can be included with repeated
`--extra-binary <path>` arguments to `build`.

## 5. Container image

Build from a checkout that carries the compile-time tables - the dev tree,
or the export after step 3:

```sh
docker build -f docker/Dockerfile -t megu:0.1.0-alpha.1 .
```

[docker/Dockerfile](../docker/Dockerfile) is multi-stage: the Rust 1.98.0
builder runs `cargo build --release --locked`, and the slim Debian runtime
ships only the binary, the licence texts, the public tools and the
entrypoint - no Mojang jar, no `config_packets.bin`, no `datapacks/vanilla`,
no `*_1211*` asset file. It runs as uid 10001 with `/data` as the working
directory and exposes `25565/tcp`.

Gate the EULA behaviour before publishing - both must exit non-zero and
never start the server:

```sh
docker run --rm megu:0.1.0-alpha.1; test $? -ne 0
docker run --rm -e EULA=FALSE megu:0.1.0-alpha.1; test $? -ne 0
```

Run:

```sh
docker run --name megu -e EULA=TRUE -p 25565:25565 \
    -v megu-data:/data megu:0.1.0-alpha.1
```

[docker/entrypoint.sh](../docker/entrypoint.sh) refuses to start without
`EULA=TRUE`, passes the `MEGU_*` environment through
(`MEGU_PORT`, `MEGU_ONLINE_MODE`, `MEGU_VELOCITY_FORWARDING`,
`MEGU_BACKUP_MAX_KEEP`, `MEGU_LAN_BROADCAST`,
`MEGU_SAFE_SPAWN`), and - when `MEGU_MODPACK` names a pack inside the
container - runs the
[tools/mod_launcher.py](../tools/mod_launcher.py) pre-flight first
(`MEGU_MC_VERSION`, default `1.21.1`).

The server reads `assets/`, `datapacks/` and `config_packets.bin` from its
working directory. To serve real worlds, mount a `/data` volume that holds
the output of step 3; without it the entrypoint warns and the server comes
up without worldgen or registry data.

## 6. Live client smoke - 1.21.1 and 26.3

On a throwaway world, with a vanilla 1.21.1 client and a vanilla 26.3
client each:

- [ ] joins and finishes the configuration phase (registries and tags arrive)
- [ ] spawn chunks arrive; movement does not rubber-band; spawn is dry ground
- [ ] place and break a block; a second client sees both
- [ ] open a chest and move items in and out
- [ ] chat plus a vanilla command (`/seed`, `/time set day`)
- [ ] die, respawn, disconnect, reconnect
- [ ] `stop`, restart: world, inventory and position restore

The automated two-client [Alpha smoke harness](../tests/alpha_release_smoke.py)
covers these seven steps, including `stop` → restart persistence. Its recorded
run passed 7/7 checks. The boxes above remain open for a fresh run against
the final candidate SHA.

## 7. Alpha scope and clean-room communication

- Say what the Alpha is in the words of [README.md](../README.md) and
  [specs/ROADMAP.md](../specs/ROADMAP.md) - verified scope only, no
  compatibility or performance claims the evidence does not carry.
- The bundle and the image ship **no Mojang assets**. Operators regenerate
  the 1.21.1 data from their own server.jar with
  [tools/vanilla_data/gen_vanilla_data.sh](../tools/vanilla_data/gen_vanilla_data.sh);
  say so plainly - it is the clean-room story, not a defect.
- Licence texts ship in the payload: `LICENSE`, `COPYING.LESSER`,
  `COPYING.AGPL`, `NOTICE`, [LICENSING.md](../LICENSING.md) and
  [THIRD_PARTY.md](../THIRD_PARTY.md). LICENSING.md §5 still applies to the
  export.
- Point operators at [SECURITY_NOTES.md](../SECURITY_NOTES.md) for the
  bind/allowlist posture; in the image the published port, not the bind
  address, decides exposure.
- State the bounded strict Vanilla parity audit results (`ceaf3849`) explicitly:
  58/58 C2S Play, 51/51 GameRules, 89/89 command roots, 30/30 behavioral
  matrix; `eddeea93` records 12/12 MATCH against live Vanilla 1.21.1 cases.
  The updated live S2C Play emissions audit reports **107/124 (86.3%)** and
  **0 dead mechanisms**. The remaining S2C surface and whole-world M2 parity
  remain open.
- State that the strict NeoForge 1.21.1 / Fabric 1.21.1 repository self-audit
  is 53/53 overall (50/50 per loader, `ceaf3849`), while Wave 3 real
  third-party modpack E2E remains unverified. The debt ratchet is at its
  lock/rwlock unwrap ceiling of 19/19, with 0 wall-clock reads.
- Link the [mod/shader author guide](mod-author-guide.md) (`008a42c9`).
  External shaderpack handling covers Classic OptiFine, Iris 1.7–1.11+,
  DistantHorizons `dh_shadow`/`dh_terrain`/`dh_water`, `megu-shader.toml`,
  a GLSL 120–460 reflector, CLI/TOML/RenderGraph and a `client-ui` shader
  screen (`469a15f5`, `11e82fb8`, `001de852`, `a079e1d6`). The native
  WGSL path has a legacy-GPU-safe fallback. `client-gpu` implements a real
  `winit` + `wgpu` window and offscreen rendering, with a recorded non-blank
  Vulkan offscreen render on AMD Radeon RX 7800 XT (RADV). No third-party
  shaders are bundled; live third-party server/modpack shader visual fidelity
  and end-to-end compatibility remain unverified.
- The [release workflow](../.github/workflows/release.yml) builds and verifies
  six release binaries across Linux, macOS and Windows, including archive
  manifest/checksum verification. The `client-gpu` binary is separate from
  that six-binary release matrix. A passing workflow definition does not
  replace same-SHA release-candidate execution evidence.

## 8. Phase 2 / 3 / 4 Alpha ship gates

Record results for the **same release-candidate SHA**. These are pending
checks until run on that SHA; a code path or unit test alone does not satisfy
a live interoperability gate.

**Phase 2 — documentation and implementation surface**

- [ ] `README.md`, `docs/architecture.md`, `docs/operator-guide.md`, and this
  checklist agree with the 12 standalone crates and their actual entry points.
- [ ] Full `tests/verify.sh`, standalone Clippy loop above, provenance check,
  and `tools/release_bundle.py --self-test` pass on the candidate SHA.
- [ ] Document the boundaries of the bounded `.class` subset VM, optional
  mini-JRE/Tier 3 delegation, in-memory OP4 `/megu policy`, optional MineAll/
  CutAll, and death-slot restore's currently unconnected live toggle.
- [ ] Document the headless software rasterizer, WGSL/RenderGraph, and
  `client-ui`, and the separate `client-gpu` `winit` + `wgpu` window backend,
  including the limits of the available Vulkan offscreen hardware proof.

**Phase 3 — integration and portability**

- [ ] Build and smoke-test **Linux**, **macOS**, and **Windows** separately;
  record platform, target triple, compiler, startup and test evidence for each.
- [ ] On a disposable server, check `/megu policy` level-4 authorization,
  `list|get|set|reset`, the five map fair-play switches (including
  `player_radar_scope=party_only`), and opt-in MineAll/CutAll behavior.
- [ ] Check the client FPS cap (`10..=2000` / `Unlimited`), frame budget,
  bounded volume, headless 3D model/atlas/Hi-Z/WGSL pipeline, and client UI
  against the supported entry points.
- [ ] Run Wave 3 **real third-party modpack E2E** before claiming third-party
  Mod compatibility. Record which mods needed the optional Tier 3 mini-JRE
  delegate and which remained unsupported.

**Phase 4 — candidate, distribution and optional crash relay**

- [ ] Complete [`P4-1`](../specs/ROADMAP.md) on the same SHA, including
  the live/representative M3 evidence, export verification, and release gates.
  Existing unit and offline results do not establish full M2 parity or
  equal-work live performance.
- [ ] Verify the clean public export, playable data regeneration, bundle
  manifest/checksums, container EULA check, and live client smoke in §§2–6.
- [ ] For an enabled crash relay, provision the Cloudflare free-tier Pages
  site, Pages Functions, Workers KV (`CRASH_INDEX` binding), and optional
  Turnstile. Verify `POST /api/report` and `GET /api/latest` on the deployed
  Pages project.
- [ ] Set `GITHUB_REPO` as a non-secret variable and `GITHUB_TOKEN` as a
  Pages secret with the repository's `issues:write` permission. Set optional
  `TURNSTILE_SECRET_KEY` only as a Pages secret when Turnstile is enabled.
  Check that no tokens, secret values, or local `.env` files enter Git,
  exported sources, or release bundles.
