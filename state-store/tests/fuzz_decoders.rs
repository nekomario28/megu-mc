// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane P6b (#222): deterministic adversarial fuzz harness for every
//! untrusted persistence/storage decoder in `mc-spine-state-store`.
//!
//! Everything a hostile disk image can carry — `STV1` snapshots, `level.dat`
//! JSON/NBT documents (gzip-wrapped or raw), `r.*.*.mca` region headers and
//! `AUD1` audit-WAL frames — is hammered with corpus-guided mutations,
//! truncation sweeps, bit flips, deep-nesting bombs, forged length prefixes
//! and gzip bombs. The contract under test: **zero panics and bounded
//! memory** on every byte string, valid or hostile.
//!
//! Determinism: all inputs derive from a fixed-seed `splitmix64` PRNG — no
//! `SystemTime`, no OS entropy, no external crates. `MC_SPINE_FUZZ_ITERS`
//! scales the per-target iteration count (default `4_000`; operators crank
//! `MC_SPINE_FUZZ_ITERS=100000` for a deep soak). A decoder panic is caught,
//! the offending input is hex-dumped for reproduction, and the panic is
//! re-thrown so the test still fails.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

use mc_spine_core::version::TargetVersion;
use mc_spine_state_store::{
    decode_nbt_to_data_value, decode_snapshot, encode_data_value_to_nbt, encode_snapshot,
    gzip_decompress, gzip_encode_stored, inspect_level_dat, inspect_mca_header,
    migrate_json_document, parse_data_json, to_data_json, AuditAction, AuditLog, AuditRecord,
    DataValue, ServerContext, MCA_HEADER_BYTES, MCA_SECTOR_BYTES,
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

/// Per-target iteration count: `MC_SPINE_FUZZ_ITERS` overrides, default 4000.
fn iters() -> usize {
    std::env::var("MC_SPINE_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000)
}

// ---------------------------------------------------------------------------
// Panic witness: a decoder panic is caught so the exact input can be dumped
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

/// Attacker-crafted token strings worth splicing anywhere.
const DICTIONARY: &[&[u8]] = &[
    b"STV1",
    b"AUD1",
    b"\x1f\x8b\x08",                         // gzip magic
    b"\xff\xff\xff\xff",                     // u32be::MAX length bomb
    b"\xff\xff\xff\x7f",                     // u32be 0x7FFFFFFF
    b"\x00\x00\x00\x80",                     // i32be::MIN
    b"\x0a\x00\x00\x0a\x00\x00\x0a\x00\x00", // nested NBT compounds
    b"\x09\x0a\xff\xff\xff\xff",             // NBT list + huge count
    b"\x07\x01\x00\x00\x00",                 // NBT byte-array, 16M elements
    b"{\"DataVersion\":",
    b"[[[[[[[[",
    b"\\uD800\\uDC00",
    b"\\uD800",
];

/// Mutate `seed` with `n` primitive mutations.
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

/// Pathological standalone shapes: deep nests, surrogate escapes, gzip bombs.
fn bomb(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    match rng.below(10) {
        0 => {
            // JSON array nesting far past the 64-level cap.
            let depth = 1 + rng.below(300);
            out.extend(std::iter::repeat_n(b'[', depth));
            out.extend(std::iter::repeat_n(b']', depth));
        }
        1 => {
            // Nested JSON objects past the cap.
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
                b"\"\\uD800x\"",
                b"\"\\uXYZW\"",
                b"\"\\u\"",
                b"\"\\\"",
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
                b"-1e-9999",
                b"0x10",
                b"1.",
                b".5",
                b"01",
                b"1e",
                b"--5",
            ] {
                out.extend_from_slice(n);
                out.push(b',');
            }
        }
        4 => {
            // NBT compounds nested past the 64 cap.
            let depth = 1 + rng.below(200);
            for _ in 0..depth {
                out.extend_from_slice(&[0x0a, 0x00, 0x00]);
            }
            out.push(0x00);
        }
        5 => {
            // NBT byte-array claiming 16 MiB elements.
            out.extend_from_slice(&[0x07, 0x00, 0x00]);
            out.extend_from_slice(&((1 << 24) as u32).to_be_bytes());
            let n = rng.below(32);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        6 => {
            // gzip member: magic + a stored block that lies about its size.
            out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]);
            out.push(0x01);
            out.extend_from_slice(&u16::MAX.to_le_bytes()); // LEN
            out.extend_from_slice(&(!0xffffu16).to_le_bytes()); // NLEN
            out.extend(std::iter::repeat_with(|| rng.byte()).take(64));
            out.extend_from_slice(&u32::MAX.to_le_bytes()); // CRC junk
            out.extend_from_slice(&u32::MAX.to_le_bytes()); // ISIZE junk
        }
        7 => {
            // MCA location entries pointing at absurd sector offsets.
            out.resize(MCA_HEADER_BYTES, 0);
            for slot in 0..8 {
                out[slot * 4] = 0xff;
                out[slot * 4 + 1] = 0xff;
                out[slot * 4 + 2] = 0xff;
                out[slot * 4 + 3] = 0xff;
            }
        }
        8 => {
            // AUD1-ish frame with a 4 MiB payload claim and no payload.
            out.extend_from_slice(b"AUD1");
            out.extend_from_slice(&u32::MAX.to_be_bytes());
            let n = rng.below(64);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
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
// Corpus seeds — real encodings produced by the crate's own encoders.
// ---------------------------------------------------------------------------

/// A `level.dat`-shaped [`DataValue`]: `Data` compound with `DataVersion`,
/// `LevelName`, `Version.Name`, a seed and datapack lists — the exact shape
/// `inspect_level_dat` and `migrate_json_document` look into.
fn level_dat_value(data_version: i64) -> DataValue {
    let mut version = BTreeMap::new();
    version.insert("Name".to_string(), DataValue::Str("1.21.1".into()));
    version.insert("Id".to_string(), DataValue::Int(data_version));
    let mut packs = BTreeMap::new();
    packs.insert(
        "Enabled".to_string(),
        DataValue::List(vec![DataValue::Str("vanilla".into())]),
    );
    packs.insert("Disabled".to_string(), DataValue::List(vec![]));
    let mut data = BTreeMap::new();
    data.insert("DataVersion".to_string(), DataValue::Int(data_version));
    data.insert("LevelName".to_string(), DataValue::Str("fuzz".into()));
    data.insert("Version".to_string(), DataValue::Map(version));
    data.insert("RandomSeed".to_string(), DataValue::Int(0x5eed));
    data.insert("DataPacks".to_string(), DataValue::Map(packs));
    let mut root = BTreeMap::new();
    root.insert("DataVersion".to_string(), DataValue::Int(data_version));
    root.insert("Data".to_string(), DataValue::Map(data));
    DataValue::Map(root)
}

/// Real `STV1` snapshots for both targets, one from an empty context and one
/// carrying a dimension.
fn snapshot_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    for target in [TargetVersion::V1_21_1, TargetVersion::V26_3] {
        corpus.push(encode_snapshot(&ServerContext::new(), target));
        let ctx = ServerContext::new();
        ctx.ensure_dimension("minecraft:overworld");
        corpus.push(encode_snapshot(&ctx, target));
    }
    corpus
}

/// Real JSON documents: valid `level.dat`-ish metadata plus hostile shapes.
fn json_corpus() -> Vec<Vec<u8>> {
    let mut corpus = vec![
        to_data_json(&level_dat_value(3955)).into_bytes(),
        to_data_json(&level_dat_value(5023)).into_bytes(),
        b"{\"DataVersion\":3955}".to_vec(),
        b"[1,2,3]".to_vec(),
        b"\"text\"".to_vec(),
        b"null".to_vec(),
        b"{}".to_vec(),
        b"{\"a\":[[[{\"b\":null}]]],\"c\":\"\\uD83D\\uDE00\"}".to_vec(),
        "{\"DataVersion\":3955,\"Data\":{\"LevelName\":\"w\"}}".as_bytes().to_vec(),
        bomb(&mut Rng::new(0xB0BB_0001)),
        bomb(&mut Rng::new(0xB0BB_0002)),
        bomb(&mut Rng::new(0xB0BB_0003)),
    ];
    // A document with real migration meat: old-id block states and items.
    corpus.push(
        b"{\"DataVersion\":3955,\"Data\":{\"block\":\"minecraft:grass_block\",\
          \"item\":\"minecraft:grass_block\",\"pack_format\":48}}"
            .to_vec(),
    );
    corpus
}

/// Real NBT documents (raw) and their gzip-wrapped forms.
fn nbt_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    for dv in [3955i64, 5023] {
        if let Ok(nbt) = encode_data_value_to_nbt("", &level_dat_value(dv)) {
            corpus.push(gzip_encode_stored(&nbt));
            corpus.push(nbt);
        }
    }
    // A tiny valid compound and scalar roots.
    for v in [
        DataValue::Map(BTreeMap::new()),
        DataValue::List(vec![DataValue::Int(1), DataValue::Int(2)]),
        DataValue::Str("x".into()),
        DataValue::Int(-1),
        DataValue::Float(f64::NAN),
    ] {
        if let Ok(nbt) = encode_data_value_to_nbt("root", &v) {
            corpus.push(nbt);
        }
    }
    corpus
}

/// A valid `.mca` file image: header plus one uncompressed chunk record in
/// sector 2 — the exact shape `inspect_mca_header` accepts.
fn mca_corpus() -> Vec<Vec<u8>> {
    let mut file = vec![0u8; MCA_HEADER_BYTES + MCA_SECTOR_BYTES];
    // Slot 0 -> sector offset 2, 1 sector; slot 7 -> sector 3 (out of file —
    // a second, invalid slot exercises that branch too when mutated back).
    file[0] = 0;
    file[1] = 0;
    file[2] = 2;
    file[3] = 1;
    // Timestamp table slot 0.
    file[MCA_SECTOR_BYTES + 3] = 0x42;
    // Chunk record at sector 2: u32 declared_len (1 compression + 5 payload),
    // compression byte 3 (uncompressed), payload.
    let at = MCA_HEADER_BYTES;
    file[at..at + 4].copy_from_slice(&6u32.to_be_bytes());
    file[at + 4] = 3;
    file[at + 5..at + 9].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    vec![file]
}

/// Real `AUD1` WAL bytes: several valid frames concatenated, plus a torn tail.
fn audit_corpus() -> Vec<Vec<u8>> {
    let mut wal = Vec::new();
    let records = [
        AuditRecord::new(
            100,
            "Steve",
            "minecraft:overworld",
            [3, 64, -7],
            AuditAction::BlockPlace,
            0,
            8598,
        ),
        AuditRecord::new(
            101,
            "#tnt",
            "minecraft:the_nether",
            [-40, 30, 99],
            AuditAction::Explosion,
            1200,
            0,
        ),
        {
            let mut r = AuditRecord::new(
                102,
                "Alex",
                "minecraft:overworld",
                [8, 70, 8],
                AuditAction::ContainerInsert,
                0,
                0,
            );
            r.item_id = Some("minecraft:tnt".into());
            r.item_count = 12;
            r.rolled_back = true;
            r
        },
    ];
    for record in &records {
        wal.extend_from_slice(&AuditLog::encode_frame(record));
    }
    let mut corpus = vec![wal.clone()];
    // Torn tail: a valid WAL plus a half-written frame.
    let mut torn = wal;
    torn.extend_from_slice(&AuditLog::encode_frame(&records[0])[..9]);
    corpus.push(torn);
    corpus
}

// ---------------------------------------------------------------------------
// STV1 snapshot decoder.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_decoder_never_panics() {
    let mut rng = Rng::new(0x5756_1000_0001);
    let corpus = snapshot_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 4 * 1024 * 1024);
        drive("decode_snapshot", &input, || {
            let _ = decode_snapshot(&input);
        });
        decoded += 1;
    }
    // Deterministic sweep: every truncation + first-byte flips on real
    // snapshots.
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("decode_snapshot::truncate", input, || {
                let _ = decode_snapshot(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x7f, 0x80, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("decode_snapshot::flip", &input, || {
                    let _ = decode_snapshot(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("snapshot: {decoded} inputs");
}

// ---------------------------------------------------------------------------
// JSON front end + migration engine.
// ---------------------------------------------------------------------------

#[test]
fn json_parsers_and_migrator_never_panic() {
    let mut rng = Rng::new(0x550A_0000_0001);
    let corpus = json_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 256 * 1024);
        let text = String::from_utf8_lossy(&input);
        drive("parse_data_json", &input, || {
            let _ = parse_data_json(&text);
        });
        drive("migrate_json_document", &input, || {
            for (to, allow_downgrade) in [
                (TargetVersion::V26_3, false),
                (TargetVersion::V26_3, true),
                (TargetVersion::V1_21_1, false),
                (TargetVersion::V1_21_1, true),
            ] {
                let _ = migrate_json_document(&text, to, allow_downgrade);
            }
        });
        decoded += 1;
    }
    eprintln!("json+migrate: {decoded} inputs x 5 parser runs");
}

// ---------------------------------------------------------------------------
// NBT / gzip / level.dat / region-header decoders.
// ---------------------------------------------------------------------------

#[test]
fn nbt_gzip_and_anvil_decoders_never_panic() {
    let mut rng = Rng::new(0xAB71_0000_0001);
    let corpus = nbt_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 2 * 1024 * 1024);
        drive("decode_nbt_to_data_value", &input, || {
            let _ = decode_nbt_to_data_value(&input);
        });
        drive("inspect_level_dat", &input, || {
            let _ = inspect_level_dat(&input);
        });
        drive("gzip_decompress", &input, || {
            for budget in [0usize, 64, 1 << 20, 32 << 20] {
                let _ = gzip_decompress(&input, budget);
            }
        });
        decoded += 1;
    }
    eprintln!("nbt/gzip/level_dat: {decoded} inputs x 6 decoder calls");
}

#[test]
fn mca_header_decoder_never_panics() {
    let mut rng = Rng::new(0xAC4A_0000_0001);
    let corpus = mca_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 3 * MCA_SECTOR_BYTES);
        drive("inspect_mca_header", &input, || {
            let _ = inspect_mca_header(&input);
        });
        decoded += 1;
    }
    // Sweep: every truncation of the one valid file + every header byte
    // forced to edge values.
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("inspect_mca_header::truncate", input, || {
                let _ = inspect_mca_header(input);
            });
            decoded += 1;
        }
        for i in 0..MCA_HEADER_BYTES.min(256) {
            for v in [0x00u8, 0x02, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("inspect_mca_header::flip", &input, || {
                    let _ = inspect_mca_header(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("mca_header: {decoded} inputs");
}

// ---------------------------------------------------------------------------
// AUD1 audit WAL recovery (frame decoder + payload decoder).
// ---------------------------------------------------------------------------

#[test]
fn audit_log_recover_never_panics() {
    let mut rng = Rng::new(0xADD1_7000_0001);
    let corpus = audit_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 1024 * 1024);
        drive("AuditLog::recover", &input, || {
            let (log, tail) = AuditLog::recover(&input);
            // Bounded-memory invariant: a WAL can never recover more records
            // than could fit at one byte each.
            assert!(
                log.len() <= input.len(),
                "recover produced {} records from {} bytes",
                log.len(),
                input.len()
            );
            assert!(tail <= input.len());
        });
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("AuditLog::recover::truncate", input, || {
                let _ = AuditLog::recover(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x7f, 0x80, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("AuditLog::recover::flip", &input, || {
                    let _ = AuditLog::recover(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("audit_wal: {decoded} inputs");
}
