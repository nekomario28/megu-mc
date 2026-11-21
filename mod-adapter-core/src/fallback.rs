// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! 4-tier mod execution classifier and safe fallback planner for unsupported
//! mods (`Strict`, `Quarantine` with Save v2 integrity guard, `DelegateNeoForge`).

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
    let mut needs_bounded_jvm = false;

    for m in mods {
        let (tier, reasons) = if flagship_mod_ids.contains(&m.mod_id) {
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
}
