//! Persistence for the scheduled tick owners. The outer save envelope checks
//! the payload; this section additionally validates its lengths and queues.
use crate::scheduled_ticks::ScheduledTicks;
use std::collections::HashSet;

type Pos = (i32, i32, i32);
type Entry = (Pos, u32);
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
/// Prefix for preceding save sections. This is the same checked framing used
/// by the tick reader; do not scan arbitrary NBT bytes for a marker.
pub(crate) fn prefix(payload: &[u8]) -> Result<&[u8], &'static str> {
    if !payload.ends_with(b"TIK1") { return Ok(payload); }
    let end = payload.len().checked_sub(12).ok_or("truncated tick state footer")?;
    let size = usize::try_from(u64::from_be_bytes(payload[end..end+8].try_into().unwrap()))
        .map_err(|_| "invalid tick state size")?;
    let start = end.checked_sub(size).ok_or("invalid tick state size")?;
    Ok(&payload[..start])
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
        out.extend_from_slice(b"TIK1");
    }
    pub fn read(payload: &[u8]) -> Result<Option<Self>, &'static str> {
        if !payload.ends_with(b"TIK1") {
            return Ok(None);
        }
        let start = prefix(payload)?.len();
        let end = payload.len() - 12;
        let mut input = &payload[start..end];
        fn take<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], &'static str> {
            let (v, rest) = input.split_at_checked(N).ok_or("truncated tick state")?;
            *input = rest;
            Ok(v.try_into().unwrap())
        }
        fn pos(input: &mut &[u8]) -> Result<Pos, &'static str> {
            Ok((
                i32::from_be_bytes(take(input)?),
                i32::from_be_bytes(take(input)?),
                i32::from_be_bytes(take(input)?),
            ))
        }
        fn count(input: &mut &[u8], size: usize) -> Result<usize, &'static str> {
            let n = u32::from_be_bytes(take(input)?) as usize;
            if n > input.len() / size {
                return Err("invalid tick state count");
            }
            Ok(n)
        }
        fn read_entries(input: &mut &[u8]) -> Result<Vec<Entry>, &'static str> {
            let n = count(input, 16)?;
            let mut entries = Vec::with_capacity(n);
            let mut seen = HashSet::new();
            for _ in 0..n {
                let entry = (pos(input)?, u32::from_be_bytes(take(input)?));
                if !seen.insert(entry) {
                    return Err("duplicate generated tick");
                }
                entries.push(entry);
            }
            Ok(entries)
        }
        let now = u64::from_be_bytes(take(&mut input)?);
        let block_order = i64::from_be_bytes(take(&mut input)?);
        let blocks = ScheduledTicks::decode(&mut input)?;
        let fluid_order = i64::from_be_bytes(take(&mut input)?);
        let fluids = ScheduledTicks::decode(&mut input)?;
        let n = count(&mut input, 12)?;
        let mut owned = HashSet::with_capacity(n);
        for _ in 0..n {
            if !owned.insert(pos(&mut input)?) {
                return Err("duplicate fluid owner");
            }
        }
        let n = count(&mut input, 8)?;
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
        if !input.is_empty() || generated_fluids.iter().any(|(_, id)| *id > 3) {
            return Err("invalid tick state tail");
        }
        Ok(Some(Self {
            now,
            block_order,
            blocks,
            fluid_order,
            fluids,
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
    fn restart_preserves_pending_collection_order_and_rejects_truncation() {
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
        let mut restored = TickState::read(&payload).unwrap().unwrap();
        let mut original = state.blocks.clone();
        assert_eq!(restored.now, 10);
        assert_eq!(restored.owned, state.owned);
        assert_eq!(restored.sources, state.sources);
        assert_eq!(restored.generated_blocks, state.generated_blocks);
        assert_eq!(restored.generated_fluids, state.generated_fluids);
        for time in 10..15 {
            assert_eq!(
                restored.blocks.collect(time, 17, |_| true),
                original.collect(time, 17, |_| true)
            );
        }
        let size = payload.len();
        payload[size - 12..size - 4].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(TickState::read(&payload).is_err());
    }
}
