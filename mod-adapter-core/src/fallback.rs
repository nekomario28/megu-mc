// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! 4-tier mod execution classifier and safe fallback planner for unsupported
//! mods (`Strict`, `Quarantine` with Save v2 integrity guard, `DelegateNeoForge`),
//! plus built-in optimization superseding: redundant performance mods are
//! recognized up front and skipped so they can never trigger Tier 3 fallback.

use std::collections::BTreeSet;

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
    // Rendering / mesh / LOD.
    "sodium",
    "embeddium",
    "rubidium",
    "xenon",
    "magnesium",
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

/// Classify discovered mods against registered flagship adapters, declarative
/// offload profiles, existing saved-world namespaces, and the chosen fallback
/// policy.
pub fn plan_mod_startup(
    mods: &[DiscoveredMod],
    flagship_mod_ids: &BTreeSet<String>,
    profile_mod_ids: &BTreeSet<String>,
    saved_world_namespaces: &BTreeSet<String>,
    policy: FallbackPolicy,
) -> StartupPlan {
    let mut assignments = Vec::with_capacity(mods.len());
    let mut tier3 = Vec::new();
    let mut superseded = Vec::new();
    let mut needs_bounded_jvm = false;

    for m in mods {
        let (tier, reasons) = if is_builtin_superseded_mod(&m.mod_id) {
            // Checked before flagship/ShimCheck/Tier 3: a superseded perf
            // mod's Mixins must never reach the fallback cascade.
            (
                ExecutionTier::BuiltInSuperseded,
                vec![REASON_SUPERSEDED.to_string()],
            )
        } else if flagship_mod_ids.contains(&m.mod_id) {
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
            needs_bounded_jvm = true;
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
        };

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
            },
            DiscoveredMod {
                mod_id: "twilight_datapack".into(),
                has_java_bytecode: false,
                shimcheck_compatible: true,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec![],
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
            },
            DiscoveredMod {
                mod_id: "heavy_mixin_mod".into(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: true,
                rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".into()],
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
        };
        let mods = vec![
            heavy("create"),
            DiscoveredMod {
                mod_id: "aether_datapack".into(),
                has_java_bytecode: false,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec![],
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
}
