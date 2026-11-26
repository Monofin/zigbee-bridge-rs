//! One bounded interview on the isolated bench. Match replies at both APS and ZDO/ZCL.
use crate::{
    ash::{Callback, Destination, Ezsp},
    coordinator::{checked, request},
    interview_log::{AttributeResult, Descriptor, IdentitySource, Observation},
};
use anyhow::{Context, Result, bail, ensure};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;
use zigbee_zcl::{Direction, Frame, FrameType, Header, attributes::Status, foundation::{GeneralCommand, Payload}};

pub(crate) mod commissioning;
pub(crate) mod reporting;
use commissioning::Commissioning;

const HA: u16 = 0x0104;
const WINDOW: u8 = 180;
mod endpoints;
#[cfg(test)]
mod lock_security_test;
pub(crate) use endpoints::{client_endpoint, supports_cluster, valid_endpoint};

pub async fn register(link: &mut Ezsp<'_>) -> Result<()> {
    register_listener(link).await
}

pub async fn register_listener(link: &mut Ezsp<'_>) -> Result<()> {
    checked(link, 0x53, &[42, 1, 0]).await?;
    for e in endpoints::ENDPOINTS {
        let mut args = vec![e.id, 4, 1, 7, 0, 0, e.servers.len() as u8, e.clients.len() as u8];
        for cluster in e.servers.iter().chain(e.clients) {
            args.extend(cluster.to_le_bytes());
        }
        checked(link, 0x02, &args).await.with_context(|| format!("register local endpoint {}", e.id))?;
    }
    Ok(())
}

/// Record exactly the APS payload delivered by the NCP. Record retries too.
pub async fn listen(link: &mut Ezsp<'_>) -> Result<()> {
    listen_until(link, None).await
}

async fn listen_until(link: &mut Ezsp<'_>, end: Option<Instant>) -> Result<()> {
    counters(link).await?;
    println!("LISTENER READY: receiving APS messages");
    loop {
        let now = Instant::now();
        if end.is_some_and(|deadline| now >= deadline) { return Ok(()); }
        let tick = now + Duration::from_secs(30);
        let Some(cb) = next(link, end.map_or(tick, |deadline| tick.min(deadline))).await? else {
            counters(link).await?;
            continue;
        };
        log_incoming(&cb)?;
    }
}

fn log_incoming(cb: &Callback) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
        if cb.id == 0x45 {
            let msg = incoming(&cb.payload)?;
            println!("{}", serde_json::json!({
                "unix_ms": SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
                "sender": msg.sender, "kind": msg.kind, "profile": msg.aps.profile,
                "cluster": msg.aps.cluster, "src_ep": msg.aps.src, "dst_ep": msg.aps.dst,
                "payload": (!matches!(msg.aps.cluster, 0x0101 | 0x0501)).then_some(msg.body)
            }));
        }
    Ok(())
}

pub(crate) async fn open(link: &mut Ezsp<'_>, seconds:u8) -> Result<()> {
    crate::coordinator::set_tc_key_request_policy(link, crate::coordinator::SEND_CURRENT_TC_KEY).await?;
    let mut key = vec![0xff; 8];
    key.extend(b"ZigBeeAlliance09"); // Standard, transient commissioning link key.
    key.push(0);
    ensure!(
        request(link, 0x111, &key).await? == [0; 4],
        "transient commissioning key rejected"
    );
    crate::coordinator::join_policy(link, true).await?;
    checked(link, 0x22, &[seconds]).await
}

pub(crate) async fn close(link: &mut Ezsp<'_>) -> Result<()> {
    checked(link, 0x22, &[0]).await?;
    crate::coordinator::join_policy(link, false).await?;
    println!("Pairing closed; trust centre policy verified.");
    Ok(())
}

pub async fn clear_transient_keys(link: &mut Ezsp<'_>) -> Result<()> {
    ensure!(
        request(link, 0x6b, &[]).await?.is_empty(),
        "unexpected transient key cleanup reply"
    );
    println!("Transient key cleanup acknowledged.");
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Aps {
    pub(crate) profile: u16,
    pub(crate) cluster: u16,
    pub(crate) src: u8,
    pub(crate) dst: u8,
}
fn word(a: u8, b: u8) -> u16 {
    u16::from_le_bytes([a, b])
}

pub(crate) const APS_ENCRYPTION: u8 = 0x20;

pub(crate) struct Incoming<'a> {
    pub(crate) kind: u8,
    pub(crate) aps_encrypted: bool,
    pub(crate) aps: Aps,
    pub(crate) sender: u16,
    /// Quality measured at the coordinator for the last MAC hop.
    pub(crate) last_hop_lqi: u8,
    pub(crate) body: &'a [u8],
}
pub(crate) fn incoming(bytes: &[u8]) -> Result<Incoming<'_>> {
    let [
        kind,
        p,
        q,
        c,
        d,
        src,
        dst,
        options, _, // APS options
        _, _, // APS group ID
        _,    // APS sequence
        last_hop_lqi,
        _,    // last-hop RSSI
        lo,
        hi,
        _,
        _,
        len,
        body @ ..,
    ] = bytes
    else {
        bail!("short incoming message")
    };
    // EZSP messageContents is length-prefixed. Firmware can append callback
    // data after that field; it does not belong to the APS payload.
    let (body, _extension) = body.split_at_checked(usize::from(*len)).with_context(|| {
        format!(
            "truncated incoming message: declared {len}, available {}",
            body.len()
        )
    })?;
    Ok(Incoming {
        kind: *kind,
        aps_encrypted: options & APS_ENCRYPTION != 0,
        aps: Aps {
            profile: word(*p, *q),
            cluster: word(*c, *d),
            src: *src,
            dst: *dst,
        },
        sender: word(*lo, *hi),
        last_hop_lqi: *last_hop_lqi,
        body,
    })
}

pub(crate) async fn next(link: &mut Ezsp<'_>, until: Instant) -> Result<Option<Callback>> {
    if Instant::now() >= until {
        return Ok(None);
    }
    let cb = link
        .callback_until(until)
        .await
        .map_err(|e| anyhow::anyhow!("interview link: {e:?}"))?;
    if let Some(cb) = &cb { inspect_callback(cb)?; }
    Ok(cb)
}

pub(crate) fn inspect_callback(cb: &Callback) -> Result<()> {
        match cb.id {
            0x19 => ensure!(
                matches!(cb.payload.as_slice(), [0x90 | 0x9c | 0x9d]),
                "stack status {:02x?}",
                cb.payload
            ),
            0x24 => {
                if let [lo, hi, _, _, _, _, _, _, _, _, update, decision, _, _] = cb.payload[..] {
                    println!(
                        "Trust centre: node {:#06x}, update {update}, decision {decision}",
                        word(lo, hi)
                    );
                }
            }
            _ => {}
        }
    Ok(())
}

// The NCP counters cover this radio, not an individual transmitter. These
// four readings distinguish a quiet coordinator from a lost host callback.
async fn counters(link: &mut Ezsp<'_>) -> Result<()> {
    let bytes = request(link, 0xf1, &[]).await?;
    ensure!(bytes.len() >= 34, "short Ember counter vector");
    let count = |n: usize| word(bytes[2 * n], bytes[2 * n + 1]);
    println!(
        "Radio counters: MAC rx broadcast {}, unicast {}, APS rx unicast {}, join indications {}",
        count(0),
        count(2),
        count(8),
        count(16)
    );
    Ok(())
}

async fn announcement(link: &mut Ezsp<'_>, until: Instant, keys: &mut Commissioning) -> Result<Destination> {
    let mut admitted = BTreeMap::<[u8; 8], Destination>::new();
    while let Some(cb) = keys.receive(link, until).await? {
        if let (0x24, [lo, hi, ieee @ .., 1 | 3, decision, _, _]) = (cb.id, cb.payload.as_slice())
            && *decision != 2
            && let Ok(ieee) = <[u8; 8]>::try_from(ieee)
            && let Ok(dest) = link.destination(u64::from_le_bytes(ieee), word(*lo, *hi))
            && link.current(&dest)
        {
            admitted.insert(ieee, dest);
        }
        if cb.id != 0x45 {
            continue;
        }
        let msg = incoming(&cb.payload)?;
        if msg.aps
            != (Aps {
                profile: 0,
                cluster: 0x13,
                src: 0,
                dst: 0,
            })
        {
            continue;
        }
        if let [_, lo, hi, ieee @ .., _cap] = msg.body
            && msg.sender == word(*lo, *hi)
            && let Ok(ieee) = <[u8; 8]>::try_from(ieee)
            && let Some(dest) = admitted.get(&ieee)
            && dest.short() == msg.sender && link.current(dest)
        {
            return Ok(dest.clone());
        }
    }
    bail!("no device announcement within the join window")
}

pub(crate) trait InterviewIo {
    fn next_sequence(&self,current:u8)->u8 {current.wrapping_add(1)}
    async fn send(&mut self,args:&[u8]) -> Result<Vec<u8>>;
    async fn receive(&mut self,deadline:Instant) -> Result<Option<Callback>>;
}
enum WireTarget<'a> {
    Device(&'a Destination),
    IdentityProbe(&'a Destination),
}

struct Wire<'a,'s> {
    link: &'a mut Ezsp<'s>,
    keys: &'a mut Commissioning,
    target: WireTarget<'a>,
}

impl InterviewIo for Wire<'_,'_> {
    async fn send(&mut self,args:&[u8])->Result<Vec<u8>> {
        match self.target {
            WireTarget::Device(d) => self.link.submit_to(d,args).await,
            WireTarget::IdentityProbe(d) => {
                ensure!(self.link.current(d), "historical address changed during discovery");
                self.link.probe_ieee(d.short(), args).await
            }
        }
    }
    async fn receive(&mut self,deadline:Instant)->Result<Option<Callback>> {
        let cb = self.keys.receive(self.link,deadline).await?;
        let (WireTarget::Device(d) | WireTarget::IdentityProbe(d)) = self.target;
        ensure!(self.link.current(d), "interview destination changed");
        Ok(cb)
    }
}

type Recorder<'a> = dyn FnMut(Observation) -> Result<()> + 'a;

/// A descriptor server can hold information about multiple nodes. Even an error
/// identifies the address of interest; a successful Simple_Desc_rsp also names its endpoint.
fn descriptor_reply_matches(cluster: u16, request: &[u8], reply: &[u8]) -> bool {
    let [_, status, lo, hi, len, data @ ..] = reply else { return false };
    if request.get(1..3) != Some(&[*lo, *hi]) || data.len() != usize::from(*len) {
        return false;
    }
    if *status != 0 {
        return data.is_empty();
    }
    cluster == 5 || data.first().is_some_and(|endpoint| Some(endpoint) == request.get(3))
}

struct Target<'a> {
    record: &'a mut Recorder<'a>,
    node: u16,
    ieee: [u8; 8],
    seq: u8,
}
impl Target<'_> {
    async fn exchange(
        &mut self,
        link: &mut impl InterviewIo,
        aps: Aps,
        response: u16,
        mut body: Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let tsn = self.seq;
        self.seq = link.next_sequence(tsn);
        let zdo = aps.profile == 0;
        // Use APS encryption when reading lock policy. Never retry the query at network security.
        let protected = !zdo && aps.cluster == zigbee_devices::equipment::DOOR_LOCK;
        let expected_commands = if zdo { None } else {
            Some(match Frame::decode(&body)?.header.general_command() {
                Some(GeneralCommand::ConfigureReporting) => (GeneralCommand::ConfigureReporting, GeneralCommand::ConfigureReportingResponse),
                Some(GeneralCommand::ReadReportingConfiguration) => (GeneralCommand::ReadReportingConfiguration, GeneralCommand::ReadReportingConfigurationResponse),
                Some(GeneralCommand::ReadAttributes) => (GeneralCommand::ReadAttributes, GeneralCommand::ReadAttributesResponse),
                Some(GeneralCommand::DiscoverAttributesExtended) => (GeneralCommand::DiscoverAttributesExtended, GeneralCommand::DiscoverAttributesExtendedResponse),
                Some(GeneralCommand::DiscoverCommandsReceived) => (GeneralCommand::DiscoverCommandsReceived, GeneralCommand::DiscoverCommandsReceivedResponse),
                _ => bail!("unsupported interview request"),
            })
        };
        body[usize::from(!zdo)] = tsn;
        (self.record)(Observation::Request{
            sequence:tsn,profile:aps.profile,endpoint:aps.dst,cluster:aps.cluster,payload:body.clone()
        })?;
        let mut args = vec![0];
        args.extend(self.node.to_le_bytes());
        args.extend(aps.profile.to_le_bytes());
        args.extend(aps.cluster.to_le_bytes());
        args.extend([
            aps.src,
            aps.dst,
            if protected {0x40 | APS_ENCRYPTION} else {0x40},
            1,
            0,
            0,
            0,
            tsn,
            body.len().try_into()?,
        ]);
        args.extend_from_slice(&body);
        let ack = link.send(&args).await?;
        if protected && matches!(ack.as_slice(),[status,_] if *status!=0) {
            return Ok(None);
        }
        ensure!(
            matches!(ack.as_slice(), [0, _]),
            "sendUnicast returned {ack:02x?}"
        );
        let expected = Aps {
            profile: aps.profile,
            cluster: response,
            src: aps.dst,
            dst: aps.src,
        };
        let until = Instant::now() + Duration::from_secs(7);
        while let Some(cb) = link.receive(until).await? {
            if cb.id == 0x3f
                && let [0, lo, hi, _, _, _, _, _, _, _, _, _, _, _, tag, status, ..] =
                    cb.payload.as_slice()
                && *tag == tsn
                && word(*lo, *hi) == self.node
            {
                println!(
                    "TX tag {tag} to {:#06x}, cluster {:#06x}: delivery status {status:#04x}",
                    self.node, aps.cluster
                );
            }
            if let (0x7c, [lo,hi]) = (cb.id,cb.payload.as_slice()) {
                ensure!(word(*lo,*hi) != self.node, "interview address is disputed");
            }
            if let (0x24, [lo,hi,identity @ ..,_,_,_,_]) = (cb.id,cb.payload.as_slice()) {
                ensure!(word(*lo,*hi) != self.node && identity != self.ieee, "interviewed membership changed");
            }
            if cb.id != 0x45 {
                continue;
            }
            let m = incoming(&cb.payload)?;
            if (m.aps.profile,m.aps.cluster,m.aps.src,m.aps.dst) == (0,0x13,0,0)
                && let [_, lo, hi, identity @ .., _] = m.body
                && identity.len() == 8 && m.sender == word(*lo,*hi)
            {
                ensure!((identity == self.ieee) == (m.sender == self.node), "interview address reassigned");
            }
            if m.sender != self.node || m.aps != expected || m.kind > 1 || protected && !m.aps_encrypted {
                continue;
            }
            if zdo {
                if m.body.first() != Some(&tsn) {
                    continue;
                }
                if matches!(aps.cluster, 4 | 5) && !descriptor_reply_matches(aps.cluster, &body, m.body) {
                    continue;
                }
            } else if let Some((request, response)) = expected_commands {
                let Ok(f) = Frame::decode(m.body) else { continue };
                if f.header.sequence != tsn
                    || f.header.manufacturer.is_some()
                    || f.header.direction != Direction::ServerToClient
                    || f.header.frame_type != FrameType::General
                {
                    continue;
                }
                if f.header.general_command() != Some(response)
                    && !matches!(Payload::decode(f), Ok(Payload::DefaultResponse(reply)) if reply.command_id == request as u8) {
                    continue;
                }
            }
            return Ok(Some(m.body.to_vec()));
        }
        (self.record)(Observation::Timeout{sequence:tsn})?;
        Ok(None)
    }
    async fn zdo(
        &mut self,
        link: &mut impl InterviewIo,
        cluster: u16,
        ep: Option<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let sequence=self.seq;
        let mut body = vec![0];
        body.extend(self.node.to_le_bytes());
        body.extend(ep);
        let Some(msg) = self
            .exchange(
                link,
                Aps {
                    profile: 0,
                    cluster,
                    src: 0,
                    dst: 0,
                },
                cluster | 0x8000,
                body,
            )
            .await?
        else {
            return Ok(None);
        };
        match msg.as_slice() {
            [_, 0, lo, hi, len, rest @ ..]
                if word(*lo, *hi) == self.node && usize::from(*len) == rest.len() =>
            {
                Ok(Some(rest.to_vec()))
            }
            [_, status, ..] if *status != 0 => {
                (self.record)(Observation::ZdoError{sequence,status:*status})?;
                println!("ZDO {cluster:#06x} status {status:#04x}");
                Ok(None)
            }
            _ => bail!("inconsistent ZDO response to {cluster:#06x}"),
        }
    }
    async fn foundation(&mut self, link: &mut impl InterviewIo, profile: u16, ep: u8, cluster: u16,
        payload: Payload<'_>) -> Result<Option<Vec<u8>>> {
        let mut body = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::General, manufacturer: None, direction: Direction::ClientToServer,
                disable_default_response: true, sequence: 0, command_id: payload.command() as u8,
            },
            payload: &payload.encode()?,
        }.encode_into(&mut body);
        self.exchange(link, Aps { profile, cluster, src: client_endpoint(cluster), dst: ep }, cluster, body).await
    }

    async fn cover_commands(&mut self, link: &mut impl InterviewIo, profile: u16, ep: u8) -> Result<()> {
        use zigbee_devices::cover::CLUSTER;
        use zigbee_zcl::discovery::Discover;
        // Only 0x05 and 0x08 affect percentage control. Short pages still
        // provide positive evidence, and the cursor has a four-ID bound.
        let mut start = 5;
        let mut found = Vec::new();
        while start <= 8 {
            let query = Payload::DiscoverCommandsReceived(Discover { start, maximum: 4 });
            let Some(bytes) = self.foundation(link, profile, ep, CLUSTER, query).await? else { break };
            let response = Frame::decode(&bytes)?;
            let Ok(Payload::DiscoverCommandsReceivedResponse(page)) = Payload::decode(response) else { break };
            if page.entries.is_empty() {
                if page.complete {(self.record)(Observation::CoverCommandsReviewed{endpoint:ep,commands:found})?;}
                break;
            }
            if page.entries.len() > 4 || page.entries[0] < start ||
                !page.entries.windows(2).all(|pair| pair[0] < pair[1]) { break; }
            let last = *page.entries.last().expect("nonempty page");
            found.extend(page.entries.iter().copied().filter(|id|(5..=8).contains(id)));
            (self.record)(Observation::CommandsReceived { endpoint: ep, cluster: CLUSTER, commands: page.entries })?;
            if page.complete || last >= 8 {
                (self.record)(Observation::CoverCommandsReviewed{endpoint:ep,commands:found})?;
                break;
            }
            start = last + 1;
        }
        Ok(())
    }

    async fn basic(&mut self, link: &mut impl InterviewIo, ep: u8, d: &Descriptor, admitted: bool) -> Result<()> {
        let mut maker = None;
        let mut model = None;
        self.read_observing(link, d.profile, ep, 0, &[4, 5, 0x4000], |id, result| {
            match id {
                4 => maker = result.character_string().map(str::to_owned),
                5 => model = result.character_string().map(str::to_owned),
                _ => {}
            }
        }).await?;
        if admitted && let (Some(maker), Some(model)) = (maker, model) {
            let endpoint = zigbee_devices::Endpoint {
                id: ep, profile: d.profile, servers: &d.servers, clients: &d.clients,
            };
            if zigbee_devices::tuya::services::basic_setup(&maker, &model, &endpoint, d.device)?
            {
                self.read(link, d.profile, ep, 0, &zigbee_devices::tuya::services::SETUP_ATTRIBUTES).await?;
            }
        }
        Ok(())
    }

    async fn read(
        &mut self,
        link: &mut impl InterviewIo,
        profile: u16,
        ep: u8,
        cluster: u16,
        ids: &[u16],
    ) -> Result<()> {
        self.read_observing(link, profile, ep, cluster, ids, |_, _| {}).await
    }

    async fn read_observing(
        &mut self,
        link: &mut impl InterviewIo,
        profile: u16,
        ep: u8,
        cluster: u16,
        ids: &[u16],
        mut observe: impl FnMut(u16, &AttributeResult),
    ) -> Result<()> {
        let sequence=self.seq;
        let response = self.foundation(link, profile, ep, cluster, Payload::ReadAttributes(ids.to_vec())).await?;
        let Some(bytes) = response else {
            println!("EP {ep} cluster {cluster:#06x}: read timed out");
            return Ok(());
        };
        match Payload::decode(Frame::decode(&bytes)?)? {
            Payload::ReadAttributesResponse(records) => {
                for id in ids {
                    let record = records.iter().find(|r| r.id == *id).filter(|_| {
                        cluster != zigbee_devices::equipment::DOOR_LOCK ||
                            *id != zigbee_devices::equipment::SECURITY_LEVEL || records.len() == 1
                    });
                    let result = match record.map(|r| &r.result) {
                        Some(Status::Success(v)) => AttributeResult::Value {data_type:v.data_type().id(),bytes:v.as_bytes().to_vec()},
                        Some(Status::Error(e)) => AttributeResult::Error {status:e.get()},
                        None => AttributeResult::Omitted,
                    };
                    println!("EP {ep} cluster {cluster:#06x} attribute {id:#06x}: {result:02x?}");
                    observe(*id, &result);
                    (self.record)(Observation::Attribute{sequence,endpoint:ep,cluster,attribute:*id,result})?;
                }
            }
            Payload::DefaultResponse(r) => {
                ensure!(r.command_id==0,"default response for another command");
                println!("EP {ep} cluster {cluster:#06x}: default response to {} status {:#04x}",r.command_id,r.status);
                (self.record)(Observation::DefaultResponse{sequence,command:r.command_id,status:r.status})?;
            }
            _ => bail!("unexpected ZCL response"),
        }
        Ok(())
    }
}

fn clusters(buf: &mut &[u8]) -> Result<Vec<u16>> {
    let (count, rest) = buf.split_first().context("missing cluster count")?;
    let (ids, tail) = rest
        .split_at_checked(usize::from(*count) * 2)
        .context("short cluster list")?;
    *buf = tail;
    Ok(ids.chunks_exact(2).map(|b| word(b[0], b[1])).collect())
}
pub(crate) fn descriptor(ep: u8, data: &[u8]) -> Result<Descriptor> {
    let [actual, p, q, d, e, version, mut_rest @ ..] = data else {
        bail!("short simple descriptor")
    };
    ensure!(*actual == ep, "descriptor for wrong endpoint");
    let mut rest = mut_rest;
    let desc = Descriptor {
        profile: word(*p, *q),
        device: word(*d, *e),
        version: version & 15,
        servers: clusters(&mut rest)?,
        clients: clusters(&mut rest)?,
    };
    ensure!(rest.is_empty(), "trailing descriptor bytes");
    Ok(desc)
}
pub(crate) async fn interview(link: &mut impl InterviewIo, node:u16, ieee:[u8;8],source:IdentitySource,record:&mut Recorder<'_>) -> Result<()> {
    let admitted = source == IdentitySource::Announcement;
    record(Observation::Target{ieee:format!("{:016x}",u64::from_le_bytes(ieee)),short:node,identity_source:source})?;
    let mut t = Target { node, ieee, seq: 1,record };
    let sequence=t.seq;
    let discovered = t.zdo(link, 5, None).await?;
    let fallback = discovered.is_none();
    let eps = match discovered {
        Some(eps) => {
            (t.record)(Observation::ActiveEndpoints{sequence,endpoints:eps.clone()})?;
            println!("Active endpoints {eps:02x?}");
            eps
        }
        None => {
            println!(
                "No active endpoints response. Trying candidate EP 1 for the diagnostic probe."
            );
            vec![1]
        }
    };
    for ep in eps {
        let sequence=t.seq;
        let Some(raw) = t.zdo(link, 4, Some(ep)).await? else {
            println!("EP {ep}: no descriptor");
            if fallback {
                t.read(link, HA, ep, 6, &[0]).await?;
            }
            continue;
        };
        let d = descriptor(ep, &raw)?;
        println!(
            "EP {ep}: profile {:#06x}, device {:#06x}, servers {:04x?}, clients {:04x?}",
            d.profile, d.device, d.servers, d.clients
        );
        (t.record)(Observation::SimpleDescriptor{sequence,endpoint:ep,descriptor:d.clone()})?;
        if !(1..=240).contains(&ep) || !matches!(d.profile, HA | 0xc05e) {
            continue;
        }
        for &cluster in d.servers.iter().filter(|&&c| zigbee_devices::basic_io::supported(c)) {
            use zigbee_devices::basic_io::{PRESENT, probe};
            t.read(link, d.profile, ep, cluster, probe(cluster)).await?;
            let query = zigbee_zcl::discovery::Discover { start: PRESENT, maximum: 1 };
            if let Some(bytes) = t.foundation(link, d.profile, ep, cluster, Payload::DiscoverAttributesExtended(query)).await?
                && let Ok(Payload::DiscoverAttributesExtendedResponse(page)) = Payload::decode(Frame::decode(&bytes)?)
                && let [entry] = page.entries.as_slice()
                && entry.attribute.id == PRESENT {
                (t.record)(Observation::AttributeAccess { endpoint: ep, cluster, attribute: PRESENT,
                    data_type: entry.attribute.data_type, access: entry.access })?;
            }
        }
        use zigbee_devices::metering::{ELECTRICAL_MEASUREMENT, ELECTRICAL_PROBE, METERING, METERING_PROBE};
        for (cluster, attrs) in [(ELECTRICAL_MEASUREMENT, ELECTRICAL_PROBE), (METERING, METERING_PROBE)] {
            if d.servers.contains(&cluster) {
                t.read(link, d.profile, ep, cluster, attrs).await?;
            }
        }
        if d.servers.contains(&zigbee_devices::ias::CLUSTER) {
            t.read(link, d.profile, ep, zigbee_devices::ias::CLUSTER, &zigbee_devices::ias::PROBE).await?;
        }
        if d.servers.contains(&zigbee_devices::cover::CLUSTER) {
            t.read(link, d.profile, ep, zigbee_devices::cover::CLUSTER, &[0, 7, 8, 9]).await?;
            t.cover_commands(link, d.profile, ep).await?;
        }
        if d.servers.contains(&6) {
            t.read(link, d.profile, ep, 6, &[0]).await?;
        }
        if d.servers.contains(&0x0300) {
            t.read(link, d.profile, ep, 0x0300, &[0x400a, 0x400b, 0x400c]).await?;
        }
        if d.profile == HA && d.servers.contains(&zigbee_devices::hvac::THERMOSTAT) && zigbee_devices::hvac::thermostat_type(d.device) {
            t.read(link, d.profile, ep, zigbee_devices::hvac::THERMOSTAT, zigbee_devices::hvac::THERMOSTAT_PROBE).await?;
        }
        for &cluster in &d.servers {
            if !matches!(d.profile,HA | 0xc05e) {continue;}
            if cluster == zigbee_devices::equipment::DOOR_LOCK {
                let mut network_security=false;
                t.read_observing(link,d.profile,ep,cluster,&[zigbee_devices::equipment::SECURITY_LEVEL],|_,r|{
                    network_security=r.allows_lock_network_security();
                }).await?;
                if !network_security {continue;}
            }
            if d.profile==HA && d.device==3 && cluster==8 && d.servers.contains(&6) {
                t.read(link,d.profile,ep,cluster,&[2,3]).await?;
            }
            let mut optional: Vec<_> = zigbee_devices::equipment::FIELDS.iter()
                .filter(|f| f.cluster == cluster && !f.mandatory)
                .map(|f| f.id).collect();
            match cluster { 0x0301=>optional.extend([0,1]), 0x0200=>optional.extend([3,4,5,6,7,8,9,10,11,12,0x12]), 0x0103=>optional.push(3), 0x0502=>optional.push(0), 0x0201=>optional.extend([0x21,0x22]), _=>{} }
            for page in optional.chunks(6) {
                t.read(link, d.profile, ep, cluster, page).await?;
            }
        }
        if d.profile == HA && d.servers.contains(&zigbee_devices::hvac::FAN) {
            t.read(link, d.profile, ep, zigbee_devices::hvac::FAN, zigbee_devices::hvac::FAN_PROBE).await?;
        }
        if d.servers.contains(&0) {
            t.basic(link, ep, &d, admitted).await?;
        }
    }
    (t.record)(Observation::Finished)
}
/// Recheck an announced device on the existing network with admissions closed.
pub async fn probe(link: &mut Ezsp<'_>, previous: u16, ieee: [u8; 8]) -> Result<()> {
    probe_observed(link,previous,ieee, |_| Ok(())).await
}

pub async fn probe_observed(link:&mut Ezsp<'_>,previous:u16,ieee:[u8;8],mut record:impl FnMut(Observation)->Result<()>) -> Result<()> {
    let resolved = request(link, 0x60, &ieee).await?;
    let [lo, hi] = resolved[..] else {
        bail!("unexpected address lookup reply")
    };
    let node = word(lo, hi);
    let (node, source) = if node == 0xffff {
        ensure!((1..0xfff8).contains(&previous), "previous address is reserved");
        let reverse = request(link, 0x61, &previous.to_le_bytes()).await?;
        if let [0, found @ ..] = reverse.as_slice() {
            ensure!(
                found == ieee,
                "previous short address belongs to another device"
            );
            (previous, IdentitySource::AdapterLookup)
        } else {
            // After an NCP reset, ask the node itself before attributing observations.
            let mut target=Target{node:previous,ieee,seq:0,record:&mut |_| Ok(())};
            let mut req=vec![0];
            req.extend(previous.to_le_bytes());
            req.extend([0,0]);
            let candidate = link.destination(u64::from_le_bytes(ieee), previous)?;
            let reply=target.exchange(&mut Wire{link,keys:&mut Commissioning::default(),target:WireTarget::IdentityProbe(&candidate)},Aps{profile:0,cluster:1,src:0,dst:0},0x8001,req).await?;
            let Some([_,0,tail @ ..]) = reply.as_deref() else {
                bail!("device identity unconfirmed at previous short address")
            };
            ensure!(tail.len()==10 && tail[..8]==ieee && tail[8..]==previous.to_le_bytes(),
                "device returned a different or invalid address mapping");
            (previous, IdentitySource::DeviceReply)
        }
    } else {
        println!("NCP resolved target IEEE to {node:#06x}.");
        (node,IdentitySource::AdapterLookup)
    };
    let id = u64::from_le_bytes(ieee);
    if source == IdentitySource::AdapterLookup {
        ensure!(link.adapter_mapping(id, node), "adapter address assignment changed");
    }
    let destination = link.destination(id, node)?;
    counters(link).await?;
    let result = interview(&mut Wire{link,keys:&mut Commissioning::default(),target:WireTarget::Device(&destination)}, node, ieee,source,&mut record).await;
    counters(link).await?;
    result
}
pub async fn run(link: &mut Ezsp<'_>) -> Result<()> {
    run_observed(link, |_| Ok(())).await
}
/// Proof that this session interviewed the specified device. Admission verifies it
/// again before replacing any device description or publishing discovery.
pub struct InterviewReceipt(pub(crate) Destination);

pub async fn run_observed(link: &mut Ezsp<'_>, record: impl FnMut(Observation) -> Result<()>) -> Result<()> {
    interview_for_admission(link,record).await.map(drop)
}

pub async fn interview_for_admission(link: &mut Ezsp<'_>, mut record: impl FnMut(Observation) -> Result<()>) -> Result<InterviewReceipt> {
    counters(link).await?;
    let mut keys = Commissioning::opened();
    let join = async {
        open(link,WINDOW).await?;
        println!("INTERVIEW READY: pairing open for {WINDOW}s; listening for device announcements.");
        announcement(link, Instant::now() + Duration::from_secs(WINDOW.into()), &mut keys).await
    }.await;
    if let Err(error) = close(link).await {
        clear_transient_keys(link).await.context("clearing keys after failed pairing closure")?;
        return Err(error);
    }
    keys.admissions_closed(join.as_ref().ok().map(|d| d.ieee().to_le_bytes()), Instant::now());
    let result = match join {
        Ok(destination) => {
            let (node,ieee)=(destination.short(),destination.ieee().to_le_bytes());
            println!("Device announced at {node:#06x}, IEEE {:016x}", u64::from_le_bytes(ieee));
            interview(&mut Wire{link,keys:&mut keys,target:WireTarget::Device(&destination)},node,ieee,IdentitySource::Announcement,&mut record).await.map(|()| InterviewReceipt(destination))
        }
        Err(error) => Err(error),
    };
    // Attribute timeouts and a failed interview do not end the APS key exchange.
    keys.clean_if_due(link).await?;
    while let Some(end) = keys.deadline() {
        if let Some(cb) = keys.receive(link, end).await? {
            log_incoming(&cb)?;
        }
    }
    let receipt = result?;
    ensure!(link.current(&receipt.0), "interview assignment retired during key cleanup");
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_keeps_server_and_client_lists_separate() {
        let d = [1, 4, 1, 1, 1, 0, 2, 0, 0, 6, 0, 1, 0x19, 0];
        let parsed = descriptor(1, &d).unwrap();
        assert_eq!(parsed.servers, [0, 6]);
        assert_eq!(parsed.clients, [0x19]);
        assert!(descriptor(1, &d[..13]).is_err());
        assert!(descriptor(2, &d).is_err());
    }
    #[test]
    fn descriptor_correlation_checks_the_subject_on_success_and_error() {
        let active = [0x22, 0x34, 0x12];
        let simple = [0x22, 0x34, 0x12, 9];
        assert!(descriptor_reply_matches(5, &active, &[0x22, 0, 0x34, 0x12, 0]));
        assert!(!descriptor_reply_matches(5, &active, &[0x22, 0, 0x78, 0x56, 0]));
        assert!(!descriptor_reply_matches(5, &active, &[0x22, 0x89, 0x78, 0x56, 0]));
        assert!(descriptor_reply_matches(4, &simple, &[0x22, 0x89, 0x34, 0x12, 0]));
        // A late descriptor for endpoint 8 cannot finish endpoint 9's request.
        let mut reply = [0x22, 0, 0x34, 0x12, 8, 8, 4, 1, 0, 0, 0, 0, 0];
        assert!(!descriptor_reply_matches(4, &simple, &reply));
        reply[5] = 9;
        assert!(descriptor_reply_matches(4, &simple, &reply));
        assert!(!descriptor_reply_matches(4, &simple, &reply[..8]));
    }

    #[test]
    fn incoming_checks_the_reported_payload_length() {
        let mut b = [0u8; 20];
        b[11] = 19;
        b[12] = 211;
        b[13] = 190;
        b[18] = 1;
        assert_eq!(incoming(&b).unwrap().last_hop_lqi,211);
        assert_eq!(incoming(&b).unwrap().body, &[0]);
        assert!(incoming(&b[..19]).is_err());
        b[18] = 0;
        assert!(incoming(&b).unwrap().body.is_empty());
    }
}

#[cfg(test)]
mod cover_test;

#[cfg(test)]
mod tuya_test;
