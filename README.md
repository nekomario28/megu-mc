<div align="center">
  <img src="website/logo.svg" width="96" height="96" alt="Megu logo">
  <h1>Megu</h1>
  <p><strong>Clean-room Rust server &amp; native GPU client for Minecraft: Java Edition 1.21.1 — zero JVM, zero OpenGL.</strong></p>
  <p>
    <a href="https://github.com/nekomario28/megu-mc/releases"><img src="https://img.shields.io/github/v/release/nekomario28/megu-mc?include_prereleases&style=flat-square&label=release&color=e05d44" alt="Latest Release"></a>
    <a href="https://github.com/nekomario28/megu-mc/actions/workflows/ci.yml"><img src="https://github.com/nekomario28/megu-mc/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <a href="https://github.com/nekomario28/megu-mc/actions/workflows/cross-platform.yml"><img src="https://github.com/nekomario28/megu-mc/actions/workflows/cross-platform.yml/badge.svg" alt="Cross-Platform"></a>
    <a href="https://megu.megumin.tech/"><img src="https://img.shields.io/badge/minecraft-1.21.1-2ea44f?style=flat-square" alt="Minecraft 1.21.1"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-LGPL--3.0--or--later-blue?style=flat-square" alt="License: LGPL-3.0-or-later"></a>
    <a href="https://github.com/sponsors/nekomario28"><img src="https://img.shields.io/github/sponsors/nekomario28?label=Sponsors&color=db61a2&style=flat-square" alt="GitHub Sponsors"></a>
  </p>
  <p>
    <strong>English</strong> · <a href="README.ja.md"><strong>日本語</strong></a> · <a href="https://megu.megumin.tech/"><strong>Website</strong></a> · <a href="https://github.com/nekomario28/megu-mc/releases"><strong>Downloads</strong></a> · <a href="docs/operator-guide.md"><strong>Docs</strong></a> · <a href="https://github.com/nekomario28/megu-mc/discussions"><strong>Discussions</strong></a> · <a href="https://megu.megumin.tech/#support"><strong>Support</strong></a>
  </p>
</div>

---

Megu is a free, open-source, clean-room reimplementation of **Minecraft: Java Edition 1.21.1** in Rust — a dedicated server (`megu`) and a pure-`wgpu` native GPU client (`megu-client-gpu`) built for low memory usage, fast startup, and zero OpenGL / JVM dependency.

> **Beta Status (`v0.1.0-beta.2`)**: Early pre-release. Core 1.21.1 gameplay, native Vulkan/DX12/Metal rendering, and data-driven mods work today, while 100% vanilla worldgen/AI coverage and arbitrary Java Mixin execution remain in progress. See the [Roadmap](specs/ROADMAP.md).

> **Start here:** the **[Operator Guide](docs/operator-guide.md)** explains the clean split between the dedicated server (`megu`) and the native GPU client (`megu-client-gpu`), with first-boot, mods, and admin commands.

## Measured Performance

`WARM=1 bash tools/load_compare.sh 20 0` — a 20-second **moving-viewer** load test (same seed, view distance and movement script, Linux x86_64): Vanilla 1.21.1 `server.jar` vs `megu`.

| Metric | **Megu** | Vanilla 1.21.1 | Δ |
| :--- | ---: | ---: | ---: |
| CPU time | **3.42 s** | 5.39 s | **−37%** |
| CPU ms per delivered chunk | **20.60 ms/chunk** | 32.87 ms/chunk | **−37%** |
| Memory (VmRSS) | **331 MiB** | 1,122 MiB | **−70%** |
| Threads | **16** | 66 | **−76%** |

<sub>**CPU ms per delivered chunk** is the server process's CPU time over the measured window divided by the chunks the driver actually received (login + moving window) — a whole-server streaming cost, not worldgen CPU time alone. Benchmark conditions, identical on both sides: same seed (0), `level-type=minecraft:normal`, view distance 4 / simulation distance 5, the same movement script, **network compression disabled** (`network-compression-threshold=-1` / `MEGU_COMPRESSION_THRESHOLD=-1`) and **anti-cheat disabled** on `megu` (`MEGU_ANTICHEAT=off`) so neither side pays for a codec or kicks the world-model-free driver. Idle `megu` sits at ~122 MiB. Reproduce with `WARM=1 bash tools/load_compare.sh 20 0`. Honest scope: Megu is a beta implementing fewer vanilla features, so part of the gap is "does less", not "does it cheaper"; general-seed 100% worldgen parity and arbitrary Java Mixin execution remain in progress.</sub>

## Why Megu

| Dedicated Server (`megu`) | Native GPU Client (`megu-client-gpu`) | Mods &amp; Native SDK |
| :--- | :--- | :--- |
| Vanilla 1.21.1 on port **25565** | Pure `wgpu` — Vulkan / DX12 / Metal | `.mmod` native format + `--compat-jar` for Modrinth / CurseForge JARs |
| Async chunk delivery + pre-compressed chunk-frame cache | Zero OpenGL, zero JVM — one native binary | Tiered JVM-free NeoForge / Fabric compatibility (`Create 6.0.10`, 35+ mixed-modpack E2E) |
| Pure-Rust TTY auto-detect TUI or `--cli`; `systemd` `sd_notify` | Greedy-meshed translucent clouds, 64-light dynamic light cache | `megu-mod-check` pre-flight without a JVM (or in-browser at [megu.megumin.tech/#checker](https://megu.megumin.tech/#checker)) |
| Hardened auth, DDoS &amp; zero-unwrap ingress | First-launch hardware auto-tune + eye-comfort optics | Typed builders, packaging &amp; test harness in [`sdk/megu-mod-sdk`](sdk/megu-mod-sdk) |
| Reversible Vanilla Anvil world import/export | Official Launcher, PrismLauncher &amp; Modrinth App compatible | Data-driven mods: drop in `mods/`, no Java required |

## Quick Start

**Players** — pick one from the **[Downloads page](https://megu.megumin.tech/#downloads)** or **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases)**:

| OS / Launcher | File / Command |
| :--- | :--- |
| **Windows (x86_64)** | [`megu-installer-windows-x86_64.exe`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-installer-windows-x86_64.exe) — standalone installer, `gui` arg for GUI mode |
| **Linux (x86_64)** | `curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh \| sh` (or [`megu-installer-linux-x86_64`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-installer-linux-x86_64)) |
| **macOS (Apple Silicon)** | [`megu-installer-macos-arm64`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-installer-macos-arm64) |
| **Prism Launcher** | [`megu-prism-1.21.1.zip`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-prism-1.21.1.zip) (ready-to-use custom-patch instance; requires `megu-client-gpu` from `megu-installer` or an OS archive) or run `megu-installer prism`. Modrinth App instances, worlds, and `mods/` are read directly by `megu-client-gpu`; no `.mrpack` is needed. Optional Fabric/NeoForge bridge mod: [`megu-integrated.jar`](https://github.com/nekomario28/megu-mc/releases/tag/v0.1.0-beta.2) in Releases. |

The installer stages `megu-client-gpu` into `<minecraft_dir>/bin/` and registers the **Megu (1.21.1)** launcher profile. On first launch the client reuses an existing 1.21.1 client JAR from the Official Launcher, PrismLauncher or Modrinth App — or fetches and SHA-1-verifies it from Mojang's CDN. It can read existing launcher installs, worlds, and `mods/` directly. No Java, no `-Xmx` tuning: adjust the optional RAM budget slider anytime in **Settings → Video**.

**Verify a pinned Linux server archive before installing** — the example pins `v0.1.0-beta.2`; change `TAG` only when intentionally selecting another release. The release publishes `SHA256SUMS-all.txt`; the commands extract the selected archive's entry to `SHA256SUMS.txt` so `sha256sum -c` verifies just the downloaded file:

```sh
TAG=v0.1.0-beta.2
TARGET=linux-x86_64
ARCHIVE="megu-${TAG}-${TARGET}.tar.gz"
BASE="https://github.com/nekomario28/megu-mc/releases/download/${TAG}"
curl -fLO "${BASE}/${ARCHIVE}"
curl -fLO "${BASE}/SHA256SUMS-all.txt"
grep -F "  ${ARCHIVE}" SHA256SUMS-all.txt > SHA256SUMS.txt
test "$(wc -l < SHA256SUMS.txt)" -eq 1
sha256sum -c SHA256SUMS.txt
tar -xzf "${ARCHIVE}"
"./megu-${TAG}-${TARGET}/megu-installer" server --dir ./megu-server
```

**Server admins** — one command from any full platform archive:

```sh
./megu-installer server --dir ./megu-server
# Set eula=true in ./megu-server/eula.txt after reviewing the Minecraft EULA:
cd ./megu-server && ./megu                 # defaults to port 25565
```

<details>
<summary><strong>All 12 platform archives (v0.1.0-beta.2)</strong> — desktop archives contain <code>megu</code>, <code>megu-client-gpu</code>, and <code>megu-installer</code>; server-only and musl-client archives differ</summary>

| Target | Full Archive | Included Binaries |
| :--- | :--- | :--- |
| **`windows-x86_64`** | **[`megu-v0.1.0-beta.2-windows-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-windows-x86_64.tar.gz)** | `megu.exe`, `megu-client-gpu.exe`, `megu-installer.exe` |
| **`windows-arm64`** | **[`megu-v0.1.0-beta.2-windows-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-windows-arm64.tar.gz)** | `megu.exe`, `megu-client-gpu.exe`, `megu-installer.exe` |
| **`linux-x86_64`** | **[`megu-v0.1.0-beta.2-linux-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-linux-x86_64.tar.gz)** | `megu`, `megu-client-gpu`, `megu-installer` |
| **`linux-arm64`** | **[`megu-v0.1.0-beta.2-linux-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-linux-arm64.tar.gz)** | `megu`, `megu-client-gpu`, `megu-installer` |
| **`macos-arm64`** | **[`megu-v0.1.0-beta.2-macos-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-macos-arm64.tar.gz)** | `megu`, `megu-client-gpu`, `megu-installer` |
| **`macos-x86_64`** | **[`megu-v0.1.0-beta.2-macos-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-macos-x86_64.tar.gz)** | `megu`, `megu-client-gpu`, `megu-installer` |
| **`linux-x86_64-musl`** | **[`megu-v0.1.0-beta.2-linux-x86_64-musl.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-linux-x86_64-musl.tar.gz)** | Server-only (static): `megu`, `megu-installer` |
| **`linux-arm64-musl`** | **[`megu-v0.1.0-beta.2-linux-arm64-musl.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-linux-arm64-musl.tar.gz)** | Server-only (static): `megu`, `megu-installer` |
| **`freebsd-x86_64`** | **[`megu-v0.1.0-beta.2-freebsd-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-freebsd-x86_64.tar.gz)** | Server-only: `megu`, `megu-installer` |
| **`linux-riscv64`** | **[`megu-v0.1.0-beta.2-linux-riscv64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-linux-riscv64.tar.gz)** | Server-only: `megu`, `megu-installer` |
| **`linux-x86_64-musl-client`** | **[`megu-v0.1.0-beta.2-linux-x86_64-musl-client.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-linux-x86_64-musl-client.tar.gz)** | GPU-client-only (dynamic musl): `megu-client-gpu` |
| **`linux-arm64-musl-client`** | **[`megu-v0.1.0-beta.2-linux-arm64-musl-client.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-beta.2/megu-v0.1.0-beta.2-linux-arm64-musl-client.tar.gz)** | GPU-client-only (dynamic musl): `megu-client-gpu` |

> **Which binary do I run?** `megu-client-gpu` is the native GPU game client for playing (`megu-client` is the headless/CLI client and status probe); `megu` runs the dedicated server. `megu-client-gpu` obtains Vanilla 1.21.1 assets by itself on first launch, so `megu-installer` is a convenience for staging the binary and launcher profile, never a prerequisite.

</details>

## Verified Feature Highlights (v0.1.0-beta.2)

- **Server:** Vanilla 1.21.1 on port **25565** by default, with 51/51 GameRules, 42/42 Enchantments, 39/39 Effects, Vanilla fuels and death messages. Built-in pure-Rust TTY-auto-detect TUI or plain CLI (`--cli`), `systemd` `sd_notify`, Hub `/server`/`/hub`/`/world`, Survival Moderator `/megu mod`, hardened authentication, DDoS and zero-unwrap ingress, and `.mmod` / `.jar` recognition.
- **Client:** Pure `wgpu` eye-comfort optics (`comfort_exposure`, `indirect_light_bounce`, Snell water refraction, dawn/dusk air-mass optics and three-layer translucent clouds); per-part lit entities, mobs and block entities, mob cascade and contact shadows, universal data-driven dynamic lights (Vanilla, redstone and modded), and 3D/2D enchantment glint. First-launch audio defaults to 50% master / 45% music with a `Loud Sound Limiter` dB display. Installed vs `[+ Browse]` Mod Menu supports in-process Soft Reload; E2EE Friends and LAN use isolated `127.0.0.1:0` hosting. OS locale, clipboard, Japanese IME and a four-line sign editor are supported.
- **Opt-in extensions (off by default for strict Vanilla parity):** 3D directional shadow-light gameplay (`directional_shadow_light`, `/megu shadowlight on|off`, or the local client Settings toggle); chunk-loader-free O(1) unloaded catch-up (`lazy_catchup`, `/megu lazycatchup on|off`, or the local client Settings toggle); custom-shaped Nether portals (`MEGU_CUSTOM_PORTAL_SHAPES=1`).

These highlights describe wired capabilities, not a promise of full Vanilla worldgen/AI or arbitrary Java Mixin compatibility. See the [Operator Guide](docs/operator-guide.md) for operational limits.

## What You Need

- **No Java, anywhere.** Server and client are pure Rust native binaries — no JVM to install, no `-Xmx` sliders to tune.
- **Client:** any GPU with Vulkan, DirectX 12 or Metal support; first launch auto-tunes to your hardware. `megu-client-gpu` obtains official 1.21.1 assets itself — reusing your existing launcher JAR or fetching SHA-1-verified files from Mojang's CDN. **Clean-room:** no Mojang code or assets ship in this repository; the client downloads what it needs from Mojang at runtime.
- **Server:** runs on glibc and static `*-musl` builds alike; import an existing Vanilla world with `--import-anvil` or `MEGU_ANVIL_WORLD_DIR`.

## Platform Support

| Platform | `megu-client-gpu` | `megu` server |
| :--- | :---: | :---: |
| Windows x86_64 / arm64 | ✓ | ✓ |
| macOS arm64 / x86_64 | ✓ | ✓ |
| Linux x86_64 / arm64 (glibc) | ✓ | ✓ |
| Linux x86_64 / arm64 (musl, static) | — | ✓ |

## Architecture

13 standalone crates plus the root server — no workspace, each manifest is its own source of truth. Full map: **[Crate Architecture](docs/architecture-crates.md)** · **[Design Principles & De-Hardcoding Blueprint (DESIGN.md)](DESIGN.md)**.

| Crate | Owns |
| :--- | :--- |
| `megu` (root) | Dedicated server — protocol 767 gameplay, tick engine, worldgen, saves |
| `megu-core` | Canonical version constants, `ResourceLocation`, Java-exact RNG, vanilla math |
| `megu-proto` | Protocol IR & packet tables (767/777), codec, `VCR1` proximity voice relay |
| `megu-state-store` | 64-shard runtime store, `STV1` snapshots, `1.21.1 ↔ 26.3` DataFixer, Anvil inspector |
| `megu-server-files` | `server.properties`/eula/ops persistence, `megu-init`/`megu-admin`, claims &amp; parties |
| `mod-adapter-core` · `create-adapter` · `mod-offload` | Zero-JVM mod classification, Mixin→Event lifting, Create optimizer, factory/combat kernels |
| `megu-kernel` · `megu-client-mesh` · `megu-client-ui` | Protocol-767 kernel, voxel mesher + `VFX1` engine, client UI screens |
| `megu-client` · `megu-client-gpu` | Headless/CLI client; windowed `winit` + `wgpu` GPU client |
| `megu-mod-sdk` | Typed builders, `.mmod` packaging, in-memory mod test harness |

## Mod &amp; Shader Authors

- Place data-driven Fabric / NeoForge mods in `mods/`, or pre-flight any modpack without a JVM using `megu-mod-check --scan-mods-dir ./mods` (or in your browser at **[megu.megumin.tech/#checker](https://megu.megumin.tech/#checker)**).
- Native `.mmod` format and `--compat-jar` for Modrinth/CurseForge: **[Mod SDK guide](docs/mod-sdk-guide.md)** · **[SDK agent guide](sdk/AGENTS.md)**
- Authoring guide & WGSL shader specifications: **[Mod & Shader Author Guide](docs/mod-author-guide.md)**
- End-to-end handling of embedded JARs, tag namespaces, mod sync and modded saves: **[Mod Compatibility](docs/mod-compatibility.md)**

## Building From Source

To compile Megu from source, you need **Rust `1.98.0`** (pinned in [rust-toolchain.toml](rust-toolchain.toml)) and Python 3:

```sh
python3 tools/dev_check.py
cargo build --locked --release
```

- Architecture & local test tiers: **[Development Guide](docs/development.md)** · **[Crate Architecture](docs/architecture-crates.md)**
- Server configuration & hosting: **[Operator Guide](docs/operator-guide.md)** · **[Security Notes](SECURITY_NOTES.md)** · Docker: `docker build -f docker/Dockerfile .`
- Friend invites without port forwarding, `FRL3` E2EE and the trust store: **[Friend Play & E2EE](docs/friend-play-e2ee.md)**

## Community &amp; Contributing

Questions and ideas → **[GitHub Discussions](https://github.com/nekomario28/megu-mc/discussions)**; bug reports without a GitHub account → **[web report](https://megu.megumin.tech/#report)**.

See **[Contributing](CONTRIBUTING.md)** and our **[Code of Conduct](CODE_OF_CONDUCT.md)**. All commits require DCO sign-off (`git commit -s`). For security issues, see **[SECURITY.md](SECURITY.md)**.

## Support Us

Megu is 100% free and open-source software under `LGPL-3.0-or-later`. Recurring infrastructure, CI runners, and hardware testing are supported by the community:

- **GitHub Sponsors**: **[github.com/sponsors/nekomario28](https://github.com/sponsors/nekomario28)**
- **Hosting & Commercial Partnerships**: **[partners@megumin.tech](mailto:partners@megumin.tech)**

## License

Licensed under **[LGPL-3.0-or-later](LICENSE)** (companion GPLv3 text in [docs/COPYING.GPL](docs/COPYING.GPL); see [LICENSING.md](LICENSING.md), [NOTICE](NOTICE), and [THIRD_PARTY.md](THIRD_PARTY.md)). Independent third-party Mod JARs loaded at runtime remain under their own licenses.

*Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft. ([TRADEMARK.md](TRADEMARK.md))*
