//! Management requests run on the serial owner. MQTT only acknowledges a bind
//! once the source device has returned its ZDO status for each cluster.
use super::{
    Bindings,
    joining::{self, Interview},
    zdo::{self, BindingPlan},
};
use crate::{
    ash::{Callback, Destination, Ezsp},
    coordinator::request,
    interview::{self, commissioning::Commissioning},
    interview_log::{InterviewLog, InterviewReport},
    mqtt::{
        Publisher,
        admin::{Action, Reply, Request},
    },
    ota::Server,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

#[path = "admin_sequences.rs"]
pub(super) mod sequences;

pub(super) fn submission_uncertain(error: &anyhow::Error) -> bool {
    error.is::<zdo::UncertainSubmission>()
}

pub(super) fn finish_send_error(reply: Reply, error: &anyhow::Error) -> Result<()> {
    if submission_uncertain(error) {
        reply.indeterminate(&format!("Operation outcome unknown: {error:#}"))?;
    } else {
        reply.error(&format!("{error:#}"))?;
    }
    Ok(())
}

fn announced_target(report: &InterviewReport) -> Result<(u64, u16)> {
    use crate::interview_log::{IdentitySource, Observation};
    let (ieee, short) = report.entries.iter().find_map(|entry| match &entry.observation {
        Observation::Target { ieee, short, identity_source: IdentitySource::Announcement } => Some((ieee, *short)),
        _ => None,
    }).context("fresh interview requires an admitted device announcement")?;
    Ok((u64::from_str_radix(ieee, 16)?, short))
}

struct BindingRequest {
    plan: BindingPlan,
    reply: Reply,
    result: Value,
    index: usize,
    destination: Destination,
    transaction: sequences::Reservation,
    until: Instant,
}

// The directory belongs to the running coordinator's exclusive store lock.
pub struct Management {
    dir: PathBuf,
    names: BTreeMap<u64, String>,
    pub(super) ota: Server,
    coordinator: u64,
    startup_interview: Option<(InterviewReport, interview::InterviewReceipt)>,
    discovery: super::discovery_ownership::Ownership,
    pub(super) initial_discovery: Option<tokio::sync::oneshot::Receiver<()>>,
    pub(super) zones: super::enrollment::Enrollment,
    pub(super) ace_readers: super::ace::Readers,
    transactions: crate::mqtt::admin::journal::Journal,
    expires: Option<Instant>,
    broadcast: bool,
    keys: Commissioning,
    announcements: BTreeMap<u64, (Destination, Instant)>,
    advertised_updates: std::collections::BTreeSet<u64>,
    binding: Option<BindingRequest>,
    binding_ids: sequences::Sequences,
    pub(super) endpoint_ops: super::endpoint_admin::Operations,
    sequence: u8,
    waiting: VecDeque<(Destination, u8)>,
    setup_waiting: VecDeque<Destination>,
    interview: Option<Interview>,
    updates: super::updates::Updates,
    ota_routes: BTreeMap<u64, Destination>,
}

impl Management {
    /// Opt in named lock endpoints by supplying a private credential directory.
    pub fn set_lock_credentials(&mut self, path: &Path) -> Result<()> {
        self.endpoint_ops.lock_credentials = Some(super::door_lock::Credentials::open(path, &self.dir)?);
        Ok(())
    }

    pub fn needs_cleanup(&self) -> bool {
        self.expires.is_some() || self.broadcast || self.keys.deadline().is_some()
    }
    pub async fn recover(&mut self, link: &mut Ezsp<'_>) -> Result<()> {
        interview::close(link).await?;
        if self.broadcast {
            let seq = self.next_seq();
            zdo::broadcast_permit(link, 0, seq).await?;
        }
        interview::clear_transient_keys(link).await
    }

    pub fn new(
        state: &Path,
        names: BTreeMap<u64, String>,
        manifest: Option<&Path>,
    ) -> Result<Self> {
        let mut ota = match manifest {
            Some(path) => Server::load(path)?,
            None => Server::default(),
        };
        ota.persist_in(state)?;
        let dir = state.join("members");
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            names,
            ota,
            coordinator: 0,
            startup_interview: None,
            initial_discovery: None,
            discovery: super::discovery_ownership::Ownership::open(state)?,
            zones: super::enrollment::Enrollment::open(state)?,
            ace_readers: super::ace::Readers::load(state)?,
            transactions:crate::mqtt::admin::journal::Journal::open(state)?,
            expires: None,
            broadcast: false,
            keys: Commissioning::default(),
            announcements: BTreeMap::new(),
            advertised_updates: Default::default(),
            binding: None,
            binding_ids: Default::default(),
            endpoint_ops: Default::default(),
            sequence: 0,
            waiting: VecDeque::new(),
            setup_waiting: VecDeque::new(),
            interview: None,
            updates: Default::default(),
            ota_routes: Default::default(),
        })
    }
    /// Hand the managed session an interview it just performed on this radio.
    /// Loading an archived device report only permits the read-only startup survey.
    pub fn accept_fresh_interview(&mut self, report: InterviewReport, receipt: interview::InterviewReceipt) -> Result<()> {
        ensure!(self.coordinator == 0 && self.startup_interview.is_none(), "unexpected startup interview");
        ensure!(announced_target(&report)? == (receipt.0.ieee(), receipt.0.short()), "interview report and radio receipt disagree");
        self.startup_interview=Some((report,receipt));
        Ok(())
    }

    fn commit_interview(&mut self, report:&InterviewReport, devices:&mut Bindings, out: &Publisher) -> Result<u64> {
        let (ieee, _) = announced_target(report)?;
        let mut candidate=devices.clone();
        candidate.add_fresh(report,&self.names)?;
        if let Some(creds)=&self.endpoint_ops.lock_credentials {creds.discover(&mut candidate,ieee);}
        let mut log=InterviewLog::create(&self.dir)?;
        for entry in &report.entries {log.record_entry(entry)?;}
        let active = candidate.member_topics(ieee, out)?;
        self.discovery.claim(ieee, &active, out)?;
        self.zones.interviewed(ieee,&candidate)?;
        fs::rename(log.path(),self.dir.join(format!("{ieee:016x}.member")))?;
        fs::File::open(&self.dir)?.sync_all()?;
        candidate.mqtt_routes(out)?;
        *devices=candidate;
        Ok(ieee)
    }
    async fn admit_interview(&mut self, link: &mut Ezsp<'_>, destination: &Destination, report: &InterviewReport, devices: &mut Bindings, out: &Publisher) -> Result<u64> {
        ensure!(link.current(destination) && announced_target(report)? == (destination.ieee(), destination.short()),
            "interview assignment changed before admission");
        let ieee = self.commit_interview(report, devices, out)?;
        // The member must be routable before its MCU can answer the identity query.
        if let Some((ep, profile)) = super::tuya::version_endpoint(devices, ieee, report)
            && zdo::still_assigned(link, destination).await?
        {
            let wire = [0x11, self.next_seq(), 0x10, 2, 0];
            let status = zdo::submit_unicast(link, destination, profile, zigbee_devices::tuya::household::CLUSTER,
                (1, ep), 0xff, &wire).await?;
            if status != 0 { eprintln!("Tuya MCU version query declined: Ember {status:#04x}"); }
        }
        Ok(ieee)
    }

    pub(super) async fn start(
        &mut self,
        link: &mut Ezsp<'_>,
        devices: &mut Bindings,
        out: &Publisher,
    ) -> Result<()> {
        devices.load_availability(self.dir.parent().context("missing state directory")?)?;
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|x| x.to_str()) == Some("member") {
                devices.add(&InterviewReport::open(&path)?, &self.names)?;
            }
        }
        self.coordinator =
            u64::from_le_bytes(request(link, 0x26, &[]).await?.as_slice().try_into()?);
        self.zones.start(self.coordinator,devices)?;
        if let Some((report,receipt)) = self.startup_interview.take() {
            let original = receipt.0;
            if zdo::lookup_node(link, original.ieee()).await? == Some(original.short())
                && let Ok(current) = link.adopt(&original)
            {
                let ieee=self.admit_interview(link, &current, &report, devices, out).await?;
                if link.current(&current) && !super::reporting::plans(devices,ieee).is_empty() {
                    self.setup_waiting.push_back(current);
                    self.start_next_interview(devices);
                }
            } else {
                eprintln!("Startup interview discarded: address assignment changed or could not be verified");
            }
        }
        if let Some(creds)=&self.endpoint_ops.lock_credentials {
            for ieee in devices.peers.keys().copied().collect::<Vec<_>>() {creds.discover(devices,ieee);}
        } else {devices.lock_controls.clear();}
        devices.mqtt_routes(out)?;
        self.discovery.startup(devices, out).await?;
        out.permit_state(false, None)?;
        out.discover_admin(out.discovery_prefix(), self.coordinator)
            .await?;
        Ok(())
    }
    pub(super) fn start_next_interview(&mut self,devices:&Bindings) {
        if self.interview.is_some() {return;}
        if let Some((destination, capability)) = self.waiting.pop_front() {
            self.setup_waiting.retain(|target| target.ieee() != destination.ieee());
            self.interview = Some(Interview::new(destination, capability));
            return;
        }
        while let Some(destination) = self.setup_waiting.pop_front() {
            let plans = super::reporting::plans(devices, destination.ieee());
            if !plans.is_empty() {
                self.interview = Some(Interview::setup(destination, self.coordinator, plans));
                break;
            }
        }
    }
    fn next_seq(&mut self) -> u8 {
        self.sequence = (self.sequence+1) % interview::reporting::FIRST_SEQUENCE;
        self.sequence
    }
    pub(super) fn poll_work(&self, ieee: u64) -> bool {
        self.interview.as_ref().is_some_and(|job| job.destination.ieee() == ieee)
            || self.binding.as_ref().is_some_and(|job| job.plan.source.0 == ieee)
            || self.endpoint_ops.outstanding(ieee)
            || self.zones.has_outstanding(ieee)
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        [
            self.expires,
            self.announcements.values().map(|(_, until)| *until).min(),
            self.updates.deadline(),
            self.endpoint_ops.deadline(),
            self.zones.deadline(),
            self.keys.deadline(),
            self.binding.as_ref().map(|b| b.until),
        ]
        .into_iter()
        .flatten()
        .min()
    }
    async fn close(
        &mut self,
        link: &mut Ezsp<'_>,
        out: &Publisher,
        peer: Option<[u8; 8]>,
    ) -> Result<()> {
        interview::close(link).await?;
        if self.broadcast {
            let seq = self.next_seq();
            zdo::broadcast_permit(link, 0, seq).await?;
            self.broadcast = false;
        }
        if self.expires.take().is_some() {
            self.keys.admissions_closed(peer, Instant::now());
        }
        out.permit_state(false, None)?;
        self.keys.clean_if_due(link).await
    }
    pub(super) async fn tick(&mut self, link: &mut Ezsp<'_>, out: &Publisher) -> Result<()> {
        self.endpoint_ops.expire()?;
        self.zones.tick(link).await?;
        self.announcements
            .retain(|_, (_, until)| *until > Instant::now());
        self.updates.expire(&mut self.ota)?;
        if self.expires.is_some_and(|end| end <= Instant::now()) {
            self.close(link, out, None).await?;
        }
        self.keys.clean_if_due(link).await?;
        if self
            .binding
            .as_ref()
            .is_some_and(|b| b.until <= Instant::now())
        {
            let b = self.binding.take().unwrap();
            b.reply.indeterminate(&format!(
                "Binding timed out waiting for {}. Confirmed results so far: {}",
                zdo::cluster_name(b.plan.clusters[b.index]),
                b.result
            ))?;
        }
        Ok(())
    }
    async fn permit(
        &mut self,
        link: &mut Ezsp<'_>,
        out: &Publisher,
        time: u8,
        target: Option<&str>,
    ) -> Result<Value> {
        ensure!(
            target.is_none_or(|s| s.eq_ignore_ascii_case("coordinator")),
            "only coordinator or network-wide admission is supported"
        );
        if time == 0 {
            self.close(link, out, None).await?;
        } else {
            ensure!(
                self.keys.deadline().is_none(),
                "previous link-key exchange is still in progress"
            );
            if self.expires.is_none() {
                self.keys = Commissioning::opened();
            }
            let all = target.is_none();
            // Reserve cleanup before any change, including a partly failed open.
            self.expires = Some(Instant::now() + Duration::from_secs(time.into()));
            let previous = self.broadcast;
            self.broadcast = previous || all;
            let opened = async {
                if previous && !all {
                    let seq = self.next_seq();
                    zdo::broadcast_permit(link, 0, seq).await?;
                }
                interview::open(link, time).await?;
                if all {
                    let seq = self.next_seq();
                    zdo::broadcast_permit(link, time, seq).await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = opened {
                self.close(link, out, None).await?;
                return Err(error);
            }
            self.broadcast = all;
            let end = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64
                + u64::from(time) * 1000;
            out.permit_state(true, Some(end))?;
        }
        let mut data = json!({"time":time});
        if let Some(device) = target {
            data["device"] = json!(device);
        }
        Ok(data)
    }
    fn update_state(&mut self, out: &Publisher, devices: &Bindings, ieee: u64) -> Result<()> {
        use crate::mqtt::{
            SubmitError,
            admin::UpdatePhase,
            discovery::{Endpoint, Feature},
        };
        let declared = devices.devices.range((ieee, 0)..=(ieee, u8::MAX))
            .next().is_some_and(|(_, binding)| binding.declares_ota);
        if !declared && !self.ota.known_product(ieee) {
            return Ok(());
        }
        let Some((name, _)) = devices.peers.get(&ieee) else {
            return Ok(());
        };
        let Some(endpoint) = self.ota.observed_endpoint(ieee) else { return Ok(()) };
        let check = match self.ota.check(ieee) {
            Ok(check) => check,
            Err(error) => {
                eprintln!("OTA catalogue selection: {error}");
                return Ok(());
            }
        };
        let current = check.current.context("unknown running image")?;
        let schedule = self.ota.authorization(ieee)?;
        let phase = match schedule {
            Some((expected, true)) if expected == current => UpdatePhase::Idle,
            Some((_, true)) => UpdatePhase::Updating,
            Some(_) if self.ota.is_transferring(ieee) => UpdatePhase::Updating,
            Some(_) => UpdatePhase::Scheduled,
            _ if check.available == Some(true) => UpdatePhase::Available,
            _ => UpdatePhase::Idle,
        };
        let desc = Endpoint {
            device_availability: devices.availability.enabled(ieee),
            ieee,
            endpoint,
            device_name: name,
            topic_name: name,
        };
        if !declared && !self.advertised_updates.contains(&ieee) {
            match out.discover(out.discovery_prefix(), &desc, Feature::FirmwareUpdate) {
                Ok(()) => {
                    self.advertised_updates.insert(ieee);
                }
                Err(SubmitError::Full) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        match out.update_state(name, current, check.latest.unwrap_or(current), phase, None) {
            Ok(()) | Err(SubmitError::Full) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub(super) async fn handle(
        &mut self,
        link: &mut Ezsp<'_>,
        devices: &Bindings,
        out: &Publisher,
        req: Request,
    ) -> Result<()> {
        let Request { action, reply } = req;
        let Some(reply)=reply.reserve(&self.transactions,&action)? else {return Ok(())};
        if let Action::DoorLock(req) = action {
            if devices.peer(&req.id).is_ok_and(|id|self.zones.departed(id)) {
                reply.error("lock has left the network")?;
                return Ok(());
            }
            return self.endpoint_ops.start_lock(devices, link, req, reply).await;
        }
        if let Action::Endpoint(req) = action {
            return self.endpoint_ops.start(devices, link, req, reply).await;
        }
        if let Action::OtaUpdate(args) = action {
            let start = async {
                ensure!(
                    args.url.is_none(),
                    "select images in the local publisher manifest"
                );
                let ieee = devices.peer(&args.id)?;
                let node = zdo::node(link, ieee).await?;
                let destination = link.destination(ieee, node)?;
                let seq = self.next_seq();
                self.updates
                    .begin(&mut self.ota, link, &destination, seq)
                    .await
                    .map(|begin| (destination, begin))
            }
            .await;
            match start {
                Ok((destination, begin)) => {
                    let ieee = destination.ieee();
                    self.ota_routes.insert(ieee, destination);
                    self.updates.track(ieee, args.id, begin, reply);
                    self.update_state(out, devices, ieee)?;
                }
                Err(e) => finish_send_error(reply, &e)?,
            }
            return Ok(());
        }
        if matches!(action, Action::Bind(_) | Action::Unbind(_)) {
            let remove = matches!(action, Action::Unbind(_));
            let (Action::Bind(args) | Action::Unbind(args)) = action else {
                unreachable!()
            };
            let prepared=async{
                ensure!(self.binding.is_none(),"another binding request is in progress");
                let plan=BindingPlan::new(devices,&args,remove,self.coordinator)?;
                let short=zdo::node(link,plan.source.0).await?;
                let destination=link.destination(plan.source.0,short)?;
                let transaction=self.binding_ids.reserve((short,0,plan.request),0..interview::reporting::FIRST_SEQUENCE,1)?;
                let send=zdo::unicast(link,&destination,0,plan.request,(0,0),0xff,&plan.payload(transaction.first,plan.clusters[0])).await;
                if let Err(e)=send {
                    if !submission_uncertain(&e) {self.binding_ids.unsent(transaction);}
                    return Err(e);
                }
                let data=json!({"from":args.from,"to":args.to,"from_endpoint":plan.source.1,"clusters":[],"failed":[]});
                Ok::<_,anyhow::Error>((plan,destination,transaction,data))
            }.await;
            match prepared {
                Ok((plan, destination, transaction, result)) => {
                    self.binding = Some(BindingRequest {
                        plan,
                        reply,
                        result,
                        index: 0,
                        destination,
                        transaction,
                        until: Instant::now() + Duration::from_secs(12),
                    })
                }
                Err(e) => finish_send_error(reply, &e)?,
            }
            return Ok(());
        }
        let result=async{
            Ok(match action {
                Action::Describe(query) => {
                    let mut description=devices.describe_standard(&query)?;
                    if let Some(ep)=query.endpoint && query.cluster.is_none_or(|c|c==zigbee_devices::ias::CLUSTER) {
                        let key=(devices.peer(&query.id)?,ep);
                        if super::enrollment::server(devices,key).is_some() {
                            description["ias_enrollment"]=self.zones.status(key);
                        }
                    }
                    description
                }
                Action::HealthCheck=>{ensure!(request(link,0x18,&[]).await?==[2],"coordinator is not joined");json!({"healthy":true})}
                Action::PermitJoin{time,device}=>self.permit(link,out,time,device.as_deref()).await?,
                Action::OtaCheck(v)=>{ensure!(v.url.is_none(),"select images in the local publisher manifest");let id=devices.peer(&v.id)?;let check=self.ota.check(id)?;
                    json!({"id":v.id,"update_available":check.available,"installed_version":check.current,"latest_version":check.latest,"image":check.image})}
                Action::OtaSchedule(v)=>{ensure!(v.url.is_none(),"select images in the local publisher manifest");let ieee=devices.peer(&v.id)?;ensure!(!self.updates.active(ieee),"an update is already running");self.ota.schedule(ieee,None,Duration::from_secs(86400))?;self.update_state(out,devices,ieee)?;json!({"id":v.id})}
                Action::OtaUnschedule{id}=>{let ieee=devices.peer(&id)?;self.ota.cancel(ieee)?;self.updates.stop(ieee,"Update cancelled")?;self.update_state(out,devices,ieee)?;json!({"id":id})}
                _=>unreachable!(),
            })
        }.await;
        match result {
            Ok(data) => reply.ok(data)?,
            Err(e) => reply.error(&format!("{e:#}"))?,
        }
        Ok(())
    }
    /// Retire volatile work for the old assignment. Membership and durable
    /// OTA authorizations have their own lifetimes.
    pub(super) fn invalidate_address(&mut self, link: &Ezsp<'_>, ieee: Option<u64>, short: u16) -> Result<()> {
        self.zones.invalidate_address(ieee, short, |d|link.current(d));
        self.retire_work(|d| (Some(d.ieee()) == ieee || d.short() == short) && !link.current(d))
    }

    fn retire_work(&mut self, affected: impl Fn(&Destination) -> bool) -> Result<()> {
        self.endpoint_ops.retire_if(&affected)?;
        self.announcements.retain(|_, (d, _)| !affected(d));
        self.waiting.retain(|(d, _)| !affected(d));
        self.setup_waiting.retain(|d| !affected(d));
        if self.interview.as_ref().is_some_and(|i| affected(&i.destination)) {
            self.interview = None;
        }
        if self.binding.as_ref().is_some_and(|b| affected(&b.destination)) {
            let b = self.binding.take().unwrap();
            b.reply.indeterminate(&format!("Address changed during binding; current cluster outcome unknown. Confirmed results so far: {}", b.result))?;
        }
        let mut retired = Vec::new();
        self.ota_routes.retain(|id, d| {
            if affected(d) {retired.push(*id); false} else {true}
        });
        for id in retired {
            self.ota.retire_transfer(id);
            self.updates.stop(id, "Address changed before update confirmation; outcome unknown")?;
        }
        Ok(())
    }
    pub(super) async fn observe(
        &mut self,
        link: &mut Ezsp<'_>,
        out: &Publisher,
        devices: &Bindings,
        cb: &Callback,
    ) -> Result<()> {
        self.keys.observe(cb);
        if cb.id == 0x24
            && let [lo, hi, eui @ .., update, decision, _, _] = cb.payload.as_slice()
            && let Ok(eui) = <[u8; 8]>::try_from(eui)
        {
            let ieee = u64::from_le_bytes(eui);
            let short = u16::from_le_bytes([*lo, *hi]);
            self.invalidate_address(link,Some(ieee),short)?;
            self.zones.membership(ieee,short,*update,*decision,devices)?;
            if (*update==2 || *decision==2) && self.ace_readers.has_member(ieee) && !self.zones.departed(ieee) {
                self.zones.revoke_reader(ieee)?;
            }
            if *update == 2 {
                self.ota.forget(ieee)?;
            } else if matches!(update, 0|1|3) && *decision != 2
                && let Ok(destination) = link.destination(ieee, short)
            {
                if matches!(update, 1|3) && self.keys.admitted(&eui) {
                    self.announcements.insert(ieee, (destination, Instant::now() + Duration::from_secs(180)));
                } else if devices.peers.contains_key(&ieee) {
                    self.setup_waiting.push_back(destination);
                }
            }
        }
        if cb.id != 0x45 {
            self.start_next_interview(devices);
            return Ok(());
        }
        let m = interview::incoming(&cb.payload)?;
        self.zones.incoming(link,devices,&m).await?;
        if m.aps.profile == 0
            && m.aps.cluster == 0x13
            && m.aps.src == 0
            && m.aps.dst == 0
            && let [_, lo, hi, eui @ .., capability] = m.body
            && let Ok(eui) = <[u8; 8]>::try_from(eui)
            && m.sender == u16::from_le_bytes([*lo, *hi])
            && self
                .announcements
                .get(&u64::from_le_bytes(eui))
                .is_some_and(|(d, _)| d.short() == m.sender && link.current(d))
        {
            let ieee = u64::from_le_bytes(eui);
            let (destination, _) = self.announcements.remove(&ieee).unwrap();
            if self.expires.is_some() {
                self.close(link, out, Some(eui)).await?;
            }
            if link.current(&destination)
                && !self.waiting.iter().any(|(d,_)| d.ieee() == ieee)
                && self.interview.as_ref().is_none_or(|i| i.destination.ieee() != ieee)
            {
                self.waiting.push_back((destination, *capability));
            }
        }
        if let Some(i) = &self.interview
            && let Err(e) = i.observe(link, cb)
        {
            eprintln!("Interview aborted: {e}");
            self.interview = None;
        }
        self.start_next_interview(devices);
        if let Some(b) = self.binding.as_ref()
            && let Some(status) = b.plan.confirmed(&m, b.destination.short(), b.transaction.first)
        {
            let mut b = self.binding.take().unwrap();
            let sender = zdo::lookup_ieee(link, m.sender).await?;
            if !link.current(&b.destination) || sender != Some(b.destination.ieee()) {
                let reason = if !link.current(&b.destination) || sender.is_some() {
                    "Original binding assignment is no longer current; operation outcome unknown"
                } else {
                    "Adapter could not confirm the binding reply's sender; operation outcome unknown"
                };
                b.reply.indeterminate(reason)?;
                return Ok(());
            }
            self.binding_ids.confirmed(b.transaction);
            let bucket = if status == 0 { "clusters" } else { "failed" };
            b.result[bucket]
                .as_array_mut()
                .unwrap()
                .push(json!(zdo::cluster_name(b.plan.clusters[b.index])));
            b.index += 1;
            if b.index == b.plan.clusters.len() {
                if b.result["clusters"].as_array().unwrap().is_empty() {
                    b.reply.error(&format!(
                        "Source rejected all binding clusters (ZDO {status:#04x})"
                    ))?;
                } else {
                    b.reply.ok(b.result)?;
                }
            } else {
                b.transaction = match self.binding_ids.reserve((b.destination.short(),0,b.plan.request),0..interview::reporting::FIRST_SEQUENCE,1) {
                    Ok(tx) => tx,
                    Err(e) => {
                        b.reply.indeterminate(&format!("Partial binding result {}: {e}", b.result))?;
                        return Ok(());
                    }
                };
                match zdo::unicast(
                    link,
                    &b.destination,
                    0,
                    b.plan.request,
                    (0, 0),
                    0xff,
                    &b.plan.payload(b.transaction.first, b.plan.clusters[b.index]),
                )
                .await
                {
                    Ok(()) => {
                        b.until = Instant::now() + Duration::from_secs(12);
                        self.binding = Some(b);
                    }
                    Err(e) => {
                        if !submission_uncertain(&e) {self.binding_ids.unsent(b.transaction);}
                        b.reply.indeterminate(&format!("Partial binding result {}: {e}", b.result))?;
                    }
                }
            }
        }
        if matches!(m.kind, 0 | 1) && m.aps.profile == 0x104 && m.aps.cluster == 0x19 && m.aps.dst == 1 {
            if let Some(ieee) = zdo::lookup_ieee(link, m.sender).await?
                && let Ok(destination) = link.destination(ieee, m.sender)
                && link.current(&destination)
            {
                let mut empty = Server::default();
                let server = if devices
                    .peers
                    .get(&ieee)
                    .and_then(|(_, d)| d.get(&m.aps.src))
                    .is_some_and(|d| d.clients.contains(&0x19))
                {
                    if self.ota_routes.insert(ieee, destination.clone()).is_some_and(|old| !link.current(&old)) {
                        self.ota.retire_transfer(ieee);
                        self.updates.stop(ieee, "Address changed before update confirmation; outcome unknown")?;
                    }
                    &mut self.ota
                } else {
                    &mut empty
                };
                if let Some(body) = super::ota::reply(server, ieee, m.aps.src, m.body)? {
                    self.updates.observe(&self.ota, ieee)?;
                    self.update_state(out, devices, ieee)?;
                    if let Err(error) = zdo::unicast(
                        link,
                        &destination,
                        m.aps.profile,
                        0x19,
                        (1, m.aps.src),
                        0xff,
                        &body,
                    )
                    .await
                    {
                        eprintln!("OTA response delivery to endpoint {}: {error:#}", m.aps.src);
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) async fn interview_event(&mut self) -> joining::Event {
        // The startup snapshot may still be publishing a member that just rejoined.
        // Let it finish before a new interview can replace that member's configs.
        if let Some(ready) = &mut self.initial_discovery {
            let _ = ready.await;
            self.initial_discovery = None;
        }
        match self.interview.as_mut() {
            Some(i) => i.event().await,
            None => std::future::pending().await,
        }
    }
    pub(super) async fn interview_work(
        &mut self,
        event: joining::Event,
        link: &mut Ezsp<'_>,
        devices: &mut Bindings,
        out: &Publisher,
    ) -> Result<Option<(u64,bool)>> {
        let mut joined=None;
        let Some(active) = self.interview.as_ref().filter(|job| event.belongs_to(job)) else {
            return Ok(None);
        };
        if !link.current(&active.destination) {
            self.interview.take();
            self.start_next_interview(devices);
            return Ok(None);
        }
        match event.step {
            joining::Step::Send(tx) => {
                let i = self.interview.as_ref().context("interview stopped")?;
                let result = async {
                    ensure!(
                        zdo::still_assigned(link, &i.destination).await?,
                        "interview address changed"
                    );
                    link.submit_to(&i.destination, &tx.args).await
                }
                .await;
                let _ = tx.result.send(result);
            }
            joining::Step::Finished(done) => {
                let i = self.interview.take().unwrap();
                let ieee = i.destination.ieee();
                if !link.current(&i.destination) {
                    self.start_next_interview(devices);
                    return Ok(None);
                }
                match done {
                    Ok(report) if i.maintenance => {
                        if super::reporting::persist(&self.dir,ieee,report,devices,&self.names)? {
                            devices.advertise_member(out,ieee).await?;
                        }
                    }
                    Ok(report) => {
                        self.admit_interview(link, &i.destination, &report, devices, out).await?;
                        if !link.current(&i.destination) {
                            self.start_next_interview(devices);
                            return Ok(None);
                        }
                        self.setup_waiting.push_back(i.destination);
                        joined=Some((ieee,i.mains_powered));
                        self.discovery.after_interview(ieee, devices, out).await?;
                        devices.advertise_member(out, ieee).await?;
                    }
                    Err(e) => eprintln!("Device interview failed: {e:#}"),
                }
                self.start_next_interview(devices);
            }
        }
        Ok(joined)
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::interview_log::{Descriptor,Entry,IdentitySource,Observation};

    #[test]
    fn conflict_retires_interview_and_setup_only_for_the_affected_assignment() -> Result<()> {
        let dir=tempfile::tempdir()?;
        let mut manager=Management::new(dir.path(),Default::default(),None)?;
        let affected=Destination::fixture(1,0x1111);
        let other=Destination::fixture(2,0x2222);
        manager.interview=Some(Interview::new(affected.clone(),0x0c));
        manager.announcements.insert(affected.ieee(),(affected.clone(),Instant::now()));
        manager.waiting.push_back((affected.clone(),0x0c));
        manager.setup_waiting.extend([affected.clone(),other.clone()]);
        manager.ota_routes.insert(affected.ieee(),affected.clone());
        manager.ota_routes.insert(other.ieee(),other.clone());

        manager.retire_work(|d| d.short() == affected.short() && d.same_assignment(&affected))?;
        assert!(manager.interview.is_none() && manager.waiting.is_empty() && manager.announcements.is_empty());
        assert_eq!(manager.setup_waiting.len(),1);
        assert!(manager.setup_waiting[0].same_assignment(&other));
        assert_eq!(manager.ota_routes.len(),1);
        assert!(manager.ota_routes[&other.ieee()].same_assignment(&other));

        manager.retire_work(|d| d.same_assignment(&other))?;
        assert!(manager.setup_waiting.is_empty() && manager.ota_routes.is_empty());
        Ok(())
    }

    #[test]
    fn fresh_startup_interview_persists_member_and_pending_zone_together() -> Result<()> {
        let dir=tempfile::tempdir()?;
        let mut manager=Management::new(dir.path(),Default::default(),None)?;
        let mut devices=Bindings::default();
        let cie=0x0200_0000_0000_0001;
        let ieee=cie+1;
        let report=|identity_source|InterviewReport {
            finished:true,
            entries:[
                Observation::Target{ieee:format!("{ieee:016x}"),short:1,identity_source},
                Observation::ActiveEndpoints{sequence:1,endpoints:vec![1]},
                Observation::SimpleDescriptor{sequence:2,endpoint:1,descriptor:Descriptor{
                    profile:0x104,device:0x402,version:1,servers:vec![0x500],clients:vec![]}},
                Observation::Finished,
            ].into_iter().map(|observation|Entry{observation,unix_ms:0}).collect(),
        };
        manager.zones.start(cie,&devices)?;
        let (out, _worker, _) = crate::mqtt::channel(crate::mqtt::MqttOptions::new("test", "localhost", 1883), "test", 8)?;
        assert!(manager.commit_interview(&report(IdentitySource::Z2mDatabase),&mut devices,&out).is_err());
        for (wrong_ieee, wrong_short) in [(ieee + 1, 1), (ieee, 2)] {
            let receipt = interview::InterviewReceipt(Destination::fixture(wrong_ieee, wrong_short));
            assert!(manager.accept_fresh_interview(report(IdentitySource::Announcement), receipt).is_err());
            assert!(manager.startup_interview.is_none());
        }
        let receipt=interview::InterviewReceipt(Destination::fixture(ieee,1));
        manager.accept_fresh_interview(report(IdentitySource::Announcement),receipt)?;
        let (admitted,_)=manager.startup_interview.take().unwrap();
        manager.commit_interview(&admitted,&mut devices,&out)?;
        assert_eq!(manager.zones.status((ieee,1))["state"],"reading_zone");
        drop(manager);
        let mut manager=Management::new(dir.path(),Default::default(),None)?;
        let mut restored=Bindings::default();
        restored.add(&InterviewReport::open(&dir.path().join(format!("members/{ieee:016x}.member")))?,&Default::default())?;
        manager.zones.start(cie,&restored)?;
        assert_eq!(manager.zones.status((ieee,1))["state"],"reading_zone");
        Ok(())
    }
}

#[cfg(test)]
#[path = "admin_discovery_test.rs"]
mod discovery_test;
