// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane P6e: deterministic adversarial fuzz harness for every untrusted
//! mod/JAR/bytecode and delegate-wire parser in `mod-adapter-core`.
//!
//! Everything a hostile mod archive, modpack, or delegated JVM peer can
//! carry — ZIP/raw-deflate modpack manifests, JVMS `.class` constant pools
//! and runtime-annotation attributes, culinary recipe/tag/loot JSON inside
//! arbitrary JARs, `MSY1` sync manifests, `CGD1`/`EAD1` delegate frames,
//! `0x58` entity-metadata entry streams, NeoForge `0x07` registry-data
//! bodies, and Maven/SemVer version-range strings — is hammered with
//! corpus-guided mutations, raw noise, ZIP/deflate bombs, truncation
//! sweeps, bit flips and forged length prefixes. The contract under test:
//! **zero panics and bounded memory** on every byte string, valid or
//! hostile.
//!
//! Determinism: all inputs derive from fixed-seed `splitmix64` PRNGs — no
//! `SystemTime`, no OS entropy, no external crates. `MC_SPINE_FUZZ_ITERS`
//! scales the per-target iteration count (default `3_000`; operators crank
//! `MC_SPINE_FUZZ_ITERS=100000` for a deep soak). A parser panic is caught,
//! the offending input is hex-dumped for reproduction, and the panic is
//! re-thrown so the test still fails.

use std::path::{Path, PathBuf};

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

use mod_adapter_core::bytecode_subset_vm::testkit::{ClassAssembler, MethodSpec};
use mod_adapter_core::bytecode_subset_vm::{
    execute_verified_method, parse_class_methods, verify_subset_method, HostCallKind, SubsetVmHost,
    VmError, VmType, VmValue,
};
use mod_adapter_core::class_mixin_scanner::{
    parse_mixin_class, scan_jar_mixins_with_options, JarScanOptions,
};
use mod_adapter_core::creature_primitives::{
    build_set_entity_data_0x58, decode_metadata_entries, DynamicEntityMetadata, MetaValue,
};
use mod_adapter_core::culinary_adapter::CulinaryRegistry;
use mod_adapter_core::ecosystem_compat::{
    plan_dual_loader_mods_dir, scan_mod_jar, scan_mod_jar_named, AccessoryInventory, AccessoryItem,
    AttributeModifier, MapFairPlayConfig, ModifierOperation,
};
use mod_adapter_core::mod_sync::{
    decode_sync_manifest, encode_sync_manifest, validate_download_url, validate_jar_file_name,
    ModSyncEntry, SyncManifest, SyncPolicy,
};
use mod_adapter_core::modpack::ModpackManifest;
use mod_adapter_core::neoforge_wire::{
    build_config_registry_data_0x07, decode_config_registry_data_0x07,
};
use mod_adapter_core::unified_bundle::{export_builtin_dimension_bundles, DeclarativeModBundle};
use mod_adapter_core::version_compat::VersionRange;
use mod_adapter_core::world_extensions::{
    decode_ai_mutation_batch, decode_ai_snapshot, decode_appleskin_f32,
    decode_chunk_gen_request, decode_chunk_gen_response, encode_ai_mutation_batch,
    encode_ai_snapshot, encode_chunk_gen_request, encode_chunk_gen_response,
    validate_chunk_gen_response, ChunkGenDelegateRequest, ChunkGenDelegateResponse,
    EntityAiDelegateSnapshot, EntityAiMutation, EntityAiMutationBatch,
};

// ---------------------------------------------------------------------------
// Deterministic PRNG (splitmix64) — the same finalizer the codebase uses.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// Per-target iteration count: `MC_SPINE_FUZZ_ITERS` overrides, default 3000.
fn iters() -> usize {
    std::env::var("MC_SPINE_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000)
}

// ---------------------------------------------------------------------------
// Panic witness: a parser panic is caught so the exact input can be dumped
// (hex, one line per 32 bytes) before the panic resumes and fails the test.
// ---------------------------------------------------------------------------

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i % 32 == 0 {
            out.push_str(&format!("\n  {i:06x}:"));
        }
        out.push_str(&format!(" {b:02x}"));
        if out.len() > 1 << 20 {
            out.push_str("\n  ... (dump truncated)");
            break;
        }
    }
    out
}

fn drive(target: &str, input: &[u8], f: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
        eprintln!("\n=== FUZZ PANIC in {target} ({} bytes) ===", input.len());
        eprintln!("{}", hex_dump(input));
        resume_unwind(payload);
    }
}

// ---------------------------------------------------------------------------
// Input generators.
// ---------------------------------------------------------------------------

/// Attacker-crafted tokens worth splicing anywhere in this crate's formats.
const DICTIONARY: &[&[u8]] = &[
    b"PK\x03\x04",
    b"PK\x01\x02",
    b"PK\x05\x06",
    b"PK\x06\x06",                         // ZIP64 EOCD
    b"PK\x06\x07",                         // ZIP64 locator
    b"MSY1",
    b"CGD1",
    b"EAD1",
    b"\xca\xfe\xba\xbe",                   // .class magic
    b"modrinth.index.json",
    b"manifest.json",
    b"minecraftModpack",
    b"../",
    b"..\\",
    b"/etc/passwd",
    b"mods/",
    b".jar",
    b"https://",
    b"http://",
    b"cdn.modrinth.com",
    b"mediafilez.forgecdn.net",
    b"Lorg/spongepowered/asm/mixin/Mixin;",
    b"Lorg/spongepowered/asm/mixin/injection/Inject;",
    b"Lorg/spongepowered/asm/mixin/injection/At;",
    b"RuntimeVisibleAnnotations",
    b"RuntimeInvisibleAnnotations",
    b"data/culinary/recipes/",
    b"\xff\xff\xff\xff",                   // u32be::MAX length bomb
    b"\xff\xff\xff\x7f",
    b"\x00\x00\x00\x80",
    b"{\"formatVersion\":1,",
    b"{\"manifestType\":\"minecraftModpack\",",
    b"\"files\":[",
    b"\"dependencies\":{\"minecraft\":\"1.21.1\"}",
    b"[[[[[[[[",
    b"\\uD800\\uDC00",
    b"\\uD800",
];

/// Mutate `seed` with a handful of primitive mutations, capped at `max_len`.
fn mutate(rng: &mut Rng, seed: &[u8], max_len: usize) -> Vec<u8> {
    let mut buf = seed.to_vec();
    let edits = 1 + rng.below(8);
    for _ in 0..edits {
        match rng.below(10) {
            0 if !buf.is_empty() => {
                let i = rng.below(buf.len());
                buf[i] ^= 1 << rng.below(8);
            }
            1 if !buf.is_empty() => {
                let i = rng.below(buf.len());
                let b = rng.byte();
                buf[i] = *rng.pick(&[0x00u8, 0x7f, 0x80, 0xff, b]);
            }
            2 if buf.len() < max_len => {
                let i = rng.below(buf.len() + 1);
                buf.insert(i, rng.byte());
            }
            3 if !buf.is_empty() => {
                let i = rng.below(buf.len());
                buf.remove(i);
            }
            4 if !buf.is_empty() => {
                let i = rng.below(buf.len());
                let n = 1 + rng.below(32.min(buf.len() - i));
                buf.drain(i..i + n);
            }
            5 if buf.len() < max_len => {
                let tok = *rng.pick(DICTIONARY);
                if buf.len() + tok.len() <= max_len {
                    let i = rng.below(buf.len() + 1);
                    buf.splice(i..i, tok.iter().copied());
                }
            }
            6 if !buf.is_empty() => {
                buf.truncate(rng.below(buf.len() + 1));
            }
            7 => {
                let n = rng.below(1 + (max_len - buf.len()).min(64));
                for _ in 0..n {
                    buf.push(rng.byte());
                }
            }
            8 if !buf.is_empty() && buf.len() < max_len => {
                let i = rng.below(buf.len());
                let n = 1 + rng.below(16.min(buf.len() - i));
                let span: Vec<u8> = buf[i..i + n].to_vec();
                let at = rng.below(buf.len() + 1);
                if buf.len() + n <= max_len {
                    buf.splice(at..at, span);
                }
            }
            _ if !buf.is_empty() => {
                let i = rng.below(buf.len());
                let n = 1 + rng.below(16.min(buf.len() - i));
                let fill = *rng.pick(&[0x00u8, 0x80, 0xff]);
                buf[i..i + n].fill(fill);
            }
            _ => {}
        }
    }
    buf.truncate(max_len);
    buf
}

/// Pathological standalone shapes: deep nests, surrogate escapes, wire
/// length bombs, archive framing lies.
fn bomb(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    match rng.below(12) {
        0 => {
            // JSON array nesting far past any sane depth cap.
            let depth = 1 + rng.below(300);
            out.extend(std::iter::repeat_n(b'[', depth));
            out.extend(std::iter::repeat_n(b']', depth));
        }
        1 => {
            // Nested JSON objects.
            let depth = 1 + rng.below(200);
            for _ in 0..depth {
                out.extend_from_slice(b"{\"a\":");
            }
            out.push(b'1');
            out.extend(std::iter::repeat_n(b'}', depth));
        }
        2 => {
            // Surrogate escapes: lone halves, reversed pairs, bad hex.
            for esc in [
                &b"\"\\uD800\""[..],
                b"\"\\uDC00\"",
                b"\"\\uDC00\\uD800\"",
                b"\"\\uD800\\u0041\"",
                b"\"\\uXYZW\"",
                b"\"\\u\"",
            ] {
                out.extend_from_slice(esc);
            }
        }
        3 => {
            // Numbers: past i64, absurd exponents, malformed.
            for n in [
                &b"9223372036854775808"[..],
                b"-9223372036854775809",
                b"1e999999999",
                b"1.",
                b".5",
                b"01",
                b"--5",
            ] {
                out.extend_from_slice(n);
                out.push(b',');
            }
        }
        4 => {
            // MSY1 header claiming the max entry count with no body.
            out.extend_from_slice(b"MSY1");
            out.extend_from_slice(&u16::MAX.to_be_bytes());
            let n = rng.below(64);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        5 => {
            // VarInt run that never terminates (5+ continuation bytes).
            out.extend(std::iter::repeat_n(0x80, 4 + rng.below(8)));
            out.extend_from_slice(&u32::MAX.to_be_bytes());
        }
        6 => {
            // ZIP local header lying about sizes, no central directory.
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&u32::MAX.to_le_bytes()); // crc
            out.extend_from_slice(&u32::MAX.to_le_bytes()); // compressed
            out.extend_from_slice(&u32::MAX.to_le_bytes()); // uncompressed
            out.extend_from_slice(&u16::MAX.to_le_bytes()); // name len
            let n = rng.below(32);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        7 => {
            // EOCD advertising absurd entry counts / offsets.
            out.extend_from_slice(b"PK\x05\x06");
            out.extend_from_slice(&u16::MAX.to_le_bytes());
            out.extend_from_slice(&u16::MAX.to_le_bytes());
            out.extend_from_slice(&u16::MAX.to_le_bytes());
            out.extend_from_slice(&u16::MAX.to_le_bytes());
            out.extend_from_slice(&u32::MAX.to_le_bytes());
            out.extend_from_slice(&u32::MAX.to_le_bytes());
            out.extend_from_slice(&u16::MAX.to_le_bytes());
        }
        8 => {
            // JVMS .class header: valid magic, hostile pool/interface counts.
            out.extend_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
            out.extend_from_slice(&u16::MAX.to_be_bytes()); // minor
            out.extend_from_slice(&u16::MAX.to_be_bytes()); // major
            out.extend_from_slice(&u16::MAX.to_be_bytes()); // cp_count
            let n = rng.below(48);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        9 => {
            // EAD1 frame with a huge mutation count claim.
            out.extend_from_slice(b"EAD1");
            out.push(0x02);
            out.extend_from_slice(&u32::MAX.to_be_bytes());
            out.extend_from_slice(&u32::MAX.to_be_bytes());
            let n = rng.below(48);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        10 => {
            // 0x58 metadata stream: type byte spam, no terminator.
            out.extend_from_slice(&u32::MAX.to_be_bytes()); // entity id
            let n = rng.below(64);
            for _ in 0..n {
                out.push(rng.byte() & 0x7f);
                out.push(rng.byte());
            }
        }
        _ => {
            let n = rng.below(512);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
    }
    out
}

/// The next fuzz input: a corpus mutation, a boundary shape, a bomb, or raw
/// noise, weighted toward corpus-guided cases.
fn fuzz_input(rng: &mut Rng, corpus: &[Vec<u8>], max_len: usize) -> Vec<u8> {
    match rng.below(100) {
        0..=44 => {
            let seed = rng.pick(corpus);
            mutate(rng, seed, max_len)
        }
        45..=59 => {
            let seed = rng.pick(corpus);
            let mut v = seed[..rng.below(seed.len() + 1)].to_vec();
            if rng.below(4) == 0 {
                let n = rng.below(24);
                v.extend(std::iter::repeat_with(|| rng.byte()).take(n));
            }
            v.truncate(max_len);
            v
        }
        60..=69 => {
            let a = rng.pick(corpus);
            let b = rng.pick(corpus);
            let mut v = Vec::with_capacity(a.len() + b.len());
            v.extend_from_slice(&a[..rng.below(a.len() + 1)]);
            v.extend_from_slice(&b[..rng.below(b.len() + 1)]);
            v.truncate(max_len);
            v
        }
        70..=79 => {
            let mut v = bomb(rng);
            v.truncate(max_len);
            v
        }
        _ => {
            let n = if rng.below(16) == 0 {
                rng.below(max_len + 1)
            } else {
                rng.below(257)
            };
            std::iter::repeat_with(|| rng.byte()).take(n).collect()
        }
    }
}

// ---------------------------------------------------------------------------
// ZIP / JAR / deflate builders — real framing so corpus seeds reach deep.
// ---------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Raw-deflate stored blocks (BTYPE=00) wrapping `data` verbatim — a legal
/// method-8 stream with ~1:1 expansion.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunks = data.chunks(0xffff);
    let n = chunks.len().max(1);
    for (i, chunk) in chunks.by_ref().enumerate() {
        out.push(u8::from(i + 1 == n)); // BFINAL on the last block
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    out
}

/// LSB-first bit writer for hand-rolled fixed-Huffman deflate streams.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }

    /// Emit `bits` low bits of `code`, LSB first (deflate bit order).
    fn put(&mut self, code: u32, bits: u32) {
        self.acc |= code << self.nbits;
        self.nbits += bits;
        while self.nbits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// Reverse the low `bits` of `v` — fixed-Huffman codes go MSB-first onto
/// the LSB-first wire.
fn rev_bits(v: u32, bits: u32) -> u32 {
    let mut r = 0u32;
    for i in 0..bits {
        r |= ((v >> i) & 1) << (bits - 1 - i);
    }
    r
}

/// A real fixed-Huffman deflate stream expanding to `total` copies of
/// `byte`: one literal, then `length-258 / distance-1` copy pairs — a
/// genuine zip-bomb ratio (~13 input bits per 258 output bytes).
fn deflate_repeat_bomb(byte: u8, total: usize) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(1, 1); // BFINAL
    w.put(0b01, 2); // BTYPE = fixed Huffman
    if total == 0 {
        w.put(rev_bits(0, 7), 7); // end-of-block
        return w.finish();
    }
    let literal = |w: &mut BitWriter, b: u8| {
        let code = if (b as u32) < 144 {
            rev_bits(0x30 + b as u32, 8)
        } else {
            rev_bits(0x190 + (b as u32 - 144), 9)
        };
        w.put(code, if (b as u32) < 144 { 8 } else { 9 });
    };
    literal(&mut w, byte);
    let mut remaining = total - 1;
    while remaining >= 258 {
        w.put(rev_bits(0xC5, 8), 8); // length code 285 -> 258 bytes
        w.put(rev_bits(0, 5), 5); // distance code 0 -> back 1
        remaining -= 258;
    }
    for _ in 0..remaining {
        literal(&mut w, byte);
    }
    w.put(rev_bits(0, 7), 7); // end-of-block
    w.finish()
}

/// Pack `entries` `(name, method, stored_bytes, uncompressed_len, crc)` into
/// a real ZIP: local headers, central directory, EOCD. `stored` selects
/// method 0 (verbatim) vs 8 (raw deflate over `stored_bytes`).
fn zip_build(entries: &[(&str, u16, &[u8], u32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, method, data, uncomp_len, crc) in entries {
        let name_b = name.as_bytes();
        let offset = out.len() as u32;
        out.extend(0x0403_4B50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes());
        out.extend(0u16.to_le_bytes()); // flags
        out.extend(method.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(uncomp_len.to_le_bytes());
        out.extend((name_b.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes()); // extra len
        out.extend(name_b);
        out.extend(*data);

        central.extend(0x0201_4B50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(method.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(crc.to_le_bytes());
        central.extend((data.len() as u32).to_le_bytes());
        central.extend(uncomp_len.to_le_bytes());
        central.extend((name_b.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name_b);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4B50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

/// Stored-member convenience wrapper over [`zip_build`].
fn zip_stored(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let framed: Vec<(&str, u16, &[u8], u32, u32)> = entries
        .iter()
        .map(|(name, data)| (*name, 0u16, *data, data.len() as u32, crc32(data)))
        .collect();
    zip_build(&framed)
}

// ---------------------------------------------------------------------------
// Temp-file plumbing for the path-based parsers (`from_path`). One fixed
// scratch file per test keeps the write count bounded and leaves nothing
// behind.
// ---------------------------------------------------------------------------

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mc-spine-fuzz-p6e-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn scratch_path(dir: &Path, file_name: &str) -> PathBuf {
    dir.join(file_name)
}

fn write_scratch(path: &PathBuf, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

// ---------------------------------------------------------------------------
// JVMS .class builder for bytecode-parser corpus seeds.
// ---------------------------------------------------------------------------

/// One constant-pool item for the seed builder; `Wide` is the unusable
/// second slot of a `Long`/`Double`.
enum CpItem {
    Utf8(&'static str),
    Class(u16),
    String(u16),
    Int(i32),
    Long,
    Wide,
}

fn cp_bytes(items: &[CpItem]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        match item {
            CpItem::Utf8(s) => {
                out.push(1);
                out.extend((s.len() as u16).to_be_bytes());
                out.extend(s.as_bytes());
            }
            CpItem::Class(name_index) => {
                out.push(7);
                out.extend(name_index.to_be_bytes());
            }
            CpItem::String(utf8_index) => {
                out.push(8);
                out.extend(utf8_index.to_be_bytes());
            }
            CpItem::Int(v) => {
                out.push(3);
                out.extend(v.to_be_bytes());
            }
            CpItem::Long => {
                out.push(5);
                out.extend(0xdead_beefu32.to_be_bytes());
                out.extend(0xdead_beefu32.to_be_bytes());
            }
            CpItem::Wide => {}
        }
    }
    out
}

/// `element_value` fragment for the @Mixin class-level array.
const EV_CLASS_TARGET: &[u8] = &[b'c', 0, 6]; // Class -> target class name

/// A `.class` file with real JVMS framing: a populated constant pool, a
/// class-level `RuntimeVisibleAnnotations` carrying `@Mixin`, and one
/// method whose `RuntimeVisibleAnnotations` carries `@Inject` with a
/// nested `@At`.
fn mixin_class_seed() -> Vec<u8> {
    // cp layout (1-based):
    //  1 Utf8 "Lorg/spongepowered/asm/mixin/Mixin;"
    //  2 Utf8 "RuntimeVisibleAnnotations"
    //  3 Utf8 "RuntimeInvisibleAnnotations"
    //  4 Utf8 "value"
    //  5 Utf8 "com/example/Target"
    //  6 Class(5)
    //  7 Utf8 "Lorg/spongepowered/asm/mixin/injection/Inject;"
    //  8 Utf8 "method"
    //  9 Utf8 "Lorg/spongepowered/asm/mixin/injection/At;"
    // 10 Utf8 "value"
    // 11 Utf8 "target()V"
    // 12 Utf8 "target"
    // 13 Utf8 "Lorg/spongepowered/asm/mixin/injection/At$Shift;"
    // 14 Utf8 "INVOKE"
    // 15 Utf8 "TestMixin"
    // 16 Class(15)
    // 17 Utf8 "java/lang/Object"
    // 18 Class(17)
    // 19 Utf8 "method()V"
    // 20 Utf8 "()V"
    // 21 String(5)
    // 22 Int(0x2a)
    // 23-24 Long (takes two slots)
    // 25 Utf8 "at"
    let cp = cp_bytes(&[
        CpItem::Utf8("Lorg/spongepowered/asm/mixin/Mixin;"),               // 1
        CpItem::Utf8("RuntimeVisibleAnnotations"),                        // 2
        CpItem::Utf8("RuntimeInvisibleAnnotations"),                      // 3
        CpItem::Utf8("value"),                                            // 4
        CpItem::Utf8("com/example/Target"),                               // 5
        CpItem::Class(5),                                                 // 6
        CpItem::Utf8("Lorg/spongepowered/asm/mixin/injection/Inject;"),   // 7
        CpItem::Utf8("method"),                                           // 8
        CpItem::Utf8("Lorg/spongepowered/asm/mixin/injection/At;"),       // 9
        CpItem::Utf8("value"),                                            // 10
        CpItem::Utf8("target()V"),                                        // 11
        CpItem::Utf8("target"),                                           // 12
        CpItem::Utf8("Lorg/spongepowered/asm/mixin/injection/At$Shift;"), // 13
        CpItem::Utf8("INVOKE"),                                           // 14
        CpItem::Utf8("TestMixin"),                                        // 15
        CpItem::Class(15),                                                // 16
        CpItem::Utf8("java/lang/Object"),                                 // 17
        CpItem::Class(17),                                                // 18
        CpItem::Utf8("method()V"),                                        // 19
        CpItem::Utf8("()V"),                                              // 20
        CpItem::String(5),                                                // 21
        CpItem::Int(0x2a),                                                // 22
        CpItem::Long,                                                     // 23
        CpItem::Wide,                                                     // 24
        CpItem::Utf8("at"),                                               // 25
    ]);
    let cp_count = 26u16;

    // Class-level RuntimeVisibleAnnotations: one @Mixin with
    // value = [class com/example/Target].
    let mut class_attr_body = Vec::new();
    class_attr_body.extend(1u16.to_be_bytes()); // num_annotations
    class_attr_body.extend(1u16.to_be_bytes()); // type = @Mixin (cp 1)
    class_attr_body.extend(1u16.to_be_bytes()); // one element pair
    class_attr_body.extend(4u16.to_be_bytes()); // "value"
    class_attr_body.push(b'['); // array of classes
    class_attr_body.extend(1u16.to_be_bytes());
    class_attr_body.extend(EV_CLASS_TARGET);

    // Method-level RuntimeVisibleAnnotations: one @Inject with
    // method = "method()V", at = {@At(value="target()V", target=INVOKE)}.
    let mut method_attr_body = Vec::new();
    method_attr_body.extend(1u16.to_be_bytes());
    method_attr_body.extend(7u16.to_be_bytes()); // type = @Inject (cp 7)
    method_attr_body.extend(2u16.to_be_bytes()); // two element pairs
    method_attr_body.extend(8u16.to_be_bytes()); // "method"
    method_attr_body.push(b's');
    method_attr_body.extend(19u16.to_be_bytes()); // "method()V"
    method_attr_body.extend(25u16.to_be_bytes()); // "at"
    method_attr_body.push(b'@'); // nested @At annotation
    method_attr_body.extend(9u16.to_be_bytes()); // type = @At (cp 9)
    method_attr_body.extend(2u16.to_be_bytes()); // two element pairs
    method_attr_body.extend(10u16.to_be_bytes()); // "value"
    method_attr_body.push(b's');
    method_attr_body.extend(11u16.to_be_bytes()); // "target()V"
    method_attr_body.extend(12u16.to_be_bytes()); // "target"
    method_attr_body.push(b'e'); // enum
    method_attr_body.extend(13u16.to_be_bytes()); // At$Shift descriptor
    method_attr_body.extend(14u16.to_be_bytes()); // "INVOKE"

    let mut out = Vec::new();
    out.extend([0xca, 0xfe, 0xba, 0xbe]);
    out.extend(0u16.to_be_bytes()); // minor
    out.extend(61u16.to_be_bytes()); // major (Java 17)
    out.extend(cp_count.to_be_bytes());
    out.extend(cp);
    out.extend(0x0021u16.to_be_bytes()); // public | super
    out.extend(16u16.to_be_bytes()); // this_class
    out.extend(18u16.to_be_bytes()); // super_class
    out.extend(0u16.to_be_bytes()); // interfaces
    out.extend(0u16.to_be_bytes()); // fields
    out.extend(1u16.to_be_bytes()); // methods
    {
        out.extend(0x0001u16.to_be_bytes()); // public
        out.extend(19u16.to_be_bytes()); // name "method()V"
        out.extend(20u16.to_be_bytes()); // desc "()V"
        out.extend(1u16.to_be_bytes()); // one attribute
        out.extend(2u16.to_be_bytes()); // RuntimeVisibleAnnotations
        out.extend((method_attr_body.len() as u32).to_be_bytes());
        out.extend(method_attr_body);
    }
    out.extend(1u16.to_be_bytes()); // class attributes
    out.extend(2u16.to_be_bytes()); // RuntimeVisibleAnnotations
    out.extend((class_attr_body.len() as u32).to_be_bytes());
    out.extend(class_attr_body);
    out
}

// ---------------------------------------------------------------------------
// Corpus seeds.
// ---------------------------------------------------------------------------

const MODRINTH_JSON: &[u8] = br#"{
  "formatVersion": 1,
  "game": "minecraft",
  "name": "fuzzpack",
  "versionId": "1.0.0",
  "dependencies": {"minecraft": "1.21.1", "fabric-loader": "0.15.0"},
  "files": [
    {
      "path": "mods/example-1.0.jar",
      "hashes": {"sha1": "0123456789abcdef0123456789abcdef01234567",
                 "sha512": "ab"},
      "env": {"client": "required", "server": "required"},
      "downloads": ["https://cdn.modrinth.com/data/x/example.jar"],
      "fileSize": 12345
    }
  ]
}"#;

const CURSEFORGE_JSON: &[u8] = br#"{
  "manifestType": "minecraftModpack",
  "manifestVersion": 1,
  "name": "fuzzpack",
  "version": "1.0.0",
  "author": "fuzz",
  "minecraft": {
    "version": "1.21.1",
    "modLoaders": [{"id": "neoforge-21.1.0", "primary": true}]
  },
  "files": [
    {"projectID": 123456, "fileID": 7890, "required": true,
     "slug": "example-mod", "fileName": "example-1.0.jar"}
  ]
}"#;

/// `.mrpack`-shaped archives: real ZIPs whose manifest member is stored or
/// raw-deflate, plus hostile member-name variants.
fn modpack_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    let modrinth_deflate = deflate_stored(MODRINTH_JSON);
    let cf_deflate = deflate_stored(CURSEFORGE_JSON);
    corpus.push(zip_build(&[
        ("modrinth.index.json", 8, modrinth_deflate.as_slice(), MODRINTH_JSON.len() as u32, crc32(MODRINTH_JSON)),
        ("overrides/mods/x.jar", 0, b"\xca\xfe\xba\xbe", 4, crc32(b"\xca\xfe\xba\xbe")),
    ]));
    corpus.push(zip_build(&[
        ("manifest.json", 8, cf_deflate.as_slice(), CURSEFORGE_JSON.len() as u32, crc32(CURSEFORGE_JSON)),
    ]));
    corpus.push(zip_stored(&[
        ("modrinth.index.json", MODRINTH_JSON),
        ("overrides/config.cfg", b"a=b"),
    ]));
    corpus.push(zip_stored(&[("manifest.json", CURSEFORGE_JSON)]));
    // Path-traversal member names — the reader must reject, never write.
    for evil in [
        "../evil.jar",
        "..\\evil.jar",
        "/abs/evil.jar",
        "mods/../../evil.jar",
        "mods//double.jar",
        "",
        ".",
    ] {
        corpus.push(zip_build(&[
            ("modrinth.index.json", 0, MODRINTH_JSON, MODRINTH_JSON.len() as u32, crc32(MODRINTH_JSON)),
            (evil, 0, b"x", 1, crc32(b"x")),
        ]));
    }
    // Wrong-CRC and wrong-size deflate members.
    corpus.push(zip_build(&[
        ("modrinth.index.json", 8, modrinth_deflate.as_slice(), MODRINTH_JSON.len() as u32, 0xdead_beef),
    ]));
    corpus.push(zip_build(&[
        ("modrinth.index.json", 8, modrinth_deflate.as_slice(), 0xffff_ffff, crc32(MODRINTH_JSON)),
    ]));
    // Unknown compression method.
    corpus.push(zip_build(&[
        ("modrinth.index.json", 99, b"zz", 2, crc32(b"zz")),
    ]));
    // Standalone JSON documents (the `from_standalone_json` branch).
    corpus.push(MODRINTH_JSON.to_vec());
    corpus.push(CURSEFORGE_JSON.to_vec());
    corpus.push(b"{\"formatVersion\":1}".to_vec());
    corpus.push(b"{\"manifestType\":\"minecraftModpack\"}".to_vec());
    // Not a zip at all / truncated archives.
    corpus.push(b"PK\x03\x04".to_vec());
    corpus.push(Vec::new());
    corpus
}

/// Real zip bombs kept out of the mutation corpus (truncating them below
/// the member cap would defuse them): a 4 MiB inflater workout inside the
/// budget and a 17 MiB stream that must hit the inflate byte cap, not OOM.
fn modpack_bombs() -> Vec<Vec<u8>> {
    let bomb_payload = vec![b'A'; 4 * 1024 * 1024];
    let bomb_stream = deflate_repeat_bomb(b'A', bomb_payload.len());
    let inside_budget = zip_build(&[
        ("modrinth.index.json", 8, bomb_stream.as_slice(), bomb_payload.len() as u32, crc32(&bomb_payload)),
    ]);
    let over_stream = deflate_repeat_bomb(b'B', 17 * 1024 * 1024);
    let over_budget = zip_build(&[
        ("modrinth.index.json", 8, over_stream.as_slice(), 17 * 1024 * 1024u32, crc32(&[b'B'; 64])),
    ]);
    vec![inside_budget, over_budget]
}

/// `.class`-file corpus: one real annotated class plus framing variants.
fn class_corpus() -> Vec<Vec<u8>> {
    let mut corpus = vec![mixin_class_seed()];
    // Magic + minimal empty-pool class.
    corpus.push(vec![
        0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 61, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]);
    // A class whose cp_count overruns the real entries.
    let mut over = mixin_class_seed();
    over[8] = 0xff;
    over[9] = 0xff;
    corpus.push(over);
    corpus
}

/// JAR corpus for the mixin scanner and the culinary loader: archives
/// carrying mixin configs, classes, and culinary data files.
fn jar_corpus() -> Vec<Vec<u8>> {
    let class = mixin_class_seed();
    let mixin_cfg: &[u8] = br#"{
      "required": true,
      "package": "com.example.mixin",
      "mixins": ["TestMixin"],
      "client": ["ClientOnlyMixin"],
      "server": [],
      "minVersion": "0.8"
    }"#;
    let recipe: &[u8] = br#"{
      "type": "farmersdelight:cooking",
      "ingredients": [{"item": "minecraft:beef"}],
      "result": {"item": "minecraft:beef", "count": 1},
      "container": {"item": "minecraft:bowl"},
      "cookingtime": 200
    }"#;
    let tag: &[u8] = br##"{
      "replace": false,
      "values": ["minecraft:beef", "#minecraft:meat"]
    }"##;
    let food: &[u8] = br#"{"nutrition": 4, "saturation": 0.3}"#;
    let loot: &[u8] = br#"{
      "type": "farmersdelight:add_item",
      "conditions": [],
      "item": "minecraft:carrot",
      "chance": 0.5
    }"#;
    let mut corpus = vec![
        zip_stored(&[
            ("test.mixins.json", mixin_cfg),
            ("com/example/mixin/TestMixin.class", &class),
        ]),
        zip_stored(&[("com/example/mixin/TestMixin.class", &class)]),
        zip_stored(&[
            ("data/farmersdelight/recipes/cooking/beef.json", recipe),
            ("data/farmersdelight/recipes/cutting/beef.json", recipe),
            ("data/farmersdelight/tags/items/meat.json", tag),
            ("data/c/tags/items/foods.json", tag),
            ("data/farmersdelight/loot_modifiers/carrot.json", loot),
            ("data/farmersdelight/food/beef.json", food),
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\n"),
        ]),
    ];
    // Deflate member versions of the same JAR.
    let class_def = deflate_stored(&class);
    let cfg_def = deflate_stored(mixin_cfg);
    corpus.push(zip_build(&[
        ("test.mixins.json", 8, cfg_def.as_slice(), mixin_cfg.len() as u32, crc32(mixin_cfg)),
        ("com/example/mixin/TestMixin.class", 8, class_def.as_slice(), class.len() as u32, crc32(&class)),
    ]));
    // JAR with a deflate bomb member riding alongside real members.
    let food_bomb_stream = deflate_repeat_bomb(b'{', 1024 * 1024);
    let food_bomb_payload = vec![b'{'; 1024 * 1024];
    corpus.push(zip_build(&[
        ("data/farmersdelight/recipes/cooking/bomb.json", 8, food_bomb_stream.as_slice(),
         1024 * 1024, crc32(&food_bomb_payload)),
    ]));
    corpus
}

/// `MSY1` bodies plus URL/filename strings for the mod-sync validators.
fn mod_sync_corpus() -> Vec<Vec<u8>> {
    let entry = ModSyncEntry {
        mod_id: "create".to_string(),
        file_name: "create-1.0.jar".to_string(),
        download_url: "https://cdn.modrinth.com/data/abcd/create-1.0.jar".to_string(),
        sha256_hex: "ab".repeat(32),
        size_bytes: 1024,
    };
    let mut corpus = Vec::new();
    for count in 0..3usize {
        let manifest = SyncManifest {
            entries: std::iter::repeat_n(entry.clone(), count).collect(),
        };
        if let Ok(bytes) = encode_sync_manifest(&manifest) {
            corpus.push(bytes);
        }
    }
    corpus.push(b"MSY1\x00\x00".to_vec());
    corpus.push(b"MSY1\xff\xff".to_vec());
    // URL and filename fuzz seeds ride in the same corpus so mutations
    // splice wire bytes into validator strings too.
    for s in [
        &b"https://cdn.modrinth.com/x.jar"[..],
        &b"http://cdn.modrinth.com/x.jar"[..],
        &b"https://evil.example.com/x.jar"[..],
        &b"https://cdn.modrinth.com@evil.example/x.jar"[..],
        &b"https://cdn.modrinth.com.evil.example/x.jar"[..],
        &b"https://127.0.0.1/x.jar"[..],
        &b"https://[::1]/x.jar"[..],
        &b"file:///etc/passwd"[..],
        &b"mod.jar"[..],
        &b"../mod.jar"[..],
        &b"mod.jar\x00.png"[..],
        &b"mod.JAR"[..],
        &b".jar"[..],
    ] {
        corpus.push(s.to_vec());
    }
    corpus.push(vec![b'a'; 300]);
    corpus
}

/// `CGD1`/`EAD1` delegate frames from the crate's own encoders.
fn delegate_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    let req = ChunkGenDelegateRequest {
        dim: "minecraft:overworld".to_string(),
        seed: -42,
        cx: 3,
        cz: -7,
        min_y: -64,
        height: 384,
    };
    if let Ok(bytes) = encode_chunk_gen_request(&req) {
        corpus.push(bytes);
    }
    let resp = ChunkGenDelegateResponse {
        cx: 3,
        cz: -7,
        sections: vec![vec![0u32; 4096]; 2],
    };
    if let Ok(bytes) = encode_chunk_gen_response(&resp) {
        corpus.push(bytes);
    }
    let snap = EntityAiDelegateSnapshot {
        entity_id: 7,
        pos_xyz: [1.5, 64.0, -9.25],
        yaw_deg: 180.0,
        pitch_deg: -45.0,
        health: 10.0,
        max_health: 20.0,
        target_entity_id: Some(9),
    };
    corpus.push(encode_ai_snapshot(&snap));
    let batch = EntityAiMutationBatch {
        entity_id: 7,
        mutations: vec![
            EntityAiMutation::MoveTo {
                xyz: [0.0, 64.0, 0.0],
                speed: 0.5,
            },
            EntityAiMutation::SetVelocity { xyz: [0.0, 1.0, 0.0] },
            EntityAiMutation::AttackEntity {
                target_entity_id: 9,
                damage: 4.0,
            },
            EntityAiMutation::SetBlock {
                pos: [1, 2, 3],
                state_id: 42,
            },
            EntityAiMutation::SpawnParticle {
                particle_id: 7,
                xyz: [0.0, 0.0, 0.0],
                count: 10,
            },
            EntityAiMutation::PlaySound {
                sound_id: "minecraft:entity.zombie.ambient".to_string(),
                volume: 1.0,
                pitch: 1.0,
            },
        ],
    };
    if let Ok(bytes) = encode_ai_mutation_batch(&batch) {
        corpus.push(bytes);
    }
    corpus.push(b"CGD1".to_vec());
    corpus.push(b"EAD1".to_vec());
    corpus
}

/// `0x58` entry streams and NeoForge `0x07` registry bodies.
fn entity_meta_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    let mut meta = DynamicEntityMetadata::new();
    meta.set(0, MetaValue::Byte(0x40));
    meta.set(8, MetaValue::Pose(7));
    meta.set(9, MetaValue::VarInt(1234));
    meta.set(10, MetaValue::Float(1.5));
    meta.set(11, MetaValue::String("variant".to_string()));
    meta.set(12, MetaValue::Bool(true));
    meta.set(13, MetaValue::OptionalUuid(Some([0xab; 16])));
    corpus.push(build_set_entity_data_0x58(42, &mut meta));
    // Bare entry stream (no packet header) terminated by 0xff.
    corpus.push(vec![0, 0, 0x7f, 8, 21, 2, 0xff]);
    corpus.push(vec![0xff]);
    corpus.push(b"\x07\x0fminecraft:block".to_vec());
    if let Ok(body) = build_config_registry_data_0x07(
        "mcspine:custom",
        &["minecraft:stone".to_string(), "minecraft:dirt".to_string()],
    ) {
        corpus.push(body);
    }
    if let Ok(body) = build_config_registry_data_0x07("minecraft:block", &[]) {
        corpus.push(body);
    }
    corpus
}

/// Version-range strings for `VersionRange::parse`.
fn version_range_corpus() -> Vec<Vec<u8>> {
    [
        "1.21.1",
        "[1.20,1.22)",
        "(,1.21.1]",
        "[1.21.1,)",
        "(,)",
        "~1.21",
        ">=1.20 <1.22",
        "1.x",
        "[0,999999)",
        "1.20.1-alpha.3",
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect()
}

// ---------------------------------------------------------------------------
// Modpack manifest: archive reader + both JSON dialects.
// ---------------------------------------------------------------------------

#[test]
fn modpack_manifest_never_panics() {
    let mut rng = Rng::new(0x40DC_A0CC_0001);
    let corpus = modpack_corpus();
    let iters = iters();
    let names = ["fuzzpack.mrpack", "manifest.json", "modrinth.index.json", "pack.zip"];
    let dir = scratch_dir("modpack");
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 1024 * 1024);
        // Archive branch: from_path reads bytes off disk and runs the whole
        // ZIP + raw-deflate + path-jail + JSON pipeline.
        let file_name = *rng.pick(&names);
        let path = scratch_path(&dir, file_name);
        write_scratch(&path, &input);
        drive("ModpackManifest::from_path", &input, || {
            let _ = ModpackManifest::from_path(&path);
        });
        // Pure JSON branches get the same bytes lossy-decoded, plus raw
        // UTF-8 passes so the valid seeds reach the typed parser.
        let text = String::from_utf8_lossy(&input);
        drive("ModpackManifest::from_modrinth_json", &input, || {
            let _ = ModpackManifest::from_modrinth_json(&text);
        });
        drive("ModpackManifest::from_curseforge_json", &input, || {
            let _ = ModpackManifest::from_curseforge_json(&text);
        });
        if let Ok(text) = std::str::from_utf8(&input) {
            drive("from_json::strict", &input, || {
                let _ = ModpackManifest::from_modrinth_json(text);
                let _ = ModpackManifest::from_curseforge_json(text);
            });
        }
        decoded += 1;
    }
    // Deterministic bomb runs at full size — the inside-budget stream must
    // inflate fully then fail as JSON; the over-budget stream must stop at
    // the member byte cap, never OOM.
    for input in modpack_bombs() {
        let path = scratch_path(&dir, "bomb.mrpack");
        write_scratch(&path, &input);
        drive("from_path::zip_bomb", &input, || {
            let _ = ModpackManifest::from_path(&path);
        });
        decoded += 1;
    }
    // Deterministic sweeps over the small seeds:
    // every truncation and first-64-byte flips through the archive reader.
    for seed in corpus.iter().filter(|s| s.len() <= 4096) {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            let path = scratch_path(&dir, "cut.mrpack");
            write_scratch(&path, input);
            drive("from_path::truncate", input, || {
                let _ = ModpackManifest::from_path(&path);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x7f, 0x80, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                let path = scratch_path(&dir, "flip.mrpack");
                write_scratch(&path, &input);
                drive("from_path::flip", &input, || {
                    let _ = ModpackManifest::from_path(&path);
                });
                decoded += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("modpack_manifest: {decoded} inputs x ~4 parser calls");
}

// ---------------------------------------------------------------------------
// JVMS .class parser + JAR mixin scanner + culinary JAR loader.
// ---------------------------------------------------------------------------

#[test]
fn class_and_jar_parsers_never_panic() {
    let mut rng = Rng::new(0xC1A5_5BAD_0002);
    let classes = class_corpus();
    let jars = jar_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        // .class bytes against the constant-pool/annotation walker.
        let input = fuzz_input(&mut rng, &classes, 1024 * 1024);
        drive("parse_mixin_class", &input, || {
            let _ = parse_mixin_class(&input);
        });
        decoded += 1;
        // JAR bytes against the archive-driven mixin scanner and the
        // culinary data loader — same ZIP code, different consumers.
        let jar = fuzz_input(&mut rng, &jars, 4 * 1024 * 1024);
        drive("scan_jar_mixins_with_options", &jar, || {
            for opts in [
                JarScanOptions { server_only: true },
                JarScanOptions { server_only: false },
            ] {
                let _ = scan_jar_mixins_with_options(&jar, opts);
            }
        });
        drive("CulinaryRegistry::load_from_jar_bytes", &jar, || {
            let mut registry = CulinaryRegistry::new();
            let _ = registry.load_from_jar_bytes(&jar);
        });
        decoded += 1;
    }
    for seed in classes.iter().chain(jars.iter()).filter(|s| s.len() <= 4096) {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("parse_mixin_class::truncate", input, || {
                let _ = parse_mixin_class(input);
            });
            drive("scan_jar::truncate", input, || {
                let _ = scan_jar_mixins_with_options(input, JarScanOptions::default());
            });
            drive("load_jar::truncate", input, || {
                let mut registry = CulinaryRegistry::new();
                let _ = registry.load_from_jar_bytes(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x07, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("class_jar::flip", &input, || {
                    let _ = parse_mixin_class(&input);
                    let _ = scan_jar_mixins_with_options(&input, JarScanOptions::default());
                    let mut registry = CulinaryRegistry::new();
                    let _ = registry.load_from_jar_bytes(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("class_and_jar: {decoded} inputs x ~4 parser calls");
}

// ---------------------------------------------------------------------------
// MSY1 sync manifest + URL / filename validators.
// ---------------------------------------------------------------------------

#[test]
fn mod_sync_parsers_never_panic() {
    let mut rng = Rng::new(0x4571_1C00_0003);
    let corpus = mod_sync_corpus();
    let iters = iters();
    let policy = SyncPolicy::canonical();
    let mirror_policy =
        SyncPolicy::with_operator_mirrors(&["mirror.example.org"]).unwrap_or_default();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 4 * 1024 * 1024);
        drive("decode_sync_manifest", &input, || {
            let _ = decode_sync_manifest(&input);
        });
        let text = String::from_utf8_lossy(&input);
        drive("validate_download_url", &input, || {
            let _ = validate_download_url(&text, &policy);
            let _ = validate_download_url(&text, &mirror_policy);
        });
        drive("validate_jar_file_name", &input, || {
            let _ = validate_jar_file_name(&text);
        });
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("decode_sync_manifest::truncate", input, || {
                let _ = decode_sync_manifest(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x2f, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("msy1::flip", &input, || {
                    let _ = decode_sync_manifest(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("mod_sync: {decoded} inputs x 4 parser calls");
}

// ---------------------------------------------------------------------------
// CGD1 / EAD1 delegate frames + AppleSkin wire value.
// ---------------------------------------------------------------------------

#[test]
fn delegate_frame_decoders_never_panic() {
    let mut rng = Rng::new(0xDE1E_6A7E_0004);
    let corpus = delegate_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 1024 * 1024);
        drive("decode_chunk_gen_request", &input, || {
            let _ = decode_chunk_gen_request(&input);
        });
        drive("decode_chunk_gen_response", &input, || {
            let _ = decode_chunk_gen_response(&input);
        });
        drive("decode_ai_snapshot", &input, || {
            let _ = decode_ai_snapshot(&input);
        });
        drive("decode_ai_mutation_batch", &input, || {
            let _ = decode_ai_mutation_batch(&input);
        });
        drive("decode_appleskin_f32", &input, || {
            let _ = decode_appleskin_f32(&input);
        });
        // Round-trip path: a decoded response is fed back through the
        // contract check with a random request.
        drive("validate_chunk_gen_response", &input, || {
            if let Some(resp) = decode_chunk_gen_response(&input) {
                let req = ChunkGenDelegateRequest {
                    dim: "minecraft:overworld".to_string(),
                    seed: 0,
                    cx: resp.cx,
                    cz: resp.cz,
                    min_y: -64,
                    height: 384,
                };
                let _ = validate_chunk_gen_response(&req, &resp);
            }
        });
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len().min(4096) {
            let input = &seed[..cut];
            drive("delegate::truncate", input, || {
                let _ = decode_chunk_gen_request(input);
                let _ = decode_chunk_gen_response(input);
                let _ = decode_ai_snapshot(input);
                let _ = decode_ai_mutation_batch(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x7f, 0x80, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("delegate::flip", &input, || {
                    let _ = decode_chunk_gen_request(&input);
                    let _ = decode_chunk_gen_response(&input);
                    let _ = decode_ai_snapshot(&input);
                    let _ = decode_ai_mutation_batch(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("delegate_frames: {decoded} inputs x ~6 decoder calls");
}

// ---------------------------------------------------------------------------
// 0x58 metadata entries + NeoForge 0x07 registry data + version ranges.
// ---------------------------------------------------------------------------

#[test]
fn metadata_registry_and_version_parsers_never_panic() {
    let mut rng = Rng::new(0x5E17_1A7A_0005);
    let corpus = entity_meta_corpus();
    let versions = version_range_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 4 * 1024 * 1024);
        drive("decode_metadata_entries", &input, || {
            let _ = decode_metadata_entries(&input);
        });
        drive("decode_config_registry_data_0x07", &input, || {
            let _ = decode_config_registry_data_0x07(&input);
        });
        decoded += 1;
        let range_input = fuzz_input(&mut rng, &versions, 8 * 1024);
        let text = String::from_utf8_lossy(&range_input);
        drive("VersionRange::parse", &range_input, || {
            let _ = VersionRange::parse(&text);
        });
        if let Ok(text) = std::str::from_utf8(&range_input) {
            drive("VersionRange::parse::strict", &range_input, || {
                let _ = VersionRange::parse(text);
            });
        }
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("metadata::truncate", input, || {
                let _ = decode_metadata_entries(input);
                let _ = decode_config_registry_data_0x07(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x15, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("metadata::flip", &input, || {
                    let _ = decode_metadata_entries(&input);
                    let _ = decode_config_registry_data_0x07(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("metadata/registry/version: {decoded} inputs x ~3 parser calls");
}

// ---------------------------------------------------------------------------
// Lane M18-FUZZ-2: the `.class` subset VM (M18), the dual-loader TOML/JSON
// scanner + `ACC1` accessories (M16), and the unified declarative mod bundle
// (CONSOL-1) — the whole parse → verify → execute / scan → plan / decode
// pipelines under adversarial bytes.
// ---------------------------------------------------------------------------

/// Strict instruction budget every VM run in this lane is pinned under.
const FUZZ_VM_FUEL: u32 = 256;

/// Cap on parsed methods pushed through verify+execute per input — a
/// forged `methods_count` may claim up to `MAX_METHODS` entries.
const VM_METHODS_PER_INPUT: usize = 16;

/// A `SubsetVmHost` that answers every allowlisted call with the cheapest
/// correctly-typed value. The contract under test is the VM's fuel and
/// panic discipline, not host semantics.
struct FuzzVmHost;

impl SubsetVmHost for FuzzVmHost {
    fn call(&mut self, kind: HostCallKind, _args: &[VmValue]) -> Result<Option<VmValue>, VmError> {
        Ok(match kind.signature().result {
            Some(VmType::Int) => Some(VmValue::Int(0)),
            Some(VmType::Float) => Some(VmValue::Float(0.0)),
            Some(VmType::Ref) => Some(VmValue::Ref(0)),
            Some(VmType::Str) => Some(VmValue::Str(String::new())),
            None => None,
        })
    }
}

// JVMS §6 opcode fragments for the testkit corpus builders.
fn bc(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}
fn bc_ldc(index: u16) -> [u8; 2] {
    [0x12, index as u8]
}
fn bc_invokevirtual(index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [0xB6, hi, lo]
}
fn bc_invokestatic(index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [0xB8, hi, lo]
}
fn bc_branch(opcode: u8, delta: i16) -> [u8; 3] {
    let [hi, lo] = delta.to_be_bytes();
    [opcode, hi, lo]
}

/// `.class` corpus assembled by `bytecode_subset_vm::testkit`: real
/// `0xCAFEBABE` binaries spanning methods that verify *and* execute
/// (event-hook host calls, a bounded `iinc` loop, an infinite `goto 0`
/// spin that must burn exactly `fuel`), methods that only verify-reject
/// (forbidden `java/lang/Runtime`, unknown host classes), the
/// annotation-rich mixin seed, and hostile structural forgeries.
fn vm_class_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();

    // Damage-hook event method: host calls + fcmpl + ifle branch, fully
    // verified and executed (like a real mod's onDamage hook).
    let mut a = ClassAssembler::new("com/fuzz/DamageHook");
    let get_damage = a.cp.method_ref("mcspine/api/Event", "getDamage", "()F");
    let set_damage = a.cp.method_ref("mcspine/api/Event", "setDamage", "(F)V");
    let ten = a.cp.float(10.0);
    let half = a.cp.float(0.5);
    let hook = bc(&[
        &[0x2A], // aload_0
        &bc_invokevirtual(get_damage),
        &bc_ldc(ten),
        &[0x94],              // fcmpl
        &bc_branch(0x9E, 14), // ifle -> return
        &[0x2A],
        &[0x2A],
        &bc_invokevirtual(get_damage),
        &bc_ldc(half),
        &[0x6A], // fmul
        &bc_invokevirtual(set_damage),
        &[0xB1], // return
    ]);
    corpus.push(a.finish(&[MethodSpec::new(
        "onDamage",
        "(Lmcspine/api/Event;)V",
        4,
        1,
        hook,
    )]));

    // `goto 0` self-loop: verifies cleanly, must hit `OutOfFuel` after
    // exactly `fuel` instructions — the escape-proof seed.
    let a = ClassAssembler::new("com/fuzz/Spin");
    corpus.push(a.finish(&[MethodSpec::static_method(
        "spin",
        "()V",
        0,
        0,
        vec![0xA7, 0x00, 0x00], // goto +0 -> itself (infinite loop)
    )]));

    // Bounded iinc loop -> ireturn (verified + executed, returns 10).
    let a = ClassAssembler::new("com/fuzz/Sum");
    let sum = [
        vec![0x03],                   // iconst_0
        vec![0x3B],                   // istore_0
        vec![0x03],                   // iconst_0
        vec![0x3C],                   // istore_1
        vec![0x1A],                   // iload_0
        vec![0x08],                   // iconst_5
        bc_branch(0xA2, 13).to_vec(), // if_icmpge -> exit
        vec![0x1B],                   // iload_1
        vec![0x1A],                   // iload_0
        vec![0x60],                   // iadd
        vec![0x3C],                   // istore_1
        vec![0x84, 0x00, 0x01],       // iinc 0, 1
        bc_branch(0xA7, -12).to_vec(), // goto -> loop head
        vec![0x1B],                   // iload_1
        vec![0xAC],                   // ireturn
    ]
    .concat();
    corpus.push(a.finish(&[MethodSpec::static_method("sum", "()I", 2, 2, sum)]));

    // Static Math host call returning a float.
    let mut a = ClassAssembler::new("com/fuzz/Clamp");
    let clamp = a.cp.method_ref("mcspine/api/Math", "clamp", "(FFF)F");
    let clamped = bc(&[
        &[0x0B], // fconst_0
        &[0x0C], // fconst_1
        &[0x0D], // fconst_2
        &bc_invokestatic(clamp),
        &[0xAE], // freturn
    ]);
    corpus.push(a.finish(&[MethodSpec::static_method(
        "clamp", "()F", 3, 0, clamped,
    )]));

    // Hostile: forbidden java/lang/Runtime invocation — verify-reject.
    let mut a = ClassAssembler::new("com/fuzz/Pwn");
    let exec = a
        .cp
        .method_ref("java/lang/Runtime", "getRuntime", "()Ljava/lang/Runtime;");
    corpus.push(a.finish(&[MethodSpec::static_method(
        "run",
        "()V",
        1,
        0,
        bc(&[&bc_invokestatic(exec), &[0x57], &[0xB1]]),
    )]));

    // Hostile: unknown host class — verify-reject via UnknownHostCall.
    let mut a = ClassAssembler::new("com/fuzz/Unknown");
    let pwn = a.cp.method_ref("com/evil/Pwn", "run", "()V");
    corpus.push(a.finish(&[MethodSpec::static_method(
        "run",
        "()V",
        0,
        0,
        bc(&[&bc_invokestatic(pwn), &[0xB1]]),
    )]));

    // The annotation-rich mixin class seed plus a cp_count forgery —
    // different parser regions than the testkit emits.
    corpus.push(mixin_class_seed());
    let mut over = mixin_class_seed();
    over[8] = 0xff;
    over[9] = 0xff;
    corpus.push(over);
    // Magic + all-zero tables stub, and a wrong-magic stub.
    corpus.push(vec![
        0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 61, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]);
    corpus.push(b"\xde\xad\xbe\xefCAFEBABE".to_vec());
    corpus
}

/// The full subset-VM pipeline on one `.class` input: parse, then verify +
/// execute each declared method (bounded count) under `FUZZ_VM_FUEL` with
/// the dummy host. Any Ok result must respect the fuel contract.
fn drive_vm_pipeline(input: &[u8], args: &[VmValue]) {
    let Ok(parsed) = parse_class_methods(input) else {
        return;
    };
    for m in parsed.methods.iter().take(VM_METHODS_PER_INPUT) {
        let Ok(verified) = verify_subset_method(&parsed, &m.name, &m.descriptor) else {
            continue;
        };
        let mut host = FuzzVmHost;
        if let Ok(res) = execute_verified_method(&verified, args, &mut host, FUZZ_VM_FUEL) {
            assert!(
                res.instructions_executed <= FUZZ_VM_FUEL,
                "subset-VM escaped fuel: {} > {FUZZ_VM_FUEL}",
                res.instructions_executed,
            );
        }
    }
}

#[test]
fn bytecode_subset_vm_never_panics_or_escapes_fuel() {
    let mut rng = Rng::new(0xB17E_C0DE_0006);
    let corpus = vm_class_corpus();
    let iters = iters();
    let arg_sets: Vec<Vec<VmValue>> = vec![
        vec![],
        vec![VmValue::Ref(1)],
        vec![VmValue::Ref(1), VmValue::Int(-7)],
        vec![
            VmValue::Int(3),
            VmValue::Float(2.5),
            VmValue::Str("fuzz".to_string()),
        ],
    ];
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 256 * 1024);
        let args = rng.pick(&arg_sets).clone();
        drive("vm::pipeline", &input, || {
            drive_vm_pipeline(&input, &args);
        });
        decoded += 1;
    }
    // Deterministic sweeps: every truncation and first-64-byte flips
    // through the same parse → verify → execute pipeline.
    for seed in corpus.iter().filter(|s| s.len() <= 4096) {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("vm::truncate", input, || {
                drive_vm_pipeline(input, &[VmValue::Ref(1)]);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x07, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("vm::flip", &input, || {
                    drive_vm_pipeline(&input, &[VmValue::Ref(1), VmValue::Int(1)]);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("subset_vm: {decoded} inputs x parse+verify+exec pipeline");
}

// ---------------------------------------------------------------------------
// Dual-loader JAR scanner + fair-play properties + ACC1 accessories.
// ---------------------------------------------------------------------------

/// `key =` followed by `depth` nested `open`/`close` pairs — the TOML
/// value-nesting shape that overflowed the stack pre-`MAX_TOML_NESTING_DEPTH`.
fn toml_deep_value(key: &str, open: u8, close: u8, depth: usize) -> Vec<u8> {
    let mut v = format!("{key} = ").into_bytes();
    v.extend(std::iter::repeat_n(open, depth));
    v.extend_from_slice(b"1");
    v.extend(std::iter::repeat_n(close, depth));
    v.push(b'\n');
    v
}

/// `[a.b.c.…]` table header (or `[[…]]` array-of-tables header) with
/// `segments` dotted components — iterative descent, deep `TomlValue` tree.
fn toml_dotted_header(segments: usize, array: bool) -> Vec<u8> {
    let path: Vec<&str> = std::iter::repeat_n("a", segments).collect();
    let mut v = Vec::new();
    v.extend_from_slice(if array { b"[[" } else { b"[" });
    v.extend_from_slice(path.join(".").as_bytes());
    v.extend_from_slice(if array { b"]]" } else { b"]" });
    v.extend_from_slice(b"\nk = 1\n");
    v
}

/// `a.b.c.… = 1` dotted key — builds one `TomlValue::Table` chain
/// `segments` deep inside the root map.
fn toml_dotted_key(segments: usize) -> Vec<u8> {
    let path: Vec<&str> = std::iter::repeat_n("a", segments).collect();
    let mut v = path.join(".").into_bytes();
    v.extend_from_slice(b" = 1\n");
    v
}

/// JAR corpus for `scan_mod_jar`/`plan_dual_loader_mods_dir`: real
/// Fabric, NeoForge, legacy-Forge, and dual-manifest archives, datapack
/// evidence, and NeoForge manifests carrying TOML nested far past the
/// 32-level value cap (`[[[[` arrays, `{{{{` inline tables, deep dotted
/// headers and dotted keys).
fn dual_loader_corpus() -> Vec<Vec<u8>> {
    let fabric_manifest: &[u8] = br#"{
        "schemaVersion": 1,
        "id": "fabricfuzz",
        "version": "1.2.3",
        "environment": "*",
        "entrypoints": {"main": ["com.fuzz.FabricInit"]},
        "depends": {"fabricloader": ">=0.15.0", "minecraft": "~1.21"},
        "mixins": ["fabricfuzz.mixins.json"]
    }"#;
    let fabric_client: &[u8] =
        br#"{"schemaVersion":1,"id":"clientonly","version":"0.9","environment":"client"}"#;
    let mixin_cfg: &[u8] =
        br#"{"required":true,"package":"com.fuzz.mixin","mixins":["FuzzMixin"]}"#;
    let neoforge_toml: &[u8] = br#"modLoader="javafml"
loaderVersion="[4.0.39]"
license="MIT"

[[mods]]
modId="neofuzz"
version="1.0.0"
displayName="Neo Fuzz"

[[dependencies.neofuzz]]
modId="minecraft"
type="required"
versionRange="[1.21.1,1.22)"
side="BOTH"

[[dependencies.neofuzz]]
modId="neoforge"
type="required"
versionRange="[21.1.172,)"
side="BOTH"

[[mixins]]
config="neofuzz.mixins.json"
"#;
    let mut corpus = vec![
        zip_stored(&[
            ("fabric.mod.json", fabric_manifest),
            ("fabricfuzz.mixins.json", mixin_cfg),
            ("com/fuzz/FabricInit.class", b"\xca\xfe\xba\xbe"),
        ]),
        zip_stored(&[
            ("META-INF/neoforge.mods.toml", neoforge_toml),
            ("neofuzz.mixins.json", mixin_cfg),
            ("com/fuzz/NeoInit.class", b"\xca\xfe\xba\xbe"),
        ]),
        zip_stored(&[("META-INF/mods.toml", neoforge_toml)]),
        zip_stored(&[("fabric.mod.json", fabric_client)]),
        zip_stored(&[
            ("fabric.mod.json", fabric_manifest),
            ("META-INF/neoforge.mods.toml", neoforge_toml),
        ]),
        zip_stored(&[
            ("pack.mcmeta", br#"{"pack":{"pack_format":48}}"#),
            ("data/fuzz/recipes/x.json", b"{}"),
        ]),
    ];
    // NeoForge manifests whose values nest far past MAX_TOML_NESTING_DEPTH
    // (32): the exact `[[[[` / `{{{{` shapes that once overflowed the
    // parser's recursion, at depths both sides of the cap.
    for depth in [8, 31, 33, 64, 96] {
        let mut toml =
            b"modLoader=\"javafml\"\n[[mods]]\nmodId=\"deep\"\n".to_vec();
        toml.extend(toml_deep_value("arr", b'[', b']', depth));
        corpus.push(zip_stored(&[("META-INF/neoforge.mods.toml", &toml)]));
        let mut toml =
            b"modLoader=\"javafml\"\n[[mods]]\nmodId=\"deep\"\n".to_vec();
        toml.extend(toml_deep_value("tbl", b'{', b'}', depth));
        corpus.push(zip_stored(&[("META-INF/neoforge.mods.toml", &toml)]));
    }
    // Deep dotted paths: array-of-tables headers, plain headers, and a
    // long dotted key — `table_at_path`/`insert_dotted` territory.
    for segs in [16, 64] {
        corpus.push(zip_stored(&[(
            "META-INF/neoforge.mods.toml",
            &toml_dotted_header(segs, true),
        )]));
        corpus.push(zip_stored(&[(
            "META-INF/neoforge.mods.toml",
            &toml_dotted_header(segs, false),
        )]));
        corpus.push(zip_stored(&[(
            "META-INF/neoforge.mods.toml",
            &toml_dotted_key(segs * 8),
        )]));
    }
    corpus.push(b"PK\x03\x04".to_vec());
    corpus.push(Vec::new());
    corpus
}

/// `server.properties`/fair-play config text corpus.
fn properties_corpus() -> Vec<Vec<u8>> {
    [
        &b"# mc-spine fair-play\nmotd=fuzz\nview-distance: 10\nfair-play = true\nallow-cave-map = false\nallow-entity-radar = false\nallow-waypoint-teleport = true\nworld-level-id = 13371337\n"[..],
        b"[mc_spine]\nfair_play = true\nworld_level_id = -42\n",
        b"fair-play:true\nallow-cave-map:true\nxaero.allow-entity-radar = off\n",
        b"just-a-lonely-line\n",
        b"key=value\na:b:c\nworld_level_id = 0\n",
        b"= \n: \nk = \"quoted\" \nk2 = 'literal'\n",
        b"",
    ]
    .iter()
    .map(|s| s.to_vec())
    .collect()
}

/// `ACC1` accessory-inventory frames: the standard inventory, one with
/// equipped items + attribute modifiers, and hostile stub frames.
fn acc1_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    corpus.push(AccessoryInventory::standard().to_bytes());
    let mut inv = AccessoryInventory::standard();
    let ring = AccessoryItem::new("aether:iron_ring", ["ring"]).with_modifiers(vec![
        AttributeModifier {
            attribute: "minecraft:generic.armor".to_string(),
            operation: ModifierOperation::AddValue,
            amount: 2.0,
        },
        AttributeModifier {
            attribute: "minecraft:generic.luck".to_string(),
            operation: ModifierOperation::AddMultipliedTotal,
            amount: 0.25,
        },
    ]);
    let _ = inv.equip("ring", 0, ring);
    let _ = inv.equip("charm", 0, AccessoryItem::new("artifacts:charm", ["charm"]));
    corpus.push(inv.to_bytes());
    // Truncated / wrong-magic / corrupt-crc stubs.
    corpus.push(b"ACC1".to_vec());
    corpus.push(b"ACC1\xff\xff\xff\xff\x00\x00".to_vec());
    corpus.push(b"ACC2\x00\x00\x00\x00\x00\x00".to_vec());
    corpus
}

#[test]
fn dual_loader_and_accessories_never_panic() {
    let mut rng = Rng::new(0xD1A1_10AD_0007);
    let jars = dual_loader_corpus();
    let props = properties_corpus();
    let acc1 = acc1_corpus();
    let iters = iters();
    let names = ["fuzz-a.jar", "fuzz-b.jar", "mod.jar"];
    let mut decoded = 0usize;
    for _ in 0..iters {
        let jar_a = fuzz_input(&mut rng, &jars, 4 * 1024 * 1024);
        let jar_b = fuzz_input(&mut rng, &jars, 4 * 1024 * 1024);
        drive("scan_mod_jar", &jar_a, || {
            let _ = scan_mod_jar(&jar_a);
        });
        drive("scan_mod_jar_named", &jar_a, || {
            let _ = scan_mod_jar_named(&jar_a, "fuzz-mod.jar");
        });
        let name_a = *rng.pick(&names);
        let name_b = *rng.pick(&names);
        drive("plan_dual_loader_mods_dir", &jar_a, || {
            let _ = plan_dual_loader_mods_dir([
                (name_a, jar_a.as_slice()),
                (name_b, jar_b.as_slice()),
                ("neofuzz-1.0.0.jar", jars[1].as_slice()),
            ]);
        });
        decoded += 1;
        let props_input = fuzz_input(&mut rng, &props, 64 * 1024);
        let text = String::from_utf8_lossy(&props_input);
        drive("MapFairPlayConfig::from_properties_text", &props_input, || {
            let _ = MapFairPlayConfig::from_properties_text(&text);
        });
        decoded += 1;
        let acc_input = fuzz_input(&mut rng, &acc1, 64 * 1024);
        drive("AccessoryInventory::from_bytes", &acc_input, || {
            if let Ok(inv) = AccessoryInventory::from_bytes(&acc_input) {
                // Round-trip a decoded inventory back through the codec.
                let _ = AccessoryInventory::from_bytes(&inv.to_bytes());
            }
        });
        decoded += 1;
    }
    // Deterministic sweeps over the small seeds: truncations and
    // first-64-byte flips through all four surfaces.
    for seed in jars
        .iter()
        .chain(acc1.iter())
        .chain(props.iter())
        .filter(|s| s.len() <= 4096)
    {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("dual::truncate", input, || {
                let _ = scan_mod_jar(input);
                let _ = AccessoryInventory::from_bytes(input);
                let text = String::from_utf8_lossy(input);
                let _ = MapFairPlayConfig::from_properties_text(&text);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x5b, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("dual::flip", &input, || {
                    let _ = scan_mod_jar(&input);
                    let _ = AccessoryInventory::from_bytes(&input);
                    let text = String::from_utf8_lossy(&input);
                    let _ = MapFairPlayConfig::from_properties_text(&text);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("dual_loader+acc1: {decoded} inputs x ~5 surface calls");
}

// ---------------------------------------------------------------------------
// Unified declarative mod bundle (`mc-spine-mod-bundle/1`).
// ---------------------------------------------------------------------------

/// Bundle-JSON corpus: every `export_builtin_dimension_bundles()` document
/// (real `mc-spine-mod-bundle/1` wire form), a minimal valid bundle,
/// near-misses, and deep-nesting shapes past the 64-level JSON cap.
fn bundle_corpus() -> Vec<Vec<u8>> {
    let mut corpus: Vec<Vec<u8>> = export_builtin_dimension_bundles()
        .iter()
        .map(|b| b.to_json().into_bytes())
        .collect();
    corpus.push(
        br#"{"schema":"mc-spine-mod-bundle/1","mod_id":"fuzzmod","mc_version_range":"[1.21,)","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":[]}"#
            .to_vec(),
    );
    corpus.push(
        br#"{"schema":"mc-spine-mod-bundle/1","mod_id":"fuzzmod","mc_version_range":"[1.21,)"}"#
            .to_vec(),
    );
    corpus.push(br#"{"schema":"mc-spine-mod-bundle/2"}"#.to_vec());
    corpus.push(b"{\"portals\":[{\"name\":\"x\"}]}".to_vec());
    // Deep nests far past MAX_JSON_DEPTH (64): object and array forms.
    let depth = 192;
    let mut deep_obj = Vec::new();
    for _ in 0..depth {
        deep_obj.extend_from_slice(b"{\"a\":");
    }
    deep_obj.push(b'1');
    deep_obj.extend(std::iter::repeat_n(b'}', depth));
    corpus.push(deep_obj);
    let mut deep_arr = Vec::new();
    deep_arr.extend(std::iter::repeat_n(b'[', depth));
    deep_arr.extend(std::iter::repeat_n(b']', depth));
    corpus.push(deep_arr);
    // Nesting inside a real bundle document (deep "portals" payload).
    let mut nested = br#"{"schema":"mc-spine-mod-bundle/1","mod_id":"fuzzmod","mc_version_range":"[1.21,)","portals":"#.to_vec();
    nested.extend(std::iter::repeat_n(b'[', depth));
    nested.extend(std::iter::repeat_n(b']', depth));
    nested.extend_from_slice(
        b",\"progression_barriers\":[],\"biome_regions\":[],\"creatures\":[],\"custom_channels\":[]}",
    );
    corpus.push(nested);
    corpus.push(Vec::new());
    corpus
}

#[test]
fn unified_mod_bundle_json_never_panics() {
    let mut rng = Rng::new(0xB1BD_1E00_0008);
    let corpus = bundle_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 4 * 1024 * 1024);
        let text = String::from_utf8_lossy(&input);
        drive("DeclarativeModBundle::from_json", &input, || {
            if let Ok(bundle) = DeclarativeModBundle::from_json(&text) {
                // A parsed bundle must re-validate and round-trip cleanly.
                let _ = bundle.validate();
                let rendered = bundle.to_json();
                let _ = DeclarativeModBundle::from_json(&rendered);
            }
        });
        if let Ok(text) = std::str::from_utf8(&input) {
            drive("DeclarativeModBundle::from_json::strict", &input, || {
                let _ = DeclarativeModBundle::from_json(text);
            });
        }
        decoded += 1;
    }
    for seed in corpus.iter().filter(|s| s.len() <= 4096) {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("bundle::truncate", input, || {
                let text = String::from_utf8_lossy(input);
                let _ = DeclarativeModBundle::from_json(&text);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x22, 0x7b, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("bundle::flip", &input, || {
                    let text = String::from_utf8_lossy(&input);
                    let _ = DeclarativeModBundle::from_json(&text);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("unified_bundle: {decoded} inputs x ~2 from_json calls");
}
