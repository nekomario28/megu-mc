# Megu (`megu-mc`)

**English** | **[日本語 (README.ja.md)](README.ja.md)**

> Clean-room Rust engine & native GPU client for **Minecraft: Java Edition 1.21.1** (Protocol 767).
>
> **[Official Site](https://megu.megumin.tech/)** · **[Releases](https://github.com/nekomario28/megu-mc/releases)** · **[License: LGPL-3.0-or-later](LICENSE)** · **[Sponsors](https://github.com/sponsors/nekomario28)**

Megu is a **100% clean-room Rust server and native GPU client (Vulkan / DX12 / Metal — zero OpenGL dependency)** targeting Minecraft: Java Edition 1.21.1. It focuses on faithful vanilla behavior, low memory usage, fast startup, and safe data-driven mod compatibility.

> **Alpha Notice (`v0.1.0-alpha.1`)**: Megu is an early pre-release. It does not yet cover 100% of vanilla features or run arbitrary complex Java/Mixin mods natively. See the [Roadmap](specs/ROADMAP.md) for current boundaries.
>
> *Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.* ([TRADEMARK.md](TRADEMARK.md))

---

## Highlights

- **Vanilla 1.21.1 Focus**: Continuously verified against the official 1.21.1 server across core gameplay, survival, redstone, and Protocol 767 packets.
- **Lightweight Rust Server**: Fast boot, low memory footprint, and an external zero-overhead TUI dashboard.
- **Native GPU Client**: Pure Vulkan (Linux), DX12 & Vulkan (Windows), and Metal (macOS) rendering with zero OpenGL dependencies. Reads assets directly from your local `1.21.1.jar`.
- **Safe Mod Compatibility**: Built-in data-driven loader for Fabric / NeoForge packs plus a static bytecode security scanner (no JVM required for scanning).
- **One-Click Installers**: Standalone native `.exe` / Linux / macOS installers for the Official Launcher, Prism Launcher, Modrinth (`.mrpack`), and dedicated servers.

---

## Quick Start

Download the installer or archive for your OS from **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases)** or **[megu.megumin.tech](https://megu.megumin.tech/)**.

### 1. Client Installation (Official Launcher / Prism Launcher)

#### Windows (`.exe` — No Java required)
Download `megu-installer-windows-x86_64.exe` (or extract `megu-v0.1.0-alpha.1-windows-x86_64.tar.gz`) and run:

```powershell
# Double-click megu-installer.exe in Explorer, or run in PowerShell:
.\megu-installer.exe

# Or create a Prism Launcher instance:
.\megu-installer.exe prism
```

#### Linux (One-liner or native binary)
```sh
# Automatic Linux installer (verifies SHA-256, installs to ~/.local, registers profile):
curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh | sh

# Or run the standalone binary from the release archive:
./megu-installer
```

#### macOS (Apple Silicon)
```sh
# Extract the macOS archive and run:
./megu-installer
```

*(When `megu-client-gpu` and `megu-bootstrapper.jar` are in the same folder as `megu-installer`, they are detected and wired automatically without extra flags.)*

### 2. Dedicated Server

```sh
# Initialize a server directory (creates eula.txt with eula=false, run.sh / run.bat)
./megu-installer server --dir ./megu-server

# Review and accept eula.txt (eula=true), then start the server (default bind: 127.0.0.1)
cd ./megu-server
./megu --port 25565
```

See the [Operator Guide](docs/operator-guide.md) and [SECURITY_NOTES.md](SECURITY_NOTES.md) before exposing a server outside localhost.

---

## Current Scope (`v0.1.0-alpha.1`)

| Supported Today | Work in Progress / Out of Scope |
|---|---|
| Core 1.21.1 gameplay, survival, redstone, Protocol 767 | 100% coverage of all biomes, structures, and mob AI |
| Native Vulkan / DX12 / Metal GPU client, UI, audio | Full compatibility with every external OptiFine / Iris shaderpack |
| Data-driven mods & static JAR security inspection | Arbitrary complex Java / Mixin code execution |
| Linux, macOS, and Windows release builds & installers | Large-scale public server soak testing across all hardware |

---

## Build from Source

Prerequisites: Rust `1.98.0` (pinned in [rust-toolchain.toml](rust-toolchain.toml)) and Python 3.

```sh
python3 tools/dev_check.py
cargo build --locked --release
```

For local test-data generation, testing tiers, and crate architecture, see the [Development Guide](docs/development.md), [Crate Architecture](docs/architecture-crates.md), and [CONTRIBUTING.md](CONTRIBUTING.md).

---

## Documentation

- **Users & Operators**: [Operator Guide](docs/operator-guide.md) · [Mod Author Guide](docs/mod-author-guide.md) · [Security Notes](SECURITY_NOTES.md)
- **Contributors**: [Development Guide](docs/development.md) · [Crate Architecture](docs/architecture-crates.md) · [Roadmap](specs/ROADMAP.md) · [Contributing (DCO)](CONTRIBUTING.md) · [Code of Conduct](CODE_OF_CONDUCT.md)
- **Legal & Security**: [Security Policy](SECURITY.md) · [Licensing Details](LICENSING.md) · [Third-Party Notices](THIRD_PARTY.md) · [Trademark Guidelines](TRADEMARK.md)

## Support & Sponsorship

Megu is 100% free and open-source software under `LGPL-3.0-or-later`.

- **GitHub Sponsors**: [github.com/sponsors/nekomario28](https://github.com/sponsors/nekomario28)
- **Hosting & Commercial Partnerships**: `partners@megumin.tech` ([megu.megumin.tech/#support](https://megu.megumin.tech/#support))

## License & Clean-Room Policy

- **License**: **`LGPL-3.0-or-later`** ([LICENSE](LICENSE), companion GPLv3 text in [docs/COPYING.GPL](docs/COPYING.GPL), [NOTICE](NOTICE), [LICENSING.md](LICENSING.md)).
- Independent external Mod JARs loaded dynamically at runtime remain under their own licenses (copyleft does not spread to external mod JARs).
- Megu is a 100% clean-room original Rust implementation. No decompiled Mojang code/assets and no PolyForm Shield or All Rights Reserved code/shaders are copied or distributed. Contributions require `LGPL-3.0-or-later` and DCO sign-off (`git commit -s`).
