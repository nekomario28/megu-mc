// SPDX-License-Identifier: LGPL-3.0-or-later

//! Byte-level WebAssembly binary encoder + stored-ZIP builder used by the
//! test-suite (and embedders' tests) to assemble real `.wasm`/`.jar`
//! payloads with zero external toolchain.
//!
//! Every helper emits spec-shaped bytes; nothing is skipped or patched in
//! ways a real toolchain couldn't produce.

use megu_core::hash::crc32_ieee;

// Value-type bytes.
pub const I32: u8 = 0x7F;
pub const I64: u8 = 0x7E;
pub const F32: u8 = 0x7D;
pub const F64: u8 = 0x7C;

// Export kinds.
pub const KIND_FUNC: u8 = 0x00;
pub const KIND_TABLE: u8 = 0x01;
pub const KIND_MEM: u8 = 0x02;
pub const KIND_GLOBAL: u8 = 0x03;

/// Unsigned LEB128.
pub fn uv(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            return out;
        }
    }
}

pub fn uv32(v: u32) -> Vec<u8> {
    uv(u64::from(v))
}

/// Signed LEB128.
pub fn sv(mut v: i64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        out.push(if done { b } else { b | 0x80 });
        if done {
            return out;
        }
    }
}

pub fn sv32(v: i32) -> Vec<u8> {
    sv(i64::from(v))
}

/// Length-prefixed name.
pub fn name(s: &str) -> Vec<u8> {
    let mut out = uv32(s.len() as u32);
    out.extend_from_slice(s.as_bytes());
    out
}

/// `vec(x)` — count-prefixed concatenation.
pub fn vec_of(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = uv32(items.len() as u32);
    for i in items {
        out.extend_from_slice(i);
    }
    out
}

/// One module section.
pub fn section(id: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut out = vec![id];
    out.extend_from_slice(&uv32(payload.len() as u32));
    out.extend_from_slice(&payload);
    out
}

/// Assemble magic + version + sections into a module.
pub fn module(sections: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    for s in sections {
        out.extend_from_slice(s);
    }
    out
}

/// Type section (id 1): `sigs` of `(param type bytes, result type bytes)`.
pub fn types_sec(sigs: &[(&[u8], &[u8])]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = sigs
        .iter()
        .map(|(p, r)| {
            let mut e = vec![0x60];
            e.extend_from_slice(&vec_of(&p.iter().map(|t| vec![*t]).collect::<Vec<_>>()));
            e.extend_from_slice(&vec_of(&r.iter().map(|t| vec![*t]).collect::<Vec<_>>()));
            e
        })
        .collect();
    section(1, vec_of(&entries))
}

/// Import section (id 2), function imports only.
pub fn import_funcs_sec(imports: &[(&str, &str, u32)]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = imports
        .iter()
        .map(|(m, n, ty)| {
            let mut e = name(m);
            e.extend_from_slice(&name(n));
            e.push(0x00);
            e.extend_from_slice(&uv32(*ty));
            e
        })
        .collect();
    section(2, vec_of(&entries))
}

/// Function section (id 3).
pub fn func_sec(type_indices: &[u32]) -> Vec<u8> {
    section(3, vec_of(&type_indices.iter().map(|t| uv32(*t)).collect::<Vec<_>>()))
}

/// Memory section (id 5) with one memory.
pub fn mem_sec(min: u32, max: Option<u32>) -> Vec<u8> {
    let mut e = uv32(1);
    match max {
        Some(mx) => {
            e.push(0x01);
            e.extend_from_slice(&uv32(min));
            e.extend_from_slice(&uv32(mx));
        }
        None => {
            e.push(0x00);
            e.extend_from_slice(&uv32(min));
        }
    }
    section(5, e)
}

/// Table section (id 4) with one funcref table.
pub fn table_sec(min: u32, max: Option<u32>) -> Vec<u8> {
    let mut e = uv32(1);
    e.push(0x70);
    match max {
        Some(mx) => {
            e.push(0x01);
            e.extend_from_slice(&uv32(min));
            e.extend_from_slice(&uv32(mx));
        }
        None => {
            e.push(0x00);
            e.extend_from_slice(&uv32(min));
        }
    }
    section(4, e)
}

/// Global section (id 6): `(valtype, mutable, init_expr_body)`.
/// `init_expr_body` is the opcode bytes *without* the trailing `end`.
pub fn global_sec(globals: &[(u8, bool, Vec<u8>)]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = globals
        .iter()
        .map(|(ty, mutable, init)| {
            let mut e = vec![*ty, u8::from(*mutable)];
            e.extend_from_slice(init);
            e.push(0x0B);
            e
        })
        .collect();
    section(6, vec_of(&entries))
}

/// Export section (id 7).
pub fn export_sec(exports: &[(&str, u8, u32)]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = exports
        .iter()
        .map(|(n, k, i)| {
            let mut e = name(n);
            e.push(*k);
            e.extend_from_slice(&uv32(*i));
            e
        })
        .collect();
    section(7, vec_of(&entries))
}

/// Start section (id 8).
pub fn start_sec(func: u32) -> Vec<u8> {
    section(8, uv32(func))
}

/// Element section (id 9), active MVP segments.
pub fn elem_sec(elems: &[(u32, i32, Vec<u32>)]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = elems
        .iter()
        .map(|(table, offset, funcs)| {
            let mut e = uv32(*table);
            e.push(0x41);
            e.extend_from_slice(&sv32(*offset));
            e.push(0x0B);
            e.extend_from_slice(&vec_of(&funcs.iter().map(|f| uv32(*f)).collect::<Vec<_>>()));
            e
        })
        .collect();
    section(9, vec_of(&entries))
}

/// Code section (id 10) from function bodies built by [`body`].
pub fn code_sec(bodies: &[Vec<u8>]) -> Vec<u8> {
    section(10, vec_of(bodies))
}

/// One function body: `(count, valtype)` locals + instruction bytes
/// (the function-level `end` `0x0B` must be included in `code`).
pub fn body(locals: &[(u32, u8)], code: &[u8]) -> Vec<u8> {
    let mut inner = vec_of(
        &locals
            .iter()
            .map(|(n, t)| {
                let mut e = uv32(*n);
                e.push(*t);
                e
            })
            .collect::<Vec<_>>(),
    );
    inner.extend_from_slice(code);
    let mut out = uv32(inner.len() as u32);
    out.extend_from_slice(&inner);
    out
}

/// Data section (id 11), active MVP segments.
pub fn data_sec(segs: &[(u32, i32, &[u8])]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = segs
        .iter()
        .map(|(mem, offset, bytes)| {
            let mut e = uv32(*mem);
            e.push(0x41);
            e.extend_from_slice(&sv32(*offset));
            e.push(0x0B);
            e.extend_from_slice(&uv32(bytes.len() as u32));
            e.extend_from_slice(bytes);
            e
        })
        .collect();
    section(11, vec_of(&entries))
}

// -- Instruction snippet helpers -----------------------------------------

pub fn i32_const(v: i32) -> Vec<u8> {
    let mut o = vec![0x41];
    o.extend_from_slice(&sv32(v));
    o
}
pub fn i64_const(v: i64) -> Vec<u8> {
    let mut o = vec![0x42];
    o.extend_from_slice(&sv(v));
    o
}
pub fn call(f: u32) -> Vec<u8> {
    let mut o = vec![0x10];
    o.extend_from_slice(&uv32(f));
    o
}
pub fn global_get(i: u32) -> Vec<u8> {
    let mut o = vec![0x23];
    o.extend_from_slice(&uv32(i));
    o
}
pub fn global_set(i: u32) -> Vec<u8> {
    let mut o = vec![0x24];
    o.extend_from_slice(&uv32(i));
    o
}
pub fn local_get(i: u32) -> Vec<u8> {
    let mut o = vec![0x20];
    o.extend_from_slice(&uv32(i));
    o
}
pub fn local_set(i: u32) -> Vec<u8> {
    let mut o = vec![0x21];
    o.extend_from_slice(&uv32(i));
    o
}
pub fn local_tee(i: u32) -> Vec<u8> {
    let mut o = vec![0x22];
    o.extend_from_slice(&uv32(i));
    o
}
/// `i32.store` with default alignment and offset 0.
pub fn i32_store() -> Vec<u8> {
    vec![0x36, 0x02, 0x00]
}
/// `i32.load` with default alignment and offset 0.
pub fn i32_load() -> Vec<u8> {
    vec![0x28, 0x02, 0x00]
}
/// `i32.load` with explicit byte offset.
pub fn i32_load_off(off: u32) -> Vec<u8> {
    let mut o = vec![0x28, 0x02];
    o.extend_from_slice(&uv32(off));
    o
}
/// `i32.store` with explicit byte offset.
pub fn i32_store_off(off: u32) -> Vec<u8> {
    let mut o = vec![0x36, 0x02];
    o.extend_from_slice(&uv32(off));
    o
}

pub fn concat(parts: &[&[u8]]) -> Vec<u8> {
    let mut o = Vec::new();
    for p in parts {
        o.extend_from_slice(p);
    }
    o
}

// -- Stored-ZIP builder ---------------------------------------------------

fn w16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn w32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A minimal, well-formed stored (uncompressed) ZIP — the same layout a
/// `.mmod` pack or a Modrinth `.jar` uses for stored members.
pub fn stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let crc = crc32_ieee(data);
        let offset = out.len() as u32;
        // Local file header.
        w32(&mut out, 0x0403_4B50);
        w16(&mut out, 20); // version needed
        w16(&mut out, 0); // flags
        w16(&mut out, 0); // method: stored
        w16(&mut out, 0); // mod time
        w16(&mut out, 0); // mod date
        w32(&mut out, crc);
        w32(&mut out, data.len() as u32);
        w32(&mut out, data.len() as u32);
        w16(&mut out, name.len() as u16);
        w16(&mut out, 0); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        // Central directory record.
        w32(&mut central, 0x0201_4B50);
        w16(&mut central, 20); // version made by
        w16(&mut central, 20); // version needed
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, crc);
        w32(&mut central, data.len() as u32);
        w32(&mut central, data.len() as u32);
        w16(&mut central, name.len() as u16);
        w16(&mut central, 0); // extra
        w16(&mut central, 0); // comment
        w16(&mut central, 0); // disk
        w16(&mut central, 0); // internal attrs
        w32(&mut central, 0); // external attrs
        w32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&central);
    // End of central directory.
    w32(&mut out, 0x0605_4B50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, files.len() as u16);
    w16(&mut out, files.len() as u16);
    w32(&mut out, central.len() as u32);
    w32(&mut out, cd_off);
    w16(&mut out, 0);
    out
}
