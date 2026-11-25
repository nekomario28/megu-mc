// SPDX-License-Identifier: LGPL-3.0-or-later

//! Static module IR: everything the decoder extracts from a `.wasm` binary
//! before validation and instantiation.

/// The four MVP value types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
}

impl ValType {
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x7F => Some(ValType::I32),
            0x7E => Some(ValType::I64),
            0x7D => Some(ValType::F32),
            0x7C => Some(ValType::F64),
            _ => None,
        }
    }

    pub(crate) fn zero(self) -> Value {
        match self {
            ValType::I32 => Value::I32(0),
            ValType::I64 => Value::I64(0),
            ValType::F32 => Value::F32(0.0),
            ValType::F64 => Value::F64(0.0),
        }
    }
}

/// A runtime value on the operand stack. 16 bytes, `Copy`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
}

impl Value {
    /// The validator guarantees operand types; these accessors never panic
    /// and fall back to zero on an impossible state.
    pub(crate) fn i32(self) -> i32 {
        match self {
            Value::I32(v) => v,
            _ => 0,
        }
    }
    pub(crate) fn i64(self) -> i64 {
        match self {
            Value::I64(v) => v,
            _ => 0,
        }
    }
    pub(crate) fn f32(self) -> f32 {
        match self {
            Value::F32(v) => v,
            _ => 0.0,
        }
    }
    pub(crate) fn f64(self) -> f64 {
        match self {
            Value::F64(v) => v,
            _ => 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub min: u32,
    pub max: Option<u32>,
}

/// Import descriptor. Non-function imports are parsed for structure but
/// always rejected at instantiation — the payloads are not retained.
#[derive(Clone, Debug)]
pub enum ImportDesc {
    Func(u32),
    Table,
    Memory,
    Global,
}

#[derive(Clone, Debug)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub desc: ImportDesc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternKind {
    Func,
    Table,
    Memory,
    Global,
}

#[derive(Clone, Debug)]
pub struct Export {
    pub name: String,
    pub kind: ExternKind,
    pub index: u32,
}

#[derive(Clone, Debug)]
pub struct GlobalDecl {
    pub ty: ValType,
    pub mutable: bool,
    pub init: ConstExpr,
}

/// MVP constant expression: a single opcode.
#[derive(Clone, Copy, Debug)]
pub enum ConstExpr {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// `global.get` of an earlier immutable global (validated).
    GlobalGet(u32),
}

#[derive(Clone, Debug)]
pub struct TableDecl {
    pub limits: Limits,
}

#[derive(Clone, Debug)]
pub struct MemDecl {
    pub limits: Limits,
}

#[derive(Clone, Debug)]
pub struct ElemSeg {
    pub table: u32,
    pub offset: ConstExpr,
    pub funcs: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct DataSeg {
    pub mem: u32,
    pub offset: ConstExpr,
    pub bytes: Vec<u8>,
}

/// A fully decoded + validated module ready for instantiation.
#[derive(Debug)]
pub struct Module {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    /// Type index of each *defined* function (index space starts after the
    /// imported functions).
    pub func_types: Vec<u32>,
    /// Executable op lists, parallel to `func_types`.
    pub funcs: Vec<Vec<Op>>,
    /// Declared locals (after params), parallel to `func_types`.
    pub locals: Vec<Vec<ValType>>,
    pub tables: Vec<TableDecl>,
    pub mems: Vec<MemDecl>,
    pub globals: Vec<GlobalDecl>,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub elems: Vec<ElemSeg>,
    pub datas: Vec<DataSeg>,
}

impl Module {
    /// Number of imported functions — defined function `i` lives at func
    /// index `import_funcs + i`.
    pub fn import_func_count(&self) -> usize {
        self.imports
            .iter()
            .filter(|i| matches!(i.desc, ImportDesc::Func(_)))
            .count()
    }

    /// Type index of func index `f` (imports first), if it exists.
    pub(crate) fn func_type_index(&self, f: u32) -> Option<u32> {
        let mut imported = 0u32;
        for imp in &self.imports {
            if let ImportDesc::Func(ty) = imp.desc {
                if imported == f {
                    return Some(ty);
                }
                imported += 1;
            }
        }
        let def = (f as usize).checked_sub(imported as usize)?;
        self.func_types.get(def).copied()
    }

    pub(crate) fn export_func(&self, name: &str) -> Option<u32> {
        self.exports
            .iter()
            .find(|e| e.kind == ExternKind::Func && e.name == name)
            .map(|e| e.index)
    }
}

/// Resolved branch target in the flat op list: absolute `pc`, `keep` values
/// carried from the stack top, and `height` the stack truncates to before
/// the kept values are pushed back.
#[derive(Clone, Copy, Debug)]
pub struct JumpDest {
    pub pc: u32,
    pub keep: u16,
    pub height: u32,
}

/// Executable op set. Structured instructions (`block`/`loop`/`if`/`else`/
/// `end`) are erased by the validator, which rewrites branches into
/// absolute [`JumpDest`]s carrying their stack fixups — the interpreter
/// itself maintains no control stack.
#[derive(Clone, Debug)]
pub enum Op {
    Unreachable,
    Nop,
    /// `br` — unconditional resolved jump.
    Jump(JumpDest),
    /// `br_if` — pops an i32 condition; jumps when nonzero.
    JumpIf(JumpDest),
    /// `if` without a then-fallthrough fixup — pops an i32 condition;
    /// jumps to the else arm (or block end) when zero.
    JumpIfZero { pc: u32 },
    /// `br_table`.
    JumpTable {
        targets: Box<[JumpDest]>,
        default: JumpDest,
    },
    /// Decode-only: `br l` — the validator rewrites it to [`Op::Jump`].
    /// Never reaches the interpreter.
    Br(u32),
    /// Decode-only: `br_if l` — the validator rewrites it to
    /// [`Op::JumpIf`]. Never reaches the interpreter.
    BrIf(u32),
    /// Decode-only: `br_table` — the validator rewrites it to
    /// [`Op::JumpTable`]. Never reaches the interpreter.
    BrTable {
        targets: Box<[u32]>,
        default: u32,
    },
    /// `return` — unwind the current frame.
    Return,
    Call { func: u32 },
    CallIndirect { ty: u32 },
    Drop,
    /// Untyped MVP `select`.
    Select,
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),
    I32Load { off: u32 },
    I64Load { off: u32 },
    F32Load { off: u32 },
    F64Load { off: u32 },
    I32Load8S { off: u32 },
    I32Load8U { off: u32 },
    I32Load16S { off: u32 },
    I32Load16U { off: u32 },
    I64Load8S { off: u32 },
    I64Load8U { off: u32 },
    I64Load16S { off: u32 },
    I64Load16U { off: u32 },
    I64Load32S { off: u32 },
    I64Load32U { off: u32 },
    I32Store { off: u32 },
    I64Store { off: u32 },
    F32Store { off: u32 },
    F64Store { off: u32 },
    I32Store8 { off: u32 },
    I32Store16 { off: u32 },
    I64Store8 { off: u32 },
    I64Store16 { off: u32 },
    I64Store32 { off: u32 },
    MemSize,
    MemGrow,
    ConstI32(i32),
    ConstI64(i64),
    ConstF32(f32),
    ConstF64(f64),
    I32Eqz,
    I32Eq,
    I32Ne,
    I32LtS,
    I32LtU,
    I32GtS,
    I32GtU,
    I32LeS,
    I32LeU,
    I32GeS,
    I32GeU,
    I64Eqz,
    I64Eq,
    I64Ne,
    I64LtS,
    I64LtU,
    I64GtS,
    I64GtU,
    I64LeS,
    I64LeU,
    I64GeS,
    I64GeU,
    F32Eq,
    F32Ne,
    F32Lt,
    F32Gt,
    F32Le,
    F32Ge,
    F64Eq,
    F64Ne,
    F64Lt,
    F64Gt,
    F64Le,
    F64Ge,
    I32Clz,
    I32Ctz,
    I32Popcnt,
    I32Add,
    I32Sub,
    I32Mul,
    I32DivS,
    I32DivU,
    I32RemS,
    I32RemU,
    I32And,
    I32Or,
    I32Xor,
    I32Shl,
    I32ShrS,
    I32ShrU,
    I32Rotl,
    I32Rotr,
    I64Clz,
    I64Ctz,
    I64Popcnt,
    I64Add,
    I64Sub,
    I64Mul,
    I64DivS,
    I64DivU,
    I64RemS,
    I64RemU,
    I64And,
    I64Or,
    I64Xor,
    I64Shl,
    I64ShrS,
    I64ShrU,
    I64Rotl,
    I64Rotr,
    F32Abs,
    F32Neg,
    F32Ceil,
    F32Floor,
    F32Trunc,
    F32Nearest,
    F32Sqrt,
    F32Add,
    F32Sub,
    F32Mul,
    F32Div,
    F32Min,
    F32Max,
    F32Copysign,
    F64Abs,
    F64Neg,
    F64Ceil,
    F64Floor,
    F64Trunc,
    F64Nearest,
    F64Sqrt,
    F64Add,
    F64Sub,
    F64Mul,
    F64Div,
    F64Min,
    F64Max,
    F64Copysign,
    I32WrapI64,
    I32TruncF32S,
    I32TruncF32U,
    I32TruncF64S,
    I32TruncF64U,
    I64ExtendI32S,
    I64ExtendI32U,
    I64TruncF32S,
    I64TruncF32U,
    I64TruncF64S,
    I64TruncF64U,
    F32ConvertI32S,
    F32ConvertI32U,
    F32ConvertI64S,
    F32ConvertI64U,
    F32DemoteF64,
    F64ConvertI32S,
    F64ConvertI32U,
    F64ConvertI64S,
    F64ConvertI64U,
    F64PromoteF32,
    I32ReinterpretF32,
    I64ReinterpretF64,
    F32ReinterpretI32,
    F64ReinterpretI64,
}
