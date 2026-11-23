#!/usr/bin/env sh
# SPDX-License-Identifier: LGPL-3.0-or-later
# Megu (megu-mc) Linux installer — downloads the verified Linux release bundle,
# checks SHA-256 digests before installing, stages binaries under ~/.local, and
# runs megu-installer (Official Launcher profile by default, or prism/server).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh | sh
#   sh tools/install_linux.sh [client|prism|server --dir <path>|uninstall|--binaries-only]
#
# Environment:
#   MEGU_TARGET         Override the detected release target (e.g. linux-x86_64-musl)
#   MEGU_VERSION        Release tag to install (default: v0.1.0-alpha.1)
#   MEGU_GITHUB_REPO    owner/repo override (default: nekomario28/megu-mc)
#   MEGU_RELEASE_BASE_URL  Full base URL override for mirrors

set -eu

REPO="${MEGU_GITHUB_REPO:-nekomario28/megu-mc}"
VERSION="${MEGU_VERSION:-v0.1.0-alpha.1}"
INSTALL_ROOT="${MEGU_INSTALL_DIR:-$HOME/.local/share/megu}"
BIN_DIR="${MEGU_BIN_DIR:-$HOME/.local/bin}"

say() {
    printf '[megu-install] %s\n' "$*"
}

die() {
    printf '[megu-install] error: %s\n' "$*" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# http_get <url> <output>: fetch a URL with curl when available, otherwise
# wget (BusyBox wget on Alpine supports `-q -O <file> <url>`). Fails only when
# neither tool exists or the download itself fails.
http_get() {
    _url="$1"
    _out="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$_url" -o "$_out" && return 0
    fi
    if command -v wget >/dev/null 2>&1; then
        wget -q -O "$_out" "$_url" && return 0
    fi
    return 1
}

is_musl() {
    if command -v ldd >/dev/null 2>&1; then
        if ldd --version 2>&1 | grep -qi musl; then
            return 0
        fi
    fi
    for _libc in /lib/ld-musl-*.so.1 /usr/lib/ld-musl-*.so.1 /lib/libc.musl-*.so.1; do
        if [ -e "$_libc" ]; then
            return 0
        fi
    done
    return 1
}

OS="$(uname -s)"
ARCH="$(uname -m)"
[ "$OS" = "Linux" ] || die "this installer targets Linux (detected OS: $OS)"

if [ -n "${MEGU_TARGET:-}" ]; then
    TARGET="$MEGU_TARGET"
else
    LIBC="gnu"
    if is_musl; then LIBC="musl"; fi
    case "$ARCH" in
        x86_64|amd64)
            if [ "$LIBC" = "musl" ]; then TARGET="linux-x86_64-musl"; else TARGET="linux-x86_64"; fi
            ;;
        aarch64|arm64)
            if [ "$LIBC" = "musl" ]; then TARGET="linux-arm64-musl"; else TARGET="linux-arm64"; fi
            ;;
        *) die "unsupported Linux architecture: $ARCH (supported: x86_64/amd64 -> linux-x86_64 or linux-x86_64-musl, aarch64/arm64 -> linux-arm64 or linux-arm64-musl; override with MEGU_TARGET)" ;;
    esac
fi

if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    die "neither curl nor wget found; install one to download the release (e.g. apk add wget on Alpine)"
fi
need_cmd tar
need_cmd sha256sum
need_cmd mktemp

ARCHIVE="megu-${VERSION}-${TARGET}.tar.gz"
JAR="megu-bootstrapper.jar"
SUMS="SHA256SUMS-all.txt"
BASE_URL="${MEGU_RELEASE_BASE_URL:-https://github.com/${REPO}/releases/download/${VERSION}}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/megu-install.XXXXXXXX")"
cleanup() {
    rm -f "$WORK/$ARCHIVE" "$WORK/$JAR" "$WORK/$SUMS" "$WORK/check.sums" 2>/dev/null || true
    rmdir "$WORK" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

say "Downloading Megu ${VERSION} (${TARGET}) from ${BASE_URL} ..."
http_get "${BASE_URL}/${SUMS}" "${WORK}/${SUMS}" || die "download failed: ${BASE_URL}/${SUMS}"
http_get "${BASE_URL}/${ARCHIVE}" "${WORK}/${ARCHIVE}" || die "download failed: ${BASE_URL}/${ARCHIVE}"
http_get "${BASE_URL}/${JAR}" "${WORK}/${JAR}" || die "download failed: ${BASE_URL}/${JAR}"

say "Verifying SHA-256 checksums ..."
grep -E "  (${ARCHIVE}|${JAR})\$" "${WORK}/${SUMS}" > "${WORK}/check.sums" \
    || die "checksums for ${ARCHIVE} / ${JAR} not found in ${SUMS}"
(cd "$WORK" && sha256sum -c check.sums >/dev/null) \
    || die "SHA-256 verification failed"

mkdir -p "$INSTALL_ROOT" "$BIN_DIR"
tar -xzf "${WORK}/${ARCHIVE}" -C "$INSTALL_ROOT"
BUNDLE_DIR="${INSTALL_ROOT}/megu-${VERSION}-${TARGET}"
[ -d "$BUNDLE_DIR" ] || die "expected extracted directory ${BUNDLE_DIR} not found"
cp -f "${WORK}/${JAR}" "${BUNDLE_DIR}/${JAR}"

for bin in megu megu-client megu-client-gpu megu-installer megu-mod-check megu-world-migrate megu-init megu-admin; do
    if [ -f "${BUNDLE_DIR}/${bin}" ]; then
        chmod 0755 "${BUNDLE_DIR}/${bin}"
        ln -sf "${BUNDLE_DIR}/${bin}" "${BIN_DIR}/${bin}"
    fi
done

say "Installed Megu binaries to ${BUNDLE_DIR} (linked in ${BIN_DIR})"

if [ "${1:-}" = "--binaries-only" ]; then
    say "Done (--binaries-only)."
    exit 0
fi

if [ "$#" -eq 0 ]; then
    set -- client
fi

# Detect standard ~/.minecraft or Flatpak ~/.var/app/com.mojang.Minecraft/.minecraft
MC_DIR="${HOME}/.minecraft"
if [ ! -d "$MC_DIR" ] && [ -d "${HOME}/.var/app/com.mojang.Minecraft/.minecraft" ]; then
    MC_DIR="${HOME}/.var/app/com.mojang.Minecraft/.minecraft"
fi

if [ "$1" = "client" ]; then
    mkdir -p "${MC_DIR}/bin"
    cp -f "${BUNDLE_DIR}/megu-client-gpu" "${MC_DIR}/bin/megu-client-gpu"
    cp -f "${BUNDLE_DIR}/megu" "${MC_DIR}/bin/megu"
    chmod 0755 "${MC_DIR}/bin/megu-client-gpu" "${MC_DIR}/bin/megu"
fi

say "Running: megu-installer $*"
"${BUNDLE_DIR}/megu-installer" "$@"

say "Complete! Open the Minecraft Launcher and select 'Megu (1.21.1)'."
