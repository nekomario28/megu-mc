//! Chunk batch acknowledgement (Java protocol 767).
//!
//! Vanilla paces chunk sending: the server sends a batch of chunks framed by
//! Chunk Batch Start / Chunk Batch Finished, and the client answers the finished
//! frame with Chunk Batch Received (serverbound 0x08) carrying the number of
//! chunks per tick it wants next. The server waits for that answer before it
//! starts the next batch.
//!
//! This port sent everything the view window held in one go and ignored 0x08, so
//! a client that asked for a slower rate did not get one, and a client that never
//! acknowledged was never noticed. The state here is the gate; the caller decides
//! when to pump the next batch.
//!
//! One deliberate difference from vanilla, because the project's own clients are
//! partial: a client that has never acknowledged is not gated (its first batch is
//! what this port always sent). A vanilla client always acknowledges, so from its
//! second batch on the behaviour is vanilla's.

use std::time::{Duration, Instant};

/// How long a batch may stay unacknowledged before the client is treated as gone.
/// Vanilla disconnects on a chunk-load timeout; this is the same idea with a
/// generous bound, so a client that stalls mid-load does not hold a slot forever.
pub(crate) const ACK_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper bound on a single batch, whatever the client asks for. A client cannot
/// make the server send more per batch than a whole maximum view window.
const HARD_MAX_BATCH: usize = 4096;

#[derive(Debug)]
pub(crate) struct ChunkFeedback {
    max_batch: usize,
    preferred: usize,
    awaiting_since: Option<Instant>,
    acks_seen: u32,
}

impl ChunkFeedback {
    /// `window_batch` is how many chunks the current view window holds: the
    /// initial pace, before the client has said anything.
    pub(crate) fn new(window_batch: usize) -> Self {
        let max_batch = window_batch.clamp(1, HARD_MAX_BATCH);
        Self {
            max_batch,
            preferred: max_batch,
            awaiting_since: None,
            acks_seen: 0,
        }
    }

    /// The view distance changed: the window size is the new ceiling.
    pub(crate) fn set_window(&mut self, window_batch: usize) {
        self.max_batch = window_batch.clamp(1, HARD_MAX_BATCH);
        self.preferred = self.preferred.min(self.max_batch).max(1);
    }

    pub(crate) fn batch_size(&self) -> usize {
        self.preferred
    }

    pub(crate) fn awaiting(&self) -> bool {
        self.awaiting_since.is_some()
    }

    /// Record a batch we just finished sending. Nothing to record before the
    /// client has acknowledged once (see the module note).
    pub(crate) fn sent_batch(&mut self, now: Instant, chunks: usize) {
        if chunks > 0 && self.acks_seen > 0 {
            self.awaiting_since = Some(now);
        }
    }

    /// Chunk Batch Received: exactly four big-endian bytes, a finite positive
    /// chunks-per-tick rate. Anything else is a client this server cannot pace.
    pub(crate) fn acknowledge(&mut self, body: &[u8]) -> Result<(), &'static str> {
        if body.len() != 4 {
            return Err("invalid chunk batch acknowledgement size");
        }
        let rate = f32::from_be_bytes(body.try_into().unwrap());
        if !rate.is_finite() || rate <= 0.0 {
            return Err("invalid client chunk rate");
        }
        self.acks_seen = self.acks_seen.saturating_add(1);
        self.awaiting_since = None;
        self.preferred = (rate.ceil() as usize).clamp(1, self.max_batch);
        Ok(())
    }

    pub(crate) fn timed_out(&self, now: Instant) -> bool {
        self.awaiting_since
            .is_some_and(|since| now.saturating_duration_since(since) > ACK_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(rate: f32) -> Vec<u8> {
        rate.to_be_bytes().to_vec()
    }

    #[test]
    fn the_first_batch_is_not_gated_and_does_not_arm_the_timeout() {
        let now = Instant::now();
        let mut f = ChunkFeedback::new(64);
        assert_eq!(f.batch_size(), 64);
        f.sent_batch(now, 64);
        assert!(
            !f.awaiting(),
            "a client that has never acknowledged is not gated"
        );
        assert!(!f.timed_out(now + ACK_TIMEOUT * 2));
    }

    #[test]
    fn an_acknowledged_client_gates_the_next_batch_on_its_ack() {
        let now = Instant::now();
        let mut f = ChunkFeedback::new(64);
        assert!(f.acknowledge(&ack(30.0)).is_ok());
        assert_eq!(f.batch_size(), 30, "the client's requested rate is adopted");
        f.sent_batch(now, 30);
        assert!(f.awaiting());
        assert!(!f.timed_out(now + Duration::from_secs(14)));
        assert!(f.timed_out(now + ACK_TIMEOUT + Duration::from_millis(1)));
        assert!(f.acknowledge(&ack(12.0)).is_ok());
        assert!(!f.awaiting(), "the ack opens the gate again");
        assert_eq!(f.batch_size(), 12);
    }

    #[test]
    fn the_requested_rate_is_clamped_to_the_window_and_to_positive() {
        let mut f = ChunkFeedback::new(16);
        assert!(f.acknowledge(&ack(1000.0)).is_ok());
        assert_eq!(f.batch_size(), 16, "never more than the window holds");
        assert!(f.acknowledge(&ack(0.5)).is_ok());
        assert_eq!(
            f.batch_size(),
            1,
            "never zero: that would starve the client"
        );
        f.set_window(4);
        assert_eq!(f.batch_size(), 1);
        f.set_window(64);
        assert_eq!(
            f.batch_size(),
            1,
            "a wider window does not raise a client's rate"
        );
    }

    #[test]
    fn a_malformed_acknowledgement_is_refused() {
        let mut f = ChunkFeedback::new(16);
        assert!(f.acknowledge(&[]).is_err());
        assert!(f.acknowledge(&[0, 0, 0]).is_err());
        assert!(f.acknowledge(&[0, 0, 0, 0, 0]).is_err());
        assert!(f.acknowledge(&f32::NAN.to_be_bytes()).is_err());
        assert!(f.acknowledge(&f32::INFINITY.to_be_bytes()).is_err());
        assert!(f.acknowledge(&(-1.0f32).to_be_bytes()).is_err());
        assert!(f.acknowledge(&0.0f32.to_be_bytes()).is_err());
        assert_eq!(f.batch_size(), 16, "a refused ack changes nothing");
    }
}
