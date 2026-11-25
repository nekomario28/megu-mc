// SPDX-License-Identifier: LGPL-3.0-or-later
//! Behaviour tests for [`megu_proto::friend_session`]: the
//! `MEGU-XXXX-XXXX-XXXX` invite codec (roundtrip, typo/tamper/expiry
//! fail-closed), RFC 6886 NAT-PMP and RFC 6887 PCP wire codecs, UPnP
//! IGD SSDP+SOAP builders/parsers (with XXE refusal), the exactly-once
//! [`PortLeaseGuard`], RFC 5389 STUN binding plus the deterministic
//! `HolePunchMachine`, the `MGR1` self-host relay framing and its
//! token-bucket rate limiter — and the EULA/consent gates that keep
//! offline auth LAN-only and port mappings explicit-publish-only.

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::rc::Rc;
use std::time::{Duration, Instant};

use megu_proto::friend_session::*;

const NOW: u64 = 1_800_000_000;

fn sock4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(a, b, c, d), port))
}

/// Test-side Crockford Base32 encoder over the public alphabet: proves
/// parsing accepts exactly what the documented alphabet produces.
fn render_word(word: u64) -> String {
    let mut s = String::from("MEGU-");
    for i in 0..12u64 {
        let d = ((word >> (55 - i * 5)) & 0x1F) as usize;
        s.push(INVITE_CODE_ALPHABET[d] as char);
        if i == 3 || i == 7 {
            s.push('-');
        }
    }
    s
}

fn issue() -> InviteCode {
    InviteCode::issue(
        FriendTransportMode::HolePunch,
        42,
        0x2_A345,
        NOW,
        4 * 3600,
    )
    .unwrap()
}

// ---------- checksums ----------

#[test]
fn crc16_ccitt_check_vector() {
    // CRC-16/CCITT-FALSE("123456789") = 0x29B1.
    assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
}

#[test]
fn crc32_ieee_check_vector() {
    // CRC-32/ISO-HDLC("123456789") = 0xCBF43926.
    assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
}

// ---------- invite code ----------

#[test]
fn invite_renders_expected_shape() {
    let code = issue();
    let text = code.render();
    assert!(text.starts_with("MEGU-"));
    let digits: String = text.chars().filter(|c| *c != '-').skip(4).collect();
    assert_eq!(digits.len(), 12);
    assert!(digits
        .bytes()
        .all(|b| INVITE_CODE_ALPHABET.contains(&b)));
}

#[test]
fn invite_roundtrip() {
    for (mode, sid, tok) in [
        (FriendTransportMode::Direct, 0u16, 0u32),
        (FriendTransportMode::HolePunch, 42, 0x2_A345),
        (FriendTransportMode::SelfHostedRelay, 1023, 262_143),
    ] {
        let code = InviteCode::issue(mode, sid, tok, NOW, 7200).unwrap();
        let parsed = InviteCode::parse(&code.render(), NOW).unwrap();
        assert_eq!(parsed.transport, mode);
        assert_eq!(parsed.session_id, sid);
        assert_eq!(parsed.join_token, tok);
        assert_eq!(parsed.expiry_slot(), code.expiry_slot());
        assert_eq!(parsed.expires_at_unix_secs, code.expires_at_unix_secs);
    }
}

#[test]
fn invite_parse_tolerates_surface_variants() {
    let text = issue().render();
    let lower = text.to_lowercase();
    let no_hyphens: String = text.chars().filter(|c| *c != '-').collect();
    let no_prefix: String = text.chars().skip(5).collect();
    for variant in [&lower, &no_hyphens, &no_prefix] {
        let p = InviteCode::parse(variant, NOW).unwrap();
        assert_eq!(p.session_id, 42);
        assert_eq!(p.join_token, 0x2_A345);
    }
}

#[test]
fn invite_parse_misread_chars() {
    // Build a code whose digits include 0/1 then respell them o/i/l.
    let code = InviteCode::issue(FriendTransportMode::Direct, 0, 0, NOW, 3600).unwrap();
    let text = code.render();
    assert!(InviteCode::parse(&text, NOW).is_ok());
    let respelled = text.replace('0', "O").replace('1', "I");
    assert!(InviteCode::parse(&respelled, NOW).is_ok());
}

#[test]
fn invite_single_digit_typo_fails_checksum() {
    let text = issue().render();
    let mut chars: Vec<char> = text.chars().collect();
    // Flip the first payload digit to a different alphabet character.
    let idx = 5;
    let alt = if chars[idx] == '0' { '1' } else { '0' };
    chars[idx] = alt;
    let tampered: String = chars.iter().collect();
    assert_eq!(
        InviteCode::parse(&tampered, NOW),
        Err(InviteError::BadChecksum)
    );
}

#[test]
fn invite_tampered_token_fails_checksum() {
    // Forge a code: take a valid word, alter token bits, keep old CRC.
    let text = issue().render();
    let word = decode_invite_code(&text).unwrap();
    let forged = render_word(word ^ 0x1_0000); // flip a join-token bit
    assert_eq!(
        InviteCode::parse(&forged, NOW),
        Err(InviteError::BadChecksum)
    );
}

#[test]
fn invite_expired_fails_closed() {
    let code = InviteCode::issue(FriendTransportMode::Direct, 7, 99, NOW, 3600).unwrap();
    let text = code.render();
    // Three slots (~96 min) past a ~2-slot TTL: unambiguously expired.
    assert_eq!(
        InviteCode::parse(&text, NOW + 3 * INVITE_EXPIRY_SLOT_SECS),
        Err(InviteError::Expired)
    );
}

#[test]
fn invite_ttl_out_of_range_rejected_at_issue() {
    assert_eq!(
        InviteCode::issue(FriendTransportMode::Direct, 1, 1, NOW, 0),
        Err(InviteError::TtlOutOfRange)
    );
    assert_eq!(
        InviteCode::issue(
            FriendTransportMode::Direct,
            1,
            1,
            NOW,
            MAX_INVITE_TTL_SECS + INVITE_EXPIRY_SLOT_SECS,
        ),
        Err(InviteError::TtlOutOfRange)
    );
}

#[test]
fn invite_field_out_of_range_rejected() {
    assert_eq!(
        InviteCode::issue(FriendTransportMode::Direct, 1024, 1, NOW, 3600),
        Err(InviteError::FieldOutOfRange("session_id"))
    );
    assert_eq!(
        InviteCode::issue(FriendTransportMode::Direct, 1, 262_144, NOW, 3600),
        Err(InviteError::FieldOutOfRange("join_token"))
    );
}

#[test]
fn invite_reserved_mode_and_bad_version_fail() {
    let good = issue().render();
    let word = decode_invite_code(&good).unwrap();
    let payload = word >> 16;
    // Reserved mode tag = 3.
    let bad_mode_payload = (payload & !(0x3u64 << 38)) | (0x3u64 << 38);
    let bad_mode_word = (bad_mode_payload << 16)
        | u64::from(crc16_ccitt(&bad_mode_payload.to_be_bytes()));
    assert_eq!(
        InviteCode::parse(&render_word(bad_mode_word), NOW),
        Err(InviteError::ReservedMode)
    );
    // Version 9 with a valid checksum.
    let bad_ver_payload = (0x9u64 << 40) | (payload & ((1u64 << 40) - 1));
    let bad_ver_word =
        (bad_ver_payload << 16) | u64::from(crc16_ccitt(&bad_ver_payload.to_be_bytes()));
    assert_eq!(
        InviteCode::parse(&render_word(bad_ver_word), NOW),
        Err(InviteError::UnsupportedVersion(9))
    );
}

#[test]
fn invite_bad_length_and_character() {
    assert_eq!(
        InviteCode::parse("MEGU-1234-5678", NOW),
        Err(InviteError::BadLength)
    );
    assert_eq!(
        InviteCode::parse("MEGU-1234-5678-90AB-extra", NOW),
        Err(InviteError::BadLength)
    );
    // 'U' outside the prefix is not a Crockford digit.
    assert_eq!(
        InviteCode::parse("MEGU-1234-5678-U0AB", NOW),
        Err(InviteError::BadCharacter(b'U'))
    );
    assert_eq!(
        InviteCode::parse("MEGU-1234-5678-90A\u{203}", NOW),
        Err(InviteError::BadCharacter(0xC8))
    );
}

#[test]
fn invite_issue_with_rng_stays_in_budget() {
    let mut rng = megu_core::random::Xoroshiro128pp::new(0x5EED);
    for _ in 0..64 {
        let code =
            InviteCode::issue_with_rng(&mut rng, FriendTransportMode::Direct, NOW, 3600).unwrap();
        assert!(code.session_id <= MAX_INVITE_SESSION_ID);
        assert!(code.join_token <= MAX_INVITE_JOIN_TOKEN);
        assert!(InviteCode::parse(&code.render(), NOW).is_ok());
    }
}

// ---------- publish policy & consent ----------

#[test]
fn policy_defaults_require_online_auth() {
    let policy = FriendPublishPolicy::default();
    assert!(policy.require_online_auth);
    assert!(policy.validate().is_ok());
}

#[test]
fn offline_auth_only_legal_on_lan() {
    let lan = FriendPublishPolicy {
        scope: PublishScope::LanOnly,
        require_online_auth: false,
    };
    assert!(lan.validate().is_ok());
    let public = FriendPublishPolicy {
        scope: PublishScope::PublicInternet,
        require_online_auth: false,
    };
    assert_eq!(
        public.validate(),
        Err(PolicyViolation::OfflineAuthRequiresLan)
    );
}

#[test]
fn cancelled_consent_publishes_nothing() {
    let policy = FriendPublishPolicy::default();
    assert_eq!(
        plan_publish(&policy, PublishConsent::Cancelled),
        Err(PublishError::ConsentDenied)
    );
}

#[test]
fn lan_share_never_maps_ports() {
    // Even with a PublicInternet policy, a ShareLanOnly click yields a
    // single LAN step — no AcquireNatMap anywhere in the plan.
    let policy = FriendPublishPolicy::default();
    let steps = plan_publish(&policy, PublishConsent::ShareLanOnly).unwrap();
    assert_eq!(steps, vec![PublishStep::LanDiscoveryOnly]);
    assert!(!steps.contains(&PublishStep::AcquireNatMap));
}

#[test]
fn publish_plan_orders_mapping_punch_relay() {
    let policy = FriendPublishPolicy::default();
    let steps = plan_publish(
        &policy,
        PublishConsent::PublishToFriends {
            allow_port_mapping: true,
        },
    )
    .unwrap();
    assert_eq!(
        steps,
        vec![
            PublishStep::AcquireNatMap,
            PublishStep::ProbeStunAndPunch,
            PublishStep::OpenRelayTunnel
        ]
    );
}

#[test]
fn publish_without_mapping_consent_skips_nat() {
    let policy = FriendPublishPolicy::default();
    let steps = plan_publish(
        &policy,
        PublishConsent::PublishToFriends {
            allow_port_mapping: false,
        },
    )
    .unwrap();
    assert_eq!(
        steps,
        vec![PublishStep::ProbeStunAndPunch, PublishStep::OpenRelayTunnel]
    );
}

#[test]
fn invalid_policy_blocks_publish_plan() {
    let policy = FriendPublishPolicy {
        scope: PublishScope::PublicInternet,
        require_online_auth: false,
    };
    assert_eq!(
        plan_publish(
            &policy,
            PublishConsent::PublishToFriends {
                allow_port_mapping: true
            }
        ),
        Err(PublishError::Policy(
            PolicyViolation::OfflineAuthRequiresLan
        ))
    );
}

// ---------- NAT-PMP ----------

#[test]
fn natpmp_external_address_request_bytes() {
    assert_eq!(
        encode_natpmp_request(&NatPmpRequest::ExternalAddress),
        vec![0, 0]
    );
}

#[test]
fn natpmp_map_request_bytes() {
    let req = NatPmpRequest::MapPort(NatMapRequest {
        protocol: NatProtocol::Tcp,
        internal_port: 25565,
        suggested_external_port: 40000,
        lifetime_secs: 3600,
    });
    let bytes = encode_natpmp_request(&req);
    assert_eq!(
        bytes,
        vec![0, 2, 0, 0, 0x63, 0xDD, 0x9C, 0x40, 0, 0, 0x0E, 0x10]
    );
}

#[test]
fn natpmp_decode_external_address_response() {
    let bytes = [0, 128, 0, 0, 0, 0, 0, 7, 203, 0, 113, 9];
    assert_eq!(
        decode_natpmp_response(&bytes),
        Ok(NatPmpResponse::ExternalAddress {
            epoch_secs: 7,
            address: Ipv4Addr::new(203, 0, 113, 9)
        })
    );
}

#[test]
fn natpmp_decode_map_response() {
    let bytes = [
        0, 129, 0, 0, 0, 0, 0, 9, 0x63, 0xDD, 0x9C, 0x40, 0, 0, 0x0E, 0x10,
    ];
    assert_eq!(
        decode_natpmp_response(&bytes),
        Ok(NatPmpResponse::Mapped {
            epoch_secs: 9,
            protocol: NatProtocol::Udp,
            internal_port: 25565,
            assigned_external_port: 40000,
            lifetime_secs: 3600
        })
    );
}

#[test]
fn natpmp_gateway_error_fails_closed() {
    // result code 3 = "Failure processing request".
    let bytes = [0, 129, 0, 3, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(decode_natpmp_response(&bytes), Err(NatMapError::Gateway(3)));
}

#[test]
fn natpmp_rejects_requests_and_garbage() {
    assert_eq!(
        decode_natpmp_response(&[0, 0]),
        Err(NatMapError::Truncated)
    );
    let mut req = [0u8; 16];
    req[1] = 1; // request opcode, not a response
    assert_eq!(
        decode_natpmp_response(&req),
        Err(NatMapError::NotAResponse)
    );
    req[1] = 0x89; // 0x80 | 9: no such opcode
    assert_eq!(decode_natpmp_response(&req), Err(NatMapError::BadOpcode(0x89)));
    req[1] = 0x80;
    req[0] = 1; // version != 0
    assert_eq!(decode_natpmp_response(&req), Err(NatMapError::BadVersion(1)));
}

// ---------- PCP ----------

#[test]
fn pcp_map_request_wire_layout() {
    let nonce = [0xA5; 12];
    let req = NatMapRequest {
        protocol: NatProtocol::Tcp,
        internal_port: 25565,
        suggested_external_port: 40000,
        lifetime_secs: 7200,
    };
    let bytes = encode_pcp_map_request(
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
        nonce,
        &req,
        None,
    );
    assert_eq!(bytes.len(), 60);
    assert_eq!(bytes[0], 0x04); // version 2 << 1 | R=0
    assert_eq!(bytes[1], 0x01); // MAP opcode
    assert_eq!(&bytes[2..4], &[0, 0]); // reserved
    assert_eq!(&bytes[4..8], &7200u32.to_be_bytes());
    // client IPv4 as v4-mapped v6.
    assert_eq!(&bytes[8..22], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 192, 168]);
    assert_eq!(&bytes[24..36], &nonce);
    assert_eq!(bytes[36], 6); // TCP
    assert_eq!(&bytes[40..42], &25565u16.to_be_bytes());
    assert_eq!(&bytes[42..44], &40000u16.to_be_bytes());
    // suggested external address = all zeros.
    assert!(bytes[44..60].iter().all(|b| *b == 0));
}

fn pcp_map_response(nonce: [u8; 12], result: u8) -> Vec<u8> {
    let mut b = vec![0x05, 0x01, 0x00, result];
    b.extend_from_slice(&3600u32.to_be_bytes()); // lifetime
    b.extend_from_slice(&11u32.to_be_bytes()); // epoch
    b.extend_from_slice(&[0u8; 12]); // reserved
    b.extend_from_slice(&nonce);
    b.push(6); // TCP
    b.extend_from_slice(&[0, 0, 0]);
    b.extend_from_slice(&25565u16.to_be_bytes());
    b.extend_from_slice(&40000u16.to_be_bytes());
    // assigned external = v4-mapped 203.0.113.99
    let mut addr = [0u8; 16];
    addr[10] = 0xFF;
    addr[11] = 0xFF;
    addr[12..16].copy_from_slice(&[203, 0, 113, 99]);
    b.extend_from_slice(&addr);
    b
}

#[test]
fn pcp_map_response_decodes() {
    let nonce = [0x11; 12];
    let bytes = pcp_map_response(nonce, 0);
    let resp = decode_pcp_map_response(&bytes, &nonce).unwrap();
    assert_eq!(resp.epoch_secs, 11);
    assert_eq!(resp.lifetime_secs, 3600);
    assert_eq!(resp.protocol, 6);
    assert_eq!(resp.internal_port, 25565);
    assert_eq!(resp.assigned_external_port, 40000);
    assert_eq!(
        resp.assigned_external_addr,
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 99))
    );
}

#[test]
fn pcp_nonce_mismatch_fails_closed() {
    let bytes = pcp_map_response([0x22; 12], 0);
    assert_eq!(
        decode_pcp_map_response(&bytes, &[0x11; 12]),
        Err(NatMapError::NonceMismatch)
    );
}

#[test]
fn pcp_error_result_fails_closed() {
    let nonce = [0x11; 12];
    let bytes = pcp_map_response(nonce, 8); // NO_RESOURCES
    assert_eq!(
        decode_pcp_map_response(&bytes, &nonce),
        Err(NatMapError::Gateway(8))
    );
}

#[test]
fn pcp_rejects_requests_and_wrong_version() {
    let nonce = [0x11; 12];
    let mut bytes = pcp_map_response(nonce, 0);
    bytes[0] = 0x04; // R=0: a request
    assert_eq!(
        decode_pcp_map_response(&bytes, &nonce),
        Err(NatMapError::NotAResponse)
    );
    bytes[0] = (1 << 1) | 1; // version 1
    assert_eq!(
        decode_pcp_map_response(&bytes, &nonce),
        Err(NatMapError::BadVersion(1))
    );
    bytes[0] = 0x05;
    bytes[1] = 0x07; // not MAP
    assert_eq!(
        decode_pcp_map_response(&bytes, &nonce),
        Err(NatMapError::BadOpcode(7))
    );
    assert_eq!(
        decode_pcp_map_response(&bytes[..30], &nonce),
        Err(NatMapError::Truncated)
    );
}

// ---------- UPnP ----------

#[test]
fn ssdp_msearch_request_shape() {
    let req = ssdp_msearch_request(UPNP_IGD_DEVICE).unwrap();
    let text = String::from_utf8(req).unwrap();
    assert!(text.starts_with("M-SEARCH * HTTP/1.1\r\n"));
    assert!(text.contains("HOST: 239.255.255.250:1900\r\n"));
    assert!(text.contains("MAN: \"ssdp:discover\"\r\n"));
    assert!(text.contains("ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n"));
}

#[test]
fn ssdp_msearch_rejects_injection() {
    assert!(ssdp_msearch_request("x\r\nEvil: 1").is_err());
    assert!(ssdp_msearch_request("").is_err());
    assert!(ssdp_msearch_request(&"x".repeat(200)).is_err());
}

#[test]
fn ssdp_response_parses_headers() {
    let resp = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nUSN: uuid:abc::urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nSERVER: router/1.0 UPnP/1.0 miniupnpd/2\r\nLocation: http://192.168.1.1:5000/rootDesc.xml\r\n\r\n";
    let dev = parse_ssdp_response(resp).unwrap();
    assert_eq!(dev.location, "http://192.168.1.1:5000/rootDesc.xml");
    assert!(dev.st.contains("InternetGatewayDevice"));
    assert!(dev.usn.starts_with("uuid:abc"));
}

#[test]
fn ssdp_response_fails_closed() {
    assert!(parse_ssdp_response(b"NOTIFY * HTTP/1.1\r\n\r\n").is_err());
    assert!(parse_ssdp_response(b"HTTP/1.1 404 NF\r\n\r\n").is_err());
    // Missing LOCATION.
    assert_eq!(
        parse_ssdp_response(b"HTTP/1.1 200 OK\r\nST: x\r\n\r\n"),
        Err(UpnpError::Missing("LOCATION"))
    );
    assert_eq!(
        parse_ssdp_response(&vec![b'x'; 9000]),
        Err(UpnpError::Oversize)
    );
}

#[test]
fn soap_add_body_has_all_fields() {
    let body = soap_add_port_mapping_body(
        UPNP_WANIP_SERVICE,
        40000,
        NatProtocol::Tcp,
        25565,
        Ipv4Addr::new(192, 168, 1, 50),
        "megu <world> & friends",
        3600,
    )
    .unwrap();
    for needle in [
        "AddPortMapping",
        "urn:schemas-upnp-org:service:WANIPConnection:1",
        "<NewExternalPort>40000</NewExternalPort>",
        "<NewProtocol>TCP</NewProtocol>",
        "<NewInternalPort>25565</NewInternalPort>",
        "<NewInternalClient>192.168.1.50</NewInternalClient>",
        "<NewEnabled>1</NewEnabled>",
        "megu &lt;world&gt; &amp; friends",
        "<NewLeaseDuration>3600</NewLeaseDuration>",
    ] {
        assert!(body.contains(needle), "missing {needle}");
    }
}

#[test]
fn soap_delete_body_has_minimal_fields() {
    let body = soap_delete_port_mapping_body(UPNP_WANIP_SERVICE, 40000, NatProtocol::Udp).unwrap();
    assert!(body.contains("DeletePortMapping"));
    assert!(body.contains("<NewExternalPort>40000</NewExternalPort>"));
    assert!(body.contains("<NewProtocol>UDP</NewProtocol>"));
    assert!(!body.contains("NewInternalPort"));
}

#[test]
fn upnp_post_request_shape() {
    let body = soap_delete_port_mapping_body(UPNP_WANIP_SERVICE, 40000, NatProtocol::Udp).unwrap();
    let req = upnp_http_post_request(
        "192.168.1.1:5000",
        "/ctl/IPConn",
        UPNP_WANIP_SERVICE,
        "DeletePortMapping",
        &body,
    )
    .unwrap();
    let text = String::from_utf8(req).unwrap();
    assert!(text.starts_with("POST /ctl/IPConn HTTP/1.1\r\n"));
    assert!(text.contains("SOAPACTION: \"urn:schemas-upnp-org:service:WANIPConnection:1#DeletePortMapping\""));
    assert!(text.contains(&format!("CONTENT-LENGTH: {}", body.len())));
}

#[test]
fn upnp_action_response_success_is_ok() {
    let ok = b"<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:AddPortMappingResponse xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\"/></s:Body></s:Envelope>";
    assert!(parse_upnp_action_response(ok).is_ok());
}

#[test]
fn upnp_action_response_fault_yields_code() {
    let fault = b"<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError><errorCode>718</errorCode><errorDescription>ConflictInMappingEntry</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";
    assert_eq!(
        parse_upnp_action_response(fault),
        Err(UpnpError::Soap { code: 718 })
    );
}

#[test]
fn upnp_xml_refuses_doctype() {
    let evil = b"<?xml version=\"1.0\"?><!DOCTYPE x [<!ENTITY e SYSTEM \"file:///etc/passwd\">]><x>&e;</x>";
    assert_eq!(
        parse_upnp_action_response(evil),
        Err(UpnpError::ForbiddenDoctype)
    );
    assert_eq!(
        parse_device_description_control_url(evil, UPNP_WANIP_SERVICE),
        Err(UpnpError::ForbiddenDoctype)
    );
}

#[test]
fn device_description_finds_control_url() {
    let xml = b"<root><device><serviceList>\
        <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
        <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>\
        <controlURL>/ctl/IPConn</controlURL></service>\
        <service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>\
        <controlURL>/ctl/PPPConn</controlURL></service>\
        </serviceList></device></root>";
    assert_eq!(
        parse_device_description_control_url(xml, UPNP_WANIP_SERVICE).unwrap(),
        "/ctl/IPConn"
    );
    assert_eq!(
        parse_device_description_control_url(xml, UPNP_WANPPP_SERVICE).unwrap(),
        "/ctl/PPPConn"
    );
    assert_eq!(
        parse_device_description_control_url(xml, "urn:nope"),
        Err(UpnpError::Missing("serviceType"))
    );
}

// ---------- HTTP/SOAP injection hardening ----------

#[test]
fn post_request_rejects_crlf_in_every_field() {
    let body = "<x/>";
    for (host, path, service, action) in [
        ("192.168.1.1\r\nX: 1", "/ctl", "urn:a", "Act"),
        ("192.168.1.1:5000", "/ctl\r\nX: 1", "urn:a", "Act"),
        ("192.168.1.1:5000", "/ctl", "urn:a\r\nX: 1", "Act"),
        ("192.168.1.1:5000", "/ctl", "urn:a", "Act\r\nX: 1"),
        ("192.168.1.1\nGET /x HTTP/1.1", "/ctl", "urn:a", "Act"),
        ("192.168.1.1:5000", "/ctl\n\nGET / HTTP/1.1", "urn:a", "Act"),
    ] {
        assert!(
            upnp_http_post_request(host, path, service, action, body).is_err(),
            "accepted injected field: {host:?} {path:?} {service:?} {action:?}"
        );
    }
}

#[test]
fn post_request_rejects_control_and_space_bytes() {
    let body = "<x/>";
    for bad in ["h\u{0}ost", "h ost", "h\u{7F}ost", "h\u{80}ost"] {
        assert_eq!(
            upnp_http_post_request(bad, "/ctl", "urn:a", "Act", body),
            Err(UpnpError::InvalidToken("host"))
        );
    }
    for bad in ["/c\u{0}tl", "/c tl", "/ctl\u{9}", "/ctl\u{1F}"] {
        assert_eq!(
            upnp_http_post_request("192.168.1.1", bad, "urn:a", "Act", body),
            Err(UpnpError::InvalidToken("control_path"))
        );
    }
    // Quote/angle escapes out of the SOAPACTION header value.
    for bad in ["urn:a\"#x", "urn:<a", "urn:a>"] {
        assert_eq!(
            upnp_http_post_request("192.168.1.1", "/ctl", bad, "Act", body),
            Err(UpnpError::InvalidToken("service"))
        );
        assert_eq!(
            upnp_http_post_request("192.168.1.1", "/ctl", "urn:a", bad, body),
            Err(UpnpError::InvalidToken("action"))
        );
    }
}

#[test]
fn post_request_rejects_non_origin_target() {
    // Absolute-URI or authority-looking targets are refused: the request
    // line only ever carries origin-form `/path[?query]`.
    for bad in [
        "ctl/IPConn",
        "http://evil.example/x",
        "//evil.example/x",
        "*",
        "\\\\unc\\share",
        "",
    ] {
        assert_eq!(
            upnp_http_post_request("192.168.1.1", bad, "urn:a", "Act", "<x/>"),
            Err(UpnpError::InvalidToken("control_path")),
            "accepted target {bad:?}"
        );
    }
}

#[test]
fn resolve_control_target_accepts_safe_forms() {
    let loc = "http://192.168.1.1:5000/rootDesc.xml";
    // Root-relative.
    let t = resolve_upnp_control_target("/ctl/IPConn", loc).unwrap();
    assert_eq!(t.host, "192.168.1.1:5000");
    assert_eq!(t.path, "/ctl/IPConn");
    // Base-relative.
    let t = resolve_upnp_control_target("ctl/IPConn", loc).unwrap();
    assert_eq!(t.path, "/ctl/IPConn");
    // Absolute http(s) URL keeps its own clean origin.
    let t = resolve_upnp_control_target("https://192.168.1.1:8443/c?x=1", loc).unwrap();
    assert_eq!(t.host, "192.168.1.1:8443");
    assert_eq!(t.path, "/c?x=1");
    // IPv6 authority.
    let t = resolve_upnp_control_target("http://[fd00::1]:5000/ctl", "http://[fd00::1]:5000/d.xml").unwrap();
    assert_eq!(t.host, "[fd00::1]:5000");
}

#[test]
fn resolve_control_target_rejects_injection() {
    let loc = "http://192.168.1.1:5000/rootDesc.xml";
    for bad in [
        "/ctl\r\nX-Injected: 1",
        "/ctl IPConn",
        "/ctl\u{0}x",
        "http://ok\r\nX: 1/x",
        "http://user@evil.example/x",      // userinfo smuggle
        "http://evil.example/a b",          // space in path
        "file:///etc/passwd",
        "gopher://evil.example/x",
        "ftp://evil.example/x",
        "javascript:alert(1)",
        "HTTP://evil.example/",             // uppercase scheme: not http
        "",
        "   ",
    ] {
        assert!(
            resolve_upnp_control_target(bad, loc).is_err(),
            "accepted {bad:?}"
        );
    }
    // A hostile LOCATION can never launder a relative controlURL.
    assert!(resolve_upnp_control_target("/ctl", "file:///etc/passwd").is_err());
    assert!(resolve_upnp_control_target("/ctl", "http://evil\r\nX:1/d").is_err());
    assert!(resolve_upnp_control_target("/ctl", "not-a-url").is_err());
}

#[test]
fn soap_body_builders_reject_unsafe_service() {
    for bad in ["urn:a\"xmlns:b=\"x\"", "urn:<x>", "urn:a\nb"] {
        assert!(
            soap_add_port_mapping_body(
                bad,
                40000,
                NatProtocol::Tcp,
                25565,
                Ipv4Addr::new(192, 168, 1, 50),
                "w",
                3600,
            )
            .is_err()
        );
        assert!(soap_delete_port_mapping_body(bad, 40000, NatProtocol::Udp).is_err());
        assert!(soap_envelope(bad, "Act", "<x/>").is_err());
        assert!(soap_envelope("urn:a", bad, "<x/>").is_err());
    }
}

#[test]
fn malicious_igd_control_url_cannot_smuggle() {
    // An IGD description whose controlURL carries a second request: the
    // extracted URL is handed to the resolver, which refuses it before
    // any request bytes exist.
    let xml = b"<root><device><serviceList>\
        <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
        <controlURL>/ctl/IPConn&#13;&#10;POST /admin HTTP/1.1</controlURL></service>\
        </serviceList></device></root>";
    let url = parse_device_description_control_url(xml, UPNP_WANIP_SERVICE).unwrap();
    assert!(resolve_upnp_control_target(
        &url,
        "http://192.168.1.1:5000/rootDesc.xml"
    )
    .is_err());
}

// ---------- PortLeaseGuard ----------

fn lease() -> PortLease {
    PortLease {
        protocol: NatProtocol::Tcp,
        internal_port: 25565,
        external_port: 40000,
        external_addr: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))),
        granted_lifetime_secs: 3600,
    }
}

#[test]
fn port_lease_guard_unmaps_on_drop() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    {
        let calls2 = Rc::clone(&calls);
        let _guard = PortLeaseGuard::new(lease(), move |l| calls2.borrow_mut().push(l.clone()));
    }
    let calls = calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].external_port, 40000);
}

#[test]
fn port_lease_guard_close_unmaps_exactly_once() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    {
        let calls2 = Rc::clone(&calls);
        let guard = PortLeaseGuard::new(lease(), move |l| calls2.borrow_mut().push(l.clone()));
        let closed = guard.close();
        assert_eq!(closed.external_port, 40000);
    }
    // Drop after close must NOT re-run the hook.
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn port_lease_guard_disarmed_state() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let calls2 = Rc::clone(&calls);
    let guard = PortLeaseGuard::new(lease(), move |l| calls2.borrow_mut().push(l.clone()));
    let dbg = format!("{guard:?}");
    assert!(dbg.contains("armed: true"));
    drop(guard);
    assert_eq!(calls.borrow().len(), 1);
}

// ---------- STUN ----------

#[test]
fn stun_binding_request_layout() {
    let txid = [0x7Au8; 12];
    let req = encode_stun_binding_request(&txid);
    assert_eq!(&req[0..2], &0x0001u16.to_be_bytes());
    assert_eq!(&req[2..4], &0u16.to_be_bytes());
    assert_eq!(&req[4..8], &0x2112_A442u32.to_be_bytes());
    assert_eq!(&req[8..20], &txid);
}

fn stun_response(msg_type: u16, txid: &[u8; 12], attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (ty, val) in attrs {
        body.extend_from_slice(&ty.to_be_bytes());
        body.extend_from_slice(&(val.len() as u16).to_be_bytes());
        body.extend_from_slice(val);
        while body.len() % 4 != 0 {
            body.push(0);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&0x2112_A442u32.to_be_bytes());
    out.extend_from_slice(txid);
    out.extend_from_slice(&body);
    out
}

fn xor_mapped_v4(ip: [u8; 4], port: u16) -> Vec<u8> {
    let cookie = 0x2112_A442u32.to_be_bytes();
    let mut v = vec![0, 0x01];
    v.extend_from_slice(&(port ^ 0x2112).to_be_bytes());
    for (b, k) in ip.iter().zip(cookie) {
        v.push(b ^ k);
    }
    v
}

#[test]
fn stun_xor_mapped_ipv4_decodes() {
    let txid = [0x11u8; 12];
    let resp = stun_response(0x0101, &txid, &[(0x0020, xor_mapped_v4([203, 0, 113, 55], 54321))]);
    let binding = decode_stun_binding_response(&resp, &txid).unwrap();
    assert_eq!(binding.mapped, sock4(203, 0, 113, 55, 54321));
    assert!(binding.via_xor);
}

#[test]
fn stun_xor_mapped_ipv6_decodes() {
    let txid = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x10, 0x32, 0x54, 0x76,
    ];
    let ip: [u8; 16] = [
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x42,
    ];
    let mut key = [0u8; 16];
    key[..4].copy_from_slice(&0x2112_A442u32.to_be_bytes());
    key[4..].copy_from_slice(&txid);
    let mut v = vec![0, 0x02];
    v.extend_from_slice(&(8080u16 ^ 0x2112).to_be_bytes());
    for (b, k) in ip.iter().zip(key) {
        v.push(b ^ k);
    }
    let resp = stun_response(0x0101, &txid, &[(0x0020, v)]);
    let binding = decode_stun_binding_response(&resp, &txid).unwrap();
    assert_eq!(binding.mapped, SocketAddr::from((Ipv6Addr::from(ip), 8080)));
}

#[test]
fn stun_plain_mapped_address_fallback() {
    let txid = [0x99u8; 12];
    let mut val = vec![0, 0x01];
    val.extend_from_slice(&25565u16.to_be_bytes());
    val.extend_from_slice(&[198, 51, 100, 4]);
    let resp = stun_response(0x0101, &txid, &[(0x0001, val)]);
    let binding = decode_stun_binding_response(&resp, &txid).unwrap();
    assert_eq!(binding.mapped, sock4(198, 51, 100, 4, 25565));
    assert!(!binding.via_xor);
}

#[test]
fn stun_verifies_cookie_and_txid() {
    let txid = [0x11u8; 12];
    let resp = stun_response(0x0101, &txid, &[(0x0020, xor_mapped_v4([1, 2, 3, 4], 9))]);
    let mut wrong_txid = txid;
    wrong_txid[0] ^= 0xFF;
    assert_eq!(
        decode_stun_binding_response(&resp, &wrong_txid),
        Err(StunError::TxIdMismatch)
    );
    let mut bad_cookie = resp.clone();
    bad_cookie[4] ^= 0xFF;
    assert_eq!(
        decode_stun_binding_response(&bad_cookie, &txid),
        Err(StunError::BadCookie)
    );
    assert_eq!(
        decode_stun_binding_response(&resp[..10], &txid),
        Err(StunError::Truncated)
    );
}

#[test]
fn stun_error_response_surfaces_code() {
    let txid = [0x33u8; 12];
    let mut err = vec![0u8, 0, 4, 1]; // class 4, number 01 -> 401
    err.extend_from_slice(b"Unauthorized");
    let resp = stun_response(0x0111, &txid, &[(0x0009, err)]);
    assert_eq!(
        decode_stun_binding_response(&resp, &txid),
        Err(StunError::Server { code: 401 })
    );
}

#[test]
fn stun_missing_mapped_address_fails() {
    let txid = [0x44u8; 12];
    let resp = stun_response(0x0101, &txid, &[(0x8022, vec![0; 4])]);
    assert_eq!(
        decode_stun_binding_response(&resp, &txid),
        Err(StunError::NoMappedAddress)
    );
}

#[test]
fn stun_truncated_attribute_fails() {
    let txid = [0x55u8; 12];
    let mut resp = stun_response(0x0101, &txid, &[(0x0020, xor_mapped_v4([1, 2, 3, 4], 9))]);
    // Declare 24 body bytes but ship fewer.
    resp[3] = 24;
    resp.truncate(28);
    assert_eq!(
        decode_stun_binding_response(&resp, &txid),
        Err(StunError::Truncated)
    );
}

// ---------- hole-punch machine ----------

#[test]
fn holepunch_happy_path() {
    let mut m = HolePunchMachine::new(HolePunchConfig::default());
    assert_eq!(m.state(), HolePunchState::Idle);
    assert_eq!(m.step(HolePunchEvent::Start), vec![HolePunchAction::SendStunRequest]);
    assert_eq!(m.state(), HolePunchState::ProbingStun);

    let public = sock4(203, 0, 113, 7, 50000);
    assert_eq!(m.step(HolePunchEvent::StunBound(public)), Vec::<HolePunchAction>::new());
    assert_eq!(m.mapped_addr(), Some(public));
    assert_eq!(m.state(), HolePunchState::Punching { attempts: 0 });

    let peer = sock4(192, 0, 2, 44, 60000);
    assert_eq!(
        m.step(HolePunchEvent::PeerKnown(peer)),
        vec![HolePunchAction::SendPunchProbe(peer)]
    );
    assert_eq!(m.state(), HolePunchState::Punching { attempts: 1 });

    assert_eq!(
        m.step(HolePunchEvent::PeerReached(peer)),
        vec![HolePunchAction::ReportConnected(peer)]
    );
    assert_eq!(m.state(), HolePunchState::Connected(peer));
    // Terminal: further events are ignored.
    assert!(m.step(HolePunchEvent::PunchAttemptTimedOut).is_empty());
}

#[test]
fn holepunch_stun_retry_then_fallback_relay() {
    let cfg = HolePunchConfig {
        stun_retries: 2,
        max_punch_attempts: 4,
        relay_fallback: true,
    };
    let mut m = HolePunchMachine::new(cfg);
    m.step(HolePunchEvent::Start);
    // First timeout -> retry (attempt 2 of 2).
    assert_eq!(
        m.step(HolePunchEvent::StunTimeout),
        vec![HolePunchAction::SendStunRequest]
    );
    // Second timeout exhausts retries -> relay fallback.
    assert_eq!(
        m.step(HolePunchEvent::StunTimeout),
        vec![HolePunchAction::OpenRelayTunnel]
    );
    assert_eq!(m.state(), HolePunchState::FallbackRelay);
}

#[test]
fn holepunch_stun_exhaustion_without_fallback_fails() {
    let cfg = HolePunchConfig {
        stun_retries: 1,
        max_punch_attempts: 4,
        relay_fallback: false,
    };
    let mut m = HolePunchMachine::new(cfg);
    m.step(HolePunchEvent::Start);
    assert_eq!(
        m.step(HolePunchEvent::StunTimeout),
        vec![HolePunchAction::ReportFailed]
    );
    assert_eq!(m.state(), HolePunchState::Failed);
}

#[test]
fn holepunch_punch_attempts_then_fallback() {
    let cfg = HolePunchConfig {
        stun_retries: 3,
        max_punch_attempts: 3,
        relay_fallback: true,
    };
    let mut m = HolePunchMachine::new(cfg);
    m.step(HolePunchEvent::Start);
    m.step(HolePunchEvent::StunBound(sock4(203, 0, 113, 7, 50000)));
    let peer = sock4(192, 0, 2, 44, 60000);
    m.step(HolePunchEvent::PeerKnown(peer)); // attempt 1
    // timeout -> attempt 2
    assert_eq!(
        m.step(HolePunchEvent::PunchAttemptTimedOut),
        vec![HolePunchAction::SendPunchProbe(peer)]
    );
    assert_eq!(m.state(), HolePunchState::Punching { attempts: 2 });
    // timeout at attempt 2 -> next would be 3 == max -> fallback.
    assert_eq!(
        m.step(HolePunchEvent::PunchAttemptTimedOut),
        vec![HolePunchAction::OpenRelayTunnel]
    );
    assert_eq!(m.state(), HolePunchState::FallbackRelay);
}

#[test]
fn holepunch_timeout_before_peer_known_falls_back() {
    let cfg = HolePunchConfig {
        stun_retries: 3,
        max_punch_attempts: 8,
        relay_fallback: true,
    };
    let mut m = HolePunchMachine::new(cfg);
    m.step(HolePunchEvent::Start);
    m.step(HolePunchEvent::StunBound(sock4(203, 0, 113, 7, 50000)));
    // A punch timeout while the rendezvous has not delivered a peer
    // cannot be retried: nothing to aim at.
    assert_eq!(
        m.step(HolePunchEvent::PunchAttemptTimedOut),
        vec![HolePunchAction::OpenRelayTunnel]
    );
}

#[test]
fn holepunch_abort_is_terminal() {
    let mut m = HolePunchMachine::new(HolePunchConfig::default());
    m.step(HolePunchEvent::Start);
    assert_eq!(m.step(HolePunchEvent::Abort), vec![HolePunchAction::AbortAll]);
    assert_eq!(m.state(), HolePunchState::Failed);
    // Abort on a terminal state is a no-op.
    assert!(m.step(HolePunchEvent::Abort).is_empty());
}

#[test]
fn holepunch_ignores_events_out_of_order() {
    let mut m = HolePunchMachine::new(HolePunchConfig::default());
    // PeerReached while Idle does nothing.
    assert!(m
        .step(HolePunchEvent::PeerReached(sock4(1, 2, 3, 4, 5)))
        .is_empty());
    assert_eq!(m.state(), HolePunchState::Idle);
    // StunBound while Idle does nothing.
    assert!(m
        .step(HolePunchEvent::StunBound(sock4(1, 2, 3, 4, 5)))
        .is_empty());
}

// ---------- MGR1 relay framing ----------

fn rt(frame: RelayFrame) -> RelayFrame {
    let bytes = encode_relay_frame(&frame).unwrap();
    assert!(bytes.len() <= MAX_RELAY_FRAME_BYTES);
    decode_relay_frame(&bytes).unwrap()
}

#[test]
fn relay_frame_roundtrips() {
    let frames = vec![
        RelayFrame::RegisterHost {
            session_id: 0x1122_3344_5566_7788,
            join_token: 0xDEAD_BEEF_CAFE_F00D,
            max_guests: 8,
            world_name: "Steve's World".into(),
        },
        RelayFrame::JoinByToken {
            session_id: 7,
            join_token: 99,
            guest_name: "Alex".into(),
        },
        RelayFrame::DataChunk {
            stream_id: 42,
            payload: b"hello game bytes".to_vec(),
        },
        RelayFrame::Heartbeat { nonce: u64::MAX },
        RelayFrame::HeartbeatAck { nonce: 1 },
        RelayFrame::Revoke {
            session_id: 5,
            join_token: 6,
        },
        RelayFrame::RevokeAck { accepted: true },
        RelayFrame::RegisterAck { session_id: 77 },
        RelayFrame::JoinAck { stream_id: 0xFFFF_FFFF },
        RelayFrame::JoinReject { reason: 2 },
    ];
    for f in frames {
        assert_eq!(rt(f.clone()), f);
    }
}

#[test]
fn relay_frame_envelope_layout() {
    let bytes = encode_relay_frame(&RelayFrame::Heartbeat { nonce: 7 }).unwrap();
    let declared = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    assert_eq!(declared, bytes.len());
    assert_eq!(&bytes[4..8], b"MGR1");
    assert_eq!(bytes[8], 0x04);
}

#[test]
fn relay_frame_rejects_bad_magic_tag_and_length() {
    let mut bytes = encode_relay_frame(&RelayFrame::Heartbeat { nonce: 7 }).unwrap();
    bytes[4] = b'X';
    assert_eq!(decode_relay_frame(&bytes), Err(RelayError::BadMagic));

    let mut bytes = encode_relay_frame(&RelayFrame::Heartbeat { nonce: 7 }).unwrap();
    bytes[8] = 0x7F;
    assert_eq!(decode_relay_frame(&bytes), Err(RelayError::BadTag(0x7F)));

    let bytes = encode_relay_frame(&RelayFrame::Heartbeat { nonce: 7 }).unwrap();
    // Declared length smaller than actual -> trailing bytes -> BadLength.
    let mut short = bytes.clone();
    short[3] = (bytes.len() - 1) as u8;
    assert_eq!(decode_relay_frame(&short), Err(RelayError::BadLength));
    assert_eq!(decode_relay_frame(&bytes[..6]), Err(RelayError::BadLength));
    assert_eq!(decode_relay_frame(&bytes[..2]), Err(RelayError::Truncated));
}

#[test]
fn relay_frame_truncated_body_fails() {
    let bytes = encode_relay_frame(&RelayFrame::Heartbeat { nonce: 7 }).unwrap();
    // Chop two bytes but keep the declared length by lying in the prefix.
    let mut forged = bytes[..bytes.len() - 2].to_vec();
    assert_eq!(decode_relay_frame(&forged), Err(RelayError::BadLength));
    // Claim a smaller length that still leaves a truncated u64.
    forged[3] = (forged.len() - 5) as u8;
    // declared != actual still -> BadLength
    assert_eq!(decode_relay_frame(&forged), Err(RelayError::BadLength));
}

#[test]
fn relay_name_length_enforced() {
    let too_long = "x".repeat(MAX_RELAY_NAME_BYTES + 1);
    assert_eq!(
        encode_relay_frame(&RelayFrame::RegisterHost {
            session_id: 1,
            join_token: 2,
            max_guests: 4,
            world_name: too_long.clone(),
        }),
        Err(RelayError::NameTooLong)
    );
    // A forged frame lying about name length: body says 200-byte name.
    let mut body = b"MGR1".to_vec();
    body.push(0x02);
    body.extend_from_slice(&[0u8; 16]);
    body.extend_from_slice(&200u16.to_be_bytes());
    body.extend_from_slice(b"short");
    let mut frame = Vec::new();
    frame.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    assert_eq!(decode_relay_frame(&frame), Err(RelayError::NameTooLong));
}

#[test]
fn relay_data_chunk_crc_enforced() {
    let mut bytes = encode_relay_frame(&RelayFrame::DataChunk {
        stream_id: 1,
        payload: b"payload".to_vec(),
    })
    .unwrap();
    // Corrupt one payload byte without fixing the CRC.
    let n = bytes.len();
    bytes[n - 6] ^= 0xFF;
    assert_eq!(decode_relay_frame(&bytes), Err(RelayError::BadChunkCrc));
}

#[test]
fn relay_data_chunk_oversize_refused() {
    let big = RelayFrame::DataChunk {
        stream_id: 1,
        payload: vec![0u8; MAX_RELAY_FRAME_BYTES],
    };
    assert_eq!(encode_relay_frame(&big), Err(RelayError::Oversize));
}

#[test]
fn relay_frame_max_size_boundary() {
    // Largest payload that still fits: 4(len)+4(magic)+1(tag)+4(id)+4(len)+payload+4(crc) <= 65536
    let payload_max = MAX_RELAY_FRAME_BYTES - 4 - 4 - 1 - 4 - 4 - 4;
    let ok = RelayFrame::DataChunk {
        stream_id: 1,
        payload: vec![0xAB; payload_max],
    };
    let bytes = encode_relay_frame(&ok).unwrap();
    assert_eq!(bytes.len(), MAX_RELAY_FRAME_BYTES);
    assert_eq!(decode_relay_frame(&bytes).unwrap(), ok);
}

// ---------- token bucket / session budget ----------

#[test]
fn token_bucket_admits_then_refills() {
    let now = Instant::now();
    let mut b = TokenBucket::new(100, 50, now);
    assert_eq!(b.available(now), 100);
    assert!(b.try_consume(now, 60));
    assert_eq!(b.available(now), 40);
    assert!(!b.try_consume(now, 41));
    // One second later: +50, capped at capacity.
    let later = now + Duration::from_secs(1);
    assert_eq!(b.available(later), 90);
    let much_later = now + Duration::from_secs(100);
    assert_eq!(b.available(much_later), 100);
    // Oversized single costs are always refused.
    assert!(!b.try_consume(much_later, 101));
}

#[test]
fn token_bucket_fractional_refill_carries() {
    let now = Instant::now();
    let mut b = TokenBucket::new(10, 3, now); // 3 B/s
    assert!(b.try_consume(now, 10));
    // 400ms -> 1.2 tokens: only 1 lands now, the 0.2 carries.
    let t1 = now + Duration::from_millis(400);
    assert!(!b.try_consume(t1, 2));
    let t2 = t1 + Duration::from_millis(400); // 0.8s total -> 2.4
    assert!(b.try_consume(t2, 2));
}

#[test]
fn relay_session_budget_limits_frames_and_bytes() {
    let now = Instant::now();
    let mut budget = RelaySessionBudget::new(4, 1000, now);
    // Four frames in.
    for _ in 0..4 {
        assert!(budget.allow(now, 200));
    }
    // Fifth frame same instant: frame bucket empty even though bytes fit.
    assert!(!budget.allow(now, 200));
    // Half a second later: 2 frames + 500 bytes available.
    let later = now + Duration::from_millis(500);
    assert!(budget.allow(later, 400));
    assert!(budget.allow(later, 100));
    assert!(!budget.allow(later, 1));
    // A frame over the absolute cap is never admitted.
    assert!(!budget.allow(later + Duration::from_secs(3600), MAX_RELAY_FRAME_BYTES + 1));
}

#[test]
fn relay_session_budget_refusal_spends_nothing() {
    let now = Instant::now();
    let mut budget = RelaySessionBudget::new(10, 1000, now);
    // Bytes bucket allows 1000 but ask for 2000: refused, nothing spent.
    assert!(!budget.allow(now, 2000));
    assert!(budget.allow(now, 999));
}
