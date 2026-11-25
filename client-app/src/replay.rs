// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded compressed packet blocks for an Instant Replay owner.
//!
//! This checkout has no capture/replay subsystem yet. `InstantReplayBuffer` is
//! the bounded storage primitive for that future owner; it preserves packet
//! boundaries and never decodes beyond its configured byte cap.

use std::collections::VecDeque;

use megu_core::lz4::{compress_block, decompress_block, Lz4Error};

const BLOCK_PACKET_LIMIT: usize = 16;
const BLOCK_RAW_LIMIT: usize = 16 * 1024;
const MAX_DEFAULT_DECODE_MULTIPLIER: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub packets: usize,
    pub blocks: usize,
    pub uncompressed_bytes: usize,
    pub compressed_bytes: usize,
    pub compression_ratio_milli: usize,
}

struct PacketBlock {
    encoded: Vec<u8>,
    packet_count: usize,
    raw_bytes: usize,
}

/// Ring of lossless packet snapshots, evicting complete oldest blocks to fit
/// the user configured uncompressed history cap.
pub struct InstantReplayBuffer {
    max_bytes: usize,
    packets: Vec<Vec<u8>>,
    pending_bytes: usize,
    blocks: VecDeque<PacketBlock>,
    compressed_bytes: usize,
    raw_bytes: usize,
    packet_count: usize,
}

impl InstantReplayBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            packets: Vec::new(),
            pending_bytes: 0,
            blocks: VecDeque::new(),
            compressed_bytes: 0,
            raw_bytes: 0,
            packet_count: 0,
        }
    }

    /// Append one complete packet. A packet larger than the whole ring cap is
    /// rejected. Blocks are compressed after 16 packets or 16 KiB.
    pub fn push_packet(&mut self, packet: &[u8]) -> bool {
        if packet.len() > u32::MAX as usize {
            return false;
        }
        let Some(packet_size) = packet.len().checked_add(4) else {
            return false;
        };
        if packet_size > self.max_bytes {
            return false;
        }
        if !self.packets.is_empty()
            && self.pending_bytes.saturating_add(packet_size) > BLOCK_RAW_LIMIT
        {
            self.flush_pending();
        }
        self.pending_bytes += packet_size;
        self.packets.push(packet.to_vec());
        if self.packets.len() >= BLOCK_PACKET_LIMIT || self.pending_bytes >= BLOCK_RAW_LIMIT {
            self.flush_pending();
        }
        self.enforce_cap();
        true
    }

    /// Flush the partial block, useful before exporting a replay snapshot.
    pub fn flush(&mut self) {
        self.flush_pending();
        self.enforce_cap();
    }

    fn flush_pending(&mut self) {
        if self.packets.is_empty() {
            return;
        }
        let mut raw = Vec::with_capacity(self.pending_bytes);
        for packet in &self.packets {
            raw.extend_from_slice(&(packet.len() as u32).to_le_bytes());
            raw.extend_from_slice(packet);
        }
        let encoded = compress_block(&raw);
        self.raw_bytes += raw.len();
        self.compressed_bytes += encoded.len();
        self.packet_count += self.packets.len();
        self.blocks.push_back(PacketBlock {
            encoded,
            packet_count: self.packets.len(),
            raw_bytes: raw.len(),
        });
        self.packets.clear();
        self.pending_bytes = 0;
    }

    fn enforce_cap(&mut self) {
        while self.compressed_bytes.saturating_add(self.pending_bytes) > self.max_bytes {
            let Some(block) = self.blocks.pop_front() else {
                // A pending packet cannot exceed the cap, but this handles a
                // cap smaller than its framing overhead without retaining it.
                self.packets.clear();
                self.pending_bytes = 0;
                return;
            };
            self.compressed_bytes -= block.encoded.len();
            self.raw_bytes -= block.raw_bytes;
            self.packet_count -= block.packet_count;
        }
    }

    pub fn stats(&self) -> ReplayStats {
        let uncompressed_bytes = self.raw_bytes + self.pending_bytes;
        let compressed_bytes = self.compressed_bytes + self.pending_bytes;
        ReplayStats {
            packets: self.packet_count + self.packets.len(),
            blocks: self.blocks.len() + usize::from(!self.packets.is_empty()),
            uncompressed_bytes,
            compressed_bytes,
            compression_ratio_milli: uncompressed_bytes
                .saturating_mul(1000)
                .checked_div(compressed_bytes)
                .unwrap_or(0),
        }
    }

    /// Decode all retained packets in chronological order, bounded by the
    /// ring's cap, rejecting any malformed block or packet framing.
    pub fn packets(&self) -> Result<Vec<Vec<u8>>, ReplayError> {
        self.packets_capped(self.max_bytes.saturating_mul(MAX_DEFAULT_DECODE_MULTIPLIER))
    }

    /// Decode retained packets under an explicit aggregate output cap.
    pub fn packets_capped(&self, max_out: usize) -> Result<Vec<Vec<u8>>, ReplayError> {
        let mut packets = Vec::with_capacity(self.packet_count + self.packets.len());
        let mut decoded_bytes = 0usize;
        for block in &self.blocks {
            let remaining = max_out.saturating_sub(decoded_bytes);
            let raw = decompress_block(&block.encoded, remaining)?;
            if raw.len() != block.raw_bytes {
                return Err(ReplayError::Malformed);
            }
            decoded_bytes = decoded_bytes
                .checked_add(raw.len())
                .ok_or(ReplayError::OutputLimit)?;
            decode_packets(&raw, block.packet_count, &mut packets)?;
        }
        for packet in &self.packets {
            decoded_bytes = decoded_bytes
                .checked_add(packet.len().saturating_add(4))
                .ok_or(ReplayError::OutputLimit)?;
            if decoded_bytes > max_out {
                return Err(ReplayError::OutputLimit);
            }
            packets.push(packet.clone());
        }
        Ok(packets)
    }
}

fn decode_packets(raw: &[u8], count: usize, packets: &mut Vec<Vec<u8>>) -> Result<(), ReplayError> {
    let mut cursor: usize = 0;
    for _ in 0..count {
        let header_end = cursor.checked_add(4).ok_or(ReplayError::Malformed)?;
        let header = raw.get(cursor..header_end).ok_or(ReplayError::Malformed)?;
        let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        cursor = header_end;
        let end = cursor.checked_add(length).ok_or(ReplayError::Malformed)?;
        packets.push(raw.get(cursor..end).ok_or(ReplayError::Malformed)?.to_vec());
        cursor = end;
    }
    if cursor != raw.len() {
        return Err(ReplayError::Malformed);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayError {
    Malformed,
    OutputLimit,
}

impl From<Lz4Error> for ReplayError {
    fn from(error: Lz4Error) -> Self {
        match error {
            Lz4Error::TooLarge => Self::OutputLimit,
            Lz4Error::Truncated | Lz4Error::BadOffset | Lz4Error::LengthMismatch => {
                Self::Malformed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_blocks_round_trip_and_enforce_memory_cap() {
        let mut ring = InstantReplayBuffer::new(64 * 1024);
        let expected: Vec<Vec<u8>> = (0..80)
            .map(|packet| {
                let mut bytes = Vec::with_capacity(1024);
                bytes.extend_from_slice(&[0x27, 0, 0, 0, 0]);
                bytes.resize(1024, (packet % 4) as u8);
                bytes
            })
            .collect();
        for packet in &expected {
            assert!(ring.push_packet(packet));
        }
        ring.flush();
        let stats = ring.stats();
        println!(
            "replay fixture: {} raw -> {} compressed bytes ({:.3}x)",
            stats.uncompressed_bytes,
            stats.compressed_bytes,
            stats.compression_ratio_milli as f64 / 1000.0
        );
        assert!(stats.compressed_bytes <= 64 * 1024);
        assert_eq!(ring.packets_capped(1024), Err(ReplayError::OutputLimit));
        assert_eq!(ring.packets().unwrap(), expected);
        assert!(stats.compression_ratio_milli > 1_000);
    }

    #[test]
    fn rejects_packets_that_cannot_fit_ring() {
        let mut ring = InstantReplayBuffer::new(8);
        assert!(!ring.push_packet(&[1; 8]));
        assert_eq!(ring.stats().packets, 0);
    }
}
