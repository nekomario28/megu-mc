//! `eula.txt` (Vanilla's `Eula`): the server may only run once the file says
//! `eula=true`. [`write_default_eula`] always writes `eula=false` — agreeing
//! to the EULA is an operator's deliberate edit, never something code does
//! for them.

use crate::atomic::write_atomic;
use crate::properties::PropertiesFile;
use std::path::Path;

/// The file's Vanilla name.
pub const EULA_FILE: &str = "eula.txt";

/// The agreement the notice links to (`CommonLinks.EULA`).
pub const EULA_LINK: &str = "https://aka.ms/MinecraftEULA";

/// What [`check_eula`] found in `dir/eula.txt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EulaStatus {
    /// `eula=true`: the server may run.
    Accepted,
    /// The file exists but does not say `eula=true` (including unreadable or
    /// malformed content — those fail closed, never accepted).
    NotAccepted,
    /// No `eula.txt`.
    Missing,
}

/// Read `dir/eula.txt` and report whether it accepts the EULA.
pub fn check_eula(dir: &Path) -> EulaStatus {
    let bytes = match std::fs::read(dir.join(EULA_FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return EulaStatus::Missing,
        Err(_) => return EulaStatus::NotAccepted,
    };
    match PropertiesFile::parse_bytes(&bytes) {
        Ok(props) if props.get("eula").map(|v| v.trim().eq_ignore_ascii_case("true")) == Some(true) => {
            EulaStatus::Accepted
        }
        _ => EulaStatus::NotAccepted,
    }
}

/// Write the standard notice plus `eula=false` to `dir/eula.txt` (atomically).
/// This is what a first boot creates so the operator sees what to change — it
/// must never write `eula=true`.
pub fn write_default_eula(dir: &Path) -> std::io::Result<()> {
    let body = format!(
        "#By changing the setting below to TRUE you are indicating your agreement to our EULA ({EULA_LINK}).\n\
         eula=false\n"
    );
    write_atomic(&dir.join(EULA_FILE), body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("server-files-eula-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_then_default_then_accepted() {
        let dir = tmp_dir("flow");
        assert_eq!(check_eula(&dir), EulaStatus::Missing);

        write_default_eula(&dir).unwrap();
        assert_eq!(check_eula(&dir), EulaStatus::NotAccepted);
        let text = std::fs::read_to_string(dir.join(EULA_FILE)).unwrap();
        assert!(text.contains("eula=false"), "{text}");
        assert!(text.contains(EULA_LINK), "{text}");
        assert!(!text.contains("eula=true"), "{text}");

        // The operator's edit is what accepts — and a `key = value` spelling
        // parses like the rest of properties files do.
        std::fs::write(dir.join(EULA_FILE), "# agreed\neula = true\n").unwrap();
        assert_eq!(check_eula(&dir), EulaStatus::Accepted);

        // Present but wrong content fails closed.
        std::fs::write(dir.join(EULA_FILE), "eula=\\u00zz").unwrap();
        assert_eq!(check_eula(&dir), EulaStatus::NotAccepted);
        std::fs::write(dir.join(EULA_FILE), "eula=maybe").unwrap();
        assert_eq!(check_eula(&dir), EulaStatus::NotAccepted);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
