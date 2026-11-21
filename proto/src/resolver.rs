// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Minecraft server DNS redirects (`ServerRedirectHandler` /
//! `ServerNameResolver`), pure `std`.
//!
//! Vanilla's rule: when the port is the default 25565 and the host is neither
//! an IP literal nor `localhost`, ask for the SRV record
//! `_minecraft._tcp.<host>.` — a record means the connection goes to its
//! target instead, while no record or a failed query leaves the original
//! address in use. The protocol handshake still carries the original address.
//!
//! [`resolve`] is the entry point; [`resolve_with`] splits the SRV query and
//! the address lookup behind injectable seams so tests never touch a live
//! network. The wire side (RFC 1035/2782 query construction and reply
//! parsing) lives in [`dns`].
use std::fmt;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

mod dns;

/// The hard failure of [`resolve`]: the final address — the redirect target
/// when one exists, else the original host — has no socket address. SRV
/// lookup failures are *not* errors: they fall back to the original address
/// exactly like vanilla.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveError(pub String);

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ResolveError {}

/// Resolve `host`:`port` the way vanilla's `ServerNameResolver` does: a
/// `_minecraft._tcp.<host>.` SRV lookup first on the default port, then the
/// surviving name through the system address resolution.
pub fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, ResolveError> {
    resolve_with(host, port, srv_redirect, |host, port| {
        (host, port)
            .to_socket_addrs()
            .map(|addrs| addrs.collect())
            .map_err(|e| ResolveError(format!("unknown host {host}: {e}")))
    })
}

/// [`resolve`] with both network steps injected. `srv` answers the redirect
/// target for a query name (or `None` for no record / failure); `addresses`
/// maps a surviving `(host, port)` to socket addresses.
fn resolve_with(
    host: &str,
    port: u16,
    srv: impl FnOnce(&str) -> Option<(String, u16)>,
    addresses: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, ResolveError>,
) -> Result<Vec<SocketAddr>, ResolveError> {
    // Vanilla also asks for `_minecraft._tcp.127.0.0.1`, which never has a
    // record: IP literals and localhost skip the query.
    if port == 25565 && host.parse::<IpAddr>().is_err() && host != "localhost" {
        if let Some((target, target_port)) =
            srv(&format!("_minecraft._tcp.{}.", host.trim_end_matches('.')))
        {
            // No second SRV lookup at the target, and no fallback to the
            // original host when the target is unknown.
            return addresses(&target, target_port);
        }
    }
    addresses(host, port)
}

fn srv_redirect(name: &str) -> Option<(String, u16)> {
    dns::query_srv(name, &dns::system_name_servers(), dns::SERVER_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_default_port_even_when_the_original_host_has_no_addresses() {
        let result = resolve_with(
            "play.example.org",
            25565,
            |name| {
                assert_eq!(name, "_minecraft._tcp.play.example.org.");
                Some(("backend.example.org.".into(), 25570))
            },
            |host, port| {
                assert_eq!((host, port), ("backend.example.org.", 25570));
                Ok(vec!["127.0.0.1:25570".parse().unwrap()])
            },
        )
        .unwrap();
        assert_eq!(
            result,
            vec!["127.0.0.1:25570".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn no_record_or_lookup_failure_keeps_the_original_address() {
        resolve_with(
            "play.example.org",
            25565,
            |_| None,
            |host, port| {
                assert_eq!((host, port), ("play.example.org", 25565));
                Ok(vec![])
            },
        )
        .unwrap();
    }

    #[test]
    fn explicit_nondefault_ports_and_ip_literals_do_not_query_srv() {
        for (host, port) in [
            ("play.example.org", 25570),
            ("127.0.0.1", 25565),
            ("::1", 25565),
            ("localhost", 25565),
        ] {
            resolve_with(
                host,
                port,
                |_| panic!("unexpected SRV query"),
                |actual, actual_port| {
                    assert_eq!((actual, actual_port), (host, port));
                    Ok(vec![])
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn fully_qualified_host_does_not_get_a_second_trailing_dot() {
        resolve_with(
            "play.example.org.",
            25565,
            |name| {
                assert_eq!(name, "_minecraft._tcp.play.example.org.");
                None
            },
            |_, _| Ok(vec![]),
        )
        .unwrap();
    }

    #[test]
    fn unknown_redirect_target_does_not_fall_back_to_the_original() {
        let result = resolve_with(
            "play.example.org",
            25565,
            |_| Some(("missing.example.org.".into(), 25570)),
            |host, _| {
                assert_eq!(host, "missing.example.org.");
                Err(ResolveError("unknown redirected host".into()))
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "unknown redirected host");
    }
}
