#![cfg(unix)]
use anyhow::Result;
use serde_json::{Value, json};
#[path = "support/ota.rs"]
mod fixture;
use fixture::{block, entry, image, manifest, query};
use std::{
    fs,
    time::Duration,
};
use tempfile::tempdir;
use zigbee_bridge_rs::{bridge::ota::reply, ota::Server};

const BULB: u64 = 0x1122334455667788;
const TTL: Duration = Duration::from_secs(3600);
const NO: &[u8] = &[0x19, 7, 2, 0x98];

fn send(s: &mut Server, addr: u64, ep: u8, p: &[u8]) -> Vec<u8> {
    reply(s, addr, ep, p).unwrap().unwrap()
}

#[test]
fn only_the_scheduled_peer_receives_exact_pinned_bytes_after_a_compatible_query() -> Result<()> {
    let dir = tempdir()?;
    let bytes = image(8, Some(BULB), Some((2, 4)));
    let p = manifest(dir.path(), vec![entry(dir.path(), "v8", &bytes)?])?;
    let mut server = Server::load(&p)?;
    assert_eq!(server.check(BULB)?.available, None);
    for hw in [None, Some(1), Some(5)] {
        assert_eq!(send(&mut server, BULB, 1, &query(3, hw)), NO);
        assert_eq!(server.check(BULB)?.available, Some(false));
    }
    assert_eq!(send(&mut server, BULB, 1, &query(3, Some(2))), NO);
    assert_eq!(server.check(BULB)?.image.as_deref(), Some("v8"));
    server.schedule(BULB, None, TTL)?;
    assert!(server.schedule(BULB + 1, Some("v8"), TTL).is_err());
    assert_eq!(send(&mut server, BULB + 1, 1, &query(3, Some(2))), NO);
    for version in [8, 9] {
        assert_eq!(send(&mut server, BULB, 1, &query(version, Some(3))), NO);
    }
    let offer = send(&mut server, BULB, 1, &query(3, Some(4)));
    assert_eq!(
        &offer[..12],
        &[0x19, 7, 2, 0, 0x34, 0x12, 0x56, 0, 8, 0, 0, 0]
    );
    assert_eq!(&offer[12..], &(bytes.len() as u32).to_le_bytes());
    fs::write(dir.path().join("v8.ota"), b"replaced after validation")?;
    let denied = [0x18, 9, 0x0b, 3, 0x98];
    for (addr, ep, version) in [(BULB + 1, 1, 8), (BULB, 2, 8), (BULB, 1, 7)] {
        assert_eq!(send(&mut server, addr, ep, &block(version, 0, 16)), denied);
    }
    let mut transfer = Vec::new();
    while transfer.len() < bytes.len() {
        let req = block(8, transfer.len() as u32, 250);
        let response = send(&mut server, BULB, 1, &req);
        assert_eq!(response, send(&mut server, BULB, 1, &req));
        assert_eq!(
            &response[..12],
            &[0x19, 9, 5, 0, 0x34, 0x12, 0x56, 0, 8, 0, 0, 0]
        );
        assert_eq!(&response[12..16], &(transfer.len() as u32).to_le_bytes());
        assert_eq!(usize::from(response[16]), response.len() - 17);
        assert!(response[16] <= 32);
        transfer.extend_from_slice(&response[17..]);
    }
    assert_eq!(transfer, bytes);
    assert_eq!(
        send(&mut server, BULB, 1, &block(8, bytes.len() as u32, 16)),
        [0x18, 9, 0x0b, 3, 0x80]
    );
    assert_eq!(
        send(&mut server, BULB, 1, &block(8, 0, 0)),
        [0x18, 9, 0x0b, 3, 0x80]
    );
    let mut end = vec![1, 10, 6, 0, 0x34, 0x12, 0x56, 0, 8, 0, 0, 0];
    let response = send(&mut server, BULB, 1, &end);
    assert_eq!(
        response,
        [
            0x19, 10, 7, 0x34, 0x12, 0x56, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ]
    );
    assert_eq!(send(&mut server, BULB, 1, &end), response);
    assert_eq!(send(&mut server, BULB, 1, &query(3, Some(4))), NO);
    server.schedule(BULB, None, TTL)?;
    assert_eq!(send(&mut server, BULB, 1, &query(3, Some(4)))[3], 0);
    end[3] = 0x96;
    assert_eq!(send(&mut server, BULB, 1, &end), [0x18, 10, 0x0b, 6, 0]);
    assert_eq!(send(&mut server, BULB, 1, &block(8, 0, 16)), denied);
    Ok(())
}

#[test]
fn persisted_grants_require_a_fresh_query_and_expire_or_cancel() -> Result<()> {
    let dir = tempdir()?;
    let p = manifest(
        dir.path(),
        vec![entry(dir.path(), "v9", &image(9, None, None))?],
    )?;
    let open = || -> Result<_> {
        let mut s = Server::load(&p)?;
        s.persist_in(dir.path())?;
        Ok(s)
    };
    let mut s = open()?;
    assert!(s.schedule(BULB, None, TTL).is_err());
    s.schedule(BULB, Some("v9"), TTL)?;
    assert_eq!(send(&mut s, BULB, 3, &query(2, None))[3], 0);
    let mut s = open()?;
    assert_eq!(s.check(BULB)?.current, None);
    assert_eq!(
        send(&mut s, BULB, 3, &block(9, 0, 1)),
        [0x18, 9, 11, 3, 0x98]
    );
    assert_eq!(send(&mut s, BULB, 3, &query(2, None))[3], 0);
    s.forget(BULB)?;
    assert_eq!(send(&mut open()?, BULB, 3, &query(2, None)), NO);
    s.schedule(BULB, Some("v9"), TTL)?;
    let state = dir.path().join("ota-schedules.json");
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&state)?)?;
    snapshot["schedules"][BULB.to_string()]["expires"] = json!(1);
    fs::write(state, serde_json::to_vec(&snapshot)?)?;
    assert_eq!(send(&mut open()?, BULB, 3, &query(2, None)), NO);
    Ok(())
}

#[test]
fn ambiguous_or_replaced_images_never_inherit_an_authorization() -> Result<()> {
    let dir = tempdir()?;
    let p = dir.path();
    let bytes = image(6, None, None);
    let entries = vec![entry(p, "a", &bytes)?, entry(p, "b", &bytes)?];
    let file = manifest(p, entries)?;
    let mut s = Server::load(&file)?;
    s.persist_in(p)?;
    assert_eq!(send(&mut s, BULB, 1, &query(1, None)), NO);
    assert!(s.schedule(BULB, None, TTL).is_err());
    s.schedule(BULB, Some("a"), TTL)?;
    let mut changed = bytes;
    *changed.last_mut().unwrap() ^= 1;
    manifest(p, vec![entry(p, "a", &changed)?])?;
    let mut s = Server::load(&file)?;
    s.persist_in(p)?;
    assert_eq!(send(&mut s, BULB, 1, &query(1, None)), NO);
    Ok(())
}

#[test]
fn bad_headers_tags_digests_and_unexpected_frames_are_rejected() -> Result<()> {
    let dir = tempdir()?;
    let p = dir.path();
    let original = image(4, None, None);
    for (index, byte) in [
        (0, 9),
        (4, 5),
        (6, 55),
        (8, 8),
        (52, 99),
        (58, 255),
        (56, 1),
    ] {
        let mut bad = original.clone();
        bad[index] = byte;
        assert!(Server::load(&manifest(p, vec![entry(p, "broken", &bad)?])?).is_err());
    }
    let file = manifest(p, vec![entry(p, "test", &original)?])?;
    fs::write(p.join("test.ota"), b"changed")?;
    assert!(Server::load(&file).is_err());
    let mut server = Server::default();
    assert_eq!(
        send(&mut server, BULB, 1, &[1, 7, 1, 0, 1]),
        [0x18, 7, 11, 1, 0x80]
    );
    assert_eq!(
        send(&mut server, BULB, 1, &[1, 7, 0x70]),
        [0x18, 7, 11, 0x70, 0x81]
    );
    assert_eq!(reply(&mut server, BULB, 1, &[9, 7, 1])?, None);
    assert_eq!(reply(&mut server, BULB, 1, &[0x05, 1, 2, 7, 1])?, None);
    Ok(())
}

#[test]
fn completion_receipt_survives_lost_reply_and_restart_without_reoffering() -> Result<()> {
    let dir = tempdir()?;
    let p = manifest(
        dir.path(),
        vec![entry(dir.path(), "v8", &image(8, None, None))?],
    )?;
    let mut s = Server::load(&p)?;
    s.persist_in(dir.path())?;
    s.schedule(BULB, Some("v8"), TTL)?;
    assert_eq!(send(&mut s, BULB, 1, &query(3, None))[3], 0);
    let end = [1, 10, 6, 0, 0x34, 0x12, 0x56, 0, 8, 0, 0, 0];
    let old = send(&mut s, BULB, 1, &end);
    let mut s = Server::load(&p)?;
    s.persist_in(dir.path())?;
    assert_eq!(send(&mut s, BULB, 1, &end), old);
    assert_eq!(send(&mut s, BULB, 2, &end), [0x18, 10, 11, 6, 0x98]);
    assert_eq!(send(&mut s, BULB, 1, &query(3, None)), NO);
    assert_eq!(
        send(&mut s, BULB, 1, &block(8, 0, 16)),
        [0x18, 9, 11, 3, 0x98]
    );
    Ok(())
}
