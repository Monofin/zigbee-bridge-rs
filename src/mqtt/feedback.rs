//! Entity-specific HA state is derived from the observation that arrived.
//! The aggregate MQTT snapshot remains available to existing subscribers.
use super::discovery::{Endpoint, Feature, Sensor};
use super::{Delivery, Map, Pending, Publisher, SubmitError, Value, diagnostic_route};

const PREFIX: &str = "bridge/feedback/";
const LIGHT: &str = "@light";
const BARRIER: &str = "@barrier";
const LIGHT_FIELDS: &[&str] = &[
    "state",
    "brightness",
    "color",
    "color_temp",
    "color_mode",
    "effect",
];
const BARRIER_FIELDS: &[&str] = &["barrier_moving_state", "barrier_position"];

pub(super) fn owner(route: &str) -> Option<&str> {
    route
        .strip_prefix(PREFIX)?
        .rsplit_once('/')
        .map(|(device, _)| device)
}

fn topic(device: &str, key: &str) -> String {
    format!("{PREFIX}{device}/{key}")
}

// Each tuple links one HA subscription option to the observation it consumes.
// Multiple subscriptions on a cover or thermostat therefore change independently.
impl Feature {
    pub(super) fn feedback(self, ep: &Endpoint<'_>) -> Vec<(&'static str, String)> {
        use Feature::*;
        let state = |key: &str| vec![("state_topic", key.to_owned())];
        match self {
            Actions(_) | Button | Identify | Effect(_) | WriteOnlyText(_) => Vec::new(),
            RemoteTrigger { .. } => vec![("topic", format!("zcl_{:02x}_command", ep.endpoint))],
            Light { .. } => state(LIGHT),
            Switch | Valve => state("state"),
            Contact => state("contact"),
            DoorLock => state("lock_state"),
            FirmwareUpdate => state("update"),
            ReportedEffect(_) => state("effect"),
            LastSeen { .. } => state("last_seen"),
            Sensor(sensor) => state(sensor.metadata().0),
            BasicIo(io) => state(io.property()),
            Binary(property, _)
            | TextSensor(property)
            | CompositeSensor(property)
            | BooleanSetting(property)
            | Select { property, .. }
            | Number { property, .. }
            | StringBinary { property, .. }
            | Measurement { property, .. }
            | Numeric { property, .. } => state(property),
            Thermostat { setpoint, .. } => vec![
                ("mode_state_topic", "system_mode".into()),
                ("temperature_state_topic", setpoint.into()),
                ("current_temperature_topic", "local_temperature".into()),
            ],
            Barrier { .. } => vec![
                ("state_topic", BARRIER.into()),
                ("position_topic", "barrier_position".into()),
            ],
            Cover | StandardCover(_) => {
                use zigbee_devices::cover::Axis;
                let mut fields = state("state");
                if matches!(self, Cover) || matches!(self, StandardCover(c) if c.reads(Axis::Lift))
                {
                    fields.push(("position_topic", "position".into()));
                }
                if matches!(self, StandardCover(c) if c.reads(Axis::Tilt)) {
                    fields.push(("tilt_status_topic", "tilt".into()));
                }
                fields
            }
        }
    }

    fn source<'a>(self, ep: &'a Endpoint<'_>) -> &'a str {
        match self {
            Feature::FirmwareUpdate
            | Feature::LastSeen { .. }
            | Feature::Sensor(Sensor::LinkQuality)
            | Feature::RemoteTrigger { .. } => ep.device_name,
            _ => ep.topic_name,
        }
    }

    pub(super) fn set_feedback_topics(self, ep: &Endpoint<'_>, base: &str, config: &mut Value) {
        for (option, key) in self.feedback(ep) {
            config[option] = Value::String(format!("{base}/{}", topic(self.source(ep), &key)));
        }
    }
}

impl Publisher {
    /// A light needs a power observation from this coordinator session to
    /// interpret level and colour reports. A restored snapshot cannot grant it.
    pub fn has_confirmed_light_power(&self, device: &str) -> bool {
        self.0
            .pending
            .lock()
            .unwrap()
            .live_light
            .get(device)
            .and_then(|fields| fields.get("state"))
            .is_some_and(valid_power)
    }

    pub(super) fn register_feedback(
        &self,
        ep: &Endpoint<'_>,
        feature: Feature,
    ) -> Result<(), SubmitError> {
        let mut q = self.0.pending.lock().unwrap();
        let source = feature.source(ep);
        for (_, key) in feature.feedback(ep) {
            let input = if super::RADIO_FIELDS.contains(&key.as_str()) {
                diagnostic_route(source)
            } else {
                source.to_owned()
            };
            if !q
                .feedback
                .entry(input.clone())
                .or_default()
                .insert(key.clone())
            {
                continue;
            }
            if matches!(feature, Feature::RemoteTrigger { .. }) {
                continue;
            }
            // Materialize a checkpoint for a subscriber arriving after startup.
            let projection = if key == LIGHT {
                q.live_light.get(source).and_then(|v| project(&key, v, v))
            } else {
                q.snapshots.get(&input).and_then(|v| project(&key, v, v))
            };
            if let Some(fields) = projection {
                q.put(
                    &topic(source, &key),
                    fields,
                    Delivery::State,
                    None,
                    self.0.capacity,
                )?;
            }
        }
        self.0.wake.notify_one();
        Ok(())
    }
}

fn valid_power(value: &Value) -> bool {
    matches!(value.as_str(), Some("ON" | "OFF"))
}

fn project(
    key: &str,
    observed: &Map<String, Value>,
    snapshot: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    let keys = match key {
        LIGHT if snapshot.get("state").is_some_and(valid_power) => LIGHT_FIELDS,
        LIGHT => return None,
        BARRIER => BARRIER_FIELDS,
        "temperature" => {
            let value = observed.get(key)?;
            let mut fields = Map::from_iter([(key.to_owned(), value.clone())]);
            if let Some(sample) = observed.get("temperature_observation") {
                fields.insert("temperature_observation".into(), sample.clone());
            }
            return Some(fields);
        }
        field => {
            return Some(Map::from_iter([(
                field.to_owned(),
                observed.get(field)?.clone(),
            )]));
        }
    };
    keys.iter().any(|k| observed.contains_key(*k)).then(|| {
        keys.iter()
            .filter_map(|k| snapshot.get(*k).map(|v| ((*k).to_owned(), v.clone())))
            .collect()
    })
}

impl Pending {
    pub(super) fn invalidate_light_power(&mut self, device: &str) {
        self.live_light.remove(device);
        self.clear_snapshot(&topic(device, LIGHT));
    }

    pub(super) fn put_report(
        &mut self,
        device: &str,
        fields: Map<String, Value>,
        delivery: Delivery,
        action: Option<Vec<u8>>,
        capacity: usize,
    ) -> Result<(), SubmitError> {
        if delivery != Delivery::State {
            // All publications for an ordered observation occupy one FIFO slot.
            // Reserve bytes for the complete delivery before acknowledging it.
            let updates: Vec<_> = self
                .feedback
                .get(device)
                .into_iter()
                .flatten()
                .filter_map(|key| {
                    project(key, &fields, &fields)
                        .map(|projection| (topic(device, key), projection))
                })
                .collect();
            let payloads = updates
                .iter()
                .map(|(route, projection)| {
                    (
                        route.clone(),
                        serde_json::to_vec(projection).expect("JSON observation"),
                    )
                })
                .collect();
            let result =
                self.put_with_feedback(device, fields, delivery, action, payloads, capacity);
            if delivery == Delivery::Observation
                && (result.is_ok() || result == Err(SubmitError::Full))
            {
                for (route, projection) in updates {
                    self.snapshots.insert(route.clone(), projection);
                    if result.is_err() {
                        self.remove_state_slot(&route);
                        self.defer_state(&route);
                    } else if let Some(dirty) = self.dirty.get_mut(&route) {
                        dirty.followed_by_event = true;
                    }
                }
            }
            return result;
        }
        self.put(device, fields.clone(), delivery, action, capacity)?;
        if fields.keys().any(|k| LIGHT_FIELDS.contains(&k.as_str())) {
            let live = self.live_light.entry(device.to_owned()).or_default();
            live.extend(
                fields
                    .iter()
                    .filter(|(k, _)| LIGHT_FIELDS.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        let Some(keys) = self.feedback.get(device) else {
            return Ok(());
        };
        let owner = device
            .strip_prefix(super::RADIO_DIAGNOSTICS)
            .unwrap_or(device);
        let updates: Vec<_> = keys
            .iter()
            .filter_map(|key| {
                let snapshot = if key == LIGHT {
                    self.live_light.get(device)
                } else {
                    self.snapshots.get(device)
                };
                project(key, &fields, snapshot.unwrap_or(&fields)).map(|f| (topic(owner, key), f))
            })
            .collect();
        for (route, projected) in updates {
            self.put(&route, projected, delivery, None, capacity)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::{MqttOptions, admin::UpdatePhase, channel};
    use serde_json::json;

    fn endpoint() -> Endpoint<'static> {
        Endpoint {
            ieee: 42,
            endpoint: 1,
            topic_name: "room/light",
            device_name: "room/light",
            device_availability: false,
        }
    }

    fn drain(out: &Publisher) -> Vec<(String, Value)> {
        let mut queue = out.0.pending.lock().unwrap();
        let mut messages = Vec::new();
        while let Some(item) = queue.take() {
            messages.push((item.device, serde_json::from_slice(&item.payload).unwrap()));
            messages.extend(
                item.feedback
                    .into_iter()
                    .map(|(route, body)| (route, serde_json::from_slice(&body).unwrap())),
            );
        }
        messages
    }

    #[test]
    fn repeated_temperature_and_restored_snapshot_keep_their_radio_times() -> anyhow::Result<()> {
        let (out, _worker, _) = channel(MqttOptions::new("clock", "localhost", 1883), "test", 32)?;
        let ep = endpoint();
        let sensor = Sensor::Temperature;
        let route = topic(ep.topic_name, "temperature");
        out.register_feedback(&ep, Feature::Sensor(sensor))?;
        for millis in [1000, 2000] {
            sensor.report_temperature(&out, ep.topic_name, (ep.ieee, ep.endpoint),
                json!(15.1), millis, "attributeReport")?;
            let updates = drain(&out);
            let body = &updates.iter().find(|(r, _)| r == &route).unwrap().1;
            assert_eq!(body["temperature"], 15.1);
            assert_eq!(body["temperature_observation"]["observed_at_ms"], millis);
        }
        let checkpoint = serde_json::to_vec(&out.0.pending.lock().unwrap().snapshots[ep.topic_name])?;
        let (restored, _worker, _) = channel(MqttOptions::new("restart", "localhost", 1883), "test", 32)?;
        restored.0.pending.lock().unwrap().restore_snapshot(ep.topic_name, &checkpoint)?;
        restored.register_feedback(&ep, Feature::Sensor(sensor))?;
        assert_eq!(drain(&restored)[0].1["temperature_observation"]["observed_at_ms"], 2000);
        restored.state(ep.topic_name, "temperature", json!(14.8))?;
        assert!(drain(&restored).iter().all(|(_, body)| body.get("temperature_observation").is_none()));
        Ok(())
    }

    #[test]
    fn ota_before_and_after_confirmed_power_only_updates_the_firmware_entity() -> anyhow::Result<()>
    {
        let (out, _worker, _) = channel(MqttOptions::new("fixture", "localhost", 1883), "zb", 2)?;
        let ep = endpoint();
        let light = Feature::Light {
            effects: &[],
            xy: false,
            hs: false,
            brightness: true,
            color_temperature: false,
            mired_range: None,
        };
        out.register_feedback(&ep, light)?;
        out.register_feedback(&ep, Feature::FirmwareUpdate)?;
        out.set_routes(std::collections::BTreeMap::from([(
            ep.topic_name.into(),
            (42, 1),
        )]))?;
        out.update_state(ep.topic_name, 10, 10, UpdatePhase::Idle, None)?;
        let firmware =
            json!({"update":{"installed_version":10, "latest_version":10, "state":"idle"}});
        let first = drain(&out);
        assert_eq!(
            first,
            [
                (ep.topic_name.into(), firmware.clone()),
                (topic(ep.topic_name, "update"), firmware.clone())
            ]
        );
        out.state(ep.topic_name, "brightness", json!(112))?;
        assert!(!out.has_confirmed_light_power(ep.topic_name));
        assert_eq!(drain(&out).len(), 1);
        out.state(ep.topic_name, "state", json!("ON"))?;
        assert!(out.has_confirmed_light_power(ep.topic_name));
        let power = drain(&out);
        assert_eq!(
            power[1],
            (
                topic(ep.topic_name, LIGHT),
                json!({"brightness":112, "state":"ON"})
            )
        );
        out.update_state(ep.topic_name, 10, 10, UpdatePhase::Idle, None)?;
        let last = drain(&out);
        assert_eq!(last[0].1["state"], "ON");
        assert_eq!(last[1], (topic(ep.topic_name, "update"), firmware));
        assert_eq!(last.len(), 2);

        // A subsequent birth replays each existing feed; retire both the old
        // name and its outstanding publications when an endpoint changes route.
        out.0.pending.lock().unwrap().queue_birth_snapshot();
        let replay = drain(&out);
        assert!(
            replay
                .iter()
                .any(|(name, v)| name == &topic(ep.topic_name, LIGHT) && v["state"] == "ON")
        );
        out.0.pending.lock().unwrap().queue_birth_snapshot();
        out.membership_changed(ep.ieee, true)?;
        assert!(!out.has_confirmed_light_power(ep.topic_name));
        assert!(drain(&out).iter().all(|(name, _)| name != &topic(ep.topic_name, LIGHT)));
        out.0.pending.lock().unwrap().queue_birth_snapshot();
        out.set_routes(std::collections::BTreeMap::from([(
            "renamed".into(),
            (42, 1),
        )]))?;
        assert!(drain(&out).is_empty());
        assert!(!out.has_confirmed_light_power(ep.topic_name));
        Ok(())
    }

    #[test]
    fn sensor_observations_keep_their_fifo_slot_without_republishing_other_entities()
    -> anyhow::Result<()> {
        let (out, _worker, _) = channel(MqttOptions::new("fixture", "localhost", 1883), "zb", 1)?;
        let ep = endpoint();
        let temperature = Sensor::TemperatureWithForceUpdate(true);
        out.register_feedback(&ep, Feature::Sensor(temperature))?;
        out.register_feedback(&ep, Feature::Sensor(Sensor::Humidity))?;
        temperature.report(&out, ep.topic_name, json!(20))?;
        assert_eq!(
            drain(&out),
            [
                (ep.topic_name.to_owned(), json!({"temperature":20})),
                (
                    topic(ep.topic_name, "temperature"),
                    json!({"temperature":20})
                ),
            ]
        );
        out.state(ep.topic_name, "humidity", json!(65))?;
        let next = drain(&out);
        assert_eq!(
            next[1],
            (topic(ep.topic_name, "humidity"), json!({"humidity":65}))
        );
        assert_eq!(next.len(), 2);
        Ok(())
    }
}
