// SPDX-License-Identifier: LGPL-3.0-or-later

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::Trap;

/// Cloneable watchdog handle for one plugin. A bump interrupts execution
/// already in flight. Subsequent callbacks take a fresh checkpoint.
#[derive(Clone, Debug, Default)]
pub struct EpochGuard(Arc<AtomicU64>);

impl EpochGuard {
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn checkpoint(&self) -> EpochCheckpoint<'_> {
        EpochCheckpoint {
            current: &self.0,
            initial: self.0.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct EpochCheckpoint<'a> {
    current: &'a AtomicU64,
    initial: u64,
}

impl EpochCheckpoint<'_> {
    pub(crate) fn check(self) -> Result<(), Trap> {
        if self.current.load(Ordering::Relaxed) != self.initial {
            return Err(Trap::EpochExceeded);
        }
        Ok(())
    }
}
