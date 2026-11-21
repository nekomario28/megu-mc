//! Crash-safe file replacement. Every save in this crate goes through
//! [`write_atomic`]: the new content lands in a sibling temp file on the same
//! filesystem (the only thing that makes `rename` atomic), is flushed to disk
//! with `sync_all`, and is then renamed over the target. A crash anywhere in
//! that sequence leaves either the old file or the new one — never a
//! truncated one, and at worst one stray `.tmp` file.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide temp-name sequence. Names are `.<name>.<pid>.<n>.tmp` — no
/// clock needed, and a recycled PID colliding with a stale temp just retries
/// on the next sequence number.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// How many temp names to try before giving up: each attempt burns one
/// sequence number, so the loop only spins while stale files under this PID
/// literally occupy every name it mints.
const MAX_TEMP_ATTEMPTS: u32 = 64;

/// Write `bytes` to `path` atomically, replacing any existing file.
///
/// The temp file is created with `create_new` under the same parent
/// directory; on any failure it is removed before the error is returned, so
/// the directory is never left with a half-written successor beside `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed");
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut last_err: Option<io::Error> = None;
    for _ in 0..MAX_TEMP_ATTEMPTS {
        let seq = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), seq));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        // `sync_all` runs while the handle is open; the handle is dropped before
        // the rename so platforms that refuse to rename an open file still work.
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        return match written.and_then(|()| std::fs::rename(&temp, path)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                Err(e)
            }
        };
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "no free temp file name")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("server-files-atomic-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stray_temps(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn writes_and_overwrites_without_leaving_temp_files() {
        let dir = tmp_dir("roundtrip");
        let path = dir.join("ops.json");

        write_atomic(&path, br#"[{"name":"a"}]"#).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), br#"[{"name":"a"}]"#);

        // A second write replaces atomically, and both runs clean up.
        write_atomic(&path, b"[]").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[]");
        assert_eq!(stray_temps(&dir), Vec::<String>::new());

        // Repeated writes mint distinct temp names, never `AlreadyExists`.
        for i in 0..40u8 {
            write_atomic(&path, &[i]).unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), vec![39u8]);
        assert_eq!(stray_temps(&dir), Vec::<String>::new());

        // A stale temp from an earlier crashed PID does not block the write.
        let stale = dir.join(".ops.json.1.0.tmp");
        std::fs::write(&stale, b"stale").unwrap();
        write_atomic(&path, b"final").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"final");
        assert!(stale.is_file()); // someone else's temp is left alone
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_writes_clean_the_temp_file() {
        let dir = tmp_dir("failure");
        // The parent does not exist: create_new fails and nothing is left.
        let missing = dir.join("nope").join("ops.json");
        assert!(write_atomic(&missing, b"x").is_err());
        assert!(!dir.join("nope").exists());
        assert_eq!(stray_temps(&dir), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
