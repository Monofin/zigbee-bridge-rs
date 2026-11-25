//! Device and subscriber witnesses for a cold address table after migration.
use super::*;

pub(super) const MOTION: u16 = 2;

pub(super) fn bind_sensor(devices: &mut Bindings) -> Result<()> {
    let mut facts = vec![
        Observation::Target {
            ieee: format!("{:016x}", ieee(MOTION)), short: 0x1202,
            identity_source: IdentitySource::Announcement,
        },
        Observation::SimpleDescriptor {
            sequence: 1, endpoint: 1,
            descriptor: Descriptor { profile: 0x104, device: 0x0402, version: 0,
                servers: vec![0, 1, 0x0500], clients: vec![] },
        },
    ];
    for (attribute, text) in [(4, "TUYATEC-zn9wyqtr"), (5, "RH3040")] {
        let mut bytes = vec![text.len() as u8];
        bytes.extend(text.bytes());
        facts.push(Observation::Attribute { sequence: 2, endpoint: 1, cluster: 0, attribute,
            result: AttributeResult::Value { data_type: 0x42, bytes } });
    }
    facts.push(Observation::Finished);
    devices.add(&InterviewReport { finished: true,
        entries: facts.into_iter().map(|observation| Entry { unix_ms: 0, observation }).collect() },
        &BTreeMap::from([(ieee(MOTION), "motion".into())]))?;
    Ok(())
}

pub(super) fn frames() -> Vec<Vec<u8>> {
    let mut illuminance = incoming(environment::DEVICE, 0x0400, &[0x18, 12, 10, 0, 0, 0x21, 1, 0]);
    illuminance[5] = 2; // Ember incoming multicast, delivered to a local group endpoint.
    illuminance[14..16].copy_from_slice(&0x1234_u16.to_le_bytes());
    let button = incoming(BUTTON, 6, &[0x11, 0x34, 0xfd, 0]);
    let mut frames = vec![illuminance, button.clone(), button];
    // Captured zone-status payloads from the Oven sensor, including its zone ID.
    for (seq, status) in [(121, 1), (123, 0)] {
        let mut packet = incoming(MOTION, 0x0500, &[0x19, seq, 0, status, 0, 0, 23, 0, 0]);
        packet[10] = 1;
        frames.push(packet);
    }
    frames
}

pub(super) async fn observe(base: &str) -> Result<()> {
    let (client, mut events) = AsyncClient::new(test_broker("observation-witness".into())?, 32);
    client.subscribe(format!("{base}/#"), QoS::AtLeastOnce).await?;
    let mut illuminance = false;
    let mut presses = 0;
    let mut edges = Vec::new();
    loop {
        if let Event::Incoming(Incoming::Publish(msg)) = events.poll().await? {
            if msg.retain { continue; }
            if msg.topic == format!("{base}/environment") {
                illuminance |= serde_json::from_slice::<Value>(&msg.payload)?["illuminance"].as_f64() == Some(1.0);
            }
            if msg.topic == format!("{base}/button/action") {
                ensure!(msg.payload.as_ref() == b"single");
                presses += 1;
            }
            if msg.topic == format!("{base}/motion")
                && let Some(value) = serde_json::from_slice::<Value>(&msg.payload)?["occupancy"].as_bool()
                && edges.last() != Some(&value) {
                edges.push(value);
            }
            if illuminance && edges.len() >= 2 {
                ensure!(edges == [true, false] && presses == 1, "edges {edges:?}, presses {presses}");
                return Ok(());
            }
        }
    }
}
