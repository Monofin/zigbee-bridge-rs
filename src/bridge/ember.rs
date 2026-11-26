//! Multiplex MQTT controls and Ember callbacks in one adapter session.
use crate::availability::tracker::{Device,Tracker,wall_ms};
use super::responses::{self, Outcome};
use super::addresses::{Addresses, Query, Waiting, valid_pair};
use super::{Bindings, Identity, Read, endpoint};
use crate::{
    ash::{Callback, Destination, Ezsp, MemberUpdate, Next},
    interview::{incoming, inspect_callback},
    mqtt::{Command, Publisher, ReceivedCommand, SubmitError, discovery::Feature},
};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use zigbee_devices::measurement::{self, Measurement};
use zigbee_zcl::{
    Direction, Frame,
    attributes::Status,
    foundation::{GeneralCommand, Payload},
};

const APPLICATION_SEQUENCES:u8 = super::endpoint_admin::FIRST_SEQUENCE;
// A follow-up must have a different TSN while the completed operation still
// owns its old one. Limit the number in use, not which TSNs can be used.
const MAX_APPLICATION_IN_FLIGHT:usize=APPLICATION_SEQUENCES as usize-1;
pub(super) fn application_room(pending:&BTreeMap<u8,Pending>, count:usize)->bool {
    pending.len()+count<=MAX_APPLICATION_IN_FLIGHT
}
fn batch_start(cursor:u8,count:usize)->u8 {
    if usize::from(cursor)+count>usize::from(APPLICATION_SEQUENCES) {0} else {cursor}
}

pub(super) fn free_application_sequence(after:u8,pending:&BTreeMap<u8,Pending>)->Option<u8>{
    (1..u16::from(APPLICATION_SEQUENCES)).map(|i| ((u16::from(after)+i)%u16::from(APPLICATION_SEQUENCES)) as u8)
        .find(|tag|!pending.contains_key(tag))
}

/// Allocate a pending slot whose wire identity is also clear of late replies.
pub(super) fn available_tag(link: &Ezsp<'_>, pending: &BTreeMap<u8, Pending>,
    target: &Destination, profile: u16, cluster: u16, endpoints: (u8, u8), first: u8) -> Option<u8> {
    (0..APPLICATION_SEQUENCES).map(|offset|
        ((u16::from(first) + u16::from(offset)) % u16::from(APPLICATION_SEQUENCES)) as u8
    ).find(|tag| !pending.contains_key(tag) && link.application_sequence_free(target, profile, cluster, endpoints, *tag))
}

fn ready_refresh(pending:&BTreeMap<u8,Pending>,now:Instant,cursor:u8)->Option<(u8,u8,Identity,&'static str)> {
    let (&previous,p)=pending.iter().find(|(_,p)|p.refresh_deadline().is_some_and(|at|at<=now))?;
    let key = p.refresh_property()?;
    let next=if !pending.contains_key(&cursor) {cursor} else {free_application_sequence(previous,pending)?};
    Some((previous,next,p.peer,key))
}

/// The manufacturer code, when present, precedes the sequence number.
fn transaction_tag(frame: &[u8]) -> Result<u8> {
    Ok(Frame::decode(frame)?.header.sequence)
}

pub(super) struct Pending {
    pub(super) route_epoch: Option<u64>,
    pub(super) peer: Identity,
    pub(super) short: u16,
    pub(super) destination: Destination,
    pub(super) cluster: u16,
    pub(super) read: Option<Read>,
    pub(super) schedule: Option<Box<super::weekly_schedule::Job>>,
    pub(super) until: Instant,
}

impl Pending {
    /// A failed delivery or a write with no follow-up retires the entry.
    /// The settle interval starts after the adapter confirms delivery.
    fn delivered(&mut self, status:u8, now:Instant) -> bool {
        if status != 0 || (self.read.is_none() && self.schedule.is_none()) {
            return false;
        }
        if matches!(self.read, Some(Read::ConfirmOnOff(_))) {
            self.read = Some(Read::Refresh("state"));
            self.until = now;
        } else if matches!(self.read, Some(Read::LoopStop)) {
            self.read = Some(Read::Refresh("color"));
            self.until = now + Duration::from_millis(100);
        }
        true
    }
    fn schedule_setting_readback(&mut self, query: super::household::Query, frame: Frame<'_>, now: Instant) -> bool {
        use zigbee_devices::settings::Written;
        use zigbee_zcl::attributes::WriteStatus;
        if let super::household::Query::SettingWrite(name, Written::VerifyProtection) = query
            && matches!(Payload::decode(frame), Ok(Payload::WriteAttributesResponse(WriteStatus::Success))) {
            self.read=Some(Read::Refresh(name));
            self.until=now;
            return true;
        }
        false
    }
    fn refresh_property(&self) -> Option<&'static str> {
        match self.read {
            Some(Read::Refresh(key)) => Some(key),
            Some(Read::PowerReadback | Read::ConfirmOnOff(_)) => Some("state"),
            _ => None,
        }
    }

    // After a missing delivery callback or read response, issue one ordinary
    // GET. An ordinary GET has no follow-up, even when that reply is lost.
    fn recovery_read(&self) -> bool {
        matches!(self.read, Some(Read::ConfirmOnOff(_) | Read::PowerReadback))
    }

    fn phase(&self) -> &'static str {
        match self.read {
            Some(Read::ConfirmOnOff(_)) => "on/off delivery",
            Some(Read::PowerReadback) => "on/off confirmation",
            Some(Read::Light(zigbee_devices::lighting::Property::State)) => "on/off read",
            _ => "control",
        }
    }

    pub(super) fn refresh_deadline(&self) -> Option<Instant> {
        self.refresh_property().map(|_| self.until)
    }
}

/// One radio owner multiplexes queued callbacks, commands and read deadlines.
/// An external command only interrupts a cancellation-safe ASH byte receive.
pub async fn serve(
    link: &mut Ezsp<'_>,
    devices: &Bindings,
    out: &Publisher,
    commands: &mut mpsc::Receiver<Command>,
) -> Result<()> {
    let mut current = devices.clone();
    let mut addresses=Addresses::default();
    addresses.restore_departed(out.departed_members());
    link.manage_addresses(addresses.authority());
    let radio = serve_radio(link, addresses, &mut current, out, commands.into(), None);
    tokio::try_join!(devices.advertise(out), radio)?;
    Ok(())
}

pub enum CommandReceiver<'a> {
    Plain(&'a mut mpsc::Receiver<Command>),
    Managed(&'a mut mpsc::Receiver<ReceivedCommand>),
}
impl<'a> From<&'a mut mpsc::Receiver<Command>> for CommandReceiver<'a> {
    fn from(rx: &'a mut mpsc::Receiver<Command>) -> Self {
        Self::Plain(rx)
    }
}
impl<'a> From<&'a mut mpsc::Receiver<ReceivedCommand>> for CommandReceiver<'a> {
    fn from(rx: &'a mut mpsc::Receiver<ReceivedCommand>) -> Self {
        Self::Managed(rx)
    }
}
impl CommandReceiver<'_> {
    async fn recv(&mut self) -> Option<ReceivedCommand> {
        match self {
            Self::Plain(rx) => rx.recv().await.map(Into::into),
            Self::Managed(rx) => rx.recv().await,
        }
    }
}

/// Serve device and administrative requests on the same running adapter.
pub async fn serve_managed<'a>(
    link: &mut Ezsp<'_>,
    devices: &mut Bindings,
    out: &Publisher,
    commands: impl Into<CommandReceiver<'a>>,
    admin: &mut super::Management,
    requests: &mut mpsc::Receiver<crate::mqtt::admin::Request>,
) -> Result<()> {
    let mut addresses=Addresses::default();
    addresses.restore_departed(out.departed_members());
    link.manage_addresses(addresses.authority());
    admin.start(link, devices, out).await?;
    let discovery = devices.clone();
    let (published, ready) = tokio::sync::oneshot::channel();
    admin.initial_discovery = Some(ready);
    let publish = async {
        discovery.advertise(out).await?;
        let _ = published.send(());
        Ok(())
    };
    let radio = serve_radio(link, addresses, devices, out, commands.into(), Some((admin, requests)));
    tokio::try_join!(publish, radio)?;
    Ok(())
}

enum Input {
    Device(Option<ReceivedCommand>),
    Resume(Waiting),
    Admin(crate::mqtt::admin::Request),
    Interview(super::joining::Event),
    Timer,
    BatchStep,
    Refresh(u8, u8, Identity, &'static str),
}

fn register_peer(
    presence: &mut Tracker,
    devices: &Bindings,
    ieee: u64,
    announced_mains: Option<bool>,
) -> Result<()> {
    let Some((name, endpoints)) = devices.peers.get(&ieee) else { return Ok(()) };
    let application = endpoints.iter().filter(|(ep, d)|
        (1..=240).contains(*ep) && matches!(d.profile, 0x104 | 0xc05e));
    let selected = application.clone().find(|(_, d)| d.servers.contains(&0))
        .or_else(|| application.into_iter().next());
    let Some((&endpoint, descriptor)) = selected else { return Ok(()) };
    let now = Instant::now();
    let wall = wall_ms()?;
    let device = Device { ieee, name, endpoint, profile: descriptor.profile, mains: announced_mains,
        receiver_on: devices.receiver_on_when_idle.get(&ieee).copied() };
    presence.register(&devices.availability, device, now, wall);
    if announced_mains.is_some() {
        presence.admit(ieee, now);
        presence.seen(ieee, now, wall);
    }
    Ok(())
}

struct Batch {
    destination: Destination,
    id: Identity,
    until: Option<Instant>,
    command: ReceivedCommand,
    submitted: usize,
    steps: VecDeque<(u8, zigbee_devices::lighting::Request, Option<Read>)>,
}

impl Batch {
    fn interrupted(
        &self,
        out: &Publisher,
        uncertain: usize,
        unsent: usize,
        reason: &str,
    ) -> Result<()> {
        // Counts describe adapter handoff. A successful send response alone
        // says nothing about which operations the device has applied.
        let detail = serde_json::json!({"submitted":self.submitted,"outcome_unknown":uncertain,"unsent":unsent,"reason":reason});
        eprintln!("MQTT control interrupted: {detail}");
        if out.is_current(&self.command) {
            submitted(out.event(&self.command.device, "control_interrupted", detail))?;
        }
        Ok(())
    }

    async fn next(
        &mut self,
        link: &mut Ezsp<'_>,
        devices: &mut Bindings,
        out: &Publisher,
        pending: &mut BTreeMap<u8, Pending>,
        sequence: &mut u8,
    ) -> Result<bool> {
        if !link.current(&self.destination)
            || !out.is_current(&self.command)
            || self.until.is_some_and(|t| t <= Instant::now())
            || !devices.devices.contains_key(&self.id)
        {
            self.interrupted(
                out,
                0,
                self.steps.len(),
                "destination, route or deadline changed",
            )?;
            return Ok(false);
        }
        let Some((planned_tag, mut r, read)) = self.steps.pop_front() else {
            return Ok(false);
        };
        let profile = devices.peers[&self.id.0].1[&self.id.1].profile;
        let endpoints = (crate::interview::client_endpoint(r.cluster), r.endpoint);
        let Some(tag) = available_tag(
            link,
            pending,
            &self.destination,
            profile,
            r.cluster,
            endpoints,
            planned_tag,
        ) else {
            self.interrupted(
                out,
                0,
                self.steps.len() + 1,
                "no unambiguous transaction slot",
            )?;
            return Ok(false);
        };
        if tag != planned_tag {
            // Only the ZCL transaction byte changes; the payload and requested
            // effect were planned against the original endpoint.
            let offset = if r.frame[0] & 4 == 0 { 1 } else { 3 };
            r.frame[offset] = tag;
        }
        *sequence = tag.wrapping_add(1) % APPLICATION_SEQUENCES;
        let submission = if r.cluster == super::ace::CLUSTER {
            super::zdo::submit_protected_unicast(
                link,
                &self.destination,
                profile,
                r.cluster,
                endpoints,
                tag,
                &r.frame,
            )
            .await
        } else {
            super::zdo::submit_unicast(
                link,
                &self.destination,
                profile,
                r.cluster,
                endpoints,
                tag,
                &r.frame,
            )
            .await
        };
        match submission {
            Ok(0) => (),
            Ok(_) => {
                self.interrupted(out, 0, self.steps.len() + 1, "adapter declined frame")?;
                return Ok(false);
            }
            Err(e) if crate::ash::locally_refused(&e) => {
                self.interrupted(out, 0, self.steps.len() + 1, "destination retired")?;
                return Ok(false);
            }
            Err(e) => {
                let unsent = !e.is::<super::zdo::UncertainSubmission>();
                self.interrupted(
                    out,
                    usize::from(!unsent),
                    self.steps.len() + usize::from(unsent),
                    "radio submission interrupted",
                )?;
                return Err(e);
            }
        }
        self.submitted += 1;
        if self.command.operation == crate::mqtt::Operation::Set
            && ["effect", "color", "color_temp", "state"]
                .iter()
                .any(|k| self.command.properties.contains_key(*k))
            && let Some(b) = devices.devices.get_mut(&self.id)
        {
            b.confirmed_hue_scene = None;
        }
        pending.insert(
            tag,
            Pending {
                route_epoch: self.command.epoch(),
                peer: self.id,
                short: self.destination.short(),
                destination: self.destination.clone(),
                cluster: r.cluster,
                read,
                schedule: None,
                until: Instant::now() + Duration::from_secs(8),
            },
        );
        Ok(!self.steps.is_empty())
    }
}

fn retire_work(
    addresses: &mut Addresses,
    link: &Ezsp<'_>,
    poll: &mut super::poll::Client,
    mut manager: Option<&mut super::Management>,
    pending: &mut BTreeMap<u8, Pending>,
    deferred: &mut Option<(Waiting, usize)>,
    out: &Publisher,
) -> Result<()> {
    for revoked in addresses.take_revocations() {
        let short = revoked.short.unwrap_or(0xffff);
        poll.forget(revoked.ieee.unwrap_or(0), short);
        if let Some(m) = manager.as_deref_mut() {
            m.invalidate_address(link, revoked.ieee, short)?;
        }
        let affected =
            |p: &Pending| revoked.ieee == Some(p.peer.0) || revoked.short == Some(p.short);
        super::weekly_schedule::abort(
            pending,
            out,
            |_, p| affected(p) && !link.current(&p.destination),
            "address changed",
        )?;
        pending.retain(|_, p| !affected(p) || link.current(&p.destination));
        if deferred
            .as_ref()
            .is_some_and(|(w, _)| Some(w.peer.0) == revoked.ieee)
        {
            *deferred = None;
        }
    }
    Ok(())
}

async fn serve_radio(
    link: &mut Ezsp<'_>,
    mut addresses: Addresses,
    devices: &mut Bindings,
    out: &Publisher,
    mut commands: CommandReceiver<'_>,
    mut management: Option<(
        &mut super::Management,
        &mut mpsc::Receiver<crate::mqtt::admin::Request>,
    )>,
) -> Result<()> {
    let mut poll = super::poll::Client::default();
    let mut clock = super::time::Server::system();
    let mut presence = Tracker::new(&devices.availability, Instant::now())?;
    for &ieee in devices.peers.keys() {
        register_peer(&mut presence, devices, ieee, None)?;
        if addresses.departed(ieee) {presence.left(ieee,Instant::now());}
        else if out.manages_membership() {presence.admit(ieee,Instant::now());}
    }
    presence.reconcile_topics(out).await?;
    let mut initial_presence = true;
    let mut probe_sequence=0u8;
    let mut resolving_probe: Option<crate::availability::tracker::Probe> = None;
    let mut discovered = BTreeSet::new();
    let mut pending = BTreeMap::new();
    let mut sequence = 1u8;
    let mut batch:Option<Batch>=None;
    let mut deferred: Option<(Waiting, usize)> = None;
    println!(
        "MQTT RADIO READY: {} exposed endpoints",
        devices.devices.len()
    );
    loop {
        pending.retain(|_, p: &mut Pending| devices.devices.get(&p.peer).is_some_and(|b| out.route_is_current(&b.name, p.route_epoch)));
        retire_work(&mut addresses,link,&mut poll,management.as_mut().map(|(m,_)|&mut **m),&mut pending,&mut deferred,out)?;
        if let Some((manager,_)) = management.as_mut() {
            manager.start_next_interview(devices);
            if let Err(error)=manager.tick(link,out).await && !crate::ash::locally_refused(&error) {return Err(error);}
        }
        let now=Instant::now();
        addresses.tick(link, now).await?;
        if deferred.as_ref().is_some_and(|(w,_)| w.until.is_some_and(|t|t<=now)) {
            eprintln!("MQTT command expired waiting for a transaction slot after address discovery");
            deferred=None;
        }
        let due = presence.due(now, |target| link.current(target))?;
        // The address owner handles correlation, pacing and expiry. Resume only a
        // still-live availability probe and use the newly verified assignment.
        let mut ready = None;
        if let Some(probe) = resolving_probe.take().filter(|p| presence.awaiting_address(p.ieee)) {
            if let Some(short) = addresses.node(probe.ieee, now) {
                ready = Some((probe, short));
            } else if addresses.discovery_until(probe.ieee).is_some() {
                resolving_probe = Some(probe);
            } else {
                eprintln!("Availability address discovery ended without a current address for {:016x}", probe.ieee);
                presence.not_submitted(probe.ieee, now);
            }
        }
        if let Some(probe) = due {
            if let Some(short) = addresses.resolve_node(link, probe.ieee).await? {
                ready = Some((probe, short));
            } else if addresses.enqueue(Query::Network(probe.ieee), now)
                && let Some(until) = addresses.discovery_until(probe.ieee) {
                eprintln!("Availability awaiting network address for {:016x}", probe.ieee);
                presence.wait_for_address(probe.ieee, until);
                resolving_probe = Some(probe);
            } else {
                presence.not_submitted(probe.ieee, now);
            }
        }
        if let Some((probe, short)) = ready {
            probe_sequence = probe_sequence.wrapping_add(1);
            let read_version = [0, probe_sequence, 0, 0, 0];
            let accepted = match link.destination(probe.ieee,short) {
                Ok(target) => match super::zdo::submit_unicast(link,&target,probe.profile,0,(1,probe.endpoint),0xff,&read_version).await {
                    Ok(status)=>(status==0).then_some(target), Err(e) if crate::ash::locally_refused(&e)=>None, Err(e)=>return Err(e),
                },
                Err(_) => None,
            };
            if let Some(target)=accepted {presence.submitted(target,Instant::now());}
            else {presence.not_submitted(probe.ieee,Instant::now());}
        }
        presence.publish_due(out,now)?;
        if initial_presence && presence.all_states_queued() {
            out.device_snapshot_queued();
            initial_presence = false;
        }
        let devices_due=devices.household_due(out,now)?;
        super::weekly_schedule::abort(&mut pending,out,|_,p|!link.current(&p.destination),"assignment retired")?;
        super::weekly_schedule::abort(&mut pending,out,|_,p|now>=p.until,"response timed out")?;
        pending.retain(|tag, p: &mut Pending| {
            if !link.current(&p.destination) {return false;}
            if now < p.until || p.refresh_deadline().is_some() {
                return true;
            }
            eprintln!(
                "MQTT control timed out: transaction {tag}, endpoint {}, cluster {:#06x}, peer {:#06x}, phase {}",
                p.peer.1, p.cluster, p.short, p.phase()
            );
            false
        });
        poll.finish_idle(link, super::poll::Work {
            pending: &pending, deferred: deferred.as_ref().map(|(w,_)| w.peer.0),
            management: management.as_ref().map(|(m,_)| &**m),
        }, devices, now).await?;
        let slot_available = deferred.as_ref().is_some_and(|(w, count)| {
            let (cmd,id)=(&w.command,&w.peer);
            if super::equipment::busy(&pending,*id,cmd) || super::level_config::busy(&pending,*id,cmd) || super::hvac::busy(&pending,*id,cmd) {return false;}

            let start=batch_start(sequence,*count);
            application_room(&pending,*count) && (0..*count).all(|offset| !pending.contains_key(&(start+offset as u8)))
        });
        // A scheduled refresh keeps its old tag until we select the fresh one.
        let refresh=ready_refresh(&pending,now,sequence);
        let deadline = pending.values().map(|p|p.until).chain(addresses.deadline()).chain(deferred.as_ref().and_then(|(w,_)| w.until)).chain(devices_due).chain(poll.deadline()).chain(presence.deadline()).chain(management.as_ref().and_then(|(m,_)|m.deadline())).min();
        let wake = async {
            if batch.is_some() {return Ok(Input::BatchStep);}
            if let Some((previous,next,id,key))=refresh {return Ok(Input::Refresh(previous,next,id,key));}
            if slot_available {
                return Ok(Input::Resume(deferred.take().expect("available slot").0));
            }
            if deferred.is_none() && let Some(cmd) = addresses.ready(Instant::now()) {
                return Ok(Input::Resume(cmd));
            }
            let expiry = async {
                if let Some(end) = deadline {
                    tokio::time::sleep_until(end).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let admin=async {
                let Some((manager,rx))=management.as_mut() else {return std::future::pending().await;};
                tokio::select! {
                    r=rx.recv()=>Ok(Input::Admin(r.context("MQTT administration stopped")?)),
                    e=manager.interview_event()=>Ok(Input::Interview(e)),
                }
            };
            tokio::select! {
                cmd=commands.recv(), if deferred.is_none() => Ok(Input::Device(cmd)),
                r=admin => r,
                _=expiry => Ok::<_,anyhow::Error>(Input::Timer),
            }
        };
        let (next, source_eui) = link
            .callback_or_with_sender(wake)
            .await
            .map_err(|e| anyhow::anyhow!("bridge radio: {e:?}"))?;
        match next {
            Next::Callback(cb) => {
                let arrived = Instant::now();
                let observed_at_ms = wall_ms()?;
                inspect_callback(&cb)?;
                if cb.id == 0x7c && let [lo, hi] = cb.payload[..] {
                    let short = u16::from_le_bytes([lo, hi]);
                    addresses.conflict(short);
                    continue;
                }
                if let Some((ieee, short, change)) = cb.member_update() {
                    let admitted = change != MemberUpdate::Left;
                    let known = devices.peers.contains_key(&ieee);
                    if !addresses.membership(ieee, short, admitted, known, Instant::now()) {continue;}
                    if admitted || known {
                        out.membership_changed(ieee, admitted)?;
                        devices.mqtt_routes(out)?;
                        if admitted { presence.admit(ieee, Instant::now()); }
                        else { presence.left(ieee, Instant::now()); }
                    }
                    retire_work(&mut addresses,link,&mut poll,management.as_mut().map(|(m,_)|&mut **m),&mut pending,&mut deferred,out)?;
                    devices.household_leave(ieee);
                    for (_, binding) in devices.devices.range_mut((ieee, 0)..=(ieee, u8::MAX)) {
                        binding.confirmed_hue_scene = None;
                        binding.level_state = Default::default();
                    }
                }
                let mut resolved_sender = source_eui;
                let mut corroborated = false;
                if cb.id == 0x45 {
                    let msg = incoming(&cb.payload)?;
                    if msg.aps.profile != 0 && sensitive_cluster(msg.aps.cluster) && !observation(&msg) {
                        if addresses.disputed(msg.sender) {continue;}
                        if let Some(ieee) = source_eui {
                            if !valid_pair(ieee, msg.sender) || addresses.departed(ieee) {continue;}
                            if addresses.contradicts(ieee, msg.sender) {
                                addresses.dispute(ieee, msg.sender);
                                continue;
                            }
                            let Some(adapter_ieee) = super::zdo::lookup_ieee(link, msg.sender).await? else {continue;};
                            if adapter_ieee != ieee {
                                addresses.dispute(ieee, msg.sender);
                                continue;
                            }
                        }
                    }
                    let response = msg.aps.profile == 0 && matches!(msg.aps.cluster, 0x8000 | 0x8001);
                    let report = application_message(&msg) && observation(&msg);
                    let announcement = (msg.aps.profile,msg.aps.cluster,msg.aps.src,msg.aps.dst) == (0,0x13,0,0);
                    let address = if response {
                        addresses.response(&msg, source_eui, |id| devices.peers.contains_key(&id), Instant::now())
                    } else if announcement {
                        let Some(ieee) = announced(&msg,source_eui) else {continue};
                        Some((ieee, msg.sender))
                    } else if matches!(msg.kind,0|1|2|4) && (!sensitive_cluster(msg.aps.cluster) || report) {
                        source_eui.map(|ieee| (ieee, msg.sender))
                    } else {None};
                    let subject = address.map_or(msg.sender, |(_, node)| node);
                    let via_neighbor = subject != msg.sender;
                    let reported_ieee = address.map(|(ieee, _)| ieee);
                    let mut identity = reported_ieee;
                    if report && (reported_ieee.is_none() || sensitive_cluster(msg.aps.cluster)) {
                        let adapter = super::zdo::lookup_ieee(link, msg.sender).await?;
                        if let (Some(reported), Some(known)) = (reported_ieee, adapter) && reported != known {
                            addresses.dispute(reported, msg.sender);
                            continue;
                        }
                        if addresses.disputed(msg.sender) {
                            // Discover the new owner without saving input from the disputed assignment.
                            addresses.enqueue(Query::Ieee(msg.sender), Instant::now());
                            continue;
                        }
                        corroborated = adapter.is_some();
                        identity = reported_ieee.or(adapter).or_else(|| addresses.observation_sender(msg.sender, Instant::now()));
                    }
                    if let Some(ieee) = identity {
                        if !valid_pair(ieee,subject) || addresses.departed(ieee) {continue;}
                        if addresses.contradicts(ieee, subject) {
                            addresses.dispute(ieee, subject);
                            continue;
                        }
                        if devices.peers.contains_key(&ieee) {
                            let held = if announcement { Vec::new() }
                                else { addresses.take_observations(subject, Instant::now()) };
                            if address.is_some() {
                                let now = Instant::now();
                                let learned = if via_neighbor { addresses.learn_neighbor(ieee, subject, now) }
                                    else { addresses.learn(ieee, subject, now) };
                                if !learned { continue; }
                            }
                            let Ok(destination) = link.destination(ieee, subject) else { continue };
                            if response && !via_neighbor && !held.is_empty() && link.current(&destination) {
                                // The address reply itself is fresh radio activity.
                                // Releasing an older report must not renew its timestamp.
                                presence.seen(ieee, Instant::now(), wall_ms()?);
                                devices.observed_last_seen(out, ieee, std::time::SystemTime::now())?;
                            }
                            for (received, observed_at_ms, saved) in held {
                                if !link.current(&destination) { break; }
                                addresses.remember_recovered(ieee, subject, &saved, Instant::now());
                                let msg = incoming(&saved.payload)?;
                                let result = publish_held(link, devices, out, &mut pending, &mut discovered, &destination, &msg, received, observed_at_ms).await;
                                if let Err(error) = result && !crate::ash::locally_refused(&error) {return Err(error);}
                            }
                            if report {
                                if addresses.already_recovered(ieee, subject, &cb, Instant::now()) { continue; }
                                resolved_sender = Some(ieee);
                            }
                        } else if report {continue;}
                    } else if report {
                        addresses.hold_observation(msg.sender, cb, arrived, observed_at_ms);
                        continue;
                    }
                    if response {continue;}
                }
                if let Some((m,_))=management.as_mut() && let Err(e)=m.observe(link,out,devices,&cb).await {if crate::ash::locally_refused(&e) {continue;} return Err(e);}
                let handled=receive(link, cb, observed_at_ms, resolved_sender, corroborated, &mut addresses, devices, out, &mut pending, &mut discovered,&mut presence, management.as_mut().map(|(m,_)| &mut **m), &mut clock, &mut poll, deferred.as_ref().map(|(w,_)| w.peer.0)).await;
                if let Err(e)=handled && !crate::ash::locally_refused(&e) {return Err(e);}
            }
            Next::Wake(command) => {
                let (cmd,refresh,expected,until,original)=match command? {
                    Input::Device(cmd)=>(cmd.context("MQTT command worker stopped")?,None,None,None,None),
                    Input::Resume(w)=>(w.command,w.refresh.map(|key|(w.peer,key)),Some(w.peer),w.until,w.original),
                    Input::Timer=>continue,
                    Input::BatchStep=>{
                        let active=batch.as_mut().expect("scheduled batch");
                        if !active.next(link,devices,out,&mut pending,&mut sequence).await? {batch=None;}
                        continue;
                    }
                    Input::Refresh(old_tag,next,id,key)=>{
                        let Some(previous)=pending.remove(&old_tag) else {continue;};
                        let recovery = previous.recovery_read();
                        if recovery {
                            eprintln!("MQTT control timed out: transaction {old_tag}, endpoint {}, cluster {:#06x}, peer {:#06x}, phase {}; checking current power once",
                                previous.peer.1, previous.cluster, previous.short, previous.phase());
                        }
                        sequence = next;
                        let Some(b) = devices.devices.get(&id) else {
                            continue;
                        };
                        (
                            ReceivedCommand::followup(Command {
                                device: b.name.clone(),
                                operation: crate::mqtt::Operation::Get,
                                properties: serde_json::Map::from_iter([(key.into(), "".into())]),
                            }, previous.route_epoch),
                            (!recovery).then_some((id, key)),Some(id),None,Some(previous.destination),
                        )
                    }
                    Input::Admin(req)=>{management.as_mut().unwrap().0.handle(link,devices,out,&mut presence,req).await?;continue;}
                    Input::Interview(event) => {
                        let manager = &mut management.as_mut().unwrap().0;
                        if let Some((ieee,mains)) = manager.interview_work(event,link,devices,out).await? {
                            register_peer(&mut presence, devices, ieee, Some(mains))?;
                            presence.reconcile_topics(out).await?;
                        }
                        continue;
                    }
                };
                if original.as_ref().is_some_and(|d| !link.current(d)) || until.is_some_and(|t|t <= Instant::now()) || !out.is_current(&cmd) {
                    continue;
                }
                match super::weekly_schedule::prepare(devices, &cmd) {
                    Ok(Some((peer, job))) => {
                        let node = addresses.resolve_node(link, peer.0).await?;
                        let now = Instant::now();
                        if until.is_some_and(|deadline| now >= deadline) { continue; }
                        let Some(short) = node else {
                            addresses.hold(Waiting { peer, command: cmd, refresh: None, until, original }, now);
                            continue;
                        };
                        let Ok(target) = link.destination(peer.0, short) else { continue };
                        if original.is_some_and(|prior| !target.same_assignment(&prior)) { continue; }
                        super::weekly_schedule::start(
                            link,
                            out,
                            &mut pending,
                            peer,
                            job,
                            target,
                            &mut sequence,
                        )
                        .await?;
                        continue;
                    }
                    Err(e) => {
                        if devices.topics.contains_key(&cmd.device) {
                            submitted(out.event(&cmd.device,"weekly_schedule_rejected",e.to_string().into()))?;
                        }
                        eprintln!("MQTT schedule rejected: {e}");continue;
                    }
                    Ok(None)=>{}
                }
                use zigbee_devices::standard_controls::level_config::{Field, PROPERTY};
                let plan=if let Some((id,PROPERTY))=refresh {
                    // The normal level_config GET queries each attribute separately.
                    // Read both in a single transaction for this write's follow-up.
                    let Some(binding)=devices.devices.get(&id) else {continue;};
                    if binding.is_light && binding.controls.level_config {
                        Field::read_both(id.1,sequence).map(|req|
                            (id,vec![(req,Some(Read::LevelConfig(super::level_config::Operation::Readback)))]))
                    } else { Err(anyhow::anyhow!("level configuration no longer available")) }
                } else {devices.plan(&cmd,sequence)};
                let plan=plan.and_then(|(id,steps)| {
                    anyhow::ensure!(refresh.is_none_or(|(peer,_)| peer==id && steps.len()==1),"refresh must address one request to its original endpoint");
                    anyhow::ensure!(steps.len()<=MAX_APPLICATION_IN_FLIGHT,"too many operations in one MQTT command");
                    if batch_start(sequence,steps.len())!=sequence {
                        sequence=0;
                        devices.plan(&cmd,sequence)
                    } else {Ok((id,steps))}
                });
                match plan {
                    Err(e) => eprintln!("MQTT control rejected: {e}"),
                    Ok((id, steps)) => {
                        if expected.is_some_and(|peer|peer!=id) {continue;}
                        let steps = steps.into_iter().map(|(request, read)| {
                            let read = match (refresh, read) {
                                (Some((_, "state")), Some(Read::Light(zigbee_devices::lighting::Property::State))) => Some(Read::PowerReadback),
                                (_, read) => read,
                            };
                            let tag = transaction_tag(&request.frame)?;
                            Ok((tag, request, read))
                        }).collect::<Result<Vec<_>>>()?;
                        if !application_room(&pending,steps.len()) || steps.iter().any(|(tag, _, _)| pending.contains_key(tag)) || super::equipment::busy(&pending,id,&cmd) || super::level_config::busy(&pending,id,&cmd) || super::hvac::busy(&pending,id,&cmd) {
                            let original=original.or_else(|| addresses.node(id.0,Instant::now()).and_then(|n|link.destination(id.0,n).ok()));
                            deferred = Some((Waiting{original,peer:id,command:cmd,refresh:refresh.map(|(_,key)|key),until},steps.len()));
                        } else {
                            let now = Instant::now();
                            if addresses.departed(id.0) { continue; }
                            let sensitive = steps.iter().any(|(_,r,_)| sensitive_cluster(r.cluster));
                            let node = if sensitive {
                                super::zdo::lookup_node(link,id.0).await?.filter(|&n|addresses.adapter_agrees(id.0,n))
                            } else {addresses.resolve_node(link, id.0).await?};
                            if until.is_some_and(|t|t <= Instant::now()) {continue;}
                            if let Some(node) = node {
                                let Ok(destination)=link.destination(id.0,node) else {continue;};
                                if original.as_ref().is_some_and(|d| !destination.same_assignment(d)) {continue;}
                                batch=Some(Batch {
                                    destination, id, until, command:cmd, submitted:0,
                                    steps:steps.into(),
                                });
                            } else {
                                if sensitive {
                                    eprintln!("MQTT control rejected: adapter could not verify the destination");
                                } else {
                                    addresses.hold(Waiting{original,peer:id,command:cmd,refresh:refresh.map(|(_,key)|key),until}, now);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn sensitive_cluster(cluster: u16) -> bool {
    matches!(cluster, 0x0101 | 0x0500 | 0x0501 | 0x0019)
}

/// Only unsolicited observations may wait for identity. A delayed service
/// request or transaction reply may have lost its original recipient or deadline.
fn observation(msg: &crate::interview::Incoming<'_>) -> bool {
    let Ok(frame) = Frame::decode(msg.body) else { return false };
    let cluster = msg.aps.cluster;
    if cluster == zigbee_devices::ias::CLUSTER {
        return frame.header.direction == Direction::ServerToClient
            && zigbee_devices::ias::observe(frame, None).status.is_some()
            && (frame.header.general_command() == Some(GeneralCommand::ReportAttributes)
                || frame.header.frame_type == zigbee_zcl::FrameType::ClusterSpecific);
    }
    if cluster == zigbee_devices::tuya::household::CLUSTER {
        return zigbee_devices::tuya::household::unsolicited_report(frame);
    }
    if sensitive_cluster(cluster) { return false; }
    if frame.header.general_command() == Some(GeneralCommand::ReportAttributes) {
        return frame.header.direction == Direction::ServerToClient;
    }
    if frame.header.frame_type != zigbee_zcl::FrameType::ClusterSpecific { return false; }
    match frame.header.direction {
        Direction::ClientToServer => super::remote::cluster_name(cluster).is_some() || cluster == 0xfc80,
        Direction::ServerToClient => cluster == 9 && frame.header.command_id == 0,
    }
}

fn application_message(msg: &crate::interview::Incoming<'_>) -> bool {
    matches!(msg.kind, 0 | 1 | 2 | 4) && matches!(msg.aps.profile, 0x104 | 0xc05e)
        && (msg.aps.dst == 1 || msg.aps.dst == crate::interview::client_endpoint(msg.aps.cluster))
}

async fn publish_held(
    link: &mut Ezsp<'_>, devices: &mut Bindings, out: &Publisher, pending: &mut BTreeMap<u8, Pending>,
    discovered: &mut BTreeSet<(Identity, u16, u16)>, destination: &Destination,
    msg: &crate::interview::Incoming<'_>, received: Instant, observed_at_ms: u64,
) -> Result<()> {
    let peer = (destination.ieee(), msg.aps.src);
    let Some(descriptor) = devices.peers.get(&peer.0).and_then(|(_, endpoints)| endpoints.get(&peer.1))
        else { return Ok(()) };
    let frame = Frame::decode(msg.body)?;
    if descriptor.profile != msg.aps.profile || !observation(msg) || !valid_application_endpoint(msg, frame) {
        return Ok(());
    }
    // A historical observation cannot finish a newer transaction, refresh
    // last_seen, update the alarm panel, or generate a late protocol response.
    if msg.aps.cluster == 9
        && let Ok(zigbee_zcl::cluster::Command::Alarms(zigbee_zcl::cluster::Alarms::Alarm { alarm_code, cluster_id })) =
            zigbee_zcl::cluster::Command::decode(9, frame) {
        if descriptor.servers.contains(&9) {
            submitted(out.event(&devices.peers[&peer.0].0, &format!("zcl_{:02x}_alarm",peer.1),
                serde_json::json!({"cluster":cluster_id,"code":alarm_code})))?;
        }
        return Ok(());
    }
    dispatch(link, devices, out, &mut BTreeMap::new(), discovered, peer, msg, frame,
        &mut Outcome::initial(msg.aps.cluster, frame), received, observed_at_ms).await?;
    confirm_light_power(link, devices, pending, peer, destination).await
}

fn valid_application_endpoint(msg: &crate::interview::Incoming<'_>, frame: Frame<'_>) -> bool {
    match frame.header.direction {
        Direction::ClientToServer => msg.aps.dst == 1 || (msg.aps.dst == 2 && msg.aps.cluster == 0x0501),
        Direction::ServerToClient => msg.aps.dst == crate::interview::client_endpoint(msg.aps.cluster)
            || frame.header.general_command() == Some(GeneralCommand::ReportAttributes),
    }
}

fn announced(msg: &crate::interview::Incoming<'_>, source: Option<u64>) -> Option<u64> {
    if (msg.aps.profile,msg.aps.cluster,msg.aps.src,msg.aps.dst) == (0,0x13,0,0)
        && let [_,lo,hi,bytes @ ..,_] = msg.body
        && let Ok(bytes) = <[u8;8]>::try_from(bytes)
        && u16::from_le_bytes([*lo,*hi]) == msg.sender {
        let ieee = u64::from_le_bytes(bytes);
        if source.is_none_or(|s| s == ieee) { return Some(ieee); }
    }
    None
}

// An adapter can resolve the destination of a read and miss the reverse
// lookup for its reply. The request's lease is sufficient for this reply
// alone. It grants no routing authority to subsequent unsolicited traffic.
fn pending_power_sender(
    msg: &crate::interview::Incoming<'_>, pending: &BTreeMap<u8, Pending>,
    devices: &Bindings, link: &Ezsp<'_>,
) -> Option<u64> {
    if !matches!(msg.kind, 0 | 1) || msg.aps.cluster != 6
        || msg.aps.dst != crate::interview::client_endpoint(6) { return None; }
    let frame = Frame::decode(msg.body).ok()?;
    let p = pending.get(&frame.header.sequence)?;
    if !matches!(p.read, Some(Read::PowerReadback | Read::Light(zigbee_devices::lighting::Property::State)))
        || p.short != msg.sender || p.peer.1 != msg.aps.src || p.cluster != msg.aps.cluster
        || !link.current(&p.destination) { return None; }
    let binding = devices.devices.get(&p.peer)?;
    if binding.descriptor.profile != msg.aps.profile { return None; }
    let ep = endpoint(p.peer.1, &binding.descriptor);
    let light = binding.light(&ep).ok()?;
    let power = light.read_response(zigbee_devices::lighting::Property::State, frame.header.sequence, 6, frame);
    (matches!(power, Ok(Some(_))) || declined_power_read(frame).is_some()).then_some(p.peer.0)
}

#[allow(clippy::too_many_arguments)]
async fn receive(
    link: &mut Ezsp<'_>,
    cb: Callback,
    observed_at_ms: u64,
    source_eui: Option<u64>,
    corroborated: bool,
    addresses: &mut Addresses,
    devices: &mut Bindings,
    out: &Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    discovered: &mut BTreeSet<(Identity, u16, u16)>,
    presence:&mut Tracker,
    mut management: Option<&mut super::Management>,
    clock: &mut super::time::Server,
    poll: &mut super::poll::Client,
    deferred: Option<u64>,
) -> Result<()> {
    if cb.id == 0x3f
        && let [0, lo, hi, pl, ph, cl, ch, local, endpoint, _, _, _, _, _, tag, status, ..] = cb.payload.as_slice()
        && let Some(p) = pending.get_mut(tag)
        && link.current(&p.destination)
        && p.short == u16::from_le_bytes([*lo, *hi])
        && p.cluster == u16::from_le_bytes([*cl,*ch])
        && *local == crate::interview::client_endpoint(p.cluster)
        && p.peer.1 == *endpoint
        && devices.devices.get(&p.peer).is_some_and(|b| b.descriptor.profile == u16::from_le_bytes([*pl,*ph]))
    {
        if *status != 0 {
            eprintln!("MQTT control delivery failed: Ember {status:#04x}, transaction {tag}, peer {:#06x}, endpoint {}, cluster {:#06x}, phase {}", p.short, p.peer.1, p.cluster, p.phase());
        }
        if !p.delivered(*status,Instant::now()) {
            super::weekly_schedule::abort(pending,out,|t,_|t==*tag,"delivery failed")?;
            pending.remove(tag);
        }
    }
    if cb.id != 0x45 {
        return Ok(());
    }
    let msg = incoming(&cb.payload)?;
    let application = application_message(&msg);
    if !matches!(msg.kind,0|1|2|4) || (!application && !presence.has_peers() && !devices.last_seen_enabled()) {
        return Ok(());
    }
    let now = Instant::now();
    if !(1..0xfff8).contains(&msg.sender) { return Ok(()); }
    // Security-sensitive exchanges keep their independent adapter identity check.
    let observational = application && observation(&msg);
    let sensitive = sensitive_cluster(msg.aps.cluster) && application && !observational;
    let identity = if !sensitive && source_eui.is_some() {
        source_eui
    } else {
        match super::zdo::lookup_ieee(link, msg.sender).await? {
            Some(ieee) => Some(ieee),
            None if !sensitive => addresses.ieee(msg.sender,Instant::now())
                .or_else(|| pending_power_sender(&msg, pending, devices, link)),
            None => None,
        }
    };
    let Some(ieee) = identity else {
        if application && !sensitive { addresses.enqueue(Query::Ieee(msg.sender), now); }
        return Ok(());
    };
    if !valid_pair(ieee,msg.sender) || !devices.peers.contains_key(&ieee)
        || addresses.departed(ieee) || source_eui.is_some_and(|claimed|claimed!=ieee) { return Ok(()); }
    let destination=link.destination(ieee,msg.sender)?;
    if !link.current(&destination) {return Ok(());}
    let peer = (ieee,msg.aps.src);
    if let Some(manager)=management.as_mut(){manager.zones.peer_seen(ieee);}
    // Identity lookup may have consumed and buffered another lifecycle event.
    if let Ok(f)=Frame::decode(msg.body) && pending.get(&f.header.sequence).is_some_and(|p|!link.current(&p.destination)) {
        super::weekly_schedule::abort(pending,out,|tag,_|tag==f.header.sequence,"assignment retired")?;
        pending.remove(&f.header.sequence);
    }
    presence.seen(peer.0,Instant::now(),wall_ms()?);
    devices.observed_last_seen(out,peer.0,std::time::SystemTime::now())?;
    if !application{return Ok(());}
    if msg.aps.cluster==zigbee_devices::equipment::DOOR_LOCK && !devices.allows_network_security(peer.0) {
        return Ok(());
    }
    let (_, endpoints) = &devices.peers[&peer.0];
    let Some(descriptor) = endpoints.get(&peer.1) else {return Ok(())};
    if descriptor.profile != msg.aps.profile {return Ok(())}
    let Ok(frame) = Frame::decode(msg.body) else {return Ok(())};
    let local = crate::interview::client_endpoint(msg.aps.cluster);
    if msg.aps.cluster == super::ace::CLUSTER && !msg.aps_encrypted {return Ok(());}
    // A keypad may carry a credential. Consume all of its client commands here,
    // before generic MQTT events or raw command handling can see the payload.
    if msg.aps.cluster == super::ace::CLUSTER && frame.header.direction == Direction::ClientToServer
        && frame.header.frame_type == zigbee_zcl::FrameType::ClusterSpecific {
        if msg.aps.dst == local && let Some(manager) = management.as_ref() {
            manager.keypad(link, devices, peer, &msg, frame).await?;
        }
        return Ok(());
    }
    if !valid_application_endpoint(&msg, frame) { return Ok(()); }
    let mut outcome = Outcome::initial(msg.aps.cluster, frame);
    let unicast = matches!(msg.kind, 0 | 1);
    if (!observational || corroborated) && msg.aps.cluster == zigbee_devices::ias::CLUSTER && let Some(manager) = management.as_mut() {
        manager.zones.observe_ace_status(peer,&msg,frame,&destination);
    }
    if unicast && let Some(manager) = management.as_mut()
        && manager.endpoint_ops.received(link,devices,out,peer, &msg, frame).await? {
        return responses::finish(link, &destination, &msg, frame, Outcome::Success).await;
    }
    if msg.aps.cluster == 9 && descriptor.servers.contains(&9)
        && let Ok(zigbee_zcl::cluster::Command::Alarms(zigbee_zcl::cluster::Alarms::Alarm { alarm_code, cluster_id }))
            = zigbee_zcl::cluster::Command::decode(9, frame) {
        submitted(out.event(&devices.peers[&peer.0].0, &format!("zcl_{:02x}_alarm",peer.1),
            serde_json::json!({"cluster":cluster_id,"code":alarm_code})))?;
        return responses::finish(link, &destination, &msg, frame, Outcome::Success).await;
    }
    let h = frame.header;
    if unicast && msg.aps.cluster == zigbee_devices::tuya::household::CLUSTER
        && let Some(epoch) = devices.devices.get(&peer).and_then(|b| b.datapoints).and_then(|d| d.time_epoch())
        && epoch.is_request(frame)
    {
        let now = jiff::Timestamp::now();
        let status = match epoch.response(frame, now.as_second(), clock.utc_offset_seconds(now)) {
            Ok(wire) => return responses::send(link, &destination, &msg, &wire).await,
            Err(zigbee_devices::tuya::TimeError::Malformed) => Outcome::Malformed,
            Err(zigbee_devices::tuya::TimeError::Unavailable) => Outcome::Failure,
        };
        return responses::finish(link, &destination, &msg, frame, status).await;
    }
    if unicast && msg.aps.cluster == zigbee_devices::tuya::household::CLUSTER
        && let Some(binding) = devices.devices.get(&peer)
        && let Some(service) = zigbee_devices::tuya::services::incoming(frame)
        && binding.datapoints.is_some_and(|device| device.supports(service))
    {
        use zigbee_devices::tuya::{Service, services};
        let status = match service {
            Service::GatewayStatus if frame.payload.len() != 2 => Outcome::Malformed,
            Service::GatewayStatus => {
                if let Some(online) = devices.tuya_gateway_internet
                    && let Some(wire) = services::gateway_reply(frame, online)
                {
                    return responses::send(link, &destination, &msg, &wire).await;
                }
                Outcome::Failure
            }
            Service::McuVersion => match frame.payload {
                [_, _, v] => {
                    if let Some(version) = services::version(*v) {
                        submitted(out.state(&binding.device_name, "mcu_version", version.into()))?;
                    }
                    Outcome::Success
                }
                _ => Outcome::Malformed,
            },
            _ => Outcome::UnsupportedCommand,
        };
        return responses::finish(link, &destination, &msg, frame, status).await;
    }
    if unicast && msg.aps.cluster == super::poll::CLUSTER && h.direction == Direction::ServerToClient
        && h.frame_type == zigbee_zcl::FrameType::ClusterSpecific && h.command_id == 0 && h.manufacturer.is_none() {
        return poll.check_in(link, peer, &msg, frame, super::poll::Work {pending, deferred, management:management.as_deref()}).await;
    }
    if unicast && msg.aps.cluster == super::time::CLUSTER && h.direction == Direction::ClientToServer
        && h.frame_type == zigbee_zcl::FrameType::General && h.manufacturer.is_none() {
        match clock.handle(frame, jiff::Timestamp::now()) {
            Ok(Some(wire)) => return responses::send(link, &destination, &msg, &wire).await,
            Ok(None) => return Ok(()),
            Err(status) => return responses::finish(link, &destination, &msg, frame, status).await,
        }
    }
    if management.is_some() && unicast && h.frame_type == zigbee_zcl::FrameType::ClusterSpecific && h.manufacturer.is_none()
        && ((msg.aps.profile == 0x0104 && msg.aps.cluster == 0x0019 && h.direction == Direction::ClientToServer)
        || (msg.aps.cluster == 0x0500 && h.direction == Direction::ServerToClient && h.command_id == 1
            && zigbee_zcl::cluster::Command::decode(0x0500, frame).is_ok())) {
        // These transactions belong to the OTA server or the enrollment state machine.
        return Ok(());
    }
    dispatch(link, devices, out, pending, discovered, peer, &msg, frame, &mut outcome, now, observed_at_ms).await?;
    responses::finish(link, &destination, &msg, frame, outcome).await?;
    confirm_light_power(link, devices, pending, peer, &destination).await
}

// A level or colour observation cannot establish whether the light is on. Resolve
// that missing attribute using the normal correlated read-response path.
async fn confirm_light_power(
    link: &mut Ezsp<'_>, devices: &mut Bindings, pending: &mut BTreeMap<u8, Pending>,
    peer: Identity, target: &Destination,
) -> Result<()> {
    use zigbee_devices::lighting::Property;
    let Some(binding) = devices.devices.get_mut(&peer) else { return Ok(()) };
    if !binding.needs_light_power || !binding.permits(Property::State, |a| a.read)
        || !application_room(pending, 1) || !link.current(target)
        || pending.values().any(|p| p.peer == peer && p.destination.same_assignment(target)
            && matches!(p.read, Some(Read::Light(Property::State) | Read::ConfirmOnOff(_) | Read::PowerReadback | Read::Refresh("state")))) {
        return Ok(());
    }
    let profile = binding.descriptor.profile;
    let endpoints = (crate::interview::client_endpoint(6), peer.1);
    let Some(tag) = available_tag(link, pending, target, profile, 6, endpoints, 0) else { return Ok(()) };
    let ep = endpoint(peer.1, &binding.descriptor);
    let lamp = binding.light(&ep)?;
    if !lamp.supports(Property::State) {
        binding.needs_light_power = false;
        return Ok(());
    }
    let request = lamp.get(Property::State, tag)?;
    binding.needs_light_power = false;
    if super::zdo::submit_unicast(link, target, profile, request.cluster, endpoints, tag, &request.frame).await? == 0 {
        pending.insert(tag, Pending {
            route_epoch: None, peer, short: target.short(), destination: target.clone(),
            cluster: request.cluster, read: Some(Read::Light(Property::State)), schedule: None,
            until: Instant::now() + Duration::from_secs(8),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    link: &mut Ezsp<'_>, devices: &mut Bindings, out: &Publisher, pending: &mut BTreeMap<u8, Pending>,
    discovered: &mut BTreeSet<(Identity, u16, u16)>, peer: Identity, msg: &crate::interview::Incoming<'_>,
    frame: Frame<'_>, outcome: &mut Outcome, received: Instant, observed_at_ms: u64,
) -> Result<()> {
    let (name, endpoints) = &devices.peers[&peer.0];
    let descriptor = &endpoints[&peer.1];
    let unicast = matches!(msg.kind, 0 | 1);
    let group_report = matches!(msg.kind, 2 | 4) && !sensitive_cluster(msg.aps.cluster)
        && frame.header.direction == Direction::ServerToClient
        && frame.header.general_command() == Some(GeneralCommand::ReportAttributes);
    // A group report may update many listeners. It cannot complete any
    // listener's outstanding unicast request, even with the same sequence.
    let mut no_transactions = BTreeMap::new();
    let pending = if unicast { pending } else { &mut no_transactions };
    let accepts_report = unicast || group_report;
    if accepts_report && zigbee_devices::basic_io::supported(msg.aps.cluster)
        && let Some(channel) = devices.devices.get_mut(&peer)
            .and_then(|b| b.io.iter_mut().find(|c|c.layout.cluster()==msg.aps.cluster)) {
        super::basic_io::service_report(channel,frame);
    }
    if unicast && msg.aps.cluster==0x0201
        && super::weekly_schedule::receive(link,out,pending,peer,msg.sender,frame).await? {
        *outcome=Outcome::Success;return Ok(());
    }
    let binding = devices.devices.get(&peer);
    if let Some(binding) = binding {
        submitted(out.state(&binding.device_name,"linkquality",msg.last_hop_lqi.into()))?;
    }
    let generic_reports = devices.all_standard_reports || binding.is_none_or(|b| b.preserve_raw_reports);
    if (generic_reports || devices.has_remote_trigger(peer,descriptor)) && let Some(event) = super::standard::client_command(descriptor,msg.aps.cluster,frame) {
        *outcome = Outcome::Success;
        submitted(out.event(name,&format!("zcl_{:02x}_command",peer.1),event))?;
    }
    if accepts_report && descriptor.servers.contains(&msg.aps.cluster) {
        let seq = frame.header.sequence;
        let requested = pending.get(&seq).and_then(|p| match p.read {
            Some(Read::Standard(attribute)) if (p.peer,p.short,p.cluster)==(peer,msg.sender,msg.aps.cluster) => Some(attribute),
            _=>None,
        });
        let (complete, values) = super::standard::observe(msg.aps.cluster, frame, requested, generic_reports);
        if !values.is_empty() { *outcome = Outcome::Success; }
        super::standard::publish(values, peer.1, name, out)?;
        if complete {pending.remove(&seq);}
    }
    let Some(b) = binding else {return Ok(())};
    if !matches!(
        frame.header.general_command(),
        Some(
            GeneralCommand::ReportAttributes
                | GeneralCommand::ReadAttributesResponse
                | GeneralCommand::WriteAttributesResponse
                | GeneralCommand::DefaultResponse
        )
    ) && frame.header.frame_type != zigbee_zcl::FrameType::ClusterSpecific && msg.aps.cluster != zigbee_devices::tuya::household::CLUSTER
    {
        return Ok(());
    }
    if accepts_report && let Some(channel) = b.io.iter().find(|c|c.layout.cluster()==msg.aps.cluster)
        && super::basic_io::receive(channel,&b.name,out,pending,peer,msg.sender,frame)? {
        *outcome=Outcome::Success;
        return Ok(());
    }
    if unicast && b.zone.is_some() && msg.aps.cluster == zigbee_devices::ias::CLUSTER && frame.header.general_command() != Some(GeneralCommand::DefaultResponse) {
        if frame.header.frame_type == zigbee_zcl::FrameType::ClusterSpecific && frame.header.command_id == 0
            && frame.header.direction == Direction::ServerToClient && frame.header.manufacturer.is_none() {
            *outcome = if zigbee_zcl::cluster::Command::decode(msg.aps.cluster, frame).is_ok() {
                Outcome::Success
            } else { Outcome::Malformed };
        }
        return super::ias::receive(devices, out, pending, peer, msg.sender, frame);
    }
    if accepts_report && b.controls.level_config && msg.aps.cluster == zigbee_devices::standard_controls::level_config::CLUSTER {
        super::level_config::receive(link, devices, out, pending, peer, msg.sender, frame).await?;
    }
    let b = &devices.devices[&peer];
    if frame.header.general_command() == Some(GeneralCommand::DefaultResponse) {
        if unicast && b.equipment_commands.output.is_some() && matches!(msg.aps.cluster,6|8)
            && super::equipment::receive(link,devices,out,pending,peer,(msg.sender,msg.aps.cluster),frame).await? {
            return Ok(());
        }
        let seq = frame.header.sequence;
        // These vendor commands expose their accepted setting only after a correlated ZCL response.
        if let Some(p) = pending.get(&seq)
            && let Some(Read::TuyaLamp(setting)) = p.read
            && (p.peer, p.short, p.cluster) == (peer, msg.sender, msg.aps.cluster)
            && unicast && frame.header.direction == Direction::ServerToClient && frame.header.manufacturer.is_none()
            && let Ok(Payload::DefaultResponse(reply)) = Payload::decode(frame)
            && reply.command_id == setting.id()
        {
            if reply.status == 0 {
                let (key, value) = setting.state();
                submitted(out.state(&b.name, key, value))?;
            } else {
                eprintln!("Tuya lamp declined setting with ZCL status {:#04x}", reply.status);
            }
            pending.remove(&seq);
            return Ok(());
        }
        if let Some(p) = pending.get(&seq)
            && p.read.is_some()
            && (p.peer, p.short, p.cluster) == (peer, msg.sender, msg.aps.cluster)
            && frame.header.direction == Direction::ServerToClient
            && unicast
            && frame.header.manufacturer == p.read.and_then(|read| match read {
                Read::Household(query) => b.household.request_manufacturer(query),
                _ => None,
            })
            && let Ok(Payload::DefaultResponse(reply)) = Payload::decode(frame)
            && reply.command_id == match p.read {
                Some(Read::BasicIo{write:true,..} | Read::Environment(_,true) | Read::Equipment(super::equipment::Operation::Write(_)) | Read::Hvac(super::hvac::Operation {write:true,..}) | Read::LevelConfig(super::level_config::Operation::Write) | Read::Startup(zigbee_devices::standard_controls::StartupRequest::Write(_)) | Read::Household(super::household::Query::SettingWrite(..))) => GeneralCommand::WriteAttributes as u8,
                Some(Read::Setpoints(super::hvac::SetpointOperation::Write(proposed))) => proposed.write_command() as u8,
                Some(Read::Equipment(super::equipment::Operation::OutputCommand(_,id))) => id,
                Some(Read::Equipment(super::equipment::Operation::Panel)) => 7,
                Some(Read::Equipment(super::equipment::Operation::Schedule(_))) => 2,
                Some(Read::TuyaLamp(setting)) => setting.id(),
                Some(Read::ConfirmOnOff(on)) => u8::from(on),
                Some(Read::Refresh("state")) => return Ok(()),
                _ => GeneralCommand::ReadAttributes as u8,
            }
            && reply.status != 0
        {
            eprintln!(
                "MQTT request declined: transaction {seq}, ZCL status {:#04x}",
                reply.status
            );
            pending.remove(&seq);
        }
        return Ok(());
    }
    if accepts_report && (b.equipment_commands.handles(msg.aps.cluster) || b.equipment.iter().any(|f| f.cluster == msg.aps.cluster))
        && super::equipment::receive(link, devices, out, pending, peer, (msg.sender,msg.aps.cluster), frame).await? {
        *outcome = Outcome::Success;
        return Ok(());
    }
    if accepts_report && b.hvac.has_cluster(msg.aps.cluster) {
        return super::hvac::receive(link, devices, out, pending, peer, (msg.sender,msg.aps.cluster), frame).await;
    }
    if accepts_report && b.controls.hue_effects && msg.aps.cluster == zigbee_devices::standard_controls::hue::CLUSTER {
        if let Some(state) = zigbee_devices::standard_controls::hue::report(frame) {
            if let Some(id) = state.scene_id {
                devices.devices.get_mut(&peer).unwrap().confirmed_hue_scene = Some(id);
            }
            let b = &devices.devices[&peer];
            if let Some([x,y]) = state.coordinates {
                let xy = serde_json::json!({"x": f64::from(x)/65535., "y": f64::from(y)/65535.});
                submitted(out.state(&b.name, "color", xy))?;
                submitted(out.state(&b.name, "color_mode", "xy".into()))?;
            }
            if let Some(name) = state.effect {
                submitted(out.state(&b.name, "effect", name.into()))?;
            }
            if let Some(speed) = state.speed {
                submitted(out.state(&b.name, "effect_speed", speed.into()))?;
            }
        }
        return Ok(());
    }
    if msg.aps.cluster == zigbee_devices::tuya::household::CLUSTER && let Some(dp) = b.datapoints {
        if !dp.accepts(frame) { *outcome = Outcome::UnsupportedCommand; return Ok(()); }
        match dp.observe(frame) {
            Ok(values) => { *outcome = Outcome::Success; for (property,value) in values { submitted(out.state(&b.name,&property,value))?; } },
            Err(_) => { *outcome = Outcome::Malformed; eprintln!("Rejected malformed Tuya datapoint report"); },
        }
        return Ok(());
    }
    let ep = endpoint(peer.1, &b.descriptor);
    if !accepts_report && (b.household.input.is_none() || frame.header.frame_type!=zigbee_zcl::FrameType::ClusterSpecific) {return Ok(())}
    let seq = frame.header.sequence;
    if msg.aps.cluster==zigbee_devices::standard_controls::COLOR_CONTROL && b.controls.startup_mireds.is_some() {
        let correlated=pending.get(&seq).and_then(|p|match p.read {
            Some(Read::Startup(op)) if (p.peer,p.short,p.cluster)==(peer,msg.sender,msg.aps.cluster)=>Some(op),
            _=>None,
        });
        match b.controls.observe(frame,correlated) {
            Ok(result)=>{
                if result.complete {pending.remove(&seq);}
                if let Some(value)=result.value {submitted(out.state(&b.name,zigbee_devices::standard_controls::STARTUP,value.into()))?;}
            }
            Err(error)=>eprintln!("Invalid startup temperature frame: {error}"),
        }
        // Reports can contain both startup and current color attributes.
    }
    if msg.aps.cluster == zigbee_devices::cover::CLUSTER && let Some(cover) = b.cover {
        let requested = pending.get(&seq).and_then(|p| match p.read {
            Some(Read::Cover(axis)) if (p.peer, p.short, p.cluster) == (peer, msg.sender, msg.aps.cluster) => Some(axis),
            _ => None,
        });
        match cover.observe(frame, requested) {
            Ok(feedback) => {
                if feedback.complete { pending.remove(&seq); }
                for (axis, open) in feedback.positions {
                    submitted(out.state(&b.name, axis.property(), open.into()))?;
                    if axis == cover.primary() {
                        submitted(out.state(&b.name, "state", if open == 0 {"CLOSE"} else {"OPEN"}.into()))?;
                    }
                }
            }
            Err(error) => eprintln!("Invalid window covering feedback: {error}"),
        }
        return Ok(());
    }
    let requested = pending.get(&seq).and_then(|p| {
        if (p.peer,p.short,p.cluster) != (peer,msg.sender,msg.aps.cluster) {return None}
        match p.read {Some(Read::Household(query))=>Some(query),_=>None}
    });
    match b.household.frame_at(msg.aps.cluster, frame, requested, received.into_std()) {
        Ok(Some(values))=>{
            if !values.is_empty() || (frame.header.general_command() == Some(GeneralCommand::ReportAttributes) && Payload::decode(frame).is_ok()) { *outcome = Outcome::Success; }
            devices.update_deadline(peer,&b.household);
            if let Some(query) = requested && b.household.completes(query, msg.aps.cluster, frame)
                && let Some(entry) = pending.get_mut(&seq) && !entry.schedule_setting_readback(query, frame, Instant::now()) {
                pending.remove(&seq);
            }
            for (property,value) in values {
                let name = if b.household.input_property(property) { &b.device_name } else { &b.name };
                if property=="action" || property=="action_duration" { submitted(out.event(name,property,value))?; }
                else {submitted(out.state(name,property,value))?;}
            }
            // A bulb can report OnOff and StartUpOnOff together. The
            // lighting handler still owns the current OnOff attribute.
            if !b.is_light {
                return Ok(());
            }
        }
        Ok(None)=>{}
        Err(error)=>{*outcome = Outcome::Malformed; eprintln!("Invalid relay or meter frame: {error}"); return Ok(())}
    }
    if accepts_report && (0x0400..=0x042b).contains(&msg.aps.cluster)
        && b.environment.iter().any(|r| r.cluster == msg.aps.cluster)
        && super::environment::receive(b, out, pending, peer, msg.sender, msg.aps.cluster, frame)? {
        *outcome = Outcome::Success;
        return Ok(());
    }
    if matches!(msg.aps.cluster, 1 | 0x402 | 0x403 | 0x405)
        && frame.header.direction == Direction::ServerToClient
        && frame.header.manufacturer.is_none()
    {
        let mut publish = |id, value, kind| -> Result<()> {
            if let Some(v) = measurement::observe(&ep, msg.aps.cluster, id, None, value) {
                use crate::mqtt::discovery::Sensor;
                let (sensor, v) = match v {
                    Measurement::BatteryHalfPercent(v) => (Sensor::Battery, (f64::from(v) / 2.).into()),
                    Measurement::BatteryMillivolts(v) => (Sensor::Voltage, serde_json::Value::from(v)),
                    Measurement::TemperatureCentidegreesCelsius(v) => {
                        (Sensor::Temperature, (f64::from(v) / 100.).into())
                    }
                    Measurement::HumidityHundredthsPercent(v) => {
                        (Sensor::Humidity, (f64::from(v) / 100.).into())
                    }
                    Measurement::PressureHectopascals(v) => (Sensor::Pressure, v.into()),
                };
                let sensor = b.configured_sensor(sensor);
                let key = (peer, msg.aps.cluster, id);
                if !discovered.contains(&key) {
                    match out.discover(out.discovery_prefix(), &b.discovery(peer), Feature::Sensor(sensor)) {
                        Ok(()) => {
                            discovered.insert(key);
                        }
                        Err(SubmitError::Full) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                if matches!(sensor, Sensor::Temperature | Sensor::TemperatureWithForceUpdate(_)) {
                    submitted(sensor.report_temperature(out, &b.name, peer, v, observed_at_ms, kind))?;
                } else {
                    submitted(sensor.report(out, &b.name, v))?;
                }
            }
            Ok(())
        };
        match Payload::decode(frame) {
            Ok(Payload::ReportAttributes(values)) => {
                for attr in values {
                    publish(attr.id, attr.value, "attributeReport")?;
                }
            }
            Ok(Payload::ReadAttributesResponse(records)) => {
                let seq = frame.header.sequence;
                if let Some(p) = pending.get(&seq)
                    && (p.peer, p.short, p.cluster) == (peer, msg.sender, msg.aps.cluster)
                    && let Some(Read::Sensor(attr)) = p.read
                {
                    let mut matches = records.into_iter().filter(|r| r.id == attr.address().1);
                    if let Some(r) = matches.next()
                        && matches.next().is_none()
                    {
                        match r.result {
                            Status::Success(v) => publish(r.id, v, "readResponse")?,
                            Status::Error(status) => eprintln!(
                                "MQTT read declined: transaction {seq}, attribute {:#06x}, ZCL status {:#04x}",
                                r.id,
                                status.get()
                            ),
                        }
                        pending.remove(&seq);
                    }
                }
            }
            _ => {}
        }
        return Ok(());
    }
    if let Some(button) = &b.button {
        if let Some(action) = button.action(
            msg.aps.profile,
            msg.aps.cluster,
            msg.aps.src,
            msg.aps.dst,
            frame,
        ) {
            *outcome = Outcome::Success;
            submitted(out.event(&b.name, "action", action.as_str().into()))?;
        }
        return Ok(());
    }
    if !b.is_light {
        return Ok(());
    }
    let lamp = b.light(&ep)?;
    let values = if frame.header.general_command() == Some(GeneralCommand::ReadAttributesResponse) {
        let seq = frame.header.sequence;
        let Some(p) = pending.get(&seq) else {
            return Ok(());
        };
        if (p.peer, p.cluster, p.short) != (peer, msg.aps.cluster, msg.sender) {
            return Ok(());
        }
        let prop = match p.read {
            Some(Read::Light(prop)) => prop,
            Some(Read::PowerReadback) => zigbee_devices::lighting::Property::State,
            _ => return Ok(()),
        };
        if prop == zigbee_devices::lighting::Property::State && msg.aps.cluster == 6
            && let Some(status) = declined_power_read(frame) {
            eprintln!("MQTT on/off read declined: transaction {seq}, peer {:#06x}, endpoint {}, ZCL status {status:#04x}", msg.sender, peer.1);
            pending.remove(&seq);
            return Ok(());
        }
        let value = lamp.read_response(prop, seq, msg.aps.cluster, frame);
        if matches!(value, Ok(Some(_))) {
            pending.remove(&seq);
        }
        value.map(|v| v.into_iter().collect())
    } else {
        lamp.observations(msg.aps.cluster, frame)
    };
    match values {
        Ok(values) => {
            let mut needs_power = None;
            for v in values {
                if frame.header.general_command() == Some(GeneralCommand::ReadAttributesResponse)
                    || b.permits(v.property(), |a| a.report)
                {
                    use zigbee_devices::lighting::Setting;
                    let mode=match v {Setting::Xy(..)=>Some("xy"),Setting::Hs(..)=>Some("hs"),Setting::ColorTemperature(_)=>Some("color_temp"),_=>None};
                    let prop=v.property().name();
                    needs_power = Some(prop != "state" && !out.has_confirmed_light_power(&b.name));
                    if let Some(mode)=mode {
                        let mut fields=serde_json::Map::from_iter([(prop.into(),v.value()),("color_mode".into(),mode.into())]);
                        if let Some(temperature) = lamp.temperature_from_color(v) {
                            fields.insert("color_temp".into(), temperature.value());
                        }
                        submitted(out.state_fields(&b.name,prop,fields))?;
                    }
                    else {submitted(out.state(&b.name,prop,v.value()))?;}
                }
            }
            if let Some(needed) = needs_power {
                devices.devices.get_mut(&peer).expect("interviewed light").needs_light_power = needed;
            }
        }
        Err(e) => eprintln!("Invalid lighting report: {e}"),
    }
    Ok(())
}

fn declined_power_read(frame: Frame<'_>) -> Option<u8> {
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() { return None; }
    let Payload::ReadAttributesResponse(records) = Payload::decode(frame).ok()? else { return None };
    let mut power = records.into_iter().filter(|row| row.id == 0);
    let Status::Error(status) = power.next()?.result else { return None };
    power.next().is_none().then_some(status.get())
}

// The publisher accounts for overload. A full broker queue must not retire the radio.
pub(super) fn submitted(value: Result<(), SubmitError>) -> Result<()> {
    match value {
        Ok(()) | Err(SubmitError::Full) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod effect_tests {
    #[test]
    fn refresh_rotates_through_every_application_tag_at_capacity_without_reusing_its_write() {
        let now=Instant::now();
        let mut pending=BTreeMap::new();
        let entry=|| Pending{route_epoch:None,schedule:None,peer:(1,1),short:3,destination:Destination::fixture(1,3),cluster:8,read:Some(Read::Refresh("level_config")),until:now};
        for tag in 0..APPLICATION_SEQUENCES-1 {pending.insert(tag,entry());}
        assert!(!application_room(&pending,1));
        let mut used=std::collections::BTreeSet::new();
        while let Some((previous,next,_,_))=ready_refresh(&pending,now,0) {
            assert_ne!(previous,next);
            assert!(!pending.contains_key(&next) && next<APPLICATION_SEQUENCES);
            used.insert(next);
            let mut slot=pending.remove(&previous).unwrap();
            slot.read=Some(Read::LevelConfig(super::super::level_config::Operation::Readback));
            slot.until=now+Duration::from_secs(8);
            pending.insert(next,slot);
        }
        assert_eq!(used.len(),MAX_APPLICATION_IN_FLIGHT);
        assert!(!application_room(&pending,1));
        assert!(used.contains(&(APPLICATION_SEQUENCES-1)));
        assert!(pending.values().all(|p|p.until>now));
        pending.pop_first();
        assert!(application_room(&pending,1));
        assert_eq!(batch_start(APPLICATION_SEQUENCES-1,2),0);
    }

    #[test]
    fn verified_protection_write_defers_publication_to_actual_readback() {
        use super::super::household::Query;
        use zigbee_devices::settings::Written;
        let now=Instant::now();
        let query=Query::SettingWrite("overload_protection",Written::VerifyProtection);
        let mut p=Pending{route_epoch:None,schedule:None,peer:(5,1),short:9,destination:Destination::fixture(5,9),cluster:0xfc11,read:Some(Read::Household(query)),until:now+Duration::from_secs(8)};
        let failed=Frame::decode(&[0x18,2,4,0x86,3,0x70]).unwrap();
        assert!(!p.schedule_setting_readback(query,failed,now));
        assert_eq!(p.refresh_deadline(),None);
        let ok=Frame::decode(&[0x18,2,4,0]).unwrap();
        assert!(p.schedule_setting_readback(query,ok,now));
        assert_eq!(p.refresh_deadline(),Some(now));
        assert!(matches!(p.read,Some(Read::Refresh("overload_protection"))));
    }
    #[test]
    fn aps_tag_is_the_sequence_in_both_zcl_header_formats() {
        assert_eq!(transaction_tag(&[0x11, 212, 0]).unwrap(),212);
        assert_eq!(transaction_tag(&[0x15, 0x0b, 0x10, 213, 0]).unwrap(),213);
        assert!(transaction_tag(&[0x15, 0x0b]).is_err());
    }
    use super::*;
    #[test]
    fn only_an_unambiguous_on_off_error_retires_the_read() {
        for (wire, expected) in [
            (&[0x18, 7, 1, 0, 0, 0x86][..], Some(0x86)),
            (&[0x18, 7, 1, 1, 0, 0x86], None),
            (&[0x18, 7, 1, 0, 0, 0x86, 0, 0, 0x86], None),
            (&[0x18, 7, 1, 0, 0, 0, 0x10, 0], None),
            (&[0x10, 7, 1, 0, 0, 0x86], None),
        ] {
            assert_eq!(declined_power_read(Frame::decode(wire).unwrap()), expected);
        }
    }

    #[test]
    fn ordinary_power_read_has_no_recovery_even_after_deadline() {
        let now = Instant::now();
        let ordinary = Pending {
            route_epoch: None, schedule: None, peer: (13, 11), short: 7,
            destination: Destination::fixture(13, 7), cluster: 6,
            read: Some(Read::Light(zigbee_devices::lighting::Property::State)), until: now,
        };
        assert!(!ordinary.recovery_read());
        assert_eq!(ordinary.refresh_deadline(), None);
    }

    #[test]
    fn delivery_makes_the_on_off_readback_due_immediately() {
        let now = Instant::now();
        let mut write = Pending { route_epoch: None, schedule: None, peer: (13, 11), short: 7,
            destination: Destination::fixture(13, 7), cluster: 6, read: Some(Read::ConfirmOnOff(false)),
            until: now + Duration::from_secs(8) };
        assert!(!write.delivered(1, now));
        assert_eq!(write.refresh_deadline(), Some(now + Duration::from_secs(8)));
        assert!(write.delivered(0, now));
        assert_eq!(write.refresh_deadline(), Some(now));
        assert!(matches!(write.read, Some(Read::Refresh("state"))));
    }
    #[test]
    fn colour_readback_needs_successful_delivery_and_one_settle_interval() {
        let now=Instant::now();
        let make=||Pending{route_epoch:None,schedule:None,peer:(13,11),short:7,destination:Destination::fixture(13,7),cluster:768,read:Some(Read::LoopStop),until:now+Duration::from_secs(8)};
        let mut rejected=make();
        assert!(!rejected.delivered(1,now));
        assert!(rejected.refresh_deadline().is_none());
        let mut accepted=make();
        assert!(accepted.refresh_deadline().is_none());
        assert!(accepted.delivered(0,now));
        assert_eq!(accepted.refresh_deadline(),Some(now+Duration::from_millis(100)));
        assert!(accepted.delivered(0,now+Duration::from_millis(40)));
        assert_eq!(accepted.refresh_deadline(),Some(now+Duration::from_millis(100)));
    }
}
