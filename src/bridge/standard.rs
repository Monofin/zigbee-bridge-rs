//! Descriptor-backed standard access for a member with or without a product profile.
use super::{Bindings, Identity, Planned, Read};
use crate::{
    interview_log::Descriptor,
    mqtt::{Command, Operation},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value as Json, json};
use zigbee_devices::lighting::Request;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header, attributes::Status, foundation::Payload,
    standard_attributes::wire_type, value::Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Attribute {
    cluster: u16,
    id: u16,
}

impl Attribute {
    fn key(self, endpoint: u8) -> String {
        format!("zcl_{endpoint:02x}_{:04x}_{:04x}", self.cluster, self.id)
    }

    fn parse(key: &str) -> Result<(u8, Self)> {
        let mut parts = key.split('_');
        ensure!(
            parts.next() == Some("zcl"),
            "expected a standard attribute key"
        );
        let endpoint = u8::from_str_radix(parts.next().context("missing endpoint")?, 16)?;
        let cluster = u16::from_str_radix(parts.next().context("missing cluster")?, 16)?;
        let id = u16::from_str_radix(parts.next().context("missing attribute")?, 16)?;
        let attribute = Self { cluster, id };
        ensure!(
            (1..=240).contains(&endpoint) && attribute.key(endpoint) == key,
            "invalid standard attribute key"
        );
        Ok((endpoint, attribute))
    }

    fn request(self, endpoint: u8, seq: u8, op: Operation, value: &Json) -> Result<Planned> {
        ensure!(
            wire_type(self.cluster, self.id).is_some(),
            "unknown or non-scalar standard attribute"
        );
        let (kind, command, payload, response) = match op {
            Operation::Get => (
                FrameType::General,
                0,
                self.id.to_le_bytes().to_vec(),
                Some(Read::Standard(self)),
            ),
            // OnOff is a read-only attribute. Its server implements On and Off commands.
            Operation::Set if (self.cluster, self.id) == (6, 0) => {
                let on = match value.as_str() {
                    Some("ON") => true,
                    Some("OFF") => false,
                    _ => value.as_bool().context("use ON, OFF or a boolean")?,
                };
                (FrameType::ClusterSpecific, u8::from(on), Vec::new(), None)
            }
            Operation::Set => anyhow::bail!("no standard control mapped to this attribute"),
        };
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type: kind,
                manufacturer: None,
                direction: Direction::ClientToServer,
                disable_default_response: true,
                sequence: seq,
                command_id: command,
            },
            payload: &payload,
        }
        .encode_into(&mut frame);
        Ok((
            Request {
                endpoint,
                cluster: self.cluster,
                frame,
            },
            response,
        ))
    }
}

pub(super) fn application(endpoint: u8, desc: &Descriptor) -> bool {
    (1..=240).contains(&endpoint) && matches!(desc.profile, 0x0104 | 0xc05e)
}

impl Bindings {
    // Some remotes advertise On/Off as a server for vendor-specific button events.
    fn controls_on_off(&self, id: Identity) -> bool {
        self.devices.get(&id).is_some_and(|b| {
            b.button.is_none()
                && b.datapoints.is_none()
                && (b.household.has_relay()
                    || b.is_light
                        && b.permits(zigbee_devices::lighting::Property::State, |a| a.write))
        })
    }

    /// Include generic telemetry for endpoints that already have native functions.
    pub fn set_all_standard_reports(&mut self, enabled: bool) {
        self.all_standard_reports = enabled;
    }

    pub(super) fn standard_plan(
        &self,
        command: &Command,
        seq: u8,
    ) -> Result<(Identity, Vec<Planned>)> {
        ensure!(
            command.properties.len() == 1,
            "address one standard attribute per request"
        );
        let (key, value) = command
            .properties
            .iter()
            .next()
            .context("missing standard attribute")?;
        let (ep, attribute) = Attribute::parse(key)?;
        let ieee = self.peer(&command.device)?;
        ensure!(!matches!(attribute.cluster,4|5|zigbee_devices::equipment::DOOR_LOCK) || self.allows_network_security(ieee),
            "lock security policy does not permit this network-security request");
        let descriptor = self.peers[&ieee]
            .1
            .get(&ep)
            .context("endpoint absent from interview")?;
        ensure!(
            application(ep, descriptor) && descriptor.servers.contains(&attribute.cluster),
            "standard server absent from endpoint"
        );
        if command.operation == Operation::Set {
            ensure!(
                self.controls_on_off((ieee, ep)),
                "endpoint has no confirmed on/off actuator"
            );
        }
        Ok((
            (ieee, ep),
            vec![attribute.request(ep, seq, command.operation, value)?],
        ))
    }
}

/// Preserve the on-wire magnitude. Units, sentinels and scaling depend on the attribute.
/// Wide integers are strings so JSON consumers using binary64 retain all their bits.
fn scalar(value: Value<'_>) -> Option<Json> {
    let bytes = value.as_bytes();
    let ty = value.data_type().id();
    match ty {
        0x10 => match bytes {
            [0] => Some(false.into()),
            [1] => Some(true.into()),
            _ => None,
        },
        0x42 => {
            let (&n, s) = bytes.split_first()?;
            (n != 255 && usize::from(n) == s.len()).then_some(())?;
            std::str::from_utf8(s).ok().map(Json::from)
        }
        0x39 => serde_json::Number::from_f64(f64::from(f32::from_le_bytes(bytes.try_into().ok()?)))
            .map(Json::Number),
        0x08..=0x0f | 0x18..=0x2f | 0x30 | 0x31 | 0xe2 => {
            let signed = (0x28..=0x2f).contains(&ty);
            let extension = if signed && bytes.last()? & 0x80 != 0 {
                0xff
            } else {
                0
            };
            let mut raw = [extension; 8];
            raw.get_mut(..bytes.len())?.copy_from_slice(bytes);
            Some(if signed {
                let v = i64::from_le_bytes(raw);
                if bytes.len() > 6 {
                    json!(v.to_string())
                } else {
                    json!(v)
                }
            } else {
                let v = u64::from_le_bytes(raw);
                if bytes.len() > 6 {
                    json!(v.to_string())
                } else {
                    json!(v)
                }
            })
        }
        _ => None,
    }
}

/// Successful reads must correlate to the same attribute. A failed read is a
/// status observation only. Manufacturer and type mismatches never yield a value.
pub(super) fn observe(
    cluster: u16,
    frame: Frame<'_>,
    requested: Option<Attribute>,
    publish_reports: bool,
) -> (bool, Vec<(Attribute, Json)>) {
    if frame.header.direction != Direction::ServerToClient || frame.header.manufacturer.is_some() {
        return (false, Vec::new());
    }
    let mut values = Vec::new();
    let mut add = |id, value: Value<'_>| {
        if wire_type(cluster, id) == Some(value.data_type().id())
            && let Some(raw) = scalar(value)
        {
            values.push((
                Attribute { cluster, id },
                json!({"type":value.data_type().id(),"raw":raw}),
            ));
            true
        } else {
            false
        }
    };
    match frame.header.general_command() {
        Some(zigbee_zcl::foundation::GeneralCommand::ReportAttributes) if publish_reports => {
            if let Ok(Payload::ReportAttributes(rows)) = Payload::decode(frame) {
                for r in rows {
                    add(r.id, r.value);
                }
            }
            (false, values)
        }
        Some(zigbee_zcl::foundation::GeneralCommand::ReadAttributesResponse)
            if requested.is_some() =>
        {
            let key = requested.unwrap();
            let Ok(Payload::ReadAttributesResponse(rows)) = Payload::decode(frame) else {
                return (false, values);
            };
            let mut matching = rows
                .into_iter()
                .filter(|r| r.id == key.id && cluster == key.cluster);
            let Some(row) = matching.next() else {
                return (false, values);
            };
            if matching.next().is_some() {
                return (false, values);
            }
            let complete = match row.result {
                Status::Success(value) => add(row.id, value),
                Status::Error(status) => {
                    values.push((key, json!({"status":status.get()})));
                    true
                }
            };
            (complete, values)
        }
        _ => (false, values),
    }
}

pub(super) fn publish(
    values: Vec<(Attribute, Json)>,
    ep: u8,
    name: &str,
    out: &crate::mqtt::Publisher,
) -> Result<()> {
    for (attr, value) in values {
        match out.event(name, &attr.key(ep), value) {
            Ok(()) | Err(crate::mqtt::SubmitError::Full) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

impl Bindings {
    pub(super) fn describe_standard(&self, query: &crate::mqtt::admin::Describe) -> Result<Json> {
        let ieee = self.peer(&query.id)?;
        let (name, endpoints) = &self.peers[&ieee];
        let Some(ep) = query.endpoint else {
            return Ok(json!({"id":name,"endpoints":endpoints.keys().collect::<Vec<_>>()}));
        };
        let desc = endpoints
            .get(&ep)
            .context("endpoint absent from interview")?;
        if let Some(cluster) = query.cluster {
            ensure!(
                zigbee_zcl::device_type::has_profile(desc.profile)
                    && desc.servers.contains(&cluster),
                "standard server absent from endpoint"
            );
            let readable = application(ep, desc);
            let mut entries = zigbee_zcl::standard_attributes::attributes(cluster)
                .filter(|(id, _, _)| query.after.is_none_or(|after| *id > after))
                .peekable();
            let page: Vec<_> = entries.by_ref().take(32).collect();
            let next = entries
                .peek()
                .and_then(|_| page.last().map(|&(id, _, _)| id));
            let attrs: Vec<_> = page
                .into_iter()
                .map(|(id, ty, label)| {
                    json!({
                        "attribute":id,"name":label,"type":ty,"key":readable.then(||Attribute{cluster,id}.key(ep)),
                        "set_on_off":cluster==6 && id==0 && self.controls_on_off((ieee,ep)),
                    })
                })
                .collect();
            return Ok(
                json!({"id":name,"endpoint":ep,"cluster":cluster,"standard_access":readable,"catalogue_attributes":attrs,
                             "next_after":next}),
            );
        }
        let types: Vec<_> = zigbee_zcl::device_type::lookup(desc.profile, desc.device)
            .iter()
            .map(|t| t.name)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(
            json!({"id":name,"endpoint":ep,"profile":desc.profile,"device_type":desc.device,
                  "type_names":types,"servers":desc.servers,"clients":desc.clients,
                  "standard_access":application(ep,desc)}),
        )
    }
}

/// Only commands whose complete, standard payload was decoded are emitted.
pub(super) fn client_command(
    descriptor: &Descriptor,
    cluster: u16,
    frame: Frame<'_>,
) -> Option<Json> {
    if !descriptor.clients.contains(&cluster)
        || frame.header.direction != Direction::ClientToServer
        || frame.header.manufacturer.is_some()
        || super::remote::cluster_name(cluster).is_none()
    {
        return None;
    }
    let command = zigbee_zcl::cluster::Command::decode(cluster, frame).ok()?;
    let mut event=json!({"cluster":cluster,"command":frame.header.command_id,"name":command.name()});
    use zigbee_zcl::cluster::{Command,LevelControl};
    match command {
        Command::LevelControl(LevelControl::Move {move_mode,rate,..} | LevelControl::MoveWithOnOff {move_mode,rate,..}) => {
            event["move_mode"]=json!(move_mode); event["rate"]=json!(rate);
        }
        Command::LevelControl(LevelControl::Step {step_mode,step_size,..} | LevelControl::StepWithOnOff {step_mode,step_size,..}) => {
            event["step_mode"]=json!(step_mode); event["step_size"]=json!(step_size);
        }
        _=>()
    }
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn descriptor(servers: Vec<u16>, clients: Vec<u16>) -> Descriptor {
        Descriptor {
            profile: 0x0104,
            device: 0xabcd,
            version: 1,
            servers,
            clients,
        }
    }

    #[test]
    fn unfamiliar_device_requires_its_actual_server_and_application_endpoint() -> Result<()> {
        let mut devices = Bindings::default();
        devices.peers.insert(
            1,
            (
                "fixture".into(),
                [
                    (3, descriptor(vec![6, 0x402], vec![])),
                    (4, descriptor(vec![], vec![6])),
                ]
                .into_iter()
                .collect(),
            ),
        );
        let plan = |op, key: &str, v| {
            devices.plan(
                &Command {
                    device: "fixture".into(),
                    operation: op,
                    properties: Map::from_iter([(key.into(), v)]),
                },
                9,
            )
        };
        let (id, steps) = plan(Operation::Get, "zcl_03_0402_0000", json!(""))?;
        assert_eq!(id, (1, 3));
        assert_eq!(steps[0].0.frame, [0x10, 9, 0, 0, 0]);
        assert!(plan(Operation::Set, "zcl_03_0006_0000", json!("OFF")).is_err());
        assert!(plan(Operation::Set, "zcl_03_0402_0000", json!(20)).is_err());
        assert!(plan(Operation::Set, "zcl_04_0006_0000", json!("ON")).is_err());
        assert!(plan(Operation::Get, "zcl_03_fc00_0000", json!("")).is_err());
        let desc = crate::mqtt::admin::Describe {
            id: "fixture".into(),
            endpoint: Some(3),
            cluster: None,
            after: None,
        };
        let description = devices.describe_standard(&desc)?;
        assert_eq!(description["type_names"], json!([]));
        assert_eq!(description["servers"], json!([6, 0x402]));
        Ok(())
    }

    #[test]
    fn typed_samples_keep_signed_values_and_separate_errors_from_readings() {
        let key = Attribute {
            cluster: 0x0402,
            id: 0,
        };
        let bytes = [0x18, 7, 1, 0, 0, 0, 0x29, 0x9c, 0xff];
        let frame = Frame::decode(&bytes).unwrap();
        let (complete, values) = observe(key.cluster, frame, Some(key), false);
        assert!(complete);
        assert_eq!(values[0].1, json!({"type":0x29,"raw":-100}));
        assert!(observe(key.cluster, frame, None, true).1.is_empty());
        let wrong_type = [0x18, 7, 1, 0, 0, 0, 0x21, 1, 0];
        assert_eq!(
            observe(
                key.cluster,
                Frame::decode(&wrong_type).unwrap(),
                Some(key),
                false
            ),
            (false, vec![])
        );
        let wrong = [0x18, 7, 0x0a, 0, 0, 0x21, 1, 0];
        assert!(
            observe(key.cluster, Frame::decode(&wrong).unwrap(), None, true)
                .1
                .is_empty()
        );
        let denied = [0x18, 7, 1, 0, 0, 0x86];
        assert_eq!(
            observe(
                key.cluster,
                Frame::decode(&denied).unwrap(),
                Some(key),
                false
            )
            .1[0]
                .1,
            json!({"status":0x86})
        );
    }

    #[test]
    fn client_action_validates_role_direction_and_payload() {
        let d = descriptor(vec![], vec![6]);
        let command = Frame::decode(&[0x11, 1, 2]).unwrap();
        assert_eq!(client_command(&d, 6, command).unwrap()["name"], "Toggle");
        assert!(client_command(&descriptor(vec![6], vec![]), 6, command).is_none());
        assert!(client_command(&d, 6, Frame::decode(&[0x11, 1, 2, 99]).unwrap()).is_none());
        assert!(client_command(&d, 6, Frame::decode(&[0x19, 1, 2]).unwrap()).is_none());
    }
}

#[cfg(test)]
#[test]
fn all_indexed_type_templates_survive_interview_and_description() -> Result<()> {
    use crate::interview_log::{Entry, IdentitySource, InterviewReport, Observation};
    use std::collections::{BTreeMap, BTreeSet};
    use zigbee_zcl::device_type::{ALL, Role};
    #[derive(Default)]
    struct Coverage {
        ids: BTreeSet<u16>,
        entities: BTreeSet<u16>,
        triggers: BTreeSet<u16>,
    }
    let mut profiles = BTreeMap::<u16, Coverage>::new();
    for template in ALL {
        let descriptor = Descriptor {
            profile: template.profile,
            device: template.device,
            version: 1,
            servers: template
                .clusters
                .iter()
                .filter(|c| c.server == Role::Required)
                .map(|c| c.id)
                .collect(),
            clients: template
                .clusters
                .iter()
                .filter(|c| c.client == Role::Required)
                .map(|c| c.id)
                .collect(),
        };
        let records = [
            Observation::Target {
                ieee: "0200000000000001".into(),
                short: 0x1234,
                identity_source: IdentitySource::Z2mDatabase,
            },
            Observation::ActiveEndpoints {
                sequence: 1,
                endpoints: vec![1],
            },
            Observation::SimpleDescriptor {
                sequence: 2,
                endpoint: 1,
                descriptor: descriptor.clone(),
            },
            Observation::Finished,
        ];
        let mut devices = Bindings::default();
        devices.add(
            &InterviewReport {
                finished: true,
                entries: records
                    .into_iter()
                    .map(|observation| Entry {
                        unix_ms: 0,
                        observation,
                    })
                    .collect(),
            },
            &Default::default(),
        )?;
        let profile = profiles.entry(template.profile).or_default();
        profile.ids.insert(template.device);
        for topic in devices.discovery_snapshot("bench", "ha", None)?.keys() {
            if topic.starts_with("ha/device_automation/") {
                profile.triggers.insert(template.device);
            } else if !topic.ends_with("/linkquality/config") {
                profile.entities.insert(template.device);
            }
        }
        let id = "0x0200000000000001";
        let desc = devices.describe_standard(&crate::mqtt::admin::Describe {
            id: id.into(),
            endpoint: Some(1),
            cluster: None,
            after: None,
        })?;
        assert!(
            desc["type_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == template.name)
        );
        assert_eq!(desc["servers"], json!(descriptor.servers));
        let outlet = matches!(
            (template.profile, template.device),
            (0x0104, 0x0051 | 0x010a) | (0x0104 | 0xc05e, 0x0010)
        );
        if outlet && descriptor.servers.contains(&6) {
            let set = Command {
                device: id.into(),
                operation: Operation::Set,
                properties: serde_json::Map::from_iter([("zcl_01_0006_0000".into(), json!("OFF"))]),
            };
            assert_eq!(devices.plan(&set, 9)?.1[0].0.frame, [0x11, 9, 0]);
        }
        // Only try a GET that this synthetic descriptor actually advertises.
        if let Some((cluster, attr)) = descriptor.servers.iter().find_map(|c| {
            zigbee_zcl::standard_attributes::attributes(*c)
                .next()
                .map(|(attr, _, _)| (*c, attr))
        }) {
            let key = Attribute { cluster, id: attr }.key(1);
            let page = devices.describe_standard(&crate::mqtt::admin::Describe {
                id: id.into(),
                endpoint: Some(1),
                cluster: Some(cluster),
                after: None,
            })?;
            let radio = application(1, &descriptor);
            assert_eq!(page["standard_access"], radio);
            assert_eq!(
                page["catalogue_attributes"][0]["key"].as_str(),
                radio.then_some(key.as_str())
            );
            let req = Command {
                device: id.into(),
                operation: Operation::Get,
                properties: serde_json::Map::from_iter([(key, json!(""))]),
            };
            assert_eq!(devices.plan(&req, 1).is_ok(), application(1, &descriptor));
        }
    }
    for (profile, c) in profiles {
        println!("Profile {profile:#06x}: {} of {} IDs with functional discovery ({} with entities, {} with triggers)",
            c.entities.union(&c.triggers).count(), c.ids.len(), c.entities.len(), c.triggers.len());
    }
    Ok(())
}
