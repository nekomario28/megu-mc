#!/usr/bin/env sh
# SPDX-License-Identifier: LGPL-3.0-or-later
# Megu (megu-mc) Linux installer — downloads the verified Linux release bundle,
# checks SHA-256 digests before installing, stages binaries under ~/.local, and
# runs megu-installer (Official Launcher profile by default, or prism/server).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh | sh
#   sh tools/install_linux.sh [client|prism|server --dir <path>|uninstall|--binaries-only]

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

OS="$(uname -s)"
ARCH="$(uname -m)"
[ "$OS" = "Linux" ] || die "this installer targets Linux (detected OS: $OS)"
case "$ARCH" in
    x86_64|amd64) TARGET="linux-x86_64" ;;
    *) die "unsupported Linux architecture: $ARCH (prebuilt binaries currently target x86_64)" ;;
esac

need_cmd curl
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
curl -fsSL "${BASE_URL}/${SUMS}" -o "${WORK}/${SUMS}"
curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "${WORK}/${ARCHIVE}"
curl -fsSL "${BASE_URL}/${JAR}" -o "${WORK}/${JAR}"

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

if [ -d "${HOME}/.minecraft" ] || [ "$1" != "client" ]; then
    say "Running: megu-installer $*"
    "${BUNDLE_DIR}/megu-installer" "$@"
else
    say "Note: ~/.minecraft was not found yet. Run '${BIN_DIR}/megu-installer' after initializing the Official Launcher, or '${BIN_DIR}/megu-installer prism' for Prism Launcher."
fi

say "Complete! Ensure ${BIN_DIR} is in your PATH."
