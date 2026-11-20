//! Persistence for the scheduled tick owners. The outer save envelope checks
//! the payload; this module owns its own section of it - the footer that frames
//! the section, and the body that footer frames.
use crate::scheduled_ticks::ScheduledTicks;
use std::collections::HashSet;

type Pos = (i32, i32, i32);
type Entry = (Pos, u32);

/// A section ends with its body length and then its magic, so one section's
/// footer is also where the section before it ends.
const FOOTER_LEN: usize = 12;
const MAGIC: &[u8; 4] = b"TIK1";
pub(crate) struct TickState {
    pub now: u64,
    pub block_order: i64,
    pub blocks: ScheduledTicks,
    pub fluid_order: i64,
    pub fluids: ScheduledTicks,
    pub owned: HashSet<Pos>,
    pub sources: HashSet<(i32, i32)>,
    pub generated_blocks: Vec<Entry>,
    pub generated_fluids: Vec<Entry>,
}
/// The bytes this section was appended after, and the body it frames. `None`
/// when the payload does not end in this section's footer at all.
///
/// One parser owns the footer, so the two consumers below cannot disagree about
/// where the section starts.
fn framed(payload: &[u8]) -> Result<Option<(&[u8], &[u8])>, &'static str> {
    if !payload.ends_with(MAGIC) {
        return Ok(None);
    }
    let end = payload
        .len()
        .checked_sub(FOOTER_LEN)
        .ok_or("truncated tick state footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid tick state size")?;
    // `append` always writes a body, so a footer that frames nothing is corrupt.
    if size == 0 {
        return Err("invalid tick state size");
    }
    let start = end.checked_sub(size).ok_or("invalid tick state size")?;
    Ok(Some((&payload[..start], &payload[start..end])))
}

/// The preceding save sections: whatever this section was appended after.
/// A payload without this section's footer is returned unchanged.
pub(crate) fn prefix(payload: &[u8]) -> Result<&[u8], &'static str> {
    Ok(framed(payload)?.map_or(payload, |(before, _)| before))
}
fn position(out: &mut Vec<u8>, p: Pos) {
    for n in [p.0, p.1, p.2] {
        out.extend_from_slice(&n.to_be_bytes());
    }
}
fn entries(out: &mut Vec<u8>, entries: &[Entry]) {
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(p, id) in entries {
        position(out, p);
        out.extend_from_slice(&id.to_be_bytes());
    }
}
fn take<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], &'static str> {
    let (v, rest) = input.split_at_checked(N).ok_or("truncated tick state")?;
    *input = rest;
    Ok(v.try_into().unwrap())
}
/// The reader for `position`.
fn read_position(input: &mut &[u8]) -> Result<Pos, &'static str> {
    Ok((
        i32::from_be_bytes(take(input)?),
        i32::from_be_bytes(take(input)?),
        i32::from_be_bytes(take(input)?),
    ))
}
/// A `u32` element count, bounded by what the remaining bytes can hold.
fn read_count(input: &mut &[u8], size: usize) -> Result<usize, &'static str> {
    let n = u32::from_be_bytes(take(input)?) as usize;
    if n > input.len() / size {
        return Err("invalid tick state count");
    }
    Ok(n)
}
/// The reader for `entries`.
fn read_entries(input: &mut &[u8]) -> Result<Vec<Entry>, &'static str> {
    let n = read_count(input, 16)?;
    let mut entries = Vec::with_capacity(n);
    let mut seen = HashSet::new();
    for _ in 0..n {
        let entry = (read_position(input)?, u32::from_be_bytes(take(input)?));
        if !seen.insert(entry) {
            return Err("duplicate generated tick");
        }
        entries.push(entry);
    }
    Ok(entries)
}
impl TickState {
    pub fn append(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&self.now.to_be_bytes());
        out.extend_from_slice(&self.block_order.to_be_bytes());
        self.blocks.encode(out);
        out.extend_from_slice(&self.fluid_order.to_be_bytes());
        self.fluids.encode(out);
        let mut owned: Vec<_> = self.owned.iter().copied().collect();
        owned.sort_unstable();
        out.extend_from_slice(&(owned.len() as u32).to_be_bytes());
        for p in owned {
            position(out, p);
        }
        let mut sources: Vec<_> = self.sources.iter().copied().collect();
        sources.sort_unstable();
        out.extend_from_slice(&(sources.len() as u32).to_be_bytes());
        for (x, z) in sources {
            out.extend_from_slice(&x.to_be_bytes());
            out.extend_from_slice(&z.to_be_bytes());
        }
        entries(out, &self.generated_blocks);
        entries(out, &self.generated_fluids);
        let size = (out.len() - start) as u64;
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(MAGIC);
    }
    pub fn read(payload: &[u8]) -> Result<Option<Self>, &'static str> {
        let Some((_, body)) = framed(payload)? else {
            return Ok(None);
        };
        let mut input = body;
        let now = u64::from_be_bytes(take(&mut input)?);
        let block_order = i64::from_be_bytes(take(&mut input)?);
        let blocks = ScheduledTicks::decode(&mut input)?;
        let fluid_order = i64::from_be_bytes(take(&mut input)?);
        let fluids = ScheduledTicks::decode(&mut input)?;
        let n = read_count(&mut input, 12)?;
        let mut owned = HashSet::with_capacity(n);
        for _ in 0..n {
            if !owned.insert(read_position(&mut input)?) {
                return Err("duplicate fluid owner");
            }
        }
        let n = read_count(&mut input, 8)?;
        let mut sources = HashSet::with_capacity(n);
        for _ in 0..n {
            let p = (
                i32::from_be_bytes(take(&mut input)?),
                i32::from_be_bytes(take(&mut input)?),
            );
            if !sources.insert(p) {
                return Err("duplicate generation source");
            }
        }
        let generated_blocks = read_entries(&mut input)?;
        let generated_fluids = read_entries(&mut input)?;
        if !input.is_empty() {
            return Err("invalid tick state tail");
        }
        if generated_fluids.iter().any(|(_, id)| *id > 3) {
            return Err("invalid fluid tick id");
        }
        Ok(Some(Self {
            now,
            block_order,
            blocks: blocks.unpack_saved(now as i64),
            fluid_order,
            fluids: fluids.unpack_saved(now as i64),
            owned,
            sources,
            generated_blocks,
            generated_fluids,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduled_ticks::ScheduledTick;
    #[test]
    fn queue_snapshot_preserves_order_and_tick_state_rejects_truncation() {
        let mut blocks = ScheduledTicks::new();
        for x in -3..=3 {
            for z in -3..=3 {
                for sub_order in 0..3 {
                    blocks.schedule(ScheduledTick {
                        pos: (x * 16, 64 + sub_order, z * 16),
                        block: 1,
                        due: 10 + i64::from(sub_order % 2),
                        priority: 0,
                        sub_order: i64::from(sub_order),
                    });
                }
            }
        }
        let _ = blocks.collect(10, 11, |_| true);
        let state = TickState {
            now: 10,
            block_order: 31,
            blocks,
            fluid_order: 7,
            fluids: ScheduledTicks::new(),
            owned: HashSet::from([(1, 2, 3)]),
            sources: HashSet::from([(0, 0), (-1, 2)]),
            generated_blocks: vec![((2, 3, 4), 7)],
            generated_fluids: vec![((3, 4, 5), 0)],
        };
        let mut payload = Vec::new();
        state.append(&mut payload);
        let restored = TickState::read(&payload).unwrap().unwrap();
        let mut original = state.blocks.clone();
        assert_eq!(restored.now, 10);
        assert_eq!(restored.owned, state.owned);
        assert_eq!(restored.sources, state.sources);
        assert_eq!(restored.generated_blocks, state.generated_blocks);
        assert_eq!(restored.generated_fluids, state.generated_fluids);
        // The raw heap and deadline-table codec retains exact in-memory state.
        // TickState::read separately applies native SavedTick unpack on restart.
        let mut snapshot = Vec::new();
        state.blocks.encode(&mut snapshot);
        let mut input = snapshot.as_slice();
        let mut restored_queue = ScheduledTicks::decode(&mut input).unwrap();
        assert!(input.is_empty());
        for time in 10..15 {
            assert_eq!(
                restored_queue.collect(time, 17, |_| true),
                original.collect(time, 17, |_| true)
            );
        }
        let size = payload.len();
        payload[size - 12..size - 4].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(TickState::read(&payload).is_err());
    }

    fn empty_state() -> TickState {
        TickState {
            now: 1,
            block_order: 0,
            blocks: ScheduledTicks::new(),
            fluid_order: 0,
            fluids: ScheduledTicks::new(),
            owned: HashSet::new(),
            sources: HashSet::new(),
            generated_blocks: Vec::new(),
            generated_fluids: Vec::new(),
        }
    }

    #[test]
    fn native_callback_metadata_after_block_and_fluid_reload() {
        use crate::scheduled_ticks::delivery_fixture;

        let cases = delivery_fixture::replay(true, |now, blocks, fluids| {
            let state = TickState {
                now: now as u64,
                block_order: 16,
                blocks: blocks.clone(),
                fluid_order: 16,
                fluids: fluids.clone(),
                ..empty_state()
            };
            let mut payload = Vec::new();
            state.append(&mut payload);
            let restored = TickState::read(&payload).unwrap().unwrap();
            assert_eq!(restored.now, state.now);
            assert_eq!(restored.block_order, state.block_order);
            assert_eq!(restored.fluid_order, state.fluid_order);
            *blocks = restored.blocks;
            *fluids = restored.fluids;
        });
        assert_eq!(cases, 1);
    }

    /// The footer boundary has one owner: the two consumers of it cannot hold
    /// different opinions about what a valid section is. A section that frames
    /// no body used to be accepted by `prefix` (which returned an empty prefix
    /// and let the preceding section be parsed from nothing) while `read`
    /// rejected it.
    #[test]
    fn a_footer_that_frames_no_body_is_refused_by_both_consumers() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u64.to_be_bytes());
        payload.extend_from_slice(MAGIC);
        assert!(TickState::read(&payload).is_err(), "read accepted it");
        assert!(
            prefix(&payload).is_err(),
            "prefix accepted a section with no body"
        );
    }

    /// `prefix` and `read` take the section start from the same footer: a footer
    /// that claims one byte less moves both boundaries by exactly one byte, and
    /// the shifted body no longer parses.
    #[test]
    fn prefix_and_read_take_the_section_start_from_the_same_footer() {
        let mut good = Vec::new();
        empty_state().append(&mut good);
        assert!(prefix(&good).unwrap().is_empty());
        assert!(TickState::read(&good).unwrap().is_some());
        let size = (good.len() - FOOTER_LEN) as u64;
        let mut bad = good.clone();
        let end = bad.len() - FOOTER_LEN;
        bad[end..end + 8].copy_from_slice(&(size - 1).to_be_bytes());
        assert_eq!(
            prefix(&bad).unwrap().len(),
            1,
            "the prefix must grow by the byte the body lost"
        );
        assert!(
            TickState::read(&bad).is_err(),
            "a body shifted by one byte must not parse"
        );
    }

    /// A payload with no tick section is not an error: the caller may hold a
    /// save written before this section existed.
    #[test]
    fn a_payload_without_this_section_is_unchanged() {
        let payload = b"GEN1-section-only".to_vec();
        assert_eq!(prefix(&payload).unwrap(), &payload[..]);
        assert!(TickState::read(&payload).unwrap().is_none());
    }

    /// The fluid id bound is its own rule; it used to report the trailing-bytes
    /// error, which names a different problem.
    #[test]
    fn a_fluid_tick_id_out_of_range_is_named_as_such() {
        let mut state = empty_state();
        state.generated_fluids = vec![((0, 64, 0), 4)];
        let mut payload = Vec::new();
        state.append(&mut payload);
        assert_eq!(
            match TickState::read(&payload) {
                Err(error) => error,
                Ok(_) => panic!("an out-of-range fluid id must be refused"),
            },
            "invalid fluid tick id"
        );
        // The bound is `> 3`, not `>= 3`: an id of 3 still loads.
        let mut ok = empty_state();
        ok.generated_fluids = vec![((0, 64, 0), 3)];
        let mut payload = Vec::new();
        ok.append(&mut payload);
        assert!(TickState::read(&payload).unwrap().is_some());
    }
}
