# Alpha4 convergence plan — 2026-11-25

Status: **draft persistent plan / not yet a completion claim**.

This plan is a control-plane reset for converging the current alpha4 audit and compatibility work into a releasable, evidence-honest candidate. It does not replace `specs/ROADMAP.md`, `docs/release-checklist.md`, or the audit ledgers; it orders their execution.


## Current integration status — refreshed 2026-11-25

The control-plane state above is historical. The active local integration branch is now
`alpha4/integration-alpha4-convergence-local` at the current local HEAD (inspect `git rev-parse HEAD`; prior hard-coded SHAs in this file are historical),
27 commits ahead of `origin/alpha4/integration-alpha4-convergence` and not yet pushed.

Release-blocking lanes already integrated on this line include inventory conservation, non-vacuous A04 Anvil
transaction tests, clippy warning elimination to the ratchet baseline, broad A08 UI recovery with explicitly held
limitations, wiring-audit scanner correction with documented staged boundaries, installer/runtime-data generation,
complete runtime-pack inventory in `tools/dev_check.py`, and release artifact/documentation shape correction.
The installer generates the canonical datapack plus the complete block-registry runtime asset pack. A fresh client
install probe at `/home/yuu/.cache/megu-client-install-final.ELI6Tq/mc` exited 0. Using that installed runtime root,
a direct `megu-client` quickplay loopback run exited 0, decoded/loaded 9 chunks, rendered a nonzero PNG, and the child
server logged `[wg] vanilla datapack worldgen active (seed 0)` rather than the flat-generator fallback. The client E2E
also fails if that fallback line appears.

This is still not release authority. Same-SHA static/unit/export/bundle/live gates are being rerun on the final head;
no tag, push, release artifact publication, or external comment has occurred.

## Evidence baseline refreshed for this plan

Live evidence below was checked locally on 2026-11-25. It supersedes stale chat/handoff text.

- Live `origin/main`: `63d098e0ed74a7bba4e6d49978c54700e821fc79` (`Merge alpha4/compat-p1-entity-fluid-collision into main`).
- Local repair branch `alpha4/fix-client-kernel-dimid` = `3d7e30b1c` (`fix(client-kernel): keep dimension lava scale out of physics projection`), based on `63d098e0e`; it is local-only and not yet merged/pushed. Validation on that exact head: `cargo test --manifest-path client-kernel/Cargo.toml --locked` PASS, `cargo clippy --manifest-path client-kernel/Cargo.toml --all-targets --locked -- -D warnings` PASS, `cargo check --bin megu --locked` PASS, `bash tools/debt_ratchet.sh` PASS (`0 over limit`), and generated `physics.rs` no longer contains `DimId`, `dim_ultrawarm`, or `lava_motion_scale_for_dim`.
- Local integration branch `alpha4/integration-alpha4-convergence-local` = `cff205939`, local-only and not pushed. It is based on repair commit `3d7e30b1c` and merges A06/A07, A05, P0 canSurvive, Aero, and A12. Partial affected-surface validation before the heavy root compile was deliberately stopped: client-kernel tests PASS; core `world_view` 5 PASS; mod-adapter-core `capability_bus` 11 PASS; create-adapter `aeronautics` 27 PASS and `contraption` 10 PASS; clippy-ratchet tests 4 PASS; website tests 33 PASS; `website/check_site.py` PASS; layers PASS; debt ratchet PASS; root `support_shape` 5 PASS; root feature `aeronautics` 7 PASS. This is not release authority: root feature `tick_queue` did not finish, and the clean worktree's root `update_shape` filter ran 0 tests without the generated registry environment, so it is not evidence.
- **Authoritative first blocker before that repair:** latest visible `main` CI is red.
  - Rust regression run `45545864773`: failed.
  - Cross-platform run `45545864703`: failed on Ubuntu, macOS, and Windows.
  - First concrete failure in both: `client-kernel` standalone compilation fails with `E0425` in generated/included `physics.rs`: `DimId` is not in scope at line 808 and `dim_ultrawarm` is not found in the crate root at line 809.
  - Secret scan and dependency audit passed on the same head.
  - Therefore the first task is not more feature work: repair `origin/main` standalone compilation and prove the same crate locally before integrating lanes.
- Tags already published include `v0.1.0-alpha.1`, `v0.1.0-alpha.2`, and `v0.1.0-alpha.3`. The next release identifier is expected to be `v0.1.0-alpha.4`, but the version bump/tag is a later publication decision, not current authority.
- Local integration candidate `cb3b8a09f8268c93333556a3c1078fed7f494e8c` (`Merge alpha4/compat-p0-aero-contraption-cap into main`) exists locally and contains `4281a80665235d891e3077ef645a6c98a2bf3d18`.
  - Partial validation already recorded for that candidate in `/tmp/m20-verify-logs`:
    - `bash tools/layers_check.sh`: PASS.
    - `bash tools/debt_ratchet.sh`: `0 over limit`.
    - `cargo test --manifest-path core/Cargo.toml --locked -- world_view`: 5 passed / 0 failed in the matching suite.
    - `cargo test --manifest-path mod-adapter-core/Cargo.toml --locked -- capability`: 11 passed / 0 failed in matching suites.
    - `cargo test --manifest-path create-adapter/Cargo.toml --locked -- aeronautics contraption`: 37 passed / 0 failed in matching suites.
    - `cargo test --bin megu --features mod-suite,create-offload --locked -- aeronautics tick_queue`: rerun completed; 21 passed / 0 failed in matching suites. This is still affected-surface evidence only, not release authority.
  - This is only affected-surface evidence, not release authority.
- Branch state relative to `origin/main`:
  - `alpha4/audit-a04-persistence-crash-recovery` = `841007235`; ancestor of `origin/main`; no unique commits. Its delegated worker hit a provider rate limit, so A04 remains unstarted/unproven rather than complete.
  - `alpha4/audit-a05-inventory-crafting-diff` = `8c7664ef6`; 1 ahead / 0 behind; unique commit adds inventory/crafting conservation tests. The worker reported production gaps in `src/play_protocol.rs` but did not edit that owning file because its lane boundary forbade it. Those production gaps remain open follow-up work.
  - `alpha4/audit-a08-ui-layout-state-replay` = `63d098e0e`; same as `origin/main`; no unique commits. The active worktree `/home/yuu/.t3/worktrees/mc-spine/t3-7ed4eca0-2bed04781413cd98` contains uncommitted UI layout changes plus a 314-line test file; its full new test file passes 3/3 locally, but it is not committed or integrated. Its earlier stale draft is preserved at `target/a08-worktree-recovery/ui_visual_and_interaction_audit_tests.rs`.
  - `alpha4/audit-a06-a07-meshpool-clippy` = `008f0ffb8`; 2 ahead / 2 behind; unique files: `client-gpu/src/mesh_pool.rs`, `tests/clippy_ratchet_test.py`, `tools/clippy_ratchet.sh`. Worker-reported validation: client-gpu clippy PASS, mesh-pool tests 13/13 PASS, clippy ratchet tests 4/4 PASS, SPDX PASS, debt ratchet PASS; full clippy ratchet remains red because measured warnings are 47 vs baseline 39 on both old and new script, so this is a pre-existing baseline breach rather than lane regression.
  - `alpha4/compat-p0-cansurvive-oracle` = `217380749`; 1 ahead / 0 behind; unique files: `src/block_registry.rs`, `src/template_structures.rs`. Worker-reported validation used `MEGU_VERSION_DIR=/home/yuu/mc-spine/assets`: matching survive/support/shape tests 37 passed / 0 failed, root cargo check PASS, SPDX PASS, strict defect audit PASS, layers PASS, debt ratchet PASS. This also means generated-registry availability is part of the evidence boundary for those tests.
  - `alpha4/audit-a12-website-release-sync` = `7966cd144`; 1 ahead / 0 behind; unique files: README and website release/crash-report sync files.
  - `alpha4/compat-p0-aero-contraption-cap` = `4281a8066`; 1 ahead / 11 behind; unique files: `core/src/lib.rs`, `core/src/world_view.rs`, `create-adapter/src/aeronautics.rs`, `create-adapter/src/contraption_adapter.rs`, `mod-adapter-core/src/capability_bus.rs`, `src/aeronautics_and_dim_bridge.rs`, `src/tick_queue.rs`.
  - The unique file sets of A06/A07, P0 canSurvive, A12, and Aero currently have no direct path overlap.
- `/home/yuu/mc-spine` root checkout is not a clean integration base: local `main` is `30a96c94363ad2acef1c88601c93685d31eb5fde`, an ancestor of `origin/main`, with 38 tracked modified files, 3 tracked deleted files, and 13 untracked audit/spec entries. None of the tracked modified working files are byte-identical to `origin/main`. Those changes are **unclassified evidence/work**, not disposable noise.
- The A08 lane was previously blocked by stale worktree branch ownership. The stale checkout has since been detached and the draft preserved, but the process defect class remains: **lane branch/worktree ownership is not preflighted**.
- Dependabot PRs `#1` through `#9` are open. They are not part of the first blocker and must not distract from main repair/convergence.

## Planning decisions proposed for approval

1. **Main repair before convergence:** do not integrate feature lanes onto a red `origin/main`. First repair the `client-kernel` standalone compile failure and prove it on a clean local worktree.
2. **Scope decision:** ship a verified-scope alpha4 candidate, not a claim of full Minecraft/mod compatibility. Public wording must match only evidence collected on the exact release SHA.
3. **Convergence before expansion:** finish or explicitly hold the existing audit/compatibility branches before opening new implementation lanes. More parallelism is allowed only inside non-overlapping ownership boundaries.
4. **Clean-base integration:** use fresh worktrees based on the repaired `origin/main` for integration. Do not use the dirty/stale root checkout as an integration base until its foreign changes are classified and the user authorizes cleanup.
5. **Publication hold:** pushing, tags, PR mutation, releases, package publishing, external comments, and deployment require explicit per-request user instruction plus the required publish guard environment. Local integration and validation are not publication approval.
6. **Mod compatibility boundary:** Rust owns tick/world/save. Java/JVM paths are optional compatibility lanes; when no Java listener or supported mod path is active, the normal Rust path must make zero JVM calls. Full FML/ModLauncher/arbitrary JAR support remains unproven unless a real independent gate runs it.
7. **Guard consolidation:** handle the A08 worktree-collision class inside an existing developer preflight/check mechanism rather than adding a new standalone process layer. The guard must fail on a stale owned branch and must preserve untracked work before any detach/cleanup.

## Delegation contract for Codex `gpt-6-luna` and other child agents

The user explicitly allows heavier use of Codex `gpt-6-luna`. Use it aggressively, but only under bounded contracts.

1. Every child prompt must include this exact destructive-command prohibition:
   - Do not use `rm`, `rm -rf`, `git clean`, `git reset --hard`, branch deletion, force-push, or any destructive filesystem/git command.
   - Do not use cleanup traps such as `trap 'rm -rf ...' EXIT`. Use a stable cache directory, or leave a uniquely named temporary directory in place and report it.
   - If cleanup appears necessary, stop and report the exact paths and proposed non-destructive preservation step.
2. Do not put children into approval-required mode for ordinary bounded work: approval loops are process waste. Read-only review/planning lanes must instead be explicitly instructed to avoid any command that would need approval or mutate state. Implementation lanes use a fresh worktree, never the dirty root checkout.
3. One lane owns one explicit file set. No two active implementation lanes may own overlapping files or generated outputs.
4. Every child must report commands actually run, exact head, tests passed/failed/not run, and whether claims are verified or inferred.
5. Children never push, tag, open/update PRs, comment externally, delete branches, clean worktrees, install dependencies, or run destructive commands.
6. The parent owns integration. A child success report is input evidence, not integration authority.


## Codex Luna planning review outcomes — applied corrections

Three independent read-only Luna reviews converged on the same corrections. These are planning constraints, not new release evidence:

- `alpha4/integration-main` exists and is only an older A12-only partial integration head (`56e0ac347` merging `7966cd144`); it is not a competing complete integration authority. The newer local convergence head is `cff205939`.
- Live worktree inventory is much larger than the initial lane list implied: 402 worktrees, 379 attached, 2 prunable; about 100 attached alpha4 worktrees across 97 distinct alpha4 branches were reported by the Luna parallelism review. Three duplicate branch attachments were reported for A04, Aero, and A06/A07. No detach/cleanup is authorized.
- Evidence receipts must record exact SHA, clean/dirty state, full command, exit code, and log path. Filtered suites are affected-surface evidence only. Zero matched tests are not coverage.
- Required protected release checks for the final exact SHA are `gitleaks`, `cargo-deny`, `rust`, `mutation`, and `xplat` on Ubuntu, macOS, and Windows. Same-SHA evidence does not transfer across rebases or merge recreations.
- The branch/worktree ownership guard should extend existing `tools/dev_check.py` and `tests/dev_check_test.py`, not add a new process layer. It must be read-only, report duplicate/conflicting branch attachments and dirty ownership state, and hold lane allocation rather than detach or clean anything.

## Phase -1 — repair `origin/main` first

Goal: make the current authoritative head compile again before merging anything else.

1. Create a fresh local worktree from `origin/main` `63d098e0e` without touching the dirty root checkout.
2. Reproduce the first failure with the smallest command:
   - `cargo test --manifest-path client-kernel/Cargo.toml --locked`
   - if compilation is enough to reproduce, use the matching `cargo check`/`cargo clippy` command from the CI standalone-crate loop rather than widening blindly.
3. Diagnose the correct ownership seam before editing:
   - either the client-kernel include adapter must expose the dimension types/functions used by shared physics, or shared physics must avoid server-only dimension symbols behind a proper abstraction;
   - do not weaken tests, delete assertions, or hide the code merely to silence compilation.
4. Run the affected standalone crate gate, then the same standalone-crate loop shape CI uses.
5. Add the cheapest guard that would have caught this class before merge: exact-head standalone compile/check in the existing pre-merge/dev-check path. Do not add a new process layer.
6. Only after the repair is locally green, treat that repair head as the new integration base.

Exit condition: `client-kernel` and the standalone crate loop pass locally on the exact repair head; no speculative downstream fixes remain unexamined.

## Phase 0 — control-plane convergence

Goal: recover one trustworthy local integration head and stop lane collision/waste before adding more work.

1. Finish validation of the Aero integration candidate. Because `cb3b8a09f` is based on the currently broken pre-repair head, rebase or recreate that merge onto the repaired main base before treating it as integration material.
2. Affected-surface validation for Aero, split into single-purpose commands so zero-match filters cannot masquerade as coverage:
   - `bash tools/layers_check.sh`
   - `bash tools/debt_ratchet.sh`
   - `cargo test --manifest-path core/Cargo.toml --locked -- world_view`
   - `cargo test --manifest-path mod-adapter-core/Cargo.toml --locked -- capability_bus`
   - `cargo test --manifest-path create-adapter/Cargo.toml --locked -- aeronautics`
   - `cargo test --manifest-path create-adapter/Cargo.toml --locked -- contraption`
   - `cargo test --bin megu --features mod-suite,create-offload --locked -- aeronautics`
   - `cargo test --bin megu --features mod-suite,create-offload --locked -- tick_queue`
3. Integrate completed branches in an order that minimizes behavioral risk, using fresh local merge/rebase worktrees:
   1. main compile repair;
   2. `alpha4/audit-a06-a07-meshpool-clippy` after rebasing/merging its 2 unique commits and validating mesh-pool plus clippy-ratchet behavior;
   3. `alpha4/compat-p0-cansurvive-oracle` after validating `update_shape` and `support_shape` tests;
   4. Aero after the validation ladder above;
   5. `alpha4/audit-a12-website-release-sync` after `node website/test_functions.mjs`, `python3 website/check_site.py`, and any existing release-workflow tests pass.
   Reorder only when fresh file-overlap or failure evidence says otherwise.
4. Treat A04 as **held/unstarted**: its worker hit a provider rate limit and produced no verified lane work. Treat A05 as test-only integrated input, not as inventory correctness proof: its production gaps in `src/play_protocol.rs` remain a required inventory-conservation implementation lane before release. Treat A08 as **held**: the active worktree has uncommitted layout/test work, but no branch commits currently exist to integrate.
5. Quarantine the root checkout's unknown work without deleting it: capture exact `git diff`, untracked file list, hashes, and timestamps; classify each hunk as already-upstream, duplicate, valuable lane work, or obsolete. No reset/clean/deletion without explicit user approval.
6. Inventory existing worktrees and branch ownership before spawning further implementation lanes. Integrate that check into the existing dev-check/preflight path; do not create a separate standalone process layer.
7. Produce the first **local integration head**. Do not push it until explicit publication approval exists.

Exit condition: one clean local integration head; no unknown active lane state; root divergence classified; no branch-collision class left unguarded.

## Phase 1 — close release-relevant defect clusters in independent lanes

Start only after Phase 0, with at most one owner per shared hot file. Recommended lanes:

| Lane | Owned cluster | Representative open issues | Acceptance boundary |
|---|---|---|---|
| S1 server correctness/parity | ticket/scheduler, structure residue, spawn comparisons, support/shape oracle | M35, M39-M42, active P0 oracle | Vanilla-oracle mismatches in the owned cases go red before fix and green after; save/reload included where applicable |
| S2 server parallelism/perf | connection model, serial `game_tick`, global mutex contention, mob-tick clones | H7, H9-H11, L24-L25 | Same behavior under regression fixtures plus representative tick/CPU/RSS evidence; no perf-only claim without equal-work measurement |
| S3 client render/memory | mesh-pool scaling, mesh residency, CPU Hi-Z fallback, render hot paths | H17, H19-H21 | Real counters prove changed path executed; memory/draw-call evidence on the same client build |
| S4 mod-compat honesty | JNI lifecycle, registry/event ordering, Mixin rejection, FML boundary, real modpack matrix | M27-M31, M38 and external JNI/FML review | Unsupported contracts fail closed; supported cases run through real JARs; docs say exactly what is and is not proven |
| S5 release/CI hygiene | default features, artifact diet, Docker/runtime dependencies, clippy and missing local inspectors | H3/H4 decision, H61-H62, L8/L12/L15/L22 | CI/local gates fail on the fixed defect class; release artifacts match documented scope |
| S7 installer/runtime data | release bundle, `megu-installer`, bootstrapper, vanilla-data generation | alpha3 fresh install reached the GUI but singleplayer failed with missing `config_packets.bin`/`assets`/`datapacks`; the alpha3 bundle also left the server runtime root unresolvable from `<gameDir>/bin` | A fresh OS profile can install, launch, and create an integrated singleplayer world using only documented artifacts. The solution must stay clean-room compliant: either ship a legally vetted runtime-data artifact or automatically obtain/generate it from the user's official 1.21.1 installation with explicit EULA/consent handling; no silent half-install |
| S6 protocol/feature matrix | remaining packet/menu/component/feature rows | M43-M44 | Matrix rows are backed by live behavior, not registry-name presence; unproven rows stay `NEEDS_VERIFY` |

Non-next work during Phase 1: cosmetic refactors, additional optimization kernels without a measured consumer, new mod abstraction layers, broad feature expansion not tied to a release claim, and dependabot batching unless it directly unblocks the release gate.

## Phase 2 — same-SHA release-candidate gates

All release claims must be re-proven on one exact candidate SHA. A green lane report is not release evidence.

1. Static and unit gates:
   - `bash tests/verify.sh`; its artifact-preserving trap is non-destructive in the current tree
   - standalone crate clippy loop from `docs/release-checklist.md`
   - `python3 tools/wiring_audit.py --strict --tolerated-unwired tools/wiring_audit_unwired.json` (the canonical gate used by `tests/verify.sh`; plain `--strict` intentionally reports the documented staged unwired modules)
   - `python3 tools/vanilla_parity_audit.py --format text --strict --min-c2s-handled 58 --min-gamerules-wired 51`
   - `python3 tools/modloader_parity_audit.py --format text --strict --min-overall-pct 100`
   - `python3 tools/provenance_check.py --self-test`
   - `bash tools/debt_ratchet.sh`
   - `bash tools/layers_check.sh`
2. Clean-room export and build:
   - `bash tools/export_public_branch.sh --out ../mc-spine-public --verify`
   - exported-tree `python3 tools/provenance_check.py --strict-public`
   - exported-tree `cargo check --bins --offline --locked`
3. Bundle/container:
   - deterministic release bundle build and `verify`
   - Docker image build, EULA refusal checks, and runtime smoke
   - fresh-profile installer smoke must prove the integrated-singleplayer runtime-data path end to end: profile registration, server binary discovery, runtime-root discovery, `config_packets.bin`, `datapacks/vanilla`, the complete `assets/*_1211` block-registry pack (including `blocks_1211.json`, the binary physical-fact tables, and `feature_order_1211.json`) availability, vanilla-datapack worldgen activation, and world creation. This is a release blocker because alpha3 installed successfully but could not create a world on a fresh laptop
   - artifact hashes and mtimes recorded with the exact candidate SHA
4. Live behavior:
   - fresh automated alpha smoke, not only historical recorded results
   - vanilla 1.21.1 client smoke; 26.3/777 only if server support is actually implemented and independently proven
   - equal-work interleaved comparison and 20 TPS / tick-budget proof if performance is claimed
   - real third-party modpack E2E if any third-party mod compatibility is claimed
5. Documentation honesty: README, release checklist, mod-author guide, website, and issue templates must be regenerated or checked against the same SHA. Unproven items remain explicit limitations.

Exit condition: release-candidate decision is either `GO` with exact evidence links or `HOLD_AT_GATE` with the first failing gate and next safe action.

## Phase 3 — publication/distribution, only after explicit approval

1. Push the exact integration/release branch and let hosted CI prove provider/runner behavior.
2. Confirm the next version identifier publicly before use; expected `v0.1.0-alpha.4`, but do not bump/tag by assumption.
3. Tag and publish only after the same-SHA gates are green.
4. Publish release assets: server bundle, bootstrapper JAR, Prism zip, Modrinth pack, Docker image, SBOM, checksums, and license texts.
5. Deploy/verify the website and crash relay only with secrets provisioned through approved secret channels; no token in chat, Git, or logs.
6. External comments, PR actions, package-manager submissions, and release notes are separate publication actions requiring explicit instruction.

## Phase 4 — post-alpha expansion backlog

These are valuable but should not block a verified-scope alpha unless the user chooses otherwise:

- broad `MECH`/`ADV`/`RCP`/`TAG`/`REG` feature-matrix completion;
- full arbitrary FML/ModLauncher/JAR execution and broader Mixin injection families;
- automatic real-mod graph offload into `mod-offload`/`create-adapter`;
- protocol translation beyond directly implemented 767/777 surfaces;
- deeper multi-region parallel tick architecture and complete client-render scaling roadmap;
- dependabot update triage after release blockers are closed.

## Alpha04 release critical path — current minimum plan

Assumption: alpha04 is a **verified-scope alpha**, not a claim of full vanilla or mod compatibility. The current canonical local integration line is the branch HEAD; it is not pushed and not release-approved.

1. **Freeze and finish the integration candidate**
   - Treat `cff205939` as the candidate assembly point unless a fresh upstream `origin/main` change invalidates it.
   - Finish only the missing affected validation on that exact head, especially feature `tick_queue` and generated-registry-dependent canSurvive/updateShape tests with `MEGU_VERSION_DIR` explicitly recorded.
   - If any lane changes, reassemble/recheck the new exact SHA; do not transfer evidence across SHAs.

2. **Close hard release blockers before broad gates**
   - Inventory/crafting conservation implementation in `src/play_protocol.rs`: offhand button 40, crafting-output remainders, shift-click crafting results, and container-close overflow must be fixed with non-vacuous tests.
   - Pre-existing clippy debt: measured 47 warnings vs baseline 39. Fix warnings; do not raise the baseline merely to obtain green.
   - Branch/worktree ownership preflight inside existing `tools/dev_check.py` / `tests/dev_check_test.py`; duplicate attached branches must hold lane allocation.
   - Root checkout classification: preserve and classify all unique/untracked work before any cleanup; no deletion without explicit approval.

3. **Decide held audit scope explicitly**
   - A04 persistence/crash recovery is currently unstarted. Either implement it as a release blocker or explicitly defer it with a written limitation.
   - A08 UI layout/state replay is uncommitted but locally test-passing. Either commit/integrate it through a fresh owned lane or explicitly defer it with a written limitation.
   - A05 tests alone are not proof; inventory production gaps above remain the release-relevant work.

4. **Run same-SHA candidate gates**
   - Use the exact final candidate SHA only: static/unit gates, standalone crate tests/clippy, wiring/parity/modloader/provenance/debt/layers checks, clean public export/build, deterministic bundle, container smoke, live alpha smoke, and documentation sync.
   - Record every receipt with SHA, clean/dirty state, full command, exit code, and log/artifact path.
   - Claims not proven on that SHA must be removed or marked unsupported.

5. **Publication only after explicit approval**
   - Push branch, open/update PR, tag `v0.1.0-alpha.4`, publish assets, deploy website/crash relay, or comment externally only after explicit user instruction and publish guard authorization.
   - Required protected checks on the exact release SHA: `gitleaks`, `cargo-deny`, `rust`, `mutation`, and `xplat` Ubuntu/macOS/Windows.

Current first safe action: finish the same-SHA release-candidate gate inventory on the active final integration head, integrate any first-failure fixes, then rebuild/verify artifacts and update release documentation before publication.

## Reopen / hold conditions

- Hold at Phase -1 until `origin/main` standalone compilation is repaired and locally proven.
- Hold at Phase 0 if the root checkout's unclassified changes may contain unique user work and cleanup is not approved.
- Hold any merge at the first concrete failing gate; do not speculative-fix downstream cascade noise.
- Hold performance claims until equal-work evidence exists on the exact candidate SHA.
- Hold mod-compatibility claims until real third-party JAR evidence exists; static self-audit scores are not mod compatibility proof.
- Reopen branch hygiene if any lane is blocked by branch ownership again after the preflight guard lands.

## Immediate next safe action

Finish the same-SHA release-candidate gate inventory on the active integration head. Do not push, publish, clean the root checkout, treat filtered/zero-match tests as release evidence, or publish while the installer/worldgen blocker lacks exact-head end-to-end evidence.
