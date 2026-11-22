use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
}

const MAGIC: &[u8; 4] = b"SPN4";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Edit {
    cx: i32,
    cz: i32,
    index: u32,
    state: u32,
}

fn put_varint(mut n: u64, out: &mut Vec<u8>) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}
#[cfg(test)]
fn get_varint(bytes: &[u8], at: &mut usize) -> Result<u64, &'static str> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let b = *bytes.get(*at).ok_or("truncated varint")?;
        *at += 1;
        if shift == 63 && b > 1 {
            return Err("varint overflow");
        }
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("varint overflow")
}

// Mirrors the persisted overlay table: chunk count, (cx,cz), cell count, then
// big-endian cell index + Minecraft VarInt block state.
fn current_overlay(edits: &[Edit]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(edits.len() as u64, &mut out);
    for e in edits {
        out.extend_from_slice(&e.cx.to_be_bytes());
        out.extend_from_slice(&e.cz.to_be_bytes());
        put_varint(1, &mut out);
        out.extend_from_slice(&e.index.to_be_bytes());
        put_varint(u64::from(e.state), &mut out);
    }
    out
}

#[cfg(test)]
fn parse_overlay(bytes: &[u8]) -> Result<Vec<Edit>, &'static str> {
    let mut at = 0;
    let count = usize::try_from(get_varint(bytes, &mut at)?).map_err(|_| "count overflow")?;
    if count > bytes.len() / 10 + 1 {
        return Err("invalid chunk count");
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let coords = bytes.get(at..at + 8).ok_or("truncated chunk position")?;
        let cx = i32::from_be_bytes(coords[..4].try_into().unwrap());
        let cz = i32::from_be_bytes(coords[4..].try_into().unwrap());
        at += 8;
        let cells =
            usize::try_from(get_varint(bytes, &mut at)?).map_err(|_| "cell count overflow")?;
        if cells > (bytes.len() - at) / 5 {
            return Err("invalid cell count");
        }
        for _ in 0..cells {
            let index = u32::from_be_bytes(
                bytes
                    .get(at..at + 4)
                    .ok_or("truncated index")?
                    .try_into()
                    .unwrap(),
            );
            at += 4;
            let state = u32::try_from(get_varint(bytes, &mut at)?).map_err(|_| "state overflow")?;
            out.push(Edit {
                cx,
                cz,
                index,
                state,
            });
        }
    }
    if at != bytes.len() {
        return Err("trailing overlay bytes");
    }
    Ok(out)
}

fn sha1(input: &[u8]) -> [u8; 20] {
    let mut data = input.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    let mut h = [
        0x67452301u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for block in data.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().copied().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, temp);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0; 20];
    for (chunk, word) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}
fn envelope(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 32);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    out.extend_from_slice(&sha1(payload));
    out.extend_from_slice(payload);
    out
}
fn unwrap(bytes: &[u8]) -> Result<&[u8], &'static str> {
    if bytes.len() < 32 || &bytes[..4] != MAGIC {
        return Err("invalid save header");
    }
    let len = u64::from_be_bytes(bytes[4..12].try_into().unwrap());
    if len != (bytes.len() - 32) as u64 {
        return Err("save length mismatch");
    }
    if sha1(&bytes[32..]) != bytes[12..32] {
        return Err("save checksum mismatch");
    }
    Ok(&bytes[32..])
}

fn write_current(path: &Path, data: &[u8]) -> std::io::Result<u64> {
    let tmp = suffix(path, ".tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    let copied = if path.exists() {
        fs::copy(path, suffix(path, ".bak"))?
    } else {
        0
    };
    fs::rename(tmp, path)?;
    Ok(data.len() as u64 + copied)
}
fn suffix(path: &Path, s: &str) -> PathBuf {
    let mut x = path.as_os_str().to_owned();
    x.push(s);
    x.into()
}
#[cfg(test)]
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = suffix(path, ".tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    fs::rename(tmp, path)?;
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}
fn write_wal_append(path: &Path, data: &[u8]) -> std::io::Result<u64> {
    let mut f = OpenOptions::new().append(true).open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(data.len() as u64)
}
fn write_indexed_checkpoint(path: &Path, edits: &[Edit]) -> std::io::Result<u64> {
    let regions = edits.len().div_ceil(1024);
    let header_len = regions * 8192;
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    // Write chunk records first and make them durable before publishing indices.
    for (i, e) in edits.iter().enumerate() {
        let at = header_len as u64 + (i * 4096) as u64;
        f.seek(SeekFrom::Start(at))?;
        let mut record = [0; 4096];
        record[..4].copy_from_slice(&1u32.to_be_bytes());
        record[4..8].copy_from_slice(&e.cx.to_be_bytes());
        record[8..12].copy_from_slice(&e.cz.to_be_bytes());
        record[12..16].copy_from_slice(&e.index.to_be_bytes());
        record[16..20].copy_from_slice(&e.state.to_be_bytes());
        f.write_all(&record)?;
    }
    f.sync_all()?;
    for (i, _) in edits.iter().enumerate() {
        let region = i / 1024;
        let slot = i % 1024;
        let header_at = (region * 8192 + slot * 4) as u64;
        let sector = (header_len / 4096 + i) as u32;
        let packed = (sector << 8) | 1;
        f.seek(SeekFrom::Start(header_at))?;
        f.write_all(&packed.to_be_bytes())?;
    }
    f.sync_all()?;
    Ok(edits.len() as u64 * (4096 + 4))
}

fn wal_bytes(edits: &[Edit]) -> Vec<u8> {
    let mut out = Vec::with_capacity(edits.len() * 24);
    for (seq, e) in edits.iter().enumerate() {
        out.extend_from_slice(&(seq as u64 + 1).to_be_bytes());
        out.extend_from_slice(&e.cx.to_be_bytes());
        out.extend_from_slice(&e.cz.to_be_bytes());
        out.extend_from_slice(&e.index.to_be_bytes());
        out.extend_from_slice(&e.state.to_be_bytes());
        let sum = sha1(&out[out.len() - 24..]);
        out.extend_from_slice(&sum[..4]);
    }
    out
}
fn verify_wal(bytes: &[u8]) -> Result<usize, &'static str> {
    if bytes.len() % 28 != 0 {
        return Err("truncated WAL record");
    }
    let mut expected = 1u64;
    for record in bytes.chunks_exact(28) {
        if u64::from_be_bytes(record[..8].try_into().unwrap()) != expected {
            return Err("WAL sequence gap");
        }
        if sha1(&record[..24])[..4] != record[24..] {
            return Err("WAL checksum mismatch");
        }
        expected += 1;
    }
    Ok(expected as usize - 1)
}
fn verify_indexed(bytes: &[u8], regions: usize, chunks: usize) -> Result<(), &'static str> {
    let header_len = regions.checked_mul(8192).ok_or("header overflow")?;
    if bytes.len() < header_len {
        return Err("truncated index headers");
    }
    for i in 0..chunks {
        let region = i / 1024;
        let slot = i % 1024;
        let off = region * 8192 + slot * 4;
        let packed = u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap());
        let sector = (packed >> 8) as usize;
        let sectors = (packed & 0xff) as usize;
        if sectors == 0 {
            return Err("missing chunk index");
        }
        let start = sector.checked_mul(4096).ok_or("sector overflow")?;
        let end = start.checked_add(sectors * 4096).ok_or("sector overflow")?;
        if start < header_len || end > bytes.len() {
            return Err("index points beyond durable records");
        }
        let length = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
        if length > sectors * 4096 - 4 {
            return Err("invalid record length");
        }
    }
    Ok(())
}
fn indexed_bytes(edits: &[Edit]) -> Vec<u8> {
    // Clean-room Anvil-inspired layout: 4 KiB sector index + 4 KiB timestamps,
    // per 1024-slot region, then one 4 KiB sector per one-cell chunk record.
    // Not Minecraft Anvil-compatible.
    let regions = edits.len().div_ceil(1024);
    let mut out = vec![0; regions * 8192 + edits.len() * 4096];
    for (i, e) in edits.iter().enumerate() {
        let region = i / 1024;
        let start_sector = regions * 2 + i;
        let slot = i % 1024;
        let off = region * 8192 + slot * 4;
        let packed = ((start_sector as u32) << 8) | 1;
        out[off..off + 4].copy_from_slice(&packed.to_be_bytes());
        let at = regions * 8192 + i * 4096;
        out[at..at + 4].copy_from_slice(&1u32.to_be_bytes());
        out[at + 4..at + 8].copy_from_slice(&e.cx.to_be_bytes());
        out[at + 8..at + 12].copy_from_slice(&e.cz.to_be_bytes());
        out[at + 12..at + 16].copy_from_slice(&e.index.to_be_bytes());
        out[at + 16..at + 20].copy_from_slice(&e.state.to_be_bytes());
    }
    out
}

fn read_pass(path: &Path, bytes: usize, passes: usize) -> (Duration, Duration, bool) {
    let mut f = File::open(path).unwrap();
    #[cfg(target_os = "linux")]
    let cache_drop_advised = {
        use std::os::fd::AsRawFd;
        // POSIX_FADV_DONTNEED: a best-effort page-cache eviction for this file only.
        unsafe { posix_fadvise(f.as_raw_fd(), 0, 0, 4) == 0 }
    };
    #[cfg(not(target_os = "linux"))]
    let cache_drop_advised = false;
    let mut buf = vec![0; bytes];
    let t0 = Instant::now();
    f.read_exact(&mut buf).unwrap();
    let cold = t0.elapsed();
    let t1 = Instant::now();
    for _ in 0..passes {
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_exact(&mut buf).unwrap();
    }
    (cold, t1.elapsed() / passes as u32, cache_drop_advised)
}
fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000_000.0
}
fn edits(n: usize) -> Vec<Edit> {
    (0..n)
        .map(|i| Edit {
            cx: i as i32,
            cz: (i as i32).wrapping_mul(-7),
            index: (i as u32).wrapping_mul(257) & 0x00ff_ffff,
            state: 1 + (i as u32 % 1_000),
        })
        .collect()
}
fn bench(root: &Path) {
    println!(
        "chunks,format,logical_bytes,write_amplification,write_bytes,checkpoint_us,cold_read_us,warm_read_us,cold_cache_drop_advised"
    );
    for n in [64usize, 512, 2048] {
        let data = edits(n);
        let current = envelope(&current_overlay(&data));
        let wal = wal_bytes(&data);
        let indexed = indexed_bytes(&data);
        let logical = n * 5; // changed cell index (4 B) + block-state VarInt (1 B)
        for (name, bytes, mode) in [
            ("current-full-snapshot", current.as_slice(), 0),
            ("wal-append", wal.as_slice(), 1),
            ("indexed-sector-prototype", indexed.as_slice(), 2),
        ] {
            let dir = root.join(format!("{n}-{name}"));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("store.bin");
            let (written, checkpoint) = match mode {
                0 => {
                    let old = envelope(&current_overlay(&edits(n)));
                    fs::write(&path, &old).unwrap();
                    let t0 = Instant::now();
                    let written = write_current(&path, bytes).unwrap();
                    (written, t0.elapsed())
                }
                1 => {
                    fs::write(&path, []).unwrap();
                    let t0 = Instant::now();
                    let written = write_wal_append(&path, bytes).unwrap();
                    (written, t0.elapsed())
                }
                _ => {
                    let header_len = n.div_ceil(1024) * 8192;
                    let f = File::create(&path).unwrap();
                    f.set_len(header_len as u64).unwrap();
                    f.sync_all().unwrap();
                    drop(f);
                    let t0 = Instant::now();
                    let written = write_indexed_checkpoint(&path, &data).unwrap();
                    (written, t0.elapsed())
                }
            };
            let (cold, warm, cache_drop) = read_pass(&path, bytes.len(), 200);
            println!(
                "{n},{name},{logical},{:.3},{written},{:.1},{:.1},{:.1},{cache_drop}",
                written as f64 / logical as f64,
                micros(checkpoint),
                micros(cold),
                micros(warm)
            );
        }
    }
}

fn durability(root: &Path) -> Vec<(&'static str, bool, &'static str)> {
    let clean = b"known-good-overlay-payload";
    let wrapped = envelope(clean);
    let mut truncated = wrapped.clone();
    truncated.truncate(wrapped.len() - 3);
    let mut flipped = wrapped.clone();
    let flip_at = flipped.len() - 1;
    flipped[flip_at] ^= 0x40;
    let stale = root.join("stale.tmp");
    fs::write(&stale, b"partial-new-save").unwrap();
    let existing = root.join("existing.sav");
    fs::write(&existing, &wrapped).unwrap();
    let stale_ignored = unwrap(&fs::read(&existing).unwrap()).ok() == Some(clean.as_slice());
    let interrupted = root.join("interrupted.sav");
    fs::write(&interrupted, &wrapped).unwrap();
    let interrupted_tmp = suffix(&interrupted, ".tmp");
    fs::write(&interrupted_tmp, &truncated).unwrap();
    let truncated_tmp_safe = unwrap(&fs::read(&interrupted_tmp).unwrap()).is_err()
        && unwrap(&fs::read(&interrupted).unwrap()).ok() == Some(clean.as_slice());
    // Ordering model: publishing an index before its journal record is durable
    // must fail closed because the indexed sequence has no verified record.
    let journal = wal_bytes(&edits(1));
    let mut truncated_wal = journal.clone();
    truncated_wal.truncate(27);
    let indexed = indexed_bytes(&edits(1));
    let mut index_only = indexed[..8192].to_vec();
    index_only[..4].copy_from_slice(&((2u32 << 8) | 1).to_be_bytes());
    let order_rejected = verify_indexed(&index_only, 1, 1).is_err();
    vec![
        (
            "truncated SPN4 temp before rename",
            truncated_tmp_safe,
            "reject incomplete temp; previous live file remains readable",
        ),
        (
            "mid-file bit flip",
            unwrap(&flipped).is_err(),
            "reject SHA-1 checksum mismatch",
        ),
        (
            "stale .tmp alongside valid live file",
            stale_ignored && stale.exists(),
            "load live path; ignore tmp; stale tmp cleanup is not performed by reader",
        ),
        (
            "truncated WAL append",
            verify_wal(&truncated_wal).is_err(),
            "reject incomplete fixed-size journal record",
        ),
        (
            "mid-record WAL byte flip",
            verify_wal(&{
                let mut b = journal.clone();
                b[10] ^= 1;
                b
            })
            .is_err(),
            "reject record digest mismatch",
        ),
        (
            "index fsync before record fsync",
            order_rejected,
            "reject index pointing to a record beyond durable file length",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("storage-bench-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }
    #[test]
    fn current_overlay_roundtrips_and_rejects_truncation() {
        let bytes = current_overlay(&edits(64));
        assert_eq!(parse_overlay(&bytes).unwrap(), edits(64));
        assert!(parse_overlay(&bytes[..bytes.len() - 1]).is_err());
    }
    #[test]
    fn spn4_rejects_truncation_and_bit_flip() {
        assert_eq!(
            sha1(b"abc"),
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
                0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
        let mut bytes = envelope(b"payload");
        assert_eq!(unwrap(&bytes), Ok(&b"payload"[..]));
        assert!(unwrap(&bytes[..bytes.len() - 1]).is_err());
        let end = bytes.len() - 1;
        bytes[end] ^= 1;
        assert!(unwrap(&bytes).is_err());
    }
    #[test]
    fn atomic_write_keeps_previous_file_if_tmp_creation_fails() {
        let p = scratch();
        let live = p.join("save");
        fs::write(&live, b"old").unwrap();
        fs::create_dir(suffix(&live, ".tmp")).unwrap();
        assert!(write_atomic(&live, b"new").is_err());
        assert_eq!(fs::read(&live).unwrap(), b"old");
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn durability_controls_fail_closed_or_ignore_stale_tmp() {
        let p = scratch();
        let rows = durability(&p);
        assert!(rows.iter().all(|r| r.1));
        let _ = fs::remove_dir_all(p);
    }
    #[test]
    fn wal_recovery_rejects_partial_and_corrupt_records() {
        let good = wal_bytes(&edits(3));
        assert_eq!(verify_wal(&good), Ok(3));
        assert!(verify_wal(&good[..good.len() - 1]).is_err());
        let mut flipped = good;
        flipped[10] ^= 0x80;
        assert!(verify_wal(&flipped).is_err());
    }
    #[test]
    fn wal_and_indexed_prototypes_cover_requested_sizes() {
        for n in [64, 512, 2048] {
            assert_eq!(parse_overlay(&current_overlay(&edits(n))).unwrap().len(), n);
            assert_eq!(wal_bytes(&edits(n)).len(), n * 28);
            assert_eq!(
                indexed_bytes(&edits(n)).len(),
                n.div_ceil(1024) * 8192 + n * 4096
            );
        }
    }
}

fn main() {
    let temp = std::env::temp_dir().join(format!("megu-storage-bench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp).expect("create bench temp dir");
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "--durability") {
        for (name, passed, detail) in durability(&temp) {
            println!(
                "{}: {} — {}",
                if passed { "PASS" } else { "FAIL" },
                name,
                detail
            );
        }
    } else {
        bench(&temp);
    }
    let _ = fs::remove_dir_all(temp);
}
