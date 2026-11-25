<!-- SPDX-License-Identifier: LGPL-3.0-or-later -->

# Platform support matrix

Everything on this page is enforced: the release workflow
(`.github/workflows/release.yml`) builds every listed target on each tagged
release, `tests/release_workflow_test.py` and `tests/xplat_packaging_test.py`
fail if the matrix and this document drift apart.

## Tier 1 — shipped and exercised

Full lanes ship exactly three end-user binaries: `megu` (dedicated
server), `megu-client-gpu` (native wgpu client) and `megu-installer`
(launcher/server setup). The internal tools (`megu-client`,
`megu-mod-check`, `megu-world-migrate`, `megu-init`, `megu-admin`) stay
source-buildable but are not published.

| Target | Release archive | Contents | Notes |
| --- | --- | --- | --- |
| Windows x86_64 | `megu-*-windows-x86_64` | `megu` + `megu-client-gpu` + `megu-installer` | DX12 + Vulkan client |
| Windows ARM64 | `megu-*-windows-arm64` | `megu` + `megu-client-gpu` + `megu-installer` | DX12 + Vulkan client |
| macOS ARM64 | `megu-*-macos-arm64` | `megu` + `megu-client-gpu` + `megu-installer` | Metal client, macOS 11+ floor |
| macOS x86_64 | `megu-*-macos-x86_64` | `megu` + `megu-client-gpu` + `megu-installer` | cross from arm64 runner |
| Linux glibc x86_64 | `megu-*-linux-x86_64` | `megu` + `megu-client-gpu` + `megu-installer` | glibc ≥ 2.35 (Ubuntu 22.04 baseline) |
| Linux glibc ARM64 | `megu-*-linux-arm64` | `megu` + `megu-client-gpu` + `megu-installer` | covers SteamOS-adjacent aarch64 hosts |
| Linux musl x86_64 server | `megu-*-linux-x86_64-musl` | `megu` + `megu-installer` | `+crt-static`, `FROM scratch`-ready |
| Linux musl ARM64 server | `megu-*-linux-arm64-musl` | `megu` + `megu-installer` | `+crt-static`, `FROM scratch`-ready |

Every archive is verified against its own `release-manifest.json` during
the release build and again at publish time; the GitHub Release then
attaches a single aggregated `megu-<version>-release-manifest.json`
covering all twelve targets instead of twelve per-target files.

SteamOS is served by `linux-x86_64` (glibc): Steam Deck's SteamOS image is
Arch/glibc-based and runs both the server and the GPU client natively.

## Tier 1.5 — shipped, narrower scope

| Target | Release archive | Contents | Why it is narrow |
| --- | --- | --- | --- |
| Linux musl x86_64 client | `megu-*-linux-x86_64-musl-client` | `megu-client-gpu` | Dynamic musl build (Alpine container, `-C target-feature=-crt-static`) so Vulkan/Wayland/X11 can be dlopen'ed; GUI testing is manual. |
| Linux musl ARM64 client | `megu-*-linux-arm64-musl-client` | `megu-client-gpu` | Same contract, aarch64. |
| FreeBSD x86_64 | `megu-*-freebsd-x86_64` | `megu` + `megu-installer` | Server only; built against a checksum-pinned FreeBSD 14.4-RELEASE sysroot, runs on 14.x and 15.x (COMPAT_FREEBSD14). |
| Linux RISC-V 64 | `megu-*-linux-riscv64` | `megu` + `megu-installer` | Server only; `riscv64gc-unknown-linux-gnu` via `gcc-riscv64-linux-gnu`, lp64d ABI. |

### Dynamic musl client contract

Rust's `*-unknown-linux-musl` targets default to `+crt-static`. That is
correct for the server bundles (they run under `FROM scratch` with zero
libraries) but wrong for the GPU client: Vulkan, Wayland and X11 drivers
only exist as shared objects. `packaging/docker/Dockerfile.alpine-client` builds
`megu-client-gpu` inside `alpine:3.21` with `-C target-feature=-crt-static`
and refuses the artifact if it is not an `ld-musl-*` dynamic binary. The
client archive is kept in a separate `*-musl-client` target so the static
server bundles can never accidentally go dynamic. `tools/install_linux.sh`
downloads and checksum-verifies both musl halves, then stitches the GPU
client into the local installation bundle so callers still see exactly the
three slim end-user binaries.

### FreeBSD build path

`megu` cross-compiles on `ubuntu-22.04` with `clang --target=x86_64-unknown-
freebsd` + `ld.lld` against `base.txz` from `download.freebsd.org`
(SHA-256-pinned in the workflow). `ring` 0.17.14 compiles its generic C
sources for this target — verified end to end during development. The GPU
client is **not** shipped for FreeBSD yet: `client-gpu/Cargo.toml` already
unlocks the wgpu Vulkan backend on `freebsd`/`openbsd`/`netbsd`/`dragonfly`,
but `winit`'s Wayland/X11 surfaces on BSD have not been exercised in CI —
source builds work (`cargo build --manifest-path client-gpu/Cargo.toml` on a
FreeBSD host with `vulkan-loader`, `wayland`, `libxkbcommon`, `alsa-lib`
installed) and are community-supported only.

### Linux specifics that do not gate BSD

The server has two `cfg(target_os = "linux")` sites, both with correct
non-Linux fallbacks: the size-log RSS probe (`src/main.rs`, reads
`/proc/self/status`, prints an empty field elsewhere) and
`backup_volume_free_bytes` (`src/operator_ux.rs`, `statvfs`, returns `None`
elsewhere so the prune plan prints without a disk verdict).

The GPU client has one more: `detect_backend` in `client-gpu/src/clipboard.rs`
autodetects the `wl-copy`/`xclip` system clipboard only on Linux — on
FreeBSD and the other BSDs the clipboard falls back to the in-process
buffer (`Backend::None`), which is correct but loses desktop integration.
`lib.rs` and `window.rs` already gate on `unix, not(macos)` and pick up the
BSDs for free. Verified: `cargo check --target x86_64-unknown-freebsd` for
`client-gpu` completes on the pinned 1.98.0 toolchain.

## Explicit non-goals

- **OpenBSD / NetBSD / DragonFly builds**: the Vulkan cfg is unlocked in
  `client-gpu/Cargo.toml`, but no CI lane or sysroot exists yet.
- **FreeBSD client, OpenBSD/NetBSD anything**: see above.
- **32-bit targets, big-endian, RISC-V client, s390x/ppc64le**: not built.
- **musl client for `megu-client` (headless CLI)**: covered by glibc builds;
  the musl-client lane exists only for the GPU binary.
- **Nix**: not packaged yet. Homebrew, Scoop, Winget and AUR metadata live under `packaging/` and are synchronized by `tools/check_pkg_manifests.py`.
