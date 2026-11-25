// SPDX-License-Identifier: LGPL-3.0-or-later

//! Error and fault taxonomy for the WASM mod runtime.
//!
//! Two layers are kept distinct on purpose:
//!
//! * [`LoadError`] — a module/archive that could not even be instantiated
//!   (malformed binary, failed validation, unsupported features, missing
//!   exports). Loading fails closed and the mod never runs.
//! * [`Trap`] — a runtime fault raised *while* guest code executes (OOB
//!   memory, divide by zero, fuel exhaustion, capability denial, ...).
//!   A trap quarantines exactly one mod instance; the host keeps running.

use std::error::Error;
use std::fmt;

use crate::abi::Capability;

/// The module could not be decoded, validated, or instantiated.
#[derive(Clone, Debug, PartialEq)]
pub enum LoadError {
    /// Binary does not decode as a WebAssembly MVP module.
    Malformed(String),
    /// Binary decodes but fails spec validation (type errors, bad
    /// indices, alignment violations, ...).
    Invalid(String),
    /// A feature outside the supported MVP subset was requested
    /// (non-function imports, >1 memory, multi-value block types, ...).
    Unsupported(String),
    /// A required guest export is absent.
    MissingExport(&'static str),
    /// A guest export or host import has a signature other than the
    /// ABI v1 contract.
    SignatureMismatch(String),
    /// `megu_abi_version` returned a version this runtime cannot drive.
    UnsupportedAbi(i32),
    /// The archive container (`.mmod`/`.jar`) could not be read.
    Archive(String),
    /// A declared limit exceeds the configured budget (memory pages,
    /// locals, table size, ...).
    LimitExceeded(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Malformed(m) => write!(f, "malformed module: {m}"),
            LoadError::Invalid(m) => write!(f, "invalid module: {m}"),
            LoadError::Unsupported(m) => write!(f, "unsupported feature: {m}"),
            LoadError::MissingExport(n) => write!(f, "missing required export `{n}`"),
            LoadError::SignatureMismatch(m) => write!(f, "ABI signature mismatch: {m}"),
            LoadError::UnsupportedAbi(v) => write!(f, "unsupported ABI version {v}"),
            LoadError::Archive(m) => write!(f, "archive error: {m}"),
            LoadError::LimitExceeded(m) => write!(f, "declared limit exceeds budget: {m}"),
        }
    }
}

impl Error for LoadError {}

/// A fault raised while guest code executes. Always contained to the
/// offending mod instance.
#[derive(Clone, Debug, PartialEq)]
pub enum Trap {
    /// Guest executed `unreachable`.
    Unreachable,
    /// The per-call fuel budget ran out (infinite loops die here).
    FuelExhausted,
    /// Linear-memory access outside the allocated region.
    MemoryOutOfBounds { addr: u64, len: u64 },
    /// `memory.grow` or initial data exceeded the memory cap.
    MemoryLimit,
    /// Integer divide by zero.
    DivisionByZero,
    /// `INT_MIN / -1` or `INT_MIN % -1` style overflow.
    IntegerOverflow,
    /// Float-to-int conversion of NaN or an out-of-range value.
    BadConversion,
    /// `call_indirect` hit a null table slot.
    IndirectNull { index: u32 },
    /// `call_indirect` hit a function of the wrong signature.
    IndirectSignature { index: u32 },
    /// A host import was invoked without the declared capability.
    CapabilityDenied { capability: Capability, import: &'static str },
    /// Host call attempted to read/write guest memory out of bounds.
    HostMemoryAccess { ptr: u32, len: u32 },
    /// Call depth exceeded the interpreter's bound (guest recursion).
    StackOverflow,
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Trap::Unreachable => write!(f, "guest executed `unreachable`"),
            Trap::FuelExhausted => write!(f, "fuel budget exhausted"),
            Trap::MemoryOutOfBounds { addr, len } => {
                write!(f, "memory access out of bounds: addr={addr:#x} len={len:#x}")
            }
            Trap::MemoryLimit => write!(f, "linear memory cap reached"),
            Trap::DivisionByZero => write!(f, "integer division by zero"),
            Trap::IntegerOverflow => write!(f, "integer overflow"),
            Trap::BadConversion => write!(f, "invalid float-to-int conversion"),
            Trap::IndirectNull { index } => write!(f, "call_indirect on null table index {index}"),
            Trap::IndirectSignature { index } => {
                write!(f, "call_indirect signature mismatch at index {index}")
            }
            Trap::CapabilityDenied { capability, import } => write!(
                f,
                "capability `{}` denied for import `{import}`",
                capability.manifest_key()
            ),
            Trap::HostMemoryAccess { ptr, len } => {
                write!(f, "host string access out of bounds: ptr={ptr:#x} len={len:#x}")
            }
            Trap::StackOverflow => write!(f, "guest call depth exceeded bound"),
        }
    }
}

impl Error for Trap {}

/// Everything that can go wrong driving one mod. The value stored in the
/// instance once it is quarantined.
#[derive(Clone, Debug, PartialEq)]
pub enum ModFault {
    /// Instantiation-time failure; the mod never executed.
    Load(LoadError),
    /// Execution-time trap; the mod is quarantined.
    Trap(Trap),
    /// Any entry point was called after a previous fault quarantined
    /// this instance.
    Quarantined,
}

impl fmt::Display for ModFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModFault::Load(e) => write!(f, "load fault: {e}"),
            ModFault::Trap(t) => write!(f, "trap: {t}"),
            ModFault::Quarantined => write!(f, "mod is quarantined"),
        }
    }
}

impl Error for ModFault {}

impl From<LoadError> for ModFault {
    fn from(e: LoadError) -> Self {
        ModFault::Load(e)
    }
}

impl From<Trap> for ModFault {
    fn from(t: Trap) -> Self {
        ModFault::Trap(t)
    }
}
