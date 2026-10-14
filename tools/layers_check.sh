#!/usr/bin/env bash
# Source-bound dependency policy + the separate intended design model.
set -euo pipefail
cd "$(dirname "$0")/.."
export PYTHONDONTWRITEBYTECODE=1
python3 tools/check_layers.py
