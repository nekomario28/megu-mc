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

## 2. Clean public export

Produce the Mojang-free export and let `--verify` rehearse it:

```sh
bash tools/export_public_branch.sh --out ../mc-spine-public --verify
```

[tools/export_public_branch.sh](../tools/export_public_branch.sh) creates a
fresh single-commit repository: `datapacks/vanilla`, `config_packets.bin`
and every generated `*_1211*` file are removed and ignored.

Re-check provenance on the exact directory you will ship:

```sh
cd ../mc-spine-public
python3 tools/provenance_check.py --strict-public
```

`--verify` already ran it; `--strict-public` in
[tools/provenance_check.py](../tools/provenance_check.py) also proves the
stripped paths are untracked *and* ignored, so a release commit cannot pick
them up again by accident.

## 3. Regenerate the build inputs the export strips

The release build compiles four generated tables into the binary, so the
export needs the 1.21.1 data once:

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
    --binary target/release/mc-spine \
    --out-dir dist \
    --version 0.1.0-alpha.1 \
    --target x86_64-unknown-linux-gnu
python3 tools/release_bundle.py verify \
    --bundle dist/mc-spine-0.1.0-alpha.1-x86_64-unknown-linux-gnu.tar.gz \
    --manifest dist/release-manifest.json \
    --sums dist/SHA256SUMS
```

[tools/release_bundle.py](../tools/release_bundle.py) builds a deterministic
tarball (sorted members, mtime 0, normalised modes) and `verify` re-reads
the tarball bytes: manifest, `SHA256SUMS` and per-member records must all
agree, and the purity guard rejects any Mojang-derived or
repository-internal member.

## 5. Container image

Build from a checkout that carries the compile-time tables - the dev tree,
or the export after step 3:

```sh
docker build -f docker/Dockerfile -t mc-spine:0.1.0-alpha.1 .
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
docker run --rm mc-spine:0.1.0-alpha.1; test $? -ne 0
docker run --rm -e EULA=FALSE mc-spine:0.1.0-alpha.1; test $? -ne 0
```

Run:

```sh
docker run --name mc-spine -e EULA=TRUE -p 25565:25565 \
    -v mc-spine-data:/data mc-spine:0.1.0-alpha.1
```

[docker/entrypoint.sh](../docker/entrypoint.sh) refuses to start without
`EULA=TRUE`, passes the `MC_SPINE_*` environment through
(`MC_SPINE_PORT`, `MC_SPINE_ONLINE_MODE`, `MC_SPINE_VELOCITY_FORWARDING`,
`MC_SPINE_BACKUP_MAX_KEEP`, `MC_SPINE_LAN_BROADCAST`,
`MC_SPINE_SAFE_SPAWN`), and - when `MC_SPINE_MODPACK` names a pack inside the
container - runs the
[tools/mod_launcher.py](../tools/mod_launcher.py) pre-flight first
(`MC_SPINE_MC_VERSION`, default `1.21.1`).

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
