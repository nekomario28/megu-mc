//! Connection-level limits and abuse guards for the listeners.
//!
//! Everything here is pure policy plus small std-only state so it can be unit
//! tested without a socket. `main.rs` only wires it in. Defaults are chosen so a
//! single developer on loopback never notices them; every limit has an env knob:
//!
//! | env | default | meaning |
//! |---|---|---|
//! | `MC_SPINE_ALLOW_PUBLIC_BIND` | off | required to bind any non-loopback address |
//! | `MC_SPINE_MAX_CONN_PER_IP` | 4 (non-loopback only) | concurrent connections per remote IP, `0` = unlimited; when set explicitly it also applies to loopback |
//! | `MC_SPINE_LOGIN_TIMEOUT_SECS` | 15 | wall-clock budget for handshake + login + configuration |
//! | `MC_SPINE_IDLE_TIMEOUT_SECS` | 60 | play state: no complete packet for this long drops the client |
//! | `MC_SPINE_ALLOWLIST` | unset (off) | comma/space separated player names and/or UUIDs permitted to log in |
//! | `MC_SPINE_RCON_MAX_FAILURES` | 5 | failed RCON passwords from one IP before a lockout |
//! | `MC_SPINE_RCON_LOCKOUT_SECS` | 30 | first lockout length; doubles per further failure, capped at 15 min |
//! | `MC_SPINE_BACKUP_INTERVAL_SECS` | 600 | minimum age before an older world backup generation is rotated |

use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // The guarded state is plain counters; a poisoned lock is still usable.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

// ---------------------------------------------------------------- bind policy

/// Loopback means a client on this host. `localhost` is accepted by name; any
/// other hostname or address (including `0.0.0.0` and `::`) is not loopback.
pub(crate) fn is_loopback_bind(addr: &str) -> bool {
    let a = addr.trim().trim_start_matches('[').trim_end_matches(']');
    if a.eq_ignore_ascii_case("localhost") {
        return true;
    }
    a.parse::<IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

pub(crate) fn public_bind_allowed() -> bool {
    std::env::var("MC_SPINE_ALLOW_PUBLIC_BIND").is_ok_and(|v| truthy(&v))
}

/// `Ok` for loopback, or for anything when the operator opted in explicitly.
pub(crate) fn check_bind(what: &str, addr: &str, allow_public: bool) -> Result<(), String> {
    if allow_public || is_loopback_bind(addr) {
        return Ok(());
    }
    Err(format!(
        "refusing to bind {what} to non-loopback address {addr}: this server uses offline-mode \
         identities (no authentication) and plaintext traffic. Set MC_SPINE_ALLOW_PUBLIC_BIND=1 \
         only behind a firewall/private network you control."
    ))
}

// ------------------------------------------------------------ timeouts / knobs

pub(crate) fn login_timeout() -> Duration {
    Duration::from_secs(
        env_parse("MC_SPINE_LOGIN_TIMEOUT_SECS")
            .unwrap_or(15)
            .max(1),
    )
}

pub(crate) fn idle_timeout() -> Duration {
    Duration::from_secs(env_parse("MC_SPINE_IDLE_TIMEOUT_SECS").unwrap_or(60).max(1))
}

pub(crate) fn backup_interval() -> Duration {
    Duration::from_secs(env_parse("MC_SPINE_BACKUP_INTERVAL_SECS").unwrap_or(600))
}

// ------------------------------------------------------------ per-IP limiting

/// Concurrent connection count per remote address.
pub(crate) struct IpConnections {
    counts: Mutex<BTreeMap<IpAddr, usize>>,
}

/// Releases the per-IP slot on drop, including when the handler panics.
pub(crate) struct IpSlot {
    table: &'static IpConnections,
    ip: IpAddr,
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut c = lock(&self.table.counts);
        if let Some(n) = c.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.remove(&self.ip);
            }
        }
    }
}

impl IpConnections {
    pub(crate) const fn new() -> Self {
        IpConnections {
            counts: Mutex::new(BTreeMap::new()),
        }
    }

    /// `limit == 0` means unlimited (the slot is still counted). Returns `None`
    /// when `ip` already holds `limit` connections.
    pub(crate) fn acquire(&'static self, ip: IpAddr, limit: usize) -> Option<IpSlot> {
        let ip = ip.to_canonical();
        let mut c = lock(&self.counts);
        let n = c.entry(ip).or_insert(0);
        if limit != 0 && *n >= limit {
            if *n == 0 {
                c.remove(&ip);
            }
            return None;
        }
        *n += 1;
        Some(IpSlot { table: self, ip })
    }

    #[cfg(test)]
    fn count(&self, ip: IpAddr) -> usize {
        lock(&self.counts)
            .get(&ip.to_canonical())
            .copied()
            .unwrap_or(0)
    }
}

/// Per-IP limit for `ip`. Default 4, not applied to loopback (a developer's own
/// bots, or a local reverse proxy where everyone is 127.0.0.1); setting
/// `MC_SPINE_MAX_CONN_PER_IP` explicitly applies it everywhere. `0` = unlimited.
pub(crate) fn per_ip_limit(ip: IpAddr) -> usize {
    per_ip_limit_with(ip, env_parse::<usize>("MC_SPINE_MAX_CONN_PER_IP"))
}

pub(crate) fn per_ip_limit_with(ip: IpAddr, explicit: Option<usize>) -> usize {
    match explicit {
        Some(n) => n,
        None if ip.to_canonical().is_loopback() => 0,
        None => 4,
    }
}

// ------------------------------------------------------- duplicate login guard

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoginRefusal {
    NameInUse,
    UuidInUse,
}

/// Identities that are logged in or still finishing logout. A reservation is
/// taken atomically at login and held until the connection handler has finished
/// saving the player, so a second login with the same name cannot load stale
/// inventory/position while the first session is still writing them.
pub(crate) struct LoginRegistry {
    held: Mutex<Vec<(String, [u8; 16])>>,
}

pub(crate) struct LoginGuard {
    registry: &'static LoginRegistry,
    name: String,
    uuid: [u8; 16],
}

impl Drop for LoginGuard {
    fn drop(&mut self) {
        let mut h = lock(&self.registry.held);
        if let Some(i) = h
            .iter()
            .position(|(n, u)| *n == self.name && *u == self.uuid)
        {
            h.swap_remove(i);
        }
    }
}

impl LoginRegistry {
    pub(crate) const fn new() -> Self {
        LoginRegistry {
            held: Mutex::new(Vec::new()),
        }
    }

    /// Names compare case-insensitively (as vanilla does); UUIDs exactly.
    pub(crate) fn reserve(
        &'static self,
        name: &str,
        uuid: [u8; 16],
    ) -> Result<LoginGuard, LoginRefusal> {
        let key = name.to_ascii_lowercase();
        let mut h = lock(&self.held);
        if h.iter().any(|(n, _)| *n == key) {
            return Err(LoginRefusal::NameInUse);
        }
        if h.iter().any(|(_, u)| *u == uuid) {
            return Err(LoginRefusal::UuidInUse);
        }
        h.push((key.clone(), uuid));
        Ok(LoginGuard {
            registry: self,
            name: key,
            uuid,
        })
    }
}

// ------------------------------------------------------------------ allowlist

/// Optional login allowlist of names and/or UUIDs. `None` from `parse` means
/// the feature is off.
#[derive(Debug, Default)]
pub(crate) struct Allowlist {
    names: HashSet<String>,
    uuids: HashSet<[u8; 16]>,
}

fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

impl Allowlist {
    pub(crate) fn parse(spec: &str) -> Option<Allowlist> {
        let mut list = Allowlist::default();
        for tok in spec.split(|c: char| c == ',' || c.is_whitespace()) {
            if tok.is_empty() {
                continue;
            }
            match parse_uuid(tok) {
                Some(u) => {
                    list.uuids.insert(u);
                }
                None => {
                    list.names.insert(tok.to_ascii_lowercase());
                }
            }
        }
        (!list.names.is_empty() || !list.uuids.is_empty()).then_some(list)
    }

    pub(crate) fn permits(&self, name: &str, uuid: &[u8; 16]) -> bool {
        self.names.contains(&name.to_ascii_lowercase()) || self.uuids.contains(uuid)
    }
}

/// The configured allowlist (`MC_SPINE_ALLOWLIST`), read once. `None` = off.
pub(crate) fn allowlist() -> Option<&'static Allowlist> {
    static LIST: std::sync::OnceLock<Option<Allowlist>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let list = Allowlist::parse(&std::env::var("MC_SPINE_ALLOWLIST").unwrap_or_default());
        if let Some(l) = &list {
            eprintln!(
                "[security] login allowlist active: {} name(s), {} uuid(s)",
                l.names.len(),
                l.uuids.len()
            );
        }
        list
    })
    .as_ref()
}

// ----------------------------------------------------------------------- chat

/// Vanilla's `ServerboundChatPacket` / `ServerboundChatCommandPacket` bound.
pub(crate) const MAX_CHAT_CHARS: usize = 256;

/// Vanilla `SharedConstants.isAllowedChatCharacter`: no section sign, no
/// control characters. A vanilla client cannot type these, so a message that
/// has them is not from one; vanilla disconnects for it.
pub(crate) fn chat_text_valid(s: &str) -> bool {
    s.chars().count() <= MAX_CHAT_CHARS
        && s.chars()
            .all(|c| c != '\u{a7}' && c >= ' ' && c != '\u{7f}')
}

/// Vanilla's chat spam counter (`ServerGamePacketListenerImpl.chatSpamThreshold`):
/// every chat message or command adds 20, the counter drains one point per
/// 50 ms tick, and exceeding 200 disconnects ("Kicked for spamming!"). That is
/// a burst of 10 and a sustained one message per second.
pub(crate) struct ChatThrottle {
    score: u64,
    last: Instant,
}

const SPAM_COST: u64 = 20;
const SPAM_LIMIT: u64 = 200;
const SPAM_DRAIN_MS: u64 = 50;

impl ChatThrottle {
    pub(crate) fn new(now: Instant) -> Self {
        ChatThrottle {
            score: 0,
            last: now,
        }
    }

    /// `false` means the player exceeded the limit and should be kicked.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        let drained = now.saturating_duration_since(self.last).as_millis() as u64 / SPAM_DRAIN_MS;
        if drained > 0 {
            self.score = self.score.saturating_sub(drained);
            self.last += Duration::from_millis(drained * SPAM_DRAIN_MS);
        }
        self.score += SPAM_COST;
        self.score <= SPAM_LIMIT
    }
}

// ----------------------------------------------------------------------- RCON

/// Constant-time byte comparison; also hides which byte differs, and compares
/// over the longer of the two inputs so a short guess is not faster to reject.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let n = a.len().max(b.len());
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..n {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    std::hint::black_box(diff) == 0
}

struct AuthEntry {
    failures: u32,
    locked_until: Option<Instant>,
    last_failure: Instant,
}

/// Per-IP failed-password throttle: after `max_failures` failures the address
/// is locked out for `base_lockout`, doubling for each further failure up to
/// `MAX_LOCKOUT`. A success clears the address.
pub(crate) struct AuthThrottle {
    entries: Mutex<BTreeMap<IpAddr, AuthEntry>>,
}

const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);
const MAX_TRACKED: usize = 1024;

impl AuthThrottle {
    pub(crate) const fn new() -> Self {
        AuthThrottle {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Remaining lockout for `ip`, if any.
    pub(crate) fn locked_for(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let e = lock(&self.entries);
        let until = e.get(&ip.to_canonical())?.locked_until?;
        until.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    /// Records a failed attempt and returns the failure count.
    pub(crate) fn record_failure(
        &self,
        ip: IpAddr,
        now: Instant,
        max_failures: u32,
        base_lockout: Duration,
    ) -> u32 {
        let ip = ip.to_canonical();
        let mut e = lock(&self.entries);
        if e.len() >= MAX_TRACKED && !e.contains_key(&ip) {
            e.retain(|_, v| {
                v.locked_until.is_some_and(|u| u > now)
                    || now.saturating_duration_since(v.last_failure) < FORGET_AFTER
            });
            if e.len() >= MAX_TRACKED {
                // still full of live entries: forget the stalest one
                if let Some(oldest) = e
                    .iter()
                    .min_by_key(|(_, v)| v.last_failure)
                    .map(|(k, _)| *k)
                {
                    e.remove(&oldest);
                }
            }
        }
        let ent = e.entry(ip).or_insert(AuthEntry {
            failures: 0,
            locked_until: None,
            last_failure: now,
        });
        if now.saturating_duration_since(ent.last_failure) >= FORGET_AFTER {
            ent.failures = 0;
        }
        ent.failures = ent.failures.saturating_add(1);
        ent.last_failure = now;
        if max_failures > 0 && ent.failures >= max_failures {
            let doublings = (ent.failures - max_failures).min(16);
            let len = base_lockout
                .checked_mul(1u32 << doublings)
                .unwrap_or(MAX_LOCKOUT)
                .min(MAX_LOCKOUT);
            ent.locked_until = Some(now + len);
        }
        ent.failures
    }

    pub(crate) fn record_success(&self, ip: IpAddr) {
        lock(&self.entries).remove(&ip.to_canonical());
    }
}

pub(crate) fn rcon_max_failures() -> u32 {
    env_parse("MC_SPINE_RCON_MAX_FAILURES").unwrap_or(5)
}

pub(crate) fn rcon_lockout() -> Duration {
    Duration::from_secs(env_parse("MC_SPINE_RCON_LOCKOUT_SECS").unwrap_or(30).max(1))
}

// ------------------------------------------------------------- tick supervision

/// Runs `f`, turning a panic into an `Err` with the panic message. The tick
/// thread uses this so a panicking tick is logged and ends the process instead
/// of leaving the listener alive with a dead tick.
///
/// Note on poisoned locks: after a panic, every `Mutex` the panicking thread
/// held is poisoned, and the many `lock().unwrap()` calls elsewhere would
/// panic in turn. That is deliberately not "fixed" here; the process exits
/// instead of limping on with half-updated shared state.
pub(crate) fn catch_panic<F: FnOnce()>(f: F) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn loopback_binds_are_allowed_everything_else_needs_opt_in() {
        for ok in [
            "127.0.0.1",
            "127.0.0.2",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
        ] {
            assert!(check_bind("x", ok, false).is_ok(), "{ok}");
        }
        for bad in [
            "0.0.0.0",
            "::",
            "192.168.1.5",
            "10.0.0.1",
            "example.org",
            "",
        ] {
            let e = check_bind("game listener", bad, false).unwrap_err();
            assert!(e.contains("MC_SPINE_ALLOW_PUBLIC_BIND"), "{bad}: {e}");
            assert!(check_bind("x", bad, true).is_ok(), "{bad} with opt-in");
        }
        assert!(truthy("1") && truthy("TRUE") && !truthy("0") && !truthy(""));
    }

    #[test]
    fn per_ip_limit_defaults_skip_loopback_but_explicit_applies_everywhere() {
        assert_eq!(per_ip_limit_with(ip(127, 0, 0, 1), None), 0);
        assert_eq!(per_ip_limit_with(ip(203, 0, 113, 9), None), 4);
        assert_eq!(per_ip_limit_with(ip(127, 0, 0, 1), Some(2)), 2);
        assert_eq!(per_ip_limit_with(ip(203, 0, 113, 9), Some(0)), 0);
    }

    #[test]
    fn per_ip_slots_are_capped_and_released() {
        static T: IpConnections = IpConnections::new();
        let a = ip(203, 0, 113, 1);
        let b = ip(203, 0, 113, 2);
        let s1 = T.acquire(a, 2).unwrap();
        let _s2 = T.acquire(a, 2).unwrap();
        assert!(T.acquire(a, 2).is_none(), "third from the same IP");
        assert!(T.acquire(b, 2).is_some(), "other IPs are independent");
        drop(s1);
        assert!(T.acquire(a, 2).is_some(), "a freed slot is reusable");
        // limit 0 = unlimited
        let held: Vec<_> = (0..20)
            .map(|_| T.acquire(ip(10, 1, 1, 1), 0).unwrap())
            .collect();
        assert_eq!(T.count(ip(10, 1, 1, 1)), 20);
        drop(held);
        assert_eq!(T.count(ip(10, 1, 1, 1)), 0);
    }

    #[test]
    fn a_panicking_handler_still_releases_its_ip_slot() {
        static T: IpConnections = IpConnections::new();
        let a = ip(198, 51, 100, 7);
        let r = std::panic::catch_unwind(|| {
            let _slot = T.acquire(a, 1).unwrap();
            panic!("synthetic");
        });
        assert!(r.is_err());
        assert_eq!(T.count(a), 0);
    }

    #[test]
    fn mapped_ipv6_counts_as_the_same_ipv4_peer() {
        static T: IpConnections = IpConnections::new();
        let v4 = ip(203, 0, 113, 50);
        let mapped: IpAddr = "::ffff:203.0.113.50".parse().unwrap();
        let _s = T.acquire(v4, 1).unwrap();
        assert!(T.acquire(mapped, 1).is_none());
    }

    #[test]
    fn duplicate_name_or_uuid_is_refused_until_the_first_session_ends() {
        static R: LoginRegistry = LoginRegistry::new();
        let first = R.reserve("Steve", [1; 16]).unwrap();
        assert_eq!(
            R.reserve("Steve", [2; 16]).err(),
            Some(LoginRefusal::NameInUse)
        );
        assert_eq!(
            R.reserve("sTEVE", [2; 16]).err(),
            Some(LoginRefusal::NameInUse)
        );
        assert_eq!(
            R.reserve("Alex", [1; 16]).err(),
            Some(LoginRefusal::UuidInUse)
        );
        let other = R.reserve("Alex", [2; 16]).unwrap();
        drop(first);
        let again = R.reserve("Steve", [1; 16]).unwrap();
        drop(other);
        drop(again);
        assert!(R.reserve("Alex", [2; 16]).is_ok());
    }

    #[test]
    fn allowlist_is_off_when_empty_and_matches_names_or_uuids() {
        assert!(Allowlist::parse("").is_none());
        assert!(Allowlist::parse(" , ,").is_none());
        let l = Allowlist::parse("Steve, alex 01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert!(l.permits("steve", &[0; 16]));
        assert!(l.permits("ALEX", &[0; 16]));
        assert!(!l.permits("Mallory", &[0; 16]));
        let u = parse_uuid("0123456789abcdef0123456789abcdef").unwrap();
        assert!(l.permits("Mallory", &u), "UUID entry permits any name");
        assert!(parse_uuid("not-a-uuid").is_none());
    }

    #[test]
    fn chat_text_follows_vanilla_validity() {
        assert!(chat_text_valid("hello world"));
        assert!(chat_text_valid(&"a".repeat(256)));
        assert!(!chat_text_valid(&"a".repeat(257)));
        assert!(
            chat_text_valid(&"\u{3042}".repeat(256)),
            "256 chars, not bytes"
        );
        assert!(!chat_text_valid("bad \u{a7}c colour"));
        assert!(!chat_text_valid("tab\there"));
        assert!(!chat_text_valid("nul\0"));
        assert!(!chat_text_valid("del\u{7f}"));
    }

    #[test]
    fn chat_burst_of_ten_passes_eleventh_is_spam_and_it_drains() {
        let t0 = Instant::now();
        let mut t = ChatThrottle::new(t0);
        for i in 0..10 {
            assert!(t.allow(t0), "message {i}");
        }
        assert!(!t.allow(t0), "11th in the same instant");
        // a patient player is never kicked: one message per second forever
        let mut t = ChatThrottle::new(t0);
        for i in 0..500u64 {
            assert!(t.allow(t0 + Duration::from_secs(i)), "message {i}");
        }
        // after a kick-worthy burst, draining 20 points (1 s) buys one message
        let mut t = ChatThrottle::new(t0);
        for _ in 0..10 {
            t.allow(t0);
        }
        assert!(t.allow(t0 + Duration::from_secs(1)));
        assert!(!t.allow(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn constant_time_compare_is_correct() {
        assert!(ct_eq(b"hunter2", b"hunter2"));
        assert!(!ct_eq(b"hunter2", b"hunter3"));
        assert!(!ct_eq(b"hunter2", b"hunter"));
        assert!(!ct_eq(b"hunter", b"hunter2"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
        assert!(
            !ct_eq(&[b'a'; 256], &[b'a'; 512]),
            "length difference beyond a byte"
        );
        assert!(!ct_eq(&[b'a'; 256], b""));
    }

    #[test]
    fn rcon_failures_lock_the_ip_out_with_growing_delay_and_success_clears() {
        let th = AuthThrottle::new();
        let a = ip(203, 0, 113, 9);
        let b = ip(203, 0, 113, 10);
        let t0 = Instant::now();
        let base = Duration::from_secs(30);
        for i in 1..5 {
            assert_eq!(th.record_failure(a, t0, 5, base), i);
            assert!(
                th.locked_for(a, t0).is_none(),
                "failure {i} is below the threshold"
            );
        }
        assert_eq!(th.record_failure(a, t0, 5, base), 5);
        assert_eq!(th.locked_for(a, t0), Some(base));
        assert!(th.locked_for(b, t0).is_none(), "other IPs unaffected");
        assert!(th
            .locked_for(a, t0 + base + Duration::from_secs(1))
            .is_none());
        // the next failure doubles the lockout
        th.record_failure(a, t0 + base, 5, base);
        assert_eq!(th.locked_for(a, t0 + base), Some(base * 2));
        // capped
        for _ in 0..40 {
            th.record_failure(a, t0 + base, 5, base);
        }
        assert_eq!(th.locked_for(a, t0 + base), Some(MAX_LOCKOUT));
        th.record_success(a);
        assert!(th.locked_for(a, t0 + base).is_none());
        assert_eq!(th.record_failure(a, t0 + base, 5, base), 1);
    }

    #[test]
    fn rcon_failure_table_stays_bounded() {
        let th = AuthThrottle::new();
        let t0 = Instant::now();
        for i in 0..(MAX_TRACKED as u32 * 2) {
            let ip = IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i));
            th.record_failure(
                ip,
                t0 + Duration::from_millis(i as u64),
                5,
                Duration::from_secs(30),
            );
        }
        assert!(lock(&th.entries).len() <= MAX_TRACKED);
    }

    #[test]
    fn catch_panic_reports_the_message() {
        assert_eq!(catch_panic(|| {}), Ok(()));
        assert_eq!(catch_panic(|| panic!("boom")), Err("boom".to_string()));
        let n = 7;
        assert_eq!(catch_panic(move || panic!("n={n}")), Err("n=7".to_string()));
    }
}
