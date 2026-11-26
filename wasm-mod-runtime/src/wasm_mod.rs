// SPDX-License-Identifier: LGPL-3.0-or-later

//! `WasmMod`: a loaded, instantiated, fault-isolated WASM mod.

use crate::abi::{
    CapabilitySet, HostBinding, MeguHost, ABI_VERSION, BASE_CONTRACT_REV, CURRENT_CONTRACT_REV,
    EXPORT_ABI_VERSION, EXPORT_ON_BLOCK_INTERACT, EXPORT_ON_LOAD, EXPORT_ON_TICK,
};
use crate::decode::decode_module;
use crate::epoch::EpochCheckpoint;
use crate::error::{LoadError, ModFault, Trap};
use crate::exec::{FuncAddr, Instance, LinearMem, Vm};
use crate::kv::{PluginKvStore, TransactionalKvStore};
use crate::module::{ConstExpr, ExternKind, ImportDesc, Module, ValType, Value};
use crate::validate::validate;

const PAGE: usize = 65_536;
/// Absolute linear-memory ceiling even when an embedder misconfigures
/// `ModConfig`: 1024 pages = 64 MiB per mod. The configurable budget can only
/// tighten this hard cap, never disable it.
const MAX_MEM_PAGES_HARD_CAP: u32 = 1024;

/// Per-mod resource budgets. All bounds are hard: crossing one produces a
/// [`ModFault`], never silent slowdown or host memory growth.
#[derive(Clone, Debug)]
pub struct ModConfig {
    /// Maximum accepted `.wasm` byte size.
    pub max_module_bytes: usize,
    /// Maximum linear-memory pages (64 KiB each).
    pub max_mem_pages: u32,
    /// Fuel for `megu_on_load` / the `start` function.
    pub fuel_on_load: u64,
    /// Fuel for one `megu_on_tick` call.
    pub fuel_per_tick: u64,
    /// Fuel for one `megu_on_block_interact` call.
    pub fuel_per_event: u64,
    /// Maximum nested guest call depth.
    pub max_frames: usize,
    /// Maximum `ptr`/`len` string a host import may read.
    pub max_string: u32,
    /// Maximum manifest bytes read from an archive for capability scan.
    pub max_manifest_bytes: usize,
}

impl Default for ModConfig {
    fn default() -> Self {
        ModConfig {
            max_module_bytes: 8 * 1024 * 1024,
            max_mem_pages: 256, // 16 MiB
            fuel_on_load: 1_000_000,
            fuel_per_tick: 200_000,
            fuel_per_event: 200_000,
            max_frames: 1024,
            max_string: 64 * 1024,
            max_manifest_bytes: 256 * 1024,
        }
    }
}

/// Expected guest-export signatures, checked at instantiation for every
/// export that is present.
fn expected_sig(name: &str) -> Option<(&'static [ValType], &'static [ValType])> {
    use ValType::*;
    match name {
        EXPORT_ABI_VERSION => Some((&[], &[I32])),
        EXPORT_ON_LOAD => Some((&[], &[I32])),
        EXPORT_ON_TICK => Some((&[I64], &[I32])),
        EXPORT_ON_BLOCK_INTERACT => Some((&[I32, I32, I32, I32], &[I32])),
        _ => None,
    }
}

/// A loaded mod instance with an owned host. The supervisor uses the same
/// guest interpreter with a borrowed host and a store shared across generations.
pub struct WasmMod {
    guest: Guest,
    host: Box<dyn MeguHost>,
    kv: PluginKvStore,
}

impl WasmMod {
    /// Decode, validate, instantiate, and handshake a raw `.wasm` module.
    ///
    /// Fails closed on malformed binaries, unknown or mis-signed imports,
    /// absent or unsupported ABI versions, and traps during initialization.
    /// Capability checks in this API take place when an import is called.
    pub fn from_wasm(
        bytes: &[u8],
        mut host: Box<dyn MeguHost>,
        caps: CapabilitySet,
        config: ModConfig,
    ) -> Result<Self, ModFault> {
        let mut kv = PluginKvStore::new(Default::default());
        let mut transaction = kv.transaction();
        let mut context = CallContext {
            host: host.as_mut(),
            kv: &mut transaction,
            epoch: None,
            isolated: false,
        };
        let guest = Guest::prepare(bytes, caps, config, CURRENT_CONTRACT_REV, &mut context)?;
        transaction.commit();
        Ok(Self { guest, host, kv })
    }

    /// The mod's capabilities from the manifest or caller's explicit grant.
    pub fn capabilities(&self) -> CapabilitySet {
        self.guest.caps
    }

    /// ABI version reported by the guest (always 1 after a successful load).
    pub fn abi_version(&self) -> i32 {
        self.guest.abi_version
    }

    /// The fault that quarantined this mod, if any.
    pub fn quarantined(&self) -> Option<&ModFault> {
        self.guest.quarantined()
    }

    /// `megu_on_load()` — missing export is a no-op returning 0.
    pub fn on_load(&mut self) -> Result<i32, ModFault> {
        self.call(EXPORT_ON_LOAD, &[], self.guest.config.fuel_on_load)
    }

    /// `megu_on_tick(tick)`.
    pub fn on_tick(&mut self, tick: i64) -> Result<i32, ModFault> {
        self.call(
            EXPORT_ON_TICK,
            &[Value::I64(tick)],
            self.guest.config.fuel_per_tick,
        )
    }

    /// `megu_on_block_interact(x, y, z, player_id)`.
    pub fn on_block_interact(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        player_id: i32,
    ) -> Result<i32, ModFault> {
        self.call(
            EXPORT_ON_BLOCK_INTERACT,
            &[
                Value::I32(x),
                Value::I32(y),
                Value::I32(z),
                Value::I32(player_id),
            ],
            self.guest.config.fuel_per_event,
        )
    }

    /// Check whether the guest exports the named function.
    pub fn has_export(&self, name: &str) -> bool {
        self.guest.inst.module.export_func(name).is_some()
    }

    fn call(&mut self, name: &'static str, args: &[Value], fuel: u64) -> Result<i32, ModFault> {
        let mut transaction = self.kv.transaction();
        let mut context = CallContext {
            host: self.host.as_mut(),
            kv: &mut transaction,
            epoch: None,
            isolated: false,
        };
        let status = self.guest.call_optional(name, args, fuel, &mut context)?;
        transaction.commit();
        Ok(status)
    }
}

pub(crate) struct CallContext<'a, 'kv> {
    pub host: &'a mut dyn MeguHost,
    pub kv: &'a mut TransactionalKvStore<'kv>,
    pub epoch: Option<EpochCheckpoint<'a>>,
    pub isolated: bool,
}

pub(crate) struct Guest {
    inst: Instance,
    caps: CapabilitySet,
    config: ModConfig,
    quarantine: Option<ModFault>,
    abi_version: i32,
}

impl Guest {
    pub(crate) fn prepare(
        bytes: &[u8],
        caps: CapabilitySet,
        config: ModConfig,
        max_contract_rev: u32,
        context: &mut CallContext<'_, '_>,
    ) -> Result<Self, ModFault> {
        if bytes.len() > config.max_module_bytes {
            return Err(LoadError::LimitExceeded("module byte size".into()).into());
        }
        let raw = decode_module(bytes)?;
        let module = validate(raw)?;
        check_export_signatures(&module)?;
        let inst = instantiate(
            module,
            &config,
            context.isolated.then_some(caps),
            max_contract_rev,
        )?;
        let mut guest = Self {
            inst,
            caps,
            config,
            quarantine: None,
            abi_version: -1,
        };

        let fuel = guest.config.fuel_on_load;
        if guest.inst.module.export_func(EXPORT_ABI_VERSION).is_none() {
            return Err(LoadError::MissingExport(EXPORT_ABI_VERSION).into());
        }
        let version = guest.call_optional(EXPORT_ABI_VERSION, &[], fuel, context)?;
        if version != ABI_VERSION {
            return Err(LoadError::UnsupportedAbi(version).into());
        }
        guest.abi_version = version;

        if let Some(start) = guest.inst.module.start {
            guest.invoke(start, &[], fuel, context)?;
        }
        Ok(guest)
    }

    pub(crate) fn contract_rev(&self) -> u32 {
        self.inst.contract_rev
    }

    pub(crate) fn quarantined(&self) -> Option<&ModFault> {
        self.quarantine.as_ref()
    }

    pub(crate) fn on_load(&mut self, context: &mut CallContext<'_, '_>) -> Result<i32, ModFault> {
        self.call_optional(EXPORT_ON_LOAD, &[], self.config.fuel_on_load, context)
    }

    pub(crate) fn on_tick(
        &mut self,
        tick: i64,
        context: &mut CallContext<'_, '_>,
    ) -> Result<i32, ModFault> {
        self.call_optional(
            EXPORT_ON_TICK,
            &[Value::I64(tick)],
            self.config.fuel_per_tick,
            context,
        )
    }

    fn call_optional(
        &mut self,
        name: &'static str,
        args: &[Value],
        fuel: u64,
        context: &mut CallContext<'_, '_>,
    ) -> Result<i32, ModFault> {
        if self.quarantine.is_some() {
            return Err(ModFault::Quarantined);
        }
        let Some(fidx) = self.inst.module.export_func(name) else {
            return Ok(0);
        };
        match self.invoke(fidx, args, fuel, context) {
            Ok(results) => Ok(results.first().map_or(0, |v| v.i32())),
            Err(t) => {
                let fault = ModFault::Trap(t);
                self.quarantine = Some(fault.clone());
                Err(fault)
            }
        }
    }

    fn invoke(
        &mut self,
        fidx: u32,
        args: &[Value],
        fuel: u64,
        context: &mut CallContext<'_, '_>,
    ) -> Result<Vec<Value>, Trap> {
        let mut vm = Vm {
            m: &self.inst.module,
            funcs: &self.inst.funcs,
            mem: &mut self.inst.mem,
            globals: &mut self.inst.globals,
            table: &mut self.inst.table,
            host: &mut *context.host,
            kv: &mut *context.kv,
            epoch: context.epoch,
            isolated: context.isolated,
            caps: self.caps,
            fuel,
            max_frames: self.config.max_frames,
            max_string: self.config.max_string,
            stack: Vec::new(),
            frames: Vec::new(),
        };
        vm.invoke(fidx, args)
    }
}

/// Build the runtime instance: resolve imports, allocate memory/globals/
/// table, apply element and data segments (bounds-checked).
fn instantiate(
    module: Module,
    config: &ModConfig,
    required_grants: Option<CapabilitySet>,
    max_contract_rev: u32,
) -> Result<Instance, ModFault> {
    // Resolve every import against the ABI v1 host binding table.
    let mut funcs: Vec<FuncAddr> = Vec::new();
    let mut contract_rev = BASE_CONTRACT_REV;
    for imp in &module.imports {
        let ImportDesc::Func(ty_idx) = imp.desc else {
            return Err(LoadError::Unsupported(format!(
                "non-function import `{}.{}`",
                imp.module, imp.name
            ))
            .into());
        };
        let ft = &module.types[ty_idx as usize];
        let binding = HostBinding::resolve(&imp.module, &imp.name, &ft.params, &ft.results)
            .map_err(LoadError::SignatureMismatch)?;
        if let (Some(caps), Some(capability)) = (required_grants, binding.capability()) {
            caps.require(capability, binding.name())?;
        }
        contract_rev = contract_rev.max(binding.contract_rev());
        funcs.push(FuncAddr::Host {
            binding,
            params: ft.params.len(),
            results: ft.results.len(),
        });
    }
    if contract_rev > max_contract_rev {
        return Err(LoadError::UnsupportedContract {
            required: contract_rev,
            supported: max_contract_rev,
        }
        .into());
    }
    for ty_idx in &module.func_types {
        let ft = &module.types[*ty_idx as usize];
        funcs.push(FuncAddr::Wasm {
            def: funcs.len() - module.import_func_count(),
            params: ft.params.len(),
            results: ft.results.len(),
        });
    }

    // Memory: allocate the declared minimum, capped by config.
    let mem = match module.mems.first() {
        Some(decl) => {
            let hard_max = config.max_mem_pages.min(MAX_MEM_PAGES_HARD_CAP);
            if decl.limits.min > hard_max {
                return Err(LoadError::LimitExceeded("memory minimum exceeds cap".into()).into());
            }
            let max_pages = decl.limits.max.unwrap_or(hard_max).min(hard_max);
            LinearMem {
                data: vec![0; decl.limits.min as usize * PAGE],
                max_pages,
            }
        }
        None => LinearMem {
            data: Vec::new(),
            max_pages: 0,
        },
    };

    // Globals: constant init expressions in order.
    let mut globals = Vec::with_capacity(module.globals.len());
    for g in &module.globals {
        let v = match g.init {
            ConstExpr::I32(v) => Value::I32(v),
            ConstExpr::I64(v) => Value::I64(v),
            ConstExpr::F32(v) => Value::F32(v),
            ConstExpr::F64(v) => Value::F64(v),
            ConstExpr::GlobalGet(i) => globals.get(i as usize).copied().ok_or_else(|| {
                LoadError::Invalid("global init references missing global".into())
            })?,
        };
        globals.push(v);
    }

    // Table.
    let table = match module.tables.first() {
        Some(t) => {
            const MAX_TABLE: u32 = 1_000_000;
            if t.limits.min > MAX_TABLE {
                return Err(LoadError::LimitExceeded("table size".into()).into());
            }
            vec![None; t.limits.min as usize]
        }
        None => Vec::new(),
    };

    let mut inst = Instance {
        contract_rev,
        module,
        funcs,
        mem,
        globals,
        table,
    };

    // Element segments.
    for e in &inst.module.elems {
        let ConstExpr::I32(off) = e.offset else {
            return Err(LoadError::Invalid("elem offset not i32".into()).into());
        };
        let base = off as u32 as usize;
        let end = base.saturating_add(e.funcs.len());
        if end > inst.table.len() {
            return Err(LoadError::Invalid("elem segment out of bounds".into()).into());
        }
        for (i, f) in e.funcs.iter().enumerate() {
            inst.table[base + i] = Some(*f);
        }
    }

    // Data segments.
    for d in &inst.module.datas {
        let ConstExpr::I32(off) = d.offset else {
            return Err(LoadError::Invalid("data offset not i32".into()).into());
        };
        let base = off as u32 as usize;
        let end = base.saturating_add(d.bytes.len());
        if end > inst.mem.data.len() {
            return Err(LoadError::Invalid("data segment out of bounds".into()).into());
        }
        inst.mem.data[base..end].copy_from_slice(&d.bytes);
    }

    Ok(inst)
}

/// Evaluate which megu_v1 exports exist and check their signatures at
/// load time — a wrong-shaped export is a load fault, not a later trap.
pub(crate) fn check_export_signatures(m: &Module) -> Result<(), LoadError> {
    for e in &m.exports {
        if e.kind != ExternKind::Func {
            continue;
        }
        if let Some((want_p, want_r)) = expected_sig(&e.name) {
            let ty_idx = m
                .func_type_index(e.index)
                .ok_or_else(|| LoadError::Invalid("export func index".into()))?;
            let ft = &m.types[ty_idx as usize];
            if ft.params.as_slice() != want_p || ft.results.as_slice() != want_r {
                return Err(LoadError::SignatureMismatch(format!(
                    "export `{}` wants {want_p:?}->{want_r:?}, got {:?}->{:?}",
                    e.name, ft.params, ft.results
                )));
            }
        }
    }
    Ok(())
}
