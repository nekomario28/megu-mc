#!/usr/bin/env bash
# Build a local release candidate. Never change a source ref or inherit its object database.
set -euo pipefail
usage() { echo 'Usage: export_public_branch.sh --out <empty-or-new-dir> [--branch <name>] [--verify] [--from-worktree]'; }
die() { echo "export: $*" >&2; exit 1; }
out= branch=public-export verify=0 worktree=0
while (($#)); do
    case $1 in
        --out|--branch)
            [[ -n ${2:-} && $2 != --* ]] || die "missing value for $1"
            if [[ $1 == --out ]]; then out=$2; else branch=$2; fi
            shift 2 ;;
        --verify) verify=1; shift ;;
        --from-worktree) worktree=1; shift ;;
        --help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
done
[[ -n $out ]] || { usage >&2; exit 1; }
[[ ! -L $out ]] || die 'destination is a symlink'
out=$(realpath -m -- "$out")
cd -P -- "$(dirname -- "$0")/.."
# Git hooks, in particular, can pass the caller's index/worktree to child processes.
repo_env=$(git rev-parse --local-env-vars)
unset $repo_env GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS
src=$(git rev-parse --show-toplevel)
common=$(git rev-parse --path-format=absolute --git-common-dir)
case "$out/" in "$src/"*|"$common/"*) die 'destination must be outside the source repository' ;; esac
git check-ref-format --branch "$branch" >/dev/null
head=$(git rev-parse HEAD)
short=$(git rev-parse --short "$head")
if [[ -e $out ]]; then
    [[ -d $out ]] || die 'destination is not a directory'
    first=$(find "$out" -mindepth 1 -maxdepth 1 -print -quit)
    [[ -z $first ]] || die "destination is not empty: $out"
fi
# Install by rename only after every check passes. Never delete a caller-owned directory.
parent=$(dirname -- "$out")
mkdir -p -- "$parent"
stage=$(mktemp -d -- "$parent/.megu-export.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
if ((worktree)); then
    { git ls-files -z; git ls-files --others --exclude-standard -z; } | tar --null --no-recursion -cf - -T - | tar -xf - -C "$stage"
else
    git archive --format=tar "$head" | tar -xf - -C "$stage"
fi
cd -- "$stage"
# No links in the current export format: do not follow one while removing or writing files.
# world_overlay*.bin* is local server state that must never be archived or tracked.
unsafe=$(find . \( -type l -o -name .git -o -name '*.jar' -o -name '*.class' \
                -o -name 'world_overlay*.bin*' \) -print -quit)
[[ -z $unsafe ]] || die "unexpected archive member: $unsafe"
excluded=(datapacks/vanilla/ assets/block_states_1211.json config_packets.bin
          tests/data/biome_probe_1211.tsv tests/data/vanilla_vectors_1211.tsv
          tests/data/biome_probe_ties_1211.tsv tests/data/random_next_int_1211.tsv
          resourcepack.zip assets/pblocks.json assets/properties.json)
printf '\n# Local inputs excluded from the public export\n' >> .gitignore
for path in "${excluded[@]}"; do
    rm -rf -- "${path%/}"
    printf '/%s\n' "$path" >> .gitignore
done
# Every remaining *_1211* name under assets/ and tests/data/ is generated from the
# pinned 1.21.1 jar by tools/vanilla_data/ - Mojang-derived data the public tree
# regenerates instead of redistributing. Collect first so removing a directory
# cannot confuse the traversal.
generated=()
for dir in assets tests/data; do
    [[ -d $dir ]] || continue
    while IFS= read -r -d '' path; do generated+=("$path"); done \
        < <(find "$dir" -name '*_1211*' -print0)
done
for path in "${generated[@]}"; do rm -rf -- "$path"; done
# Strip raw local systemd journal dumps (`*unit.jsonl`) from public exports so
# host paths and invocation IDs never enter the public source tree.
while IFS= read -r -d '' path; do rm -f -- "$path"; done \
    < <(find evidence -name '*unit.jsonl' -print0 2>/dev/null || true)
printf '/assets/**/*_1211*\n/tests/data/**/*_1211*\n/evidence/**/*unit.jsonl\n' >> .gitignore

git init -q --template= -b "$branch"
git -c core.hooksPath=/dev/null add -- .
GIT_AUTHOR_NAME="${GIT_AUTHOR_NAME:-megu release}" \
GIT_AUTHOR_EMAIL="${GIT_AUTHOR_EMAIL:-release@megu.invalid}" \
GIT_COMMITTER_NAME="${GIT_COMMITTER_NAME:-megu release}" \
GIT_COMMITTER_EMAIL="${GIT_COMMITTER_EMAIL:-release@megu.invalid}" \
    git -c core.hooksPath=/dev/null -c commit.gpgSign=false commit -q -s -m "Clean public export from $short (P0-3 D5)"

count=$(git rev-list --count HEAD)
parents=$(git show -s --format=%P HEAD)
[[ $count == 1 && -z $parents ]] || die 'expected a single root commit'
git ls-files --error-unmatch -- .gitignore >/dev/null
for path in "${excluded[@]}"; do
    git check-ignore -q --no-index -- "$path" || die "not ignored: $path"
done
for path in assets/blocks_1211.json tests/data/dimension_terrain_1211 \
            tests/data/basalt_columns_1211.tsv world_overlay.bin \
            world_overlay_nether.bin.bak; do
    git check-ignore -q --no-index -- "$path" || die "not ignored: $path"
done
forbidden=$(git ls-files -- '*.jar' '*.class' 'world_overlay*.bin*' \
                         'assets/*_1211*' 'tests/data/*_1211*' "${excluded[@]%/}")
[[ -z $forbidden ]] || die "forbidden index entries: $forbidden"
ignored=$(git ls-files -ci --exclude-standard)
[[ -z $ignored ]] || die "tracked ignored files: $ignored"
leftover=$(git ls-files --others)
[[ -z $leftover ]] || die "files omitted by git add: $leftover"

if ((verify)); then
    python3 tools/provenance_check.py
    python3 tools/provenance_check.py --self-test
    python3 tools/provenance_check.py --strict-public
    rc=0
    report=$(python3 tools/dev_check.py 2>&1) || rc=$?
    printf '%s\n' "$report"
    [[ $rc == 1 ]] || die "expected dev_check exit 1, got $rc"
    if grep -Eq '\[ERROR *\]' <<< "$report"; then die 'dev_check reported an error'; fi
    grep -A1 -F '[MISSING    ] build_inputs' <<< "$report" | grep -E 'datapacks/vanilla/|_1211' >/dev/null || die 'expected missing Mojang build inputs'
    grep -E '^\[MISSING    \] runtime_data .*datapacks/vanilla.*config_packets\.bin' <<< "$report" >/dev/null || die 'expected missing runtime inputs'
fi
mv -T -- "$stage" "$out"
trap - EXIT
printf 'Exported %s to %s (%s); LICENSING.md §5 still applies.\n' "$short" "$out" "$branch"
