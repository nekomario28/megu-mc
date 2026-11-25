// SPDX-License-Identifier: LGPL-3.0-or-later

//! The fuel-metered MVP interpreter.
//!
//! One [`Vm`] executes one guest call on an iterative frame loop — no host
//! stack recursion, so guest recursion can only exhaust the bounded frame
//! vector (a trap), never the server stack. Every executed op costs
//! exactly one unit of fuel; memory access is bounds-checked against the
//! allocated region.

use megu_core::coords::{ChunkKey, DimIndex, SectionKey};
use megu_core::rev::UiLayer;

use crate::abi::{CapabilitySet, HostBinding, MeguHost};
use crate::error::Trap;
use crate::module::*;

const PAGE: usize = 65_536;

type XResult<T> = Result<T, Trap>;

/// Bounded linear memory. `data.len()` is always a multiple of
/// [`PAGE`]; allocation happens once at instantiation and only grows
/// through `memory.grow`, never past `max_pages`.
pub(crate) struct LinearMem {
    pub data: Vec<u8>,
    pub max_pages: u32,
}

impl LinearMem {
    pub fn pages(&self) -> u32 {
        (self.data.len() / PAGE) as u32
    }

    fn load(&self, base: i32, off: u32, n: usize) -> XResult<&[u8]> {
        let ea = (base as u32 as u64) + u64::from(off);
        let end = ea + n as u64;
        if end > self.data.len() as u64 {
            return Err(Trap::MemoryOutOfBounds {
                addr: ea,
                len: self.data.len() as u64,
            });
        }
        Ok(&self.data[ea as usize..end as usize])
    }

    fn store(&mut self, base: i32, off: u32, bytes: &[u8]) -> XResult<()> {
        let ea = (base as u32 as u64) + u64::from(off);
        let end = ea + bytes.len() as u64;
        if end > self.data.len() as u64 {
            return Err(Trap::MemoryOutOfBounds {
                addr: ea,
                len: self.data.len() as u64,
            });
        }
        self.data[ea as usize..end as usize].copy_from_slice(bytes);
        Ok(())
    }

    /// Host-side bounded read for `ptr`/`len` string arguments.
    pub fn read(&self, ptr: u32, len: u32) -> XResult<&[u8]> {
        let end = u64::from(ptr) + u64::from(len);
        if end > self.data.len() as u64 {
            return Err(Trap::HostMemoryAccess { ptr, len });
        }
        Ok(&self.data[ptr as usize..end as usize])
    }

    fn grow(&mut self, delta: u32) -> i32 {
        let old = self.pages();
        let Some(new) = old.checked_add(delta) else {
            return -1;
        };
        if new > self.max_pages {
            return -1;
        }
        self.data.resize(new as usize * PAGE, 0);
        old as i32
    }
}

/// A resolved function index: either a host import or a module-local
/// function body.
#[derive(Clone, Copy)]
pub(crate) enum FuncAddr {
    Host {
        binding: HostBinding,
        params: usize,
        results: usize,
    },
    Wasm {
        def: usize,
        params: usize,
        results: usize,
    },
}

/// Everything instantiation produces; owned by `WasmMod`.
pub(crate) struct Instance {
    pub module: Module,
    pub funcs: Vec<FuncAddr>,
    pub mem: LinearMem,
    pub globals: Vec<Value>,
    pub table: Vec<Option<u32>>,
}

pub(crate) struct Frame {
    pub def: usize,
    pub pc: usize,
    pub locals: Vec<Value>,
    /// Value-stack height at frame entry (args already consumed).
    pub base: usize,
    pub retc: usize,
}

/// One guest invocation in flight.
pub(crate) struct Vm<'m> {
    pub m: &'m Module,
    pub funcs: &'m [FuncAddr],
    pub mem: &'m mut LinearMem,
    pub globals: &'m mut Vec<Value>,
    pub table: &'m mut Vec<Option<u32>>,
    pub host: &'m mut dyn MeguHost,
    pub caps: CapabilitySet,
    pub fuel: u64,
    pub max_frames: usize,
    pub max_string: u32,
    pub stack: Vec<Value>,
    pub frames: Vec<Frame>,
}

impl<'m> Vm<'m> {
    /// Invoke func index `f` with `args`; returns its result values.
    pub fn invoke(&mut self, f: u32, args: &[Value]) -> XResult<Vec<Value>> {
        match self.funcs.get(f as usize).copied() {
            Some(FuncAddr::Host {
                binding,
                params,
                results,
            }) => {
                if args.len() != params {
                    return Err(Trap::Unreachable);
                }
                let r = self.host_call(binding, args)?;
                debug_assert_eq!(r.len(), results);
                Ok(r)
            }
            Some(FuncAddr::Wasm {
                def,
                params,
                results,
            }) => {
                if args.len() != params {
                    return Err(Trap::Unreachable);
                }
                let mut locals = args.to_vec();
                for t in &self.m.locals[def] {
                    locals.push(t.zero());
                }
                self.frames.push(Frame {
                    def,
                    pc: 0,
                    locals,
                    base: self.stack.len(),
                    retc: results,
                });
                self.run()?;
                let n = self.stack.len();
                Ok(self.stack.split_off(n - results))
            }
            None => Err(Trap::Unreachable),
        }
    }

    fn run(&mut self) -> XResult<()> {
        while !self.frames.is_empty() {
            let (def, pc) = {
                let f = self.frames.last().map(|f| (f.def, f.pc));
                match f {
                    Some(v) => v,
                    None => break,
                }
            };
            let ops = &self.m.funcs[def];
            if pc >= ops.len() {
                self.finish_frame();
                continue;
            }
            let op = &ops[pc];
            if let Some(f) = self.frames.last_mut() {
                f.pc += 1;
            }
            self.fuel = self.fuel.checked_sub(1).ok_or(Trap::FuelExhausted)?;
            self.step(op)?;
        }
        Ok(())
    }

    fn finish_frame(&mut self) {
        let f = match self.frames.pop() {
            Some(f) => f,
            None => return,
        };
        let len = self.stack.len();
        let mut results = self.stack.split_off(len.saturating_sub(f.retc));
        self.stack.truncate(f.base);
        self.stack.append(&mut results);
    }

    /// Apply a resolved branch: carry the top `keep` values down to the
    /// frame-relative `height`, truncate, jump to `pc`.
    fn jump(&mut self, d: JumpDest) {
        let base = self.frames.last().map_or(0, |f| f.base);
        let h = base + d.height as usize;
        let keep = d.keep as usize;
        let src = self.stack.len() - keep;
        for i in 0..keep {
            self.stack[h + i] = self.stack[src + i];
        }
        self.stack.truncate(h + keep);
        if let Some(f) = self.frames.last_mut() {
            f.pc = d.pc as usize;
        }
    }

    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }
    fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or(Value::I32(0))
    }
    fn pop_i32(&mut self) -> i32 {
        self.pop().i32()
    }
    fn pop_i64(&mut self) -> i64 {
        self.pop().i64()
    }
    fn pop_f32(&mut self) -> f32 {
        self.pop().f32()
    }
    fn pop_f64(&mut self) -> f64 {
        self.pop().f64()
    }
    fn peek(&self) -> Value {
        self.stack.last().copied().unwrap_or(Value::I32(0))
    }

    fn host_call(&mut self, b: HostBinding, args: &[Value]) -> XResult<Vec<Value>> {
        match b {
            HostBinding::Log => {
                let ptr = args.first().map_or(0, |v| v.i32()) as u32;
                let len = args.get(1).map_or(0, |v| v.i32()) as u32;
                if len > self.max_string {
                    return Err(Trap::HostMemoryAccess { ptr, len });
                }
                let bytes = self.mem.read(ptr, len)?;
                let msg = String::from_utf8_lossy(bytes).into_owned();
                self.host.log(&msg);
                Ok(Vec::new())
            }
            HostBinding::GetBlock => {
                if let Some(c) = b.capability() {
                    self.caps.require(c, "host_get_block")?;
                }
                let r = self.host.get_block(
                    args.first().map_or(0, |v| v.i32()),
                    args.get(1).map_or(0, |v| v.i32()),
                    args.get(2).map_or(0, |v| v.i32()),
                );
                Ok(vec![Value::I32(r)])
            }
            HostBinding::SetBlock => {
                if let Some(c) = b.capability() {
                    self.caps.require(c, "host_set_block")?;
                }
                let r = self.host.set_block(
                    args.first().map_or(0, |v| v.i32()),
                    args.get(1).map_or(0, |v| v.i32()),
                    args.get(2).map_or(0, |v| v.i32()),
                    args.get(3).map_or(0, |v| v.i32()),
                );
                Ok(vec![Value::I32(r)])
            }
            HostBinding::EmitChat => {
                if let Some(c) = b.capability() {
                    self.caps.require(c, "host_emit_chat")?;
                }
                let player = args.first().map_or(0, |v| v.i32());
                let ptr = args.get(1).map_or(0, |v| v.i32()) as u32;
                let len = args.get(2).map_or(0, |v| v.i32()) as u32;
                if len > self.max_string {
                    return Err(Trap::HostMemoryAccess { ptr, len });
                }
                let bytes = self.mem.read(ptr, len)?;
                let msg = String::from_utf8_lossy(bytes).into_owned();
                let r = self.host.emit_chat(player, &msg);
                Ok(vec![Value::I32(r)])
            }
            HostBinding::SectionRev => {
                if let Some(c) = b.capability() {
                    self.caps.require(c, "megu_section_rev")?;
                }
                let key = SectionKey {
                    dim: DimIndex(args.first().map_or(0, |v| v.i32()) as u32),
                    cx: args.get(1).map_or(0, |v| v.i32()),
                    sy: args.get(2).map_or(0, |v| v.i32()),
                    cz: args.get(3).map_or(0, |v| v.i32()),
                };
                let rev = self.host.spine_section_rev(key).unwrap_or(0);
                Ok(vec![Value::I64(rev as i64)])
            }
            HostBinding::ChunkRev => {
                if let Some(c) = b.capability() {
                    self.caps.require(c, "megu_chunk_rev")?;
                }
                let key = ChunkKey {
                    dim: DimIndex(args.first().map_or(0, |v| v.i32()) as u32),
                    cx: args.get(1).map_or(0, |v| v.i32()),
                    cz: args.get(2).map_or(0, |v| v.i32()),
                };
                let rev = self.host.spine_chunk_rev(key).unwrap_or(0);
                Ok(vec![Value::I64(rev as i64)])
            }
            HostBinding::BumpBlock => {
                if let Some(c) = b.capability() {
                    self.caps.require(c, "megu_bump_block")?;
                }
                let rev = self
                    .host
                    .spine_bump_block(
                        DimIndex(args.first().map_or(0, |v| v.i32()) as u32),
                        args.get(1).map_or(0, |v| v.i32()),
                        args.get(2).map_or(0, |v| v.i32()),
                        args.get(3).map_or(0, |v| v.i32()),
                    )
                    .unwrap_or(0);
                Ok(vec![Value::I64(rev as i64)])
            }
            HostBinding::UiMarkDirty => {
                if let Some(c) = b.capability() {
                    self.caps.require(c, "megu_ui_mark_dirty")?;
                }
                let raw = args.first().map_or(-1, |v| v.i32());
                let status = match UiLayer::from_index(raw as usize) {
                    Some(layer @ (UiLayer::ModHud | UiLayer::ModOverlay)) => {
                        self.host.ui_mark_dirty(layer)
                    }
                    _ => -1,
                };
                Ok(vec![Value::I32(status)])
            }
        }
    }

    fn step(&mut self, op: &'m Op) -> XResult<()> {
        macro_rules! bin_i32 {
            ($f:expr) => {{
                let b = self.pop_i32();
                let a = self.pop_i32();
                let f: fn(i32, i32) -> i32 = $f;
                self.push(Value::I32(f(a, b)));
            }};
        }
        macro_rules! cmp_i32 {
            ($f:expr) => {{
                let b = self.pop_i32();
                let a = self.pop_i32();
                let f: fn(i32, i32) -> bool = $f;
                self.push(Value::I32(i32::from(f(a, b))));
            }};
        }
        macro_rules! cmp_i64 {
            ($f:expr) => {{
                let b = self.pop_i64();
                let a = self.pop_i64();
                let f: fn(i64, i64) -> bool = $f;
                self.push(Value::I32(i32::from(f(a, b))));
            }};
        }
        macro_rules! bin_i64 {
            ($f:expr) => {{
                let b = self.pop_i64();
                let a = self.pop_i64();
                let f: fn(i64, i64) -> i64 = $f;
                self.push(Value::I64(f(a, b)));
            }};
        }
        macro_rules! bin_f32 {
            ($f:expr) => {{
                let b = self.pop_f32();
                let a = self.pop_f32();
                let f: fn(f32, f32) -> f32 = $f;
                self.push(Value::F32(f(a, b)));
            }};
        }
        macro_rules! cmp_f32 {
            ($f:expr) => {{
                let b = self.pop_f32();
                let a = self.pop_f32();
                let f: fn(f32, f32) -> bool = $f;
                self.push(Value::I32(i32::from(f(a, b))));
            }};
        }
        macro_rules! bin_f64 {
            ($f:expr) => {{
                let b = self.pop_f64();
                let a = self.pop_f64();
                let f: fn(f64, f64) -> f64 = $f;
                self.push(Value::F64(f(a, b)));
            }};
        }
        macro_rules! cmp_f64 {
            ($f:expr) => {{
                let b = self.pop_f64();
                let a = self.pop_f64();
                let f: fn(f64, f64) -> bool = $f;
                self.push(Value::I32(i32::from(f(a, b))));
            }};
        }

        match op {
            Op::Unreachable => return Err(Trap::Unreachable),
            Op::Nop => {}
            Op::Jump(d) => self.jump(*d),
            Op::JumpIf(d) => {
                let c = self.pop_i32();
                if c != 0 {
                    self.jump(*d);
                }
            }
            Op::JumpIfZero { pc } => {
                let c = self.pop_i32();
                if c == 0 {
                    if let Some(f) = self.frames.last_mut() {
                        f.pc = *pc as usize;
                    }
                }
            }
            Op::JumpTable { targets, default } => {
                let i = self.pop_i32() as u32;
                let d = targets.get(i as usize).copied().unwrap_or(*default);
                self.jump(d);
            }
            Op::Return => self.finish_frame(),
            Op::Call { func } => {
                let addr = match self.funcs.get(*func as usize) {
                    Some(a) => *a,
                    None => return Err(Trap::Unreachable),
                };
                match addr {
                    FuncAddr::Host {
                        binding,
                        params,
                        results: _,
                    } => {
                        let len = self.stack.len();
                        let args = self.stack.split_off(len - params);
                        let r = self.host_call(binding, &args)?;
                        for v in r {
                            self.push(v);
                        }
                    }
                    FuncAddr::Wasm {
                        def,
                        params,
                        results,
                    } => {
                        if self.frames.len() >= self.max_frames {
                            return Err(Trap::StackOverflow);
                        }
                        let len = self.stack.len();
                        let mut locals = self.stack.split_off(len - params);
                        for t in &self.m.locals[def] {
                            locals.push(t.zero());
                        }
                        let base = self.stack.len();
                        self.frames.push(Frame {
                            def,
                            pc: 0,
                            locals,
                            base,
                            retc: results,
                        });
                    }
                }
            }
            Op::CallIndirect { ty } => {
                let i = self.pop_i32() as u32;
                let fidx = match self.table.get(i as usize) {
                    Some(Some(f)) => *f,
                    Some(None) => return Err(Trap::IndirectNull { index: i }),
                    None => {
                        return Err(Trap::MemoryOutOfBounds {
                            addr: u64::from(i),
                            len: self.table.len() as u64,
                        })
                    }
                };
                let actual = self.m.func_type_index(fidx).unwrap_or(u32::MAX);
                if actual != *ty {
                    return Err(Trap::IndirectSignature { index: i });
                }
                match self.funcs.get(fidx as usize).copied() {
                    Some(FuncAddr::Host {
                        binding,
                        params,
                        results: _,
                    }) => {
                        let len = self.stack.len();
                        let args = self.stack.split_off(len - params);
                        let r = self.host_call(binding, &args)?;
                        for v in r {
                            self.push(v);
                        }
                    }
                    Some(FuncAddr::Wasm {
                        def,
                        params,
                        results,
                    }) => {
                        if self.frames.len() >= self.max_frames {
                            return Err(Trap::StackOverflow);
                        }
                        let len = self.stack.len();
                        let mut locals = self.stack.split_off(len - params);
                        for t in &self.m.locals[def] {
                            locals.push(t.zero());
                        }
                        let base = self.stack.len();
                        self.frames.push(Frame {
                            def,
                            pc: 0,
                            locals,
                            base,
                            retc: results,
                        });
                    }
                    None => return Err(Trap::Unreachable),
                }
            }
            Op::Drop => {
                self.pop();
            }
            Op::Select => {
                let c = self.pop_i32();
                let b = self.pop();
                let a = self.pop();
                self.push(if c != 0 { a } else { b });
            }
            Op::LocalGet(i) => {
                let v = self
                    .frames
                    .last()
                    .and_then(|f| f.locals.get(*i as usize).copied())
                    .unwrap_or(Value::I32(0));
                self.push(v);
            }
            Op::LocalSet(i) => {
                let v = self.pop();
                if let Some(f) = self.frames.last_mut() {
                    if let Some(l) = f.locals.get_mut(*i as usize) {
                        *l = v;
                    }
                }
            }
            Op::LocalTee(i) => {
                let v = self.peek();
                if let Some(f) = self.frames.last_mut() {
                    if let Some(l) = f.locals.get_mut(*i as usize) {
                        *l = v;
                    }
                }
            }
            Op::GlobalGet(i) => {
                let v = self.globals.get(*i as usize).copied().unwrap_or(Value::I32(0));
                self.push(v);
            }
            Op::GlobalSet(i) => {
                let v = self.pop();
                if let Some(g) = self.globals.get_mut(*i as usize) {
                    *g = v;
                }
            }
            Op::I32Load { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 4)?;
                self.push(Value::I32(i32::from_le_bytes(b.try_into().unwrap_or([0; 4]))));
            }
            Op::I64Load { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 8)?;
                self.push(Value::I64(i64::from_le_bytes(b.try_into().unwrap_or([0; 8]))));
            }
            Op::F32Load { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 4)?;
                self.push(Value::F32(f32::from_le_bytes(b.try_into().unwrap_or([0; 4]))));
            }
            Op::F64Load { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 8)?;
                self.push(Value::F64(f64::from_le_bytes(b.try_into().unwrap_or([0; 8]))));
            }
            Op::I32Load8S { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 1)?;
                self.push(Value::I32(i32::from(b[0] as i8)));
            }
            Op::I32Load8U { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 1)?;
                self.push(Value::I32(i32::from(b[0])));
            }
            Op::I32Load16S { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 2)?;
                self.push(Value::I32(i32::from(i16::from_le_bytes(
                    b.try_into().unwrap_or([0; 2]),
                ))));
            }
            Op::I32Load16U { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 2)?;
                self.push(Value::I32(i32::from(u16::from_le_bytes(
                    b.try_into().unwrap_or([0; 2]),
                ))));
            }
            Op::I64Load8S { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 1)?;
                self.push(Value::I64(i64::from(b[0] as i8)));
            }
            Op::I64Load8U { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 1)?;
                self.push(Value::I64(i64::from(b[0])));
            }
            Op::I64Load16S { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 2)?;
                self.push(Value::I64(i64::from(i16::from_le_bytes(
                    b.try_into().unwrap_or([0; 2]),
                ))));
            }
            Op::I64Load16U { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 2)?;
                self.push(Value::I64(i64::from(u16::from_le_bytes(
                    b.try_into().unwrap_or([0; 2]),
                ))));
            }
            Op::I64Load32S { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 4)?;
                self.push(Value::I64(i64::from(i32::from_le_bytes(
                    b.try_into().unwrap_or([0; 4]),
                ))));
            }
            Op::I64Load32U { off } => {
                let a = self.pop_i32();
                let b = self.mem.load(a, *off, 4)?;
                self.push(Value::I64(i64::from(u32::from_le_bytes(
                    b.try_into().unwrap_or([0; 4]),
                ))));
            }
            Op::I32Store { off } => {
                let v = self.pop_i32();
                let a = self.pop_i32();
                self.mem.store(a, *off, &v.to_le_bytes())?;
            }
            Op::I64Store { off } => {
                let v = self.pop_i64();
                let a = self.pop_i32();
                self.mem.store(a, *off, &v.to_le_bytes())?;
            }
            Op::F32Store { off } => {
                let v = self.pop_f32();
                let a = self.pop_i32();
                self.mem.store(a, *off, &v.to_le_bytes())?;
            }
            Op::F64Store { off } => {
                let v = self.pop_f64();
                let a = self.pop_i32();
                self.mem.store(a, *off, &v.to_le_bytes())?;
            }
            Op::I32Store8 { off } => {
                let v = self.pop_i32();
                let a = self.pop_i32();
                self.mem.store(a, *off, &[(v & 0xFF) as u8])?;
            }
            Op::I32Store16 { off } => {
                let v = self.pop_i32();
                let a = self.pop_i32();
                self.mem.store(a, *off, &(v & 0xFFFF).to_le_bytes()[..2])?;
            }
            Op::I64Store8 { off } => {
                let v = self.pop_i64();
                let a = self.pop_i32();
                self.mem.store(a, *off, &[(v & 0xFF) as u8])?;
            }
            Op::I64Store16 { off } => {
                let v = self.pop_i64();
                let a = self.pop_i32();
                self.mem.store(a, *off, &((v & 0xFFFF) as u16).to_le_bytes())?;
            }
            Op::I64Store32 { off } => {
                let v = self.pop_i64();
                let a = self.pop_i32();
                self.mem.store(a, *off, &((v & 0xFFFF_FFFF) as u32).to_le_bytes())?;
            }
            Op::MemSize => {
                let p = self.mem.pages() as i32;
                self.push(Value::I32(p));
            }
            Op::MemGrow => {
                let d = self.pop_i32();
                let r = if d < 0 { -1 } else { self.mem.grow(d as u32) };
                self.push(Value::I32(r));
            }
            Op::ConstI32(v) => self.push(Value::I32(*v)),
            Op::ConstI64(v) => self.push(Value::I64(*v)),
            Op::ConstF32(v) => self.push(Value::F32(*v)),
            Op::ConstF64(v) => self.push(Value::F64(*v)),
            Op::I32Eqz => {
                let a = self.pop_i32();
                self.push(Value::I32(i32::from(a == 0)));
            }
            Op::I32Eq => cmp_i32!(|a, b| a == b),
            Op::I32Ne => cmp_i32!(|a, b| a != b),
            Op::I32LtS => cmp_i32!(|a, b| a < b),
            Op::I32LtU => cmp_i32!(|a, b| (a as u32) < (b as u32)),
            Op::I32GtS => cmp_i32!(|a, b| a > b),
            Op::I32GtU => cmp_i32!(|a, b| (a as u32) > (b as u32)),
            Op::I32LeS => cmp_i32!(|a, b| a <= b),
            Op::I32LeU => cmp_i32!(|a, b| (a as u32) <= (b as u32)),
            Op::I32GeS => cmp_i32!(|a, b| a >= b),
            Op::I32GeU => cmp_i32!(|a, b| (a as u32) >= (b as u32)),
            Op::I64Eqz => {
                let a = self.pop_i64();
                self.push(Value::I32(i32::from(a == 0)));
            }
            Op::I64Eq => cmp_i64!(|a, b| a == b),
            Op::I64Ne => cmp_i64!(|a, b| a != b),
            Op::I64LtS => cmp_i64!(|a, b| a < b),
            Op::I64LtU => cmp_i64!(|a, b| (a as u64) < (b as u64)),
            Op::I64GtS => cmp_i64!(|a, b| a > b),
            Op::I64GtU => cmp_i64!(|a, b| (a as u64) > (b as u64)),
            Op::I64LeS => cmp_i64!(|a, b| a <= b),
            Op::I64LeU => cmp_i64!(|a, b| (a as u64) <= (b as u64)),
            Op::I64GeS => cmp_i64!(|a, b| a >= b),
            Op::I64GeU => cmp_i64!(|a, b| (a as u64) >= (b as u64)),
            Op::F32Eq => cmp_f32!(|a, b| a == b),
            Op::F32Ne => cmp_f32!(|a, b| a != b),
            Op::F32Lt => cmp_f32!(|a, b| a < b),
            Op::F32Gt => cmp_f32!(|a, b| a > b),
            Op::F32Le => cmp_f32!(|a, b| a <= b),
            Op::F32Ge => cmp_f32!(|a, b| a >= b),
            Op::F64Eq => cmp_f64!(|a, b| a == b),
            Op::F64Ne => cmp_f64!(|a, b| a != b),
            Op::F64Lt => cmp_f64!(|a, b| a < b),
            Op::F64Gt => cmp_f64!(|a, b| a > b),
            Op::F64Le => cmp_f64!(|a, b| a <= b),
            Op::F64Ge => cmp_f64!(|a, b| a >= b),
            Op::I32Clz => {
                let a = self.pop_i32();
                self.push(Value::I32(a.leading_zeros() as i32));
            }
            Op::I32Ctz => {
                let a = self.pop_i32();
                self.push(Value::I32(a.trailing_zeros() as i32));
            }
            Op::I32Popcnt => {
                let a = self.pop_i32();
                self.push(Value::I32(a.count_ones() as i32));
            }
            Op::I32Add => bin_i32!(|a, b| a.wrapping_add(b)),
            Op::I32Sub => bin_i32!(|a, b| a.wrapping_sub(b)),
            Op::I32Mul => bin_i32!(|a, b| a.wrapping_mul(b)),
            Op::I32DivS => {
                let b = self.pop_i32();
                let a = self.pop_i32();
                if b == 0 {
                    return Err(Trap::DivisionByZero);
                }
                if a == i32::MIN && b == -1 {
                    return Err(Trap::IntegerOverflow);
                }
                self.push(Value::I32(a.wrapping_div(b)));
            }
            Op::I32DivU => {
                let b = self.pop_i32() as u32;
                let a = self.pop_i32() as u32;
                if b == 0 {
                    return Err(Trap::DivisionByZero);
                }
                self.push(Value::I32((a / b) as i32));
            }
            Op::I32RemS => {
                let b = self.pop_i32();
                let a = self.pop_i32();
                if b == 0 {
                    return Err(Trap::DivisionByZero);
                }
                self.push(Value::I32(a.wrapping_rem(b)));
            }
            Op::I32RemU => {
                let b = self.pop_i32() as u32;
                let a = self.pop_i32() as u32;
                if b == 0 {
                    return Err(Trap::DivisionByZero);
                }
                self.push(Value::I32((a % b) as i32));
            }
            Op::I32And => bin_i32!(|a, b| a & b),
            Op::I32Or => bin_i32!(|a, b| a | b),
            Op::I32Xor => bin_i32!(|a, b| a ^ b),
            Op::I32Shl => bin_i32!(|a, b| a.wrapping_shl(b as u32)),
            Op::I32ShrS => bin_i32!(|a, b| a.wrapping_shr(b as u32)),
            Op::I32ShrU => bin_i32!(|a, b| ((a as u32).wrapping_shr(b as u32)) as i32),
            Op::I32Rotl => bin_i32!(|a, b| a.rotate_left((b & 31) as u32)),
            Op::I32Rotr => bin_i32!(|a, b| a.rotate_right((b & 31) as u32)),
            Op::I64Clz => {
                let a = self.pop_i64();
                self.push(Value::I64(i64::from(a.leading_zeros())));
            }
            Op::I64Ctz => {
                let a = self.pop_i64();
                self.push(Value::I64(i64::from(a.trailing_zeros())));
            }
            Op::I64Popcnt => {
                let a = self.pop_i64();
                self.push(Value::I64(i64::from(a.count_ones())));
            }
            Op::I64Add => bin_i64!(|a, b| a.wrapping_add(b)),
            Op::I64Sub => bin_i64!(|a, b| a.wrapping_sub(b)),
            Op::I64Mul => bin_i64!(|a, b| a.wrapping_mul(b)),
            Op::I64DivS => {
                let b = self.pop_i64();
                let a = self.pop_i64();
                if b == 0 {
                    return Err(Trap::DivisionByZero);
                }
                if a == i64::MIN && b == -1 {
                    return Err(Trap::IntegerOverflow);
                }
                self.push(Value::I64(a.wrapping_div(b)));
            }
            Op::I64DivU => {
                let b = self.pop_i64() as u64;
                let a = self.pop_i64() as u64;
                if b == 0 {
                    return Err(Trap::DivisionByZero);
                }
                self.push(Value::I64((a / b) as i64));
            }
            Op::I64RemS => {
                let b = self.pop_i64();
                let a = self.pop_i64();
                if b == 0 {
                    return Err(Trap::DivisionByZero);
                }
                self.push(Value::I64(a.wrapping_rem(b)));
            }
            Op::I64RemU => {
                let b = self.pop_i64() as u64;
                let a = self.pop_i64() as u64;
                if b == 0 {
                    return Err(Trap::DivisionByZero);
                }
                self.push(Value::I64((a % b) as i64));
            }
            Op::I64And => bin_i64!(|a, b| a & b),
            Op::I64Or => bin_i64!(|a, b| a | b),
            Op::I64Xor => bin_i64!(|a, b| a ^ b),
            Op::I64Shl => bin_i64!(|a, b| a.wrapping_shl(b as u64 as u32)),
            Op::I64ShrS => bin_i64!(|a, b| a.wrapping_shr(b as u64 as u32)),
            Op::I64ShrU => bin_i64!(|a, b| ((a as u64).wrapping_shr(b as u64 as u32)) as i64),
            Op::I64Rotl => bin_i64!(|a, b| a.rotate_left((b & 63) as u32)),
            Op::I64Rotr => bin_i64!(|a, b| a.rotate_right((b & 63) as u32)),
            Op::F32Abs => {
                let a = self.pop_f32();
                self.push(Value::F32(a.abs()));
            }
            Op::F32Neg => {
                let a = self.pop_f32();
                self.push(Value::F32(-a));
            }
            Op::F32Ceil => {
                let a = self.pop_f32();
                self.push(Value::F32(a.ceil()));
            }
            Op::F32Floor => {
                let a = self.pop_f32();
                self.push(Value::F32(a.floor()));
            }
            Op::F32Trunc => {
                let a = self.pop_f32();
                self.push(Value::F32(a.trunc()));
            }
            Op::F32Nearest => {
                let a = self.pop_f32();
                self.push(Value::F32(a.round_ties_even()));
            }
            Op::F32Sqrt => {
                let a = self.pop_f32();
                self.push(Value::F32(a.sqrt()));
            }
            Op::F32Add => bin_f32!(|a, b| a + b),
            Op::F32Sub => bin_f32!(|a, b| a - b),
            Op::F32Mul => bin_f32!(|a, b| a * b),
            Op::F32Div => bin_f32!(|a, b| a / b),
            Op::F32Min => bin_f32!(wasm_min32),
            Op::F32Max => bin_f32!(wasm_max32),
            Op::F32Copysign => bin_f32!(|a, b| a.copysign(b)),
            Op::F64Abs => {
                let a = self.pop_f64();
                self.push(Value::F64(a.abs()));
            }
            Op::F64Neg => {
                let a = self.pop_f64();
                self.push(Value::F64(-a));
            }
            Op::F64Ceil => {
                let a = self.pop_f64();
                self.push(Value::F64(a.ceil()));
            }
            Op::F64Floor => {
                let a = self.pop_f64();
                self.push(Value::F64(a.floor()));
            }
            Op::F64Trunc => {
                let a = self.pop_f64();
                self.push(Value::F64(a.trunc()));
            }
            Op::F64Nearest => {
                let a = self.pop_f64();
                self.push(Value::F64(a.round_ties_even()));
            }
            Op::F64Sqrt => {
                let a = self.pop_f64();
                self.push(Value::F64(a.sqrt()));
            }
            Op::F64Add => bin_f64!(|a, b| a + b),
            Op::F64Sub => bin_f64!(|a, b| a - b),
            Op::F64Mul => bin_f64!(|a, b| a * b),
            Op::F64Div => bin_f64!(|a, b| a / b),
            Op::F64Min => bin_f64!(wasm_min64),
            Op::F64Max => bin_f64!(wasm_max64),
            Op::F64Copysign => bin_f64!(|a, b| a.copysign(b)),
            Op::I32WrapI64 => {
                let a = self.pop_i64();
                self.push(Value::I32(a as i32));
            }
            Op::I32TruncF32S => {
                let a = self.pop_f32();
                self.push(Value::I32(trunc_i64_range(
                    f64::from(a),
                    -2_147_483_648.0,
                    2_147_483_648.0,
                )? as i32));
            }
            Op::I32TruncF32U => {
                let a = self.pop_f32();
                self.push(Value::I32(trunc_u64_range(
                    f64::from(a),
                    4_294_967_296.0,
                )? as i32));
            }
            Op::I32TruncF64S => {
                let a = self.pop_f64();
                self.push(Value::I32(trunc_i64_range(
                    a,
                    -2_147_483_648.0,
                    2_147_483_648.0,
                )? as i32));
            }
            Op::I32TruncF64U => {
                let a = self.pop_f64();
                self.push(Value::I32(trunc_u64_range(a, 4_294_967_296.0)? as i32));
            }
            Op::I64ExtendI32S => {
                let a = self.pop_i32();
                self.push(Value::I64(i64::from(a)));
            }
            Op::I64ExtendI32U => {
                let a = self.pop_i32();
                self.push(Value::I64(i64::from(a as u32)));
            }
            Op::I64TruncF32S => {
                let a = self.pop_f32();
                self.push(Value::I64(trunc_i64_range(
                    f64::from(a),
                    -9_223_372_036_854_775_808.0,
                    9_223_372_036_854_775_808.0,
                )?));
            }
            Op::I64TruncF32U => {
                let a = self.pop_f32();
                self.push(Value::I64(trunc_u64_range(
                    f64::from(a),
                    18_446_744_073_709_551_616.0,
                )? as i64));
            }
            Op::I64TruncF64S => {
                let a = self.pop_f64();
                self.push(Value::I64(trunc_i64_range(
                    a,
                    -9_223_372_036_854_775_808.0,
                    9_223_372_036_854_775_808.0,
                )?));
            }
            Op::I64TruncF64U => {
                let a = self.pop_f64();
                self.push(Value::I64(trunc_u64_range(a, 18_446_744_073_709_551_616.0)? as i64));
            }
            Op::F32ConvertI32S => {
                let a = self.pop_i32();
                self.push(Value::F32(a as f32));
            }
            Op::F32ConvertI32U => {
                let a = self.pop_i32();
                self.push(Value::F32((a as u32) as f32));
            }
            Op::F32ConvertI64S => {
                let a = self.pop_i64();
                self.push(Value::F32(a as f32));
            }
            Op::F32ConvertI64U => {
                let a = self.pop_i64();
                self.push(Value::F32((a as u64) as f32));
            }
            Op::F32DemoteF64 => {
                let a = self.pop_f64();
                self.push(Value::F32(a as f32));
            }
            Op::F64ConvertI32S => {
                let a = self.pop_i32();
                self.push(Value::F64(f64::from(a)));
            }
            Op::F64ConvertI32U => {
                let a = self.pop_i32();
                self.push(Value::F64(f64::from(a as u32)));
            }
            Op::F64ConvertI64S => {
                let a = self.pop_i64();
                self.push(Value::F64(a as f64));
            }
            Op::F64ConvertI64U => {
                let a = self.pop_i64();
                self.push(Value::F64((a as u64) as f64));
            }
            Op::F64PromoteF32 => {
                let a = self.pop_f32();
                self.push(Value::F64(f64::from(a)));
            }
            Op::I32ReinterpretF32 => {
                let a = self.pop_f32();
                self.push(Value::I32(a.to_bits() as i32));
            }
            Op::I64ReinterpretF64 => {
                let a = self.pop_f64();
                self.push(Value::I64(a.to_bits() as i64));
            }
            Op::F32ReinterpretI32 => {
                let a = self.pop_i32();
                self.push(Value::F32(f32::from_bits(a as u32)));
            }
            Op::F64ReinterpretI64 => {
                let a = self.pop_i64();
                self.push(Value::F64(f64::from_bits(a as u64)));
            }
            Op::Br(_) | Op::BrIf(_) | Op::BrTable { .. } => {
                // Decode-only variants: validation rewrites all of them.
                return Err(Trap::Unreachable);
            }
        }
        Ok(())
    }
}

/// wasm `f*.min`: NaN propagates, `min(-0,+0) = -0`.
fn wasm_min32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == b {
        if a.is_sign_negative() || b.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        a.min(b)
    }
}

fn wasm_max32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == b {
        if a.is_sign_positive() || b.is_sign_positive() {
            0.0
        } else {
            -0.0
        }
    } else {
        a.max(b)
    }
}

fn wasm_min64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() || b.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        a.min(b)
    }
}

fn wasm_max64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_positive() || b.is_sign_positive() {
            0.0
        } else {
            -0.0
        }
    } else {
        a.max(b)
    }
}

/// `trunc(x)` must lie in `[lo, hi)` — `hi` is the first unrepresentable
/// positive value (`2^31`, `2^63`, `2^32`, `2^64`).
fn trunc_i64_range(x: f64, lo: f64, hi: f64) -> XResult<i64> {
    if x.is_nan() {
        return Err(Trap::BadConversion);
    }
    let t = x.trunc();
    if t < lo || t >= hi {
        return Err(Trap::BadConversion);
    }
    Ok(t as i64)
}

fn trunc_u64_range(x: f64, hi: f64) -> XResult<u64> {
    if x.is_nan() {
        return Err(Trap::BadConversion);
    }
    let t = x.trunc();
    if t < 0.0 || t >= hi {
        return Err(Trap::BadConversion);
    }
    // t <= u64::MAX here: `t as u64` saturates but the bound already
    // excludes values >= 2^64.
    Ok(t as u64)
}
