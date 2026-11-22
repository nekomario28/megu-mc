// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane CORE-LAZY-ZIP tests for `megu_core::zip_archive`: lazy index I/O
//! telemetry, 1-level Jar-in-Jar for NeoForge/Fabric layouts, ZIP64 handling,
//! and fail-closed traversal/encryption/bomb rejection — all with a pure-std
//! fixture writer so no external ZIP tooling or crates are needed.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use megu_core::zip_archive::{
    inflate_raw_rfc1951, validate_entry_name, LazyZipIndex, ZipError, MAX_OGG_BYTES,
};

static DIR_CTR: AtomicUsize = AtomicUsize::new(0);

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-lazyzip-{}-{tag}-{}",
        std::process::id(),
        DIR_CTR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Minimal pure-std ZIP writer + raw-deflate encoders for fixtures
// ---------------------------------------------------------------------------

fn w16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn w32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn w64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Ent {
    name: String,
    method: u16,
    /// Bytes written after the Local File Header (compressed form).
    stored: Vec<u8>,
    /// Uncompressed size declared in both headers.
    uncomp: u64,
    flags: u16,
    cd_extra: Vec<u8>,
    /// Saturate the CD size/offset fields (ZIP64 extra carries real values).
    zip64: bool,
}

fn ent(name: &str, method: u16, stored: Vec<u8>, uncomp: usize) -> Ent {
    Ent {
        name: name.to_owned(),
        method,
        stored,
        uncomp: uncomp as u64,
        flags: 0,
        cd_extra: Vec::new(),
        zip64: false,
    }
}

/// Write local headers + data + central directory + EOCD; returns the bytes.
fn build_zip(entries: &[Ent]) -> (Vec<u8>, Vec<u64>) {
    let mut out = Vec::new();
    let mut offs = Vec::new();
    for e in entries {
        offs.push(out.len() as u64);
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, e.flags);
        w16(&mut out, e.method);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, 0);
        w32(&mut out, e.stored.len() as u32);
        w32(&mut out, e.uncomp as u32);
        w16(&mut out, e.name.len() as u16);
        w16(&mut out, 0);
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&e.stored);
    }
    let cd_off = out.len() as u64;
    for (i, e) in entries.iter().enumerate() {
        w32(&mut out, 0x0201_4b50);
        w16(&mut out, 20);
        w16(&mut out, if e.zip64 { 45 } else { 20 });
        w16(&mut out, e.flags);
        w16(&mut out, e.method);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, 0);
        if e.zip64 {
            w32(&mut out, u32::MAX);
            w32(&mut out, u32::MAX);
        } else {
            w32(&mut out, e.stored.len() as u32);
            w32(&mut out, e.uncomp as u32);
        }
        w16(&mut out, e.name.len() as u16);
        w16(&mut out, e.cd_extra.len() as u16);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, 0);
        if e.zip64 {
            w32(&mut out, u32::MAX);
        } else {
            w32(&mut out, offs[i] as u32);
        }
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&e.cd_extra);
    }
    let cd_size = out.len() as u64 - cd_off;
    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, entries.len() as u16);
    w16(&mut out, entries.len() as u16);
    w32(&mut out, cd_size as u32);
    w32(&mut out, cd_off as u32);
    w16(&mut out, 0);
    (out, offs)
}

/// Same body as [`build_zip`] but with a full ZIP64 EOCD locator + EOCD and
/// saturated EOCD32 fields, exercising the ZIP64 resolution path.
fn build_zip64_archive(entries: &[Ent]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offs = Vec::new();
    for e in entries {
        offs.push(out.len() as u64);
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, e.flags);
        w16(&mut out, e.method);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, 0);
        w32(&mut out, e.stored.len() as u32);
        w32(&mut out, e.uncomp as u32);
        w16(&mut out, e.name.len() as u16);
        w16(&mut out, 0);
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&e.stored);
    }
    let cd_off = out.len() as u64;
    for (i, e) in entries.iter().enumerate() {
        w32(&mut out, 0x0201_4b50);
        w16(&mut out, 20);
        w16(&mut out, 45);
        w16(&mut out, e.flags);
        w16(&mut out, e.method);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, 0);
        w32(&mut out, e.stored.len() as u32);
        w32(&mut out, e.uncomp as u32);
        w16(&mut out, e.name.len() as u16);
        w16(&mut out, e.cd_extra.len() as u16);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, 0);
        w32(&mut out, offs[i] as u32);
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&e.cd_extra);
    }
    let cd_size = out.len() as u64 - cd_off;
    let count = entries.len() as u64;

    let z64_off = out.len() as u64;
    w32(&mut out, 0x0606_4b50);
    w64(&mut out, 44);
    w16(&mut out, 45);
    w16(&mut out, 45);
    w32(&mut out, 0);
    w32(&mut out, 0);
    w64(&mut out, count);
    w64(&mut out, count);
    w64(&mut out, cd_size);
    w64(&mut out, cd_off);

    w32(&mut out, 0x0706_4b50);
    w32(&mut out, 0);
    w64(&mut out, z64_off);
    w32(&mut out, 1);

    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, u16::MAX);
    w16(&mut out, u16::MAX);
    w32(&mut out, u32::MAX);
    w32(&mut out, u32::MAX);
    w16(&mut out, 0);
    out
}

/// ZIP64 extended-information extra field (0x0001): u64 values in the fixed
/// order uncomp size, comp size, local header offset for saturated fields.
fn zip64_extra(uncomp: u64, comp: u64, local_off: u64) -> Vec<u8> {
    let mut x = Vec::new();
    w16(&mut x, 0x0001);
    w16(&mut x, 24);
    w64(&mut x, uncomp);
    w64(&mut x, comp);
    w64(&mut x, local_off);
    x
}

/// LSB-first deflate bit writer.
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            n: 0,
        }
    }
    fn put(&mut self, v: u32, k: u32) {
        self.acc |= v << self.n;
        self.n += k;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    /// Huffman codes are emitted most-significant-code-bit first.
    fn put_code(&mut self, code: u32, k: u32) {
        for i in (0..k).rev() {
            self.put((code >> i) & 1, 1);
        }
    }
    fn done(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// Fixed-Huffman literal code per RFC 1951 §3.2.6.
fn fixed_lit(b: u8) -> (u32, u32) {
    if b < 144 {
        (0x30 + b as u32, 8)
    } else {
        (0x190 + b as u32 - 144, 9)
    }
}

/// Single-block fixed-Huffman deflate stream with literals only.
fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    let mut w = Bits::new();
    w.put(1, 1);
    w.put(0b01, 2);
    for &b in data {
        let (code, k) = fixed_lit(b);
        w.put_code(code, k);
    }
    w.put_code(0, 7); // 256: end of block
    w.done()
}

/// Fixed-Huffman stream encoding `AB` + back-reference(len 6, dist 2) →
/// `ABABABAB`, covering the length/distance decode path.
fn deflate_fixed_backref() -> Vec<u8> {
    let mut w = Bits::new();
    w.put(1, 1);
    w.put(0b01, 2);
    for &b in b"AB" {
        let (code, k) = fixed_lit(b);
        w.put_code(code, k);
    }
    w.put_code(4, 7); // length code 260 → len 6
    w.put_code(1, 5); // distance code 1 → dist 2
    w.put_code(0, 7);
    w.done()
}

/// Single stored (uncompressed) deflate block.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x01]; // BFINAL=1, BTYPE=00
    let len = data.len() as u16;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// zlib 9-compressed raw deflate stream (dynamic Huffman, BTYPE=2) and its
/// expected plaintext — generated offline, pinned here as bytes.
const DYN_DEFLATE: [u8; 158] = [
    0x75, 0x52, 0x5b, 0x0e, 0x84, 0x20, 0x0c, 0xbc, 0x0a, 0x57, 0x23, 0x91, 0x10, 0x36, 0x28, 0x46,
    0x76, 0x3f, 0xd6, 0xd3, 0x9b, 0x69, 0x29, 0x6d, 0xd7, 0xf5, 0xa3, 0xc5, 0xbe, 0x66, 0xa6, 0x8d,
    0x35, 0x9e, 0xdf, 0xf0, 0x8a, 0x47, 0xa8, 0xf8, 0x88, 0xbd, 0xa7, 0x77, 0xff, 0x7d, 0x3e, 0x4b,
    0x69, 0x61, 0x6d, 0x8b, 0xeb, 0xe9, 0x7b, 0xd9, 0xd2, 0xa8, 0xb1, 0x1f, 0x19, 0x2e, 0x03, 0x72,
    0x8e, 0xb4, 0x9c, 0x47, 0xf5, 0xee, 0x15, 0xdd, 0xa2, 0x50, 0xec, 0x54, 0x20, 0x03, 0x1c, 0xbc,
    0xa6, 0x0a, 0x1e, 0x0b, 0x44, 0x7c, 0x67, 0xd9, 0x75, 0x27, 0x0c, 0x29, 0x09, 0xd2, 0x30, 0xb7,
    0x9b, 0x53, 0xae, 0x52, 0xd8, 0x03, 0x0c, 0x18, 0x30, 0x7b, 0x80, 0xc9, 0xc4, 0x6d, 0x44, 0xe3,
    0x4e, 0x63, 0x03, 0xa2, 0x9c, 0x70, 0x30, 0xd0, 0xe8, 0xae, 0x4a, 0x2a, 0x54, 0x0f, 0xfb, 0xc9,
    0xe8, 0x1f, 0xe1, 0x72, 0x9d, 0x21, 0x5e, 0x93, 0x0a, 0xcc, 0xfd, 0x37, 0xa9, 0x98, 0x12, 0x5d,
    0x55, 0x7e, 0x08, 0x2e, 0x51, 0x48, 0xee, 0x61, 0x2b, 0x39, 0xaa, 0x9c, 0xe3, 0x02,
];
const DYN_RAW: [u8; 593] = [
    0x6c, 0x61, 0x7a, 0x79, 0x20, 0x6a, 0x61, 0x72, 0x20, 0x6c, 0x61, 0x7a, 0x79, 0x20, 0x61, 0x73,
    0x73, 0x65, 0x74, 0x73, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x61, 0x73, 0x73, 0x65,
    0x74, 0x73, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x6c, 0x61, 0x7a,
    0x79, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x61,
    0x75, 0x64, 0x69, 0x6f, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65,
    0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x6a, 0x61, 0x72, 0x20, 0x6d, 0x6f, 0x64, 0x20,
    0x6c, 0x61, 0x7a, 0x79, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x73,
    0x70, 0x69, 0x6e, 0x65, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65,
    0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f,
    0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74,
    0x73, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x6f, 0x67, 0x67,
    0x20, 0x6d, 0x6f, 0x64, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20,
    0x6a, 0x61, 0x72, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20,
    0x6c, 0x61, 0x7a, 0x79, 0x20, 0x7a, 0x69, 0x70, 0x20, 0x6a, 0x61, 0x72, 0x20, 0x6c, 0x61, 0x7a,
    0x79, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x6d, 0x6f, 0x64, 0x20,
    0x6a, 0x61, 0x72, 0x20, 0x6a, 0x61, 0x72, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x61,
    0x75, 0x64, 0x69, 0x6f, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74,
    0x73, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x61, 0x75, 0x64, 0x69,
    0x6f, 0x20, 0x7a, 0x69, 0x70, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x6c, 0x61,
    0x7a, 0x79, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x6c, 0x61, 0x7a, 0x79, 0x20, 0x7a,
    0x69, 0x70, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x61, 0x73, 0x73,
    0x65, 0x74, 0x73, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73,
    0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x6a, 0x61, 0x72, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f,
    0x20, 0x7a, 0x69, 0x70, 0x20, 0x7a, 0x69, 0x70, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x73, 0x70, 0x69,
    0x6e, 0x65, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f,
    0x20, 0x6f, 0x67, 0x67, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20,
    0x6a, 0x61, 0x72, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20,
    0x7a, 0x69, 0x70, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x73, 0x70,
    0x69, 0x6e, 0x65, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x6d,
    0x6f, 0x64, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20,
    0x6f, 0x67, 0x67, 0x20, 0x61, 0x75, 0x64, 0x69, 0x6f, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x73, 0x70,
    0x69, 0x6e, 0x65, 0x20, 0x6f, 0x67, 0x67, 0x20, 0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x73,
    0x70, 0x69, 0x6e, 0x65, 0x20, 0x6d, 0x6f, 0x64, 0x20, 0x7a, 0x69, 0x70, 0x20, 0x7a, 0x69, 0x70,
    0x20, 0x6c, 0x61, 0x7a, 0x79, 0x20, 0x6a, 0x61, 0x72, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20,
    0x6c, 0x61, 0x7a, 0x79, 0x20, 0x6c, 0x61, 0x7a, 0x79, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20,
    0x61, 0x73, 0x73, 0x65, 0x74, 0x73, 0x20, 0x73, 0x70, 0x69, 0x6e, 0x65, 0x20, 0x6a, 0x61, 0x72,
    0x20, 0x6d, 0x6f, 0x64, 0x20, 0x6a, 0x61, 0x72, 0x20, 0x6c, 0x61, 0x7a, 0x79, 0x20, 0x7a, 0x69,
    0x70,
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn lazy_zip_reads_only_eocd_and_cd_on_stored_and_deflate_entries() {
    let dir = fresh_dir("eocd");
    let jar = dir.join("big_mod.jar");

    let big = vec![0xABu8; 2 * 1024 * 1024];
    let stored_bytes = b"stored ogg payload bytes".to_vec();
    let fixed_bytes = b"fixed huffman literal payload 0123456789".to_vec();

    let entries = vec![
        ent("dummy_payload.bin", 0, big, 2 * 1024 * 1024),
        ent(
            "assets/x/a_stored.ogg",
            0,
            stored_bytes.clone(),
            stored_bytes.len(),
        ),
        ent(
            "assets/x/b_fixed.ogg",
            8,
            deflate_fixed(&fixed_bytes),
            fixed_bytes.len(),
        ),
        ent("assets/x/c_dyn.ogg", 8, DYN_DEFLATE.to_vec(), DYN_RAW.len()),
        ent("assets/x/d_backref.ogg", 8, deflate_fixed_backref(), 8),
        ent(
            "assets/x/e_blkstored.ogg",
            8,
            deflate_stored(&stored_bytes),
            stored_bytes.len(),
        ),
    ];
    let (bytes, _) = build_zip(&entries);
    fs::write(&jar, bytes).unwrap();

    let idx = LazyZipIndex::open_filtered(&jar, |n| n.ends_with(".ogg")).unwrap();

    // Index I/O must be tiny vs the 2 MiB archive: one EOCD tail chunk plus
    // the Central Directory only.
    assert!(
        idx.bytes_read_for_index() < 8_192,
        "index read {} bytes, expected < 8192",
        idx.bytes_read_for_index()
    );
    assert_eq!(idx.entries().len(), 5);

    let meta = idx
        .entries()
        .iter()
        .find(|e| e.name == "assets/x/a_stored.ogg")
        .unwrap();
    assert_eq!(meta.archive_id, 0);
    assert_eq!(meta.compression_method, 0);
    assert_eq!(meta.nested_range, None);
    assert_eq!(meta.uncompressed_size as usize, stored_bytes.len());

    assert_eq!(
        idx.read_entry("assets/x/a_stored.ogg", MAX_OGG_BYTES)
            .unwrap(),
        stored_bytes
    );
    assert_eq!(
        idx.read_entry("assets/x/b_fixed.ogg", MAX_OGG_BYTES)
            .unwrap(),
        fixed_bytes
    );
    assert_eq!(
        idx.read_entry("assets/x/c_dyn.ogg", MAX_OGG_BYTES).unwrap(),
        DYN_RAW.to_vec()
    );
    assert_eq!(
        idx.read_entry("assets/x/d_backref.ogg", MAX_OGG_BYTES)
            .unwrap(),
        b"ABABABAB".to_vec()
    );
    assert_eq!(
        idx.read_entry("assets/x/e_blkstored.ogg", MAX_OGG_BYTES)
            .unwrap(),
        stored_bytes
    );

    // Direct decompressor check on the dynamic fixture.
    assert_eq!(
        inflate_raw_rfc1951(&DYN_DEFLATE, MAX_OGG_BYTES).unwrap(),
        DYN_RAW.to_vec()
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lazy_zip_indexes_jar_in_jar_for_neoforge_and_fabric_with_zero_temp_files() {
    let dir = fresh_dir("jarjar");
    let jar = dir.join("parent_mod.jar");

    let inner_for = |modid: &str| -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let sounds = format!("{{\"{modid}\":{{\"sound\":true}}}}").into_bytes();
        let ogg = format!("fake-ogg-payload-{modid}").into_bytes();
        let inner = build_zip(&[
            ent(
                &format!("assets/{modid}/sounds.json"),
                0,
                sounds.clone(),
                sounds.len(),
            ),
            ent(
                &format!("assets/{modid}/a.ogg"),
                8,
                deflate_fixed(&ogg),
                ogg.len(),
            ),
        ])
        .0;
        (inner, sounds, ogg)
    };

    let (nf_jar, nf_sounds, nf_ogg) = inner_for("modnf");
    let (fab_jar, fab_sounds, fab_ogg) = inner_for("modfab");

    let parent = build_zip(&[
        // NeoForge layout, Stored → zero-copy indexing into parent bytes.
        ent(
            "META-INF/jarjar/inner_nf.jar",
            0,
            nf_jar.clone(),
            nf_jar.len(),
        ),
        // Fabric layout, Deflate → in-memory index, still no disk writes.
        ent(
            "META-INF/jars/inner_fab.jar",
            8,
            deflate_fixed(&fab_jar),
            fab_jar.len(),
        ),
    ])
    .0;
    fs::write(&jar, parent).unwrap();

    let files_before = fs::read_dir(&dir).unwrap().count();
    let idx = LazyZipIndex::open_filtered(&jar, |n| {
        n.ends_with("sounds.json") || n.ends_with(".ogg")
    })
    .unwrap();
    let files_after = fs::read_dir(&dir).unwrap().count();
    assert_eq!(files_before, 1);
    assert_eq!(files_after, 1, "index must not create temp files");

    // Both nested children resolve and read identically to inner-jar bytes.
    assert_eq!(
        idx.read_entry("assets/modnf/sounds.json", MAX_OGG_BYTES)
            .unwrap(),
        nf_sounds
    );
    assert_eq!(
        idx.read_entry("assets/modnf/a.ogg", MAX_OGG_BYTES).unwrap(),
        nf_ogg
    );
    assert_eq!(
        idx.read_entry("assets/modfab/sounds.json", MAX_OGG_BYTES)
            .unwrap(),
        fab_sounds
    );
    assert_eq!(
        idx.read_entry("assets/modfab/a.ogg", MAX_OGG_BYTES)
            .unwrap(),
        fab_ogg
    );

    // Provenance: children carry non-zero archive ids and the containing
    // jar's nested_range; stored children index straight into parent bytes.
    let nf_meta = idx
        .entries()
        .iter()
        .find(|e| e.name == "assets/modnf/a.ogg")
        .unwrap();
    let fab_meta = idx
        .entries()
        .iter()
        .find(|e| e.name == "assets/modfab/a.ogg")
        .unwrap();
    assert!(nf_meta.archive_id > 0);
    assert!(fab_meta.archive_id > 0);
    assert_ne!(nf_meta.archive_id, fab_meta.archive_id);
    let (off, len) = nf_meta.nested_range.unwrap();
    assert_eq!(len as usize, nf_jar.len());
    // The stored nested jar's verbatim bytes live at the recorded range.
    assert!(off > 0);

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lazy_zip_supports_zip64_extra_fields_and_fails_closed_on_traversal_and_bombs() {
    // ---- name validation, direct ----
    for bad in [
        "",
        "/abs",
        "a//b",
        "a/./b",
        "a/../b",
        "../evil",
        "..",
        ".",
        "a\\b",
        "C:evil",
        "a\0b",
        "a/b/",
    ] {
        assert!(validate_entry_name(bad).is_err(), "expected reject: {bad:?}");
    }
    assert!(validate_entry_name("assets/mod/sounds.json").is_ok());

    let dir = fresh_dir("zip64");

    // ---- ZIP64 extra field: saturated CD sizes + offset resolved via 0x0001 ----
    let z64_data = b"zip64 payload bytes".to_vec();
    let z64_comp = deflate_fixed(&z64_data);
    let mut e = ent("assets/z/z64.ogg", 8, z64_comp.clone(), z64_data.len());
    e.zip64 = true;
    e.cd_extra = zip64_extra(z64_data.len() as u64, z64_comp.len() as u64, 0);
    let zip64_jar = dir.join("z64extra.jar");
    fs::write(&zip64_jar, build_zip(&[e]).0).unwrap();
    let idx = LazyZipIndex::open_all(&zip64_jar).unwrap();
    let meta = &idx.entries()[0];
    assert_eq!(meta.uncompressed_size as usize, z64_data.len());
    assert_eq!(meta.compressed_size as usize, z64_comp.len());
    assert_eq!(meta.local_header_offset, 0);
    assert_eq!(
        idx.read_entry("assets/z/z64.ogg", MAX_OGG_BYTES).unwrap(),
        z64_data
    );

    // ---- Full ZIP64 EOCD locator + EOCD resolution ----
    let z64e_jar = dir.join("z64eocd.jar");
    fs::write(
        &z64e_jar,
        build_zip64_archive(&[
            ent(
                "assets/z/a.ogg",
                0,
                z64_data.clone(),
                z64_data.len(),
            ),
            ent("assets/z/b.ogg", 8, z64_comp.clone(), z64_data.len()),
        ]),
    )
    .unwrap();
    let idx = LazyZipIndex::open_all(&z64e_jar).unwrap();
    assert_eq!(idx.entries().len(), 2);
    assert_eq!(
        idx.read_entry("assets/z/b.ogg", MAX_OGG_BYTES).unwrap(),
        z64_data
    );

    // ---- Path traversal inside archives: every bad name fails the open ----
    for (i, bad) in ["../evil", "a/./b", "a\\b", "C:evil", "a\0b"]
        .iter()
        .enumerate()
    {
        let p = dir.join(format!("traversal_{i}.jar"));
        fs::write(&p, build_zip(&[ent(bad, 0, vec![1], 1)]).0).unwrap();
        assert!(
            matches!(
                LazyZipIndex::open_all(&p),
                Err(ZipError::InvalidEntryName(_))
            ),
            "expected traversal reject for {bad:?}"
        );
    }

    // ---- Encrypted flag (bit 0) rejection ----
    let enc_jar = dir.join("encrypted.jar");
    let mut enc = ent("assets/x/e.ogg", 0, vec![1, 2, 3], 3);
    enc.flags = 1;
    fs::write(&enc_jar, build_zip(&[enc]).0).unwrap();
    assert!(matches!(
        LazyZipIndex::open_all(&enc_jar),
        Err(ZipError::Encrypted(_))
    ));

    // ---- ZIP-bomb caps: declared size, inflate cap, and size mismatch ----
    let bomb_jar = dir.join("bomb.jar");
    let bomb_data = vec![0xCDu8; 200 * 1024];
    let bomb_comp = deflate_fixed(&bomb_data);
    // Declared 100 KiB, real stream expands to 200 KiB: cap must trip first.
    let bomb = ent("assets/x/bomb.ogg", 8, bomb_comp, 100 * 1024);
    fs::write(&bomb_jar, build_zip(&[bomb]).0).unwrap();
    let idx = LazyZipIndex::open_all(&bomb_jar).unwrap();
    assert!(matches!(
        idx.read_entry("assets/x/bomb.ogg", 150 * 1024),
        Err(ZipError::OutputCapExceeded)
    ));
    assert!(matches!(
        idx.read_entry("assets/x/bomb.ogg", 64 * 1024),
        Err(ZipError::EntryTooLarge { .. })
    ));

    let small_jar = dir.join("small.jar");
    let small_data = vec![0xEEu8; 4096];
    let small = ent("assets/x/s.ogg", 0, small_data.clone(), small_data.len());
    fs::write(&small_jar, build_zip(&[small]).0).unwrap();
    let idx = LazyZipIndex::open_all(&small_jar).unwrap();
    assert!(matches!(
        idx.read_entry("assets/x/s.ogg", 1024),
        Err(ZipError::EntryTooLarge { .. })
    ));
    assert_eq!(
        idx.read_entry("assets/x/s.ogg", MAX_OGG_BYTES).unwrap(),
        small_data
    );

    // ---- Malformed archives fail closed, no panics ----
    let trunc_jar = dir.join("truncated.jar");
    let mut bytes = build_zip(&[ent("a.txt", 0, vec![7], 1)]).0;
    bytes.truncate(bytes.len() - 10); // chop EOCD tail
    fs::write(&trunc_jar, bytes).unwrap();
    assert!(matches!(
        LazyZipIndex::open_all(&trunc_jar),
        Err(ZipError::NotZipArchive) | Err(ZipError::Malformed(_))
    ));

    let corrupt_jar = dir.join("corrupt.jar");
    let corrupt = ent("assets/x/c.ogg", 8, vec![0xFF; 32], 4);
    fs::write(&corrupt_jar, build_zip(&[corrupt]).0).unwrap();
    let idx = LazyZipIndex::open_all(&corrupt_jar).unwrap();
    assert!(idx.read_entry("assets/x/c.ogg", MAX_OGG_BYTES).is_err());

    fs::remove_dir_all(&dir).unwrap();
}
