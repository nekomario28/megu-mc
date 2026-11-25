// SPDX-License-Identifier: LGPL-3.0-or-later

//! Spec-algorithm MVP validation plus lowering.
//!
//! The type checker walks each function's `Parsed` stream with the
//! control-frame algorithm from the WebAssembly spec appendix (value-type
//! stack + unreachable polymorphism). While walking, it erases structured
//! control (`block`/`loop`/`if`/`else`/`end`) and rewrites every branch
//! into an absolute [`JumpDest`] carrying its stack fixup
//! (`keep` top values, truncate to `height`). The interpreter therefore
//! needs no control stack at all.

use crate::decode::{Parsed, RawModule};
use crate::error::LoadError;
use crate::module::*;

type VResult<T> = Result<T, LoadError>;

fn invalid<T>(msg: impl Into<String>) -> VResult<T> {
    Err(LoadError::Invalid(msg.into()))
}

/// `None` is the spec algorithm's "unknown" type (popped past a frame
/// floor while `unreachable`).
type MT = Option<ValType>;

#[derive(Clone, Copy, PartialEq)]
enum FKind {
    Block,
    Loop,
    If,
    Func,
}

/// Which JumpDest slot inside an emitted op still needs its `pc` patched.
#[derive(Clone, Copy)]
enum Slot {
    Whole,
    Table(usize),
    Default,
}

struct Frame {
    kind: FKind,
    /// Result types of the block; for loops these are the *end* types —
    /// branch arity comes from `label_types` (empty for loops in MVP).
    results: Vec<ValType>,
    /// Value-stack height at frame entry.
    height: usize,
    unreachable: bool,
    else_seen: bool,
    /// Ops that branch to this frame's `end` and need their target `pc`
    /// patched once it is known.
    pending: Vec<(usize, Slot)>,
    /// For loops: pc of the first body op (branch target).
    loop_start: usize,
    /// For `if`: out-index of the emitted `JumpIfZero` awaiting its
    /// else/end target patch.
    if_op: Option<usize>,
}

impl Frame {
    /// Types a branch to this label must carry.
    fn label_types(&self) -> &[ValType] {
        if self.kind == FKind::Loop {
            &[]
        } else {
            &self.results
        }
    }
}

struct Fv<'m> {
    m: &'m RawModule,
    vals: Vec<MT>,
    ctrls: Vec<Frame>,
    out: Vec<Op>,
}

impl<'m> Fv<'m> {
    fn push(&mut self, t: MT) {
        self.vals.push(t);
    }

    fn push_type(&mut self, t: ValType) {
        self.vals.push(Some(t));
    }

    fn pop(&mut self) -> VResult<MT> {
        let frame = match self.ctrls.last() {
            Some(f) => f,
            None => return invalid("operand stack underflow"),
        };
        if self.vals.len() == frame.height {
            if frame.unreachable {
                return Ok(None);
            }
            return invalid("operand stack underflow");
        }
        Ok(self.vals.pop().unwrap_or(None))
    }

    fn pop_expect(&mut self, expect: ValType) -> VResult<MT> {
        let actual = self.pop()?;
        if let Some(a) = actual {
            if a != expect {
                return invalid(format!("type mismatch: expected {expect:?}, found {a:?}"));
            }
        }
        Ok(actual.or(Some(expect)))
    }

    fn pop_vals(&mut self, types: &[ValType]) -> VResult<Vec<MT>> {
        let mut popped = Vec::with_capacity(types.len());
        for t in types.iter().rev() {
            popped.push(self.pop_expect(*t)?);
        }
        popped.reverse();
        Ok(popped)
    }

    fn push_ctrl(&mut self, kind: FKind, results: Vec<ValType>) {
        self.ctrls.push(Frame {
            kind,
            results,
            height: self.vals.len(),
            unreachable: false,
            else_seen: false,
            pending: Vec::new(),
            loop_start: self.out.len(),
            if_op: None,
        });
    }

    fn pop_ctrl(&mut self) -> VResult<Frame> {
        let (results, height) = match self.ctrls.last() {
            Some(f) => (f.results.clone(), f.height),
            None => return invalid("control stack underflow"),
        };
        self.pop_vals(&results)?;
        if self.vals.len() != height {
            return invalid("stack height mismatch at block end");
        }
        let frame = match self.ctrls.pop() {
            Some(f) => f,
            None => return invalid("control stack underflow"),
        };
        for t in &frame.results {
            self.vals.push(Some(*t));
        }
        Ok(frame)
    }

    fn set_unreachable(&mut self) {
        if let Some(f) = self.ctrls.last_mut() {
            self.vals.truncate(f.height);
            f.unreachable = true;
        }
    }

    fn frame(&self, depth: u32) -> VResult<&Frame> {
        let len = self.ctrls.len();
        if (depth as usize) >= len {
            return invalid(format!("branch depth {depth} out of range"));
        }
        Ok(&self.ctrls[len - 1 - depth as usize])
    }

    /// Register the last-emitted op as branching to frame `depth`; loop
    /// targets patch immediately, block/if targets join `pending`.
    fn emit_branch(&mut self, depth: u32, slot: Slot) -> VResult<()> {
        let len = self.ctrls.len();
        if (depth as usize) >= len {
            return invalid(format!("branch depth {depth} out of range"));
        }
        let fi = len - 1 - depth as usize;
        let idx = self.out.len() - 1;
        if self.ctrls[fi].kind == FKind::Loop {
            let pc = self.ctrls[fi].loop_start as u32;
            set_slot_pc(&mut self.out[idx], slot, pc);
        } else {
            self.ctrls[fi].pending.push((idx, slot));
        }
        Ok(())
    }

    /// Patch all branches that targeted `frame`'s end to land on `pc`.
    fn patch_frame(&mut self, frame: &Frame, pc: u32) {
        for (idx, slot) in &frame.pending {
            set_slot_pc(&mut self.out[*idx], *slot, pc);
        }
    }
}

fn set_slot_pc(op: &mut Op, slot: Slot, pc: u32) {
    match (op, slot) {
        (Op::Jump(d) | Op::JumpIf(d), Slot::Whole) => d.pc = pc,
        (Op::JumpIfZero { pc: p }, Slot::Whole) => *p = pc,
        (Op::JumpTable { targets, .. }, Slot::Table(i)) => {
            if let Some(d) = targets.get_mut(i) {
                d.pc = pc;
            }
        }
        (Op::JumpTable { default, .. }, Slot::Default) => default.pc = pc,
        _ => {}
    }
}

/// Validate a whole decoded module, producing executable op lists.
pub(crate) fn validate(raw: RawModule) -> VResult<Module> {
    // Global index checks.
    for imp in &raw.imports {
        match &imp.desc {
            ImportDesc::Func(ty) => {
                if *ty as usize >= raw.types.len() {
                    return invalid("import func type index out of range");
                }
            }
            _ => {
                return Err(LoadError::Unsupported(format!(
                    "non-function import `{}.{}`",
                    imp.module, imp.name
                )))
            }
        }
    }
    for ty in &raw.func_types {
        if *ty as usize >= raw.types.len() {
            return invalid("function type index out of range");
        }
    }
    let total_funcs = raw.import_func_count() + raw.func_types.len();
    for e in &raw.exports {
        let ok = match e.kind {
            ExternKind::Func => (e.index as usize) < total_funcs,
            ExternKind::Table => (e.index as usize) < raw.tables.len(),
            ExternKind::Memory => (e.index as usize) < raw.mems.len(),
            ExternKind::Global => (e.index as usize) < raw.globals.len(),
        };
        if !ok {
            return invalid(format!("export `{}` index out of range", e.name));
        }
    }
    if let Some(s) = raw.start {
        let ok = raw.func_type_index(s).is_some_and(|t| {
            let ft = &raw.types[t as usize];
            ft.params.is_empty() && ft.results.is_empty()
        });
        if !ok {
            return invalid("start function must exist and have type [] -> []");
        }
    }
    for (i, g) in raw.globals.iter().enumerate() {
        let ty_ok = match g.init {
            ConstExpr::I32(_) => g.ty == ValType::I32,
            ConstExpr::I64(_) => g.ty == ValType::I64,
            ConstExpr::F32(_) => g.ty == ValType::F32,
            ConstExpr::F64(_) => g.ty == ValType::F64,
            ConstExpr::GlobalGet(idx) => {
                // Only earlier immutable globals may be referenced (we
                // import none, so this is the strictest MVP reading).
                (idx as usize) < i
                    && !raw.globals[idx as usize].mutable
                    && raw.globals[idx as usize].ty == g.ty
            }
        };
        if !ty_ok {
            return invalid("global init expression type mismatch");
        }
    }
    for e in &raw.elems {
        if (e.table as usize) >= raw.tables.len() {
            return invalid("elem segment table index out of range");
        }
        if !matches!(e.offset, ConstExpr::I32(_)) {
            return invalid("elem offset must be i32");
        }
        for f in &e.funcs {
            if (*f as usize) >= total_funcs {
                return invalid("elem segment func index out of range");
            }
        }
    }
    for d in &raw.datas {
        if (d.mem as usize) >= raw.mems.len() {
            return invalid("data segment memory index out of range");
        }
        if !matches!(d.offset, ConstExpr::I32(_)) {
            return invalid("data offset must be i32");
        }
    }

    let mut raw = raw;
    let bodies = std::mem::take(&mut raw.bodies);
    let mut funcs = Vec::with_capacity(bodies.len());
    let mut locals_out = Vec::with_capacity(bodies.len());
    for (i, (locals, parsed)) in bodies.into_iter().enumerate() {
        let fty = raw.types[raw.func_types[i] as usize].clone();
        let ops = validate_func(&raw, &fty, &locals, parsed)
            .map_err(|e| LoadError::Invalid(format!("func {i}: {e}")))?;
        funcs.push(ops);
        locals_out.push(locals);
    }

    Ok(Module {
        types: raw.types,
        imports: raw.imports,
        func_types: raw.func_types,
        funcs,
        locals: locals_out,
        tables: raw.tables,
        mems: raw.mems,
        globals: raw.globals,
        exports: raw.exports,
        start: raw.start,
        elems: raw.elems,
        datas: raw.datas,
    })
}

fn validate_func(
    m: &RawModule,
    fty: &FuncType,
    locals: &[ValType],
    parsed: Vec<Parsed>,
) -> VResult<Vec<Op>> {
    let mut all_locals = fty.params.clone();
    all_locals.extend_from_slice(locals);

    let mut v = Fv {
        m,
        vals: Vec::new(),
        ctrls: Vec::new(),
        out: Vec::new(),
    };
    // Implicit function frame: branches to the outermost depth return.
    v.push_ctrl(FKind::Func, fty.results.clone());

    let has_mem = !m.mems.is_empty();

    for item in parsed {
        match item {
            Parsed::Block { result } => {
                v.push_ctrl(FKind::Block, result.into_iter().collect());
            }
            Parsed::Loop { result } => {
                v.push_ctrl(FKind::Loop, result.into_iter().collect());
            }
            Parsed::If { result } => {
                v.pop_expect(ValType::I32)?;
                v.push_ctrl(FKind::If, result.into_iter().collect());
                // The JumpIfZero's target is patched at `else` (else-arm
                // start) or `end` (no else). No stack fixup: the height
                // after popping the condition equals the block height.
                v.out.push(Op::JumpIfZero { pc: 0 });
                if let Some(f) = v.ctrls.last_mut() {
                    f.if_op = Some(v.out.len() - 1);
                }
            }
            Parsed::Else => {
                match v.ctrls.last().map(|f| (f.kind, f.else_seen)) {
                    Some((FKind::If, false)) => {}
                    _ => return invalid("`else` outside `if`"),
                }
                // pop_ctrl's steps minus the result push-back: the else
                // arm re-enters the same block at the same height.
                let frame = {
                    let (results, height) = match v.ctrls.last() {
                        Some(f) => (f.results.clone(), f.height),
                        None => return invalid("control stack underflow"),
                    };
                    v.pop_vals(&results)?;
                    if v.vals.len() != height {
                        return invalid("stack height mismatch at `else`");
                    }
                    match v.ctrls.pop() {
                        Some(f) => f,
                        None => return invalid("control stack underflow"),
                    }
                };
                // Fall-through from the then-arm: keep the results and
                // jump over the else arm (patched at End).
                v.out.push(Op::Jump(JumpDest {
                    pc: 0,
                    keep: frame.results.len() as u16,
                    height: frame.height as u32,
                }));
                let jidx = v.out.len() - 1;
                // Patch the If's JumpIfZero to the else-arm start.
                if let Some(io) = frame.if_op {
                    let pc = v.out.len() as u32;
                    set_slot_pc(&mut v.out[io], Slot::Whole, pc);
                }
                // Re-push the if frame for the else arm at the ORIGINAL
                // height, inheriting pending branches to this block's end.
                let mut pending = frame.pending;
                pending.push((jidx, Slot::Whole));
                v.ctrls.push(Frame {
                    kind: FKind::If,
                    results: frame.results,
                    height: frame.height,
                    unreachable: false,
                    else_seen: true,
                    pending,
                    loop_start: 0,
                    if_op: None,
                });
            }
            Parsed::End => {
                let frame = v.pop_ctrl()?;
                if frame.kind == FKind::If && !frame.else_seen && !frame.results.is_empty() {
                    return invalid("`if` with results requires `else`");
                }
                let pc = v.out.len() as u32;
                v.patch_frame(&frame, pc);
                if let Some(io) = frame.if_op {
                    set_slot_pc(&mut v.out[io], Slot::Whole, pc);
                }
            }
            Parsed::Op(op) => {
                apply_op(&mut v, op, &all_locals, fty, has_mem)?;
            }
        }
    }

    if !v.ctrls.is_empty() {
        return invalid("unbalanced control frames");
    }
    Ok(v.out)
}

fn apply_op(
    v: &mut Fv<'_>,
    op: Op,
    locals: &[ValType],
    fty: &FuncType,
    has_mem: bool,
) -> VResult<()> {
    const I: ValType = ValType::I32;
    const L: ValType = ValType::I64;
    const F: ValType = ValType::F32;
    const D: ValType = ValType::F64;

    macro_rules! simple {
        ($pops:expr, $pushes:expr, $emit:expr) => {{
            v.pop_vals($pops)?;
            for t in $pushes {
                v.push_type(*t);
            }
            v.out.push($emit);
        }};
    }
    macro_rules! need_mem {
        () => {
            if !has_mem {
                return invalid("memory op without a memory");
            }
        };
    }

    match op {
        Op::Unreachable => v.set_unreachable(),
        Op::Nop => v.out.push(Op::Nop),
        Op::Br(depth) => {
            let lt = v.frame(depth)?.label_types().to_vec();
            let height = v.frame(depth)?.height as u32;
            v.pop_vals(&lt)?;
            v.out.push(Op::Jump(JumpDest {
                pc: 0,
                keep: lt.len() as u16,
                height,
            }));
            v.emit_branch(depth, Slot::Whole)?;
            v.set_unreachable();
        }
        Op::BrIf(depth) => {
            v.pop_expect(I)?;
            let lt = v.frame(depth)?.label_types().to_vec();
            let height = v.frame(depth)?.height as u32;
            v.pop_vals(&lt)?;
            for t in &lt {
                v.push_type(*t);
            }
            v.out.push(Op::JumpIf(JumpDest {
                pc: 0,
                keep: lt.len() as u16,
                height,
            }));
            v.emit_branch(depth, Slot::Whole)?;
        }
        Op::BrTable { targets, default } => {
            v.pop_expect(I)?;
            let arity = v.frame(default)?.label_types().len();
            let mut dests = Vec::with_capacity(targets.len());
            for d in targets.iter() {
                let lt = v.frame(*d)?.label_types().to_vec();
                if lt.len() != arity {
                    return invalid("br_table targets have different arities");
                }
                let popped = v.pop_vals(&lt)?;
                for t in popped {
                    v.push(t);
                }
                let height = v.frame(*d)?.height as u32;
                dests.push(JumpDest {
                    pc: 0,
                    keep: arity as u16,
                    height,
                });
            }
            let dlt = v.frame(default)?.label_types().to_vec();
            v.pop_vals(&dlt)?;
            let dheight = v.frame(default)?.height as u32;
            v.out.push(Op::JumpTable {
                targets: dests.into_boxed_slice(),
                default: JumpDest {
                    pc: 0,
                    keep: arity as u16,
                    height: dheight,
                },
            });
            for (i, d) in targets.iter().enumerate() {
                v.emit_branch(*d, Slot::Table(i))?;
            }
            v.emit_branch(default, Slot::Default)?;
            v.set_unreachable();
        }
        Op::Return => {
            let results = fty.results.clone();
            v.pop_vals(&results)?;
            v.out.push(Op::Return);
            v.set_unreachable();
        }
        Op::Call { func } => {
            let ty_idx = v
                .m
                .func_type_index(func)
                .ok_or_else(|| LoadError::Invalid("call func index out of range".into()))?;
            let ft = v.m.types[ty_idx as usize].clone();
            v.pop_vals(&ft.params)?;
            for t in &ft.results {
                v.push_type(*t);
            }
            v.out.push(Op::Call { func });
        }
        Op::CallIndirect { ty } => {
            if v.m.tables.is_empty() {
                return invalid("call_indirect without a table");
            }
            let ft = v
                .m
                .types
                .get(ty as usize)
                .cloned()
                .ok_or_else(|| LoadError::Invalid("call_indirect type index".into()))?;
            v.pop_expect(I)?;
            v.pop_vals(&ft.params)?;
            for t in &ft.results {
                v.push_type(*t);
            }
            v.out.push(Op::CallIndirect { ty });
        }
        Op::Drop => {
            v.pop()?;
            v.out.push(Op::Drop);
        }
        Op::Select => {
            v.pop_expect(I)?;
            let t2 = v.pop()?;
            let t1 = v.pop()?;
            if let (Some(a), Some(b)) = (t1, t2) {
                if a != b {
                    return invalid("select operand types differ");
                }
            }
            v.push(t1.or(t2));
            v.out.push(Op::Select);
        }
        Op::LocalGet(i) => {
            let t = *locals
                .get(i as usize)
                .ok_or_else(|| LoadError::Invalid("local.get index".into()))?;
            v.push_type(t);
            v.out.push(Op::LocalGet(i));
        }
        Op::LocalSet(i) => {
            let t = *locals
                .get(i as usize)
                .ok_or_else(|| LoadError::Invalid("local.set index".into()))?;
            v.pop_expect(t)?;
            v.out.push(Op::LocalSet(i));
        }
        Op::LocalTee(i) => {
            let t = *locals
                .get(i as usize)
                .ok_or_else(|| LoadError::Invalid("local.tee index".into()))?;
            v.pop_expect(t)?;
            v.push_type(t);
            v.out.push(Op::LocalTee(i));
        }
        Op::GlobalGet(i) => {
            let g = v
                .m
                .globals
                .get(i as usize)
                .ok_or_else(|| LoadError::Invalid("global.get index".into()))?;
            let ty = g.ty;
            v.push_type(ty);
            v.out.push(Op::GlobalGet(i));
        }
        Op::GlobalSet(i) => {
            let g = v
                .m
                .globals
                .get(i as usize)
                .ok_or_else(|| LoadError::Invalid("global.set index".into()))?;
            if !g.mutable {
                return invalid("global.set on immutable global");
            }
            let ty = g.ty;
            v.pop_expect(ty)?;
            v.out.push(Op::GlobalSet(i));
        }
        // Loads.
        Op::I32Load { .. }
        | Op::I32Load8S { .. }
        | Op::I32Load8U { .. }
        | Op::I32Load16S { .. }
        | Op::I32Load16U { .. } => {
            need_mem!();
            simple!(&[I], &[I], op);
        }
        Op::I64Load { .. }
        | Op::I64Load8S { .. }
        | Op::I64Load8U { .. }
        | Op::I64Load16S { .. }
        | Op::I64Load16U { .. }
        | Op::I64Load32S { .. }
        | Op::I64Load32U { .. } => {
            need_mem!();
            simple!(&[I], &[L], op);
        }
        Op::F32Load { .. } => {
            need_mem!();
            simple!(&[I], &[F], op);
        }
        Op::F64Load { .. } => {
            need_mem!();
            simple!(&[I], &[D], op);
        }
        // Stores.
        Op::I32Store { .. } | Op::I32Store8 { .. } | Op::I32Store16 { .. } => {
            need_mem!();
            simple!(&[I, I], &[], op);
        }
        Op::I64Store { .. }
        | Op::I64Store8 { .. }
        | Op::I64Store16 { .. }
        | Op::I64Store32 { .. } => {
            need_mem!();
            simple!(&[I, L], &[], op);
        }
        Op::F32Store { .. } => {
            need_mem!();
            simple!(&[I, F], &[], op);
        }
        Op::F64Store { .. } => {
            need_mem!();
            simple!(&[I, D], &[], op);
        }
        Op::MemSize => {
            need_mem!();
            simple!(&[], &[I], Op::MemSize);
        }
        Op::MemGrow => {
            need_mem!();
            simple!(&[I], &[I], Op::MemGrow);
        }
        Op::ConstI32(_) => simple!(&[], &[I], op),
        Op::ConstI64(_) => simple!(&[], &[L], op),
        Op::ConstF32(_) => simple!(&[], &[F], op),
        Op::ConstF64(_) => simple!(&[], &[D], op),
        // Numeric: type-uniform binary ops.
        Op::I32Add | Op::I32Sub | Op::I32Mul | Op::I32DivS | Op::I32DivU | Op::I32RemS
        | Op::I32RemU | Op::I32And | Op::I32Or | Op::I32Xor | Op::I32Shl | Op::I32ShrS
        | Op::I32ShrU | Op::I32Rotl | Op::I32Rotr => {
            simple!(&[I, I], &[I], op);
        }
        Op::I64Add | Op::I64Sub | Op::I64Mul | Op::I64DivS | Op::I64DivU | Op::I64RemS
        | Op::I64RemU | Op::I64And | Op::I64Or | Op::I64Xor | Op::I64Shl | Op::I64ShrS
        | Op::I64ShrU | Op::I64Rotl | Op::I64Rotr => {
            simple!(&[L, L], &[L], op);
        }
        Op::F32Add | Op::F32Sub | Op::F32Mul | Op::F32Div | Op::F32Min | Op::F32Max
        | Op::F32Copysign => {
            simple!(&[F, F], &[F], op);
        }
        Op::F64Add | Op::F64Sub | Op::F64Mul | Op::F64Div | Op::F64Min | Op::F64Max
        | Op::F64Copysign => {
            simple!(&[D, D], &[D], op);
        }
        // Numeric: type-uniform unary ops.
        Op::I32Clz | Op::I32Ctz | Op::I32Popcnt | Op::I32Eqz => {
            simple!(&[I], &[I], op);
        }
        Op::I64Clz | Op::I64Ctz | Op::I64Popcnt | Op::I64Eqz => {
            simple!(&[L], &[I], op);
        }
        Op::F32Abs | Op::F32Neg | Op::F32Ceil | Op::F32Floor | Op::F32Trunc
        | Op::F32Nearest | Op::F32Sqrt => {
            simple!(&[F], &[F], op);
        }
        Op::F64Abs | Op::F64Neg | Op::F64Ceil | Op::F64Floor | Op::F64Trunc
        | Op::F64Nearest | Op::F64Sqrt => {
            simple!(&[D], &[D], op);
        }
        // Comparisons.
        Op::I32Eq | Op::I32Ne | Op::I32LtS | Op::I32LtU | Op::I32GtS | Op::I32GtU
        | Op::I32LeS | Op::I32LeU | Op::I32GeS | Op::I32GeU => {
            simple!(&[I, I], &[I], op);
        }
        Op::I64Eq | Op::I64Ne | Op::I64LtS | Op::I64LtU | Op::I64GtS | Op::I64GtU
        | Op::I64LeS | Op::I64LeU | Op::I64GeS | Op::I64GeU => {
            simple!(&[L, L], &[I], op);
        }
        Op::F32Eq | Op::F32Ne | Op::F32Lt | Op::F32Gt | Op::F32Le | Op::F32Ge => {
            simple!(&[F, F], &[I], op);
        }
        Op::F64Eq | Op::F64Ne | Op::F64Lt | Op::F64Gt | Op::F64Le | Op::F64Ge => {
            simple!(&[D, D], &[I], op);
        }
        // Conversions.
        Op::I32WrapI64 => simple!(&[L], &[I], op),
        Op::I32TruncF32S | Op::I32TruncF32U => simple!(&[F], &[I], op),
        Op::I32TruncF64S | Op::I32TruncF64U => simple!(&[D], &[I], op),
        Op::I64ExtendI32S | Op::I64ExtendI32U => simple!(&[I], &[L], op),
        Op::I64TruncF32S | Op::I64TruncF32U => simple!(&[F], &[L], op),
        Op::I64TruncF64S | Op::I64TruncF64U => simple!(&[D], &[L], op),
        Op::F32ConvertI32S | Op::F32ConvertI32U => simple!(&[I], &[F], op),
        Op::F32ConvertI64S | Op::F32ConvertI64U => simple!(&[L], &[F], op),
        Op::F32DemoteF64 => simple!(&[D], &[F], op),
        Op::F64ConvertI32S | Op::F64ConvertI32U => simple!(&[I], &[D], op),
        Op::F64ConvertI64S | Op::F64ConvertI64U => simple!(&[L], &[D], op),
        Op::F64PromoteF32 => simple!(&[F], &[D], op),
        Op::I32ReinterpretF32 => simple!(&[F], &[I], op),
        Op::I64ReinterpretF64 => simple!(&[D], &[L], op),
        Op::F32ReinterpretI32 => simple!(&[I], &[F], op),
        Op::F64ReinterpretI64 => simple!(&[L], &[D], op),
        Op::Jump(_) | Op::JumpIf(_) | Op::JumpIfZero { .. } | Op::JumpTable { .. } => {
            return invalid("internal: resolved jump reached validator");
        }
    }
    Ok(())
}
