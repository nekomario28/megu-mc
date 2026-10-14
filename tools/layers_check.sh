#!/usr/bin/env bash
# The module layering, as a gate.
#
# Two claims, checked separately because they fail differently.
#
#   target - the layering S2.2 is aiming for must stay acyclic. That file is a design artifact: if a
#            later change makes it report a violation, the intended architecture is no longer
#            reachable and no code should move until that is understood. This must be clean, always.
#
#   current - the layering as the code stands today is allowed to be broken, because it is, but the
#            number of errors must come down. Pinned at today's count so that moving code out of
#            main.rs cannot quietly make it worse.
#
# depsgx reads the graph and reports stable codes, so this branches on the codes and not on prose.
# Run: bash tools/layers_check.sh
set -uo pipefail
cd "$(dirname "$0")/.."

DIR=evidence/foundation/module-layers
TARGET_LIMIT=0
CURRENT_LIMIT=3   # feature -> tree -> feature, plus tree and worldgen reaching up into feature

if [ ! -d "$DIR" ]; then
  echo "no $DIR; nothing to check" >&2
  exit 2
fi
if ! command -v depsgx >/dev/null 2>&1; then
  # Skipping with a message, not passing quietly. A gate that reports health while running nothing is
  # the failure mode this whole session kept finding; saying "skipped" is the honest version.
  echo "SKIP  depsgx is not installed, so the layering is not being checked here."
  echo "      install: uvx --from 'git+https://github.com/nekomario28/gxkit#subdirectory=packages/depsgx' depsgx"
  exit 0
fi

errors_in() { # file -> number of errors, or "?" when the output cannot be read
  local out
  out=$(depsgx inspect "$1" 2>&1) || true
  if echo "$out" | grep -q 'inspect: no problems'; then
    echo 0
  elif echo "$out" | grep -qE 'inspect: [0-9]+ problems \([0-9]+ errors\)'; then
    echo "$out" | sed -nE 's/.*inspect: [0-9]+ problems \(([0-9]+) errors\).*/\1/p' | tail -1
  else
    echo "?"
  fi
}

failed=0

t=$(errors_in "$DIR/deps_target.py")
case "$t" in
  "$TARGET_LIMIT") echo "ok    target layering is acyclic" ;;
  "?") echo "BLIND target: depsgx output could not be read, so this proves nothing"; failed=1 ;;
  *) echo "FAIL  target layering has $t error(s): the intended architecture is no longer reachable"
     depsgx inspect "$DIR/deps_target.py" 2>&1 | grep -E '^  \[' | head -5
     failed=1 ;;
esac

c=$(errors_in "$DIR/deps.py")
case "$c" in
  "?") echo "BLIND current: depsgx output could not be read, so this proves nothing"; failed=1 ;;
  *)
    if [ "$c" -gt "$CURRENT_LIMIT" ]; then
      echo "GREW  current layering: $c error(s), limit $CURRENT_LIMIT"
      echo "      moving code must reduce this, not add to it. Lower the limit as it drops."
      failed=1
    elif [ "$c" -lt "$CURRENT_LIMIT" ]; then
      echo "DOWN  current layering: $c error(s), limit $CURRENT_LIMIT - lower the limit to lock it in."
    else
      echo "ok    current layering: $c error(s)"
    fi ;;
esac

echo
if [ "$failed" -ne 0 ]; then
  echo "layers check: failed"
  exit 1
fi
echo "layers check: ok"
