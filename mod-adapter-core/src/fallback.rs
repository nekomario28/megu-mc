// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! 4-tier mod execution classifier and safe fallback planner for unsupported
//! mods (`Strict`, `Quarantine` with Save v2 integrity guard, `DelegateNeoForge`),
//! plus built-in optimization superseding: redundant performance mods are
//! recognized up front and skipped so they can never trigger Tier 3 fallback.

use std::collections::BTreeSet;

use crate::mixin_lifter::{lift_mixin_sites, MixinLiftVerdict, MixinSite};
use crate::version_compat::{check_mod_mc_version, McVersion, VersionCompatVerdict};

/// Execution tier assigned to one discovered mod JAR or datapack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecutionTier {
    /// Tier 0: Flagship dedicated Rust adapter (e.g. `create-adapter`). Zero JVM.
    NativeFlagship,
    /// Tier 1: Pure-data / declarative `ModOffloadProfile` native Rust execution. Zero JVM.
    UniversalNative,
    /// Tier 2: Bounded NeoForge/Fabric JVM shim (S0-S4 + DirectBatch + OFF5).
    BoundedJvmShim,
    /// Tier 3: Requires unsupported Mixin/AT or unknown `net.minecraft.*` APIs.
    RequiresFallback,
    /// Not a JVM tier: an optimization mod whose entire function is already
    /// implemented natively inside `mc-spine` / `client-port` / `client-app`.
    /// Always skipped, never staged, never counted toward Tier 3 fallback.
    ///
    /// Clean-room note: matching a mod id against
    /// [`BUILTIN_SUPERSEDED_PERF_MODS`] only *skips loading* a user-supplied
    /// JAR; it copies, links, bundles, or derives nothing from that mod. Every
    /// built-in optimization is a 100% clean-room original Rust
    /// implementation — no code, shaders, assets, or logic is ported from any
    /// third-party optimization mod (see
    /// `specs/client-fork/ux-and-builtin-optimizations.md`).
    BuiltInSuperseded,
}

/// Mod ids whose optimization surface `mc-spine` already implements natively.
///
/// When a user drops a real CurseForge/Modrinth modpack folder into `mods/`,
/// these redundant performance mods (with their heavy Mixins) would
/// previously force Tier 3 NeoForge fallback. Instead they are classified
/// [`ExecutionTier::BuiltInSuperseded`] *before* ShimCheck/Tier 2/Tier 3
/// checks so their Mixins can never trigger `DelegateToNeoForge` or a
/// `Strict` abort, while real content mods alongside them keep 100% pure
/// native Rust speed.
///
/// The catalog matches both Fabric and NeoForge/Forge mod ids and is
/// compared case-insensitively. It is an id blocklist only: ids here are
/// never loaded, so no third-party mod code is ever copied, linked, bundled,
/// or derived from — every named optimization is an original clean-room Rust
/// implementation inside `mc-spine`.
pub const BUILTIN_SUPERSEDED_PERF_MODS: &[&str] = &[
    // Rendering / mesh / LOD — superseded by mc-spine's native wgpu renderer
    // (client-gpu / client-port), never loaded as Java mods.
    "sodium",
    "embeddium",
    "rubidium",
    "xenon",
    "magnesium",
    "vulkanmod",
    "nvidium",
    "immediatelyfast",
    "entityculling",
    "moreculling",
    "distanthorizons",
    "voxy",
    "bobby",
    "farsight",
    "indium",
    "reeses-sodium-options",
    "sodium-extra",
    "embeddium_extra",
    // Server tick / lighting / memory / startup.
    "lithium",
    "radium",
    "canary",
    "starlight",
    "scalablelux",
    "phosphor",
    "scalpel",
    "ferritecore",
    "modernfix",
    "noisium",
    "c2me",
    "krypton",
    "lazydfu",
    "memoryleakfix",
    "alternate-current",
    "clumps",
    "saturn",
    "smoothboot",
    "fastload",
    "dashloader",
    "servercore",
];

/// Reason attached to [`ExecutionTier::BuiltInSuperseded`] assignments.
/// `tools/mod_launcher.py` mirrors this string so CLI output matches the
/// Rust startup report 1:1.
pub const REASON_SUPERSEDED: &str = "already covered by mc-spine built-in optimizations";

/// Whether `mod_id` names an optimization `mc-spine` already implements
/// natively. Case-insensitive; matches both Fabric and NeoForge/Forge ids.
pub fn is_builtin_superseded_mod(mod_id: &str) -> bool {
    BUILTIN_SUPERSEDED_PERF_MODS
        .iter()
        .any(|id| id.eq_ignore_ascii_case(mod_id))
}

/// Configured server policy when one or more mods fall into Tier 3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackPolicy {
    /// Fail closed before binding TCP, reporting every incompatibility reason.
    Strict,
    /// Run supported Tier 0/1/2 mods natively and quarantine unsupported JARs
    /// (refusing to drop any mod whose namespace already exists in the saved
    /// world unless `force_drop_saved_blocks` is explicitly set).
    Quarantine {
        load_safe_data_only: bool,
        force_drop_saved_blocks: bool,
    },
    /// When >=1 Tier 3 mod is present, hand off automatically to the user's
    /// pinned upstream NeoForge 1.21.1 server runtime so every mod works 100%
    /// normally; when 0 Tier 3 mods are present, run pure `mc-spine` native
    /// with zero NeoForge overhead.
    DelegateNeoForge,
}

/// Audit summary of a discovered mod (fed by `tools/shimcheck.py` / bootstrap).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredMod {
    pub mod_id: String,
    pub has_java_bytecode: bool,
    pub shimcheck_compatible: bool,
    pub has_unsupported_mixin_or_at: bool,
    pub rejection_reasons: Vec<String>,
    /// Optional declared Minecraft compatibility range in either loader
    /// syntax — NeoForge/Forge Maven interval (`"[1.21.1,1.22)"`, `"1.21.1"`)
    /// or Fabric SemVer (`">=1.21.1 <1.21.5"`, `"~1.21.1"`, `"1.21.x"`) — read
    /// from `neoforge.mods.toml`/`fabric.mod.json`/pack metadata. `None` means
    /// the mod declares nothing, so [`classify_mod_for_version`] applies no
    /// version gate to it.
    pub mc_version_range: Option<String>,
}

/// Per-mod decision inside the computed startup plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModAssignment {
    pub mod_id: String,
    pub tier: ExecutionTier,
    pub reasons: Vec<String>,
}

/// Final startup execution plan produced before opening TCP listeners.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupPlan {
    /// All mods run natively on `mc-spine` (with optional bounded JVM shim if
    /// any Tier 2 mod is present); zero upstream NeoForge runtime started.
    RunNative {
        assignments: Vec<ModAssignment>,
        needs_bounded_jvm: bool,
        quarantined_mods: Vec<ModAssignment>,
        load_quarantined_data_only: bool,
    },
    /// One or more Tier 3 mods triggered automatic fallback to upstream
    /// NeoForge 1.21.1 runtime so all mods function with full compatibility.
    DelegateToNeoForge {
        triggering_mods: Vec<ModAssignment>,
        all_assignments: Vec<ModAssignment>,
    },
    /// Startup aborted safely with explicit diagnostics (either `Strict` mode
    /// or `Quarantine` refusing to drop a mod present in existing world save).
    RefuseStartup {
        reason: String,
        blocking_mods: Vec<ModAssignment>,
        /// Perf mods skipped as [`ExecutionTier::BuiltInSuperseded`] before
        /// the refusal was decided; kept for a complete audit report.
        superseded_perf_mods: Vec<ModAssignment>,
    },
}

/// The shared Tier 0/1/2/3 classification chain *below* the superseded
/// catalog: flagship adapters first, then zero-JVM offload profiles and pure
/// datapacks, then the bounded JVM shim, then Tier 3 fallback.
fn classify_execution_path(
    m: &DiscoveredMod,
    flagship_mod_ids: &BTreeSet<String>,
    profile_mod_ids: &BTreeSet<String>,
) -> (ExecutionTier, Vec<String>) {
    if flagship_mod_ids.contains(&m.mod_id) {
        (
            ExecutionTier::NativeFlagship,
            vec!["handled by flagship native Rust adapter".to_string()],
        )
    } else if !m.has_java_bytecode || profile_mod_ids.contains(&m.mod_id) {
        (
            ExecutionTier::UniversalNative,
            vec!["handled by zero-JVM universal adapter / offload profile".to_string()],
        )
    } else if m.shimcheck_compatible && !m.has_unsupported_mixin_or_at {
        (
            ExecutionTier::BoundedJvmShim,
            vec!["verified compatible with bounded S0-S4 JVM shim".to_string()],
        )
    } else {
        let mut reasons = m.rejection_reasons.clone();
        if reasons.is_empty() {
            reasons.push("unsupported Mixin/AT or Minecraft API surface".to_string());
        }
        (ExecutionTier::RequiresFallback, reasons)
    }
}

/// Classify one discovered mod with no Minecraft version gate — equivalent to
/// [`classify_mod_for_version`] on a mod whose `mc_version_range` is `None`.
pub fn classify_mod(
    m: &DiscoveredMod,
    flagship_mod_ids: &BTreeSet<String>,
    profile_mod_ids: &BTreeSet<String>,
) -> (ExecutionTier, Vec<String>) {
    if is_builtin_superseded_mod(&m.mod_id) {
        // Checked before flagship/ShimCheck/Tier 3: a superseded perf
        // mod's Mixins must never reach the fallback cascade.
        (
            ExecutionTier::BuiltInSuperseded,
            vec![REASON_SUPERSEDED.to_string()],
        )
    } else {
        classify_execution_path(m, flagship_mod_ids, profile_mod_ids)
    }
}

/// Classify one discovered mod for a server running Minecraft `target`.
///
/// [`BUILTIN_SUPERSEDED_PERF_MODS`] is still checked *first*, so a redundant
/// performance mod built for another version (e.g. `sodium` declaring
/// `1.20.1`) stays a clean [`ExecutionTier::BuiltInSuperseded`] skip with
/// zero fallback noise. For every other mod, a declared `mc_version_range`
/// that fails [`check_mod_mc_version`] for `target` classifies
/// [`ExecutionTier::RequiresFallback`] with a plain-language reason —
/// preventing a mod built for another Minecraft release from silently
/// executing broken native code — and an unparseable range fails closed the
/// same way.
pub fn classify_mod_for_version(
    m: &DiscoveredMod,
    flagship_mod_ids: &BTreeSet<String>,
    profile_mod_ids: &BTreeSet<String>,
    target: McVersion,
) -> (ExecutionTier, Vec<String>) {
    if is_builtin_superseded_mod(&m.mod_id) {
        return (
            ExecutionTier::BuiltInSuperseded,
            vec![REASON_SUPERSEDED.to_string()],
        );
    }
    if let Some(declared) = m.mc_version_range.as_deref() {
        match check_mod_mc_version(target, declared) {
            VersionCompatVerdict::Compatible => {}
            VersionCompatVerdict::InvalidRange { declared, reason } => {
                return (
                    ExecutionTier::RequiresFallback,
                    vec![format!(
                        "declares an invalid Minecraft version range '{declared}' \
                         ({reason}); cannot confirm it supports server {target}"
                    )],
                );
            }
            VersionCompatVerdict::MinorMismatch { declared, .. }
            | VersionCompatVerdict::MajorMismatch { declared, .. } => {
                return (
                    ExecutionTier::RequiresFallback,
                    vec![format!(
                        "built for Minecraft {declared}, server is running {target}"
                    )],
                );
            }
        }
    }
    classify_execution_path(m, flagship_mod_ids, profile_mod_ids)
}

/// Whether `reason` is a Mixin-attributed rejection code. Known codes include
/// `UNSUPPORTED_MIXIN_INJECTION`, `UNSUPPORTED_MIXIN_AT`,
/// `UNSUPPORTED_MIXIN_CONFIG`, `UNSUPPORTED_RUNTIME_MIXIN`,
/// `MISSING_MIXIN_CONFIG`, and the shimcheck `MIXIN_DETECTED`/`MIXIN_INJECT`
/// family; non-mixin blockers (`CUSTOM_CHUNK_GENERATOR`,
/// `MC_VERSION_INCOMPATIBLE`, `MC_VERSION_RANGE_INVALID`,
/// `UNSCANNABLE_JAVA_MOD`, `PACK_FORMAT_INCOMPATIBLE`, ...) never match.
fn is_mixin_reason(reason: &str) -> bool {
    reason.to_ascii_uppercase().contains("MIXIN")
}

/// Mixin-aware variant of [`classify_mod_for_version`]: identical ordering —
/// [`BUILTIN_SUPERSEDED_PERF_MODS`] first, the [`McVersion`] range gate
/// second, the shared flagship/profile/ShimCheck/Tier-3 chain third — plus a
/// final clean-room promotion step powered by
/// [`crate::mixin_lifter::lift_mixin_sites`].
///
/// When a mod lands in [`ExecutionTier::RequiresFallback`] *only* because of
/// Mixin reason codes (every returned reason contains `MIXIN`; a mod with no
/// recorded reasons cannot prove mixin-only cause and never promotes), all
/// of its extracted [`MixinSite`]s are lifted. If every site binds to a
/// [`crate::mixin_lifter::StandardEventHook`] and at least one binding
/// exists, the mod is promoted to [`ExecutionTier::BoundedJvmShim`] with a
/// diagnostic naming the lifted hooks. A single unliftable site
/// (`@Redirect`, `locals` capture, `@At("INVOKE")`, cancellable post hook,
/// unknown target, ...) or any non-mixin rejection keeps it firmly in Tier 3
/// with the exact offending site and reason.
///
/// Returns `(tier, diagnostic, verdict)`: `verdict` is `Some` only when the
/// lifter actually ran — `None` for superseded mods, version-gate refusals,
/// non-Tier-3 classifications, and Tier-3 verdicts with non-mixin or
/// unrecorded blockers.
pub fn classify_mod_with_mixins(
    m: &DiscoveredMod,
    mixin_sites: &[MixinSite],
    flagship_mod_ids: &BTreeSet<String>,
    profile_mod_ids: &BTreeSet<String>,
    target: McVersion,
) -> (ExecutionTier, String, Option<MixinLiftVerdict>) {
    let (tier, reasons) = classify_mod_for_version(m, flagship_mod_ids, profile_mod_ids, target);
    if tier != ExecutionTier::RequiresFallback {
        return (tier, reasons.join("; "), None);
    }
    // Promotion requires proof the Tier 3 verdict is mixin-only:
    // - `m.rejection_reasons` non-empty: the shared chain synthesizes a
    //   generic "unsupported Mixin/AT or Minecraft API surface" reason when
    //   the audit recorded nothing, which does not prove mixin-only cause.
    // - Every *returned* reason names a mixin: version-gate refusals and
    //   other non-mixin blockers never contain "MIXIN", so they stay
    //   fail-closed Tier 3 automatically.
    let mixin_only_blockers =
        !m.rejection_reasons.is_empty() && reasons.iter().all(|r| is_mixin_reason(r));
    if !mixin_only_blockers {
        return (tier, reasons.join("; "), None);
    }

    let verdict = lift_mixin_sites(mixin_sites);
    match &verdict {
        MixinLiftVerdict::FullyLiftable(bindings) if !bindings.is_empty() => {
            let mut hooks: Vec<&'static str> = bindings.iter().map(|b| b.hook.as_str()).collect();
            hooks.sort_unstable();
            hooks.dedup();
            (
                ExecutionTier::BoundedJvmShim,
                format!(
                    "promoted from Tier 3 to bounded JVM shim: {} mixin site(s) lifted to standard event hooks ({})",
                    bindings.len(),
                    hooks.join(", ")
                ),
                Some(verdict),
            )
        }
        MixinLiftVerdict::FullyLiftable(_) => (
            ExecutionTier::RequiresFallback,
            format!(
                "{}; mixin reason codes found but no mixin sites supplied for lifting",
                reasons.join("; ")
            ),
            Some(verdict),
        ),
        MixinLiftVerdict::RequiresFallback { .. } => (
            ExecutionTier::RequiresFallback,
            format!(
                "{}; mixin lift incomplete: {}",
                reasons.join("; "),
                verdict.format_lift_summary()
            ),
            Some(verdict),
        ),
    }
}

/// Classify discovered mods against registered flagship adapters, declarative
/// offload profiles, existing saved-world namespaces, and the chosen fallback
/// policy.
///
/// Mods that set [`DiscoveredMod::mc_version_range`] are additionally gated
/// against [`McVersion::CURRENT`] (the Minecraft version this runtime
/// implements); use [`plan_mod_startup_for_version`] to plan against a
/// different target. Mods declaring no range are classified exactly as
/// before.
pub fn plan_mod_startup(
    mods: &[DiscoveredMod],
    flagship_mod_ids: &BTreeSet<String>,
    profile_mod_ids: &BTreeSet<String>,
    saved_world_namespaces: &BTreeSet<String>,
    policy: FallbackPolicy,
) -> StartupPlan {
    plan_mod_startup_for_version(
        mods,
        flagship_mod_ids,
        profile_mod_ids,
        saved_world_namespaces,
        policy,
        McVersion::CURRENT,
    )
}

/// Version-aware variant of [`plan_mod_startup`]: identical planning and
/// policy semantics, but each mod's declared `mc_version_range` is checked
/// against `target` via [`classify_mod_for_version`] (after the
/// [`BUILTIN_SUPERSEDED_PERF_MODS`] skip, before every other tier check).
pub fn plan_mod_startup_for_version(
    mods: &[DiscoveredMod],
    flagship_mod_ids: &BTreeSet<String>,
    profile_mod_ids: &BTreeSet<String>,
    saved_world_namespaces: &BTreeSet<String>,
    policy: FallbackPolicy,
    target: McVersion,
) -> StartupPlan {
    let mut assignments = Vec::with_capacity(mods.len());
    let mut tier3 = Vec::new();
    let mut superseded = Vec::new();
    let mut needs_bounded_jvm = false;

    for m in mods {
        let (tier, reasons) =
            classify_mod_for_version(m, flagship_mod_ids, profile_mod_ids, target);
        if tier == ExecutionTier::BoundedJvmShim {
            needs_bounded_jvm = true;
        }
        let assignment = ModAssignment {
            mod_id: m.mod_id.clone(),
            tier,
            reasons,
        };
        if tier == ExecutionTier::RequiresFallback {
            tier3.push(assignment.clone());
        } else if tier == ExecutionTier::BuiltInSuperseded {
            superseded.push(assignment.clone());
        }
        assignments.push(assignment);
    }

    if tier3.is_empty() {
        return StartupPlan::RunNative {
            assignments,
            needs_bounded_jvm,
            quarantined_mods: Vec::new(),
            load_quarantined_data_only: false,
        };
    }

    match policy {
        FallbackPolicy::Strict => StartupPlan::RefuseStartup {
            reason: "strict policy rejected unsupported Tier 3 mod(s)".to_string(),
            blocking_mods: tier3,
            superseded_perf_mods: superseded,
        },
        FallbackPolicy::Quarantine {
            load_safe_data_only,
            force_drop_saved_blocks,
        } => {
            if !force_drop_saved_blocks {
                let endangered: Vec<ModAssignment> = tier3
                    .iter()
                    .filter(|a| saved_world_namespaces.contains(&a.mod_id))
                    .cloned()
                    .collect();
                if !endangered.is_empty() {
                    return StartupPlan::RefuseStartup {
                        reason: "refusing to quarantine mod(s) whose blocks exist in the saved world; switch to delegate fallback or set force_drop_saved_blocks=true".to_string(),
                        blocking_mods: endangered,
                        superseded_perf_mods: superseded,
                    };
                }
            }
            let active = assignments
                .into_iter()
                .filter(|a| a.tier != ExecutionTier::RequiresFallback)
                .collect();
            StartupPlan::RunNative {
                assignments: active,
                needs_bounded_jvm,
                quarantined_mods: tier3,
                load_quarantined_data_only: load_safe_data_only,
            }
        }
        FallbackPolicy::DelegateNeoForge => StartupPlan::DelegateToNeoForge {
            triggering_mods: tier3,
            all_assignments: assignments,
        },
    }
}

impl StartupPlan {
    /// Sorted mod ids skipped as [`ExecutionTier::BuiltInSuperseded`].
    pub fn superseded_perf_mods(&self) -> Vec<String> {
        let assignments: &[ModAssignment] = match self {
            StartupPlan::RunNative { assignments, .. } => assignments,
            StartupPlan::DelegateToNeoForge {
                all_assignments, ..
            } => all_assignments,
            StartupPlan::RefuseStartup {
                superseded_perf_mods,
                ..
            } => superseded_perf_mods,
        };
        let mut ids: Vec<String> = assignments
            .iter()
            .filter(|a| a.tier == ExecutionTier::BuiltInSuperseded)
            .map(|a| a.mod_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// One-line operator notice for skipped perf mods, e.g.
    /// `Skipped 3 redundant performance mods (already built into mc-spine): lithium, modernfix, sodium`.
    /// `None` when nothing was superseded.
    pub fn superseded_notice(&self) -> Option<String> {
        let ids = self.superseded_perf_mods();
        if ids.is_empty() {
            return None;
        }
        Some(format!(
            "Skipped {} redundant performance mods (already built into mc-spine): {}",
            ids.len(),
            ids.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn zero_unsupported_mods_always_runs_pure_native_even_under_delegate_policy() {
        let mods = vec![
            DiscoveredMod {
                mod_id: "create".into(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: true,
                rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".into()],
                mc_version_range: None,
            },
            DiscoveredMod {
                mod_id: "twilight_datapack".into(),
                has_java_bytecode: false,
                shimcheck_compatible: true,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec![],
                mc_version_range: None,
            },
        ];
        let plan = plan_mod_startup(
            &mods,
            &set(&["create"]),
            &set(&[]),
            &set(&["create"]),
            FallbackPolicy::DelegateNeoForge,
        );
        assert!(matches!(
            plan,
            StartupPlan::RunNative {
                needs_bounded_jvm: false,
                ..
            }
        ));
    }

    #[test]
    fn unsupported_mixin_mod_delegates_cleanly_and_quarantine_protects_saved_world() {
        let mods = vec![
            DiscoveredMod {
                mod_id: "create".into(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: true,
                rejection_reasons: vec![],
                mc_version_range: None,
            },
            DiscoveredMod {
                mod_id: "heavy_mixin_mod".into(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: true,
                rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".into()],
                mc_version_range: None,
            },
        ];

        // DelegateNeoForge hands off cleanly with exact trigger info.
        let delegated = plan_mod_startup(
            &mods,
            &set(&["create"]),
            &set(&[]),
            &set(&["heavy_mixin_mod"]),
            FallbackPolicy::DelegateNeoForge,
        );
        match delegated {
            StartupPlan::DelegateToNeoForge {
                triggering_mods, ..
            } => {
                assert_eq!(triggering_mods.len(), 1);
                assert_eq!(triggering_mods[0].mod_id, "heavy_mixin_mod");
            }
            other => panic!("expected DelegateToNeoForge, got {other:?}"),
        }

        // Quarantine refuses to silently drop a mod whose blocks exist in the saved world.
        let protected = plan_mod_startup(
            &mods,
            &set(&["create"]),
            &set(&[]),
            &set(&["heavy_mixin_mod"]),
            FallbackPolicy::Quarantine {
                load_safe_data_only: true,
                force_drop_saved_blocks: false,
            },
        );
        assert!(matches!(protected, StartupPlan::RefuseStartup { .. }));

        // Quarantine succeeds when the unsupported mod has no saved blocks in the world.
        let isolated = plan_mod_startup(
            &mods,
            &set(&["create"]),
            &set(&[]),
            &set(&["create"]),
            FallbackPolicy::Quarantine {
                load_safe_data_only: true,
                force_drop_saved_blocks: false,
            },
        );
        match isolated {
            StartupPlan::RunNative {
                assignments,
                quarantined_mods,
                load_quarantined_data_only,
                ..
            } => {
                assert_eq!(assignments.len(), 1);
                assert_eq!(quarantined_mods.len(), 1);
                assert!(load_quarantined_data_only);
            }
            other => panic!("expected RunNative with quarantine, got {other:?}"),
        }
    }

    #[test]
    fn builtin_perf_mods_are_superseded_never_fallback() {
        // A real modpack folder: content mods + perf mods whose heavy Mixins
        // would previously force Tier 3. The perf mods are skipped and the
        // pack runs 100% pure native with zero NeoForge fallback.
        let heavy = |mod_id: &str| DiscoveredMod {
            mod_id: mod_id.into(),
            has_java_bytecode: true,
            shimcheck_compatible: false,
            has_unsupported_mixin_or_at: true,
            rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".into()],
            mc_version_range: None,
        };
        let mods = vec![
            heavy("create"),
            DiscoveredMod {
                mod_id: "aether_datapack".into(),
                has_java_bytecode: false,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec![],
                mc_version_range: None,
            },
            heavy("sodium"),
            heavy("lithium"),
            heavy("modernfix"),
        ];

        for policy in [
            FallbackPolicy::DelegateNeoForge,
            FallbackPolicy::Strict,
            FallbackPolicy::Quarantine {
                load_safe_data_only: false,
                force_drop_saved_blocks: false,
            },
        ] {
            let plan = plan_mod_startup(
                &mods,
                &set(&["create"]),
                &set(&[]),
                &set(&["create"]),
                policy,
            );
            assert_eq!(
                plan.superseded_perf_mods(),
                vec!["lithium", "modernfix", "sodium"],
                "policy {policy:?}"
            );
            let notice = plan.superseded_notice().unwrap();
            assert_eq!(
                notice,
                "Skipped 3 redundant performance mods (already built into mc-spine): \
                 lithium, modernfix, sodium"
            );
            match plan {
                StartupPlan::RunNative {
                    assignments,
                    needs_bounded_jvm,
                    ..
                } => {
                    assert!(!needs_bounded_jvm);
                    let tiers: Vec<(String, ExecutionTier)> = assignments
                        .iter()
                        .map(|a| (a.mod_id.clone(), a.tier))
                        .collect();
                    assert_eq!(
                        tiers,
                        vec![
                            ("create".to_string(), ExecutionTier::NativeFlagship),
                            ("aether_datapack".to_string(), ExecutionTier::UniversalNative),
                            ("sodium".to_string(), ExecutionTier::BuiltInSuperseded),
                            ("lithium".to_string(), ExecutionTier::BuiltInSuperseded),
                            ("modernfix".to_string(), ExecutionTier::BuiltInSuperseded),
                        ]
                    );
                    assert!(assignments
                        .iter()
                        .all(|a| a.tier != ExecutionTier::RequiresFallback));
                }
                // Even Strict must not abort on superseded perf mods.
                other => panic!("expected RunNative for policy {policy:?}, got {other:?}"),
            }
        }

        // Matching is case-insensitive and covers both loader ids.
        assert!(is_builtin_superseded_mod("Sodium"));
        assert!(is_builtin_superseded_mod("entityculling"));
        assert!(is_builtin_superseded_mod("distanthorizons"));
        assert!(is_builtin_superseded_mod("c2me"));
        assert!(is_builtin_superseded_mod("alternate-current"));
        assert!(!is_builtin_superseded_mod("create"));
        assert!(!is_builtin_superseded_mod("ae2"));
    }

    fn versioned_mod(mod_id: &str, mc_version_range: Option<&str>) -> DiscoveredMod {
        DiscoveredMod {
            mod_id: mod_id.into(),
            has_java_bytecode: true,
            shimcheck_compatible: false,
            has_unsupported_mixin_or_at: true,
            rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".into()],
            mc_version_range: mc_version_range.map(str::to_string),
        }
    }

    #[test]
    fn superseded_perf_mod_skips_version_check_entirely() {
        // `sodium` built only for 1.20.1 on a 1.21.1 server: the superseded
        // catalog wins before the version gate, so it is still a clean
        // BuiltInSuperseded skip — never a version-mismatch Tier 3 report.
        let sodium = versioned_mod("sodium", Some("[1.20.1,1.20.2)"));
        let (tier, reasons) =
            classify_mod_for_version(&sodium, &set(&[]), &set(&[]), McVersion::V1_21_1);
        assert_eq!(tier, ExecutionTier::BuiltInSuperseded);
        assert_eq!(reasons, vec![REASON_SUPERSEDED.to_string()]);
    }

    #[test]
    fn incompatible_declared_range_is_tier3_with_plain_language_reason() {
        // A real content mod declaring a 1.20.1-only Maven range on a 1.21.1
        // server must never silently execute broken native code.
        let jei = versioned_mod("jei", Some("[1.20.1,1.20.2)"));
        let (tier, reasons) =
            classify_mod_for_version(&jei, &set(&[]), &set(&[]), McVersion::V1_21_1);
        assert_eq!(tier, ExecutionTier::RequiresFallback);
        assert_eq!(
            reasons,
            vec!["built for Minecraft [1.20.1,1.20.2), server is running 1.21.1".to_string()]
        );
    }

    #[test]
    fn compatible_declared_range_runs_on_every_target_in_band() {
        let m = versioned_mod("somecontent", Some("[1.21.1,1.21.5)"));
        for target in [McVersion::V1_21_1, McVersion::V1_21_3, McVersion::V1_21_4] {
            // Mixin-incompatible with no profile: falls through to Tier 3
            // *only* on technical grounds — the version gate stays silent.
            let (_, reasons) = classify_mod_for_version(&m, &set(&[]), &set(&[]), target);
            assert_eq!(
                reasons,
                vec!["UNSUPPORTED_MIXIN_INJECTION".to_string()],
                "target {target}"
            );
        }
        // A profile-backed mod stays Tier 1 on every in-band target.
        for target in [McVersion::V1_21_1, McVersion::V1_21_4] {
            let (tier, _) = classify_mod_for_version(&m, &set(&[]), &set(&["somecontent"]), target);
            assert_eq!(tier, ExecutionTier::UniversalNative, "target {target}");
        }
    }

    #[test]
    fn invalid_declared_range_fails_closed_as_tier3() {
        let m = versioned_mod("broken_manifest", Some("not-a-version-range!!"));
        let (tier, reasons) =
            classify_mod_for_version(&m, &set(&[]), &set(&[]), McVersion::V1_21_1);
        assert_eq!(tier, ExecutionTier::RequiresFallback);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].starts_with("declares an invalid Minecraft version range"));
        assert!(reasons[0].contains("not-a-version-range!!"));
    }

    #[test]
    fn plan_mod_startup_for_version_flags_mismatch_per_policy() {
        let mods = vec![
            versioned_mod("jei", Some("[1.20.1,1.20.2)")),
            versioned_mod("sodium", Some("[1.20.1,1.20.2)")),
            versioned_mod("okmod", Some("[1.21.1,1.21.5)")),
        ];
        // Under delegate, only `jei` triggers; sodium is still superseded.
        let plan = plan_mod_startup_for_version(
            &mods,
            &set(&[]),
            &set(&["okmod"]),
            &set(&[]),
            FallbackPolicy::DelegateNeoForge,
            McVersion::V1_21_1,
        );
        match plan {
            StartupPlan::DelegateToNeoForge {
                triggering_mods,
                all_assignments,
            } => {
                assert_eq!(triggering_mods.len(), 1);
                assert_eq!(triggering_mods[0].mod_id, "jei");
                assert_eq!(
                    triggering_mods[0].reasons,
                    vec![
                        "built for Minecraft [1.20.1,1.20.2), server is running 1.21.1".to_string()
                    ]
                );
                let tiers: Vec<(String, ExecutionTier)> = all_assignments
                    .iter()
                    .map(|a| (a.mod_id.clone(), a.tier))
                    .collect();
                assert_eq!(
                    tiers,
                    vec![
                        ("jei".to_string(), ExecutionTier::RequiresFallback),
                        ("sodium".to_string(), ExecutionTier::BuiltInSuperseded),
                        ("okmod".to_string(), ExecutionTier::UniversalNative),
                    ]
                );
            }
            other => panic!("expected DelegateToNeoForge, got {other:?}"),
        }
        // On a 1.21.4 target every declared range still holds -> pure native.
        let plan = plan_mod_startup_for_version(
            &mods,
            &set(&[]),
            &set(&["okmod"]),
            &set(&[]),
            FallbackPolicy::DelegateNeoForge,
            McVersion::V1_21_4,
        );
        assert!(matches!(plan, StartupPlan::DelegateToNeoForge { .. }));
        // `jei`'s 1.20.1 range mismatches on 1.21.4 too; drop it and plan again.
        let mods = &mods[1..];
        let plan = plan_mod_startup_for_version(
            mods,
            &set(&[]),
            &set(&["okmod"]),
            &set(&[]),
            FallbackPolicy::DelegateNeoForge,
            McVersion::V1_21_4,
        );
        assert!(matches!(plan, StartupPlan::RunNative { .. }));
        // Strict refuses cleanly with the version reason in the audit trail.
        let plan = plan_mod_startup_for_version(
            &[versioned_mod("jei", Some("[1.20.1,1.20.2)"))],
            &set(&[]),
            &set(&[]),
            &set(&[]),
            FallbackPolicy::Strict,
            McVersion::V1_21_1,
        );
        match plan {
            StartupPlan::RefuseStartup { blocking_mods, .. } => {
                assert_eq!(blocking_mods[0].mod_id, "jei");
                assert!(blocking_mods[0].reasons[0].contains("server is running 1.21.1"));
            }
            other => panic!("expected RefuseStartup, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // classify_mod_with_mixins: Tier 3 -> Tier 2 promotion via the
    // clean-room Mixin-to-Event lifter.
    // ------------------------------------------------------------------

    use crate::mixin_lifter::{
        InjectPoint, MixinAnnotationKind, StandardEventHook, UnliftableReason,
    };

    fn mixin_site(target_class: &str, target_method: &str, at: InjectPoint) -> MixinSite {
        MixinSite {
            mixin_class: "com.example.mixin.StandardMixin".into(),
            target_class: target_class.into(),
            target_method: target_method.into(),
            annotation: MixinAnnotationKind::Inject,
            at: Some(at),
            cancellable: false,
            captures_locals: false,
        }
    }

    fn mixin_only_mod(mod_id: &str) -> DiscoveredMod {
        DiscoveredMod {
            mod_id: mod_id.into(),
            has_java_bytecode: true,
            shimcheck_compatible: false,
            has_unsupported_mixin_or_at: true,
            rejection_reasons: vec![
                "UNSUPPORTED_MIXIN_INJECTION".into(),
                "MIXIN_DETECTED".into(),
            ],
            mc_version_range: None,
        }
    }

    #[test]
    fn liftable_inject_only_mod_promotes_tier3_to_tier2() {
        // A mod whose whole unsupported surface is standard @Inject(HEAD)
        // damage/block-break hooks promotes cleanly, naming the lifted hooks.
        let sites = vec![
            mixin_site(
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
                InjectPoint::Head,
            ),
            mixin_site(
                "net.minecraft.server.level.ServerPlayerGameMode",
                "destroyBlock",
                InjectPoint::Head,
            ),
        ];
        let (tier, diagnostic, verdict) = classify_mod_with_mixins(
            &mixin_only_mod("damage_indicator"),
            &sites,
            &set(&[]),
            &set(&[]),
            McVersion::V1_21_1,
        );
        assert_eq!(tier, ExecutionTier::BoundedJvmShim);
        assert!(diagnostic.contains("EntityDamagePre"), "{diagnostic}");
        assert!(diagnostic.contains("BlockBreakPre"), "{diagnostic}");
        match verdict {
            Some(MixinLiftVerdict::FullyLiftable(bindings)) => {
                assert_eq!(bindings.len(), 2);
                assert_eq!(bindings[0].hook, StandardEventHook::EntityDamagePre);
                assert_eq!(bindings[1].hook, StandardEventHook::BlockBreakPre);
            }
            other => panic!("expected fully liftable verdict, got {other:?}"),
        }
    }

    #[test]
    fn one_unliftable_site_keeps_whole_mod_tier3() {
        // One good @Inject plus one @Redirect: the mod stays Tier 3 and the
        // diagnostic names the exact unliftable site and reason.
        let mut redirect = mixin_site(
            "net/minecraft/server/level/ServerLevel",
            "tick",
            InjectPoint::Head,
        );
        redirect.annotation = MixinAnnotationKind::Redirect;
        let sites = vec![
            mixin_site(
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
                InjectPoint::Head,
            ),
            redirect,
        ];
        let (tier, diagnostic, verdict) = classify_mod_with_mixins(
            &mixin_only_mod("mixed_mod"),
            &sites,
            &set(&[]),
            &set(&[]),
            McVersion::V1_21_1,
        );
        assert_eq!(tier, ExecutionTier::RequiresFallback);
        assert!(diagnostic.contains("UnsupportedAnnotation"), "{diagnostic}");
        assert!(diagnostic.contains("ServerLevel#tick"), "{diagnostic}");
        match verdict {
            Some(MixinLiftVerdict::RequiresFallback { lifted, unliftable }) => {
                assert_eq!(lifted.len(), 1);
                assert_eq!(unliftable.len(), 1);
                assert_eq!(unliftable[0].1, UnliftableReason::UnsupportedAnnotation);
            }
            other => panic!("expected RequiresFallback verdict, got {other:?}"),
        }
    }

    #[test]
    fn non_mixin_reason_codes_block_promotion() {
        // CUSTOM_CHUNK_GENERATOR is not a mixin code: a liftable @Inject on
        // top changes nothing — the mod stays Tier 3 and the lifter never
        // even runs.
        let mut m = mixin_only_mod("chunky_gen");
        m.rejection_reasons = vec![
            "CUSTOM_CHUNK_GENERATOR".into(),
            "UNSUPPORTED_MIXIN_INJECTION".into(),
        ];
        let sites = vec![mixin_site(
            "net/minecraft/world/entity/LivingEntity",
            "hurt",
            InjectPoint::Head,
        )];
        let (tier, diagnostic, verdict) =
            classify_mod_with_mixins(&m, &sites, &set(&[]), &set(&[]), McVersion::V1_21_1);
        assert_eq!(tier, ExecutionTier::RequiresFallback);
        assert_eq!(
            diagnostic,
            "CUSTOM_CHUNK_GENERATOR; UNSUPPORTED_MIXIN_INJECTION"
        );
        assert!(verdict.is_none());
    }

    #[test]
    fn superseded_perf_mod_never_reaches_the_lifter() {
        // `sodium` is skipped as BuiltInSuperseded before the version gate
        // and before any mixin analysis, even with liftable-looking sites.
        let sodium = versioned_mod("sodium", Some("[1.21.1,1.22)"));
        let sites = vec![mixin_site(
            "net/minecraft/world/entity/LivingEntity",
            "hurt",
            InjectPoint::Head,
        )];
        let (tier, diagnostic, verdict) =
            classify_mod_with_mixins(&sodium, &sites, &set(&[]), &set(&[]), McVersion::V1_21_1);
        assert_eq!(tier, ExecutionTier::BuiltInSuperseded);
        assert_eq!(diagnostic, REASON_SUPERSEDED);
        assert!(verdict.is_none());
    }

    #[test]
    fn version_gate_beats_mixin_promotion() {
        // A mod built for another Minecraft version stays Tier 3 even when
        // its mixin sites are perfectly liftable.
        let m = versioned_mod("liftable_but_old", Some("[1.20.1,1.20.2)"));
        let sites = vec![mixin_site(
            "net/minecraft/world/entity/LivingEntity",
            "hurt",
            InjectPoint::Head,
        )];
        let (tier, diagnostic, verdict) =
            classify_mod_with_mixins(&m, &sites, &set(&[]), &set(&[]), McVersion::V1_21_1);
        assert_eq!(tier, ExecutionTier::RequiresFallback);
        assert!(
            diagnostic.contains("server is running 1.21.1"),
            "{diagnostic}"
        );
        assert!(verdict.is_none());
    }

    #[test]
    fn mixin_reasons_without_sites_do_not_promote() {
        // Mixin-only codes with zero supplied sites cannot prove a fully
        // lifted binding set — stay Tier 3.
        let (tier, diagnostic, verdict) = classify_mod_with_mixins(
            &mixin_only_mod("no_sites"),
            &[],
            &set(&[]),
            &set(&[]),
            McVersion::V1_21_1,
        );
        assert_eq!(tier, ExecutionTier::RequiresFallback);
        assert!(
            diagnostic.contains("no mixin sites supplied"),
            "{diagnostic}"
        );
        assert_eq!(verdict, Some(MixinLiftVerdict::FullyLiftable(vec![])));
    }

    #[test]
    fn non_tier3_tiers_are_unaffected_by_sites() {
        // Tier 0/1/2 classifications return before the lift step: flagship
        // and profile mods keep their tiers and report no verdict.
        let sites = vec![mixin_site(
            "net/minecraft/world/entity/LivingEntity",
            "hurt",
            InjectPoint::Head,
        )];
        let m = mixin_only_mod("create");
        let (tier, _, verdict) =
            classify_mod_with_mixins(&m, &sites, &set(&["create"]), &set(&[]), McVersion::V1_21_1);
        assert_eq!(tier, ExecutionTier::NativeFlagship);
        assert!(verdict.is_none());

        let m = mixin_only_mod("profiled");
        let (tier, _, verdict) = classify_mod_with_mixins(
            &m,
            &sites,
            &set(&[]),
            &set(&["profiled"]),
            McVersion::V1_21_1,
        );
        assert_eq!(tier, ExecutionTier::UniversalNative);
        assert!(verdict.is_none());
    }
}
