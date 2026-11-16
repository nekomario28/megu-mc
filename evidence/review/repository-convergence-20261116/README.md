# Local repository convergence inventory — 2026-11-16

This directory owns **one** machine-readable disposition ledger for the local
branches, T3 worktrees and the GitHub PR surface of `nekomario28/mc-spine`.
It records what exists, what duplicates what, and what must not be touched.
It does not re-explain the work: meaning stays in `specs/`, `evidence/` and the
owning T3 tasks.

- `inventory.json` — the ledger (`mc-spine/repository-convergence-inventory@1`)
- `refresh.py` — read-only validator: reports rows that went stale, never rewrites a verdict
- `refresh_test.py` — negative controls for the validator

Nothing here was executed as a build or test. Every disposition is a Git-object
or patch comparison; see *Limits* below.

## Snapshot

| Fact | Value |
|---|---|
| canonical ref / SHA | `perf/worldgen-parallel` @ `e7ead8e66466b0b6467450cd3ea31d86a0e39cd3` |
| base | `main` @ `a205e2d56405edd99316616285e2bbd7e0b595a7` (canonical is 38 commits ahead) |
| live remote | `origin` = `https://github.com/nekomario28/mc-spine`, 9 heads via `git ls-remote --heads origin` |
| `origin/perf/worldgen-parallel` | `0d3b4a8810dc0a47f8ae2e09d7a31ded7e7d87ee` — canonical is 20 commits ahead, never pushed |
| refs | 50 local heads, 10 remote-tracking, 1 tag, 183 `refs/t3/orchestration-v2/*` (T3 checkpoints, out of scope) |
| worktrees | 49 (canonical root + 5 Antigravity + 43 T3); live counts move, `counts.*_total` in `inventory.json` is authoritative |
| PRs | 34 total: 7 open, 18 merged, 9 closed-unmerged |

Remote-tracking refs matched the live remote for all 9 heads at the snapshot.
The 183 `refs/t3/...` entries are orchestrator checkpoints, not branches; they
are deliberately excluded.

## Dispositions

Carrier-level counts from `inventory.json`:

| Disposition | refs | worktrees | meaning |
|---|---:|---:|---|
| `CANONICAL` | 2 | 1 | `main` + `perf/worldgen-parallel`; the integration target |
| `ACTIVE` | 8 | 8 | a running task owns it; never touch |
| `PATCH_EQUIVALENT` | 23 | 23 | every unique commit duplicates one in canonical or in the integration candidate |
| `RETIRE_CANDIDATE` | 8 | 8 | merged, clean, no untracked artifacts |
| `UNKNOWN_HOLD` | 9 | 9 | uncommitted/untracked work exists only here, or a new ref has no identified owner |
| `UNMERGED_UNIQUE` | 0 | 0 | no **additional** unique committed work found among the classified, non-active historical carriers, relative to canonical `e7ead8e` **and** the named candidate `2799a36` (scope below) |
| `MERGED_RETAIN_EVIDENCE` | 0 | 0 | no local ref needs it; used for the 18 merged PRs, whose commits are in `main` |

`0` in that row is a statement about **those rows only**: no additional unique committed
work was found among the classified, non-active historical carriers, when each tip was
compared against canonical `e7ead8e` **and** the named 5x candidate `2799a36`. It does not
clear `ACTIVE` or `UNKNOWN_HOLD`, and it is not evidence that the candidate is in canonical.
Two different relations are recorded separately, and neither implies the other:

- **canonical containment** — the tip is an ancestor of `perf/worldgen-parallel`
  (`equivalence_kind: ancestor`), or a commit is patch-equal to a commit that is already in
  canonical;
- **candidate containment** — the commit is patch-equal to a commit that exists only in the
  unaccepted candidate (`equivalence_kind: patch_id_equal_candidate`). The candidate is the
  `ACTIVE` carrier `t3/worldgen-integration-audit`, and it holds commits that canonical does
  not; that row is an observation and it gained commits during the audit.

Unique work that exists **only** as uncommitted or untracked bytes is not counted in the
table at all; it is listed per carrier in `inventory.json` (`artifacts_only_here`) and in
*Active protections* below.

## Active protections (do not edit, stash, reset or remove)

| Carrier | Why | Owner |
|---|---|---|
| `t3/worldgen-integration-audit` | 5A/5C/6A/6B/6C integration candidate, 0 behind canonical at the snapshot (the row gained commits during the audit) | [Independent acceptance and integration candidate](t3-thread://v2/0dad0cde-3d82-45b5-8725-7ba7b32672e2/thread:delegated-task:command%3Amcp%3A428c4310-c657-43bc-91ab-2d3e70b7535c%3Adelegate-task%3A4f1e78bd-d463-4e57-87bf-6e5e091237df) |
| `t3/chunk-state-reform` | root reform; spec/code in progress | [Root reform](t3-thread://v2/0dad0cde-3d82-45b5-8725-7ba7b32672e2/thread:delegated-task:command%3Amcp%3A428c4310-c657-43bc-91ab-2d3e70b7535c%3Adelegate-task%3Ae40bc849-7702-41d6-a608-6b5f3534eb5f) |
| `t3/mc-spine-contributor-docs` | contributor docs A (README/CONTRIBUTING/specs README/DESIGN/TODO/docs) | [Contributor readiness A](t3-thread://v2/0dad0cde-3d82-45b5-8725-7ba7b32672e2/thread:delegated-task:command%3Amcp%3A2ebeb717-82d9-4f01-b038-e73f5ca57dfe%3Adelegate-task%3Ae18bdcc8-7201-43c7-8bef-3fdfe454bc51) |
| `t3/mc-spine-contributor-preflight` | preflight B (`tools/dev_check.py`, `tests/dev_check_test.py`) | [Contributor readiness B](t3-thread://v2/0dad0cde-3d82-45b5-8725-7ba7b32672e2/thread:delegated-task:command%3Amcp%3A2ebeb717-82d9-4f01-b038-e73f5ca57dfe%3Adelegate-task%3Aa4001817-0beb-47f4-ade6-b8539d536298) |
| `t3/548b811e` | Track 5B Jungle residual, 344 uncommitted lines | [Track 5B](t3-thread://v2/0dad0cde-3d82-45b5-8725-7ba7b32672e2/thread:delegated-task:command%3Amcp%3A0e816a01-5e10-4059-8061-d9d6ec8ca26c%3Adelegate-task%3Af7e98a57-f974-403c-a3dd-32be99d666ac) |
| `t3/runtime-source-organization` | source cleanup outside the worldgen lanes | [Code organization](t3-thread://v2/0dad0cde-3d82-45b5-8725-7ba7b32672e2/thread:delegated-task:command%3Amcp%3A2ebeb717-82d9-4f01-b038-e73f5ca57dfe%3Adelegate-task%3A57a34768-8343-480c-9e5e-f5d2533b38e0) |
| `t3/repository-convergence-plan` | this audit; only adds this directory | [Repository convergence](t3-thread://v2/0dad0cde-3d82-45b5-8725-7ba7b32672e2/thread:delegated-task:command%3Amcp%3A2ebeb717-82d9-4f01-b038-e73f5ca57dfe%3Adelegate-task%3A9c9ce04a-be62-4a25-a622-9dd31b8491f4) |
| `t3/mc-spine-integration-review` | review worktree created during the audit | — |

`UNKNOWN_HOLD` carriers also hold work that exists nowhere else: the
`subagent-Beardifier…` worktree (96 uncommitted lines in `src/worldgen.rs`),
`t3/2657631b`, `t3/3536d310`, `t3/38771855` (40M template fixture),
`t3/3fa6ff6f` (1503 lines), `t3/5fc39f59`, `t3/ceb3c758`, and
`t3/mc-spine-readiness-review` (792M untracked `target-readiness/` build output).
The exact file lists are in `inventory.json` under `artifacts_only_here`.

## Convergence order (known dependencies only)

What is known now, with the dependency that actually exists. This is not a schedule and not
an authorization: every item still needs its owner's report, and every row is an observation
from the snapshot, so re-read it before acting.

1. **Accept the 5x candidate** `t3/worldgen-integration-audit` (row tip `2799a36b4…`; the
   row moved during the audit, so do not take a commit count from this page). Gate: the
   owning acceptance task reports, and the known Jungle 116→121 regression on the order fix
   is resolved or explicitly held.
2. **Fast-forward canonical** to the accepted candidate. It is 0 behind
   canonical at the snapshot, so no merge commit is needed; re-read before acting.
3. **Retire the duplicate carriers** — only after step 2, and only one carrier at a time.
   The snapshot records three different evidence classes and they are not interchangeable:
   - `ancestor` (the `RETIRE_CANDIDATE` rows): each tip is already reachable from
     canonical, so removing the ref loses no object content;
   - `patch_id_equal`: the carrier's commits have the same patch-id as a commit in
     canonical. That is the same *change*, not the same bytes — patch-id ignores line
     numbers and surrounding context;
   - `same_subject_rebase` / `patch_id_equal_candidate`: the subject matches but the
     patch-id differs (the row lists the bounded drift lines), or the matching commit
     exists only in the candidate, not in canonical.
   Retirement preconditions, all four, re-checked on a fresh read immediately before
   acting: (i) the carrier's content is **accepted in canonical at a fresh SHA**, not
     merely present in the candidate; (ii) its worktree is clean; (iii) `artifacts_only_here`
     is empty for that row; (iv) the tip SHA read then matches the row. A carrier with
     unique untracked bytes — for example `t3/mc-spine-readiness-review`'s 792M
     `target-readiness/` — is not retirable on a commit comparison at all, however many of
     its commits duplicate.
4. **Land the independent slices, in any order** — none of these depends on another:
   - **Track 5B** (`t3/548b811e`): its work is uncommitted, so it cannot be reconstructed
     from any ref. It is unfinished and it does **not** gate the docs/preflight slice.
   - **Contributor docs + preflight + the unified contributor cleanup candidate**
     (documentation, tooling, and one persistence refactor): no worldgen or
     `generation_context` conflict.
5. **Root reform** (`t3/chunk-state-reform`): the recorded tip is a recon/spec record, not
   a current `generation_context` implementation, and its production status is the owning
   task's to report. It is not scheduled here and it is **not** a dependency of items 1–4;
   re-read its row before making any containment claim about it. Not begun.

The earlier draft of this list chained 5B and root reform before the docs slice, and called
the seven carriers above byte-for-byte duplicates. Neither claim was measured: the carriers
are in three different evidence classes, and the two slices are independent.

## Safe local-only batch 1 (needs the manifest approved first)

Remove these 8 worktrees and their branches; each tip is already reachable from
canonical, each worktree is clean, and none has unique untracked files:

| branch | worktree id | tip |
|---|---|---|
| `subagent-Ruined-Portal-Parity-Fixer…` | `subagent-Ruined-…-3d541724-mc-spine` | `9747846` |
| `subagent-Stronghold-Parity-Fixer…` | `subagent-Stronghold-…-65d240ce-mc-spine` | `9747846` |
| `t3/independent-acceptance-review` | `t3-115e673c-…` | `e7ead8e` |
| `t3/28f8bfaf` | `t3-28f8bfaf-…` | `0e0ee5a` |
| `t3/44cdcc09` | `t3-44cdcc09-…` | `924b6cc` |
| `t3/518d2532` | `t3-518d2532-…` | `4d46394` |
| `t3/5a21bfce` | `t3-5a21bfce-…` | `f446b7f` |
| `t3/af28fe5e` | `t3-af28fe5e-…` | `4611f49` |

`inventory.json` → `local_batch[0]` is the authority; it carries the full
worktree ids, and `refresh.py` samples whether each recorded row still matches the live
tree (see *Refresh* for what that does and does not cover).

Do not run `git branch -D`, `git worktree prune`, or any prefix-based deletion.
Re-read `git worktree list` and `git status` immediately before the batch.

## Remote operations that need explicit confirmation

| Target | Detail |
|---|---|
| push `origin/perf/worldgen-parallel` | fast-forward `0d3b4a8 → e7ead8e` (20 commits); no rewrite |
| push after integration | another fast-forward once step 2 lands |
| PR #29, #30 | docs content is absent from canonical, but both target `main`, which is 38 behind canonical: re-target, rebase, or close |
| PR #46, #47 | unique remote-only drafts with red checks; keep as carriers or close |
| PR #26, #28, #49 | dependabot bumps, red checks, base 16 commits behind head; rebase/merge or close |
| remote branch deletion | nothing proposed: every live remote head belongs to an open PR or is `main`/`perf` |

All of these are writes and were **not** performed.

## PR read freshness

Live reads at the stamp in `snapshot.pr_read_utc` (`gh pr view/list`).
No read failed, so no value here is an inferred zero. All 7 open heads match
local remote-tracking objects.

## Drift during this audit (rows are observations, not pins)

- `t3/26c63f09` was renamed to `t3/runtime-source-organization` (same worktree, same HEAD).
- `t3/chunk-state-reform` and `t3/worldgen-integration-audit` gained commits while being read.
- `t3/mc-spine-integration-review` was created at canonical HEAD.
- `t3/mc-spine-contributor-docs` / `-preflight` gained commits and files.
- `t3/bd0038ec` + worktree `t3-bd0038ec-…` appeared at canonical HEAD with no identified owner.

`inventory.json` still carries the older SHA for those `ACTIVE` rows on purpose:
`refresh.py` reports `REF_MOVED_ACTIVE` as a warning so a reader re-checks the
owning task instead of trusting a stale pin.

## After the snapshot (not part of it)

The unified contributor cleanup candidate is `t3/bd0038ec` (worktree
`t3-bd0038ec-…`). It holds, as separate commits, contributor docs (A), the preflight (B)
plus its route/`javac` corrections, the persistence cleanup `01b7323…`, this directory,
and the correction commit that scoped the claims above. It is therefore no longer the
clean, unidentified carrier this snapshot recorded, and nothing here was re-counted for
it. The worldgen 5x candidate `2799a36` and the root-reform spec `5caca0b` are **not**
in that candidate.

## Refresh (informational only)

```sh
python3 evidence/review/repository-convergence-20261116/refresh.py        # sampled row consistency
python3 evidence/review/repository-convergence-20261116/refresh.py --json  # machine-readable
python3 evidence/review/repository-convergence-20261116/refresh_test.py    # negative controls
```

`refresh.py` is a **sampled consistency check of the rows it holds, not an authorization
gate and not a deletion checklist.** `0 fail` means only that every branch and worktree row
it carries still matches the refs and worktrees it happened to read. It does **not** mean a
carrier is safe to delete, because it does not:

- enumerate refs or worktrees that are **not** in the inventory, so a newly created carrier
  is invisible;
- compare dirty **content** (`clean` stays a boolean), so a worktree that is still dirty with
  different bytes looks unchanged;
- treat an unreadable `git status` as a failure — that is a `WARN`, so `0 fail` is possible
  while a row's dirty state is unverified;
- re-derive any disposition, patch-id or containment claim; it compares recorded values.

Before retiring anything, apply the four preconditions in step 3 with a freshly read tip SHA.
`FAIL REF_MOVED` / `WORKTREE_DIRTY_MISMATCH` / `WORKTREE_MISSING` means the recorded row no
longer describes the tree. A `WARN` on an `ACTIVE` row is expected while its task runs.

## Limits

- No build, test, JVM or GPU run: dispositions are Git object/patch judgements.
- Six branches have a same-subject commit in canonical with a **different**
  patch-id; each row lists the bounded drift lines (mutation-check anchors,
  module renames, base-only fields). They are marked `PATCH_EQUIVALENT` with
  that evidence rather than assumed equal because the subject matches.
- Porcelain status is sampled; an owning task can write between the read and a
  later `refresh.py` run.
- No fetch/prune was run; `git ls-remote` established remote freshness instead.
- The audit's own worktree is the one row with `dirty_exempt`: its
  `evidence/review/repository-convergence-20261116/` output is the deliverable,
  not uncommitted work, and the validator ignores exactly that prefix.
- Detailed host paths and the raw command outputs stay outside the repository
  (`/home/yuu/mc-spine-local/audit/git-convergence-20261116/`).
