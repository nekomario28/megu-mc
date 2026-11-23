// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
#![deny(unsafe_code)]
//! Safe atomic shutdown notification for long-running client loops.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cloneable, lock-free shutdown request shared with the session loop.
#[derive(Clone, Debug)]
pub struct ShutdownFlag {
    requested: Arc<AtomicBool>,
}

impl ShutdownFlag {
    fn new() -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether shutdown has been requested.
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Request shutdown from an embedding application or test.
    pub fn request_shutdown(&self) {
        self.requested.store(true, Ordering::SeqCst);
    }
}

/// Install safe SIGINT/SIGTERM handlers on Unix and return a flag for loop polling.
/// On other platforms this still returns a usable manually-set flag.
pub fn install_graceful_shutdown_flag() -> Result<ShutdownFlag, String> {
    let flag = ShutdownFlag::new();
    #[cfg(unix)]
    {
        signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag.requested))
            .map_err(|error| format!("cannot install SIGINT shutdown handler: {error}"))?;
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag.requested))
            .map_err(|error| format!("cannot install SIGTERM shutdown handler: {error}"))?;
    }
    Ok(flag)
}
