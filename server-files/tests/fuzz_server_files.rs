// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane P6e: deterministic adversarial fuzz harness for every untrusted
//! config/wire/cache decoder in `megu-server-files`.
//!
//! Everything a hostile server directory, disk image, or connected client
//! can carry — `server.properties` escapes, the shared JSON core behind
//! `ops.json`/`whitelist.json`/`banned-players.json`/`banned-ips.json`/
//! `usercache.json`, the OpenPAC `openpac_claims.json`/`openpac_parties.json`
//! documents, `claims_sync` wire batches and custom-payload frames, and the
//! `SBC1` startup-binary-cache envelope — is hammered with corpus-guided
//! mutations, raw noise, truncation sweeps, bit flips, deep-nesting bombs
//! and forged length prefixes. The contract under test: **zero panics and
//! bounded memory** on every byte string, valid or hostile.
//!
//! Determinism: all inputs derive from fixed-seed `splitmix64` PRNGs — no
//! `SystemTime`, no OS entropy, no external crates. `MEGU_FUZZ_ITERS`
//! scales the per-target iteration count (default `3_000`; operators crank
//! `MEGU_FUZZ_ITERS=100000` for a deep soak). A parser panic is caught,
//! the offending input is hex-dumped for reproduction, and the panic is
//! re-thrown so the test still fails.

use std::path::PathBuf;

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

use megu_server_files::claims::{
    ClaimChunkKey, ClaimsWorld, PartyRank, SubClaimId, CLAIMS_FILE, PARTIES_FILE,
};
use megu_server_files::claims_wire::{
    decode_claims_batch, decode_custom_payload_frame, encode_claims_batch,
    encode_custom_payload_frame, ClaimsWireBatch, ClaimsWireCell, ClaimsWireEntry,
    WireOwnerKind, CHANNEL_FTBCHUNKS_CLAIMS_SYNC, CHANNEL_MEGU_CLAIMS_V1,
    CHANNEL_OPENPAC_CLAIMS_SYNC,
};
use megu_server_files::eula::{check_eula, EULA_FILE};
use megu_server_files::json;
use megu_server_files::lists::{
    normalize_ban_ip, offline_uuid, valid_player_name, BannedIps, BannedPlayers, OpsList,
    Whitelist,
};
use megu_server_files::perf_cache::{
    CacheLookup, StartupBinaryCache, StartupInputFile, CACHE_SCHEMA_VERSION, STARTUP_CACHE_MAGIC,
};
use megu_server_files::properties::{PropertiesFile, ServerProperties};
use megu_server_files::usercache::UserCache;

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

/// Per-target iteration count: `MEGU_FUZZ_ITERS` overrides, default 3000.
fn iters() -> usize {
    std::env::var("MEGU_FUZZ_ITERS")
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
    b"SBC1",
    b"eula=true",
    b"eula=false",
    b"server.properties",
    b"openpac_claims.json",
    b"openpac_parties.json",
    b"openpartiesandclaims:claims_sync",
    b"ftbchunks:claims_sync",
    b"megu:claims_v1",
    b"ops.json",
    b"whitelist.json",
    b"banned-players.json",
    b"banned-ips.json",
    b"usercache.json",
    b"\"uuid\":\"00000000-0000-0000-0000-000000000001\"",
    b"[{\"uuid\":",
    b"\"claims\":[",
    b"\"parties\":[",
    b"player:",
    b"expired:",
    b"server",
    b"party-1",
    b"main",
    b"minecraft:overworld",
    b"=value",
    b"\\u0000",
    b"\\uD800",
    b"key=value\\\n",
    b"\xff\xff\xff\xff",
    b"\xff\xff\xff\x7f",
    b"[[[[[[[[",
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

/// Pathological standalone shapes: deep nests, surrogate escapes, length
/// bombs, escape-sequence mines.
fn bomb(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    match rng.below(10) {
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
            // SBC1-ish envelope: magic + huge schema/len claims, no CRC.
            out.extend_from_slice(&STARTUP_CACHE_MAGIC);
            out.extend_from_slice(&u32::MAX.to_be_bytes());
            out.extend_from_slice(&[0xab; 32]);
            out.extend_from_slice(&u32::MAX.to_be_bytes());
            let n = rng.below(64);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        5 => {
            // VarInt run that never terminates plus a giant string length.
            out.extend(std::iter::repeat_n(0x80, 4 + rng.below(8)));
            out.extend_from_slice(&u32::MAX.to_be_bytes());
        }
        6 => {
            // .properties escape mines: broken \uXXXX, dangling backslash,
            // line-continuation storms.
            for esc in [
                &b"k=\\u"[..],
                b"k=\\u1",
                b"k=\\u12",
                b"k=\\u123",
                b"k=\\uWXYZ",
                b"k=\\x",
                b"k=\\",
                b"k\\\\\\\n",
            ] {
                out.extend_from_slice(esc);
                out.push(b'\n');
            }
            let n = 1 + rng.below(64);
            for _ in 0..n {
                out.extend_from_slice(b"\\\n");
            }
        }
        7 => {
            // claims_sync batch: protocol tag + count claim that overruns.
            out.push(1); // WIRE_PROTOCOL_TAG
            out.extend_from_slice(&u64::MAX.to_be_bytes());
            out.extend_from_slice(&u64::MAX.to_be_bytes());
            out.extend_from_slice(&u16::MAX.to_be_bytes()); // dim len
            let n = rng.below(48);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        8 => {
            // A uuid-shaped string with hostile tails.
            out.extend_from_slice(b"\"00000000-0000-0000-0000-");
            let n = 1 + rng.below(64);
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
// Temp-file plumbing for the path-based loaders. One tag-scoped scratch dir
// per test keeps writes bounded and leaves nothing behind.
// ---------------------------------------------------------------------------

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-fuzz-p6e-sf-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn write_file(path: &PathBuf, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

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

// ---------------------------------------------------------------------------
// Corpus seeds.
// ---------------------------------------------------------------------------

/// `server.properties`-shaped documents: the full key space plus the escape
/// edge cases the parser is contracted to reject.
fn properties_corpus() -> Vec<Vec<u8>> {
    let real = b"#Minecraft server properties\n\
        server-port=25565\n\
        motd=A Minecraft Server\n\
        max-players=20\n\
        online-mode=true\n\
        white-list=false\n\
        enforce-whitelist=false\n\
        view-distance=10\n\
        simulation-distance=10\n\
        level-seed=\n\
        level-name=world\n\
        difficulty=easy\n\
        gamemode=survival\n\
        pvp=true\n\
        enable-rcon=false\n\
        rcon.port=25575\n\
        rcon.password=\n\
        network-compression-threshold=256\n";
    vec![
        real.to_vec(),
        b"motd=\\u0041\\u0042\nkey\\=name=va\\:lue\n".to_vec(),
        b"motd=line1\\\nline2\n".to_vec(),
        b"!comment\n#comment\n spaced key = spaced value \nempty=\n".to_vec(),
        b"bad=\\u\nbad2=\\u1\nbad3=\\u12\nbad4=\\u123\n".to_vec(),
        b"a=b\rc=d\r\nf=g\n".to_vec(),
        b"eula=true\n".to_vec(),
        b"eula=false\n".to_vec(),
        b"eula=maybe\n".to_vec(),
        Vec::new(),
    ]
}

/// JSON list documents for ops/whitelist/bans/usercache plus wrong-shape
/// roots that must fail closed.
fn list_json_corpus() -> Vec<Vec<u8>> {
    let u = "00000000-0000-0000-0000-000000000001";
    vec![
        format!(
            "[{{\"uuid\":\"{u}\",\"name\":\"Steve\",\"level\":4,\"bypassesPlayerLimit\":false}}]"
        )
        .into_bytes(),
        format!("[{{\"uuid\":\"{u}\",\"name\":\"Alex\"}}]").into_bytes(),
        format!(
            "[{{\"uuid\":\"{u}\",\"name\":\"Griefer\",\"created\":\"2024-01-01 00:00:00 +0000\",\
             \"source\":\"Server\",\"expires\":\"forever\",\"reason\":\"griefing\"}}]"
        )
        .into_bytes(),
        b"[{\"ip\":\"1.2.3.4\",\"created\":\"2024-01-01 00:00:00 +0000\",\
          \"source\":\"Server\",\"expires\":\"forever\",\"reason\":\"spam\"}]"
            .to_vec(),
        format!(
            "[{{\"name\":\"Steve\",\"uuid\":\"{u}\",\"expiresOn\":\"2030-01-01 00:00:00 +0000\"}}]"
        )
        .into_bytes(),
        b"[]".to_vec(),
        b"null".to_vec(),
        b"{}".to_vec(),
        b"[1,2,3]".to_vec(),
        b"[\"string\"]".to_vec(),
        b"[null,true,{\"uuid\":5}]".to_vec(),
        b"\"bare\"".to_vec(),
        Vec::new(),
    ]
}

/// Real `openpac_claims.json`/`openpac_parties.json` bytes produced by the
/// crate's own writer, plus hand-shaped hostile documents.
fn claims_corpus(scratch: &std::path::Path) -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    let mut world = ClaimsWorld::new();
    let main = SubClaimId::main();
    if let Ok(key) = ClaimChunkKey::new("minecraft:overworld", 0, 0) {
        let _ = world.claim_chunk("uuid-steve", key, &main);
    }
    if let Ok(key) = ClaimChunkKey::new("minecraft:the_nether", 5, -5) {
        let _ = world.claim_chunk("uuid-alex", key.clone(), &main);
        let _ = world.set_forceload("uuid-alex", &key, true);
    }
    let _ = world.create_party("uuid-steve", "fuzzy wuzzy");
    if world.save_to_dir(scratch).is_ok() {
        for file in [CLAIMS_FILE, PARTIES_FILE] {
            if let Ok(bytes) = std::fs::read(scratch.join(file)) {
                corpus.push(bytes);
            }
        }
    }
    corpus.push(
        br#"{"revision":7,"nextPartyId":2,"admins":["uuid-op"],
            "claims":[{"dim":"minecraft:overworld","cx":0,"cz":0,
                       "owner":{"kind":"player","uuid":"uuid-steve"},
                       "sub":"main","forceloaded":true,"color":255}],
            "configs":[{"owner":"player:uuid-steve","sub":"main",
                        "config":{"protect":true}}],
            "defaultConfig":{"protect":true}}"#
            .to_vec(),
    );
    corpus.push(
        br#"{"parties":[{"id":"party-1","name":"crew","ownerUuid":"uuid-steve",
            "members":{"uuid-steve":"owner"},"allies":[]}]}"#
            .to_vec(),
    );
    corpus.push(b"{}".to_vec());
    corpus.push(b"[]".to_vec());
    corpus.push(Vec::new());
    corpus
}

/// `claims_sync` wire bytes: a real batch, a wrapped frame, and hostile
/// headers.
fn claims_wire_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    let batch = ClaimsWireBatch::new(
        0,
        3,
        "minecraft:overworld",
        vec![
            ClaimsWireEntry {
                cx: 0,
                cz: 0,
                state: Some(ClaimsWireCell {
                    owner_kind: WireOwnerKind::Player,
                    owner_id: "uuid-steve".to_string(),
                    sub_claim: "main".to_string(),
                    forceloaded: true,
                    color_rgb: 0x00ff_00ff,
                }),
            },
            ClaimsWireEntry {
                cx: -1,
                cz: 2,
                state: Some(ClaimsWireCell {
                    owner_kind: WireOwnerKind::Server,
                    owner_id: String::new(),
                    sub_claim: "main".to_string(),
                    forceloaded: false,
                    color_rgb: 0,
                }),
            },
            ClaimsWireEntry {
                cx: 9,
                cz: 9,
                state: None,
            },
        ],
    );
    if let Ok(bytes) = encode_claims_batch(&batch) {
        corpus.push(bytes);
        for channel in [
            CHANNEL_OPENPAC_CLAIMS_SYNC,
            CHANNEL_FTBCHUNKS_CLAIMS_SYNC,
            CHANNEL_MEGU_CLAIMS_V1,
        ] {
            if let Ok(frame) = encode_custom_payload_frame(channel, &batch) {
                corpus.push(frame);
            }
        }
    }
    // A delta batch (revision_from > 0) and an empty batch.
    let delta = ClaimsWireBatch::new(2, 5, "minecraft:the_nether", vec![]);
    if let Ok(bytes) = encode_claims_batch(&delta) {
        corpus.push(bytes);
    }
    corpus
}

/// `SBC1` envelopes: the encoder's own output plus surgical corruptions
/// whose CRC still validates, reaching every post-CRC bounds check.
fn sbc1_corpus() -> Vec<Vec<u8>> {
    let fp = StartupBinaryCache::compute_fingerprint(
        3955,
        767,
        &["vanilla".to_string(), "fabric".to_string()],
        &[StartupInputFile::from_contents("datapacks/x/data.json", b"{}")],
    );
    let mut corpus = Vec::new();
    for payload in [b"{}".as_slice(), b"payload-bytes".as_slice(), &[0u8; 512][..]] {
        corpus.push(StartupBinaryCache::encode_envelope(
            CACHE_SCHEMA_VERSION,
            &fp,
            payload,
        ));
    }
    // Wrong schema under a valid CRC.
    corpus.push(StartupBinaryCache::encode_envelope(
        CACHE_SCHEMA_VERSION.wrapping_add(1),
        &fp,
        b"{}",
    ));
    // Wrong fingerprint under a valid CRC.
    let other = StartupBinaryCache::compute_fingerprint(1, 1, &[], &[]);
    corpus.push(StartupBinaryCache::encode_envelope(
        CACHE_SCHEMA_VERSION,
        &other,
        b"{}",
    ));
    // Lying length field with a recomputed CRC: the declared payload_len is
    // huge but the body is short — exercises the exact-length check past
    // the fingerprint gate.
    let mut liar = StartupBinaryCache::encode_envelope(CACHE_SCHEMA_VERSION, &fp, b"{}");
    liar[40..44].copy_from_slice(&u32::MAX.to_be_bytes());
    let body_len = liar.len() - 4;
    let crc = crc32(&liar[..body_len]);
    liar[body_len..].copy_from_slice(&crc.to_be_bytes());
    corpus.push(liar);
    // Raw magic only.
    corpus.push(STARTUP_CACHE_MAGIC.to_vec());
    corpus
}

// ---------------------------------------------------------------------------
// server.properties + eula.txt decoders.
// ---------------------------------------------------------------------------

#[test]
fn properties_and_eula_never_panic() {
    let mut rng = Rng::new(0x9205_E11A_0001);
    let corpus = properties_corpus();
    let iters = iters();
    let dir = scratch_dir("props");
    let eula_path = dir.join(EULA_FILE);
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 256 * 1024);
        let text = String::from_utf8_lossy(&input);
        drive("ServerProperties::parse", &input, || {
            let _ = ServerProperties::parse(&text);
        });
        drive("PropertiesFile::parse_bytes", &input, || {
            let _ = PropertiesFile::parse_bytes(&input);
        });
        if let Ok(text) = std::str::from_utf8(&input) {
            drive("ServerProperties::parse::strict", &input, || {
                let _ = ServerProperties::parse(text);
                let _ = PropertiesFile::parse(text);
            });
        }
        // eula.txt is read straight off disk.
        write_file(&eula_path, &input);
        drive("check_eula", &input, || {
            let _ = check_eula(&dir);
        });
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            let text = String::from_utf8_lossy(input);
            drive("properties::truncate", input, || {
                let _ = ServerProperties::parse(&text);
                let _ = PropertiesFile::parse_bytes(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [b'\\', b'=', 0x00, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                let text = String::from_utf8_lossy(&input);
                drive("properties::flip", &input, || {
                    let _ = ServerProperties::parse(&text);
                    let _ = PropertiesFile::parse_bytes(&input);
                });
                decoded += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("properties+eula: {decoded} inputs x ~4 parser calls");
}

// ---------------------------------------------------------------------------
// Shared JSON core + ops/whitelist/bans/usercache file decoders.
// ---------------------------------------------------------------------------

#[test]
fn json_core_and_list_files_never_panic() {
    let mut rng = Rng::new(0x1500_550F_0002);
    let corpus = list_json_corpus();
    let iters = iters();
    let dir = scratch_dir("lists");
    let path = dir.join("list.json");
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 256 * 1024);
        let text = String::from_utf8_lossy(&input);
        drive("json::parse", &input, || {
            let _ = json::parse(&text);
        });
        if let Ok(text) = std::str::from_utf8(&input) {
            drive("json::parse::strict", &input, || {
                let _ = json::parse(text);
            });
        }
        write_file(&path, &input);
        drive("OpsList::load_from", &input, || {
            let _ = OpsList::load_from(&path);
        });
        drive("Whitelist::load_from", &input, || {
            let _ = Whitelist::load_from(&path);
        });
        drive("BannedPlayers::load_from", &input, || {
            let _ = BannedPlayers::load_from(&path);
        });
        drive("BannedIps::load_from", &input, || {
            let _ = BannedIps::load_from(&path);
        });
        let capacity = *rng.pick(&[0usize, 1, 2, 16, 1024]);
        drive("UserCache::load_from_with_capacity", &input, || {
            let _ = UserCache::load_from_with_capacity(&path, capacity);
        });
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            write_file(&path, input);
            drive("lists::truncate", input, || {
                let _ = OpsList::load_from(&path);
                let _ = Whitelist::load_from(&path);
                let _ = BannedPlayers::load_from(&path);
                let _ = BannedIps::load_from(&path);
                let _ = UserCache::load_from(&path);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [b'{', b'"', 0x00, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                write_file(&path, &input);
                drive("lists::flip", &input, || {
                    let _ = OpsList::load_from(&path);
                    let _ = BannedIps::load_from(&path);
                });
                decoded += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("json+lists: {decoded} inputs x ~8 parser calls");
}

// ---------------------------------------------------------------------------
// OpenPAC claims persistence + the small identifier parsers.
// ---------------------------------------------------------------------------

#[test]
fn claims_persistence_never_panics() {
    let mut rng = Rng::new(0xC1A1_4589_0003);
    let corpus_dir = scratch_dir("claims-corpus");
    let corpus = claims_corpus(&corpus_dir);
    let iters = iters();
    let dir = scratch_dir("claims");
    let claims_path = dir.join(CLAIMS_FILE);
    let parties_path = dir.join(PARTIES_FILE);
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 256 * 1024);
        // Feed the fuzz bytes to both persistence slots; the loader reads
        // whichever exists, so alternate which file carries the noise.
        if rng.below(2) == 0 {
            write_file(&claims_path, &input);
            let _ = std::fs::remove_file(&parties_path);
        } else {
            write_file(&parties_path, &input);
            let _ = std::fs::remove_file(&claims_path);
        }
        drive("ClaimsWorld::load_from_dir", &input, || {
            let _ = ClaimsWorld::load_from_dir(&dir);
        });
        let text = String::from_utf8_lossy(&input);
        drive("SubClaimId::parse", &input, || {
            let _ = SubClaimId::parse(&text);
        });
        drive("PartyRank::parse", &input, || {
            let _ = PartyRank::parse(&text);
        });
        drive("ClaimChunkKey::new", &input, || {
            for cx in [i32::MIN, -1, 0, 1, i32::MAX] {
                let _ = ClaimChunkKey::new(&text, cx, cx.wrapping_neg());
            }
        });
        drive("normalize_ban_ip", &input, || {
            let _ = normalize_ban_ip(&text);
        });
        drive("valid_player_name+offline_uuid", &input, || {
            let _ = valid_player_name(&text);
            let _ = offline_uuid(&text);
        });
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            write_file(&claims_path, input);
            drive("claims::truncate", input, || {
                let _ = ClaimsWorld::load_from_dir(&dir);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [b'{', b'"', 0x00, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                write_file(&claims_path, &input);
                drive("claims::flip", &input, || {
                    let _ = ClaimsWorld::load_from_dir(&dir);
                });
                decoded += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&corpus_dir);
    eprintln!("claims: {decoded} inputs x ~7 parser calls");
}

// ---------------------------------------------------------------------------
// claims_sync batch + custom-payload frame decoders.
// ---------------------------------------------------------------------------

#[test]
fn claims_wire_decoders_never_panic() {
    let mut rng = Rng::new(0xC1A1_45EE_0004);
    let corpus = claims_wire_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 256 * 1024);
        drive("decode_claims_batch", &input, || {
            let _ = decode_claims_batch(&input);
        });
        drive("decode_custom_payload_frame", &input, || {
            let _ = decode_custom_payload_frame(&input);
        });
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("wire::truncate", input, || {
                let _ = decode_claims_batch(input);
                let _ = decode_custom_payload_frame(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len().min(64) {
            for v in [0x00u8, 0x01, 0x80, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("wire::flip", &input, || {
                    let _ = decode_claims_batch(&input);
                    let _ = decode_custom_payload_frame(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("claims_wire: {decoded} inputs x 2 decoder calls");
}

// ---------------------------------------------------------------------------
// SBC1 startup binary cache envelope.
// ---------------------------------------------------------------------------

#[test]
fn startup_cache_envelope_never_panics() {
    let mut rng = Rng::new(0x5BC1_0000_0005);
    let corpus = sbc1_corpus();
    let iters = iters();
    let dir = scratch_dir("sbc1");
    let path = dir.join("startup.bin");
    let fp = StartupBinaryCache::compute_fingerprint(
        3955,
        767,
        &["vanilla".to_string(), "fabric".to_string()],
        &[StartupInputFile::from_contents("datapacks/x/data.json", b"{}")],
    );
    let other_fp = StartupBinaryCache::compute_fingerprint(1, 1, &[], &[]);
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 64 * 1024);
        write_file(&path, &input);
        drive("StartupBinaryCache::load_if_fresh", &input, || {
            for expected in [&fp, &other_fp] {
                let lookup = StartupBinaryCache::load_if_fresh(&path, expected);
                // Bounded-memory invariant: a Hit can never hand back more
                // payload than the file held.
                if let CacheLookup::Hit(bytes) = lookup {
                    assert!(bytes.len() < input.len());
                }
            }
        });
        decoded += 1;
    }
    for seed in &corpus {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            write_file(&path, input);
            drive("sbc1::truncate", input, || {
                let _ = StartupBinaryCache::load_if_fresh(&path, &fp);
            });
            decoded += 1;
        }
        for i in 0..seed.len() {
            for v in [0x00u8, 0x7f, 0x80, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                write_file(&path, &input);
                drive("sbc1::flip", &input, || {
                    let _ = StartupBinaryCache::load_if_fresh(&path, &fp);
                });
                decoded += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("sbc1: {decoded} inputs x 2 decoder calls");
}
