use super::*;
use std::io::Read;
use std::net::TcpStream;
use std::time::Duration;

/// The four broadcast helpers used to be four copies of the same loop, each with
/// its own filter and its own copy of the snapshot. They now differ only in the
/// predicate, so what has to be pinned is that the predicates still select exactly
/// the clients they used to - and they take their audience as an argument, so the
/// test can pin that without publishing its clients to the rest of the suite.
fn client_with(eid: i32) -> (Arc<Client>, TcpStream) {
    let (mut c, r) = crate::effect_tests::client_pair();
    c.eid = eid;
    r.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    (Arc::new(c), r)
}

/// Bytes that arrived on this client's socket. A read timeout means nothing more
/// arrived, which is the negative case these assertions are about.
fn drained(r: &mut TcpStream) -> usize {
    let mut buf = [0u8; 4096];
    let mut total = 0;
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    total
}

#[test]
fn the_broadcast_helpers_still_select_the_same_clients() {
    let (me, mut r_me) = client_with(1);
    let (peer, mut r_peer) = client_with(2);
    let (far, mut r_far) = client_with(3);
    // Both peers have seen entity 77; only `peer` is streaming chunk (4,5).
    peer.seen.lock().unwrap().insert(77);
    far.seen.lock().unwrap().insert(77);
    peer.chunks.lock().unwrap().insert((4, 5));
    // The audience is handed to the helpers explicitly, so no other test can
    // write into these sockets: the "nobody else may receive it" assertions used
    // to depend on every concurrent broadcast holding the world guard, which the
    // eight-thread stage does not guarantee (one red run in four).
    let audience = [me.clone(), peer.clone(), far.clone()];

    let payload = |_c: &Client| (0x42i64, vec![1u8, 2, 3]);

    // broadcast: everyone except the named sender.
    broadcast_in(&audience, 1, payload);
    assert_eq!(drained(&mut r_me), 0, "the sender must not receive it");
    assert!(
        drained(&mut r_peer) > 0,
        "an unrelated client must receive it"
    );
    assert!(
        drained(&mut r_far) > 0,
        "an unrelated client must receive it"
    );

    // broadcast_seen_ex: only clients that have seen the entity, minus the excluded
    // one. `me` never saw it and `peer` is excluded, so only `far` is left.
    broadcast_seen_ex_in(&audience, 77, 2, payload);
    assert_eq!(
        drained(&mut r_peer),
        0,
        "the excluded client must not receive it"
    );
    assert_eq!(
        drained(&mut r_me),
        0,
        "a client that has not seen it must not receive it"
    );
    assert!(
        drained(&mut r_far) > 0,
        "a client that has seen it must receive it"
    );

    // broadcast_chunk: only clients streaming that chunk.
    broadcast_chunk_in(&audience, 4, 5, payload);
    assert!(
        drained(&mut r_peer) > 0,
        "a client holding the chunk must receive it"
    );
    assert_eq!(
        drained(&mut r_far),
        0,
        "a client without the chunk must not receive it"
    );

    broadcast_chunk_in(&audience, 9, 9, payload);
    assert_eq!(drained(&mut r_peer), 0, "nobody holds chunk (9,9)");
}
