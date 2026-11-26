// SPDX-License-Identifier: LGPL-3.0-or-later

//! Synchronous plugin lifecycle at the host's callback boundary. A
//! candidate's VM and pending KV writes stay private through validation
//! and initialization. The active VM is replaced only after startup succeeds.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::abi::CURRENT_CONTRACT_REV;
use crate::wasm_mod::{CallContext, Guest};
use crate::{inspect_archive, CapabilitySet, MeguHost, ModConfig, ModFault};

pub use crate::epoch::EpochGuard;
pub use crate::kv::{
    decode_kv_snapshot, encode_kv_snapshot, KvError, PluginKvLimits, PluginKvStore,
    TransactionalKvStore,
};

/// Host policy for a candidate. For raw WASM, the supplied capabilities
/// are its explicit grants. For an archive, the supervisor grants the
/// intersection of the host policy and the archive manifest.
#[derive(Clone, Debug)]
pub struct PluginConfig {
    pub runtime: ModConfig,
    pub capabilities: CapabilitySet,
    /// Highest internal host contract revision permitted for this plugin.
    /// The selected revision is derived from the binary's imports.
    pub max_contract_rev: u32,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            runtime: ModConfig::default(),
            capabilities: CapabilitySet::NONE,
            max_contract_rev: CURRENT_CONTRACT_REV,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReloadReceipt {
    pub previous_generation: Option<u64>,
    pub generation: u64,
    pub contract_rev: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReloadError {
    AlreadyLoaded,
    NotLoaded,
    NoPriorHealthyGeneration,
    GenerationExhausted,
    StartupStatus(i32),
    Fault(ModFault),
    Snapshot(KvError),
}

impl fmt::Display for ReloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyLoaded => write!(f, "plugin already loaded"),
            Self::NotLoaded => write!(f, "plugin is not loaded"),
            Self::NoPriorHealthyGeneration => write!(f, "no prior healthy plugin generation"),
            Self::GenerationExhausted => write!(f, "plugin generation counter exhausted"),
            Self::StartupStatus(status) => write!(f, "plugin startup returned status {status}"),
            Self::Fault(fault) => write!(f, "{fault}"),
            Self::Snapshot(error) => write!(f, "{error}"),
        }
    }
}

impl Error for ReloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Fault(fault) => Some(fault),
            Self::Snapshot(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ModFault> for ReloadError {
    fn from(fault: ModFault) -> Self {
        Self::Fault(fault)
    }
}

#[derive(Debug, PartialEq)]
pub struct PluginTickOutcome {
    pub mod_id: String,
    pub generation: u64,
    pub result: Result<i32, ModFault>,
}

#[derive(Debug, Default, PartialEq)]
pub struct SupervisorTickReport {
    /// One outcome per attempted tick, in mod ID order. Plugins already
    /// quarantined when the tick started are omitted.
    pub outcomes: Vec<PluginTickOutcome>,
}

struct Generation {
    number: u64,
    guest: Guest,
}

impl Generation {
    fn receipt(&self, previous_generation: Option<u64>) -> ReloadReceipt {
        ReloadReceipt {
            previous_generation,
            generation: self.number,
            contract_rev: self.guest.contract_rev(),
        }
    }
}

struct PluginSlot {
    active: Generation,
    inactive: Option<Generation>,
    highest_generation: u64,
    kv: PluginKvStore,
    epoch: EpochGuard,
}

/// Hosts a deterministic set of WASM plugins at the tick boundary.
///
/// Startup permits KV access and host reads. It suppresses candidate logs
/// and traps any attempt to mutate the live world, chat, or UI. The
/// existing live host is used for ticks after the candidate is published.
pub struct PluginSupervisor {
    plugins: BTreeMap<String, PluginSlot>,
    kv_limits: PluginKvLimits,
}

impl Default for PluginSupervisor {
    fn default() -> Self {
        Self::new(PluginKvLimits::default())
    }
}

impl PluginSupervisor {
    pub fn new(kv_limits: PluginKvLimits) -> Self {
        Self {
            plugins: BTreeMap::new(),
            kv_limits,
        }
    }

    pub fn load_plugin(
        &mut self,
        mod_id: &str,
        wasm_bytes: &[u8],
        config: PluginConfig,
        host: &mut dyn MeguHost,
    ) -> Result<ReloadReceipt, ReloadError> {
        if self.plugins.contains_key(mod_id) {
            return Err(ReloadError::AlreadyLoaded);
        }
        let mut kv = PluginKvStore::new(self.kv_limits);
        let epoch = EpochGuard::default();
        let guest = stage(wasm_bytes, config, host, &mut kv, &epoch)?;
        let active = Generation { number: 1, guest };
        let receipt = active.receipt(None);
        self.plugins.insert(
            mod_id.to_owned(),
            PluginSlot {
                active,
                inactive: None,
                highest_generation: 1,
                kv,
                epoch,
            },
        );
        Ok(receipt)
    }

    pub fn load_plugin_archive(
        &mut self,
        mod_id: &str,
        archive: &[u8],
        mut config: PluginConfig,
        host: &mut dyn MeguHost,
    ) -> Result<ReloadReceipt, ReloadError> {
        let payload = inspect_archive(archive, &config.runtime).map_err(ModFault::from)?;
        config.capabilities = config.capabilities.intersection(payload.capabilities);
        self.load_plugin(mod_id, &payload.wasm, config, host)
    }

    pub fn hot_reload_plugin(
        &mut self,
        mod_id: &str,
        new_wasm_bytes: &[u8],
        new_config: PluginConfig,
        host: &mut dyn MeguHost,
    ) -> Result<ReloadReceipt, ReloadError> {
        let slot = self.plugins.get_mut(mod_id).ok_or(ReloadError::NotLoaded)?;
        let number = slot
            .highest_generation
            .checked_add(1)
            .ok_or(ReloadError::GenerationExhausted)?;
        let guest = stage(new_wasm_bytes, new_config, host, &mut slot.kv, &slot.epoch)?;

        let next = Generation { number, guest };
        let receipt = next.receipt(Some(slot.active.number));
        let previous = std::mem::replace(&mut slot.active, next);
        if previous.guest.quarantined().is_none() {
            slot.inactive = Some(previous);
        }
        slot.highest_generation = number;
        Ok(receipt)
    }

    pub fn hot_reload_plugin_archive(
        &mut self,
        mod_id: &str,
        archive: &[u8],
        mut config: PluginConfig,
        host: &mut dyn MeguHost,
    ) -> Result<ReloadReceipt, ReloadError> {
        let payload = inspect_archive(archive, &config.runtime).map_err(ModFault::from)?;
        config.capabilities = config.capabilities.intersection(payload.capabilities);
        self.hot_reload_plugin(mod_id, &payload.wasm, config, host)
    }

    /// Restore the prior healthy VM by swapping the two generation
    /// slots. The plugin keeps all committed KV, including writes from
    /// the generation being rolled back.
    pub fn rollback_plugin(&mut self, mod_id: &str) -> Result<ReloadReceipt, ReloadError> {
        let slot = self.plugins.get_mut(mod_id).ok_or(ReloadError::NotLoaded)?;
        let previous = slot
            .inactive
            .as_mut()
            .filter(|previous| previous.number < slot.active.number)
            .ok_or(ReloadError::NoPriorHealthyGeneration)?;
        let from = slot.active.number;
        std::mem::swap(&mut slot.active, previous);
        Ok(slot.active.receipt(Some(from)))
    }

    pub fn tick_all(&mut self, tick: i64, host: &mut dyn MeguHost) -> SupervisorTickReport {
        let mut report = SupervisorTickReport::default();
        for (mod_id, slot) in &mut self.plugins {
            if slot.active.guest.quarantined().is_some() {
                continue;
            }
            let mut transaction = slot.kv.transaction();
            let mut context = CallContext {
                host,
                kv: &mut transaction,
                epoch: Some(slot.epoch.checkpoint()),
                isolated: false,
            };
            let result = slot.active.guest.on_tick(tick, &mut context);
            if result.is_ok() {
                transaction.commit();
            }
            report.outcomes.push(PluginTickOutcome {
                mod_id: mod_id.clone(),
                generation: slot.active.number,
                result,
            });
        }
        report
    }

    pub fn generation(&self, mod_id: &str) -> Option<u64> {
        Some(self.plugins.get(mod_id)?.active.number)
    }

    pub fn quarantined(&self, mod_id: &str) -> Option<&ModFault> {
        self.plugins.get(mod_id)?.active.guest.quarantined()
    }

    pub fn epoch_guard(&self, mod_id: &str) -> Option<EpochGuard> {
        Some(self.plugins.get(mod_id)?.epoch.clone())
    }

    pub fn kv_store(&self, mod_id: &str) -> Option<&PluginKvStore> {
        Some(&self.plugins.get(mod_id)?.kv)
    }

    pub fn restore_kv_snapshot(&mut self, mod_id: &str, bytes: &[u8]) -> Result<(), ReloadError> {
        let slot = self.plugins.get_mut(mod_id).ok_or(ReloadError::NotLoaded)?;
        slot.kv = decode_kv_snapshot(bytes, self.kv_limits).map_err(ReloadError::Snapshot)?;
        Ok(())
    }
}

fn stage(
    wasm_bytes: &[u8],
    config: PluginConfig,
    host: &mut dyn MeguHost,
    kv: &mut PluginKvStore,
    epoch: &EpochGuard,
) -> Result<Guest, ReloadError> {
    let checkpoint = epoch.checkpoint();
    let mut transaction = kv.transaction();
    let mut context = CallContext {
        host,
        kv: &mut transaction,
        epoch: Some(checkpoint),
        isolated: true,
    };
    let mut guest = Guest::prepare(
        wasm_bytes,
        config.capabilities,
        config.runtime,
        config.max_contract_rev.min(CURRENT_CONTRACT_REV),
        &mut context,
    )?;
    let status = guest.on_load(&mut context)?;
    if status < 0 {
        return Err(ReloadError::StartupStatus(status));
    }
    checkpoint.check().map_err(ModFault::from)?;
    transaction.commit();
    Ok(guest)
}
