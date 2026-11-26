pub const HEADER: usize = 24;
pub const MAX_FRAME: usize = 64 * 1024;
const MAGIC: u32 = 0x3546_464f; // OFF5, little endian
const MAX_RECORDS: usize = 4096;

fn u16at(b: &[u8], i: usize) -> usize {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap()) as usize
}

pub fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn u64at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

fn finite_f32(b: &[u8], i: usize) -> bool {
    f32::from_bits(u32at(b, i)).is_finite()
}

pub struct Validated {
    pub count: i32,
    slot: usize,
    generation: u64,
}

impl Validated {
    pub fn accept(self, generations: &mut [u64; 3]) -> i32 {
        generations[self.slot] = self.generation;
        self.count
    }
}

pub fn validate(frame: &[u8], generations: &[u64; 3]) -> Result<Validated, i32> {
    if !(HEADER..=MAX_FRAME).contains(&frame.len()) || u32at(frame, 0) != MAGIC || frame[4] != 1 {
        return Err(-1);
    }
    let scenario = frame[5];
    let count = u16at(frame, 6);
    let generation = u64at(frame, 8);
    let payload = &frame[HEADER..];
    if count > MAX_RECORDS
        || u32at(frame, 16) as usize != payload.len()
        || megu_core::hash::crc32_ieee(payload) != u32at(frame, 20)
    {
        return Err(-2);
    }

    match scenario {
        1 => {
            if payload.len() != count * 8 {
                return Err(-4);
            }
            for r in payload.as_chunks::<8>().0 {
                if u16at(r, 0) >= 2000 || r[2] > 3 || r[3] & !1 != 0 || !finite_f32(r, 4) {
                    return Err(-8);
                }
            }
        }
        2 => {
            const POSES_BYTES: usize = 4 * 52;
            if count != 4
                || !(POSES_BYTES..=POSES_BYTES + 192).contains(&payload.len())
                || !(payload.len() - POSES_BYTES).is_multiple_of(6)
            {
                return Err(-4);
            }
            let (poses, diffs) = payload.split_at(POSES_BYTES);
            for r in poses.as_chunks::<52>().0 {
                if !(0..3).all(|i| f64::from_bits(u64at(r, 8 + i * 8)).is_finite())
                    || !(0..5).all(|i| finite_f32(r, 32 + i * 4))
                {
                    return Err(-8);
                }
            }
            for r in diffs.as_chunks::<6>().0 {
                if u16at(r, 0) >= 1024 || u16at(r, 2) >= 64 || u16at(r, 4) != 1 {
                    return Err(-8);
                }
            }
        }
        3 => {
            if payload.len() != count * 24 {
                return Err(-4);
            }
            if count != 1
                || u32at(payload, 8) > 4096
                || ![0, 4, 12, 16, 20]
                    .into_iter()
                    .all(|i| finite_f32(payload, i))
            {
                return Err(-7);
            }
        }
        _ => return Err(-3),
    }

    let slot = usize::from(scenario - 1);
    if generation <= generations[slot] {
        return Err(-6);
    }
    Ok(Validated {
        count: count as i32,
        slot,
        generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_ieee_vectors_across_chunk_boundaries() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"12345678"), 0x9ae0_daaf);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414f_a339
        );
    }
}
