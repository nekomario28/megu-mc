<div align="center">
  <img src="website/logo.svg" width="84" height="84" alt="Megu Logo">
  <h1>Megu</h1>
  <p><strong>Clean-room Rust server &amp; native GPU client (Vulkan / DX12 / Metal) for Minecraft: Java Edition 1.21.1</strong></p>
  <p>
    <a href="https://github.com/nekomario28/megu-mc/releases/latest"><img src="https://img.shields.io/github/v/release/nekomario28/megu-mc?include_prereleases&label=release&color=e05d44" alt="Latest Release"></a>
    <a href="https://megu.megumin.tech/"><img src="https://img.shields.io/badge/minecraft-1.21.1%20(Protocol%20767)-2ea44f" alt="Minecraft 1.21.1"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-LGPL--3.0--or--later-blue" alt="License: LGPL-3.0-or-later"></a>
    <a href="https://github.com/sponsors/nekomario28"><img src="https://img.shields.io/github/sponsors/nekomario28?label=Sponsors&color=db61a2" alt="GitHub Sponsors"></a>
  </p>
  <p>
    <strong>English</strong> · <a href="README.ja.md"><strong>日本語</strong></a> · <a href="https://megu.megumin.tech/"><strong>Website</strong></a> · <a href="https://github.com/nekomario28/megu-mc/releases/latest"><strong>Downloads</strong></a> · <a href="docs/operator-guide.md"><strong>Docs</strong></a>
  </p>
</div>

---

Megu is a free, open-source, clean-room Rust server and pure-`wgpu` native GPU client for **Minecraft: Java Edition 1.21.1**, built for low memory usage, fast startup, and zero OpenGL dependency.

> **Alpha Status (`v0.1.0-alpha.1`)**: Early pre-release. Core 1.21.1 gameplay, native Vulkan/DX12/Metal rendering, and data-driven mods work today, while 100% vanilla worldgen/AI coverage and arbitrary Java Mixin execution remain in progress. See the [Roadmap](specs/ROADMAP.md).

## How To (Players)

Go to the **[Downloads page](https://megu.megumin.tech/#download)** or **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases/latest)**:

- **Official Launcher (Windows / Linux / macOS)**:
  - **Windows**: Download **[`megu-installer-windows-x86_64.exe`](https://github.com/nekomario28/megu-mc/releases/latest/download/megu-installer-windows-x86_64.exe)** and run it (no Java required). Select the **Megu (1.21.1)** profile in your Minecraft Launcher and play!
  - **Linux**: Run `curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh | sh`
  - **macOS (Apple Silicon)**: Download and run **[`megu-installer-macos-arm64`](https://github.com/nekomario28/megu-mc/releases/latest/download/megu-installer-macos-arm64)**
- **Prism Launcher / Modrinth App**:
  - Download **[`megu-1.21.1.mrpack`](https://github.com/nekomario28/megu-mc/releases/latest/download/megu-1.21.1.mrpack)** and drag & drop it directly into your launcher (or run `megu-installer prism`).

## How To (Server Admins)

Download the release archive for your OS from **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases/latest)**, initialize a server directory, accept `eula.txt`, and start `megu`:

```sh
./megu-installer server --dir ./megu-server
# Set eula=true in ./megu-server/eula.txt after reviewing the Minecraft EULA:
cd ./megu-server && ./megu --port 25565
```

- Documentation on configuring and hosting Megu: **[Operator Guide](docs/operator-guide.md)** · **[Security Notes](SECURITY_NOTES.md)**
- Docker setup: `docker build -f docker/Dockerfile .`

## How To (Mod & Shader Authors)

- Place data-driven Fabric / NeoForge mods in `mods/`, or pre-flight any modpack without a JVM using `megu-mod-check --scan-mods-dir ./mods` (or in your browser at **[megu.megumin.tech/#checker](https://megu.megumin.tech/#checker)**).
- Authoring guide & WGSL shader specifications: **[Mod & Shader Author Guide](docs/mod-author-guide.md)**

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
- **Hosting & Commercial Partnerships**: `partners@megumin.tech` (**[megu.megumin.tech/#support](https://megu.megumin.tech/#support)**)

## License

Licensed under **[LGPL-3.0-or-later](LICENSE)** (companion GPLv3 text in [docs/COPYING.GPL](docs/COPYING.GPL); see [LICENSING.md](LICENSING.md), [NOTICE](NOTICE), and [THIRD_PARTY.md](THIRD_PARTY.md)). Independent third-party Mod JARs loaded at runtime remain under their own licenses.

*Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft. ([TRADEMARK.md](TRADEMARK.md))*
