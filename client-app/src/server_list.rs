// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
#![deny(unsafe_code)]
//! Bounded, fail-closed persistence for the multiplayer server browser.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// File name stored directly inside the selected game directory.
pub const SERVER_LIST_FILE_NAME: &str = "megu_servers.toml";
/// Maximum on-disk representation accepted by the reader.
pub const MAX_SERVER_LIST_BYTES: usize = 256 * 1024;
/// Maximum number of saved servers.
pub const MAX_SERVER_ENTRIES: usize = 256;
const MAX_NAME_BYTES: usize = 128;
const MAX_ADDRESS_BYTES: usize = 255;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One saved multiplayer endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerEntry {
    pub name: String,
    pub address: String,
    pub port: u16,
}

impl ServerEntry {
    /// Validate player-provided data before it is retained or serialized.
    pub fn validate(&self) -> Result<(), String> {
        validate_text("server name", &self.name, MAX_NAME_BYTES)?;
        validate_text("server address", &self.address, MAX_ADDRESS_BYTES)?;
        if self.address == "."
            || self.address == ".."
            || self.address.contains("..")
            || self.address.contains('/')
            || self.address.contains('\\')
            || self.address.chars().any(char::is_whitespace)
        {
            return Err("invalid server address: traversal and path syntax are not allowed".into());
        }
        if self.port == 0 {
            return Err("server port must be in 1..=65535".into());
        }
        Ok(())
    }
}

/// A loaded list whose mutations are persisted before becoming visible.
#[derive(Clone, Debug)]
pub struct ServerListStore {
    path: PathBuf,
    entries: Vec<ServerEntry>,
}

impl ServerListStore {
    /// Load `<game_dir>/megu_servers.toml`; a missing file is an empty list.
    /// Existing symlinked path components and malformed files fail closed.
    pub fn load(game_dir: impl AsRef<Path>) -> Result<Self, String> {
        let game_dir = game_dir.as_ref();
        validate_path_components(game_dir)?;
        let metadata = fs::symlink_metadata(game_dir)
            .map_err(|e| format!("cannot inspect game directory {}: {e}", game_dir.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "game directory is not a regular directory: {}",
                game_dir.display()
            ));
        }
        let path = game_dir.join(SERVER_LIST_FILE_NAME);
        let entries = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(format!(
                    "cannot inspect server list {}: {error}",
                    path.display()
                ))
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(format!(
                    "server list is not a regular file: {}",
                    path.display()
                ))
            }
            Ok(metadata) if metadata.len() > MAX_SERVER_LIST_BYTES as u64 => {
                return Err(format!("server list exceeds {MAX_SERVER_LIST_BYTES} bytes"))
            }
            Ok(_) => {
                let file = File::open(&path)
                    .map_err(|e| format!("cannot read server list {}: {e}", path.display()))?;
                let mut bytes = Vec::new();
                file.take((MAX_SERVER_LIST_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("cannot read server list {}: {e}", path.display()))?;
                if bytes.len() > MAX_SERVER_LIST_BYTES {
                    return Err(format!("server list exceeds {MAX_SERVER_LIST_BYTES} bytes"));
                }
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| "server list is not valid UTF-8".to_string())?;
                parse_entries(text)?
            }
        };
        Ok(Self { path, entries })
    }

    pub fn entries(&self) -> &[ServerEntry] {
        &self.entries
    }

    pub fn find(&self, name_or_address: &str) -> Option<&ServerEntry> {
        self.entries
            .iter()
            .find(|entry| entry.name == name_or_address || entry.address == name_or_address)
    }

    pub fn add(&mut self, entry: ServerEntry) -> Result<(), String> {
        entry.validate()?;
        if self.entries.len() >= MAX_SERVER_ENTRIES {
            return Err(format!(
                "server list is limited to {MAX_SERVER_ENTRIES} entries"
            ));
        }
        let mut next = self.entries.clone();
        next.push(entry);
        self.commit(next)
    }

    pub fn remove(&mut self, index: usize) -> Result<ServerEntry, String> {
        if index >= self.entries.len() {
            return Err("server list index is out of range".into());
        }
        let mut next = self.entries.clone();
        let removed = next.remove(index);
        self.commit(next)?;
        Ok(removed)
    }

    pub fn move_up(&mut self, index: usize) -> Result<bool, String> {
        if index >= self.entries.len() {
            return Err("server list index is out of range".into());
        }
        if index == 0 {
            return Ok(false);
        }
        let mut next = self.entries.clone();
        next.swap(index, index - 1);
        self.commit(next)?;
        Ok(true)
    }

    pub fn move_down(&mut self, index: usize) -> Result<bool, String> {
        if index >= self.entries.len() {
            return Err("server list index is out of range".into());
        }
        if index + 1 == self.entries.len() {
            return Ok(false);
        }
        let mut next = self.entries.clone();
        next.swap(index, index + 1);
        self.commit(next)?;
        Ok(true)
    }

    fn commit(&mut self, entries: Vec<ServerEntry>) -> Result<(), String> {
        let bytes = serialize_entries(&entries)?;
        atomic_write(&self.path, bytes.as_bytes())?;
        self.entries = entries;
        Ok(())
    }
}

fn validate_text(label: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(format!(
            "invalid {label}: empty, too long, or contains control characters"
        ));
    }
    Ok(())
}

fn validate_path_components(path: &Path) -> Result<(), String> {
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err("game directory path cannot contain parent traversal".into());
        }
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "game directory path contains a symlink: {}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

fn parse_entries(text: &str) -> Result<Vec<ServerEntry>, String> {
    let mut entries = Vec::new();
    let mut current: Option<ServerEntry> = None;
    let mut seen = 0_u8;
    for (line_index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[servers]]" {
            if let Some(entry) = current.take() {
                if seen != 0b111 {
                    return Err(format!("server entry {} is incomplete", entries.len() + 1));
                }
                entry.validate()?;
                entries.push(entry);
            }
            if entries.len() >= MAX_SERVER_ENTRIES {
                return Err(format!("server list exceeds {MAX_SERVER_ENTRIES} entries"));
            }
            current = Some(ServerEntry {
                name: String::new(),
                address: String::new(),
                port: 0,
            });
            seen = 0;
            continue;
        }
        let entry = current
            .as_mut()
            .ok_or_else(|| format!("unexpected data on line {}", line_index + 1))?;
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed server list line {}", line_index + 1))?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "name" => {
                if seen & 1 != 0 {
                    return Err(format!("duplicate key on line {}", line_index + 1));
                }
                entry.name = parse_string(value, line_index + 1)?;
                seen |= 1;
            }
            "address" => {
                if seen & 2 != 0 {
                    return Err(format!("duplicate key on line {}", line_index + 1));
                }
                entry.address = parse_string(value, line_index + 1)?;
                seen |= 2;
            }
            "port" => {
                if seen & 4 != 0 {
                    return Err(format!("duplicate key on line {}", line_index + 1));
                }
                let port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port on line {}", line_index + 1))?;
                entry.port = port;
                seen |= 4;
            }
            _ => {
                return Err(format!(
                    "unknown server list key on line {}",
                    line_index + 1
                ))
            }
        }
    }
    if let Some(entry) = current {
        if seen != 0b111 {
            return Err("final server entry is incomplete".into());
        }
        entry.validate()?;
        entries.push(entry);
    }
    if entries.len() > MAX_SERVER_ENTRIES {
        return Err(format!("server list exceeds {MAX_SERVER_ENTRIES} entries"));
    }
    Ok(entries)
}

fn parse_string(value: &str, line: usize) -> Result<String, String> {
    if !value.starts_with('"') || !value.ends_with('"') || value.len() < 2 {
        return Err(format!("expected quoted string on line {line}"));
    }
    let inner = &value[1..value.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                _ => return Err(format!("unsupported escape on line {line}")),
            }
        } else if ch == '"' || ch.is_control() {
            return Err(format!("invalid string on line {line}"));
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn serialize_entries(entries: &[ServerEntry]) -> Result<String, String> {
    if entries.len() > MAX_SERVER_ENTRIES {
        return Err(format!("server list exceeds {MAX_SERVER_ENTRIES} entries"));
    }
    let mut text = String::new();
    for entry in entries {
        entry.validate()?;
        text.push_str("[[servers]]\nname = ");
        text.push_str(&quote(&entry.name));
        text.push_str("\naddress = ");
        text.push_str(&quote(&entry.address));
        text.push_str(&format!("\nport = {}\n\n", entry.port));
    }
    if text.len() > MAX_SERVER_LIST_BYTES {
        return Err(format!("server list exceeds {MAX_SERVER_LIST_BYTES} bytes"));
    }
    Ok(text)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path);
    match metadata {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!(
                "server list is not a regular file: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot inspect server list {}: {error}",
                path.display()
            ))
        }
        _ => {}
    }
    let parent = path
        .parent()
        .ok_or_else(|| "server list has no parent directory".to_string())?;
    let mut created = None;
    for _ in 0..64 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{SERVER_LIST_FILE_NAME}.tmp-{}-{sequence}",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => {
                created = Some((temp, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot create server list temp file: {error}")),
        }
    }
    let Some((temp, mut file)) = created else {
        return Err("cannot allocate a unique server list temp file".into());
    };
    let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(format!("cannot persist server list temp file: {error}"));
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "cannot atomically replace server list {}: {error}",
            path.display()
        ));
    }
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}
