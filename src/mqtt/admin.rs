//! Zigbee2MQTT bridge request/response envelope. The control owner supplies the
//! result after it has checked the device, radio or OTA provider.
pub(crate) mod journal;
mod endpoint;
pub use endpoint::{EndpointCommand, EndpointRequest};
use super::{MAX_COMMAND_BODY, PIPELINE, Publisher, SubmitError};
use sha2::{Digest, Sha256};
use journal::{Claim, INDETERMINATE, Journal, Ticket};
use rumqttc::v5::mqttbytes::v5::Publish;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::VecDeque;
use tokio::sync::mpsc;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Selector {
    Name(String),
    Number(u16),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bind {
    pub from: String,
    pub from_endpoint: Option<Selector>,
    pub to: Selector,
    pub to_endpoint: Option<Selector>,
    pub clusters: Option<Vec<String>>,
    #[serde(default)]
    pub skip_disable_reporting: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ota {
    pub id: String,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Describe {
    pub id: String,
    pub endpoint: Option<u8>,
    pub cluster: Option<u16>,
    pub after: Option<u16>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockAction { Lock, Unlock }

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DoorLockRequest { pub id: String, pub endpoint: u8, pub action: LockAction }

#[derive(Debug, Serialize)]
pub enum Action {
    Describe(Describe),
    HealthCheck,
    Endpoint(EndpointRequest),
    DoorLock(DoorLockRequest),
    PermitJoin { time: u8, device: Option<String> },
    Bind(Bind),
    Unbind(Bind),
    OtaCheck(Ota),
    OtaUpdate(Ota),
    OtaSchedule(Ota),
    OtaUnschedule { id: String },
}

pub struct Request {
    pub action: Action,
    pub reply: Reply,
}

pub(crate) const API_VERSION: u8 = 1;
const MAX_RESPONSE_BODY: usize = 256 * 1024;

struct Envelope {
    namespace: &'static str,
    path: String,
    transaction: Option<Value>,
}
pub(super) struct Publication {
    pub topic: String,
    pub bytes: Vec<u8>,
    pub retained: bool,
}

impl Envelope {
    fn response_topic(&self) -> String {
        format!("bridge/{}response/{}", self.namespace, self.path)
    }

    fn response(&self, data: Value, error: Option<&str>) -> Publication {
        let response = if let Some(error) = error {
            json!({"status":"error","data":{},"error":error})
        } else {
            json!({"status":"ok","data":data})
        };
        self.publication(response)
    }

    fn delivery_refused(&self, reason: &str) -> Publication {
        if self.transaction.is_none() {
            return self.response(Value::Null, Some(reason));
        }
        // The original delivery may already have reached the coordinator.
        self.publication(json!({
            "status": "error", "data": {}, "retry": "same_transaction",
            "error": format!("{reason}. This delivery could not be accepted. Retry with the same transaction ID to obtain the operation result.")
        }))
    }

    fn publication(&self, mut response: Value) -> Publication {
        if let Some(tx) = &self.transaction {
            response["transaction"] = tx.clone();
        }
        let mut bytes = serde_json::to_vec(&response).expect("JSON value");
        if bytes.len() > MAX_RESPONSE_BODY {
            response["data"] = json!({});
            response["error"] = json!("Bridge response exceeds size limit");
            response["status"] = json!("error");
            bytes = serde_json::to_vec(&response).expect("bounded JSON response");
        }
        Publication {
            topic: self.response_topic(),
            bytes,
            retained: false,
        }
    }
}

// One accepted operation owns a response slot, even when other producers fill
// every queue. Dropping it produces an explicit interruption response.
pub struct Reply {
    publisher: Publisher,
    envelope: Option<Envelope>,
    ticket: Option<Ticket>,
}
#[derive(Clone, Copy)]
enum Outcome {
    Known,
    Indeterminate,
}

impl Reply {
    /// A duplicate already in progress shares the original response topic and ID.
    pub(crate) fn reserve(mut self, journal:&Journal, action:&Action)->anyhow::Result<Option<Self>> {
        if matches!(action,Action::HealthCheck|Action::Describe(_)|Action::OtaCheck(_))
            || matches!(action, Action::Endpoint(v) if v.command.read_only()) {return Ok(Some(self))}
        let e=self.envelope.as_ref().expect("accepted reply");
        let Some(tx)=&e.transaction else{return Ok(Some(self))};
        // Preserve deployed journal keys for the original, unversioned API.
        let operation = format!("{}{}", e.namespace, e.path);
        let key=serde_json::to_string(&(&self.publisher.0.base,operation,tx))?;
        let digest=Sha256::digest(serde_json::to_vec(action)?);
        match journal.claim(key,&digest) {
            Ok(Claim::New(ticket))=>{self.ticket=Some(ticket);return Ok(Some(self))}
            Ok(Claim::Active)=>self.dispatch(None)?,
            Ok(Claim::Completed(bytes))=>{
                let publication=Publication{topic:e.response_topic(),bytes,retained:false};
                self.dispatch(Some(publication))?;
            }
            Ok(Claim::Indeterminate)=>self.error(INDETERMINATE)?,
            Ok(Claim::Conflict)=>self.error("Transaction already identifies a different request")?,
            Ok(Claim::Full)=>self.error("Administration transaction journal is full")?,
            Err(err)=>{
                eprintln!("Recording administration transaction: {err:#}");
                self.error("Transaction storage unavailable; operation was not started")?;
            }
        }
        Ok(None)
    }
    pub fn ok(mut self, data: Value) -> Result<(), SubmitError> {
        self.finish(data, None, Outcome::Known)
    }
    pub fn error(mut self, reason: &str) -> Result<(), SubmitError> {
        self.finish(Value::Null, Some(reason), Outcome::Known)
    }
    pub(crate) fn indeterminate(mut self, reason: &str) -> Result<(), SubmitError> {
        self.finish(Value::Null, Some(reason), Outcome::Indeterminate)
    }
    fn finish(&mut self, data: Value, error: Option<&str>, outcome: Outcome) -> Result<(), SubmitError> {
        let Some(e) = self.envelope.as_ref() else {
            return Ok(());
        };
        let mut response = e.response(data,error);
        if let Some(ticket)=self.ticket.take() && let Err(err) = match outcome {
            Outcome::Known => ticket.finish(&response.bytes),
            Outcome::Indeterminate => ticket.indeterminate(&response.bytes),
        } {
            eprintln!("Recording administration outcome: {err:#}");
            response=e.response(Value::Null,Some(INDETERMINATE));
        }
        self.dispatch(Some(response))
    }
    fn dispatch(&mut self,response:Option<Publication>)->Result<(),SubmitError>{
        self.envelope.take();

        let mut pending = self.publisher.0.pending.lock().unwrap();
        pending.admin.reserved -= 1;
        if pending.admin.retired {
            return Err(SubmitError::Closed);
        }
        if let Some(response)=response {pending.admin.replies.push_back(response);}
        drop(pending);
        self.publisher.0.wake.notify_one();
        Ok(())
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        if self.envelope.is_some() {
            let reason=if self.ticket.take().is_some() {INDETERMINATE} else {"Bridge operation interrupted"};
            let _ = self.finish(Value::Null,Some(reason),Outcome::Indeterminate);
        }
    }
}

#[derive(Default)]
pub(super) struct Queue {
    pub reserved: usize,
    pub retired: bool,
    replies: VecDeque<Publication>,
    refused: VecDeque<Publication>,
    info: Option<Publication>,
    prefer_refusal: bool,
}
impl Queue {
    pub fn take(&mut self) -> Option<Publication> {
        if let Some(info) = self.info.take() {
            return Some(info);
        }
        if self.prefer_refusal
            && let Some(response) = self.refused.pop_front()
        {
            self.prefer_refusal = false;
            return Some(response);
        }
        if let Some(response) = self.replies.pop_front() {
            self.prefer_refusal = true;
            return Some(response);
        }
        self.refused.pop_front()
    }
}

fn read<T: DeserializeOwned>(value: Value) -> Result<T, &'static str> {
    serde_json::from_value(value).map_err(|_| "Invalid request payload")
}
fn name(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 512 && !s.contains('\0')
}
fn parse(path: &str, val: Value) -> Result<Action, &'static str> {
    match path {
        "health_check" if val.is_null() || val == "" || val == json!({}) => Ok(Action::HealthCheck),
        "device/command" => {
            let request: EndpointRequest = read(val)?;
            if !name(&request.id) || !(1..=240).contains(&request.endpoint) {
                return Err("Expected a device and application endpoint");
            }
            Ok(Action::Endpoint(request))
        }
        "device/describe" => {
            let query: Describe = read(val)?;
            if !name(&query.id) || (query.cluster.is_some() && query.endpoint.is_none())
                || (query.after.is_some() && query.cluster.is_none()) {
                return Err("Supply a device, then an endpoint for cluster and attribute lookup");
            }
            Ok(Action::Describe(query))
        }
        "permit_join" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Permit {
                time: Value,
                device: Option<String>,
            }
            let Permit { time, device } = if val.is_object() {
                read(val)?
            } else {
                Permit {
                    time: val,
                    device: None,
                }
            };
            let time = time
                .as_u64()
                .or_else(|| time.as_str()?.parse().ok())
                .filter(|n| *n <= 254)
                .ok_or("Permit time must be 0..254 seconds")? as u8;
            if device.as_deref().is_some_and(|x| !name(x)) {
                return Err("Invalid permit device");
            }
            Ok(Action::PermitJoin { time, device })
        }
        "device/bind" | "device/unbind" => {
            let b: Bind = read(val)?;
            let valid = |s: &Selector| match s {
                Selector::Name(s) => name(s),
                Selector::Number(_) => true,
            };
            if !name(&b.from)
                || !valid(&b.to)
                || b.from_endpoint.as_ref().is_some_and(|v| !valid(v))
                || b.to_endpoint.as_ref().is_some_and(|v| !valid(v))
                || b.clusters
                    .as_ref()
                    .is_some_and(|v| v.is_empty() || v.len() > 256 || v.iter().any(|k| !name(k)))
            {
                return Err("Invalid binding selectors");
            }
            Ok(if path.ends_with("/bind") {
                Action::Bind(b)
            } else {
                Action::Unbind(b)
            })
        }
        "device/ota_update/update"
        | "device/ota_update/check"
        | "device/ota_update/schedule"
        | "device/ota_update/unschedule" => {
            if val.get("hex").is_some_and(|v| !v.is_null()) {
                return Err("Inline OTA images are unsupported");
            }
            let ota: Ota = if let Value::String(id) = val {
                Ota { id, url: None }
            } else {
                read(val)?
            };
            if !name(&ota.id)
                || ota
                    .url
                    .as_deref()
                    .is_some_and(|u| u.is_empty() || u.len() > 2048)
            {
                return Err("Invalid OTA request");
            }
            match path.rsplit('/').next() {
                Some("update") => Ok(Action::OtaUpdate(ota)),
                Some("check") => Ok(Action::OtaCheck(ota)),
                Some("schedule") => Ok(Action::OtaSchedule(ota)),
                _ if ota.url.is_none() => Ok(Action::OtaUnschedule { id: ota.id }),
                _ => Err("Unschedule takes only a device ID"),
            }
        }
        "health_check" => Err("Invalid health check payload"),
        _ => Err("Unsupported bridge request"),
    }
}

type Parsed = (Envelope, Result<Action, &'static str>);
fn decode(base: &str, p: &Publish) -> Option<Parsed> {
    let topic = std::str::from_utf8(&p.topic).ok()?.strip_prefix(base)?.strip_prefix("/bridge/")?;
    let (namespace, path) = if let Some(path) = topic.strip_prefix("v1/request/") {
        ("v1/", path)
    } else {
        ("", topic.strip_prefix("request/")?)
    };
    if p.retain
        || path.is_empty()
        || path.len() > 128
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/'))
    {
        return None;
    }
    let mut envelope = Envelope {
        namespace,
        path: path.to_owned(),
        transaction: None,
    };
    if p.payload.len() > MAX_COMMAND_BODY {
        return Some((envelope, Err("Request too large")));
    }
    let mut value = match serde_json::from_slice(&p.payload) {
        Ok(v) => v,
        Err(_) => match std::str::from_utf8(&p.payload) {
            Ok(text) if !text.trim_start().starts_with(['{', '[', '"']) => {
                Value::String(text.trim().to_owned())
            }
            _ => return Some((envelope, Err("Invalid JSON"))),
        },
    };
    if let Some(tx) = value.as_object_mut().and_then(|o| o.remove("transaction")) {
        if !tx.is_number() && tx.as_str().is_none_or(|s| s.len() > 512) {
            return Some((envelope, Err("Invalid transaction identifier")));
        }
        envelope.transaction = Some(tx);
    }
    if path == "device/lock" {
        let request = if namespace != "v1/" || envelope.transaction.is_none() {
            Err("Door lock operations require API v1 and a transaction ID")
        } else {
            read::<DoorLockRequest>(value).and_then(|req| {
                if name(&req.id) && (1..=240).contains(&req.endpoint) { Ok(Action::DoorLock(req)) }
                else { Err("Expected a device and application endpoint") }
            })
        };
        return Some((envelope, request));
    }
    Some((envelope, parse(path, value)))
}

pub(super) fn receive(publisher: &Publisher, ingress: &mpsc::Sender<Request>, p: &Publish) {
    let Some((envelope, action)) = decode(&publisher.0.base, p) else {
        return;
    };
    let mut pending = publisher.0.pending.lock().unwrap();
    if pending.closed {
        return;
    }
    let actionable = action.is_ok();
    let error = match action {
        Ok(action)
            if pending.admin.reserved + pending.admin.replies.len() < publisher.0.capacity =>
        {
            match ingress.try_reserve() {
                Ok(slot) => {
                    pending.admin.reserved += 1;
                    drop(pending);
                    slot.send(Request {
                        action,
                        reply: Reply {
                            publisher: publisher.clone(),
                            envelope: Some(envelope),
                            ticket: None,
                        },
                    });
                    return;
                }
                Err(_) => "Bridge admin queue unavailable",
            }
        }
        Ok(_) => "Bridge admin queue full",
        Err(message) => message,
    };
    if actionable {
        pending.overflows.commands += 1;
        pending.latest_rejection = Some((envelope.path.clone(), "admin request"));
    }
    // Keep refusals separate from capacity promised to already accepted requests.
    if pending.admin.refused.len() < publisher.0.capacity.max(PIPELINE) {
        let response = if actionable {
            envelope.delivery_refused(error)
        } else {
            envelope.response(Value::Null, Some(error))
        };
        pending.admin.refused.push_back(response);
    } else {
        if !actionable {
            pending.overflows.commands += 1;
        }
        pending.latest_rejection = Some((envelope.path, "admin response capacity"));
    }
    drop(pending);
    publisher.0.wake.notify_one();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Idle,
    Available,
    Scheduled,
    Updating,
}

impl Publisher {
    /// Publish the installed image from a device query plus its catalogue
    /// result. A completed transfer alone is insufficient to change installed.
    pub fn update_state(
        &self,
        device: &str,
        installed: u32,
        latest: u32,
        phase: UpdatePhase,
        progress: Option<u8>,
    ) -> Result<(), SubmitError> {
        if progress.is_some_and(|n| n > 100 || phase != UpdatePhase::Updating) {
            return Err(SubmitError::Invalid);
        }
        let mut update =
            json!({"installed_version":installed,"latest_version":latest,"state":phase});
        if let Some(progress) = progress {
            update["progress"] = json!(progress);
        }
        self.state(device, "update", update)
    }

    /// The radio owner calls this after observing the resulting admission state.
    /// End is Unix milliseconds, matching herdsman's permitJoinEnd.
    pub fn permit_state(&self, open: bool, end_ms: Option<u64>) -> Result<(), SubmitError> {
        if !open && end_ms.is_some() {
            return Err(SubmitError::Invalid);
        }
        let mut value = json!({"api_version":API_VERSION,"permit_join":open});
        if let Some(end) = end_ms {
            value["permit_join_end"] = json!(end);
        }
        let mut pending = self.0.pending.lock().unwrap();
        if pending.closed {
            return Err(SubmitError::Closed);
        }
        if pending.store.is_some() {
            drop(pending);
            return self.enqueue_configs(&mut vec![(
                format!("{}/bridge/info", self.0.base),
                serde_json::to_vec(&value).expect("valid state"),
            )]);
        }
        pending.admin.info = Some(Publication {
            topic: "bridge/info".into(),
            bytes: serde_json::to_vec(&value).expect("valid state"),
            retained: true,
        });
        drop(pending);
        self.0.wake.notify_one();
        Ok(())
    }
}

#[cfg(test)]
mod ingress_tests {
    use super::*;

    #[test]
    fn door_lock_requires_a_fresh_versioned_request_with_an_id_and_no_inline_pin() {
        let topic = "zb/bridge/v1/request/device/lock";
        let body = json!({"id":"test-lock","endpoint":1,"action":"unlock","transaction":"turn-1"});
        let packet = |topic: &str, body: &Value| {
            Publish::new(topic, rumqttc::v5::mqttbytes::QoS::AtLeastOnce, serde_json::to_vec(body).unwrap(), None)
        };
        let parse = |p: &Publish| decode("zb", p).unwrap().1;
        assert!(matches!(parse(&packet(topic, &body)), Ok(Action::DoorLock(_))));
        assert!(parse(&packet("zb/bridge/request/device/lock", &body)).is_err());
        let mut retained = packet(topic, &body);
        retained.retain = true;
        assert!(decode("zb", &retained).is_none());
        let mut missing_transaction = body.clone();
        missing_transaction.as_object_mut().unwrap().remove("transaction");
        assert!(parse(&packet(topic, &missing_transaction)).is_err());
        let mut inline = body;
        inline["pin"] = json!("test");
        assert!(parse(&packet(topic, &inline)).is_err());
    }

    #[test]
    fn versioned_commands_replay_on_their_own_response_topic() -> anyhow::Result<()> {
        use rumqttc::v5::{MqttOptions, mqttbytes::QoS};
        let dir = tempfile::tempdir()?;
        let journal = Journal::open(dir.path())?;
        let (publisher, mut worker, _) = crate::mqtt::channel(
            MqttOptions::new("test", "localhost", 1883), "zb", 2,
        )?;
        let mut requests = worker.enable_admin()?;
        let ingress = worker.admin.as_ref().unwrap();
        let mut packet = Publish::new("zb/bridge/v1/request/permit_join", QoS::AtLeastOnce,
            br#"{"transaction":"same","time":5}"#.as_slice(), None);
        packet.retain = true;
        receive(&publisher, ingress, &packet);
        assert!(requests.try_recv().is_err());
        packet.retain = false;
        for (namespace, dispatch) in [("v1/", true), ("v1/", false), ("", true)] {
            packet.topic = format!("zb/bridge/{namespace}request/permit_join").into();
            receive(&publisher, ingress, &packet);
            let Request { action, reply } = requests.try_recv()?;
            let reserved = reply.reserve(&journal, &action)?;
            assert_eq!(reserved.is_some(), dispatch);
            if let Some(reply) = reserved { reply.ok(json!({"time":5}))?; }
            let result = publisher.0.pending.lock().unwrap().admin.take().unwrap();
            assert_eq!(result.topic, format!("bridge/{namespace}response/permit_join"));
            let body: Value = serde_json::from_slice(&result.bytes)?;
            assert_eq!(body["transaction"], "same");
            assert_eq!(body["data"]["time"], 5);
        }
        Ok(())
    }

    #[test]
    fn saturated_redelivery_keeps_the_original_transaction_outcome() -> anyhow::Result<()> {
        use rumqttc::v5::{MqttOptions, mqttbytes::QoS};
        let dir = tempfile::tempdir()?;
        let journal = Journal::open(dir.path())?;
        let (publisher, mut worker, _) = crate::mqtt::channel(
            MqttOptions::new("test", "127.0.0.1", 1883), "bridge", 1,
        )?;
        let mut requests = worker.enable_admin()?;
        let ingress = worker.admin.as_ref().unwrap();
        let packet = Publish::new("bridge/bridge/request/permit_join", QoS::AtLeastOnce,
            br#"{"transaction":"one","time":12}"#.as_slice(), None);
        let mut radio_dispatches = 0;
        let mut next = || -> anyhow::Result<Option<Reply>> {
            let Request { action, reply } = requests.try_recv()?;
            let reserved = reply.reserve(&journal, &action)?;
            radio_dispatches += usize::from(reserved.is_some());
            Ok(reserved)
        };
        receive(&publisher, ingress, &packet);
        let first = next()?.expect("first delivery dispatches");
        let retry = || -> anyhow::Result<()> {
            receive(&publisher, ingress, &packet);
            let mut pending = publisher.0.pending.lock().unwrap();
            let bytes = pending.admin.refused.pop_front().expect("retryable refusal").bytes;
            let response: Value = serde_json::from_slice(&bytes)?;
            assert_eq!(response["transaction"], "one");
            assert_eq!(response["retry"], "same_transaction");
            Ok(())
        };
        retry()?; // In flight, holding the sole promised reply slot.
        first.ok(json!({"time":12}))?;
        retry()?; // Durable result, with the original publication still queued.
        let original = publisher.0.pending.lock().unwrap().admin.take().unwrap().bytes;
        receive(&publisher, ingress, &packet);
        assert!(next()?.is_none());
        assert_eq!(radio_dispatches, 1);
        let replay = publisher.0.pending.lock().unwrap().admin.take().unwrap().bytes;
        assert_eq!(replay, original);
        let response: Value = serde_json::from_slice(&replay)?;
        assert_eq!(response["status"], "ok");
        assert!(response.get("retry").is_none());
        Ok(())
    }
}
