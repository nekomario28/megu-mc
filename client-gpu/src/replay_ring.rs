// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Bounded packet history for lightweight client-side instant replays.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

pub const MAX_REPLAY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_REPLAY_SPAN_MS: u64 = 60_000;
const FRAME_HEADER_BYTES: usize = 16;
const MAX_LABEL_BYTES: usize = 4096;
const MAX_FRAMES: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayFrame {
    pub elapsed_ms: u64,
    pub packet_id: i32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplaySaveSummary {
    pub frame_count: u32,
    pub payload_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayFile {
    pub protocol_version: u32,
    pub server_label: String,
    pub frames: Vec<ReplayFrame>,
}

#[derive(Clone, Debug, Default)]
pub struct PacketReplayRing {
    frames: VecDeque<ReplayFrame>,
    payload_bytes: usize,
    encoded_bytes: usize,
}

impl PacketReplayRing {
    pub fn push(&mut self, elapsed_ms: u64, packet_id: i32, payload: &[u8]) {
        if self
            .frames
            .back()
            .is_some_and(|last| elapsed_ms < last.elapsed_ms)
        {
            return;
        }
        let frame_bytes = payload.len().saturating_add(FRAME_HEADER_BYTES);
        if frame_bytes > MAX_REPLAY_BYTES {
            self.clear();
            return;
        }
        while self
            .frames
            .front()
            .is_some_and(|frame| elapsed_ms.saturating_sub(frame.elapsed_ms) > MAX_REPLAY_SPAN_MS)
            || self.encoded_bytes + frame_bytes > MAX_REPLAY_BYTES
        {
            self.pop_front();
        }
        self.payload_bytes += payload.len();
        self.encoded_bytes += frame_bytes;
        self.frames.push_back(ReplayFrame {
            elapsed_ms,
            packet_id,
            payload: payload.to_vec(),
        });
    }

    pub fn frames(&self) -> &VecDeque<ReplayFrame> {
        &self.frames
    }

    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    fn pop_front(&mut self) {
        if let Some(frame) = self.frames.pop_front() {
            self.payload_bytes = self.payload_bytes.saturating_sub(frame.payload.len());
            self.encoded_bytes = self
                .encoded_bytes
                .saturating_sub(frame.payload.len() + FRAME_HEADER_BYTES);
        }
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.payload_bytes = 0;
        self.encoded_bytes = 0;
    }

    pub fn save_to_file(
        &self,
        path: &Path,
        protocol_version: u32,
        server_label: &str,
    ) -> io::Result<ReplaySaveSummary> {
        if server_label.len() > MAX_LABEL_BYTES || self.frames.len() > MAX_FRAMES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replay metadata exceeds cap",
            ));
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let part = path.with_extension("megu-replay.part");
        let result = (|| {
            let mut file = File::create(&part)?;
            file.write_all(b"MRPL1")?;
            file.write_all(&protocol_version.to_le_bytes())?;
            file.write_all(&(server_label.len() as u32).to_le_bytes())?;
            file.write_all(server_label.as_bytes())?;
            file.write_all(&(self.frames.len() as u32).to_le_bytes())?;
            for frame in &self.frames {
                let len = u32::try_from(frame.payload.len())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
                file.write_all(&frame.elapsed_ms.to_le_bytes())?;
                file.write_all(&frame.packet_id.to_le_bytes())?;
                file.write_all(&len.to_le_bytes())?;
                file.write_all(&frame.payload)?;
            }
            file.sync_all()?;
            fs::rename(&part, path)?;
            Ok(ReplaySaveSummary {
                frame_count: self.frames.len() as u32,
                payload_bytes: self.payload_bytes,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&part);
        }
        result
    }
}

/// Strict, bounded decoder for the `MRPL1` replay format.
pub fn load_replay_file(path: &Path) -> io::Result<ReplayFile> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let max_file = (MAX_REPLAY_BYTES + MAX_LABEL_BYTES + 32) as u64;
    if size > max_file {
        return Err(invalid("replay file exceeds size cap"));
    }
    let mut magic = [0; 5];
    file.read_exact(&mut magic)?;
    if &magic != b"MRPL1" {
        return Err(invalid("invalid replay signature"));
    }
    let protocol_version = read_u32(&mut file)?;
    let label_len = read_u32(&mut file)? as usize;
    if label_len > MAX_LABEL_BYTES {
        return Err(invalid("server label exceeds cap"));
    }
    let mut label = vec![0; label_len];
    file.read_exact(&mut label)?;
    let server_label =
        String::from_utf8(label).map_err(|_| invalid("server label is not UTF-8"))?;
    let frame_count = read_u32(&mut file)? as usize;
    if frame_count > MAX_FRAMES {
        return Err(invalid("frame count exceeds cap"));
    }
    let mut frames = Vec::new();
    let mut payload_bytes = 0usize;
    let mut prior_elapsed = 0;
    for index in 0..frame_count {
        let elapsed_ms = read_u64(&mut file)?;
        let packet_id = read_i32(&mut file)?;
        let len = read_u32(&mut file)? as usize;
        payload_bytes = payload_bytes
            .checked_add(len)
            .ok_or_else(|| invalid("payload size overflow"))?;
        if payload_bytes > MAX_REPLAY_BYTES
            || payload_bytes.saturating_add((index + 1).saturating_mul(FRAME_HEADER_BYTES))
                > MAX_REPLAY_BYTES
            || len > MAX_REPLAY_BYTES
            || (index > 0 && elapsed_ms < prior_elapsed)
        {
            return Err(invalid("invalid replay frame bounds"));
        }
        let mut payload = vec![0; len];
        file.read_exact(&mut payload)?;
        frames.push(ReplayFrame {
            elapsed_ms,
            packet_id,
            payload,
        });
        prior_elapsed = elapsed_ms;
    }
    let mut trailing = [0];
    if file.read(&mut trailing)? != 0 {
        return Err(invalid("trailing replay bytes"));
    }
    if frames
        .first()
        .zip(frames.last())
        .is_some_and(|(first, last)| {
            last.elapsed_ms.saturating_sub(first.elapsed_ms) > MAX_REPLAY_SPAN_MS
        })
    {
        return Err(invalid("replay span exceeds cap"));
    }
    Ok(ReplayFile {
        protocol_version,
        server_label,
        frames,
    })
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    reader.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut b = [0; 8];
    reader.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_i32(reader: &mut impl Read) -> io::Result<i32> {
    let mut b = [0; 4];
    reader.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("megu-replay-{name}-{}", std::process::id()))
    }

    #[test]
    fn byte_and_time_caps_evict_old_frames() {
        let mut ring = PacketReplayRing::default();
        let block = vec![7; 1024 * 1024];
        for index in 0..5 {
            ring.push(index * 1000, index as i32, &block);
        }
        assert!(ring.payload_bytes() <= MAX_REPLAY_BYTES);
        assert_eq!(ring.frames().len(), 3);
        ring.push(70_001, 9, b"new");
        assert!(ring
            .frames()
            .iter()
            .all(|frame| 70_001 - frame.elapsed_ms <= MAX_REPLAY_SPAN_MS));
        assert!(ring.payload_bytes() <= MAX_REPLAY_BYTES);
    }

    #[test]
    fn atomic_save_and_load_roundtrip() {
        let mut ring = PacketReplayRing::default();
        ring.push(10, 5, b"PK\x03\x04payload");
        let path = temp_file("roundtrip.megu-replay");
        let summary = ring.save_to_file(&path, 767, "test server").unwrap();
        let replay = load_replay_file(&path).unwrap();
        assert_eq!(summary.frame_count, 1);
        assert_eq!(replay.protocol_version, 767);
        assert_eq!(replay.server_label, "test server");
        assert_eq!(
            replay.frames,
            ring.frames().iter().cloned().collect::<Vec<_>>()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_and_truncated_files_fail_closed() {
        let path = temp_file("bad.meGU-replay");
        fs::write(&path, b"MRPL1\0").unwrap();
        assert!(load_replay_file(&path).is_err());
        fs::write(&path, b"nope!xxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        assert_eq!(
            load_replay_file(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let _ = fs::remove_file(path);
    }
}
