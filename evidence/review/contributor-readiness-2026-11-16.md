# Contributor readiness integration — 2026-11-16

One local candidate that joins the completed contributor work and one newcomer
route, with the two reported preflight defects fixed. Nothing here is a claim
that a clone is green, that the data/provenance question is settled, or that
the repository is publish-ready.

Branch `t3/bd0038ec`, worktree `t3-bd0038ec-…`, source base
`e7ead8e66466b0b6467450cd3ea31d86a0e39cd3`. The tests and preflight below were
run at `11a57c4f64cc32a7e72faeb052832d2308a0a18e`; the commit that adds this
file changes no source, test or tool.

## Inputs, and what happened to each

| Input | Source | State in this candidate |
|---|---|---|
| Contributor docs A | `t3/mc-spine-contributor-docs` @ `866d79a` (`f2ca0d6`, `866d79a`) | Included, then corrected: tier naming joined to the preflight's, T0 no longer copies a command subset, `tests/README.md` linked as present, one wrong tool path fixed, a stale live count removed from `specs/README.md` |
| Preflight B | `t3/mc-spine-contributor-preflight` @ `3aa4772` (`61d43c4`, `3aa4772`) | Included, then two defects fixed (`tools/dev_check.py`, `tests/dev_check_test.py`) |
| Persistence cleanup | `t3/runtime-source-organization` @ `01b7323` | Included unchanged as its own commit; source `sha256 9bc68c91…` matched the reviewed hash before and after |
| Git/PR convergence inventory | `t3/repository-convergence-plan` @ `8da7405` | Included unchanged as its own commit, then a correction commit scoped four claims it could not support (see `repository-convergence-20261116/README.md`) |
| 5x worldgen candidate `2799a36` | `t3/worldgen-integration-audit` | **Not imported.** Under separate acceptance; needs its own gate and the Jungle 116→121 decision |
| Root-reform spec `5caca0b` | `t3/chunk-state-reform` | **Not imported.** A recon/spec record, not a current `generation_context` implementation |
| Track 5B `t3/548b811e` | its worktree holds 344 uncommitted lines | **Not imported.** Unfinished; nothing here depends on it |

## The two preflight defects and their fixes

`tools/dev_check.py` reports what a checkout can run. Two of its verdicts were
true of the probe rather than of the route:

1. `route rust build/test` was READY whenever `rustup` listed the pinned
   channel — even with an `include_str!`/`include_bytes!` target absent, or
   `rust-toolchain.toml` unreadable. Each route now names the checks it needs
   and is READY only when all of them are OK; otherwise the line names the
   blocking check and its status, and the Rust command is not offered.
   `crate_cache` remains NOT_CHECKED (starting cargo is the only way to
   inspect it) and the Rust route now says so instead of implying a verified
   dependency cache.
2. `jdk` returned OK when `javac -version` exited 0 with empty or unparseable
   output, and when the generator's required major could not be read. The
   version is now matched as `javac <major>` (never the first digits in the
   output); an unparsed version, an unreadable requirement, or a different
   major stays NOT_CHECKED, and the jar/native route stays NOT READY.

Two smaller corrections in the same commit: the module docstring and `run()`
no longer imply the captured output is byte-bounded (the timeout bounds wall
time; the text is captured whole and truncated for display), and a malformed
`MC_SPINE_DEV_CHECK_TIMEOUT_MS` exits 2 — the advertised "preflight could not
run" — instead of raising a traceback that exits 1, where 1 means a repository
fault.

The four `refresh.py` mentions in the inventory README were narrowed the same
way, without changing validator behaviour: it is a sampled check of the rows
it holds, and `0 fail` is not an authorization to delete a carrier.

## Verification

All commands ran in the isolated worktree, in named `systemd-run --user`
scopes (`MemoryMax` as below, `CPUQuota` 100–200%), with a fresh
`resource_gate.py --class light` GO before the two heavier batches. No fetch,
prune, delete, reset, stash, canonical edit or remote action was performed.

| Check | Result |
|---|---|
| `tools/dev_check.py`, real integrated checkout | exit 0; 5 OK / 2 MISSING / 2 NOT_CHECKED / 0 ERROR; `python/fixtures` READY, `rust build/test` READY with the crate-cache caveat, layers NOT READY (no `depsgx`), jar/native NOT READY (jar absent; host `javac 27.0.1.0.3` vs the documented 21) |
| same, from `/` | same `repo:` line and same routes: the script anchors on itself, not on cwd |
| `tools/dev_check.py`, tracked-only export, `unshare -rn` | same report; export still 3553 files afterwards (no write) |
| export contents | 3553 files = 3553 tracked paths; 0 `*.jar`, `*.mca` or `world_overlay.bin` |
| negative control, disposable copy of that export | delete one included asset → `build_inputs MISSING`, `route rust build/test: NOT READY (build_inputs MISSING)`, exit 1; restore it → `130 include sites resolve`, route READY, exit 0 |
| `tests/dev_check_test.py` | 20 tests OK (13 original + 7 new) |
| same suite against the pre-fix `tools/dev_check.py` (`sha256 c7a8ca38…`), private copy | **8 failure entries / 7 new test methods fail**, the 13 original cases pass — the new cases pin the defects, they are not satisfied by the old code |
| `verify.sh` python tier in the tracked-only export | 36 commands, 35 exit 0, `tests/layers_gate_test.py` exit 1 (17 tests, 16 failures, `depsgx` absent); 29 s |
| same tier with `java`/`javac`/`cargo`/`rustc` all exiting 127 | identical per-command return-code pattern, 35 exit 0; 28 s — the tier does not need a JDK or Cargo |
| newcomer route | `dev_check` → `grep -E '^python3 tests/' tests/verify.sh` (36 commands, 1 is `layers_gate_test.py`) → `python3 tests/nextint_oracle_test.py` (`1116` rows asserted, "nextInt oracle guards passed") → `git status` clean and `git diff --check` clean |
| `bash tools/debt_ratchet.sh` | exit 0, 0 over limit |
| focused Rust tests, own `CARGO_TARGET_DIR`, `-j 2 --offline --locked` | build 15.5 s, no error; `tick_persistence::tests` **5 passed**; `persistence_tests::` **13 passed**; test binary `sha256 68f2f194…`; `src/tick_persistence.rs` `sha256 9bc68c91…` |
| `refresh_test.py` | 12 tests OK (8 original + 4 boundary cases that pin what the validator does not cover) |
| relative links and anchors | 11 documents, 136 links checked, 0 missing (no fetch: local files only) |

## NOT RUN (and therefore not claimed)

- `cargo test` **whole suite**, `cargo build` for the server binary, wire and
  adapter stages, `tools/mutation_check.sh`, and `bash tests/verify.sh` as a
  whole. Only the two persistence filters above were executed.
- `--features native-oracle`, the jar-side oracles and captures: no pinned jar
  on this host and no Java 21.
- `tools/vanilla_data/gen_vanilla_data.sh`: needs the pinned jar, Java 21 and
  (for `config_packets.bin`) an explicit EULA acceptance. None was done here.
- `tools/layers_check.sh` with the real inspector: `depsgx` is not installed.
  The failure mode is recorded, not the pass.
- Hosted CI.
- A live `refresh.py` run: it executes `git status` in every worktree of this
  repository, including the ACTIVE carriers and other owners' paths. That was
  kept out of scope; the validator's own suite ran instead.
- Regenerating `tests/data/` fixtures (the doc correction there is
  source-verified, not a re-measurement).

## Remaining onboarding blockers

- The tracked-data/provenance conflict is unresolved (Mojang-derived data and
  the two Pumpkin-identical assets are tracked while the policy documents say
  they are not). Contributors are told not to add, delete or publish; a
  newcomer still cannot tell whether their clone is "allowed" to ship.
- `depsgx` is absent locally, so one T0 command fails by design until the
  pinned inspector is installed.
- The preflight is **not** wired into `tests/verify.sh` or CI; that file and
  the workflow are a separate decision.
- The convergence inventory is a snapshot at `e7ead8e`; canonical can move.
  During this session the canonical checkout briefly showed a different HEAD
  (`0ea0863`, an earlier version of this same inventory) and was back at
  `e7ead8e`, clean, at 14:02. The inventory's own rows are therefore
  observations: re-read them rather than trusting a count.

## Stale text left for its owner (not edited here)

- `tools/vanilla_data/gen_vanilla_data.sh` header: "None of these outputs is
  committed (.gitignore)" — `git ls-files` shows most of them tracked.
- `src/worldgen.rs`: the `VANILLA_DATA_MISSING` doc comment says "a tree that
  has no game data committed next to it"; tracked data exists. Worldgen owner.

## Evidence

Raw logs, the pre-fix tree used for the negative control, the export, the
runner scripts and the private build target stay outside the repository at
`/home/yuu/mc-spine-local/audit/contributor-integration-20261116/`. This page
carries the numbers and the limits; that directory carries the bytes.
