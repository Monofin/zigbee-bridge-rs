//! Bind a standard IAS Zone after its own endpoint descriptor and type are known.
use super::{
    Bindings, Identity, Read,
    ember::{Pending, submitted},
};
use crate::mqtt::{Publisher, discovery::Feature};
use anyhow::Result;
use std::collections::BTreeMap;
use zigbee_devices::ias::{self, Kind};

pub(super) fn feature(kind: Kind) -> Feature {
    match kind {
        Kind::Contact => Feature::Contact,
        _ => Feature::Binary(kind.property(), kind.device_class()),
    }
}

pub(super) fn receive(
    devices: &mut Bindings,
    out: &Publisher,
    pending: &mut BTreeMap<u8, Pending>,
    peer: Identity,
    short: u16,
    frame: zigbee_zcl::Frame<'_>,
) -> Result<()> {
    let seq = frame.header.sequence;
    let requested = pending.get(&seq).and_then(|p| match p.read {
        Some(Read::Ias(attribute))
            if (p.peer, p.short, p.cluster) == (peer, short, ias::CLUSTER) =>
        {
            Some(attribute)
        }
        _ => None,
    });
    let obs = ias::observe(frame, requested);
    if obs.read_complete {
        pending.remove(&seq);
    }
    let b = devices.devices.get_mut(&peer).expect("matched endpoint");
    let Some(zone) = b.zone.as_mut() else {
        return Ok(());
    };
    let before = zone.kind();
    if let Some(id) = obs.zone_type {
        zone.zone_type = Some(id);
        submitted(out.state(&b.name, "zone_type", id.into()))?;
    }
    let after = zone.kind();
    // Keep an early observation within this membership while a sleepy sensor
    // answers its first ZoneType read. Never reinterpret a previous known type.
    let alarm_status = if after.is_some() {
        obs.status.or(zone.untyped_status.take())
    } else {
        if zone.zone_type.is_some() {
            zone.untyped_status = None;
        } else if let Some(status) = obs.status {
            zone.untyped_status = Some(status);
        }
        None
    };
    if before != after {
        let ep = b.discovery(peer);
        if let Some(kind) = before {
            submitted(out.remove_discovery(out.discovery_prefix(), &ep, feature(kind)))?;
            submitted(out.state(&b.name, kind.property(), serde_json::Value::Null))?;
        }
        if let Some(kind) = after {
            submitted(out.discover(out.discovery_prefix(), &ep, feature(kind)))?;
        }
    }
    if let Some(status) = obs.status.or(alarm_status) {
        let mut fields = serde_json::Map::new();
        fields.insert("zone_status".into(), status.into());
        if let Some(kind) = after {
            fields.insert(kind.property().into(), kind.mqtt_state(status).into());
        }
        submitted(out.state_fields(&b.name, "zone_status", fields))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        interview_log::{
            AttributeResult, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
        },
        mqtt::{Command, Operation},
    };
    use serde_json::Value;

    fn endpoints(profile: u16, inputs: &[u16], kind: Option<u16>, vendor_motion: bool) -> Bindings {
        let ieee = 0x0200_0000_0000_0041u64;
        let ep = 1;
        let mut facts = vec![
            Observation::Target {
                ieee: format!("{ieee:016x}"),
                short: 1,
                identity_source: IdentitySource::Announcement,
            },
            Observation::ActiveEndpoints {
                sequence: 1,
                endpoints: vec![ep],
            },
            Observation::SimpleDescriptor {
                sequence: 2,
                endpoint: ep,
                descriptor: Descriptor {
                    profile,
                    device: 0x0402,
                    version: 1,
                    servers: inputs.to_vec(),
                    clients: vec![],
                },
            },
        ];
        let mut attr = |cluster, attribute, data_type, bytes| {
            facts.push(Observation::Attribute {
                sequence: 3,
                endpoint: ep,
                cluster,
                attribute,
                result: AttributeResult::Value { data_type, bytes },
            })
        };
        if let Some(id) = kind {
            attr(ias::CLUSTER, 1, 0x31, id.to_le_bytes().to_vec());
        }
        if vendor_motion {
            for (k, v) in [(4, "TUYATEC-zn9wyqtr"), (5, "RH3040")] {
                let mut bytes = vec![v.len() as u8];
                bytes.extend(v.as_bytes());
                attr(0, k, 0x42, bytes);
            }
        }
        facts.push(Observation::Finished);
        let mut b = Bindings::default();
        b.add(
            &InterviewReport {
                finished: true,
                entries: facts
                    .into_iter()
                    .map(|observation| Entry {
                        unix_ms: 0,
                        observation,
                    })
                    .collect(),
            },
            &BTreeMap::from([(ieee, "Fixture".to_string())]),
        )
        .unwrap();
        b
    }

    #[test]
    fn a_matched_vendor_profile_keeps_its_existing_occupancy_route() {
        let b = endpoints(0x0104, &[0, 1, ias::CLUSTER], Some(0x15), true);
        let binding = b.devices.values().next().unwrap();
        assert!(binding.zone.is_none());
        let ha = b
            .discovery_snapshot("zigbee2mqtt", "homeassistant", None)
            .unwrap();
        assert!(ha.keys().any(|topic| topic.ends_with("/occupancy/config")));
        assert!(!ha.keys().any(|topic| topic.ends_with("/contact/config")));
    }

    #[tokio::test]
    async fn unknown_sensor_publishes_a_typed_entity_after_its_own_read_response() {
        let mut b = endpoints(0x0104, &[ias::CLUSTER], None, false);
        let peer = *b.devices.keys().next().unwrap();
        let (out, _worker, _commands) = crate::mqtt::channel(
            crate::mqtt::MqttOptions::new("ias-test", "localhost", 1883),
            "zigbee2mqtt",
            64,
        )
        .unwrap();
        let mut pending = BTreeMap::new();
        let notification = zigbee_zcl::Frame::decode(&[0x19, 3, 0, 1, 0, 0, 4, 0, 0]).unwrap();
        receive(&mut b, &out, &mut pending, peer, 20, notification).unwrap();
        assert_eq!(b.devices[&peer].zone.unwrap().untyped_status, Some(1));
        pending.insert(
            7,
            Pending {
                route_epoch: None,
                schedule: None,
                peer,
                short: 20,
                destination:crate::ash::Destination::fixture(peer.0,20),
                cluster: ias::CLUSTER,
                read: Some(Read::Ias(ias::Attribute::Type)),
                until: tokio::time::Instant::now(),
            },
        );
        let response = zigbee_zcl::Frame::decode(&[0x18, 7, 1, 1, 0, 0, 0x31, 0x15, 0]).unwrap();
        receive(&mut b, &out, &mut pending, peer, 21, response).unwrap();
        assert!(pending.contains_key(&7));
        assert_eq!(b.devices[&peer].zone.unwrap().kind(), None);
        receive(&mut b, &out, &mut pending, peer, 20, response).unwrap();
        assert!(pending.is_empty());
        assert_eq!(b.devices[&peer].zone.unwrap().untyped_status, None);
        assert_eq!(b.devices[&peer].zone.unwrap().kind(), Some(Kind::Contact));
        let configs = b
            .discovery_snapshot("zigbee2mqtt", "homeassistant", None)
            .unwrap();
        assert!(
            configs
                .keys()
                .any(|topic| topic.ends_with("/contact/config"))
        );
    }

    #[test]
    fn only_confirmed_endpoint_type_gets_binary_discovery_and_semantic_reads() {
        for profile in [0x0104, 0xc05e] {
            let b = endpoints(profile, &[ias::CLUSTER], Some(0x15), false);
            let ha = b
                .discovery_snapshot("zigbee2mqtt", "homeassistant", None)
                .unwrap();
            let contact = ha
                .iter()
                .find(|(topic, _)| topic.ends_with("/contact/config"))
                .unwrap()
                .1;
            assert_eq!(contact["device_class"], "opening");
            assert_eq!(contact["payload_on"], "false");
            for (key, id) in [("contact", 2), ("zone_status", 2), ("zone_type", 1)] {
                let input = |operation| Command {
                    device: "Fixture".into(),
                    operation,
                    properties: [(key.to_owned(), Value::Null)].into_iter().collect(),
                };
                let (_, plan) = b.plan(&input(Operation::Get), 17).unwrap();
                assert_eq!(plan[0].0.frame, [0x10, 17, 0, id, 0]);
                assert!(b.plan(&input(Operation::Set), 17).is_err());
            }
        }
        for (profile, inputs, kind) in [
            (0x0104, vec![ias::CLUSTER], None),
            (0x0104, vec![ias::CLUSTER], Some(0xbeef)),
            (0x0109, vec![ias::CLUSTER], Some(0x15)),
            (0x0104, vec![], Some(0x15)),
        ] {
            let b = endpoints(profile, &inputs, kind, false);
            let ha = b
                .discovery_snapshot("zigbee2mqtt", "homeassistant", None)
                .unwrap();
            assert!(!ha.keys().any(|t| t.contains("/binary_sensor/")));
        }
    }
}
