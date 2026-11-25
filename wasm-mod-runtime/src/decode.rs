// SPDX-License-Identifier: LGPL-3.0-or-later

//! WebAssembly MVP binary decoder: bytes → [`RawModule`] IR with `Parsed`
//! instruction streams (structured markers still present — the validator
//! lowers them into flat jump ops).
//!
//! Everything is bounds-checked and fails closed: the first malformed byte
//! ends decoding, and hard caps bound memory regardless of input size.

use crate::error::LoadError;
use crate::module::*;

const MAX_NAME: usize = 1024;
const MAX_TYPES: usize = 10_000;
const MAX_IMPORTS: usize = 10_000;
const MAX_EXPORTS: usize = 10_000;
const MAX_FUNCS: usize = 65_536;
const MAX_GLOBALS: usize = 4_096;
const MAX_TABLES: usize = 16;
const MAX_MEMS: usize = 1;
const MAX_LOCALS: u32 = 50_000;
const MAX_FUNC_OPS: usize = 300_000;
const MAX_ELEMS: usize = 4_096;
const MAX_DATAS: usize = 4_096;
const MAX_DATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_BR_TABLE: usize = 65_536;

type DResult<T> = Result<T, LoadError>;

fn malformed<T>(msg: impl Into<String>) -> DResult<T> {
    Err(LoadError::Malformed(msg.into()))
}

fn unsupported<T>(msg: impl Into<String>) -> DResult<T> {
    Err(LoadError::Unsupported(msg.into()))
}

/// Everything the decoder produces; `validate` consumes it into a
/// [`Module`] whose `funcs` are flat executable op lists.
pub(crate) struct RawModule {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    pub func_types: Vec<u32>,
    pub bodies: Vec<(Vec<ValType>, Vec<Parsed>)>,
    pub tables: Vec<TableDecl>,
    pub mems: Vec<MemDecl>,
    pub globals: Vec<GlobalDecl>,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub elems: Vec<ElemSeg>,
    pub datas: Vec<DataSeg>,
}

impl RawModule {
    /// Number of imported functions — defined function `i` sits at func
    /// index `import_func_count() + i`.
    pub fn import_func_count(&self) -> usize {
        self.imports
            .iter()
            .filter(|i| matches!(i.desc, ImportDesc::Func(_)))
            .count()
    }

    /// Type index of func index `f` (imports first), if it exists.
    pub fn func_type_index(&self, f: u32) -> Option<u32> {
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
}

/// Decode a full module from binary bytes.
pub fn decode_module(bytes: &[u8]) -> DResult<RawModule> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != b"\0asm" {
        return malformed("bad magic");
    }
    if r.take(4)? != [1, 0, 0, 0] {
        return malformed("unsupported binary version");
    }

    let mut module = RawModule {
        types: Vec::new(),
        imports: Vec::new(),
        func_types: Vec::new(),
        bodies: Vec::new(),
        tables: Vec::new(),
        mems: Vec::new(),
        globals: Vec::new(),
        exports: Vec::new(),
        start: None,
        elems: Vec::new(),
        datas: Vec::new(),
    };

    let mut seen = [false; 13];
    let mut last_section = 0u8;
    let mut code_bodies: Vec<(Vec<ValType>, Vec<Parsed>)> = Vec::new();

    while !r.done() {
        let id = r.u8()?;
        let size = r.vu32()? as usize;
        if size > r.remaining() {
            return malformed("section size exceeds input");
        }
        let mut sr = r.sub(size)?;
        if id != 0 {
            if id > 12 {
                return malformed(format!("unknown section id {id}"));
            }
            if seen[id as usize] {
                return malformed(format!("duplicate section {id}"));
            }
            if id < last_section {
                return malformed("sections out of order");
            }
            seen[id as usize] = true;
            last_section = id;
        }
        match id {
            0 => { /* custom: skipped */ }
            1 => read_types(&mut sr, &mut module)?,
            2 => read_imports(&mut sr, &mut module)?,
            3 => read_funcs(&mut sr, &mut module)?,
            4 => read_tables(&mut sr, &mut module)?,
            5 => read_mems(&mut sr, &mut module)?,
            6 => read_globals(&mut sr, &mut module)?,
            7 => read_exports(&mut sr, &mut module)?,
            8 => module.start = Some(sr.vu32()?),
            9 => read_elems(&mut sr, &mut module)?,
            10 => code_bodies = read_code(&mut sr)?,
            11 => read_datas(&mut sr, &mut module)?,
            _ => { /* datacount (12): not needed by MVP semantics */ }
        }
        if !sr.done() {
            return malformed(format!("section {id} has trailing bytes"));
        }
    }

    if code_bodies.len() != module.func_types.len() {
        return malformed(format!(
            "function/code count mismatch: {} declared, {} bodies",
            module.func_types.len(),
            code_bodies.len()
        ));
    }
    module.bodies = code_bodies;
    Ok(module)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn take(&mut self, n: usize) -> DResult<&'a [u8]> {
        if self.remaining() < n {
            return malformed("unexpected end of input");
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn sub(&mut self, n: usize) -> DResult<Reader<'a>> {
        Ok(Reader::new(self.take(n)?))
    }
    fn u8(&mut self) -> DResult<u8> {
        Ok(self.take(1)?[0])
    }
    /// Unsigned LEB128, bounded to `bits`; oversized final bytes rejected.
    fn vu(&mut self, bits: u32) -> DResult<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            if shift >= bits {
                return malformed("LEB128 too large");
            }
            let avail = bits - shift;
            if b & 0x80 == 0 && avail < 7 && (b & 0x7F) >> avail != 0 {
                return malformed("LEB128 overflow");
            }
            result |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }
    fn vu32(&mut self) -> DResult<u32> {
        Ok(self.vu(32)? as u32)
    }
    /// Signed LEB128, bounded to `bits`; returns sign-extended i64.
    fn vs(&mut self, bits: u32) -> DResult<i64> {
        let mut result = 0i64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            if shift >= bits {
                return malformed("signed LEB128 too large");
            }
            result |= i64::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift > bits {
                    return malformed("signed LEB128 overflow");
                }
                if shift < 64 && b & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
        }
    }
    fn vs32(&mut self) -> DResult<i32> {
        Ok(self.vs(32)? as i32)
    }
    fn vs64(&mut self) -> DResult<i64> {
        self.vs(64)
    }
    fn f32(&mut self) -> DResult<f32> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(f32::from_le_bytes(b))
    }
    fn f64(&mut self) -> DResult<f64> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(f64::from_le_bytes(b))
    }
    fn name(&mut self) -> DResult<String> {
        let n = self.vu32()? as usize;
        if n > MAX_NAME {
            return malformed("name too long");
        }
        let bytes = self.take(n)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| LoadError::Malformed("name not UTF-8".into()))
    }
}

fn valtype(r: &mut Reader<'_>) -> DResult<ValType> {
    let b = r.u8()?;
    ValType::from_byte(b).ok_or_else(|| LoadError::Malformed(format!("bad value type {b:#x}")))
}

fn limits(r: &mut Reader<'_>) -> DResult<Limits> {
    let (min, max) = match r.u8()? {
        0x00 => (r.vu32()?, None),
        0x01 => (r.vu32()?, Some(r.vu32()?)),
        b => return malformed(format!("bad limits flag {b:#x}")),
    };
    if let Some(max) = max {
        if max < min {
            return malformed("limits max < min");
        }
    }
    Ok(Limits { min, max })
}

fn blocktype(r: &mut Reader<'_>) -> DResult<Option<ValType>> {
    // A single byte encodes 0x40 or a valtype; anything else is an s33
    // type index (multi-value — outside the MVP subset we support).
    let b = r.u8()?;
    match b {
        0x40 => Ok(None),
        0x7F => Ok(Some(ValType::I32)),
        0x7E => Ok(Some(ValType::I64)),
        0x7D => Ok(Some(ValType::F32)),
        0x7C => Ok(Some(ValType::F64)),
        _ => unsupported("multi-value block type (type index) is not MVP"),
    }
}

fn const_expr(r: &mut Reader<'_>) -> DResult<ConstExpr> {
    let e = match r.u8()? {
        0x41 => ConstExpr::I32(r.vs32()?),
        0x42 => ConstExpr::I64(r.vs64()?),
        0x43 => ConstExpr::F32(r.f32()?),
        0x44 => ConstExpr::F64(r.f64()?),
        0x23 => ConstExpr::GlobalGet(r.vu32()?),
        b => return malformed(format!("bad const-expr opcode {b:#x}")),
    };
    if r.u8()? != 0x0B {
        return malformed("const expr missing `end`");
    }
    Ok(e)
}

fn read_types(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_TYPES {
        return Err(LoadError::LimitExceeded("type count".into()));
    }
    for _ in 0..n {
        if r.u8()? != 0x60 {
            return malformed("type entry is not a functype");
        }
        let np = r.vu32()? as usize;
        let mut params = Vec::with_capacity(np.min(1024));
        for _ in 0..np {
            params.push(valtype(r)?);
        }
        let nr = r.vu32()? as usize;
        let mut results = Vec::with_capacity(nr.min(1024));
        for _ in 0..nr {
            results.push(valtype(r)?);
        }
        m.types.push(FuncType { params, results });
    }
    Ok(())
}

fn read_imports(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_IMPORTS {
        return Err(LoadError::LimitExceeded("import count".into()));
    }
    for _ in 0..n {
        let module = r.name()?;
        let name = r.name()?;
        let desc = match r.u8()? {
            0x00 => ImportDesc::Func(r.vu32()?),
            0x01 => {
                let et = r.u8()?;
                if et != 0x70 {
                    return unsupported("non-funcref table import");
                }
                limits(r)?;
                ImportDesc::Table
            }
            0x02 => {
                limits(r)?;
                ImportDesc::Memory
            }
            0x03 => {
                valtype(r)?;
                match r.u8()? {
                    0 | 1 => {}
                    b => return malformed(format!("bad mutability {b:#x}")),
                }
                ImportDesc::Global
            }
            b => return malformed(format!("bad import kind {b:#x}")),
        };
        m.imports.push(Import { module, name, desc });
    }
    Ok(())
}

fn read_funcs(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_FUNCS {
        return Err(LoadError::LimitExceeded("function count".into()));
    }
    for _ in 0..n {
        m.func_types.push(r.vu32()?);
    }
    Ok(())
}

fn read_tables(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_TABLES {
        return Err(LoadError::LimitExceeded("table count".into()));
    }
    for _ in 0..n {
        if r.u8()? != 0x70 {
            return unsupported("non-funcref table");
        }
        m.tables.push(TableDecl {
            limits: limits(r)?,
        });
    }
    Ok(())
}

fn read_mems(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_MEMS {
        return unsupported("multiple memories");
    }
    for _ in 0..n {
        m.mems.push(MemDecl {
            limits: limits(r)?,
        });
    }
    Ok(())
}

fn read_globals(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_GLOBALS {
        return Err(LoadError::LimitExceeded("global count".into()));
    }
    for _ in 0..n {
        let ty = valtype(r)?;
        let mutable = match r.u8()? {
            0 => false,
            1 => true,
            b => return malformed(format!("bad mutability {b:#x}")),
        };
        let init = const_expr(r)?;
        m.globals.push(GlobalDecl { ty, mutable, init });
    }
    Ok(())
}

fn read_exports(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_EXPORTS {
        return Err(LoadError::LimitExceeded("export count".into()));
    }
    for _ in 0..n {
        let name = r.name()?;
        if m.exports.iter().any(|e: &Export| e.name == name) {
            return malformed(format!("duplicate export `{name}`"));
        }
        let kind = match r.u8()? {
            0x00 => ExternKind::Func,
            0x01 => ExternKind::Table,
            0x02 => ExternKind::Memory,
            0x03 => ExternKind::Global,
            b => return malformed(format!("bad export kind {b:#x}")),
        };
        let index = r.vu32()?;
        m.exports.push(Export { name, kind, index });
    }
    Ok(())
}

fn read_elems(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_ELEMS {
        return Err(LoadError::LimitExceeded("element segment count".into()));
    }
    for _ in 0..n {
        let table = r.vu32()?;
        let offset = const_expr(r)?;
        let count = r.vu32()? as usize;
        if count > MAX_FUNCS {
            return Err(LoadError::LimitExceeded("element size".into()));
        }
        let mut funcs = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            funcs.push(r.vu32()?);
        }
        m.elems.push(ElemSeg { table, offset, funcs });
    }
    Ok(())
}

fn read_datas(r: &mut Reader<'_>, m: &mut RawModule) -> DResult<()> {
    let n = r.vu32()? as usize;
    if n > MAX_DATAS {
        return Err(LoadError::LimitExceeded("data segment count".into()));
    }
    for _ in 0..n {
        let mem = r.vu32()?;
        let offset = const_expr(r)?;
        let len = r.vu32()? as usize;
        if len > MAX_DATA_BYTES || len > r.remaining() {
            return malformed("data segment too large");
        }
        let bytes = r.take(len)?.to_vec();
        m.datas.push(DataSeg { mem, offset, bytes });
    }
    Ok(())
}

/// Decoded-function intermediate form: block markers survive until the
/// validator rewrites them into resolved jumps.
pub(crate) enum Parsed {
    Block { result: Option<ValType> },
    Loop { result: Option<ValType> },
    If { result: Option<ValType> },
    Else,
    End,
    Op(Op),
}

enum CtrlKind {
    Block,
    Loop,
    If,
}

fn read_code(r: &mut Reader<'_>) -> DResult<Vec<(Vec<ValType>, Vec<Parsed>)>> {
    let n = r.vu32()? as usize;
    if n > MAX_FUNCS {
        return Err(LoadError::LimitExceeded("code body count".into()));
    }
    let mut out = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let size = r.vu32()? as usize;
        if size > r.remaining() {
            return malformed("code body exceeds section");
        }
        let mut br = r.sub(size)?;
        let nloc = br.vu32()? as usize;
        let mut locals = Vec::new();
        let mut total: u32 = 0;
        for _ in 0..nloc {
            let count = br.vu32()?;
            let ty = valtype(&mut br)?;
            total = total
                .checked_add(count)
                .ok_or_else(|| LoadError::LimitExceeded("local count".into()))?;
            if total > MAX_LOCALS {
                return Err(LoadError::LimitExceeded("local count".into()));
            }
            for _ in 0..count {
                locals.push(ty);
            }
        }
        let ops = read_ops(&mut br)?;
        if !br.done() {
            return malformed("code body has trailing bytes");
        }
        out.push((locals, ops));
    }
    Ok(out)
}

/// Decode one function body expression into `Parsed` items, ending at the
/// function-level `End`. Structural nesting is tracked so a missing,
/// extra, or misplaced `end`/`else` is a decode failure.
fn read_ops(r: &mut Reader<'_>) -> DResult<Vec<Parsed>> {
    let mut ops = Vec::new();
    let mut ctrl: Vec<(CtrlKind, bool)> = Vec::new(); // (kind, else_seen)

    macro_rules! mem_op {
        ($variant:ident, $natural:expr) => {{
            let align = r.vu32()?;
            let off = r.vu32()?;
            if align > $natural {
                return malformed("alignment exceeds natural alignment");
            }
            Parsed::Op(Op::$variant { off })
        }};
    }

    loop {
        if ops.len() >= MAX_FUNC_OPS {
            return Err(LoadError::LimitExceeded("instruction count".into()));
        }
        let opcode = r.u8()?;
        let parsed = match opcode {
            0x00 => Parsed::Op(Op::Unreachable),
            0x01 => Parsed::Op(Op::Nop),
            0x02 => {
                let result = blocktype(r)?;
                ctrl.push((CtrlKind::Block, false));
                Parsed::Block { result }
            }
            0x03 => {
                let result = blocktype(r)?;
                ctrl.push((CtrlKind::Loop, false));
                Parsed::Loop { result }
            }
            0x04 => {
                let result = blocktype(r)?;
                ctrl.push((CtrlKind::If, false));
                Parsed::If { result }
            }
            0x05 => match ctrl.last_mut() {
                Some((CtrlKind::If, else_seen @ false)) => {
                    *else_seen = true;
                    Parsed::Else
                }
                _ => return malformed("`else` without matching `if`"),
            },
            0x0B => {
                if ctrl.pop().is_none() {
                    ops.push(Parsed::End);
                    return Ok(ops);
                }
                Parsed::End
            }
            0x0C => Parsed::Op(Op::Br(r.vu32()?)),
            0x0D => Parsed::Op(Op::BrIf(r.vu32()?)),
            0x0E => {
                let count = r.vu32()? as usize;
                if count > MAX_BR_TABLE {
                    return Err(LoadError::LimitExceeded("br_table size".into()));
                }
                let mut targets = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    targets.push(r.vu32()?);
                }
                let default = r.vu32()?;
                Parsed::Op(Op::BrTable {
                    targets: targets.into_boxed_slice(),
                    default,
                })
            }
            0x0F => Parsed::Op(Op::Return),
            0x10 => Parsed::Op(Op::Call { func: r.vu32()? }),
            0x11 => {
                let ty = r.vu32()?;
                if r.u8()? != 0x00 {
                    return malformed("call_indirect reserved byte must be 0");
                }
                Parsed::Op(Op::CallIndirect { ty })
            }
            0x1A => Parsed::Op(Op::Drop),
            0x1B => Parsed::Op(Op::Select),
            0x1C => return unsupported("typed select"),
            0x20 => Parsed::Op(Op::LocalGet(r.vu32()?)),
            0x21 => Parsed::Op(Op::LocalSet(r.vu32()?)),
            0x22 => Parsed::Op(Op::LocalTee(r.vu32()?)),
            0x23 => Parsed::Op(Op::GlobalGet(r.vu32()?)),
            0x24 => Parsed::Op(Op::GlobalSet(r.vu32()?)),
            0x28 => mem_op!(I32Load, 2),
            0x29 => mem_op!(I64Load, 3),
            0x2A => mem_op!(F32Load, 2),
            0x2B => mem_op!(F64Load, 3),
            0x2C => mem_op!(I32Load8S, 0),
            0x2D => mem_op!(I32Load8U, 0),
            0x2E => mem_op!(I32Load16S, 1),
            0x2F => mem_op!(I32Load16U, 1),
            0x30 => mem_op!(I64Load8S, 0),
            0x31 => mem_op!(I64Load8U, 0),
            0x32 => mem_op!(I64Load16S, 1),
            0x33 => mem_op!(I64Load16U, 1),
            0x34 => mem_op!(I64Load32S, 2),
            0x35 => mem_op!(I64Load32U, 2),
            0x36 => mem_op!(I32Store, 2),
            0x37 => mem_op!(I64Store, 3),
            0x38 => mem_op!(F32Store, 2),
            0x39 => mem_op!(F64Store, 3),
            0x3A => mem_op!(I32Store8, 0),
            0x3B => mem_op!(I32Store16, 1),
            0x3C => mem_op!(I64Store8, 0),
            0x3D => mem_op!(I64Store16, 1),
            0x3E => mem_op!(I64Store32, 2),
            0x3F => {
                if r.u8()? != 0x00 {
                    return malformed("memory.size reserved byte must be 0");
                }
                Parsed::Op(Op::MemSize)
            }
            0x40 => {
                if r.u8()? != 0x00 {
                    return malformed("memory.grow reserved byte must be 0");
                }
                Parsed::Op(Op::MemGrow)
            }
            0x41 => Parsed::Op(Op::ConstI32(r.vs32()?)),
            0x42 => Parsed::Op(Op::ConstI64(r.vs64()?)),
            0x43 => Parsed::Op(Op::ConstF32(r.f32()?)),
            0x44 => Parsed::Op(Op::ConstF64(r.f64()?)),
            0x45 => Parsed::Op(Op::I32Eqz),
            0x46 => Parsed::Op(Op::I32Eq),
            0x47 => Parsed::Op(Op::I32Ne),
            0x48 => Parsed::Op(Op::I32LtS),
            0x49 => Parsed::Op(Op::I32LtU),
            0x4A => Parsed::Op(Op::I32GtS),
            0x4B => Parsed::Op(Op::I32GtU),
            0x4C => Parsed::Op(Op::I32LeS),
            0x4D => Parsed::Op(Op::I32LeU),
            0x4E => Parsed::Op(Op::I32GeS),
            0x4F => Parsed::Op(Op::I32GeU),
            0x50 => Parsed::Op(Op::I64Eqz),
            0x51 => Parsed::Op(Op::I64Eq),
            0x52 => Parsed::Op(Op::I64Ne),
            0x53 => Parsed::Op(Op::I64LtS),
            0x54 => Parsed::Op(Op::I64LtU),
            0x55 => Parsed::Op(Op::I64GtS),
            0x56 => Parsed::Op(Op::I64GtU),
            0x57 => Parsed::Op(Op::I64LeS),
            0x58 => Parsed::Op(Op::I64LeU),
            0x59 => Parsed::Op(Op::I64GeS),
            0x5A => Parsed::Op(Op::I64GeU),
            0x5B => Parsed::Op(Op::F32Eq),
            0x5C => Parsed::Op(Op::F32Ne),
            0x5D => Parsed::Op(Op::F32Lt),
            0x5E => Parsed::Op(Op::F32Gt),
            0x5F => Parsed::Op(Op::F32Le),
            0x60 => Parsed::Op(Op::F32Ge),
            0x61 => Parsed::Op(Op::F64Eq),
            0x62 => Parsed::Op(Op::F64Ne),
            0x63 => Parsed::Op(Op::F64Lt),
            0x64 => Parsed::Op(Op::F64Gt),
            0x65 => Parsed::Op(Op::F64Le),
            0x66 => Parsed::Op(Op::F64Ge),
            0x67 => Parsed::Op(Op::I32Clz),
            0x68 => Parsed::Op(Op::I32Ctz),
            0x69 => Parsed::Op(Op::I32Popcnt),
            0x6A => Parsed::Op(Op::I32Add),
            0x6B => Parsed::Op(Op::I32Sub),
            0x6C => Parsed::Op(Op::I32Mul),
            0x6D => Parsed::Op(Op::I32DivS),
            0x6E => Parsed::Op(Op::I32DivU),
            0x6F => Parsed::Op(Op::I32RemS),
            0x70 => Parsed::Op(Op::I32RemU),
            0x71 => Parsed::Op(Op::I32And),
            0x72 => Parsed::Op(Op::I32Or),
            0x73 => Parsed::Op(Op::I32Xor),
            0x74 => Parsed::Op(Op::I32Shl),
            0x75 => Parsed::Op(Op::I32ShrS),
            0x76 => Parsed::Op(Op::I32ShrU),
            0x77 => Parsed::Op(Op::I32Rotl),
            0x78 => Parsed::Op(Op::I32Rotr),
            0x79 => Parsed::Op(Op::I64Clz),
            0x7A => Parsed::Op(Op::I64Ctz),
            0x7B => Parsed::Op(Op::I64Popcnt),
            0x7C => Parsed::Op(Op::I64Add),
            0x7D => Parsed::Op(Op::I64Sub),
            0x7E => Parsed::Op(Op::I64Mul),
            0x7F => Parsed::Op(Op::I64DivS),
            0x80 => Parsed::Op(Op::I64DivU),
            0x81 => Parsed::Op(Op::I64RemS),
            0x82 => Parsed::Op(Op::I64RemU),
            0x83 => Parsed::Op(Op::I64And),
            0x84 => Parsed::Op(Op::I64Or),
            0x85 => Parsed::Op(Op::I64Xor),
            0x86 => Parsed::Op(Op::I64Shl),
            0x87 => Parsed::Op(Op::I64ShrS),
            0x88 => Parsed::Op(Op::I64ShrU),
            0x89 => Parsed::Op(Op::I64Rotl),
            0x8A => Parsed::Op(Op::I64Rotr),
            0x8B => Parsed::Op(Op::F32Abs),
            0x8C => Parsed::Op(Op::F32Neg),
            0x8D => Parsed::Op(Op::F32Ceil),
            0x8E => Parsed::Op(Op::F32Floor),
            0x8F => Parsed::Op(Op::F32Trunc),
            0x90 => Parsed::Op(Op::F32Nearest),
            0x91 => Parsed::Op(Op::F32Sqrt),
            0x92 => Parsed::Op(Op::F32Add),
            0x93 => Parsed::Op(Op::F32Sub),
            0x94 => Parsed::Op(Op::F32Mul),
            0x95 => Parsed::Op(Op::F32Div),
            0x96 => Parsed::Op(Op::F32Min),
            0x97 => Parsed::Op(Op::F32Max),
            0x98 => Parsed::Op(Op::F32Copysign),
            0x99 => Parsed::Op(Op::F64Abs),
            0x9A => Parsed::Op(Op::F64Neg),
            0x9B => Parsed::Op(Op::F64Ceil),
            0x9C => Parsed::Op(Op::F64Floor),
            0x9D => Parsed::Op(Op::F64Trunc),
            0x9E => Parsed::Op(Op::F64Nearest),
            0x9F => Parsed::Op(Op::F64Sqrt),
            0xA0 => Parsed::Op(Op::F64Add),
            0xA1 => Parsed::Op(Op::F64Sub),
            0xA2 => Parsed::Op(Op::F64Mul),
            0xA3 => Parsed::Op(Op::F64Div),
            0xA4 => Parsed::Op(Op::F64Min),
            0xA5 => Parsed::Op(Op::F64Max),
            0xA6 => Parsed::Op(Op::F64Copysign),
            0xA7 => Parsed::Op(Op::I32WrapI64),
            0xA8 => Parsed::Op(Op::I32TruncF32S),
            0xA9 => Parsed::Op(Op::I32TruncF32U),
            0xAA => Parsed::Op(Op::I32TruncF64S),
            0xAB => Parsed::Op(Op::I32TruncF64U),
            0xAC => Parsed::Op(Op::I64ExtendI32S),
            0xAD => Parsed::Op(Op::I64ExtendI32U),
            0xAE => Parsed::Op(Op::I64TruncF32S),
            0xAF => Parsed::Op(Op::I64TruncF32U),
            0xB0 => Parsed::Op(Op::I64TruncF64S),
            0xB1 => Parsed::Op(Op::I64TruncF64U),
            0xB2 => Parsed::Op(Op::F32ConvertI32S),
            0xB3 => Parsed::Op(Op::F32ConvertI32U),
            0xB4 => Parsed::Op(Op::F32ConvertI64S),
            0xB5 => Parsed::Op(Op::F32ConvertI64U),
            0xB6 => Parsed::Op(Op::F32DemoteF64),
            0xB7 => Parsed::Op(Op::F64ConvertI32S),
            0xB8 => Parsed::Op(Op::F64ConvertI32U),
            0xB9 => Parsed::Op(Op::F64ConvertI64S),
            0xBA => Parsed::Op(Op::F64ConvertI64U),
            0xBB => Parsed::Op(Op::F64PromoteF32),
            0xBC => Parsed::Op(Op::I32ReinterpretF32),
            0xBD => Parsed::Op(Op::I64ReinterpretF64),
            0xBE => Parsed::Op(Op::F32ReinterpretI32),
            0xBF => Parsed::Op(Op::F64ReinterpretI64),
            b => return malformed(format!("unknown opcode {b:#x}")),
        };
        ops.push(parsed);
    }
}
