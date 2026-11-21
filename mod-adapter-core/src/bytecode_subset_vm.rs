// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Pure-`std`, zero-JVM bounded `.class` bytecode subset evaluator.
//!
//! Runs *safe* mod event/logic methods — damage modifiers, right-click
//! handlers, simple arithmetic gates — without ever launching a JVM.
//! Three fail-closed stages stand between a mod JAR and a server tick:
//!
//! 1. [`parse_class_methods`] parses real JVMS §4 `.class` bytes (magic
//!    `0xCAFEBABE`, the full constant-pool tag set with dual-slot
//!    `Long`/`Double` handling, methods, and each method's `Code`
//!    attribute) using the same clean-room conventions as
//!    [`crate::class_mixin_scanner`]. Truncation, bad magic, oversized
//!    counts, and malformed modified UTF-8 are hard errors — never a
//!    panic, never a partial class.
//! 2. [`verify_subset_method`] statically verifies a method **before a
//!    single opcode executes**: every instruction is inside an explicit
//!    `ALLOWED` opcode table, every `u8`/`u16` operand is bounds-checked,
//!    every branch target lands exactly on an instruction boundary inside
//!    the code array, every `ldc`/`invoke*` constant-pool index resolves
//!    to the tag the opcode requires, `code_length <=
//!    [`MAX_CODE_LENGTH`]`, `max_stack <= [`MAX_OPERAND_STACK`]`,
//!    `max_locals <= [`MAX_LOCAL_SLOTS`]`, a dataflow pass proves the
//!    operand-stack depth never exceeds `max_stack` and agrees at every
//!    merge point, and every `invokestatic`/`invokevirtual`/
//!    `invokeinterface` target resolves through `resolve_host_call` to an
//!    allowlisted [`HostCallKind`] — Mojang and clean symbolic
//!    descriptors alike. Reflection (`java/lang/reflect/*`,
//!    `java/lang/Class`, `java/lang/invoke/*`), process/OS/IO/network
//!    classes (`java/lang/Runtime`, `java/lang/ProcessBuilder`,
//!    `java/io/*`, `java/nio/*`, `java/net/*`, `javax/naming/*`,
//!    `sun/misc/Unsafe`, ...), and *any* unknown class or method are hard
//!    rejections — as are `new`, `anewarray`, `athrow`, `monitorenter`,
//!    `invokedynamic`, field access, `l*`/`d*`/`a*return` forms, and
//!    every other opcode outside the table.
//! 3. [`execute_verified_method`] interprets the verified instruction
//!    list on a deterministic [`VmValue`] stack with exact Java semantics
//!    (`wrapping_*` integer arithmetic, `i32::MIN / -1` wraparound,
//!    divide/remainder-by-zero errors, `f2i` saturation, `fcmpl`/`fcmpg`
//!    NaN polarity) under a strict instruction fuel cap — an infinite
//!    `goto` loop ends in [`VmError::OutOfFuel`], never in a hung tick.
//!
//! The only way bytecode touches the outside world is the allowlisted
//! [`SubsetVmHost`] bridge: player/level/event accessors and bounded
//! float math. There is no heap, no allocation opcode, no exception
//! mechanism, no reflection, and no native call — a Megu2Shell-style
//! `InitialContext.lookup` or `Runtime.exec` payload is rejected by the
//! verifier before its first instruction executes.

use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Bounds & limits
// ---------------------------------------------------------------------------

/// Largest `code_length` the verifier accepts (JVMS `u4`, subset cap).
pub const MAX_CODE_LENGTH: usize = 4096;

/// Largest `max_stack` the verifier accepts.
pub const MAX_OPERAND_STACK: usize = 64;

/// Largest `max_locals` the verifier accepts.
pub const MAX_LOCAL_SLOTS: usize = 64;

/// Default instruction budget for [`execute_verified_method`].
pub const DEFAULT_FUEL: u32 = 10_000;

// Parse-time structural caps (defense in depth on top of `u16` counts).
const MAX_CONSTANT_POOL: usize = 8192;
const MAX_METHODS: usize = 2048;
const MAX_FIELDS: usize = 2048;
const MAX_INTERFACES: usize = 256;
const MAX_ATTRIBUTES: usize = 512;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a `.class` parse, verification, or bounded execution failed.
/// Every variant is terminal: nothing partially verified or partially
/// executed is ever treated as success.
#[derive(Clone, Debug, PartialEq)]
pub enum VmError {
    /// `magic != 0xCAFEBABE` — not a JVM class file.
    BadMagic,
    /// Fewer bytes than the JVMS §4 structure being read requires.
    Truncated(&'static str),
    /// A constant-pool index was 0 or past `constant_pool_count - 1`.
    BadConstantPoolIndex(&'static str),
    /// A constant-pool slot held a tag other than the consumer requires.
    BadConstantPoolTag(&'static str),
    /// Constant-pool tag byte outside the standard `1..=20` set.
    UnsupportedConstantPoolTag(u8),
    /// A `CONSTANT_Utf8` payload is not well-formed modified UTF-8.
    InvalidUtf8,
    /// Structurally invalid in some other way; the string names where.
    Malformed(&'static str),
    /// A `u16` count exceeded the module's structural cap.
    LimitExceeded(&'static str),
    /// No method matched the requested `(name, descriptor)` pair.
    MethodNotFound(String),
    /// The matched method has no `Code` attribute (abstract/native).
    MethodHasNoCode(String),
    /// `code_length` exceeded [`MAX_CODE_LENGTH`].
    CodeTooLong { length: usize, max: usize },
    /// `max_stack` exceeded [`MAX_OPERAND_STACK`].
    StackTooDeep { max_stack: usize, max: usize },
    /// `max_locals` exceeded [`MAX_LOCAL_SLOTS`].
    TooManyLocals { max_locals: usize, max: usize },
    /// An opcode outside the subset allowlist was found during
    /// verification — the instruction never executes.
    DisallowedOpcode { offset: usize, opcode: u8 },
    /// A multi-byte instruction ran past `code_length`.
    TruncatedInstruction { offset: usize },
    /// A branch target was outside the code array or landed inside a
    /// multi-byte instruction rather than on an instruction boundary.
    BadBranchTarget { offset: usize, target: i64 },
    /// An `invoke*` constant-pool index did not resolve to a
    /// `Methodref`/`InterfaceMethodref` (or `ldc` to a loadable
    /// `Integer`/`Float`/`String` constant).
    BadConstantKind { offset: usize, ctx: &'static str },
    /// The invocation target names an explicitly forbidden class family
    /// (reflection, process/IO/network, `sun.misc.Unsafe`, JNDI, ...).
    ForbiddenHostClass { offset: usize, class: String },
    /// The invocation target is not on the [`HostCallKind`] allowlist.
    UnknownHostCall {
        offset: usize,
        class: String,
        method: String,
        descriptor: String,
    },
    /// `(class, name)` matched an allowlisted host method but the
    /// descriptor did not — the call is rejected rather than coerced.
    HostSignatureMismatch {
        offset: usize,
        class: String,
        method: String,
        descriptor: String,
    },
    /// An `invokeinterface` `count` operand disagreed with the resolved
    /// host signature (`count` must be args + receiver per JVMS §6.5).
    BadInterfaceCount {
        offset: usize,
        count: u8,
        expected: usize,
    },
    /// The dataflow verifier proved an instruction can run with fewer
    /// operands on the stack than it pops.
    VerifyStackUnderflow { offset: usize },
    /// The dataflow verifier proved an instruction can leave the operand
    /// stack deeper than the method's declared `max_stack`.
    VerifyStackOverflow { offset: usize },
    /// Two control-flow paths reach one instruction with different
    /// operand-stack depths — unverifiable, so rejected.
    VerifyStackMerge {
        offset: usize,
        depth_a: usize,
        depth_b: usize,
    },
    /// Execution would fall off the end of the code array (the JVMS
    /// forbids the last instruction from completing normally).
    VerifyFallThrough { offset: usize },
    /// A load/store/`iinc`/argument slot was outside `max_locals`.
    LocalIndexOutOfBounds { index: usize },
    /// The instruction fuel budget was exhausted — a `goto`-self loop or
    /// runaway branch cannot hang the server tick.
    OutOfFuel,
    /// `idiv`/`irem` with a zero divisor.
    ArithmeticDivByZero,
    /// The operand stack underflowed or overflowed at run time. Verified
    /// code never reaches this; it is defense in depth.
    StackUnderflow,
    /// A value of the wrong [`VmValue`] kind reached an instruction or
    /// host call (`fload` on an `Int` local, an `Int` receiver, ...).
    TypeMismatch(&'static str),
    /// A host bridge returned an error or a value of the wrong kind.
    HostError(String),
    /// `ireturn`/`freturn` completed without a value, or `return` on a
    /// method the caller asked for a value from — defensive only.
    MissingReturnValue,
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => f.write_str("bad magic (not a 0xCAFEBABE class file)"),
            Self::Truncated(what) => write!(f, "truncated class file reading {what}"),
            Self::BadConstantPoolIndex(ctx) => {
                write!(f, "constant-pool index out of range for {ctx}")
            }
            Self::BadConstantPoolTag(ctx) => {
                write!(f, "constant-pool slot has the wrong tag for {ctx}")
            }
            Self::UnsupportedConstantPoolTag(tag) => {
                write!(f, "unsupported constant-pool tag {tag}")
            }
            Self::InvalidUtf8 => f.write_str("invalid modified UTF-8 in CONSTANT_Utf8"),
            Self::Malformed(what) => write!(f, "malformed input: {what}"),
            Self::LimitExceeded(what) => write!(f, "structural limit exceeded: {what}"),
            Self::MethodNotFound(sig) => write!(f, "no such method: {sig}"),
            Self::MethodHasNoCode(sig) => write!(f, "method has no Code attribute: {sig}"),
            Self::CodeTooLong { length, max } => {
                write!(f, "code_length {length} exceeds bound {max}")
            }
            Self::StackTooDeep { max_stack, max } => {
                write!(f, "max_stack {max_stack} exceeds bound {max}")
            }
            Self::TooManyLocals { max_locals, max } => {
                write!(f, "max_locals {max_locals} exceeds bound {max}")
            }
            Self::DisallowedOpcode { offset, opcode } => {
                write!(f, "disallowed opcode 0x{opcode:02X} at offset {offset}")
            }
            Self::TruncatedInstruction { offset } => {
                write!(f, "instruction at offset {offset} runs past code_length")
            }
            Self::BadBranchTarget { offset, target } => {
                write!(f, "branch at offset {offset} targets invalid offset {target}")
            }
            Self::BadConstantKind { offset, ctx } => {
                write!(f, "bad constant-pool kind for {ctx} at offset {offset}")
            }
            Self::ForbiddenHostClass { offset, class } => {
                write!(f, "forbidden host class {class} at offset {offset}")
            }
            Self::UnknownHostCall {
                offset,
                class,
                method,
                descriptor,
            } => write!(
                f,
                "call to non-allowlisted method {class}.{method}{descriptor} at offset {offset}"
            ),
            Self::HostSignatureMismatch {
                offset,
                class,
                method,
                descriptor,
            } => write!(
                f,
                "host method {class}.{method} does not support descriptor {descriptor} (offset {offset})"
            ),
            Self::BadInterfaceCount {
                offset,
                count,
                expected,
            } => write!(
                f,
                "invokeinterface count {count} != expected {expected} at offset {offset}"
            ),
            Self::VerifyStackUnderflow { offset } => {
                write!(f, "operand stack underflow at offset {offset}")
            }
            Self::VerifyStackOverflow { offset } => {
                write!(f, "operand stack exceeds max_stack at offset {offset}")
            }
            Self::VerifyStackMerge {
                offset,
                depth_a,
                depth_b,
            } => write!(
                f,
                "inconsistent stack depths {depth_a} vs {depth_b} merging at offset {offset}"
            ),
            Self::VerifyFallThrough { offset } => {
                write!(f, "execution can fall off the end after offset {offset}")
            }
            Self::LocalIndexOutOfBounds { index } => {
                write!(f, "local slot {index} out of bounds")
            }
            Self::OutOfFuel => f.write_str("instruction fuel exhausted"),
            Self::ArithmeticDivByZero => f.write_str("integer division by zero"),
            Self::StackUnderflow => f.write_str("operand stack underflow"),
            Self::TypeMismatch(ctx) => write!(f, "type mismatch: {ctx}"),
            Self::HostError(msg) => write!(f, "host bridge error: {msg}"),
            Self::MissingReturnValue => f.write_str("method returned no value"),
        }
    }
}

impl std::error::Error for VmError {}

// ---------------------------------------------------------------------------
// Values & types
// ---------------------------------------------------------------------------

/// The category-1 value universe of the subset VM. `Ref` is an opaque
/// host handle (player, level, event) that bytecode can store, move, and
/// pass back to the host — never inspect, mutate, or forge. `Str`
/// carries `ldc` string constants into host calls such as
/// [`HostCallKind::PlayerSendMessage`].
#[derive(Clone, Debug, PartialEq)]
pub enum VmValue {
    Int(i32),
    Float(f32),
    /// Opaque host object handle; `0` is the JVM `null` and compares
    /// unequal to any live handle.
    Ref(u32),
    Str(String),
}

/// Expected shape of a [`VmValue`] for type checking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmType {
    Int,
    Float,
    Ref,
    Str,
}

impl VmValue {
    fn ty(&self) -> VmType {
        match self {
            Self::Int(_) => VmType::Int,
            Self::Float(_) => VmType::Float,
            Self::Ref(_) => VmType::Ref,
            Self::Str(_) => VmType::Str,
        }
    }

    fn is_ref_kind(&self) -> bool {
        matches!(self, Self::Ref(_) | Self::Str(_))
    }
}

// ---------------------------------------------------------------------------
// Host calls
// ---------------------------------------------------------------------------

/// The complete host-call allowlist. Every `invoke*` instruction in a
/// verified method resolves to exactly one of these; anything else is a
/// verification failure. Static kinds take only their declared
/// parameters; the instance kinds take their receiver as `args[0]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HostCallKind {
    /// `player.getHealth()F` → `Float`.
    PlayerGetHealth,
    /// `player.setHealth(F)V`.
    PlayerSetHealth,
    /// `player.sendMessage(Ljava/lang/String;)V`.
    PlayerSendMessage,
    /// `player.giveItem(Ljava/lang/String;I)V` — item id + count.
    PlayerGiveItem,
    /// `level.getBlockId(III)Ljava/lang/String;` → block id `Str`.
    LevelGetBlock,
    /// `level.setBlock(IIILjava/lang/String;)V` — x, y, z, block id.
    LevelSetBlock,
    /// `event.getDamage()F` → `Float`.
    EventGetDamage,
    /// `event.setDamage(F)V`.
    EventSetDamage,
    /// `event.cancel()V`.
    EventCancel,
    /// `Math.max(FF)F` / `Mth.max(FF)F` → `Float`.
    MathMaxFloat,
    /// `Math.min(FF)F` / `Mth.min(FF)F` → `Float`.
    MathMinFloat,
    /// `Mth.clamp(FFF)F` → `Float`.
    MathClampFloat,
}

/// Signature of one [`HostCallKind`] in [`VmType`] terms.
pub struct HostSig {
    /// `Some(VmType::Ref)` for instance (virtual/interface) calls.
    pub receiver: Option<VmType>,
    /// Declared parameters, left-to-right push order.
    pub params: &'static [VmType],
    /// `Some` when the call pushes a result.
    pub result: Option<VmType>,
}

use VmType::{Float as FT, Int as IT, Ref as RT, Str as ST};

impl HostCallKind {
    /// The VM-level signature used for both stack-effect verification
    /// and run-time type checking of arguments and results.
    pub fn signature(self) -> HostSig {
        match self {
            Self::PlayerGetHealth => HostSig {
                receiver: Some(RT),
                params: &[],
                result: Some(FT),
            },
            Self::PlayerSetHealth => HostSig {
                receiver: Some(RT),
                params: &[FT],
                result: None,
            },
            Self::PlayerSendMessage => HostSig {
                receiver: Some(RT),
                params: &[ST],
                result: None,
            },
            Self::PlayerGiveItem => HostSig {
                receiver: Some(RT),
                params: &[ST, IT],
                result: None,
            },
            Self::LevelGetBlock => HostSig {
                receiver: Some(RT),
                params: &[IT, IT, IT],
                result: Some(ST),
            },
            Self::LevelSetBlock => HostSig {
                receiver: Some(RT),
                params: &[IT, IT, IT, ST],
                result: None,
            },
            Self::EventGetDamage => HostSig {
                receiver: Some(RT),
                params: &[],
                result: Some(FT),
            },
            Self::EventSetDamage => HostSig {
                receiver: Some(RT),
                params: &[FT],
                result: None,
            },
            Self::EventCancel => HostSig {
                receiver: Some(RT),
                params: &[],
                result: None,
            },
            Self::MathMaxFloat | Self::MathMinFloat => HostSig {
                receiver: None,
                params: &[FT, FT],
                result: Some(FT),
            },
            Self::MathClampFloat => HostSig {
                receiver: None,
                params: &[FT, FT, FT],
                result: Some(FT),
            },
        }
    }
}

/// Class families that are forbidden outright, checked before the
/// allowlist so the error names the attack rather than "not found".
/// Match is exact-name or `prefix/` — `java/lang/Runtime` covers
/// `java/lang/Runtime` itself but not `java/lang/RuntimePermission`
/// (denied anyway by the allowlist catch-all).
const FORBIDDEN_CLASS_PREFIXES: &[&str] = &[
    "java/lang/reflect/",
    "java/lang/invoke/",
    "java/lang/classfile/",
    "java/lang/foreign/",
    "java/lang/Class",
    "java/lang/ClassLoader",
    "java/lang/Module",
    "java/lang/Runtime",
    "java/lang/Process",
    "java/lang/System",
    "java/lang/Thread",
    "java/lang/SecurityManager",
    "java/lang/Compiler",
    "java/lang/Package",
    "java/io/",
    "java/nio/",
    "java/net/",
    "java/sql/",
    "java/security/",
    "javax/",
    "sun/",
    "com/sun/",
    "jdk/",
    "org/w3c/",
    "org/xml/",
];

fn is_forbidden_class(internal: &str) -> bool {
    FORBIDDEN_CLASS_PREFIXES
        .iter()
        .any(|p| internal.starts_with(p))
}

/// One allowlist row: any `class` in `classes` × any `name` in `names`
/// × any `descriptor` in `descriptors` resolves to `kind`. Descriptors
/// are enforced so e.g. `Math.max(II)I` (int overload) cannot smuggle a
/// float host call past the signature contract.
struct HostAllow {
    kind: HostCallKind,
    classes: &'static [&'static str],
    names: &'static [&'static str],
    descriptors: &'static [&'static str],
}

/// Mojang-mapped player classes plus the clean symbolic surface.
const PLAYER_CLASSES: &[&str] = &[
    "net/minecraft/world/entity/player/Player",
    "net/minecraft/world/entity/player/ServerPlayer",
    "net/minecraft/world/entity/LivingEntity",
    "net/minecraft/world/entity/Entity",
    "mcspine/api/Player",
    "mcspine/Player",
];

const LEVEL_CLASSES: &[&str] = &[
    "net/minecraft/world/level/Level",
    "net/minecraft/world/level/LevelAccessor",
    "net/minecraft/server/level/ServerLevel",
    "mcspine/api/Level",
    "mcspine/Level",
];

/// Forge/NeoForge damage events plus the clean symbolic event surface.
const EVENT_CLASSES: &[&str] = &[
    "net/minecraftforge/event/entity/living/LivingDamageEvent",
    "net/minecraftforge/event/entity/living/LivingHurtEvent",
    "net/neoforged/neoforge/event/entity/living/LivingDamageEvent",
    "mcspine/api/Event",
    "mcspine/api/DamageEvent",
    "mcspine/Event",
];

const MATH_CLASSES: &[&str] = &[
    "java/lang/Math",
    "net/minecraft/util/Mth",
    "mcspine/api/Math",
];

const CLAMP_CLASSES: &[&str] = &["net/minecraft/util/Mth", "mcspine/api/Math"];

const HOST_ALLOWLIST: &[HostAllow] = &[
    HostAllow {
        kind: HostCallKind::PlayerGetHealth,
        classes: PLAYER_CLASSES,
        names: &["getHealth"],
        descriptors: &["()F"],
    },
    HostAllow {
        kind: HostCallKind::PlayerSetHealth,
        classes: PLAYER_CLASSES,
        names: &["setHealth"],
        descriptors: &["(F)V"],
    },
    HostAllow {
        kind: HostCallKind::PlayerSendMessage,
        classes: PLAYER_CLASSES,
        names: &[
            "sendMessage",
            "sendSystemMessage",
            "displayClientMessage",
            "sendLiteralMessage",
        ],
        descriptors: &[
            "(Ljava/lang/String;)V",
            "(Lnet/minecraft/network/chat/Component;)V",
        ],
    },
    HostAllow {
        kind: HostCallKind::PlayerGiveItem,
        classes: PLAYER_CLASSES,
        names: &["giveItem", "give", "addItem"],
        descriptors: &["(Ljava/lang/String;I)V"],
    },
    HostAllow {
        kind: HostCallKind::LevelGetBlock,
        classes: LEVEL_CLASSES,
        names: &["getBlock", "getBlockId", "getBlockName"],
        descriptors: &[
            "(III)Ljava/lang/String;",
            "(III)Lnet/minecraft/world/level/block/state/BlockState;",
        ],
    },
    HostAllow {
        kind: HostCallKind::LevelSetBlock,
        classes: LEVEL_CLASSES,
        names: &["setBlock", "setBlockId"],
        descriptors: &["(IIILjava/lang/String;)V"],
    },
    HostAllow {
        kind: HostCallKind::EventGetDamage,
        classes: EVENT_CLASSES,
        names: &["getDamage", "getAmount", "getOriginalDamage"],
        descriptors: &["()F"],
    },
    HostAllow {
        kind: HostCallKind::EventSetDamage,
        classes: EVENT_CLASSES,
        names: &["setDamage", "setAmount"],
        descriptors: &["(F)V"],
    },
    HostAllow {
        kind: HostCallKind::EventCancel,
        classes: EVENT_CLASSES,
        names: &["cancel", "setCancelled"],
        descriptors: &["()V"],
    },
    HostAllow {
        kind: HostCallKind::MathMaxFloat,
        classes: MATH_CLASSES,
        names: &["max"],
        descriptors: &["(FF)F"],
    },
    HostAllow {
        kind: HostCallKind::MathMinFloat,
        classes: MATH_CLASSES,
        names: &["min"],
        descriptors: &["(FF)F"],
    },
    HostAllow {
        kind: HostCallKind::MathClampFloat,
        classes: CLAMP_CLASSES,
        names: &["clamp", "clampFloat"],
        descriptors: &["(FFF)F"],
    },
];

/// Classify one `(class, name, descriptor)` invocation target.
enum HostResolution {
    /// Fully allowlisted.
    Kind(HostCallKind),
    /// Class + name matched but no descriptor did.
    SignatureMismatch,
    /// Class is on the explicit forbidden list.
    Forbidden,
    /// Nothing matched — rejected all the same.
    Unknown,
}

fn resolve_host_call(class: &str, name: &str, descriptor: &str) -> HostResolution {
    if is_forbidden_class(class) {
        return HostResolution::Forbidden;
    }
    let mut name_matched = false;
    for entry in HOST_ALLOWLIST {
        if !entry.classes.contains(&class) || !entry.names.contains(&name) {
            continue;
        }
        name_matched = true;
        if entry.descriptors.contains(&descriptor) {
            return HostResolution::Kind(entry.kind);
        }
    }
    if name_matched {
        HostResolution::SignatureMismatch
    } else {
        HostResolution::Unknown
    }
}

/// The host bridge — the only side channel out of the VM. `args` holds
/// the receiver (instance calls) at index `0` followed by the declared
/// parameters in push order; the VM has already type-checked them
/// against [`HostCallKind::signature`]. Returning `Ok(Some(v))` for a
/// call whose signature declares a result, `Ok(None)` otherwise; a
/// result of the wrong type is re-checked by the VM and becomes
/// [`VmError::HostError`].
pub trait SubsetVmHost {
    fn call(&mut self, kind: HostCallKind, args: &[VmValue]) -> Result<Option<VmValue>, VmError>;
}

// ---------------------------------------------------------------------------
// JVMS §4 parsing
// ---------------------------------------------------------------------------

/// Big-endian bounds-checked cursor over one byte slice.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], VmError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(VmError::Malformed("length overflow"))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(VmError::Truncated(what))?;
        self.pos = end;
        Ok(slice)
    }

    fn skip(&mut self, n: usize, what: &'static str) -> Result<(), VmError> {
        self.take(n, what).map(|_| ())
    }

    fn u1(&mut self, what: &'static str) -> Result<u8, VmError> {
        Ok(self.take(1, what)?[0])
    }

    fn u2(&mut self, what: &'static str) -> Result<u16, VmError> {
        let b = self.take(2, what)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u4(&mut self, what: &'static str) -> Result<u32, VmError> {
        let b = self.take(4, what)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Constant-pool entries the VM resolves. Dual-slot `Long`/`Double`
/// members occupy two indices exactly as the JVMS requires.
#[derive(Clone, Debug)]
enum CpEntry {
    /// Index 0 and the second slot of `Long`/`Double`.
    Vacant,
    Utf8(String),
    Integer(i32),
    Float(f32),
    /// `CONSTANT_Long` — payload consumed, value unused by the subset.
    Long,
    /// `CONSTANT_Double` — payload consumed, value unused by the subset.
    Double,
    Class {
        name_index: u16,
    },
    String {
        utf8_index: u16,
    },
    Methodref {
        class_index: u16,
        nat_index: u16,
    },
    InterfaceMethodref {
        class_index: u16,
        nat_index: u16,
    },
    /// Fieldref — parsed structurally, never resolvable by the subset.
    Fieldref,
    NameAndType {
        name_index: u16,
        descriptor_index: u16,
    },
    /// MethodHandle/MethodType/Dynamic/InvokeDynamic/Module/Package.
    Other,
}

#[derive(Clone, Debug)]
struct ConstantPool(Vec<CpEntry>);

impl ConstantPool {
    fn entry(&self, index: u16, ctx: &'static str) -> Result<&CpEntry, VmError> {
        self.0
            .get(usize::from(index))
            .ok_or(VmError::BadConstantPoolIndex(ctx))
    }

    fn utf8(&self, index: u16, ctx: &'static str) -> Result<&str, VmError> {
        match self.entry(index, ctx)? {
            CpEntry::Utf8(s) => Ok(s),
            _ => Err(VmError::BadConstantPoolTag(ctx)),
        }
    }

    fn class_name(&self, index: u16, ctx: &'static str) -> Result<&str, VmError> {
        match self.entry(index, ctx)? {
            CpEntry::Class { name_index } => self.utf8(*name_index, ctx),
            _ => Err(VmError::BadConstantPoolTag(ctx)),
        }
    }

    /// `(class_name, method_name, descriptor)` for a `Methodref` or
    /// `InterfaceMethodref` constant-pool slot.
    fn method_ref(&self, index: u16) -> Result<(&str, &str, &str), VmError> {
        let (class_index, nat_index) = match self.entry(index, "invoke target")? {
            CpEntry::Methodref {
                class_index,
                nat_index,
            }
            | CpEntry::InterfaceMethodref {
                class_index,
                nat_index,
            } => (*class_index, *nat_index),
            _ => {
                return Err(VmError::BadConstantPoolTag(
                    "invoke target is not a method reference",
                ))
            }
        };
        let class = self.class_name(class_index, "invoke owner class")?;
        let (name_index, descriptor_index) = match self.entry(nat_index, "invoke name_and_type")? {
            CpEntry::NameAndType {
                name_index,
                descriptor_index,
            } => (*name_index, *descriptor_index),
            _ => return Err(VmError::BadConstantPoolTag("invoke target name_and_type")),
        };
        let name = self.utf8(name_index, "invoke method name")?;
        let descriptor = self.utf8(descriptor_index, "invoke method descriptor")?;
        Ok((class, name, descriptor))
    }
}

/// Decode one `CONSTANT_Utf8` payload — modified UTF-8: `0xC0 0x80` is
/// NUL, supplementary characters arrive as paired UTF-16 surrogates,
/// and a bare `0x00` or broken continuation is a hard error.
fn mutf8_decode(bytes: &[u8]) -> Result<String, VmError> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            return Err(VmError::InvalidUtf8);
        } else if b < 0x80 {
            units.push(u16::from(b));
            i += 1;
        } else if b >> 5 == 0b110 {
            let Some(&b2) = bytes.get(i + 1) else {
                return Err(VmError::InvalidUtf8);
            };
            if b2 & 0xC0 != 0x80 {
                return Err(VmError::InvalidUtf8);
            }
            units.push((u16::from(b & 0x1F) << 6) | u16::from(b2 & 0x3F));
            i += 2;
        } else if b >> 4 == 0b1110 {
            let (Some(&b2), Some(&b3)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                return Err(VmError::InvalidUtf8);
            };
            if b2 & 0xC0 != 0x80 || b3 & 0xC0 != 0x80 {
                return Err(VmError::InvalidUtf8);
            }
            units.push(
                (u16::from(b & 0x0F) << 12) | (u16::from(b2 & 0x3F) << 6) | u16::from(b3 & 0x3F),
            );
            i += 3;
        } else {
            return Err(VmError::InvalidUtf8);
        }
    }
    String::from_utf16(&units).map_err(|_| VmError::InvalidUtf8)
}

fn read_constant_pool(r: &mut Cursor<'_>, count: u16) -> Result<ConstantPool, VmError> {
    let mut entries = Vec::with_capacity(usize::from(count).min(MAX_CONSTANT_POOL));
    entries.push(CpEntry::Vacant);
    let mut index = 1u32;
    while index < u32::from(count) {
        let tag = r.u1("constant pool tag")?;
        let entry = match tag {
            1 => {
                let len = usize::from(r.u2("CONSTANT_Utf8 length")?);
                CpEntry::Utf8(mutf8_decode(r.take(len, "CONSTANT_Utf8 bytes")?)?)
            }
            3 => CpEntry::Integer(r.u4("CONSTANT_Integer")? as i32),
            4 => CpEntry::Float(f32::from_bits(r.u4("CONSTANT_Float")?)),
            5 => {
                r.skip(8, "CONSTANT_Long")?;
                CpEntry::Long
            }
            6 => {
                r.skip(8, "CONSTANT_Double")?;
                CpEntry::Double
            }
            7 => CpEntry::Class {
                name_index: r.u2("CONSTANT_Class name_index")?,
            },
            8 => CpEntry::String {
                utf8_index: r.u2("CONSTANT_String string_index")?,
            },
            9 => {
                r.skip(4, "CONSTANT_Fieldref")?;
                CpEntry::Fieldref
            }
            10 => CpEntry::Methodref {
                class_index: r.u2("CONSTANT_Methodref class_index")?,
                nat_index: r.u2("CONSTANT_Methodref name_and_type_index")?,
            },
            11 => CpEntry::InterfaceMethodref {
                class_index: r.u2("CONSTANT_InterfaceMethodref class_index")?,
                nat_index: r.u2("CONSTANT_InterfaceMethodref name_and_type_index")?,
            },
            12 => CpEntry::NameAndType {
                name_index: r.u2("CONSTANT_NameAndType name_index")?,
                descriptor_index: r.u2("CONSTANT_NameAndType descriptor_index")?,
            },
            15 => {
                r.skip(3, "CONSTANT_MethodHandle")?;
                CpEntry::Other
            }
            16 | 19 | 20 => {
                r.skip(2, "CONSTANT_MethodType/Module/Package")?;
                CpEntry::Other
            }
            17 | 18 => {
                r.skip(4, "CONSTANT_Dynamic/InvokeDynamic")?;
                CpEntry::Other
            }
            other => return Err(VmError::UnsupportedConstantPoolTag(other)),
        };
        entries.push(entry);
        index += 1;
        if matches!(entries.last(), Some(CpEntry::Long | CpEntry::Double)) {
            // Long and Double occupy two constant-pool slots each.
            entries.push(CpEntry::Vacant);
            index += 1;
        }
    }
    Ok(ConstantPool(entries))
}

/// One parsed method's `Code` attribute payload.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedMethod {
    /// Method name (`CONSTANT_Utf8` at `name_index`).
    pub name: String,
    /// Method descriptor, e.g. `(Lmcspine/api/Event;)V`.
    pub descriptor: String,
    /// `access_flags` — `ACC_STATIC` etc. Informational: the subset
    /// executes instance and static methods identically, seeding locals
    /// from `args` either way.
    pub access_flags: u16,
    /// Declared `max_stack` from the `Code` attribute.
    pub max_stack: u16,
    /// Declared `max_locals` from the `Code` attribute.
    pub max_locals: u16,
    /// Raw `code` bytes.
    pub code: Vec<u8>,
}

/// The methods of one parsed `.class`, plus everything the verifier
/// needs to resolve their constant-pool references.
#[derive(Clone, Debug)]
pub struct ParsedClassMethods {
    /// Internal name of this class (`this_class`).
    pub class_name: String,
    /// Every `method_info` that carried a `Code` attribute. A method
    /// listed here always has code; abstract/native methods are omitted
    /// (verification reports [`VmError::MethodNotFound`] for them).
    pub methods: Vec<ParsedMethod>,
    cp: ConstantPool,
}

/// Skip an `attribute_info` table (fields, `Exceptions`, etc.) by
/// declared lengths — bounded by [`MAX_ATTRIBUTES`] per table.
fn skip_attributes(r: &mut Cursor<'_>, what: &'static str) -> Result<(), VmError> {
    let count = usize::from(r.u2("attributes_count")?);
    if count > MAX_ATTRIBUTES {
        return Err(VmError::LimitExceeded(what));
    }
    for _ in 0..count {
        r.skip(2, "attribute_name_index")?;
        let len = r.u4("attribute_length")? as usize;
        r.skip(len, "attribute payload")?;
    }
    Ok(())
}

/// Parse one JVM `.class` file (JVMS §4) into [`ParsedClassMethods`].
///
/// Full structural parse — constant pool (all standard tags, dual-slot
/// `Long`/`Double`), `this_class`, fields, and every `method_info`'s
/// `Code` attribute (`max_stack`, `max_locals`, raw `code` bytes). Any
/// other attribute is skipped by declared length. Fail closed on bad
/// magic, truncation, oversized counts, and malformed UTF-8.
pub fn parse_class_methods(bytes: &[u8]) -> Result<ParsedClassMethods, VmError> {
    let mut r = Cursor::new(bytes);
    if r.u4("magic")? != 0xCAFE_BABE {
        return Err(VmError::BadMagic);
    }
    let _minor = r.u2("minor_version")?;
    let _major = r.u2("major_version")?;
    let cp_count = r.u2("constant_pool_count")?;
    if cp_count == 0 {
        return Err(VmError::Malformed("empty constant pool"));
    }
    if usize::from(cp_count) > MAX_CONSTANT_POOL {
        return Err(VmError::LimitExceeded("constant_pool_count"));
    }
    let cp = read_constant_pool(&mut r, cp_count)?;

    let _access_flags = r.u2("access_flags")?;
    let class_name = cp
        .class_name(r.u2("this_class")?, "this_class")?
        .to_string();
    let _super_class = r.u2("super_class")?;
    let interfaces = usize::from(r.u2("interfaces_count")?);
    if interfaces > MAX_INTERFACES {
        return Err(VmError::LimitExceeded("interfaces_count"));
    }
    r.skip(
        interfaces
            .checked_mul(2)
            .ok_or(VmError::Malformed("interfaces overflow"))?,
        "interfaces",
    )?;

    let fields = usize::from(r.u2("fields_count")?);
    if fields > MAX_FIELDS {
        return Err(VmError::LimitExceeded("fields_count"));
    }
    for _ in 0..fields {
        r.skip(6, "field_info")?;
        skip_attributes(&mut r, "field attributes_count")?;
    }

    let methods_count = usize::from(r.u2("methods_count")?);
    if methods_count > MAX_METHODS {
        return Err(VmError::LimitExceeded("methods_count"));
    }
    let mut methods = Vec::with_capacity(methods_count.min(64));
    for _ in 0..methods_count {
        let access_flags = r.u2("method access_flags")?;
        let name = cp
            .utf8(r.u2("method name_index")?, "method name")?
            .to_string();
        let descriptor = cp
            .utf8(r.u2("method descriptor_index")?, "method descriptor")?
            .to_string();
        let attr_count = usize::from(r.u2("method attributes_count")?);
        if attr_count > MAX_ATTRIBUTES {
            return Err(VmError::LimitExceeded("method attributes_count"));
        }
        let mut code: Option<(u16, u16, Vec<u8>)> = None;
        for _ in 0..attr_count {
            let attr_name = cp.utf8(r.u2("attribute_name_index")?, "attribute name")?;
            let len = r.u4("attribute_length")? as usize;
            let payload = r.take(len, "method attribute payload")?;
            if attr_name != "Code" {
                continue;
            }
            if code.is_some() {
                return Err(VmError::Malformed("duplicate Code attribute"));
            }
            let mut c = Cursor::new(payload);
            let max_stack = c.u2("Code.max_stack")?;
            let max_locals = c.u2("Code.max_locals")?;
            let code_length = c.u4("Code.code_length")? as usize;
            if code_length == 0 {
                return Err(VmError::Malformed("empty Code.code"));
            }
            let code_bytes = c.take(code_length, "Code.code")?.to_vec();
            // Exception table + nested attributes: structural skip,
            // bounded by the enclosing attribute's declared length.
            let exn_count = usize::from(c.u2("Code.exception_table_length")?);
            c.skip(
                exn_count
                    .checked_mul(8)
                    .ok_or(VmError::Malformed("exception table overflow"))?,
                "Code.exception_table",
            )?;
            let nested = usize::from(c.u2("Code.attributes_count")?);
            if nested > MAX_ATTRIBUTES {
                return Err(VmError::LimitExceeded("Code attributes_count"));
            }
            for _ in 0..nested {
                c.skip(2, "Code attribute name_index")?;
                let alen = c.u4("Code attribute_length")? as usize;
                c.skip(alen, "Code attribute payload")?;
            }
            if c.pos != payload.len() {
                return Err(VmError::Malformed("trailing bytes in Code attribute"));
            }
            code = Some((max_stack, max_locals, code_bytes));
        }
        if let Some((max_stack, max_locals, code_bytes)) = code {
            methods.push(ParsedMethod {
                name,
                descriptor,
                access_flags,
                max_stack,
                max_locals,
                code: code_bytes,
            });
        }
    }

    skip_attributes(&mut r, "class attributes_count")?;
    if r.pos != bytes.len() {
        return Err(VmError::Malformed("trailing bytes after class file"));
    }
    Ok(ParsedClassMethods {
        class_name,
        methods,
        cp,
    })
}

// ---------------------------------------------------------------------------
// Instruction decode & static verifier
// ---------------------------------------------------------------------------

/// A `ldc`-loadable constant (subset: `CONSTANT_Integer`/`Float`/`String`
/// only — `ldc` of a `Class` literal is reflection and stays out).
#[derive(Clone, Debug, PartialEq)]
pub enum LdcConst {
    Int(i32),
    Float(f32),
    Str(String),
}

/// Comparison operators for the `if*` family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Ge,
    Gt,
    Le,
}

/// One verified instruction. Branch targets are stored as indexes into
/// [`VerifiedMethod::insns`]; the verifier has already proven each one
/// lands on an instruction boundary inside the code array.
#[derive(Clone, Debug)]
enum Insn {
    Nop,
    IConst(i32),
    Ldc(LdcConst),
    FConst(f32),
    ILoad(usize),
    FLoad(usize),
    ALoad(usize),
    IStore(usize),
    FStore(usize),
    AStore(usize),
    Pop,
    Dup,
    Swap,
    IAdd,
    ISub,
    IMul,
    IDiv,
    IRem,
    INeg,
    IInc { slot: usize, delta: i32 },
    FAdd,
    FSub,
    FMul,
    FDiv,
    FNeg,
    I2F,
    F2I,
    FCmpL,
    FCmpG,
    If { op: CmpOp, target: usize },
    IfICmp { op: CmpOp, target: usize },
    Goto { target: usize },
    Invoke(HostCallKind),
    Return,
    IReturn,
    FReturn,
}

impl Insn {
    /// Successor instruction indexes for control flow. Fall-through is
    /// represented as `Some(pc + 1)` by the caller — an instruction whose
    /// `falls_through` is `true` has `pc + 1` as a successor when `pc`
    /// is not the last index.
    fn falls_through(&self) -> bool {
        !matches!(
            self,
            Self::Goto { .. } | Self::Return | Self::IReturn | Self::FReturn
        )
    }

    /// `(min_stack_required, depth_delta)` for the operand stack.
    fn stack_effect(&self) -> (usize, i32) {
        match self {
            Self::Nop | Self::IInc { .. } => (0, 0),
            Self::IConst(_) | Self::Ldc(_) | Self::FConst(_) => (0, 1),
            Self::ILoad(_) | Self::FLoad(_) | Self::ALoad(_) => (0, 1),
            Self::IStore(_) | Self::FStore(_) | Self::AStore(_) => (1, -1),
            Self::Pop => (1, -1),
            Self::Dup => (1, 1),
            Self::Swap => (2, 0),
            Self::IAdd
            | Self::ISub
            | Self::IMul
            | Self::IDiv
            | Self::IRem
            | Self::FAdd
            | Self::FSub
            | Self::FMul
            | Self::FDiv
            | Self::FCmpL
            | Self::FCmpG => (2, -1),
            Self::INeg | Self::FNeg | Self::I2F | Self::F2I => (1, 0),
            Self::If { .. } => (1, -1),
            Self::IfICmp { .. } => (2, -2),
            Self::Goto { .. } => (0, 0),
            Self::Invoke(kind) => {
                let sig = kind.signature();
                let pops = sig.params.len() + usize::from(sig.receiver.is_some());
                (pops, -(pops as i32) + i32::from(sig.result.is_some()))
            }
            Self::Return => (0, 0),
            Self::IReturn | Self::FReturn => (1, -1),
        }
    }
}

// The subset opcode table lives inside `decode_insn` (further below) as
// one big `match`. Anything absent from it is `VmError::DisallowedOpcode`.
// Deliberately excluded even when JVMS-valid: `aconst_null`, `ldc_w`/
// `ldc2_w`, `l*`/`d*` arithmetic and conversions, every array opcode,
// `get*`/`put*` field access, `if_acmp*`/`ifnull`/`ifnonnull`,
// `tableswitch`/`lookupswitch`, `goto_w`/`jsr`/`ret`, `areturn`/`lreturn`/
// `dreturn`, `athrow`, `invokespecial`/`invokedynamic`, `new`/
// `anewarray`/`newarray`/`multianewarray`, `checkcast`/`instanceof`,
// `monitorenter`/`monitorexit`, and `wide`.
enum DecodeError {
    Vm(VmError),
}

type DResult<T> = Result<T, DecodeError>;

fn verr(e: VmError) -> DecodeError {
    DecodeError::Vm(e)
}

/// Decoded result of one instruction: the [`Insn`] plus its byte width.
struct Decoded {
    insn: Insn,
    width: usize,
}

fn operand_u1_at(code: &[u8], at: usize, i: usize) -> DResult<u8> {
    code.get(at + 1 + i)
        .copied()
        .ok_or_else(|| verr(VmError::TruncatedInstruction { offset: at }))
}

impl From<VmError> for DecodeError {
    fn from(e: VmError) -> Self {
        DecodeError::Vm(e)
    }
}

impl From<DecodeError> for VmError {
    fn from(e: DecodeError) -> Self {
        match e {
            DecodeError::Vm(v) => v,
        }
    }
}

/// A method that survived the full static verifier: every opcode is
/// allowlisted, every operand and branch target is in bounds and on an
/// instruction boundary, every `invoke*` resolves to a [`HostCallKind`],
/// and a dataflow pass proves operand-stack depth never exceeds
/// `max_stack` and agrees at every merge. `insns` is in decode order;
/// `offsets[i]` is the JVMS byte offset of `insns[i]`.
#[derive(Clone, Debug)]
pub struct VerifiedMethod {
    /// Method name.
    pub name: String,
    /// Method descriptor.
    pub descriptor: String,
    /// Declared `max_locals`; locals are zero-initialized to `None`.
    pub max_locals: usize,
    /// Declared `max_stack` (already proven `<= MAX_OPERAND_STACK`).
    pub max_stack: usize,
    /// Original `code_length`.
    pub code_length: usize,
    insns: Vec<Insn>,
    offsets: Vec<usize>,
}

impl VerifiedMethod {
    /// Number of decoded instructions.
    pub fn instruction_count(&self) -> usize {
        self.insns.len()
    }

    /// JVMS byte offsets of each decoded instruction, in order.
    pub fn instruction_offsets(&self) -> &[usize] {
        &self.offsets
    }
}

/// Statically verify `class`'s `(name, descriptor)` method for subset
/// execution. Fails closed on: method missing or codeless; `code_length`
/// `>` [`MAX_CODE_LENGTH`]; `max_stack` `>` [`MAX_OPERAND_STACK`];
/// `max_locals` `>` [`MAX_LOCAL_SLOTS`]; any opcode outside the subset
/// table ([`VmError::DisallowedOpcode`] — this is where `new`,
/// `anewarray`, `athrow`, `monitorenter`, `invokedynamic`, field access,
/// `invokespecial`, `tableswitch`, `areturn`, ... all die); any operand
/// running past `code_length`; any constant-pool index out of range or
/// of the wrong tag; any `ldc` of a non-`Integer`/`Float`/`String`
/// constant; any `invoke*` target that is forbidden ([`VmError::ForbiddenHostClass`]
/// — `java/lang/Runtime`, `java/lang/ProcessBuilder`, `javax/naming/*`,
/// `java/lang/reflect/*`, `java/io/*`, `java/net/*`, `sun/*`, ...) or
/// simply not allowlisted ([`VmError::UnknownHostCall`] /
/// [`VmError::HostSignatureMismatch`]); any branch target outside the
/// code array or landing inside a multi-byte instruction
/// ([`VmError::BadBranchTarget`]); fall-through off the end of the code;
/// and any provable operand-stack underflow, depth beyond `max_stack`,
/// or merge with inconsistent depth.
///
/// Verification runs entirely before execution — a method that returns
/// `Err` here never runs a single instruction.
pub fn verify_subset_method(
    class: &ParsedClassMethods,
    name: &str,
    descriptor: &str,
) -> Result<VerifiedMethod, VmError> {
    let sig = format!("{name}{descriptor}");
    let method = class
        .methods
        .iter()
        .find(|m| m.name == name && m.descriptor == descriptor)
        .ok_or(VmError::MethodNotFound(sig.clone()))?;
    if method.code.is_empty() {
        return Err(VmError::MethodHasNoCode(sig));
    }
    if method.code.len() > MAX_CODE_LENGTH {
        return Err(VmError::CodeTooLong {
            length: method.code.len(),
            max: MAX_CODE_LENGTH,
        });
    }
    if usize::from(method.max_stack) > MAX_OPERAND_STACK {
        return Err(VmError::StackTooDeep {
            max_stack: usize::from(method.max_stack),
            max: MAX_OPERAND_STACK,
        });
    }
    if usize::from(method.max_locals) > MAX_LOCAL_SLOTS {
        return Err(VmError::TooManyLocals {
            max_locals: usize::from(method.max_locals),
            max: MAX_LOCAL_SLOTS,
        });
    }

    // Pass 1: linear decode. Build the instruction list and the
    // byte-offset → instruction-index boundary map in the same sweep so
    // the branch pass can prove every target lands on an instruction.
    let code = &method.code;
    let mut insns: Vec<Insn> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    let mut offset_to_index: HashMap<usize, usize> = HashMap::new();
    let mut branches: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0usize;
    while pos < code.len() {
        offset_to_index.insert(pos, insns.len());
        // Forward branch targets are not yet decodable as instruction
        // indexes, so decode records raw byte targets; pass 2 resolves
        // them against the completed boundary map.
        let decoded = decode_insn(code, pos, &class.cp, &mut branches).map_err(VmError::from)?;
        offsets.push(pos);
        insns.push(decoded.insn);
        pos += decoded.width;
    }
    if insns.is_empty() {
        return Err(VmError::Malformed("empty method code"));
    }

    // Pass 2: resolve recorded branch targets to instruction indexes.
    // A target outside the code array or not on an instruction boundary
    // (inside a multi-byte instruction) is a hard failure.
    for (at, target) in &branches {
        let target = *target;
        let Some(&ti) = offset_to_index.get(&target) else {
            return Err(VmError::BadBranchTarget {
                offset: *at,
                target: target as i64,
            });
        };
        match &mut insns[offset_to_index[at]] {
            Insn::If { target: t, .. }
            | Insn::IfICmp { target: t, .. }
            | Insn::Goto { target: t } => {
                *t = ti;
            }
            _ => unreachable!("branch recorded for non-branch instruction"),
        }
    }

    // Pass 3: dataflow stack-depth proof. Each instruction gets a
    // proven entry depth; merges must agree exactly, depth must never
    // exceed max_stack, and no path may fall off the end of the code.
    let mut depth: Vec<Option<usize>> = vec![None; insns.len()];
    let mut worklist: Vec<usize> = vec![0];
    depth[0] = Some(0);
    while let Some(pc) = worklist.pop() {
        let entry = depth[pc].expect("worklist only holds assigned depths");
        let (need, delta) = insns[pc].stack_effect();
        if entry < need {
            return Err(VmError::VerifyStackUnderflow {
                offset: offsets[pc],
            });
        }
        let exit = (entry as i32 + delta) as usize;
        if exit > usize::from(method.max_stack) {
            return Err(VmError::VerifyStackOverflow {
                offset: offsets[pc],
            });
        }
        let mut successors: Vec<usize> = Vec::with_capacity(2);
        match &insns[pc] {
            Insn::If { target, .. } | Insn::IfICmp { target, .. } => {
                successors.push(*target);
                successors.push(pc + 1);
            }
            Insn::Goto { target } => successors.push(*target),
            _ if insns[pc].falls_through() => successors.push(pc + 1),
            _ => {}
        }
        for succ in successors {
            if succ >= insns.len() {
                return Err(VmError::VerifyFallThrough {
                    offset: offsets[pc],
                });
            }
            match depth[succ] {
                Some(prev) if prev != exit => {
                    return Err(VmError::VerifyStackMerge {
                        offset: offsets[succ],
                        depth_a: prev,
                        depth_b: exit,
                    });
                }
                Some(_) => {}
                None => {
                    depth[succ] = Some(exit);
                    worklist.push(succ);
                }
            }
        }
    }

    // Pass 4: local-slot bounds — every load/store/iinc slot must be
    // inside the declared max_locals.
    for insn in &insns {
        let slot = match insn {
            Insn::ILoad(s)
            | Insn::FLoad(s)
            | Insn::ALoad(s)
            | Insn::IStore(s)
            | Insn::FStore(s)
            | Insn::AStore(s)
            | Insn::IInc { slot: s, .. } => Some(*s),
            _ => None,
        };
        if let Some(s) = slot {
            if s >= usize::from(method.max_locals) {
                return Err(VmError::LocalIndexOutOfBounds { index: s });
            }
        }
    }

    Ok(VerifiedMethod {
        name: method.name.clone(),
        descriptor: method.descriptor.clone(),
        max_locals: usize::from(method.max_locals),
        max_stack: usize::from(method.max_stack),
        code_length: code.len(),
        insns,
        offsets,
    })
}

/// Two-pass decode: pass 1 knows only byte offsets (the instruction
/// boundary map is still being built), so branch operands are recorded
/// as raw byte targets in `branches` and resolved in pass 2. Operand
/// *bounds* (`in-range` + on a code offset) are checked in pass 2.
fn decode_insn(
    code: &[u8],
    offset: usize,
    cp: &ConstantPool,
    branches: &mut Vec<(usize, usize)>,
) -> DResult<Decoded> {
    let opcode = code[offset];
    let at = offset;
    let operand_u1 = |i: usize| -> DResult<u8> { operand_u1_at(code, at, i) };
    let operand_u2 = || -> DResult<u16> {
        let hi = operand_u1(0)?;
        let lo = operand_u1(1)?;
        Ok(u16::from_be_bytes([hi, lo]))
    };
    let operand_s2 = || -> DResult<i16> { Ok(operand_u2()? as i16) };

    Ok(match opcode {
        0x00 => Decoded {
            insn: Insn::Nop,
            width: 1,
        },
        0x02..=0x08 => Decoded {
            insn: Insn::IConst(i32::from(opcode) - 3),
            width: 1,
        },
        0x0B..=0x0D => Decoded {
            insn: Insn::FConst(f32::from(opcode - 0x0B)),
            width: 1,
        },
        0x10 => Decoded {
            insn: Insn::IConst(i32::from(operand_u1(0)? as i8)),
            width: 2,
        },
        0x11 => Decoded {
            insn: Insn::IConst(i32::from(operand_s2()?)),
            width: 3,
        },
        0x12 => {
            let index = u16::from(operand_u1(0)?);
            let insn = match cp.entry(index, "ldc constant")? {
                CpEntry::Integer(v) => Insn::Ldc(LdcConst::Int(*v)),
                CpEntry::Float(v) => Insn::Ldc(LdcConst::Float(*v)),
                CpEntry::String { utf8_index } => Insn::Ldc(LdcConst::Str(
                    cp.utf8(*utf8_index, "ldc string")?.to_string(),
                )),
                _ => {
                    return Err(verr(VmError::BadConstantKind {
                        offset: at,
                        ctx: "ldc (only int/float/string constants load)",
                    }))
                }
            };
            Decoded { insn, width: 2 }
        }
        0x15 => Decoded {
            insn: Insn::ILoad(usize::from(operand_u1(0)?)),
            width: 2,
        },
        0x17 => Decoded {
            insn: Insn::FLoad(usize::from(operand_u1(0)?)),
            width: 2,
        },
        0x19 => Decoded {
            insn: Insn::ALoad(usize::from(operand_u1(0)?)),
            width: 2,
        },
        0x1A..=0x1D => Decoded {
            insn: Insn::ILoad(usize::from(opcode) - 0x1A),
            width: 1,
        },
        0x22..=0x25 => Decoded {
            insn: Insn::FLoad(usize::from(opcode) - 0x22),
            width: 1,
        },
        0x2A..=0x2D => Decoded {
            insn: Insn::ALoad(usize::from(opcode) - 0x2A),
            width: 1,
        },
        0x36 => Decoded {
            insn: Insn::IStore(usize::from(operand_u1(0)?)),
            width: 2,
        },
        0x38 => Decoded {
            insn: Insn::FStore(usize::from(operand_u1(0)?)),
            width: 2,
        },
        0x3A => Decoded {
            insn: Insn::AStore(usize::from(operand_u1(0)?)),
            width: 2,
        },
        0x3B..=0x3E => Decoded {
            insn: Insn::IStore(usize::from(opcode) - 0x3B),
            width: 1,
        },
        0x43..=0x46 => Decoded {
            insn: Insn::FStore(usize::from(opcode) - 0x43),
            width: 1,
        },
        0x4B..=0x4E => Decoded {
            insn: Insn::AStore(usize::from(opcode) - 0x4B),
            width: 1,
        },
        0x57 => Decoded {
            insn: Insn::Pop,
            width: 1,
        },
        0x59 => Decoded {
            insn: Insn::Dup,
            width: 1,
        },
        0x5F => Decoded {
            insn: Insn::Swap,
            width: 1,
        },
        0x60 => Decoded {
            insn: Insn::IAdd,
            width: 1,
        },
        0x64 => Decoded {
            insn: Insn::ISub,
            width: 1,
        },
        0x68 => Decoded {
            insn: Insn::IMul,
            width: 1,
        },
        0x6C => Decoded {
            insn: Insn::IDiv,
            width: 1,
        },
        0x70 => Decoded {
            insn: Insn::IRem,
            width: 1,
        },
        0x74 => Decoded {
            insn: Insn::INeg,
            width: 1,
        },
        0x84 => {
            let slot = usize::from(operand_u1(0)?);
            let delta = i32::from(operand_u1(1)? as i8);
            Decoded {
                insn: Insn::IInc { slot, delta },
                width: 3,
            }
        }
        0x62 => Decoded {
            insn: Insn::FAdd,
            width: 1,
        },
        0x66 => Decoded {
            insn: Insn::FSub,
            width: 1,
        },
        0x6A => Decoded {
            insn: Insn::FMul,
            width: 1,
        },
        0x6E => Decoded {
            insn: Insn::FDiv,
            width: 1,
        },
        0x76 => Decoded {
            insn: Insn::FNeg,
            width: 1,
        },
        0x85 => Decoded {
            insn: Insn::I2F,
            width: 1,
        },
        0x86 => Decoded {
            insn: Insn::F2I,
            width: 1,
        },
        0x94 => Decoded {
            insn: Insn::FCmpL,
            width: 1,
        },
        0x95 => Decoded {
            insn: Insn::FCmpG,
            width: 1,
        },
        0x99..=0x9E | 0x9F..=0xA4 | 0xA7 => {
            let delta = operand_s2()?;
            let target = at as i64 + i64::from(delta);
            if !(0..code.len() as i64).contains(&target) {
                return Err(verr(VmError::BadBranchTarget { offset: at, target }));
            }
            branches.push((at, target as usize));
            let op = match opcode {
                0x99 | 0x9F => CmpOp::Eq,
                0x9A | 0xA0 => CmpOp::Ne,
                0x9B | 0xA1 => CmpOp::Lt,
                0x9C | 0xA2 => CmpOp::Ge,
                0x9D | 0xA3 => CmpOp::Gt,
                _ => CmpOp::Le,
            };
            let insn = match opcode {
                0x99..=0x9E => Insn::If {
                    op,
                    target: usize::MAX,
                },
                0x9F..=0xA4 => Insn::IfICmp {
                    op,
                    target: usize::MAX,
                },
                _ => Insn::Goto { target: usize::MAX },
            };
            Decoded { insn, width: 3 }
        }
        0xAC => Decoded {
            insn: Insn::IReturn,
            width: 1,
        },
        0xAE => Decoded {
            insn: Insn::FReturn,
            width: 1,
        },
        0xB1 => Decoded {
            insn: Insn::Return,
            width: 1,
        },
        0xB6 | 0xB8 | 0xB9 => {
            let index = operand_u2()?;
            let (class, name, descriptor) = cp.method_ref(index)?;
            match resolve_host_call(class, name, descriptor) {
                HostResolution::Forbidden => {
                    return Err(verr(VmError::ForbiddenHostClass {
                        offset: at,
                        class: class.to_string(),
                    }))
                }
                HostResolution::SignatureMismatch => {
                    return Err(verr(VmError::HostSignatureMismatch {
                        offset: at,
                        class: class.to_string(),
                        method: name.to_string(),
                        descriptor: descriptor.to_string(),
                    }))
                }
                HostResolution::Unknown => {
                    return Err(verr(VmError::UnknownHostCall {
                        offset: at,
                        class: class.to_string(),
                        method: name.to_string(),
                        descriptor: descriptor.to_string(),
                    }))
                }
                HostResolution::Kind(kind) => {
                    let width = if opcode == 0xB9 {
                        // invokeinterface operands: indexbyte1 indexbyte2
                        // count 0 — indexes 0,1 are the u2 index above.
                        let count = operand_u1(2)?;
                        let zero = operand_u1(3)?;
                        let expected = kind.signature().params.len() + 1;
                        if usize::from(count) != expected || zero != 0 {
                            return Err(verr(VmError::BadInterfaceCount {
                                offset: at,
                                count,
                                expected,
                            }));
                        }
                        5
                    } else {
                        3
                    };
                    Decoded {
                        insn: Insn::Invoke(kind),
                        width,
                    }
                }
            }
        }
        other => {
            return Err(verr(VmError::DisallowedOpcode {
                offset: at,
                opcode: other,
            }))
        }
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// The result of one bounded method execution.
#[derive(Clone, Debug, PartialEq)]
pub struct VmResult {
    /// `Some` for `ireturn`/`freturn`, `None` for `return`.
    pub return_value: Option<VmValue>,
    /// Instructions executed before the method returned (or the fuel
    /// counter reached when [`VmError::OutOfFuel`] was raised).
    pub instructions_executed: u32,
}

/// Run a [`VerifiedMethod`] on the bounded stack VM.
///
/// `args` seed local slots `0..args.len()` — for an event hook, pass
/// `[VmValue::Ref(event_handle)]` so `aload_0` picks the event up.
/// `fuel` is a strict instruction budget ([`DEFAULT_FUEL`] is the normal
/// value): every executed instruction costs one, and the `fuel`th-plus-
/// one instruction fails with [`VmError::OutOfFuel`], so `goto 0` burns
/// exactly `fuel` steps and stops. Integer arithmetic wraps exactly like
/// the JVM (`wrapping_*`); `idiv`/`irem` by zero are
/// [`VmError::ArithmeticDivByZero`], never a panic; `f2i` saturates like
/// Java; `fcmpl`/`fcmpg` implement the JVMS NaN polarity (`fcmpl` pushes
/// `-1`, `fcmpg` pushes `1`). Host calls go through `host` and only
/// through `host`, with argument and result kinds checked against the
/// resolved [`HostCallKind`] signature.
pub fn execute_verified_method(
    method: &VerifiedMethod,
    args: &[VmValue],
    host: &mut dyn SubsetVmHost,
    fuel: u32,
) -> Result<VmResult, VmError> {
    let mut locals: Vec<Option<VmValue>> = vec![None; method.max_locals];
    for (i, arg) in args.iter().enumerate() {
        let slot = locals
            .get_mut(i)
            .ok_or(VmError::LocalIndexOutOfBounds { index: i })?;
        *slot = Some(arg.clone());
    }
    let mut stack: Vec<VmValue> = Vec::with_capacity(method.max_stack.min(16));
    let mut pc = 0usize;
    let mut executed = 0u32;

    macro_rules! pop {
        () => {
            stack.pop().ok_or(VmError::StackUnderflow)?
        };
    }
    macro_rules! pop_int {
        () => {
            match pop!() {
                VmValue::Int(v) => v,
                _ => return Err(VmError::TypeMismatch("expected Int operand")),
            }
        };
    }
    macro_rules! pop_float {
        () => {
            match pop!() {
                VmValue::Float(v) => v,
                _ => return Err(VmError::TypeMismatch("expected Float operand")),
            }
        };
    }
    macro_rules! push {
        ($v:expr) => {{
            stack.push($v);
            if stack.len() > method.max_stack {
                return Err(VmError::VerifyStackOverflow { offset: 0 });
            }
        }};
    }
    macro_rules! load {
        ($slot:expr) => {
            match locals.get($slot) {
                Some(Some(v)) => v.clone(),
                Some(None) => return Err(VmError::TypeMismatch("uninitialized local")),
                None => return Err(VmError::LocalIndexOutOfBounds { index: $slot }),
            }
        };
    }
    macro_rules! store {
        ($slot:expr, $v:expr) => {
            match locals.get_mut($slot) {
                Some(cell) => *cell = Some($v),
                None => return Err(VmError::LocalIndexOutOfBounds { index: $slot }),
            }
        };
    }

    loop {
        if executed >= fuel {
            return Err(VmError::OutOfFuel);
        }
        let insn = method
            .insns
            .get(pc)
            .ok_or(VmError::Malformed("pc out of bounds"))?;
        executed += 1;
        let mut next = pc + 1;
        match insn {
            Insn::Nop => {}
            Insn::IConst(v) => push!(VmValue::Int(*v)),
            Insn::FConst(v) => push!(VmValue::Float(*v)),
            Insn::Ldc(c) => push!(match c {
                LdcConst::Int(v) => VmValue::Int(*v),
                LdcConst::Float(v) => VmValue::Float(*v),
                LdcConst::Str(s) => VmValue::Str(s.clone()),
            }),
            Insn::ILoad(slot) => {
                let v = load!(*slot);
                if v.ty() != VmType::Int {
                    return Err(VmError::TypeMismatch("iload on non-Int local"));
                }
                push!(v);
            }
            Insn::FLoad(slot) => {
                let v = load!(*slot);
                if v.ty() != VmType::Float {
                    return Err(VmError::TypeMismatch("fload on non-Float local"));
                }
                push!(v);
            }
            Insn::ALoad(slot) => {
                let v = load!(*slot);
                if !v.is_ref_kind() {
                    return Err(VmError::TypeMismatch("aload on non-reference local"));
                }
                push!(v);
            }
            Insn::IStore(slot) => {
                let v = pop!();
                if v.ty() != VmType::Int {
                    return Err(VmError::TypeMismatch("istore of non-Int value"));
                }
                store!(*slot, v);
            }
            Insn::FStore(slot) => {
                let v = pop!();
                if v.ty() != VmType::Float {
                    return Err(VmError::TypeMismatch("fstore of non-Float value"));
                }
                store!(*slot, v);
            }
            Insn::AStore(slot) => {
                let v = pop!();
                if !v.is_ref_kind() {
                    return Err(VmError::TypeMismatch("astore of non-reference value"));
                }
                store!(*slot, v);
            }
            Insn::Pop => {
                pop!();
            }
            Insn::Dup => {
                let v = stack.last().ok_or(VmError::StackUnderflow)?.clone();
                push!(v);
            }
            Insn::Swap => {
                if stack.len() < 2 {
                    return Err(VmError::StackUnderflow);
                }
                let n = stack.len();
                stack.swap(n - 1, n - 2);
            }
            Insn::IAdd => {
                let b = pop_int!();
                let a = pop_int!();
                push!(VmValue::Int(a.wrapping_add(b)));
            }
            Insn::ISub => {
                let b = pop_int!();
                let a = pop_int!();
                push!(VmValue::Int(a.wrapping_sub(b)));
            }
            Insn::IMul => {
                let b = pop_int!();
                let a = pop_int!();
                push!(VmValue::Int(a.wrapping_mul(b)));
            }
            Insn::IDiv => {
                let b = pop_int!();
                let a = pop_int!();
                if b == 0 {
                    return Err(VmError::ArithmeticDivByZero);
                }
                push!(VmValue::Int(a.wrapping_div(b)));
            }
            Insn::IRem => {
                let b = pop_int!();
                let a = pop_int!();
                if b == 0 {
                    return Err(VmError::ArithmeticDivByZero);
                }
                push!(VmValue::Int(a.wrapping_rem(b)));
            }
            Insn::INeg => {
                let a = pop_int!();
                push!(VmValue::Int(a.wrapping_neg()));
            }
            Insn::IInc { slot, delta } => {
                let cell = locals
                    .get_mut(*slot)
                    .ok_or(VmError::LocalIndexOutOfBounds { index: *slot })?;
                match cell {
                    Some(VmValue::Int(v)) => *v = v.wrapping_add(*delta),
                    _ => return Err(VmError::TypeMismatch("iinc on non-Int local")),
                }
            }
            Insn::FAdd => {
                let b = pop_float!();
                let a = pop_float!();
                push!(VmValue::Float(a + b));
            }
            Insn::FSub => {
                let b = pop_float!();
                let a = pop_float!();
                push!(VmValue::Float(a - b));
            }
            Insn::FMul => {
                let b = pop_float!();
                let a = pop_float!();
                push!(VmValue::Float(a * b));
            }
            Insn::FDiv => {
                let b = pop_float!();
                let a = pop_float!();
                push!(VmValue::Float(a / b));
            }
            Insn::FNeg => {
                let a = pop_float!();
                push!(VmValue::Float(-a));
            }
            Insn::I2F => {
                let a = pop_int!();
                push!(VmValue::Float(a as f32));
            }
            Insn::F2I => {
                let a = pop_float!();
                // JVMS f2i: NaN → 0, saturates to i32 range — the same
                // semantics as Rust's `as` float→int conversion.
                push!(VmValue::Int(a as i32));
            }
            Insn::FCmpL | Insn::FCmpG => {
                let b = pop_float!();
                let a = pop_float!();
                let v = if a.is_nan() || b.is_nan() {
                    if matches!(insn, Insn::FCmpL) {
                        -1
                    } else {
                        1
                    }
                } else if a < b {
                    -1
                } else if a > b {
                    1
                } else {
                    0
                };
                push!(VmValue::Int(v));
            }
            Insn::If { op, target } => {
                let a = pop_int!();
                let taken = match op {
                    CmpOp::Eq => a == 0,
                    CmpOp::Ne => a != 0,
                    CmpOp::Lt => a < 0,
                    CmpOp::Ge => a >= 0,
                    CmpOp::Gt => a > 0,
                    CmpOp::Le => a <= 0,
                };
                if taken {
                    next = *target;
                }
            }
            Insn::IfICmp { op, target } => {
                let b = pop_int!();
                let a = pop_int!();
                let taken = match op {
                    CmpOp::Eq => a == b,
                    CmpOp::Ne => a != b,
                    CmpOp::Lt => a < b,
                    CmpOp::Ge => a >= b,
                    CmpOp::Gt => a > b,
                    CmpOp::Le => a <= b,
                };
                if taken {
                    next = *target;
                }
            }
            Insn::Goto { target } => next = *target,
            Insn::Invoke(kind) => {
                let sig = kind.signature();
                let nargs = sig.params.len() + usize::from(sig.receiver.is_some());
                if stack.len() < nargs {
                    return Err(VmError::StackUnderflow);
                }
                let call_args: Vec<VmValue> = stack.split_off(stack.len() - nargs);
                for (i, expected) in sig.params.iter().enumerate().rev() {
                    let arg = &call_args[i + usize::from(sig.receiver.is_some())];
                    if arg.ty() != *expected {
                        return Err(VmError::TypeMismatch("host call argument kind"));
                    }
                }
                if let Some(expected) = sig.receiver {
                    if call_args[0].ty() != expected {
                        return Err(VmError::TypeMismatch("host call receiver kind"));
                    }
                }
                let result = host.call(*kind, &call_args)?;
                match (sig.result, result) {
                    (Some(expected), Some(v)) if v.ty() == expected => push!(v),
                    (Some(_), Some(_)) => {
                        return Err(VmError::HostError(
                            "host returned a value of the wrong kind".into(),
                        ))
                    }
                    (Some(_), None) => {
                        return Err(VmError::HostError(
                            "host returned no value for a call that needs one".into(),
                        ))
                    }
                    (None, Some(_)) => {
                        return Err(VmError::HostError(
                            "host returned a value for a void call".into(),
                        ))
                    }
                    (None, None) => {}
                }
            }
            Insn::Return => {
                return Ok(VmResult {
                    return_value: None,
                    instructions_executed: executed,
                });
            }
            Insn::IReturn => {
                let v = pop!();
                if v.ty() != VmType::Int {
                    return Err(VmError::TypeMismatch("ireturn of non-Int value"));
                }
                return Ok(VmResult {
                    return_value: Some(v),
                    instructions_executed: executed,
                });
            }
            Insn::FReturn => {
                let v = pop!();
                if v.ty() != VmType::Float {
                    return Err(VmError::TypeMismatch("freturn of non-Float value"));
                }
                return Ok(VmResult {
                    return_value: Some(v),
                    instructions_executed: executed,
                });
            }
        }
        pc = next;
    }
}

// ---------------------------------------------------------------------------
// Test-support `.class` assembler
// ---------------------------------------------------------------------------

/// Minimal JVMS §4 `.class` assembler used to construct real
/// `0xCAFEBABE` class binaries in tests — deterministic, offline, and
/// toolchain-independent (no `javac` required). It emits a valid
/// `public` class extending `java/lang/Object` whose methods each carry
/// a single well-formed `Code` attribute.
///
/// This is a test-support utility: it validates nothing beyond the
/// constant-pool index space, so tests can deliberately build hostile or
/// malformed classes (bad branch targets, disallowed opcodes, Runtime/
/// JNDI method refs) to exercise the verifier's fail-closed paths.
pub mod testkit {
    use std::collections::HashMap;

    /// One constant-pool item.
    #[derive(Clone, Debug)]
    enum CpItem {
        Utf8(Vec<u8>),
        Integer(i32),
        Float(f32),
        Class(u16),
        String(u16),
        Methodref {
            class_index: u16,
            nat_index: u16,
        },
        InterfaceMethodref {
            class_index: u16,
            nat_index: u16,
        },
        NameAndType {
            name_index: u16,
            descriptor_index: u16,
        },
    }

    /// Constant-pool assembler with index lookup for the builder.
    #[derive(Default)]
    pub struct Cp {
        items: Vec<CpItem>,
        utf8: HashMap<Vec<u8>, u16>,
    }

    impl Cp {
        pub fn new() -> Self {
            Self::default()
        }

        fn push(&mut self, item: CpItem) -> u16 {
            let index = (self.items.len() + 1) as u16;
            self.items.push(item);
            index
        }

        /// `CONSTANT_Utf8`.
        pub fn utf8(&mut self, s: &str) -> u16 {
            if let Some(&i) = self.utf8.get(s.as_bytes()) {
                return i;
            }
            let index = self.push(CpItem::Utf8(s.as_bytes().to_vec()));
            self.utf8.insert(s.as_bytes().to_vec(), index);
            index
        }

        /// `CONSTANT_Integer`.
        pub fn int(&mut self, v: i32) -> u16 {
            self.push(CpItem::Integer(v))
        }

        /// `CONSTANT_Float`.
        pub fn float(&mut self, v: f32) -> u16 {
            self.push(CpItem::Float(v))
        }

        /// `CONSTANT_Class` for an internal name.
        pub fn class(&mut self, internal: &str) -> u16 {
            let name_index = self.utf8(internal);
            self.push(CpItem::Class(name_index))
        }

        /// `CONSTANT_String` for `ldc`.
        pub fn string(&mut self, s: &str) -> u16 {
            let utf8_index = self.utf8(s);
            self.push(CpItem::String(utf8_index))
        }

        /// `CONSTANT_NameAndType`.
        pub fn name_and_type(&mut self, name: &str, descriptor: &str) -> u16 {
            let name_index = self.utf8(name);
            let descriptor_index = self.utf8(descriptor);
            self.push(CpItem::NameAndType {
                name_index,
                descriptor_index,
            })
        }

        /// `CONSTANT_Methodref` → index for `invokevirtual`/`invokestatic`.
        pub fn method_ref(&mut self, class: &str, name: &str, descriptor: &str) -> u16 {
            let class_index = self.class(class);
            let nat_index = self.name_and_type(name, descriptor);
            self.push(CpItem::Methodref {
                class_index,
                nat_index,
            })
        }

        /// `CONSTANT_InterfaceMethodref` → index for `invokeinterface`.
        pub fn interface_method_ref(&mut self, class: &str, name: &str, descriptor: &str) -> u16 {
            let class_index = self.class(class);
            let nat_index = self.name_and_type(name, descriptor);
            self.push(CpItem::InterfaceMethodref {
                class_index,
                nat_index,
            })
        }

        /// `constant_pool_count` value for the header.
        pub fn count(&self) -> u16 {
            (self.items.len() + 1) as u16
        }

        fn encode(&self) -> Vec<u8> {
            let mut out = Vec::new();
            for item in &self.items {
                match item {
                    CpItem::Utf8(bytes) => {
                        out.push(1);
                        out.extend((bytes.len() as u16).to_be_bytes());
                        out.extend(bytes);
                    }
                    CpItem::Integer(v) => {
                        out.push(3);
                        out.extend(v.to_be_bytes());
                    }
                    CpItem::Float(v) => {
                        out.push(4);
                        out.extend(v.to_bits().to_be_bytes());
                    }
                    CpItem::Class(name_index) => {
                        out.push(7);
                        out.extend(name_index.to_be_bytes());
                    }
                    CpItem::String(utf8_index) => {
                        out.push(8);
                        out.extend(utf8_index.to_be_bytes());
                    }
                    CpItem::Methodref {
                        class_index,
                        nat_index,
                    }
                    | CpItem::InterfaceMethodref {
                        class_index,
                        nat_index,
                    } => {
                        out.push(if matches!(item, CpItem::Methodref { .. }) {
                            10
                        } else {
                            11
                        });
                        out.extend(class_index.to_be_bytes());
                        out.extend(nat_index.to_be_bytes());
                    }
                    CpItem::NameAndType {
                        name_index,
                        descriptor_index,
                    } => {
                        out.push(12);
                        out.extend(name_index.to_be_bytes());
                        out.extend(descriptor_index.to_be_bytes());
                    }
                }
            }
            out
        }
    }

    /// One method to emit: a `Code` attribute wrapping raw bytecode.
    #[derive(Clone, Debug)]
    pub struct MethodSpec {
        pub name: String,
        pub descriptor: String,
        pub access_flags: u16,
        pub max_stack: u16,
        pub max_locals: u16,
        pub code: Vec<u8>,
    }

    impl MethodSpec {
        /// `public` (non-static) method with `code` — the common case for
        /// event-hook fixtures.
        pub fn new(
            name: &str,
            descriptor: &str,
            max_stack: u16,
            max_locals: u16,
            code: Vec<u8>,
        ) -> Self {
            Self {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0x0001,
                max_stack,
                max_locals,
                code,
            }
        }

        /// `public static` variant.
        pub fn static_method(
            name: &str,
            descriptor: &str,
            max_stack: u16,
            max_locals: u16,
            code: Vec<u8>,
        ) -> Self {
            let mut m = Self::new(name, descriptor, max_stack, max_locals, code);
            m.access_flags = 0x0009;
            m
        }
    }

    /// Emit a complete JVMS §4 `.class` binary: `public` class
    /// `internal_name` extending `java/lang/Object`, no fields, the given
    /// methods each carrying exactly one `Code` attribute (no exception
    /// table, no nested attributes), no class attributes.
    pub fn assemble_class(internal_name: &str, cp: &Cp, methods: &[MethodSpec]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(0xCAFE_BABEu32.to_be_bytes());
        out.extend(0u16.to_be_bytes()); // minor
        out.extend(61u16.to_be_bytes()); // major (Java 17)
        out.extend(cp.count().to_be_bytes());
        out.extend(cp.encode());
        out.extend(0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
        out.extend(find_class_index(cp, internal_name).to_be_bytes());
        out.extend(find_class_index(cp, "java/lang/Object").to_be_bytes());
        out.extend(0u16.to_be_bytes()); // interfaces_count
        out.extend(0u16.to_be_bytes()); // fields_count
        out.extend((methods.len() as u16).to_be_bytes());
        for m in methods {
            out.extend(m.access_flags.to_be_bytes());
            out.extend(find_utf8_index(cp, &m.name).to_be_bytes());
            out.extend(find_utf8_index(cp, &m.descriptor).to_be_bytes());
            out.extend(1u16.to_be_bytes()); // attributes_count: just Code
            out.extend(find_utf8_index(cp, "Code").to_be_bytes());
            let mut code_attr = Vec::new();
            code_attr.extend(m.max_stack.to_be_bytes());
            code_attr.extend(m.max_locals.to_be_bytes());
            code_attr.extend((m.code.len() as u32).to_be_bytes());
            code_attr.extend(&m.code);
            code_attr.extend(0u16.to_be_bytes()); // exception_table_length
            code_attr.extend(0u16.to_be_bytes()); // attributes_count
            out.extend((code_attr.len() as u32).to_be_bytes());
            out.extend(code_attr);
        }
        out.extend(0u16.to_be_bytes()); // class attributes_count
        out
    }

    fn find_utf8_index(cp: &Cp, s: &str) -> u16 {
        for (index, item) in (1u16..).zip(&cp.items) {
            if let CpItem::Utf8(bytes) = item {
                if bytes == s.as_bytes() {
                    return index;
                }
            }
        }
        panic!("testkit: utf8 {s:?} was not interned in the pool");
    }

    fn find_class_index(cp: &Cp, internal: &str) -> u16 {
        let name_index = find_utf8_index(cp, internal);
        for (index, item) in (1u16..).zip(&cp.items) {
            if let CpItem::Class(n) = item {
                if *n == name_index {
                    return index;
                }
            }
        }
        panic!("testkit: class {internal:?} was not interned in the pool");
    }

    /// Convenience wrapper: builds the pool for a class whose members
    /// are fully described by `methods` plus any extra interning the
    /// caller performs on the returned `Cp` first.
    ///
    /// Usage:
    /// ```text
    /// let mut a = ClassAssembler::new("com/acme/Hook");
    /// let mr = a.cp.method_ref("mcspine/api/Event", "getDamage", "()F");
    /// let bytes = a.finish(&[MethodSpec::new("onDamage", "(Lmcspine/api/Event;)V", 4, 2, code)]);
    /// ```
    pub struct ClassAssembler {
        /// The constant pool under construction — intern strings,
        /// classes, and method refs here, then call [`Self::finish`].
        pub cp: Cp,
        internal_name: String,
    }

    impl ClassAssembler {
        pub fn new(internal_name: &str) -> Self {
            let mut cp = Cp::new();
            cp.class(internal_name);
            cp.class("java/lang/Object");
            cp.utf8("Code");
            Self {
                cp,
                internal_name: internal_name.to_string(),
            }
        }

        /// Serialize to real `.class` bytes. Method names, descriptors,
        /// and the `Code` attribute name are interned automatically.
        pub fn finish(mut self, methods: &[MethodSpec]) -> Vec<u8> {
            for m in methods {
                self.cp.utf8(&m.name);
                self.cp.utf8(&m.descriptor);
            }
            assemble_class(&self.internal_name, &self.cp, methods)
        }
    }
}
