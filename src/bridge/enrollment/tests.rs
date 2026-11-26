use super::*;
use crate::{interview::Aps, interview_log::Descriptor};

const CIE: u64 = 0x0200_0000_0000_0001;
const ZONE: Identity = (0x0200_0000_0000_0002, 3);
const OLD: Identity = (0x0200_0000_0000_0003, 1);
const PROFILE: u16 = 0x0104;

struct Fixture {
    addresses: BTreeMap<u64, u16>,
    sent: Vec<(u16, u8, Vec<u8>)>,
    accept: bool,
}
impl Fixture {
    fn new() -> Self {
        Self {
            addresses: BTreeMap::from([(ZONE.0, 0x1234), (OLD.0, 0x4321)]),
            sent: vec![],
            accept: true,
        }
    }
    fn last(&self) -> &[u8] {
        &self.sent.last().unwrap().2
    }
    async fn deliver(
        &mut self,
        e: &mut Enrollment,
        b: &Bindings,
        key: Identity,
        body: &[u8],
    ) -> Result<()> {
        let m = Incoming {
            aps_encrypted:false,
            sender: self.addresses[&key.0],
            kind: 0,
            last_hop_lqi: 255,
            aps: Aps {
                profile: PROFILE,
                cluster: CLUSTER,
                src: key.1,
                dst: 1,
            },
            body,
        };
        e.incoming(self, b, &m).await
    }
}
impl Radio for Fixture {
    async fn node(&mut self, id: u64) -> Result<Option<u16>> {
        Ok(self.addresses.get(&id).copied())
    }
    async fn ieee(&mut self, node: u16) -> Result<Option<u64>> {
        Ok(self
            .addresses
            .iter()
            .find_map(|(&id, &short)| (short == node).then_some(id)))
    }
    fn destination(&self, ieee:u64, node:u16) -> Option<Destination> {
        (self.addresses.get(&ieee) == Some(&node)).then(|| Destination::fixture(ieee,node))
    }
    fn current(&self, dest:&Destination) -> bool {self.addresses.get(&dest.ieee()) == Some(&dest.short())}
    async fn send(&mut self, dest:&Destination, profile: u16, ep: u8, data: &[u8]) -> Result<bool> {
        if !self.accept || !self.current(dest) {return Ok(false);}
        let node = dest.short();
        assert_eq!(profile, PROFILE);
        self.sent.push((node, ep, data.into()));
        Ok(true)
    }
}
fn members(keys: &[Identity]) -> Bindings {
    let mut b = Bindings::default();
    for &(ieee, ep) in keys {
        b.peers.entry(ieee).or_default().1.insert(
            ep,
            Descriptor {
                profile: PROFILE,
                device: 0x0402,
                version: 1,
                servers: vec![CLUSTER],
                clients: vec![],
            },
        );
    }
    b
}
fn attributes(seq: u8, enrolled: bool, owner: u64, id: u8) -> Vec<u8> {
    let mut b = vec![
        0x18,
        seq,
        1,
        0,
        0,
        0,
        0x30,
        enrolled.into(),
        0x10,
        0,
        0,
        0xf0,
    ];
    b.extend(owner.to_le_bytes());
    b.extend([0x11, 0, 0, 0x20, id]);
    b
}
#[tokio::test]
async fn restart_after_response_recovers_id_with_fresh_address_and_confirms_only_readback()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let b = members(&[ZONE]);
    let mut radio = Fixture::new();
    let id;
    {
        let mut e = Enrollment::open(dir.path())?;
        e.start(CIE, &Bindings::default())?;
        e.interviewed(ZONE.0, &b)?;
        e.tick(&mut radio).await?;
        let seq = radio.last()[1];
        radio
            .deliver(&mut e, &b, ZONE, &attributes(seq, false, 0, 255))
            .await?;
        assert_eq!(&radio.last()[2..6], &[2, 0x10, 0, 0xf0]);
        let write = radio.last()[1];
        for reject in [
            vec![0x18, write.wrapping_sub(1), 4, 0],
            vec![0x10, write, 4, 0],
            vec![0x1c, 1, 2, write, 4, 0],
        ] {
            let count = radio.sent.len();
            radio.deliver(&mut e, &b, ZONE, &reject).await?;
            assert_eq!(radio.sent.len(), count);
        }
        radio
            .deliver(&mut e, &b, ZONE, &[0x19, 73, 1, 0x15, 0, 0, 0])
            .await?;
        radio
            .deliver(&mut e, &b, ZONE, &[0x18, write, 4, 0])
            .await?;
        let sent = &radio.sent[radio.sent.len() - 2].2;
        id = sent[4];
        assert_eq!(sent, &[0x11, 73, 0, 0, id]);
        assert_ne!(id, 255);
        assert!(!e.table.rows[&ZONE].confirmed);
    }
    // Restore an offline copy made after a committed assignment, before verify.
    let restored = tempfile::tempdir()?;
    std::fs::copy(
        dir.path().join("ias-zones.redb"),
        restored.path().join("ias-zones.redb"),
    )?;
    radio.addresses.insert(ZONE.0, 0x2345);
    let mut e = Enrollment::open(restored.path())?;
    e.start(CIE, &b)?;
    e.tick(&mut radio).await?;
    assert_eq!(radio.sent.last().unwrap().0, 0x2345);
    assert_eq!(radio.last()[2], 0);
    let seq = radio.last()[1];
    radio
        .deliver(&mut e, &b, ZONE, &attributes(seq, true, CIE, id))
        .await?;
    assert!(e.table.rows[&ZONE].confirmed && e.jobs.is_empty());
    // A repeat request after verification gets the original ID and its new
    // transaction number. The sensor's CIE and ID are read again first.
    radio
        .deliver(&mut e, &b, ZONE, &[0x19, 81, 1, 0x15, 0, 0, 0])
        .await?;
    e.tick(&mut radio).await?;
    let seq = radio.last()[1];
    radio
        .deliver(&mut e, &b, ZONE, &attributes(seq, true, CIE, id))
        .await?;
    assert_eq!(radio.last(), [0x11, 81, 0, 0, id]);
    e.membership(ZONE.0, 0x2345, MemberUpdate::Left, &b)?;
    drop(e);
    let mut e = Enrollment::open(restored.path())?;
    e.start(CIE, &b)?;
    let prev = radio.sent.len();
    radio
        .deliver(&mut e, &b, ZONE, &[0x19, 90, 1, 0x15, 0, 0, 0])
        .await?;
    e.tick(&mut radio).await?;
    assert_eq!(radio.sent.len(), prev);
    assert!(e.jobs.is_empty() && e.table.rows.is_empty());
    Ok(())
}
#[tokio::test]
async fn imported_zone_keeps_its_id_before_new_zone_can_claim_one() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut e = Enrollment::open(dir.path())?;
    let mut b = members(&[OLD]);
    let mut radio = Fixture::new();
    e.start(CIE, &b)?;
    e.tick(&mut radio).await?;
    let old_seq = radio.last()[1];
    b.peers.extend(members(&[ZONE]).peers);
    e.interviewed(ZONE.0, &b)?;
    e.tick(&mut radio).await?;
    let new_seq = radio.last()[1];
    let count = radio.sent.len();
    radio
        .deliver(&mut e, &b, ZONE, &attributes(new_seq, false, 0, 255))
        .await?;
    assert_eq!(radio.sent.len(), count);
    radio
        .deliver(&mut e, &b, OLD, &attributes(old_seq, true, CIE, 0))
        .await?;
    assert!(e.table.rows[&OLD].confirmed);
    e.tick(&mut radio).await?;
    let seq = radio.last()[1];
    radio
        .deliver(&mut e, &b, ZONE, &attributes(seq, false, 0, 255))
        .await?;
    assert_eq!(e.table.rows[&ZONE].id, 1);
    assert!(
        radio
            .sent
            .iter()
            .filter(|(n, _, _)| *n == radio.addresses[&OLD.0])
            .all(|(_, _, frame)| frame[2] == 0)
    );
    Ok(())
}
#[tokio::test]
async fn foreign_cie_conflicting_read_and_failed_write_never_enroll() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let b = members(&[ZONE]);
    let mut radio = Fixture::new();
    let mut e = Enrollment::open(dir.path())?;
    e.start(CIE, &Bindings::default())?;
    e.interviewed(ZONE.0, &b)?;
    e.tick(&mut radio).await?;
    let seq = radio.last()[1];
    let count = radio.sent.len();
    let mut duplicate = attributes(seq, false, 0, 255);
    duplicate.extend([0, 0, 0, 0x30, 0]);
    radio.deliver(&mut e, &b, ZONE, &duplicate).await?;
    radio
        .deliver(&mut e, &b, ZONE, &attributes(seq, false, CIE + 99, 255))
        .await?;
    assert_eq!(radio.sent.len(), count);
    assert!(e.jobs.is_empty());
    e.interviewed(ZONE.0, &b)?;
    e.tick(&mut radio).await?;
    let seq = radio.last()[1];
    radio
        .deliver(&mut e, &b, ZONE, &attributes(seq, false, 0, 255))
        .await?;
    let count = radio.sent.len();
    let seq = radio.last()[1];
    radio
        .deliver(&mut e, &b, ZONE, &[0x18, seq, 4, 0x88, 0x10, 0])
        .await?;
    assert_eq!(radio.sent.len(), count);
    assert!(!e.table.rows[&ZONE].confirmed && e.jobs.is_empty());
    drop(e);
    assert!(Enrollment::open(dir.path())?.start(CIE + 1, &b).is_err());
    Ok(())
}
#[tokio::test]
async fn bounded_timeout_rearms_on_wake_and_exhausted_zone_table_replies_with_failure() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let b = members(&[ZONE]);
    let mut radio = Fixture::new();
    let mut e = Enrollment::open(dir.path())?;
    e.start(CIE, &Bindings::default())?;
    e.interviewed(ZONE.0, &b)?;
    for _ in 0..4 {
        e.jobs.get_mut(&ZONE).unwrap().due = Instant::now();
        e.tick(&mut radio).await?;
    }
    assert_eq!(radio.sent.len(), 3);
    assert!(e.deadline().is_none());
    assert_eq!(e.status(ZONE)["state"], "waiting_for_device");
    radio
        .deliver(&mut e, &b, ZONE, &[0x19, 8, 0, 1, 0, 0, 255, 0, 0])
        .await?;
    radio
        .deliver(&mut e, &b, ZONE, &[0x19, 32, 1, 0x15, 0, 0, 0])
        .await?;
    e.tick(&mut radio).await?;
    let seq = radio.last()[1];
    // Occupy each legal number with a different endpoint.
    for id in 0..255u8 {
        e.table.rows.insert(
            (OLD.0 + u64::from(id), 1),
            Row {
                profile: PROFILE,
                id,
                confirmed: true,
            },
        );
    }
    radio
        .deliver(&mut e, &b, ZONE, &attributes(seq, false, 0, 255))
        .await?;
    assert_eq!(radio.last(), [0x11, 32, 0, 3, 255]);
    assert_eq!(e.status(ZONE)["state"], "zone_table_full");
    Ok(())
}

#[tokio::test]
async fn cold_table_and_adapter_refusals_do_not_consume_enrollment_attempts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let b = members(&[ZONE]);
    let mut e = Enrollment::open(dir.path())?;
    let mut radio = Fixture::new();
    e.start(CIE, &b)?;

    let short = radio.addresses.remove(&ZONE.0).unwrap();
    for _ in 0..3 {
        e.jobs.get_mut(&ZONE).unwrap().due = Instant::now();
        e.tick(&mut radio).await?;
        assert_eq!(e.jobs[&ZONE].tries, 0);
        assert_eq!(e.status(ZONE)["state"], "waiting_for_address");
        assert!(e.jobs[&ZONE].due > Instant::now());
    }
    let job = e.jobs.get_mut(&ZONE).unwrap();
    job.preflight_until = Some(Instant::now());
    job.due = Instant::now();
    e.tick(&mut radio).await?;
    assert!(e.deadline().is_none() && radio.sent.is_empty());
    e.peer_seen(OLD.0);
    assert!(e.deadline().is_none());

    radio.addresses.insert(ZONE.0, short);
    e.peer_seen(ZONE.0);
    radio.accept = false;
    e.tick(&mut radio).await?;
    assert_eq!(e.status(ZONE)["state"], "waiting_for_adapter");
    assert_eq!(e.jobs[&ZONE].tries, 0);
    assert!(radio.sent.is_empty());

    radio.accept = true;
    e.jobs.get_mut(&ZONE).unwrap().due = Instant::now();
    e.tick(&mut radio).await?;
    assert_eq!(e.jobs[&ZONE].tries, 1);
    assert_eq!(radio.sent.len(), 1);
    let seq = radio.last()[1];
    radio.deliver(&mut e, &b, ZONE, &attributes(seq,true,CIE,7)).await?;
    assert_eq!(e.status(ZONE)["state"], "confirmed");
    Ok(())
}

#[tokio::test]
async fn duplicate_write_response_cannot_replay_an_enroll_answer_after_a_local_refusal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let b = members(&[ZONE]);
    let mut radio = Fixture::new();
    let mut e = Enrollment::open(dir.path())?;
    e.start(CIE, &Bindings::default())?;
    e.interviewed(ZONE.0, &b)?;
    e.tick(&mut radio).await?;
    let seq = radio.last()[1];
    radio.deliver(&mut e, &b, ZONE, &attributes(seq, false, 0, 255)).await?;
    let written = [0x18, radio.last()[1], 4, 0];
    radio.deliver(&mut e, &b, ZONE, &[0x19, 73, 1, 0x15, 0, 0, 0]).await?;
    radio.accept = false;
    radio.deliver(&mut e, &b, ZONE, &written).await?;
    assert_eq!(e.status(ZONE)["state"], "waiting_for_adapter");
    assert!(!e.has_outstanding(ZONE.0));
    let count = radio.sent.len();
    radio.accept = true;
    radio.deliver(&mut e, &b, ZONE, &written).await?;
    assert_eq!(radio.sent.len(), count);
    assert_eq!(e.jobs[&ZONE].request.as_ref().unwrap().1, 73);
    Ok(())
}

#[tokio::test]
async fn conflict_retires_queued_enroll_request_and_old_read_without_releasing_the_id() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let b = members(&[ZONE, OLD]);
    let mut radio = Fixture::new();
    let mut e = Enrollment::open(dir.path())?;
    e.start(CIE, &Bindings::default())?;
    e.interviewed(ZONE.0, &b)?;
    e.tick(&mut radio).await?;
    let seq = radio.last()[1];
    radio.deliver(&mut e, &b, ZONE, &[0x19,73,1,0x15,0,0,0]).await?;
    let short = radio.addresses[&ZONE.0];
    let reserved = e.table.reserve(ZONE, Some(6))?;
    e.invalidate_address(None, short, |_| false);
    let count = radio.sent.len();
    radio.deliver(&mut e,&b,ZONE,&attributes(seq,false,0,255)).await?;
    assert_eq!(radio.sent.len(), count);
    assert!(!e.has_outstanding(ZONE.0) && e.jobs[&ZONE].request.is_none());
    assert_eq!(reserved, Some(e.table.rows[&ZONE].id));
    radio.addresses.insert(ZONE.0,short+1);
    e.jobs.get_mut(&ZONE).unwrap().due = Instant::now();
    e.tick(&mut radio).await?;
    assert_eq!(radio.sent.last().unwrap().0,short+1);
    assert_eq!(radio.last()[2],0);
    Ok(())
}

#[test]
fn ace_directory_requires_confirmed_enrollment_and_current_protected_zone_status() -> Result<()> {
    let dir=tempfile::tempdir()?;
    let members=members(&[ZONE]);
    let mut e=Enrollment::open(dir.path())?;
    e.start(CIE,&members)?;
    let row=Row{profile:PROFILE,id:9,confirmed:false};
    e.table.save(ZONE,row)?;
    assert!(e.directory(&members, |_| true).is_empty());
    e.table.save(ZONE,Row{confirmed:true,..row})?;
    let notify=|e:&mut Enrollment, protected:bool, id:u8, delay:u16, status:u8| {
        let [lo,hi]=delay.to_le_bytes();
        let body=[0x19,11,0,status,0,0,id,lo,hi];
        let m=Incoming{sender:0x22,kind:0,last_hop_lqi:200,aps_encrypted:protected,
            aps:Aps{profile:PROFILE,cluster:CLUSTER,src:ZONE.1,dst:1},body:&body};
        e.observe_ace_status(ZONE,&m,Frame::decode(&body).unwrap(), &Destination::fixture(ZONE.0,m.sender));
    };
    notify(&mut e,false,9,0,3);
    notify(&mut e,true,8,0,3);
    notify(&mut e,true,9,1204,2);
    assert_eq!(e.directory(&members, |_| true)[&9].status,None);
    notify(&mut e,true,9,0,3);
    notify(&mut e,true,9,16,1);
    assert_eq!(e.directory(&members, |_| true)[&9].status,Some(3));
    assert_eq!(e.directory(&members, |_| false)[&9].status,None);
    e.membership(ZONE.0,0x22,MemberUpdate::Left,&members)?;
    assert!(e.directory(&members, |_| true).is_empty());
    Ok(())
}
