#!/usr/bin/env bash
# Source-bound dependency policy + the separate intended design model.
set -euo pipefail
cd "$(dirname "$0")/.."
export PYTHONDONTWRITEBYTECODE=1

# The inspector is pinned to an immutable revision (`.github/workflows/ci.yml`,
# GXKIT_REV). A build from an older tree reports the same version string and fewer
# faults - measured: one independent cycle instead of two - so "a depsgx is on
# PATH" is not the contract. Prefer an explicit build, then a host-local build of
# the pin, then PATH; check_layers.py proves whichever one it gets can fail.
VENV_INSPECTOR="${MEGU_DEPSGX_VENV:-$HOME/.cache/megu-work/gxkit-venv/bin/depsgx}"
if [ -z "${MEGU_DEPSGX:-}" ] && [ -x "$VENV_INSPECTOR" ]; then
  export MEGU_DEPSGX="$VENV_INSPECTOR"
fi

python3 tools/check_layers.py
