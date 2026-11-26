//! MQTT 5 transport. Device descriptions decide whether a property is
//! durable state or a transient event before submitting it here.
pub mod admin;
pub mod config;
pub mod discovery;
mod pending;
mod feedback;
mod shadow;
mod subscriptions;
mod retained;

use anyhow::{Context, Result, bail, ensure};
use rumqttc::Outgoing;
pub use rumqttc::v5::MqttOptions;
use rumqttc::v5::{AsyncClient, Event, Incoming};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{LastWill, PubAckReason, Publish, SubscribeReasonCode};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}},
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};

const HA_STATUS_TOPIC: &str = "homeassistant/status";
const ONLINE: &[u8] = br#"{"state":"online"}"#;
const OFFLINE: &[u8] = br#"{"state":"offline"}"#;
const MAX_COMMAND_BODY: usize = 16 * 1024; // decoded command or administration body
const MAX_INBOUND_PACKET: usize = 2 * 1024 * 1024;
const MAX_OUTBOUND_PACKET: usize = 2 * 1024 * 1024;
const SNAPSHOT_LIMIT: usize = 1024 * 1024;
const QUEUED_RETAINED_BYTES: usize = 4 * 1024 * 1024;
const QUEUED_STATE_BYTES: usize = 4 * 1024 * 1024;
const QUEUED_EVENT_BYTES: usize = 1024 * 1024;
const EVENT_TTL: Duration = Duration::from_secs(10);
const PIPELINE: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Get,
    Set,
}

#[derive(Debug)]
pub struct Command {
    pub device: String,
    pub operation: Operation,
    pub properties: Map<String, Value>,
}

/// A command admitted under one incarnation of a managed MQTT route.
pub struct ReceivedCommand {
    command: Command,
    route_epoch: Option<u64>,
}
impl ReceivedCommand {
    pub(crate) fn epoch(&self) -> Option<u64> { self.route_epoch }
    pub(crate) fn followup(command: Command, route_epoch: Option<u64>) -> Self {
        Self { command, route_epoch }
    }
}

impl std::ops::Deref for ReceivedCommand {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.command
    }
}
impl From<Command> for ReceivedCommand {
    fn from(command: Command) -> Self {
        Self {
            command,
            route_epoch: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    Full,
    Storage,
    Closed,
    Invalid,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MQTT publication {:?}", self)
    }
}
impl std::error::Error for SubmitError {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delivery {
    State,
    Observation,
    Event,
}

struct Item {
    device: String,
    delivery: Delivery,
    expires: Option<tokio::time::Instant>,
    payload: Vec<u8>,
    raw_action: Option<Vec<u8>>,
    feedback: Vec<(String, Vec<u8>)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct OverflowCounts {
    pub states: u64,
    pub events: u64,
    pub commands: u64,
}

struct Diagnostic {
    latest_device: String,
    latest_operation: &'static str,
    totals: OverflowCounts,
}

impl Diagnostic {
    fn payload(self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "level": "error",
            "message": "MQTT queue unavailable; updates or commands were rejected",
            "meta": {
                "totals": self.totals, "latest_device": self.latest_device,
                "latest_operation": self.latest_operation
            }
        }))
        .expect("serializable queue diagnostic")
    }
}

// Device names cannot begin with "bridge/". Diagnostics can therefore use the
// same bounded snapshot queue without colliding with any device or endpoint.
const RADIO_DIAGNOSTICS: &str = "bridge/diagnostics/";
fn diagnostic_route(device: &str) -> String {
    format!("{RADIO_DIAGNOSTICS}{device}")
}

const RADIO_FIELDS: [&str; 2] = ["last_seen", "linkquality"];

struct Dirty {
    slot: u64,
    followed_by_event: bool,
}

#[derive(Default)]
struct Pending {
    admin: admin::Queue,
    configs: BTreeMap<String, Retained>,
    initial_presence_queued: bool,
    store: Option<Arc<shadow::Shadow>>,
    identities: subscriptions::Routes,
    route_epochs: HashMap<String, u64>,
    inactive: std::collections::BTreeSet<u64>,
    desired_configs: BTreeMap<String, Vec<u8>>,
    waiting_configs: std::collections::BTreeSet<String>,
    config_bytes: usize,
    event_bytes: usize,
    state_bytes: usize,
    capacity: usize,
    waiting: std::collections::VecDeque<String>,
    waiting_names: std::collections::HashSet<String>,
    snapshots: HashMap<String, Map<String, Value>>,
    feedback: HashMap<String, std::collections::BTreeSet<String>>,
    live_light: HashMap<String, Map<String, Value>>,
    queued: BTreeMap<u64, Item>,
    dirty: HashMap<String, Dirty>,
    sequence: u64,
    latest_rejection: Option<(String, &'static str)>,
    overflows: OverflowCounts,
    closed: bool,
}
struct Retained {
    bytes: Vec<u8>,
    receipt: Option<oneshot::Sender<()>>,
}
struct Shared {
    display_names: Mutex<BTreeMap<u64, String>>,
    no_matching_subscribers: AtomicU64,
    discovery_prefix: String,
    base: String,
    ownership_scope: String,
    routes: watch::Sender<Option<Arc<subscriptions::Routes>>>,
    pending: Mutex<Pending>,
    wake: Notify,
    config_space: Notify,
    closed: watch::Sender<bool>,
    capacity: usize,
}

#[derive(Clone)]
pub struct Publisher(Arc<Shared>);

fn topic_part(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512 && !s.contains(['#', '+', '\0'])
}
pub(crate) fn device_name(s: &str) -> bool {
    topic_part(s)
        && s.split('/').all(|level| !level.is_empty())
        && s != "bridge"
        && !s.starts_with("bridge/")
        && !s.ends_with("/set")
        && !s.ends_with("/get")
}

/// MQTT 5 QoS 1 PUBLISH with a topic, packet identifier and empty property list.
pub(crate) fn publish_fits(topic: &str, body: usize) -> bool {
    let Some(remaining) = body.checked_add(topic.len() + 5) else {
        return false;
    };
    let length_bytes = match remaining {
        0..=127 => 1,
        128..=16_383 => 2,
        16_384..=2_097_151 => 3,
        _ => 4,
    };
    topic.len() <= usize::from(u16::MAX) && remaining <= MAX_OUTBOUND_PACKET - 1 - length_bytes
}

impl Publisher {
    pub(crate) fn set_routes(&self, mut routes: subscriptions::Routes) -> Result<()> {
        ensure!(
            routes.keys().all(|name| device_name(name)),
            "invalid expanded MQTT device name"
        );
        let mut pending = self.0.pending.lock().unwrap();
        routes.retain(|_, (ieee, _)| !pending.inactive.contains(ieee));
        for name in pending.identities.keys().cloned().collect::<Vec<_>>() {
            if pending.identities.get(&name) != routes.get(&name) {
                pending.clear_device(&name);
                pending.route_epochs.remove(&name);
            }
        }
        for (name, &id) in &routes {
            if pending.identities.get(name) != Some(&id)
                && let Some(store) = &pending.store
                && let Some(data) = store.get(id)?
            {
                pending.restore_snapshot(name, &data)?;
            }
        }
        let added: Vec<_> = routes
            .iter()
            .filter(|(name, id)| pending.identities.get(*name) != Some(*id))
            .map(|(name, _)| name.clone())
            .collect();
        pending.identities = routes.clone();
        for name in added {
            pending.sequence += 1;
            let epoch = pending.sequence;
            pending.route_epochs.insert(name.clone(), epoch);
            pending.replay_state(&name)?;
            pending.replay_state(&diagnostic_route(&name))?;
        }
        drop(pending);
        self.0.wake.notify_one();
        self.0.routes.send_if_modified(|current| {
            if current.as_deref() == Some(&routes) {
                false
            } else {
                *current = Some(Arc::new(routes));
                true
            }
        });
        Ok(())
    }

    pub(crate) fn manages_membership(&self) -> bool {
        self.0.pending.lock().unwrap().store.is_some()
    }

    pub(crate) fn departed_members(&self) -> std::collections::BTreeSet<u64> {
        self.0.pending.lock().unwrap().inactive.clone()
    }

    pub(crate) fn membership_changed(&self, ieee: u64, present: bool) -> Result<()> {
        let mut q = self.0.pending.lock().unwrap();
        if q.inactive.contains(&ieee) == present && let Some(store) = &q.store {
            store.membership(ieee, present)?;
        }
        if present {
            q.inactive.remove(&ieee);
        } else {
            q.inactive.insert(ieee);
        }
        // An announcement can reuse both addresses and every friendly name.
        q.sequence += 1;
        let epoch = q.sequence;
        let names: Vec<_> = q
            .identities
            .iter()
            .filter(|(_, id)| id.0 == ieee)
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            if present {
                // A rejoined light may have applied its power-on behaviour.
                q.invalidate_light_power(&name);
                q.route_epochs.insert(name, epoch);
            } else {
                q.clear_device(&name);
                q.route_epochs.remove(&name);
            }
        }
        Ok(())
    }

    pub(crate) fn is_current(&self, cmd: &ReceivedCommand) -> bool {
        self.route_is_current(&cmd.device, cmd.epoch())
    }

    pub(crate) fn route_is_current(&self, device: &str, epoch: Option<u64>) -> bool {
        epoch.is_none_or(|id| self.0.pending.lock().unwrap().route_epochs.get(device) == Some(&id))
    }

    /// Per-device retained presence shares the bounded, coalescing retained queue.
    pub fn device_availability(&self, device:&str, online:bool)->Result<(),SubmitError>{
        if !device_name(device){return Err(SubmitError::Invalid);}
        self.enqueue_configs(&mut vec![(format!("{}/{device}/availability",self.0.base),if online{ONLINE.to_vec()}else{OFFLINE.to_vec()})])
    }

    /// All initial member states have entered the bounded retained queue.
    pub(crate) fn device_snapshot_queued(&self) {
        self.0.pending.lock().unwrap().initial_presence_queued = true;
        self.0.wake.notify_one();
    }

    pub(crate) fn ownership_scope(&self) -> &str { &self.0.ownership_scope }

    fn retained_scope(&self) -> String {
        serde_json::to_string(&(self.ownership_scope(), self.discovery_prefix()))
            .expect("string tuple")
    }

    /// Retire an owned topic only after the broker acknowledges the tombstone.
    pub(crate) async fn retire_availability(&self, name: &str) -> Result<()> {
        ensure!(device_name(name), "invalid availability name");
        self.retire_topic(format!("{}/{name}/availability", self.0.base)).await
    }

    /// Hold ownership until a QoS 1 acknowledgement confirms the retained deletion.
    pub(crate) async fn retire_topic(&self, topic: String) -> Result<()> {
        if self.0.pending.lock().unwrap().store.is_some() {
            // The durable obligation survives broker failure and coordinator shutdown.
            return Ok(self.enqueue_configs(&mut vec![(topic, Vec::new())])?);
        }
        let (ack, receipt)=oneshot::channel();
        loop {
            let space=self.0.config_space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            {
                let mut q=self.0.pending.lock().unwrap();
                ensure!(!q.closed, "MQTT closed during retained topic reconciliation");
                if q.configs.len()<self.0.capacity || q.configs.contains_key(&topic) {
                    q.configs.insert(topic,Retained {bytes:Vec::new(),receipt:Some(ack)});
                    self.0.wake.notify_one();
                    break;
                }
            }
            space.await;
        }
        let mut closed=self.0.closed.subscribe();
        tokio::select! {
            confirmed=receipt=>confirmed.context("broker did not confirm retained topic deletion"),
            _=closed.wait_for(|v|*v)=>bail!("MQTT closed during retained topic reconciliation"),
        }
    }
    pub fn discovery_prefix(&self) -> &str {
        &self.0.discovery_prefix
    }
    pub fn state(&self, device: &str, property: &str, value: Value) -> Result<(), SubmitError> {
        let delivery = if matches!(property, "occupancy" | "contact" | "vibration") {
            Delivery::Observation
        } else {
            Delivery::State
        };
        self.submit_field(device, property, value, delivery)
    }
    pub fn event(&self, device: &str, property: &str, value: Value) -> Result<(), SubmitError> {
        self.submit_field(device, property, value, Delivery::Event)
    }
    fn submit_field(&self, device: &str, property: &str, value: Value, delivery: Delivery) -> Result<(), SubmitError> {
        self.submit(device, Map::from_iter([(property.to_owned(), value)]), delivery)
    }
    /// Several fields describing one observation must reach the subscriber together.
    pub fn state_fields(
        &self,
        device: &str,
        key: &str,
        fields: Map<String, Value>,
    ) -> Result<(), SubmitError> {
        if !fields.contains_key(key) {
            return Err(SubmitError::Invalid);
        }
        let delivery = if key == "zone_status" { Delivery::Observation } else { Delivery::State };
        self.submit(device, fields, delivery)
    }
    fn submit(&self, device: &str, fields: Map<String, Value>, delivery: Delivery) -> Result<(), SubmitError> {
        if !device_name(device) || fields.keys().any(|p| p.is_empty() || p.len() > 256) {
            return Err(SubmitError::Invalid);
        }
        let raw_action = match (delivery, fields.get("action")) {
            (Delivery::Event, Some(Value::String(s))) => Some(s.as_bytes().to_vec()),
            (Delivery::Event, Some(v @ (Value::Number(_) | Value::Bool(_)))) => Some(v.to_string().into_bytes()),
            (Delivery::Event, Some(_)) => return Err(SubmitError::Invalid),
            _ => None,
        };
        let diagnostic;
        let route = if delivery == Delivery::State && RADIO_FIELDS.iter().any(|key| fields.contains_key(*key)) {
            // The caller promises these fields describe one observation. Reject a
            // mixed batch rather than attributing radio activity to appliance state.
            if !fields.keys().all(|key| RADIO_FIELDS.contains(&key.as_str())) {
                return Err(SubmitError::Invalid);
            }
            diagnostic = diagnostic_route(device);
            &diagnostic
        } else {
            device
        };
        let result = self.0.pending.lock().unwrap().put_report(route, fields, delivery, raw_action, self.0.capacity);
        self.0.wake.notify_one();
        result
    }

    #[cfg(test)]
    pub(crate) fn forget_state(&self, device: &str) -> Result<()> {
        let mut q = self.0.pending.lock().unwrap();
        if let (Some(store), Some(id)) = (&q.store, q.identities.get(device)) {
            store.remove(*id)?;
        }
        q.clear_device(device);
        Ok(())
    }

    fn requeue(&self) -> Result<()> {
        let mut q = self.0.pending.lock().unwrap();
        for name in q.snapshots.keys().cloned().collect::<Vec<_>>() {
            q.replay_state(&name)?;
        }
        for topic in q.desired_configs.keys().cloned().collect::<Vec<_>>() {
            q.queue_config(&topic);
        }
        self.0.wake.notify_one();
        Ok(())
    }

    pub fn close(&self) {
        self.0.pending.lock().unwrap().closed = true;
        self.0.closed.send_replace(true);
        self.0.wake.notify_one();
        self.0.config_space.notify_waiters();
    }
    /// QoS 1 publications for which this broker explicitly reported no matching subscribers.
    pub fn no_matching_subscribers(&self) -> u64 {
        self.0.no_matching_subscribers.load(Ordering::Relaxed)
    }

    pub fn overflow_counts(&self) -> OverflowCounts {
        self.0.pending.lock().unwrap().overflows
    }
    /// Valid commands declined because the coordinator's ingress queue filled.
    pub fn rejected_commands(&self) -> u64 {
        self.overflow_counts().commands
    }
}

pub struct Worker {
    wait_for_presence: bool,
    routed: Option<mpsc::Sender<ReceivedCommand>>,
    publisher: Publisher,
    options: MqttOptions,
    base: String,
    commands: mpsc::Sender<Command>,
    admin: Option<mpsc::Sender<admin::Request>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.publisher.0.pending.lock().unwrap().admin.retired = true;
        self.publisher.close();
    }
}

/// Capacity is enforced independently for pending device topics, FIFO events and
/// inbound commands. The MQTT library also bounds its request and in-flight windows.
pub fn channel(
    options: MqttOptions,
    base: &str,
    capacity: usize,
) -> Result<(Publisher, Worker, mpsc::Receiver<Command>)> {
    channel_with_prefix(options, base, "homeassistant", capacity)
}

pub fn channel_with_prefix(
    mut options: MqttOptions,
    base: &str,
    prefix: &str,
    capacity: usize,
) -> Result<(Publisher, Worker, mpsc::Receiver<Command>)> {
    ensure!(
        topic_part(prefix) && prefix.split('/').all(|s| !s.is_empty()),
        "invalid discovery prefix"
    );
    ensure!(
        (1..=65535).contains(&capacity),
        "invalid MQTT queue capacity"
    );
    ensure!(
        topic_part(base) && base.split('/').all(|s| !s.is_empty()),
        "invalid base topic"
    );
    options.set_last_will(LastWill::new(
        format!("{base}/bridge/state"),
        OFFLINE,
        QoS::AtLeastOnce,
        true,
        None,
    ));
    options.set_outgoing_inflight_upper_limit(PIPELINE as u16);
    options.set_receive_maximum(Some(PIPELINE as u16));
    options.set_max_packet_size(Some(MAX_INBOUND_PACKET as u32));
    options.set_clean_start(true);
    options.set_session_expiry_interval(Some(0));
    let (commands, receiver) = mpsc::channel(capacity);
    let publisher = Publisher(Arc::new(Shared {
        display_names: Default::default(),
        no_matching_subscribers: Default::default(),
        routes: watch::channel(None).0,
        ownership_scope: serde_json::to_string(&(options.broker_address(),base))?,
        discovery_prefix: prefix.to_owned(),
        base: base.to_owned(),
        pending: Mutex::new(Pending {
            capacity,
            ..Default::default()
        }),
        wake: Notify::new(),
        config_space: Notify::new(),
        closed: watch::channel(false).0,
        capacity,
    }));
    Ok((
        publisher.clone(),
        Worker {
            wait_for_presence: false,
            routed: None,
            publisher,
            options,
            base: base.into(),
            commands,
            admin: None,
        },
        receiver,
    ))
}

// The MQTT client acknowledges QoS1 PUBLISH before dispatch. MQTT-4.3.2-2
// requires a reused packet ID after PUBACK to represent a new publication,
// even with DUP set. A non-idempotent exposed function needs its own request ID.
fn command(base: &str, publish: &Publish) -> Option<Command> {
    if publish.retain {
        return None;
    }
    let relative = std::str::from_utf8(&publish.topic).ok()?.strip_prefix(base)?.strip_prefix('/')?;
    let (parent, leaf) = relative.rsplit_once('/')?;
    let op = |s| match s {
        "set" => Some(Operation::Set),
        "get" => Some(Operation::Get),
        _ => None,
    };
    let (device, operation, property) = if let Some(operation) = op(leaf) {
        (parent, operation, None)
    } else {
        let (device, verb) = parent.rsplit_once('/')?;
        (device, op(verb)?, Some(leaf))
    };
    if !device_name(device) || publish.payload.len() > MAX_COMMAND_BODY {
        return None;
    }
    let properties: Map<String, Value> = if let Some(prop) = property {
        if prop.is_empty() || prop.len() > 256 {
            return None;
        }
        let value = match serde_json::from_slice(&publish.payload) {
            Ok(v) => v,
            Err(_) => {
                let text = std::str::from_utf8(&publish.payload).ok()?;
                if text.trim_start().starts_with(['{', '[', '"']) {
                    return None;
                }
                Value::String(text.into())
            }
        };
        Map::from_iter([(prop.to_owned(), value)])
    } else {
        serde_json::from_slice(&publish.payload).ok()?
    };
    if properties.is_empty() || properties.keys().any(|p| p.is_empty() || p.len() > 256) {
        return None;
    }
    Some(Command {
        device: device.into(),
        operation,
        properties,
    })
}

impl Worker {
    /// Managed coordinators seed retained device states under a bridge-offline
    /// marker. Standalone MQTT publishers have no radio inventory to await.
    pub fn wait_for_device_snapshot(&mut self) {
        self.wait_for_presence = true;
    }

    pub fn enable_admin(&mut self) -> Result<mpsc::Receiver<admin::Request>> {
        ensure!(self.admin.is_none(), "admin receiver already registered");
        let (sender, receiver) = mpsc::channel(self.publisher.0.capacity);
        self.admin = Some(sender);
        Ok(receiver)
    }

    pub fn enable_reconnect(
        &mut self,
        directory: &std::path::Path,
    ) -> Result<mpsc::Receiver<ReceivedCommand>> {
        ensure!(self.routed.is_none(), "managed MQTT is already configured");
        let store = shadow::Shadow::open(directory)?;
        let mut q = self.publisher.0.pending.lock().unwrap();
        q.inactive = store.departed()?;
        for topic in store.tombstones(&self.publisher.retained_scope())? {
            q.desired_configs.insert(topic.clone(), Vec::new());
            q.queue_config(&topic);
        }
        q.store = Some(Arc::new(store));
        // set_routes can be called before the managed store is attached.
        // Revoke any such route whose identity was removed in an earlier session.
        let previous_routes = q.identities.clone();
        drop(q);
        if !previous_routes.is_empty() {
            self.publisher.set_routes(previous_routes)?;
        }
        let (tx, rx) = mpsc::channel(self.publisher.0.capacity);
        self.routed = Some(tx);
        Ok(rx)
    }

    /// Returns success only after PUBACK for accepted publications and the
    /// retained offline marker, followed by a clean DISCONNECT. A broker/link
    /// error starts a fresh connection in managed mode. Standalone workers return it.
    pub async fn run(self) -> Result<()> {
        self.run_with_checkpoint_interval(Duration::from_secs(30))
            .await
    }

    async fn run_with_checkpoint_interval(self, interval: Duration) -> Result<()> {
        let store = self.publisher.0.pending.lock().unwrap().store.clone();
        let Some(store) = store else {
            return self.sessions().await;
        };
        let (stop, mut stopped) = oneshot::channel();
        let sessions = async {
            let result = self.sessions().await;
            self.publisher.close();
            let _ = stop.send(());
            result
        };
        let checkpoints = async {
            let mut ticks =
                tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let closing = tokio::select! {
                    biased;
                    _ = &mut stopped => true,
                    _ = ticks.tick() => false,
                };
                if store.has_changes() {
                    let writer = store.clone();
                    let result = tokio::task::spawn_blocking(move || writer.checkpoint())
                        .await
                        .context("joining MQTT checkpoint")
                        .and_then(|r| r);
                    if closing {
                        result.context("final MQTT state checkpoint")?;
                    } else if let Err(error) = result {
                        eprintln!(
                            "MQTT state checkpoint failed, retrying next interval: {error:#}"
                        );
                    }
                }
                if closing {
                    return Ok::<(), anyhow::Error>(());
                }
            }
        };
        let (transport, storage) = tokio::join!(sessions, checkpoints);
        storage?;
        transport
    }

    async fn sessions(&self) -> Result<()> {
        let mut closed = self.publisher.0.closed.subscribe();
        let mut delay = Duration::from_millis(250);
        loop {
            let connected = std::sync::atomic::AtomicBool::new(false);
            let started = tokio::time::Instant::now();
            match self.connection(&connected).await {
                Ok(()) => return Ok(()),
                Err(error) if self.routed.is_none() || *closed.borrow() => return Err(error),
                Err(error) => eprintln!("MQTT unavailable: {error:#}. Retrying in {delay:?}"),
            }
            if connected.load(Ordering::Relaxed) {
                self.publisher.requeue()?;
                if started.elapsed() >= Duration::from_secs(30) {
                    delay = Duration::from_millis(250);
                }
            }
            tokio::select! {
                _ = closed.wait_for(|v| *v) => return Ok(()),
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(Duration::from_secs(10));
        }
    }

    async fn connection(&self, connected: &std::sync::atomic::AtomicBool) -> Result<()> {
        let (client, mut network) = AsyncClient::new(self.options.clone(), PIPELINE);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (acked_tx, mut acked) = watch::channel(0_u64);
        let (sub_tx, sub_rx) = mpsc::channel(1);
        let subscriptions = subscriptions::Subscriptions::new(sub_rx);
        let subscribe = subscriptions::maintain(self, &client, &subscriptions, ready_tx);
        let witness = retained::DeletionWitness::default();
        let mut progress = acked.clone();
        let transport = async {
            let mut packet_ids = HashMap::new();
            let mut publish_sequence = 0_u64;
            loop {
                match network.poll().await.context("MQTT connection")? {
                    Event::Incoming(Incoming::SubAck(ack)) => {
                        sub_tx.try_send(ack.return_codes)?;
                    }
                    Event::Outgoing(Outgoing::Publish(id)) => {
                        publish_sequence += 1;
                        ensure!(packet_ids.insert(id, publish_sequence).is_none(), "MQTT packet identifier already in use");
                    }
                    Event::Incoming(Incoming::PubAck(ack)) => {
                        let sequence = packet_ids.remove(&ack.pkid).context("unmatched MQTT PUBACK")?;
                        match ack.reason {
                            PubAckReason::Success => {}
                            PubAckReason::NoMatchingSubscribers => {
                                let total = self.publisher.0.no_matching_subscribers
                                    .fetch_add(1, Ordering::Relaxed) + 1;
                                if total.is_power_of_two() {
                                    eprintln!("MQTT publications with no matching subscribers reported by broker: {total}");
                                }
                            }
                            reason if witness.rejected(sequence, reason) => {}
                            reason => bail!("broker rejected MQTT publication {}: {reason:?}", ack.pkid),
                        }
                        acked_tx.send_modify(|n| *n += 1);
                    }
                    Event::Incoming(Incoming::Publish(p)) => {
                        if p.topic == HA_STATUS_TOPIC && self.routed.is_some() {
                            if !p.retain && p.payload.as_ref() == b"online" {
                                let mut pending = self.publisher.0.pending.lock().unwrap();
                                if !pending.closed {
                                    pending.queue_birth_snapshot();
                                    self.publisher.0.wake.notify_one();
                                }
                            }
                            continue;
                        }
                        witness.received(&p);
                        if let Some(sender) = &self.admin {
                            admin::receive(&self.publisher, sender, &p);
                        }
                        if let Some(cmd) = command(&self.base, &p) {
                            let mut pending = self.publisher.0.pending.lock().unwrap();
                            if pending.closed {
                                continue;
                            }
                            let rejected = if let Some(routed) = &self.routed {
                                let Some(&epoch) = pending.route_epochs.get(&cmd.device) else {
                                    continue;
                                };
                                routed
                                    .try_send(ReceivedCommand {
                                        command: cmd,
                                        route_epoch: Some(epoch),
                                    })
                                    .err()
                                    .map(|err| err.into_inner().command)
                            } else {
                                self.commands
                                    .try_send(cmd)
                                    .err()
                                    .map(|err| err.into_inner())
                            };
                            if let Some(cmd) = rejected {
                                pending.overflows.commands += 1;
                                let op = match cmd.operation {
                                    Operation::Get => "get",
                                    Operation::Set => "set",
                                };
                                pending.latest_rejection = Some((cmd.device, op));
                                self.publisher.0.wake.notify_one();
                            }
                        }
                    }
                    Event::Outgoing(Outgoing::Disconnect) => return Ok::<(), anyhow::Error>(()),
                    _ => {}
                }
            }
        };
        let drain = async {
            ready_rx.await?;
            connected.store(true, Ordering::Relaxed);
            let availability = format!("{}/bridge/state", self.base);
            let mut waiting = self.wait_for_presence;
            let namespace = format!("{}/", self.base);
            client.publish(&availability, QoS::AtLeastOnce, true, if waiting { OFFLINE } else { ONLINE }).await?;
            let mut count = 1_u64;
            let mut prefer_config = true;
            let mut prefer_admin = true;
            let mut next_diagnostic = tokio::time::Instant::now();
            loop {
                if waiting {
                    let seeded = {
                        let pending = self.publisher.0.pending.lock().unwrap();
                        pending.initial_presence_queued
                            && !pending.closed
                            && !pending.waiting_configs.iter().any(|topic| {
                                topic.starts_with(&namespace) && topic.ends_with("/availability")
                            })
                            && !pending
                                .configs
                                .range(namespace.clone()..)
                                .take_while(|(topic, _)| topic.starts_with(&namespace))
                                .any(|(topic, _)| topic.ends_with("/availability"))
                    };
                    if seeded {
                        // The sole sender has submitted every preceding retained
                        // device update. PUBACK fences the snapshot at the broker.
                        acked.wait_for(|&n| n >= count).await?;
                        if !self.publisher.0.pending.lock().unwrap().closed {
                            client.publish(&availability, QoS::AtLeastOnce, true, ONLINE).await?;
                            count += 1;
                        }
                        waiting = false;
                    }
                }

                let (item, config, admin, diagnostic, has_diagnostic, closed) = {
                    let mut pending = self.publisher.0.pending.lock().unwrap();
                    let due = pending.closed || tokio::time::Instant::now() >= next_diagnostic;
                    let diagnostic = if due {
                        pending
                            .latest_rejection
                            .take()
                            .map(|(latest_device, latest_operation)| Diagnostic {
                                latest_device,
                                latest_operation,
                                totals: pending.overflows,
                            })
                    } else {
                        None
                    };
                    let admin = if diagnostic.is_none()
                        && (prefer_admin
                            || (pending.configs.is_empty()
                                && pending.queued.is_empty()))
                    {
                        pending.admin.take()
                    } else {
                        None
                    };
                    let config = if admin.is_none()
                        && diagnostic.is_none()
                        && (prefer_config
                            || pending.queued.is_empty())
                    {
                        pending.take_config()
                    } else {
                        None
                    };
                    let item = if admin.is_none() && config.is_none() && diagnostic.is_none() {
                        pending.take()
                    } else {
                        None
                    };
                    (
                        item,
                        config,
                        admin,
                        diagnostic,
                        pending.latest_rejection.is_some(),
                        pending.closed && pending.admin.reserved == 0,
                    )
                };
                if let Some(publication) = admin {
                    client
                        .publish(
                            format!("{}/{}", self.base, publication.topic),
                            QoS::AtLeastOnce,
                            publication.retained,
                            publication.bytes,
                        )
                        .await?;
                    count += 1;
                    prefer_admin = false;
                    continue;
                }
                prefer_admin = true;
                if let Some((topic, Retained{bytes:payload,receipt})) = config {
                    self.publisher.0.config_space.notify_waiters();
                    count += 1;
                    if payload.is_empty() && self.routed.is_some() {
                        witness.delete(&client, &subscriptions, &self.publisher, topic, &mut acked, count).await?;
                    } else {
                        client.publish(topic, QoS::AtLeastOnce, true, payload).await?;
                    }
                    if let Some(done)=receipt {
                        acked.wait_for(|&ack|ack>=count).await?;
                        let _=done.send(());
                    }
                    prefer_config = false;
                    continue;
                }
                if let Some(rejected) = diagnostic {
                    client
                        .publish(
                            format!("{}/bridge/logging", self.base),
                            QoS::AtLeastOnce,
                            false,
                            rejected.payload(),
                        )
                        .await?;
                    count += 1;
                    next_diagnostic = tokio::time::Instant::now() + Duration::from_millis(250);
                    continue;
                }
                match item {
                    Some(Item {
                        device,
                        delivery: _,
                        expires: _,
                        payload,
                        raw_action,
                        feedback,
                    }) => {
                        prefer_config = true;
                        let topic = format!("{}/{}", self.base, device);
                        client
                            .publish(&topic, QoS::AtLeastOnce, false, payload)
                            .await?;
                        count += 1;
                        for (route, bytes) in feedback {
                            client.publish(format!("{}/{}", self.base, route), QoS::AtLeastOnce, false, bytes).await?;
                            count += 1;
                        }
                        if let Some(raw) = raw_action {
                            client
                                .publish(format!("{topic}/action"), QoS::AtLeastOnce, false, raw)
                                .await?;
                            count += 1;
                        }
                    }
                    None if closed => break,
                    None if has_diagnostic => tokio::select! {
                        _ = self.publisher.0.wake.notified() => {},
                        _ = tokio::time::sleep_until(next_diagnostic) => {},
                    },
                    None => self.publisher.0.wake.notified().await,
                }
            }
            client
                .publish(availability, QoS::AtLeastOnce, true, OFFLINE)
                .await?;
            acked.wait_for(|&n| n == count + 1).await?;
            client.disconnect().await?;
            Ok::<(), anyhow::Error>(())
        };
        let shutdown_limit = async {
            let mut closed = self.publisher.0.closed.subscribe();
            let _ = closed.wait_for(|v| *v).await;
            let absolute = tokio::time::sleep(Duration::from_secs(30));
            tokio::pin!(absolute);
            loop {
                tokio::select! {
                    _ = &mut absolute => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => break,
                    result = progress.changed() => if result.is_err() {break;},
                }
            }
        };
        tokio::select! {
            r=subscribe => r,
            r=async {tokio::try_join!(transport,drain)} => { r?; Ok(()) },
            _=shutdown_limit => bail!("MQTT shutdown timed out before acknowledgement"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn snapshots_survive_drain_and_keep_events_out_of_later_state() {
        let (p, _worker, _) = channel(MqttOptions::new("test", "localhost", 1883), "zb", 4).unwrap();
        let next = || -> Value {
            let item = p.0.pending.lock().unwrap().take().unwrap();
            serde_json::from_slice(&item.payload).unwrap()
        };
        p.state("soil", "battery", json!(96)).unwrap();
        assert_eq!(next(), json!({"battery":96}));
        p.state_fields("soil", "temperature", Map::from_iter([
            ("temperature".into(), json!(21)), ("moisture".into(), json!(43))
        ])).unwrap();
        p.event("soil", "action", json!("single")).unwrap();
        p.submit_field("soil", "temperature", json!(22), Delivery::Observation).unwrap();
        p.state("soil", "battery", json!(95)).unwrap();
        assert_eq!(next(), json!({"action":"single"}));
        assert_eq!(next(), json!({"battery":96,"temperature":22,"moisture":43}));
        assert_eq!(next(), json!({"battery":95,"temperature":22,"moisture":43}));
        assert!(
            p.event("soil", "diagnostic", json!("x".repeat(SNAPSHOT_LIMIT)))
                .is_err()
        );
        p.state("soil", "battery", json!(94)).unwrap();
        assert_eq!(next(), json!({"battery":94,"temperature":22,"moisture":43}));
        p.state("soil", "battery", json!(93)).unwrap();
        p.forget_state("soil").unwrap();
        p.state("soil", "battery", json!(100)).unwrap();
        assert_eq!(next(), json!({"battery":100}));
    }

    #[test]
    fn burst_merges_devices_and_preserves_ordered_events_with_independent_limits() {
        let (p, _worker, _) =
            channel(MqttOptions::new("test", "localhost", 1883), "zb", 2).unwrap();
        for level in 0..1000 {
            p.state("light", "brightness", json!(level)).unwrap();
        }
        p.state("light", "state", json!("ON")).unwrap();
        p.state("second", "brightness", json!(12)).unwrap();
        assert_eq!(
            p.state("another", "state", json!("OFF")),
            Err(SubmitError::Full)
        );
        for action in ["single", "single"] {
            p.event("button", "action", json!(action)).unwrap();
        }
        assert_eq!(
            p.event("button", "action", json!("double")),
            Err(SubmitError::Full)
        );
        assert_eq!(
            p.overflow_counts(),
            OverflowCounts {
                states: 1,
                events: 1,
                commands: 0
            }
        );
        p.close();
        assert_eq!(
            p.state("light", "brightness", json!(10)),
            Err(SubmitError::Closed)
        );
        let mut queue = p.0.pending.lock().unwrap();
        let values: Vec<Value> = std::iter::from_fn(|| queue.take())
            .map(|item| serde_json::from_slice(&item.payload).unwrap())
            .collect();
        assert_eq!(
            values,
            [
                json!({"brightness":999,"state":"ON"}),
                json!({"brightness":12}),
                json!({"action":"single"}),
                json!({"action":"single"})
            ]
        );
    }
}

#[cfg(all(unix, test))]
mod recovery_tests;
