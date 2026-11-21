// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! [`ServerDirectory`]: the unified bootstrapper that owns every file a
//! dedicated server keeps in its working directory — the same facade role
//! `horizon-server-files`' directory type fills upstream.
//!
//! One `load_or_init` reads `eula.txt`, `server.properties`, `ops.json`,
//! `whitelist.json`, `banned-players.json`, `banned-ips.json` and
//! `usercache.json` from `root`; one `save_all_atomic` writes the mutable
//! registries back through [`crate::atomic::write_atomic`]; and
//! [`ServerDirectory::admit_login`] answers the only question the login path
//! asks of the whole set, with Vanilla's `PlayerList.canPlayerLogin`
//! precedence and disconnect texts.

use crate::eula::{check_eula, write_default_eula, EulaStatus};
use crate::lists::{
    BanEntry, BannedIps, BannedPlayers, OpsList, Whitelist, BANNED_IPS_FILE, BANNED_PLAYERS_FILE,
    OPS_FILE, WHITELIST_FILE,
};
use crate::properties::{ServerProperties, PROPERTIES_FILE};
use crate::usercache::{UserCache, USERCACHE_FILE};
use mc_spine_core::version::TargetVersion;
use std::io;
use std::path::{Path, PathBuf};

/// What [`ServerDirectory::admit_login`] decided for one login attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// The player may join.
    Allowed,
    /// The player is disconnected with this Vanilla message.
    Denied(String),
}

impl AdmissionDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, AdmissionDecision::Allowed)
    }
}

/// Every file a Vanilla dedicated server owns in its working directory, plus
/// the [`TargetVersion`] the server runs — the version registry in
/// `mc-spine-core` is the single source of truth, so a directory pins it
/// once instead of re-deriving it.
///
/// The registries are public fields: commands and the ops/whitelist/ban
/// handlers mutate them in place, and [`Self::save_all_atomic`] persists the
/// result. `eula.txt` is deliberately *not* rewritable through this facade —
/// the file is the operator's signature, and only `eula=false` is ever
/// written for them (at init, when missing).
#[derive(Debug)]
pub struct ServerDirectory {
    root: PathBuf,
    target: TargetVersion,
    eula: EulaStatus,
    pub properties: ServerProperties,
    pub ops: OpsList,
    pub whitelist: Whitelist,
    pub banned_players: BannedPlayers,
    pub banned_ips: BannedIps,
    pub usercache: UserCache,
}

impl ServerDirectory {
    /// Load every server file under `root`, initializing what Vanilla's
    /// first boot initializes: `root` itself, and `eula.txt` holding
    /// `eula=false` when no file exists. Missing `server.properties` reads
    /// as the defaults and every missing JSON list reads as empty — nothing
    /// on disk is created or clobbered for them. Malformed content in an
    /// existing file is an `InvalidData` error, exactly as the individual
    /// loaders fail closed.
    pub fn load_or_init(root: &Path, target: TargetVersion) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let eula = match check_eula(root) {
            EulaStatus::Missing => {
                write_default_eula(root)?;
                EulaStatus::NotAccepted
            }
            status => status,
        };
        Ok(Self {
            root: root.to_path_buf(),
            target,
            eula,
            properties: ServerProperties::load_from(&root.join(PROPERTIES_FILE))?,
            ops: OpsList::load_from(&root.join(OPS_FILE))?,
            whitelist: Whitelist::load_from(&root.join(WHITELIST_FILE))?,
            banned_players: BannedPlayers::load_from(&root.join(BANNED_PLAYERS_FILE))?,
            banned_ips: BannedIps::load_from(&root.join(BANNED_IPS_FILE))?,
            usercache: UserCache::load_from(&root.join(USERCACHE_FILE))?,
        })
    }

    /// The directory this facade manages.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The version target pinned at [`Self::load_or_init`].
    pub fn target_version(&self) -> TargetVersion {
        self.target
    }

    /// What `eula.txt` said when the directory was loaded (or the status the
    /// just-initialized default implies).
    pub fn eula_status(&self) -> EulaStatus {
        self.eula
    }

    /// Whether the whitelist gates logins. Vanilla's precedence —
    /// `enforce-whitelist` when the key is present, else `white-list` — is
    /// the same one `src/server_config.rs` gives the shared switch.
    fn whitelist_enforced(&self) -> bool {
        match self.properties.doc().get("enforce-whitelist") {
            Some(value) => value.trim().eq_ignore_ascii_case("true"),
            None => self.properties.white_list(),
        }
    }

    /// The ban entry covering this login, by name or by UUID (the file keys
    /// on the profile's UUID; a name hit covers legacy entries written
    /// without one).
    fn find_player_ban(&self, username: &str, uuid: &str) -> Option<&BanEntry> {
        self.banned_players.find(username).or_else(|| {
            self.banned_players
                .entries
                .iter()
                .find(|e| !e.uuid.is_empty() && e.uuid.eq_ignore_ascii_case(uuid))
        })
    }

    /// Vanilla `PlayerList.canPlayerLogin`, over the loaded registries:
    /// IP ban -> player ban -> whitelist (only when enforced, with the
    /// operator bypass) -> `max-players` (with the `bypassesPlayerLimit`
    /// operator bypass) -> allowed. Every denial carries Vanilla's exact
    /// disconnect text.
    ///
    /// `uuid` is the joiner's resolved profile UUID (the offline-mode one in
    /// offline mode); `online_count` is the players already in the player
    /// list, so a full server denies at `online_count == max_players`.
    pub fn admit_login(
        &self,
        username: &str,
        uuid: &str,
        ip: &str,
        online_count: usize,
    ) -> AdmissionDecision {
        if let Some(ban) = self.banned_ips.find(ip) {
            return AdmissionDecision::Denied(format!(
                "Your IP address is banned from this server.\nReason: {}",
                ban.reason
            ));
        }
        if let Some(ban) = self.find_player_ban(username, uuid) {
            return AdmissionDecision::Denied(format!(
                "You are banned from this server.\nReason: {}",
                ban.reason
            ));
        }
        if self.whitelist_enforced()
            && !self.whitelist.contains_name(username)
            && !self.whitelist.contains_uuid(uuid)
            && !self.ops.is_op(username)
        {
            return AdmissionDecision::Denied(
                "You are not white-listed on this server!".to_owned(),
            );
        }
        let max_players =
            usize::try_from(self.properties.max_players().max(0)).unwrap_or(usize::MAX);
        if online_count >= max_players && !self.ops.bypasses_player_limit(username) {
            return AdmissionDecision::Denied("The server is full!".to_owned());
        }
        AdmissionDecision::Allowed
    }

    /// Persist every mutable registry atomically — `server.properties`,
    /// `ops.json`, `whitelist.json`, `banned-players.json`,
    /// `banned-ips.json` and `usercache.json`, each through
    /// [`crate::atomic::write_atomic`]. `eula.txt` is not written: it is the
    /// operator's file, not a registry.
    ///
    /// The writes are ordered but independent; a failure stops the sequence
    /// and returns the error, leaving earlier files saved (atomically) and
    /// later ones untouched.
    pub fn save_all_atomic(&self) -> io::Result<()> {
        self.properties.save_to(&self.root.join(PROPERTIES_FILE))?;
        self.ops.save_to(&self.root.join(OPS_FILE))?;
        self.whitelist.save_to(&self.root.join(WHITELIST_FILE))?;
        self.banned_players
            .save_to(&self.root.join(BANNED_PLAYERS_FILE))?;
        self.banned_ips.save_to(&self.root.join(BANNED_IPS_FILE))?;
        self.usercache.save_to(&self.root.join(USERCACHE_FILE))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eula::EULA_FILE;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("server-files-dir-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const UUID_A: &str = "00000000-0000-0000-0000-0000000000aa";
    const UUID_B: &str = "00000000-0000-0000-0000-0000000000bb";

    #[test]
    fn load_or_init_on_an_empty_dir_writes_only_the_eula() {
        let dir = tmp_dir("init").join("fresh");
        let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();

        assert_eq!(sd.root(), dir.as_path());
        assert_eq!(sd.target_version(), TargetVersion::V26_3);
        assert_eq!(sd.eula_status(), EulaStatus::NotAccepted);
        let eula = std::fs::read_to_string(dir.join(EULA_FILE)).unwrap();
        assert!(eula.contains("eula=false"), "{eula}");
        assert!(!eula.contains("eula=true"), "{eula}");

        // Missing JSON lists default to empty and are not materialized on
        // disk; a missing server.properties reads as Vanilla defaults.
        assert!(sd.ops.entries.is_empty());
        assert!(sd.whitelist.entries.is_empty());
        assert!(sd.banned_players.entries.is_empty());
        assert!(sd.banned_ips.entries.is_empty());
        assert!(sd.usercache.is_empty());
        assert_eq!(sd.properties.max_players(), 20);
        for name in [
            OPS_FILE,
            WHITELIST_FILE,
            BANNED_PLAYERS_FILE,
            BANNED_IPS_FILE,
            USERCACHE_FILE,
            PROPERTIES_FILE,
        ] {
            assert!(!dir.join(name).exists(), "{name} must not be created");
        }

        // A reload sees the initialized eula and accepts once an operator
        // edits it.
        let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V1_21_1).unwrap();
        assert_eq!(sd.target_version(), TargetVersion::V1_21_1);
        assert_eq!(sd.eula_status(), EulaStatus::NotAccepted);
        std::fs::write(dir.join(EULA_FILE), "eula=true\n").unwrap();
        let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
        assert_eq!(sd.eula_status(), EulaStatus::Accepted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_init_reads_existing_files_and_fails_closed_on_corruption() {
        let dir = tmp_dir("existing");
        std::fs::write(dir.join(EULA_FILE), "eula=true\n").unwrap();
        std::fs::write(dir.join(WHITELIST_FILE), r#"[{"uuid":"u1","name":"Friend"}]"#).unwrap();
        std::fs::write(dir.join(PROPERTIES_FILE), "max-players=2\nwhite-list=true\n").unwrap();
        let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
        assert_eq!(sd.eula_status(), EulaStatus::Accepted);
        assert_eq!(sd.properties.max_players(), 2);
        assert!(sd.properties.white_list());
        assert!(sd.whitelist.contains_name("friend"));

        // An existing file is never rewritten by load.
        assert_eq!(
            std::fs::read_to_string(dir.join(WHITELIST_FILE)).unwrap(),
            r#"[{"uuid":"u1","name":"Friend"}]"#
        );

        // Corrupt admin JSON fails closed — a silently empty ops or ban list
        // would be a privilege escalation, not a recovery.
        std::fs::write(dir.join(OPS_FILE), "not json").unwrap();
        assert!(ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory whose registries are populated in memory for the
    /// admission matrix (the registries' own load/save coverage lives in
    /// their modules).
    fn directory_for_admission(tag: &str) -> ServerDirectory {
        let dir = tmp_dir(tag);
        ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap()
    }

    #[test]
    fn admit_login_follows_the_vanilla_precedence() {
        let mut sd = directory_for_admission("admit");
        sd.banned_players
            .ban("Griefer", "griefing", "Server", "2026-11-21T00:00:00Z");
        sd.banned_ips
            .ban("10.0.0.9", "scanning", "Server", "2026-11-21T00:00:00Z");

        // IP ban outranks the player ban (same player, both lists).
        sd.banned_players
            .ban("Scanner", "griefing", "Server", "2026-11-21T00:00:00Z");
        assert_eq!(
            sd.admit_login("Scanner", UUID_A, "10.0.0.9", 0),
            AdmissionDecision::Denied(
                "Your IP address is banned from this server.\nReason: scanning".to_owned()
            )
        );
        assert_eq!(
            sd.admit_login("GRIEFER", UUID_A, "1.2.3.4", 0),
            AdmissionDecision::Denied(
                "You are banned from this server.\nReason: griefing".to_owned()
            )
        );

        // The player ban also matches on the recorded UUID (the file keys on
        // the profile's UUID; a rename cannot dodge it).
        let uuid = sd.banned_players.entries[0].uuid.clone();
        assert_eq!(
            sd.admit_login("GrieferRenamed", &uuid, "1.2.3.4", 0),
            AdmissionDecision::Denied(
                "You are banned from this server.\nReason: griefing".to_owned()
            )
        );

        // Whitelist off by default: a clean player joins while room remains.
        assert_eq!(
            sd.admit_login("Newbie", UUID_A, "1.2.3.4", 0),
            AdmissionDecision::Allowed
        );
        assert_eq!(
            sd.admit_login("Newbie", UUID_A, "1.2.3.4", 20),
            AdmissionDecision::Denied("The server is full!".to_owned())
        );

        // Whitelist enforced via `white-list`: listed players and operators
        // pass; the ban lists still run first.
        sd.properties.set_white_list(true);
        assert_eq!(
            sd.admit_login("Newbie", UUID_A, "1.2.3.4", 0),
            AdmissionDecision::Denied("You are not white-listed on this server!".to_owned())
        );
        assert_eq!(
            sd.admit_login("Griefer", UUID_A, "10.0.0.9", 0),
            AdmissionDecision::Denied(
                "Your IP address is banned from this server.\nReason: scanning".to_owned()
            )
        );
        sd.whitelist.add_with_uuid(UUID_B, "Friend");
        assert_eq!(
            sd.admit_login("friend", "not-a-uuid", "1.2.3.4", 0),
            AdmissionDecision::Allowed
        );
        assert_eq!(
            sd.admit_login("Renamed", UUID_B, "1.2.3.4", 0),
            AdmissionDecision::Allowed
        ); // uuid hit
        sd.ops.set("Admin", 4, false);
        assert_eq!(
            sd.admit_login("admin", UUID_A, "1.2.3.4", 0),
            AdmissionDecision::Allowed
        ); // ops bypass the whitelist

        // `enforce-whitelist` overrides `white-list`, as server_config's
        // shared-variable precedence documents it.
        sd.properties.set_enforce_whitelist(false);
        assert_eq!(
            sd.admit_login("Newbie", UUID_A, "1.2.3.4", 0),
            AdmissionDecision::Allowed
        );
        sd.properties.set_enforce_whitelist(true);
        sd.properties.set_white_list(false);
        assert_eq!(
            sd.admit_login("Newbie", UUID_A, "1.2.3.4", 0),
            AdmissionDecision::Denied("You are not white-listed on this server!".to_owned())
        );
        sd.properties.set_white_list(true);

        // Full server: only a bypassesPlayerLimit operator still joins. The
        // whitelist runs first, so a non-listed bypasser is stopped there.
        sd.properties.set_max_players(2);
        assert_eq!(
            sd.admit_login("Friend", UUID_B, "1.2.3.4", 2),
            AdmissionDecision::Denied("The server is full!".to_owned())
        );
        sd.ops.set("Vip", 4, true);
        sd.whitelist.add("Vip");
        assert_eq!(
            sd.admit_login("Vip", UUID_A, "1.2.3.4", 2),
            AdmissionDecision::Allowed
        );
        assert_eq!(
            sd.admit_login("Vip", UUID_A, "1.2.3.4", 1),
            AdmissionDecision::Allowed
        );
        let _ = std::fs::remove_dir_all(sd.root());
    }

    #[test]
    fn save_all_atomic_roundtrips_every_registry() {
        let dir = tmp_dir("save");
        let mut sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
        sd.properties.set_max_players(8);
        sd.properties.set_motd("A spine server");
        sd.ops.set("Admin", 4, true);
        sd.whitelist.add("Friend");
        sd.banned_players
            .ban("Griefer", "griefing", "Server", "2026-11-21T00:00:00Z");
        sd.banned_ips
            .ban("10.0.0.9", "scanning", "Server", "2026-11-21T00:00:00Z");
        sd.usercache.upsert(
            "00000000-0000-0000-0000-000000000001",
            "Notch",
            "2030-01-01 00:00:00 +0000",
        );
        sd.save_all_atomic().unwrap();

        // A fresh load sees every registry exactly as written, and the
        // admission matrix agrees.
        let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
        assert_eq!(sd.properties.max_players(), 8);
        assert_eq!(sd.properties.motd(), "A spine server");
        assert!(sd.ops.bypasses_player_limit("admin"));
        assert!(sd.whitelist.contains_name("FRIEND"));
        assert_eq!(
            sd.admit_login("griefer", UUID_A, "1.2.3.4", 0),
            AdmissionDecision::Denied(
                "You are banned from this server.\nReason: griefing".to_owned()
            )
        );
        assert_eq!(
            sd.admit_login("Newbie", UUID_A, "10.0.0.9", 0),
            AdmissionDecision::Denied(
                "Your IP address is banned from this server.\nReason: scanning".to_owned()
            )
        );

        // The eula was never clobbered and no temp files leaked.
        let eula = std::fs::read_to_string(dir.join(EULA_FILE)).unwrap();
        assert!(eula.contains("eula=false"), "{eula}");
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_all_atomic_surfaces_io_errors() {
        let dir = tmp_dir("ioerr");
        let sd = ServerDirectory::load_or_init(&dir, TargetVersion::V26_3).unwrap();
        // Make the properties path a directory: the atomic rename fails.
        let props_dir = dir.join(PROPERTIES_FILE);
        std::fs::create_dir_all(&props_dir).unwrap();
        assert!(sd.save_all_atomic().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
