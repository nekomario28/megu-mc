// SPDX-License-Identifier: LGPL-3.0-or-later

//! Bounded binary key-value state and its per-callback write overlay.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

const SNAPSHOT_MAGIC: &[u8; 4] = b"WKV1";
const SNAPSHOT_VERSION: u16 = 1;

/// Limits applied independently to each plugin. Total bytes include both
/// keys and values, excluding the snapshot framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PluginKvLimits {
    pub max_keys: u32,
    pub max_key_bytes: u32,
    pub max_value_bytes: u32,
    pub max_total_bytes: usize,
}

impl Default for PluginKvLimits {
    fn default() -> Self {
        Self {
            max_keys: 1024,
            max_key_bytes: 256,
            max_value_bytes: 64 * 1024,
            max_total_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvError {
    LimitExceeded,
    InvalidSnapshot,
    UnsupportedSnapshotVersion(u16),
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded => write!(f, "plugin KV limit exceeded"),
            Self::InvalidSnapshot => write!(f, "invalid plugin KV snapshot"),
            Self::UnsupportedSnapshotVersion(version) => {
                write!(f, "unsupported plugin KV snapshot version {version}")
            }
        }
    }
}

impl Error for KvError {}

/// Committed state for one plugin. The supervisor owns a separate store
/// for each mod ID and supplies only that store to the guest's VM.
#[derive(Debug)]
pub struct PluginKvStore {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
    limits: PluginKvLimits,
    total_bytes: usize,
}

impl PluginKvStore {
    pub fn new(limits: PluginKvLimits) -> Self {
        Self {
            entries: BTreeMap::new(),
            limits,
            total_bytes: 0,
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.entries.get(key).map(Vec::as_slice)
    }

    pub fn transaction(&mut self) -> TransactionalKvStore<'_> {
        TransactionalKvStore {
            key_count: self.entries.len(),
            total_bytes: self.total_bytes,
            store: self,
            staged: BTreeMap::new(),
        }
    }
}

/// A read-your-writes overlay. Dropping it, including on a guest trap,
/// leaves committed state intact. Exclusive access to the store makes
/// the commit visible as one operation.
pub struct TransactionalKvStore<'a> {
    store: &'a mut PluginKvStore,
    staged: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    key_count: usize,
    total_bytes: usize,
}

impl TransactionalKvStore<'_> {
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        match self.staged.get(key) {
            Some(value) => value.as_deref(),
            None => self.store.get(key),
        }
    }

    pub(crate) fn check_key(&self, key: &[u8]) -> Result<(), KvError> {
        if key.len() > self.store.limits.max_key_bytes as usize {
            return Err(KvError::LimitExceeded);
        }
        Ok(())
    }

    pub fn set(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        self.check_key(key)?;
        let max_value_bytes = self.store.limits.max_value_bytes.min(i32::MAX as u32);
        if value.len() > max_value_bytes as usize {
            return Err(KvError::LimitExceeded);
        }
        let (keys, without_value, added_key_bytes) = match self.get(key) {
            Some(old) => (self.key_count, self.total_bytes - old.len(), 0),
            None => (self.key_count + 1, self.total_bytes, key.len()),
        };
        let bytes = without_value
            .checked_add(added_key_bytes)
            .and_then(|size| size.checked_add(value.len()))
            .ok_or(KvError::LimitExceeded)?;
        if keys > self.store.limits.max_keys as usize || bytes > self.store.limits.max_total_bytes {
            return Err(KvError::LimitExceeded);
        }
        self.staged.insert(key.to_vec(), Some(value.to_vec()));
        self.key_count = keys;
        self.total_bytes = bytes;
        Ok(())
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<bool, KvError> {
        self.check_key(key)?;
        let Some(value) = self.get(key) else {
            return Ok(false);
        };
        self.total_bytes -= key.len() + value.len();
        self.key_count -= 1;
        if self.store.entries.contains_key(key) {
            self.staged.insert(key.to_vec(), None);
        } else {
            // Keep tombstones only for committed keys. Set/delete churn
            // on previously unseen keys cannot grow the overlay.
            self.staged.remove(key);
        }
        Ok(true)
    }

    pub fn commit(self) {
        for (key, value) in self.staged {
            match value {
                Some(value) => {
                    self.store.entries.insert(key, value);
                }
                None => {
                    self.store.entries.remove(&key);
                }
            }
        }
        self.store.total_bytes = self.total_bytes;
    }

    pub fn rollback(self) {
        drop(self);
    }
}

/// Encode one plugin's committed state. Integers are little-endian.
/// The header stores magic, schema version (u16), and entry count (u32).
/// Each sorted entry stores its key and value lengths (u32 each), then
/// the raw key and value bytes.
pub fn encode_kv_snapshot(store: &PluginKvStore) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(SNAPSHOT_MAGIC);
    bytes.extend(SNAPSHOT_VERSION.to_le_bytes());
    bytes.extend((store.entries.len() as u32).to_le_bytes());
    for (key, value) in &store.entries {
        bytes.extend((key.len() as u32).to_le_bytes());
        bytes.extend((value.len() as u32).to_le_bytes());
        bytes.extend(key);
        bytes.extend(value);
    }
    bytes
}

/// Decode and enforce the receiving plugin's limits before exposing state.
/// Duplicate or unsorted keys, truncated fields and trailing bytes are
/// rejected. Arbitrary binary keys and values are supported.
pub fn decode_kv_snapshot(
    mut bytes: &[u8],
    limits: PluginKvLimits,
) -> Result<PluginKvStore, KvError> {
    bytes = bytes
        .strip_prefix(SNAPSHOT_MAGIC)
        .ok_or(KvError::InvalidSnapshot)?;
    let version = u16::from_le_bytes(take_array(&mut bytes)?);
    if version != SNAPSHOT_VERSION {
        return Err(KvError::UnsupportedSnapshotVersion(version));
    }
    let count = read_u32(&mut bytes)?;
    if count > limits.max_keys {
        return Err(KvError::LimitExceeded);
    }
    if (count as usize) > bytes.len() / 8 {
        return Err(KvError::InvalidSnapshot);
    }
    let mut store = PluginKvStore::new(limits);
    let mut transaction = store.transaction();
    let mut previous_key = None;
    for _ in 0..count {
        let key_len = read_u32(&mut bytes)? as usize;
        let value_len = read_u32(&mut bytes)? as usize;
        let (key, rest) = bytes
            .split_at_checked(key_len)
            .ok_or(KvError::InvalidSnapshot)?;
        let (value, rest) = rest
            .split_at_checked(value_len)
            .ok_or(KvError::InvalidSnapshot)?;
        bytes = rest;
        if previous_key.is_some_and(|previous| previous >= key) {
            return Err(KvError::InvalidSnapshot);
        }
        transaction.set(key, value)?;
        previous_key = Some(key);
    }
    if !bytes.is_empty() {
        return Err(KvError::InvalidSnapshot);
    }
    transaction.commit();
    Ok(store)
}

fn take_array<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], KvError> {
    let (field, rest) = bytes.split_first_chunk().ok_or(KvError::InvalidSnapshot)?;
    *bytes = rest;
    Ok(*field)
}

fn read_u32(bytes: &mut &[u8]) -> Result<u32, KvError> {
    Ok(u32::from_le_bytes(take_array(bytes)?))
}
