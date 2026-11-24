<div align="center">
  <img src="website/logo.svg" width="84" height="84" alt="Megu Logo">
  <h1>Megu</h1>
  <p><strong>Clean-room Rust server &amp; native GPU client (Vulkan / DX12 / Metal) for Minecraft: Java Edition 1.21.1</strong></p>
  <p>
    <a href="https://github.com/nekomario28/megu-mc/releases"><img src="https://img.shields.io/github/v/release/nekomario28/megu-mc?include_prereleases&label=release&color=e05d44" alt="Latest Release"></a>
    <a href="https://megu.megumin.tech/"><img src="https://img.shields.io/badge/minecraft-1.21.1-2ea44f" alt="Minecraft 1.21.1"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-LGPL--3.0--or--later-blue" alt="License: LGPL-3.0-or-later"></a>
    <a href="https://github.com/sponsors/nekomario28"><img src="https://img.shields.io/github/sponsors/nekomario28?label=Sponsors&color=db61a2" alt="GitHub Sponsors"></a>
  </p>
  <p>
    <strong>English</strong> · <a href="README.ja.md"><strong>日本語</strong></a> · <a href="https://megu.megumin.tech/"><strong>Website</strong></a> · <a href="https://github.com/nekomario28/megu-mc/releases"><strong>Downloads</strong></a> · <a href="docs/operator-guide.md"><strong>Docs</strong></a>
  </p>
</div>

---

Megu is a free, open-source, clean-room Rust server and pure-`wgpu` native GPU client for **Minecraft: Java Edition 1.21.1**, built for low memory usage, fast startup, and zero OpenGL dependency.

> **Alpha Status (`v0.1.0-alpha.2`)**: Early pre-release. Core 1.21.1 gameplay, native Vulkan/DX12/Metal rendering, and data-driven mods work today, while 100% vanilla worldgen/AI coverage and arbitrary Java Mixin execution remain in progress. See the [Roadmap](specs/ROADMAP.md).

> **Start here:** the **[Operator Guide](docs/operator-guide.md)** explains the clean split between the dedicated server (`megu`) and the native GPU client (`megu-client-gpu`), with first-boot, mods, and admin commands.

## How To (Players)

Choose **one** category below from the **[Downloads page](https://megu.megumin.tech/#downloads)** or **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases)** (ordered from most popular):

### 1. Official Launcher &amp; Standalone Client (`megu-installer` — GUI / CLI, No Java Needed)

Automatically downloads/stages `megu-client-gpu` into `<minecraft_dir>/bin/` and registers the **Megu (1.21.1)** profile in the Official Minecraft Launcher (run with `gui` / `--gui` for the graphical dialog, or default 1-click install):

> **No terminal step required before playing.** On first launch, `megu-client-gpu` reuses an existing 1.21.1 client JAR from the Official Launcher, PrismLauncher, or Modrinth App, or fetches and SHA-1-verifies it from Mojang's CDN in the background. `megu-installer` only stages the binary and launcher profile.
>
> **No Java memory tuning:** Megu runs natively in Rust (zero JVM / zero OpenGL). You do **not** need to tune Java `-Xmx` memory sliders in the Official Launcher or Prism — Megu manages memory automatically. Adjust the optional RAM budget slider (with automatic cache eviction under memory pressure) anytime in **Settings → Video**.

| OS | File / Command | Description |
| :--- | :--- | :--- |
| **Windows (x86_64)** | **[`megu-installer-windows-x86_64.exe`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-installer-windows-x86_64.exe)** | Standalone `.exe` installer (`%APPDATA%\.minecraft` &amp; Microsoft Store Launcher; run `megu-installer-windows-x86_64.exe gui` for GUI mode) |
| **Linux (x86_64)** | `curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh \| sh`<br>*(or **[`megu-installer-linux-x86_64`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-installer-linux-x86_64)**)* | Auto-detects standard `~/.minecraft` and Flatpak (`~/.var/app/com.mojang.Minecraft/.minecraft`) |
| **macOS (Apple Silicon)** | **[`megu-installer-macos-arm64`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-installer-macos-arm64)** | Standalone native installer for `~/Library/Application Support/minecraft` |

### 2. Modrinth App &amp; Prism Launcher (Modpack / Instance Import)

| Launcher | File / Command | Description |
| :--- | :--- | :--- |
| **Modrinth App / Prism Launcher** | **[`megu-1.21.1.mrpack`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-1.21.1.mrpack)** | Standard Modrinth `.mrpack` modpack — drag &amp; drop directly into your launcher |
| **Prism Launcher (Native ZIP)** | **[`megu-prism-1.21.1.zip`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-prism-1.21.1.zip)** | Pre-configured Prism instance archive (or generate locally via `megu-installer prism`) |

## Verified feature highlights (upcoming alpha.3)

- **Server:** Vanilla 1.21.1 on port **25565** by default, with 51/51 GameRules, 42/42 Enchantments, 39/39 Effects, Vanilla fuels and death messages. Built-in pure-Rust TTY-auto-detect TUI or plain CLI (`--cli`), `systemd` `sd_notify`, Hub `/server`/`/hub`/`/world`, Survival Moderator `/megu mod`, hardened authentication, DDoS and zero-unwrap ingress, and `.mmod` / `.jar` recognition.
- **Client:** Pure `wgpu` eye-comfort optics (`comfort_exposure`, `indirect_light_bounce`, Snell water refraction, dawn/dusk air-mass optics and three-layer translucent clouds); per-part lit entities, mobs and block entities, mob cascade and contact shadows, universal data-driven dynamic lights (Vanilla, redstone and modded), and 3D/2D enchantment glint. First-launch audio defaults to 50% master / 45% music with a `Loud Sound Limiter` dB display. Installed vs `[+ Browse]` Mod Menu supports in-process Soft Reload; E2EE Friends and LAN use isolated `127.0.0.1:0` hosting. OS locale, clipboard, Japanese IME and a four-line sign editor are supported.
- **Opt-in extensions (off by default for strict Vanilla parity):** 3D directional shadow-light gameplay (`directional_shadow_light`, `/megu shadowlight on|off`, or the local client Settings toggle); chunk-loader-free O(1) unloaded catch-up (`lazy_catchup`, `/megu lazycatchup on|off`, or the local client Settings toggle); custom-shaped Nether portals (`MEGU_CUSTOM_PORTAL_SHAPES=1`).

These highlights describe wired capabilities, not a promise of full Vanilla worldgen/AI or arbitrary Java Mixin compatibility. See the [Operator Guide](docs/operator-guide.md) for operational limits.

## How To (Server Admins &amp; Full CLI Suite)

Download the full platform archive (`megu-<version>-<target>.tar.gz`) from **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases/tag/v0.1.0-alpha.2)** (`v0.1.0-alpha.2`). Eight targets are published per release: `windows-x86_64`, `windows-arm64`, `linux-x86_64`, `linux-arm64`, `linux-x86_64-musl`, `linux-arm64-musl`, `macos-arm64`, and `macos-x86_64`. Each archive bundles `megu` (dedicated server), `megu-client-gpu`, `megu-client`, `megu-installer`, `megu-mod-check`, `megu-world-migrate`, `megu-init`, and `megu-admin`; the `*-musl` archives are statically linked and **server-only** (`megu` + `megu-installer`).

| Target | Full Archive | Included Binaries |
| :--- | :--- | :--- |
| **`windows-x86_64`** | **[`megu-v0.1.0-alpha.2-windows-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-windows-x86_64.tar.gz)** | `megu.exe`, `megu-client-gpu.exe`, `megu-client.exe`, `megu-installer.exe`, `megu-mod-check.exe`, `megu-world-migrate.exe`, `megu-init.exe`, `megu-admin.exe` |
| **`windows-arm64`** | **[`megu-v0.1.0-alpha.2-windows-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-windows-arm64.tar.gz)** | `megu.exe`, `megu-client-gpu.exe`, `megu-client.exe`, `megu-installer.exe`, `megu-mod-check.exe`, `megu-world-migrate.exe`, `megu-init.exe`, `megu-admin.exe` |
| **`linux-x86_64`** | **[`megu-v0.1.0-alpha.2-linux-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-linux-x86_64.tar.gz)** | `megu`, `megu-client-gpu`, `megu-client`, `megu-installer`, `megu-mod-check`, `megu-world-migrate`, `megu-init`, `megu-admin` |
| **`linux-arm64`** | **[`megu-v0.1.0-alpha.2-linux-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-linux-arm64.tar.gz)** | `megu`, `megu-client-gpu`, `megu-client`, `megu-installer`, `megu-mod-check`, `megu-world-migrate`, `megu-init`, `megu-admin` |
| **`linux-x86_64-musl`** | **[`megu-v0.1.0-alpha.2-linux-x86_64-musl.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-linux-x86_64-musl.tar.gz)** | Server-only (static): `megu`, `megu-installer` |
| **`linux-arm64-musl`** | **[`megu-v0.1.0-alpha.2-linux-arm64-musl.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-linux-arm64-musl.tar.gz)** | Server-only (static): `megu`, `megu-installer` |
| **`macos-arm64`** | **[`megu-v0.1.0-alpha.2-macos-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-macos-arm64.tar.gz)** | `megu`, `megu-client-gpu`, `megu-client`, `megu-installer`, `megu-mod-check`, `megu-world-migrate`, `megu-init`, `megu-admin` |
| **`macos-x86_64`** | **[`megu-v0.1.0-alpha.2-macos-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-macos-x86_64.tar.gz)** | `megu`, `megu-client-gpu`, `megu-client`, `megu-installer`, `megu-mod-check`, `megu-world-migrate`, `megu-init`, `megu-admin` |

> **Which binary do I run?** `megu-client-gpu` is the native GPU game client for playing (`megu-client` is the headless/CLI client and status probe); `megu` runs the dedicated server. `megu-client-gpu` obtains Vanilla 1.21.1 assets by itself on first launch, so `megu-installer` is a convenience for staging the binary and launcher profile, never a prerequisite.

```sh
./megu-installer server --dir ./megu-server
# Set eula=true in ./megu-server/eula.txt after reviewing the Minecraft EULA:
cd ./megu-server && ./megu                 # defaults to port 25565
```

- Documentation on configuring and hosting Megu: **[Operator Guide](docs/operator-guide.md)** · **[Security Notes](SECURITY_NOTES.md)**
- Friend invites without port forwarding, `FRL3` end-to-end encryption and the trust store: **[Friend Play & E2EE](docs/friend-play-e2ee.md)**
- How existing Fabric / NeoForge mods, tags, capabilities and modded saves are handled: **[Mod Compatibility](docs/mod-compatibility.md)**
- Docker setup: `docker build -f docker/Dockerfile .`

## How To (Mod & Shader Authors)

- Place data-driven Fabric / NeoForge mods in `mods/`, or pre-flight any modpack without a JVM using `megu-mod-check --scan-mods-dir ./mods` (or in your browser at **[megu.megumin.tech/#checker](https://megu.megumin.tech/#checker)**).
- Native `.mmod` format and `--compat-jar` for Modrinth/CurseForge: **[Mod SDK guide](docs/mod-sdk-guide.md)** · **[SDK agent guide](sdk/AGENTS.md)**
- Authoring guide & WGSL shader specifications: **[Mod & Shader Author Guide](docs/mod-author-guide.md)**
- End-to-end handling of embedded JARs, tag namespaces, mod sync and modded saves: **[Mod Compatibility](docs/mod-compatibility.md)**

## How To (Compiling From Source)

To compile Megu from source, you need **Rust `1.98.0`** (pinned in [rust-toolchain.toml](rust-toolchain.toml)) and Python 3:

```sh
python3 tools/dev_check.py
cargo build --locked --release
```

- Architecture & local test tiers: **[Development Guide](docs/development.md)** · **[Crate Architecture](docs/architecture-crates.md)**

## How To (Pull Request)

See **[Contributing](CONTRIBUTING.md)** and our **[Code of Conduct](CODE_OF_CONDUCT.md)**. All commits require DCO sign-off (`git commit -s`). For security issues, see **[SECURITY.md](SECURITY.md)**.

## Support Us

Megu is 100% free and open-source software under `LGPL-3.0-or-later`. Recurring infrastructure, CI runners, and hardware testing are supported by the community:

- **GitHub Sponsors**: **[github.com/sponsors/nekomario28](https://github.com/sponsors/nekomario28)**
- **Hosting & Commercial Partnerships**: **[partners@megumin.tech](mailto:partners@megumin.tech)**

## License

Licensed under **[LGPL-3.0-or-later](LICENSE)** (companion GPLv3 text in [docs/COPYING.GPL](docs/COPYING.GPL); see [LICENSING.md](LICENSING.md), [NOTICE](NOTICE), and [THIRD_PARTY.md](THIRD_PARTY.md)). Independent third-party Mod JARs loaded at runtime remain under their own licenses.

*Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft. ([TRADEMARK.md](TRADEMARK.md))*
