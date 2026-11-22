// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Online-mode login tests: FIPS-197 AES-128-CFB8 and Minecraft signed SHA-1
//! known-answer vectors, DER SPKI RSA parsing + PKCS#1 v1.5 round-trips against
//! a real openssl-generated RSA-1024 keypair, the pluggable session-join
//! client, and a full loopback `0x01 Encryption Request -> 0x01 Encryption
//! Response -> AES-128-CFB8 armed` Protocol-767 login.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use mc_spine_client::online_auth::{
    aes128_encrypt_block, decode_encryption_request_body, encode_encryption_response_body,
    mc_server_hash, parse_rsa_public_key_der, plan_encryption_response,
    rsa_pkcs1v15_encrypt, rsa_pkcs1v15_pad, rsa_pkcs1v15_unpad, rsa_public_key_to_spki_der,
    Aes128Cfb8, BigUint, MojangSessionJoinClient, OnlineAuthOptions, SessionJoinClient,
    SessionJoinRequest,
};
use mc_spine_client::protocol::{
    decode_packet_body, encode_packet, offline_uuid, write_string, write_varint, PlayEvent,
    ProtocolClient, WireReader,
};
use mc_spine_client::crash_reporter::{ReportTransport, TransportError, TransportResponse};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Real RSA-1024 test keypair (openssl genrsa 1024 + rsa -pubout -outform DER)
// ---------------------------------------------------------------------------

/// openssl-generated `SubjectPublicKeyInfo` DER for the test key below.
const TEST_SPKI_DER_HEX: &str = "30819f300d06092a864886f70d010101050003818d0030818902818100abd82e05f7a09e3113e98d9f71c3d57f62fb3188826ea5929d291167178e89d857eb18d1733ac65b68b545f114db54ce30c4bd974fed16a256b339415f32bd32d924f9f8e5d129c848aca9e0038d8e34b5c9e3cd9bfba53832173afdf2fc08917184b537fd8940e21093c5731d660a2300ac501224b061d9e38fcea21e3a8f650203010001";
const TEST_MODULUS_HEX: &str = "abd82e05f7a09e3113e98d9f71c3d57f62fb3188826ea5929d291167178e89d857eb18d1733ac65b68b545f114db54ce30c4bd974fed16a256b339415f32bd32d924f9f8e5d129c848aca9e0038d8e34b5c9e3cd9bfba53832173afdf2fc08917184b537fd8940e21093c5731d660a2300ac501224b061d9e38fcea21e3a8f65";
const TEST_PRIVATE_EXP_HEX: &str = "0a00d8950258383d3522885afabd62a53ab6d2a856a7683e03a60b5c4bf3c13475d1baad1e9f1242e9f27b72d03b0051cc66f77e6269b3c299776f62cdcedf5288f066de7a415fa21c9db79fd4c2e2543060b1963fbade10f4bcbf7369d450b68b8b28bfff2361ddab95c9d987c0f4c7aa632b752f65cf29ca78405c78b57b41";

fn test_der() -> Vec<u8> {
    unhex(TEST_SPKI_DER_HEX)
}

fn test_modulus() -> BigUint {
    BigUint::from_be_bytes(&unhex(TEST_MODULUS_HEX))
}

fn test_private_exponent() -> BigUint {
    BigUint::from_be_bytes(&unhex(TEST_PRIVATE_EXP_HEX))
}

/// RSA private-key operation for tests: `c^d mod n`, then strip PKCS#1 v1.5.
fn rsa_test_decrypt(ciphertext: &[u8]) -> Vec<u8> {
    let n = test_modulus();
    let d = test_private_exponent();
    let em = BigUint::from_be_bytes(ciphertext)
        .mod_pow(&d, &n)
        .to_be_bytes_padded(n.to_be_bytes().len());
    rsa_pkcs1v15_unpad(&em).expect("unpad decrypted block").to_vec()
}

/// Deterministic RNG: fills every buffer with `byte` (non-zero, so PKCS#1
/// padding completes immediately and output is reproducible).
fn deterministic_rng(byte: u8) -> impl FnMut(&mut [u8]) -> Result<(), String> + Send {
    move |buf: &mut [u8]| {
        buf.fill(byte);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FIPS-197 AES-128 + CFB8 known-answer vectors
// ---------------------------------------------------------------------------

#[test]
fn aes128_fips197_vector_and_cfb8_stream_roundtrip() {
    // FIPS-197 Appendix B block cipher vector.
    let key: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f")
        .try_into()
        .unwrap();
    let block: [u8; 16] = unhex("00112233445566778899aabbccddeeff")
        .try_into()
        .unwrap();
    assert_eq!(
        hex(&aes128_encrypt_block(&key, &block)),
        "69c4e0d86a7b0430d8cdb78070b4c55a"
    );

    // CFB8: Minecraft's key == IV == shared secret; stream across 200 bytes,
    // split at an odd boundary to prove byte-at-a-time feedback keeps state.
    let secret: [u8; 16] = unhex("8e8e9d1a5df24b7a9b3b8e31f0a4c2d5")
        .try_into()
        .unwrap();
    let mut enc = Aes128Cfb8::new(&secret, &secret);
    let mut dec = Aes128Cfb8::new(&secret, &secret);
    let mut msg: Vec<u8> = (0..200u32)
        .map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8)
        .collect();
    let plain = msg.clone();
    enc.encrypt(&mut msg[..37]);
    enc.encrypt(&mut msg[37..]);
    assert_ne!(msg, plain);
    dec.decrypt(&mut msg);
    assert_eq!(msg, plain);

    // Encryptor/decryptor are independent halves of a pair.
    let mut a = Aes128Cfb8::new(&secret, &secret);
    let mut b = Aes128Cfb8::new(&secret, &secret);
    let mut ping = *b"hello cipher";
    a.encrypt(&mut ping);
    b.decrypt(&mut ping);
    assert_eq!(&ping, b"hello cipher");
}

// ---------------------------------------------------------------------------
// Minecraft signed SHA-1 server hash — wiki.vg/vanilla vectors
// ---------------------------------------------------------------------------

#[test]
fn mc_server_hash_matches_official_positive_and_negative_vectors() {
    assert_eq!(
        mc_server_hash("Notch", &[], &[]),
        "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"
    );
    assert_eq!(
        mc_server_hash("jeb_", &[], &[]),
        "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"
    );
    assert_eq!(
        mc_server_hash("simon", &[], &[]),
        "88e16a1019277b15d58faf0541e11910eb756f6"
    );
    // The real login hash concatenates id || secret || key DER.
    let secret = [0x5a; 16];
    let h = mc_server_hash("", &secret, &test_der());
    assert!(!h.is_empty());
    assert_eq!(h, mc_server_hash("", &secret, &test_der()));
    assert_ne!(h, mc_server_hash("", &[0x5b; 16], &test_der()));
}

// ---------------------------------------------------------------------------
// BigUint / RSA known answers
// ---------------------------------------------------------------------------

#[test]
fn biguint_modpow_known_answers() {
    // RFC-style textbook vectors: 4^13 mod 497 = 445, 5^3 mod 13 = 8.
    let m = BigUint::from_be_bytes(&497u64.to_be_bytes());
    let e = BigUint::from_be_bytes(&13u64.to_be_bytes());
    assert_eq!(
        BigUint::from_be_bytes(&4u64.to_be_bytes()).mod_pow(&e, &m),
        BigUint::from_be_bytes(&445u64.to_be_bytes())
    );
    let m13 = BigUint::from_be_bytes(&13u64.to_be_bytes());
    let e3 = BigUint::from_be_bytes(&3u64.to_be_bytes());
    assert_eq!(
        BigUint::from_be_bytes(&5u64.to_be_bytes()).mod_pow(&e3, &m13),
        BigUint::from_be_bytes(&8u64.to_be_bytes())
    );
}

// ---------------------------------------------------------------------------
// DER SPKI parsing: openssl artifact roundtrip + fail-closed rejection
// ---------------------------------------------------------------------------

#[test]
fn der_spki_parses_real_openssl_key_and_encodes_identically() {
    let der = test_der();
    let key = parse_rsa_public_key_der(&der).expect("parse openssl SPKI DER");
    assert_eq!(key.n, test_modulus());
    assert_eq!(key.e.to_be_bytes(), vec![0x01, 0x00, 0x01]); // 65537
    assert_eq!(key.key_bytes, 128);
    // The clean-room encoder reproduces the openssl DER byte-for-byte.
    assert_eq!(
        rsa_public_key_to_spki_der(&unhex(TEST_MODULUS_HEX), &[0x01, 0x00, 0x01]),
        der
    );
}

#[test]
fn der_spki_fail_closed_on_malformed_and_weak_keys() {
    let der = test_der();
    // Truncation anywhere must reject.
    for cut in [0, 10, 30, 100, der.len() - 1] {
        assert!(
            parse_rsa_public_key_der(&der[..cut]).is_err(),
            "truncated at {cut} accepted"
        );
    }
    // Trailing bytes must reject.
    let mut trailing = der.clone();
    trailing.push(0);
    assert!(parse_rsa_public_key_der(&trailing).is_err());
    // Wrong algorithm OID (flip one OID content byte).
    let mut wrong_oid = der.clone();
    let oid_pos = wrong_oid
        .windows(9)
        .position(|w| w == [0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01])
        .expect("oid present");
    wrong_oid[oid_pos + 2] = 0x2b;
    assert!(parse_rsa_public_key_der(&wrong_oid).is_err());
    // Non-NULL algorithm parameters (replace the NULL TLV with INTEGER 0).
    let mut bad_params = der.clone();
    let null_pos = bad_params
        .windows(2)
        .position(|w| w == [0x05, 0x00])
        .expect("null params");
    bad_params[null_pos] = 0x02;
    bad_params[null_pos + 1] = 0x00; // INTEGER 0x00 — length 0, tag mismatch
    assert!(parse_rsa_public_key_der(&bad_params).is_err());

    // Weak key material through the encoder: 512-bit modulus, even modulus,
    // even/zero/too-large exponents.
    let n512 = vec![0xc5u8; 64];
    let odd1024 = {
        let mut b = unhex(TEST_MODULUS_HEX);
        b[127] |= 1;
        b
    };
    let even1024 = {
        let mut b = unhex(TEST_MODULUS_HEX);
        b[127] &= 0xfe;
        b
    };
    assert!(parse_rsa_public_key_der(&rsa_public_key_to_spki_der(&n512, &[1, 0, 1])).is_err());
    assert!(parse_rsa_public_key_der(&rsa_public_key_to_spki_der(&even1024, &[1, 0, 1])).is_err());
    assert!(parse_rsa_public_key_der(&rsa_public_key_to_spki_der(&odd1024, &[2])).is_err());
    assert!(parse_rsa_public_key_der(&rsa_public_key_to_spki_der(&odd1024, &[1, 0, 0])).is_err());
}

// ---------------------------------------------------------------------------
// PKCS#1 v1.5 Type-2 encrypt -> private-key decrypt
// ---------------------------------------------------------------------------

#[test]
fn rsa_pkcs1v15_encrypt_then_private_decrypt_roundtrip() {
    let key = parse_rsa_public_key_der(&test_der()).unwrap();
    let mut rng = deterministic_rng(0x5a);
    for payload in [
        &[0x5a; 16].as_slice(),  // shared secret shape
        &[9, 8, 7, 6].as_slice(), // verify token shape
    ] {
        let ct = rsa_pkcs1v15_encrypt(&key, payload, &mut rng).expect("encrypt");
        assert_eq!(ct.len(), key.key_bytes);
        // Deterministic padding -> block layout is inspectable.
        let em = rsa_pkcs1v15_pad(key.key_bytes, payload, &mut deterministic_rng(0x5a)).unwrap();
        assert_eq!(em[0], 0x00);
        assert_eq!(em[1], 0x02);
        assert!(em[2..2 + (key.key_bytes - payload.len() - 3)]
            .iter()
            .all(|&b| b == 0x5a));
        assert_eq!(rsa_pkcs1v15_unpad(&em).unwrap(), *payload);
        // Private-key op recovers the payload.
        assert_eq!(rsa_test_decrypt(&ct).as_slice(), *payload);
    }
    // Oversized payloads reject: max is k - 11 = 117 bytes for RSA-1024.
    let mut rng = deterministic_rng(0x01);
    assert!(rsa_pkcs1v15_encrypt(&key, &[0u8; 117], &mut rng).is_ok());
    assert!(rsa_pkcs1v15_encrypt(&key, &[0u8; 118], &mut rng).is_err());
}

// ---------------------------------------------------------------------------
// SessionJoinClient: request shape + redacted failures, no live network
// ---------------------------------------------------------------------------

struct CapturingTransport {
    status: u16,
    bodies: Mutex<Vec<String>>,
}

impl CapturingTransport {
    fn new(status: u16) -> Self {
        Self {
            status,
            bodies: Mutex::new(Vec::new()),
        }
    }
    fn bodies(&self) -> Vec<String> {
        self.bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl ReportTransport for CapturingTransport {
    fn post_json(
        &self,
        _url: &str,
        body: &str,
        _timeout: Duration,
    ) -> Result<TransportResponse, TransportError> {
        self.bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(body.to_string());
        Ok(TransportResponse {
            status: self.status,
            body: String::new(),
        })
    }
}

/// Shared handle: `Arc` cannot implement the foreign `ReportTransport`
/// (orphan rule), so a local newtype delegates and keeps a handle to the
/// capturing side after the transport moves into the client.
struct SharedTransport(Arc<CapturingTransport>);

impl ReportTransport for SharedTransport {
    fn post_json(
        &self,
        url: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<TransportResponse, TransportError> {
        self.0.as_ref().post_json(url, body, timeout)
    }
}

#[test]
fn mojang_join_posts_expected_json_and_redacts_failures() {
    let uuid = [
        0x06, 0x9a, 0x79, 0xf4, 0x44, 0xe9, 0x47, 0x26, 0xa5, 0xbe, 0xfc, 0xa9, 0x0e, 0x38, 0xaa,
        0xf5,
    ];
    let req =
        SessionJoinRequest::new("-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1", "SECRET-TOKEN", uuid);
    // Debug of the request never leaks the token.
    let dbg = format!("{req:?}");
    assert!(!dbg.contains("SECRET-TOKEN"));
    assert!(dbg.contains("<REDACTED>"));

    // 204 -> Ok, with the exact Mojang join body shape.
    let transport = Arc::new(CapturingTransport::new(204));
    let client = MojangSessionJoinClient::with_endpoint(
        "https://sessionserver.mojang.com/session/minecraft/join",
        SharedTransport(Arc::clone(&transport)),
        Duration::from_secs(5),
    );
    client.session_join(&req).expect("204 accepted");
    let bodies = transport.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0],
        "{\"accessToken\":\"SECRET-TOKEN\",\"selectedProfile\":\"069a79f444e94726a5befca90e38aaf5\",\"serverId\":\"-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1\"}"
    );

    // Non-204 -> redacted error: status only, no token, no body echo.
    let err_client = MojangSessionJoinClient::with_endpoint(
        "https://sessionserver.mojang.com/session/minecraft/join",
        SharedTransport(Arc::new(CapturingTransport::new(403))),
        Duration::from_secs(5),
    );
    let err = err_client.session_join(&req).unwrap_err();
    assert!(err.contains("403"));
    assert!(!err.contains("SECRET-TOKEN"));
}

// ---------------------------------------------------------------------------
// plan_encryption_response: fail-closed auth requirement, no secret leaks
// ---------------------------------------------------------------------------

struct RecordingJoin {
    calls: Mutex<Vec<(String, [u8; 16])>>,
}

impl RecordingJoin {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> Vec<(String, [u8; 16])> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl SessionJoinClient for RecordingJoin {
    fn session_join(&self, request: &SessionJoinRequest) -> Result<(), String> {
        assert_eq!(request.access_token, "TEST-ACCESS-TOKEN");
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((request.server_hash.clone(), request.selected_profile));
        Ok(())
    }
}

#[test]
fn plan_fails_closed_without_access_token_and_never_leaks_secret() {
    let fields = decode_encryption_request_body(&build_encryption_request_body(
        "",
        &test_der(),
        &[9, 8, 7, 6],
        true,
    ))
    .unwrap();
    let join = RecordingJoin::new();
    let err = plan_encryption_response(
        &fields,
        None,
        Some([7u8; 16]),
        &join,
        &mut deterministic_rng(0x5a),
    )
    .expect_err("missing access token must fail closed");
    assert!(err.contains("access token"));
    // Neither the token slot nor a shared secret may surface in the message.
    assert!(!err.contains("5a5a"));
    assert!(join.calls().is_empty(), "join must not run without a token");

    // A token without a real account UUID (offline-derived logins are never
    // substituted) also fails closed before the join call.
    let err = plan_encryption_response(
        &fields,
        Some("TEST-ACCESS-TOKEN"),
        None,
        &join,
        &mut deterministic_rng(0x5a),
    )
    .expect_err("missing account UUID must fail closed");
    assert!(err.contains("UUID"));
    assert!(!err.contains("TEST-ACCESS-TOKEN"));
    assert!(!err.contains("5a5a"));
    assert!(join.calls().is_empty(), "join must not run without a uuid");

    // should_authenticate = false skips the join entirely but still encrypts.
    let fields_off = decode_encryption_request_body(&build_encryption_request_body(
        "",
        &test_der(),
        &[9, 8, 7, 6],
        false,
    ))
    .unwrap();
    let plan = plan_encryption_response(
        &fields_off,
        None,
        Some([7u8; 16]),
        &RecordingJoin::new(),
        &mut deterministic_rng(0x5a),
    )
    .expect("should_authenticate=false needs no token");
    assert_eq!(plan.shared_secret, [0x5a; 16]);
    // Response body decodes into two 128-byte RSA blocks.
    let mut r = WireReader::new(&plan.response_body);
    let slen = r.read_varint().unwrap() as usize;
    assert_eq!(slen, 128);
    let sct = r.take(slen).unwrap().to_vec();
    let tlen = r.read_varint().unwrap() as usize;
    assert_eq!(tlen, 128);
    let tct = r.take(tlen).unwrap().to_vec();
    assert_eq!(rsa_test_decrypt(&sct), vec![0x5a; 16]);
    assert_eq!(rsa_test_decrypt(&tct), vec![9, 8, 7, 6]);
}

#[test]
fn encryption_request_body_codecs_roundtrip_and_fail_closed() {
    let body = build_encryption_request_body("", &test_der(), &[1, 2, 3, 4], true);
    let f = decode_encryption_request_body(&body).unwrap();
    assert_eq!(f.server_id, "");
    assert_eq!(f.public_key_der, test_der());
    assert_eq!(f.verify_token, vec![1, 2, 3, 4]);
    assert!(f.should_authenticate);

    assert!(decode_encryption_request_body(&body[..body.len() - 1]).is_err());
    let mut trailing = body.clone();
    trailing.push(0);
    assert!(decode_encryption_request_body(&trailing).is_err());
    // Oversized verify token rejects.
    let big = build_encryption_request_body("", &test_der(), &vec![0u8; 4096], true);
    assert!(decode_encryption_request_body(&big).is_err());

    // Response codec: two length-prefixed fields.
    let resp = encode_encryption_response_body(&[0xaa; 5], &[0xbb; 3]);
    let mut r = WireReader::new(&resp);
    assert_eq!(r.read_varint().unwrap(), 5);
    assert_eq!(r.take(5).unwrap(), &[0xaa; 5]);
    assert_eq!(r.read_varint().unwrap(), 3);
    assert_eq!(r.take(3).unwrap(), &[0xbb; 3]);
    assert_eq!(r.remaining(), 0);
}

fn build_encryption_request_body(
    server_id: &str,
    public_key_der: &[u8],
    verify_token: &[u8],
    should_authenticate: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    write_string(&mut out, server_id);
    write_varint(&mut out, public_key_der.len() as i32);
    out.extend_from_slice(public_key_der);
    write_varint(&mut out, verify_token.len() as i32);
    out.extend_from_slice(verify_token);
    out.push(u8::from(should_authenticate));
    out
}

// ---------------------------------------------------------------------------
// Full loopback E2E: encrypted online-mode login through Configuration->Play
// ---------------------------------------------------------------------------

/// Read `n` raw bytes off the mock server socket, decrypting with `rx` when a
/// cipher is armed — mirroring the client's ingest-at-read boundary.
fn mock_read(sock: &mut std::net::TcpStream, rx: Option<&mut Aes128Cfb8>, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    sock.read_exact(&mut buf).unwrap();
    if let Some(rx) = rx {
        rx.decrypt(&mut buf);
    }
    buf
}

/// Read one wire frame (`VarInt len | body`) with optional decryption, then
/// apply the compression layer.
fn mock_read_packet(
    sock: &mut std::net::TcpStream,
    mut rx: Option<&mut Aes128Cfb8>,
    compression: Option<usize>,
) -> (i32, Vec<u8>) {
    let mut header = Vec::new();
    loop {
        let b = mock_read(sock, rx.as_deref_mut(), 1)[0];
        header.push(b);
        if b & 0x80 == 0 {
            break;
        }
        assert!(header.len() < 5);
    }
    let mut hr = WireReader::new(&header);
    let len = hr.read_varint().unwrap() as usize;
    let body = mock_read(sock, rx, len);
    decode_packet_body(&body, compression).unwrap()
}

/// Write one frame; `tx` armed means every emitted byte is encrypted.
fn mock_write_packet(
    sock: &mut std::net::TcpStream,
    tx: Option<&mut Aes128Cfb8>,
    compression: Option<usize>,
    pid: i32,
    body: &[u8],
) {
    let mut frame = encode_packet(pid, body, compression);
    if let Some(tx) = tx {
        tx.encrypt(&mut frame);
    }
    sock.write_all(&frame).unwrap();
}

/// The `should_authenticate=true` online-mode login: Encryption Request ->
/// decrypted-secret-verifying server -> encrypted compression + login success
/// -> encrypted configuration finish -> encrypted play keepalive echo.
#[test]
fn encrypted_online_mode_login_end_to_end_loopback() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let verify_token = [9u8, 8, 7, 6];
    let der = test_der();
    let server_uuid = offline_uuid("Tester");

    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut rx: Option<Aes128Cfb8> = None;
        let mut tx: Option<Aes128Cfb8> = None;
        let mut comp: Option<usize> = None;

        // Handshake + Login Start arrive plaintext.
        let (pid, _) = mock_read_packet(&mut sock, rx.as_mut(), comp);
        assert_eq!(pid, 0x00);
        let (pid, login) = mock_read_packet(&mut sock, rx.as_mut(), comp);
        assert_eq!(pid, 0x00);
        let mut lr = WireReader::new(&login);
        assert_eq!(lr.read_string(16).unwrap(), "Tester");

        // Encryption Request (0x01), then Encryption Response still plaintext.
        let req = build_encryption_request_body("", &der, &verify_token, true);
        mock_write_packet(&mut sock, tx.as_mut(), comp, 0x01, &req);
        let (pid, resp) = mock_read_packet(&mut sock, rx.as_mut(), comp);
        assert_eq!(pid, 0x01, "encryption_response id");
        let mut rr = WireReader::new(&resp);
        let slen = rr.read_varint().unwrap() as usize;
        let sct = rr.take(slen).unwrap().to_vec();
        let tlen = rr.read_varint().unwrap() as usize;
        let tct = rr.take(tlen).unwrap().to_vec();
        assert_eq!(rr.remaining(), 0);

        // RSA unwrap with the test private key: the 16-byte shared secret and
        // the echoed verify token must come back intact.
        let secret = rsa_test_decrypt(&sct);
        assert_eq!(secret.len(), 16);
        assert_eq!(rsa_test_decrypt(&tct), verify_token.to_vec());
        let mut secret16 = [0u8; 16];
        secret16.copy_from_slice(&secret);
        rx = Some(Aes128Cfb8::new(&secret16, &secret16));
        tx = Some(Aes128Cfb8::new(&secret16, &secret16));

        // Encrypted Set Compression (0x03) then Login Success (0x02).
        let mut sc = Vec::new();
        write_varint(&mut sc, 16);
        mock_write_packet(&mut sock, tx.as_mut(), comp, 0x03, &sc);
        comp = Some(16);
        let mut ls = Vec::new();
        ls.extend_from_slice(&server_uuid);
        write_string(&mut ls, "Tester");
        write_varint(&mut ls, 0);
        ls.push(0);
        mock_write_packet(&mut sock, tx.as_mut(), comp, 0x02, &ls);
        let (pid, _) = mock_read_packet(&mut sock, rx.as_mut(), comp);
        assert_eq!(pid, 0x03, "login_acknowledged");

        // Encrypted Finish Configuration -> client_information + ack.
        mock_write_packet(&mut sock, tx.as_mut(), comp, 0x03, &[]);
        let (pid, _) = mock_read_packet(&mut sock, rx.as_mut(), comp);
        assert_eq!(pid, 0x00, "client_information");
        let (pid, _) = mock_read_packet(&mut sock, rx.as_mut(), comp);
        assert_eq!(pid, 0x03, "finish_configuration ack");

        // Encrypted Play keepalive (0x26) -> echo on c2s 0x18.
        mock_write_packet(&mut sock, tx.as_mut(), comp, 0x26, &99i64.to_be_bytes());
        let (pid, ka) = mock_read_packet(&mut sock, rx.as_mut(), comp);
        assert_eq!(pid, 0x18, "play keep_alive echo id");
        assert_eq!(ka, 99i64.to_be_bytes().to_vec());
    });

    let join = Arc::new(RecordingJoin::new());
    let join_for_client = join.clone();
    let uuid = offline_uuid("Tester");
    let (mut client, report) = ProtocolClient::connect_resolved(
        "127.0.0.1",
        port,
        "Tester",
        uuid,
        2,
        Duration::from_secs(30),
        mc_spine_proto::ProtocolVersion::V1_21_1,
        move |h, p| Ok(vec![format!("{h}:{p}").parse().unwrap()]),
        OnlineAuthOptions {
            access_token: Some("TEST-ACCESS-TOKEN".to_string()),
            profile_uuid: Some(uuid),
            session_join: Some(join_for_client),
            rng: Some(Box::new(deterministic_rng(0x5a))),
        },
    )
    .expect("encrypted online-mode connect");

    assert!(report.login_ok && report.config_ok);
    assert_eq!(report.compression_threshold, Some(16));

    // The session join ran exactly once, against the deterministic hash for
    // the fixed shared secret, with the login uuid as selectedProfile.
    let calls = join.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, mc_server_hash("", &[0x5a; 16], &test_der()));
    assert_eq!(calls[0].1, uuid);

    // Play-phase encrypted keepalive completes the loop.
    match client.recv_play(Duration::from_secs(10)).unwrap().unwrap() {
        PlayEvent::KeepAlive(id) => {
            assert_eq!(id, 99);
            client.send_keep_alive(id).unwrap();
        }
        other => panic!("expected KeepAlive, got {other:?}"),
    }
    server.join().unwrap();
}

/// `should_authenticate=true` without a token fails closed before any
/// Encryption Response leaves the client — and never through a message that
/// could carry secret material.
#[test]
fn encryption_request_without_token_fails_closed_loopback() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let der = test_der();

    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (pid, _) = mock_read_packet(&mut sock, None, None);
        assert_eq!(pid, 0x00);
        let (pid, _) = mock_read_packet(&mut sock, None, None);
        assert_eq!(pid, 0x00);
        let req = build_encryption_request_body("", &der, &[9, 8, 7, 6], true);
        sock.write_all(&encode_packet(0x01, &req, None)).unwrap();
        // The client must fail closed: it drops the socket instead of
        // answering. Any read result (EOF, reset, timeout) is acceptable.
        let mut sink = [0u8; 64];
        let _ = sock.read(&mut sink);
    });

    let err = match ProtocolClient::connect_resolved(
        "127.0.0.1",
        port,
        "Tester",
        offline_uuid("Tester"),
        2,
        Duration::from_secs(10),
        mc_spine_proto::ProtocolVersion::V1_21_1,
        move |h, p| Ok(vec![format!("{h}:{p}").parse().unwrap()]),
        OnlineAuthOptions {
            access_token: None,
            profile_uuid: None,
            session_join: Some(Arc::new(RecordingJoin::new())),
            rng: Some(Box::new(deterministic_rng(0x5a))),
        },
    ) {
        Err(e) => e,
        Ok(_) => panic!("missing access token must fail the login"),
    };
    assert!(err.contains("access token"), "unexpected error: {err}");
    assert!(!err.contains("5a5a"), "error leaked shared secret material");
    server.join().unwrap();
}
