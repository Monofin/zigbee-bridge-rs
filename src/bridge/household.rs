//! Binding for reviewed household functions. All stateful meter conversions
//! belong to the active device membership, and run on the coordinator owner.
use super::{Planned, Read};
use crate::mqtt::{
    Operation,
    discovery::{Feature, Sensor},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value as Json;
use std::sync::{Arc, Mutex};
use zigbee_devices::{
    Endpoint,
    input::{Input, Inputs},
    metering::Unit,
    outlet::{Electricity, MeterKind, Outlet, Outlets, Reading},
};
use zigbee_zcl::{
    Direction, Frame,
    attributes::{Status, WriteStatus},
    foundation::{GeneralCommand, Payload},
    value::Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Query {
    State,
    Input(zigbee_devices::input::Reading),
    Meter(Reading),
    Setting(&'static str),
    SettingWrite(&'static str, zigbee_devices::settings::Written),
}

#[derive(Clone, Default)]
pub(super) struct Household {
    outlet: Option<Outlet>,
    settings: Option<zigbee_devices::settings::Settings>,
    pub(super) input: Option<Arc<Mutex<Input>>>,
    standard_readings: Vec<Reading>,
    electricity: Option<Arc<Mutex<Electricity>>>,
    protection: Option<Arc<Mutex<serde_json::Map<String,Json>>>>,
}

impl Household {
    pub(super) fn resolve(
        profiles: &Outlets,
        inputs: &Inputs,
        identity: Option<(&str, &str)>,
        ep: &Endpoint<'_>,
        ty: u16,
    ) -> Result<Self> {
        let Some((maker, model)) = identity else {
            return Ok(Self { outlet: Outlet::standard(ep, ty), ..Self::default() });
        };
        let outlet = profiles.resolve(maker, model, ep, ty).or_else(|| Outlet::standard(ep, ty));
        let electricity = outlet
            .and_then(|o| o.meter())
            .map(|mode| Electricity::new(mode, ep, maker, model))
            .transpose()?
            .map(|m| Arc::new(Mutex::new(m)));
        let input = inputs
            .resolve(maker, model, ep, ty)
            .map(|i| Arc::new(Mutex::new(i)));
        let settings = zigbee_devices::settings::Settings::resolve(maker, model, ep, ty)?;
        let protection = settings.as_ref().and_then(|s| s.field("overload_protection")).map(|_| Default::default());
        Ok(Self {
            protection,
            standard_readings: Vec::new(),
            outlet,
            electricity,
            input,
            settings,
        })
    }
    pub(super) fn add_standard_meter<'a>(&mut self, ep: &Endpoint<'_>, values: impl Iterator<Item=(u16, u16, Value<'a>)> + Clone, fresh: bool) {
        if self.electricity.is_none() && let Some((mut meter, readings)) = Electricity::standard(ep, values) {
            // Historical observations establish the entity identity. Formatting
            // may have changed while this coordinator was offline.
            if !fresh { meter.reset(); }
            self.electricity = Some(Arc::new(Mutex::new(meter)));
            self.standard_readings = readings;
        }
    }

    fn meter_kind(&self) -> Option<MeterKind> {
        self.outlet.and_then(|outlet| outlet.meter())
            .or_else(|| (!self.standard_readings.is_empty()).then_some(MeterKind::Standard))
    }

    fn readings(&self) -> &[Reading] {
        if self.outlet.is_some_and(|outlet| outlet.meter().is_some()) {
            &Reading::ALL
        } else {
            &self.standard_readings
        }
    }

    pub(super) fn owns_primary_function(&self) -> bool {
        self.outlet.is_some() || self.input.is_some()
    }
    pub(super) fn has_relay(&self) -> bool {
        self.outlet.is_some()
    }
    pub(super) fn active(&self) -> bool {
        self.owns_primary_function() || self.settings.is_some() || self.electricity.is_some()
    }
    pub(super) fn features(&self) -> Vec<Feature> {
        let mut result = Vec::new();
        if self.outlet.is_some() {
            // These profiles expose their primary On/Off server as a Z2M switch,
            // including SWV. Preserve the existing HA entity and automations.
            result.push(Feature::Switch);
        }
        result.extend(self.readings().iter().map(|r| Feature::Sensor(match r {
            Reading::Power | Reading::Demand => Sensor::Power,
            Reading::Current => Sensor::Current,
            Reading::Voltage => Sensor::MainsVoltage,
            Reading::Energy => Sensor::Energy,
        })));
        if let Some(settings) = &self.settings {
            for (key, f) in settings.fields() {
                if f.is_composite() {
                    result.push(Feature::CompositeSensor(key));
                } else if let Some((off, on)) = f.binary_labels() {
                    result.push(Feature::StringBinary {
                        property: key,
                        off,
                        on,
                        writable: f.writable(),
                    });
                } else if !f.writable() {
                    result.push(if f.choices.is_empty() {
                        Feature::Numeric {
                            property: key,
                            device_class: f.device_class.as_deref(),
                            state_class: f.state_class.as_deref(),
                            unit: f.unit.as_deref(),
                        }
                    } else {
                        Feature::TextSensor(key)
                    });
                } else if f.boolean {
                    result.push(Feature::BooleanSetting(key));
                } else if !f.choices.is_empty() {
                    result.push(Feature::Select {
                        property: key,
                        options: &f.choices,
                    });
                } else if let (Some(min), Some(max), Some(step)) = (f.min, f.max, f.step) {
                    result.push(Feature::Number {
                        property: key,
                        min,
                        max,
                        step,
                        unit: f.unit.as_deref(),
                    });
                }
            }
        }
        if let Some(input) = &self.input {
            let input = input.lock().unwrap_or_else(|e| e.into_inner());
            if !input.actions().is_empty() {
                result.push(Feature::Actions(input.actions()));
            }
            for &property in input.other_properties() {
                match property {
                    "device_temperature" => result.push(Feature::Numeric {
                        property, unit: Some("°C"), device_class: Some("temperature"),
                        state_class: Some("measurement"),
                    }),
                    "battery" => result.push(Feature::Sensor(Sensor::Battery)),
                    "voltage" => result.push(Feature::Sensor(Sensor::Voltage)),
                    "illuminance" => result.push(Feature::Sensor(Sensor::Illuminance)),
                    "illuminance_above_threshold" => result.push(Feature::Binary(property, "light")),
                    "requested_brightness_level" | "requested_brightness_percent" => result.push(Feature::Numeric {
                        property, unit: (property == "requested_brightness_percent").then_some("%"),
                        device_class: None, state_class: None,
                    }),
                    _ => {}
                }
            }
            result.extend(
                input
                    .binary()
                    .iter()
                    .map(|&(p, class)| Feature::Binary(p, class)),
            );
        }
        result
    }
    pub(super) fn input_property(&self, property: &str) -> bool {
        self.input.as_ref().is_some_and(|input| {
            input.lock().unwrap_or_else(|e| e.into_inner()).publishes(property)
        })
    }

    pub(super) fn input_reading(&self, key: &str) -> Option<zigbee_devices::input::Reading> {
        self.input.as_ref()?.lock().unwrap_or_else(|e| e.into_inner()).reading(key)
    }

    pub(super) fn plan(
        &self,
        ep: u8,
        op: Operation,
        key: &str,
        value: &Json,
        seq: u8,
    ) -> Result<Option<Planned>> {
        if let Some((key, f)) = self.settings.as_ref().and_then(|s| s.field(key)) {
            let set = (op == Operation::Set).then_some(value);
            let (request, written) = f.prepare(ep, seq, set)?;
            let query = match written {
                Some(n) => Query::SettingWrite(key, n),
                None => Query::Setting(key),
            };
            return Ok(Some((request, Some(Read::Household(query)))));
        }
        if let Some(reading) = self.input_reading(key) {
            ensure!(op == Operation::Get, "input measurements are read-only");
            return Ok(Some((reading.read(ep, seq)?, Some(Read::Household(Query::Input(reading))))));
        }
        if let Some(mode) = self.meter_kind()
            && let Some(r) = self.readings().iter().copied().find(|r| r.property() == key)
        {
            ensure!(
                op == Operation::Get,
                "electrical measurements are read-only"
            );
            return Ok(Some((
                mode.read(ep, r, seq)?,
                Some(Read::Household(Query::Meter(r))),
            )));
        }
        let Some(outlet) = self.outlet else {
            return Ok(None);
        };
        if key == "inching_control_set" {
            ensure!(op == Operation::Set, "inching is set-only");
            return Ok(Some((outlet.set_inching(value, seq)?, None)));
        }
        if key == "state" {
            return Ok(Some(match op {
                Operation::Set => {
                    let state = value.as_str().context("relay state must be a string")?;
                    let req = if state.eq_ignore_ascii_case("ON") {
                        outlet.set_state(true, seq)
                    } else if state.eq_ignore_ascii_case("OFF") {
                        outlet.set_state(false, seq)
                    } else if state.eq_ignore_ascii_case("TOGGLE") {
                        outlet.toggle(seq)
                    } else {
                        anyhow::bail!("unsupported relay state")
                    };
                    (req, None)
                }
                Operation::Get => (outlet.get_state(seq)?, Some(Read::Household(Query::State))),
            }));
        }
        Ok(None)
    }

    /// A read can fail at the device, but its response must still identify
    /// the requested attribute. An unrelated attribute with a reused ZCL
    /// sequence cannot retire the outstanding request.
    pub(super) fn request_manufacturer(&self, query: Query) -> Option<u16> {
        match query {
            Query::Setting(name) | Query::SettingWrite(name, _) =>
                self.settings.as_ref()?.field(name)?.1.manufacturer_code,
            Query::State | Query::Input(_) | Query::Meter(_) => None,
        }
    }

    pub(super) fn completes(&self, query: Query, cluster: u16, frame: Frame<'_>) -> bool {
        if let Query::SettingWrite(name, _) = query {
            let Some((_, f)) = self.settings.as_ref().and_then(|s| s.field(name)) else {
                return false;
            };
            if frame.header.direction != Direction::ServerToClient
                || frame.header.manufacturer != f.manufacturer_code
                || cluster != f.cluster
                || frame.header.general_command() != Some(GeneralCommand::WriteAttributesResponse)
            {
                return false;
            }
            return match Payload::decode(frame) {
                Ok(Payload::WriteAttributesResponse(WriteStatus::Success)) => true,
                Ok(Payload::WriteAttributesResponse(WriteStatus::Failures(records))) => {
                    records.iter().any(|r| r.key == f.attribute)
                }
                _ => false,
            };
        }
        if frame.header.direction != Direction::ServerToClient
            || frame.header.general_command() != Some(GeneralCommand::ReadAttributesResponse)
        {
            return false;
        }
        let Ok(Payload::ReadAttributesResponse(records)) = Payload::decode(frame) else {
            return false;
        };
        records.iter().any(|r| match query {
            Query::SettingWrite(..) => false,
            Query::Input(reading) => frame.header.manufacturer.is_none() && (cluster, r.id) == (reading.cluster(), 0),
            Query::State => frame.header.manufacturer.is_none() && (cluster, r.id) == (6, 0),
            Query::Setting(name) => self
                .settings.as_ref()
                .and_then(|s| s.field(name))
                .is_some_and(|(_, f)| (f.cluster, f.attribute, f.manufacturer_code) == (cluster, r.id, frame.header.manufacturer)),
            Query::Meter(wanted) => frame.header.manufacturer.is_none() && self.meter_kind()
                .is_some_and(|m| m.matches(wanted, cluster, r.id)),
        })
    }

    /// Unrelated clusters continue to the other endpoint functions. A reply
    /// with no matching pending read cannot update state or scaling.
    #[cfg(test)]
    pub(super) fn frame(&self, cluster: u16, frame: Frame<'_>, read: Option<Query>)
        -> Result<Option<Vec<(&'static str, Json)>>> {
        self.frame_at(cluster, frame, read, tokio::time::Instant::now().into_std())
    }

    pub(super) fn frame_at(
        &self,
        cluster: u16,
        frame: Frame<'_>,
        read: Option<Query>,
        received: std::time::Instant,
    ) -> Result<Option<Vec<(&'static str, Json)>>> {
        let mut setting_output = vec![];
        let mut handled = false;
        if let Some(input) = &self.input {
            let mut input = input.lock().map_err(|_| anyhow::anyhow!("input state poisoned"))?;
            if input.handles(cluster) {
                if let Some(Query::Input(reading)) = read
                    && reading.cluster() == cluster && frame.header.direction == Direction::ServerToClient
                    && frame.header.manufacturer.is_none()
                    && frame.header.general_command() == Some(GeneralCommand::ReadAttributesResponse)
                    && let Payload::ReadAttributesResponse(records) = Payload::decode(frame)?
                {
                    let mut matching = records.into_iter().filter(|record| record.id == 0);
                    if let Some(record) = matching.next() && matching.next().is_none()
                        && let Status::Success(value) = record.result {
                        setting_output.extend(reading.observe(value));
                    }
                }
                setting_output.extend(input.handle(cluster, frame, received)?);
                handled = true;
            }
        }
        if let Some(settings) = &self.settings
            && frame.header.direction == Direction::ServerToClient
            && settings.fields().any(|(_, f)| (f.cluster, f.manufacturer_code) == (cluster, frame.header.manufacturer))
        {
            handled = true;
            let mut accept = |attr: u16, v: Value<'_>, read_only: bool| {
                if let Some((name, f)) = settings
                    .fields()
                    .find(|(_, f)| (f.cluster, f.attribute, f.manufacturer_code) == (cluster, attr, frame.header.manufacturer))
                    && (!read_only || read == Some(Query::Setting(name)))
                    && let Some(value) = f.decode(v)
                {
                    if name == "overload_protection" && let (Some(cache), Json::Object(patch)) = (&self.protection, &value) {
                        // Reports can cover only current, voltage or power. Retain
                        // previously confirmed categories until membership changes.
                        let mut confirmed = cache.lock().unwrap_or_else(|e| e.into_inner());
                        confirmed.extend(patch.clone());
                        setting_output.push((name, Json::Object(confirmed.clone())));
                    } else {
                        setting_output.push((name, value));
                    }
                }
            };
            match frame.header.general_command() {
                Some(GeneralCommand::WriteAttributesResponse) => {
                    if let Some(Query::SettingWrite(name, written)) = read
                        && let Some((key, f)) = settings.field(name)
                        && (f.cluster, f.manufacturer_code) == (cluster, frame.header.manufacturer)
                        && matches!(
                            Payload::decode(frame)?,
                            Payload::WriteAttributesResponse(WriteStatus::Success)
                        )
                        && let Some(v) = f.confirmed_write(written)
                    {
                        setting_output.push((key, v));
                    }
                }
                Some(GeneralCommand::ReportAttributes) => {
                    if let Payload::ReportAttributes(items) = Payload::decode(frame)? {
                        for r in items {
                            accept(r.id, r.value, false);
                        }
                    }
                }
                Some(GeneralCommand::ReadAttributesResponse) => {
                    if let Payload::ReadAttributesResponse(items) = Payload::decode(frame)? {
                        for r in items {
                            if let Status::Success(v) = r.result {
                                accept(r.id, v, true);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let mode = self.meter_kind();
        let relay_cluster = self.outlet.is_some() && cluster == 6;
        if !(relay_cluster || mode.is_some_and(|m| m.handles(cluster))) {
            return Ok((!setting_output.is_empty() || (self.outlet.is_none() && handled)).then_some(setting_output));
        }
        let mut output = setting_output;
        if frame.header.direction != Direction::ServerToClient {
            return Ok(Some(output));
        }
        let mf = frame.header.manufacturer;
        let attrs: Vec<(u16, Value<'_>)> = match frame.header.general_command() {
            Some(GeneralCommand::ReportAttributes) => {
                let Payload::ReportAttributes(records) = Payload::decode(frame)? else {
                    unreachable!()
                };
                if !self.standard_readings.is_empty() && mode.is_some_and(|m| m.handles(cluster)) {
                    let mut seen = std::collections::BTreeSet::new();
                    ensure!(records.iter().all(|r| seen.insert(r.id)), "duplicate meter report attribute");
                }
                records.into_iter().map(|r| (r.id, r.value)).collect()
            }
            Some(GeneralCommand::ReadAttributesResponse) if read.is_some() && mf.is_none() => {
                let Payload::ReadAttributesResponse(records) = Payload::decode(frame)? else {
                    unreachable!()
                };
                let mut seen = std::collections::BTreeSet::new();
                ensure!(
                    records.iter().all(|r| seen.insert(r.id)),
                    "duplicate relay read attribute"
                );
                if !self.standard_readings.is_empty() && let Some(Query::Meter(r)) = read
                    && !records.iter().any(|v| MeterKind::Standard.requested(r, cluster, v.id))
                {
                    return Ok(Some(output));
                }
                records
                    .into_iter()
                    .filter_map(|r| {
                        let relevant = match read? {
                            Query::State => frame.header.manufacturer.is_none() && (cluster, r.id) == (6, 0),
                            Query::Input(_) | Query::Setting(_) | Query::SettingWrite(..) => false,
                            Query::Meter(q) => mode?.requested(q, cluster, r.id),
                        };
                        match r.result {
                            Status::Success(v) if relevant => Some((r.id, v)),
                            _ => None,
                        }
                    })
                    .collect()
            }
            _ => return Ok(Some(output)),
        };
        let mut meter = self
            .electricity
            .as_ref()
            .map(|m| {
                m.lock()
                    .map_err(|_| anyhow::anyhow!("meter state poisoned"))
            })
            .transpose()?;
        if let Some(outlet) = self.outlet.filter(|_| cluster == 6) {
            for (id, v) in attrs {
                if let Some(on) = outlet.state_attribute(cluster, id, mf, v) {
                    output.push(("state", if on { "ON" } else { "OFF" }.into()));
                    if let Some(meter) = meter.as_mut() {
                        meter.relay(on);
                        if !on && mode == Some(MeterKind::Sonoff) {
                            output.extend([("current", 0.into()), ("power", 0.into())]);
                        }
                    }
                }
            }
        } else if let Some(meter) = meter.as_mut() {
            // A device can put the raw measurement before the new divisor in
            // one frame. Apply all formatting observations before conversion.
            let kind = meter.kind;
            // A correlated GET requests the complete ratio. A missing/failed field
            // must not silently inherit an earlier ratio for this quantity.
            if !self.standard_readings.is_empty()
                && frame.header.general_command() == Some(GeneralCommand::ReadAttributesResponse)
                && let Some(Query::Meter(r)) = read
            {
                meter.forget_formatting(r);
            }
            for &(id, v) in attrs
                .iter()
                .filter(|(id, _)| kind.scale_attribute(cluster, *id))
            {
                meter.observe(cluster, id, mf, v);
            }
            for &(id, v) in attrs
                .iter()
                .filter(|(id, _)| !kind.scale_attribute(cluster, *id))
            {
                if self.readings().iter().any(|r| kind.matches(*r, cluster, id))
                    && let Some(m) = meter.observe(cluster, id, mf, v) {
                    let key = match m.unit {
                        Unit::Watts => "power",
                        Unit::KilowattHours => "energy",
                        Unit::Amperes => "current",
                        Unit::Volts => "voltage",
                    };
                    let number = m.numerator as f64 / f64::from(m.denominator.get());
                    output.push((key, number.into()));
                }
            }
        }
        Ok(Some(output))
    }
}

// One replaceable deadline per active membership. A house-wide light burst
// never scans the inventory to find a sensor clear.
#[derive(Clone, Default)]
struct Scheduled {
    by_time: std::collections::BTreeSet<(tokio::time::Instant, super::Identity)>,
    by_peer: std::collections::BTreeMap<super::Identity, tokio::time::Instant>,
}
impl Scheduled {
    fn set(&mut self, id: super::Identity, end: Option<tokio::time::Instant>) {
        if self.by_peer.get(&id).copied() == end {
            return;
        }
        if let Some(old) = self.by_peer.remove(&id) {
            self.by_time.remove(&(old, id));
        }
        if let Some(end) = end {
            self.by_peer.insert(id, end);
            self.by_time.insert((end, id));
        }
    }
}
#[derive(Default)]
pub(super) struct Deadlines(Mutex<Scheduled>);
impl Clone for Deadlines {
    fn clone(&self) -> Self {
        Self(Mutex::new(
            self.0.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        ))
    }
}
impl super::Bindings {
    pub(super) fn update_deadline(&self, peer: super::Identity, house: &Household) {
        if let Some(input) = &house.input {
            let deadline = input
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .deadline()
                .map(tokio::time::Instant::from_std);
            self.deadlines
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set(peer, deadline);
        }
    }
    pub(super) fn household_due(
        &self,
        out: &crate::mqtt::Publisher,
        now: tokio::time::Instant,
    ) -> Result<Option<tokio::time::Instant>> {
        let mut scheduled = self
            .deadlines
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("timer index poisoned"))?;
        while let Some(&(at, peer)) = scheduled.by_time.first() {
            if at > now {
                return Ok(Some(at));
            }
            scheduled.set(peer, None);
            if let Some(b) = self.devices.get(&peer)
                && let Some(input) = &b.household.input
            {
                let mut input = input
                    .lock()
                    .map_err(|_| anyhow::anyhow!("input state poisoned"))?;
                if let Some((name, value)) = input.expire(now.into_std()) {
                    match out.state(&b.device_name, name, value) {
                        Ok(()) => {}
                        Err(crate::mqtt::SubmitError::Full) => input
                            .retry_clear((now + std::time::Duration::from_millis(50)).into_std()),
                        Err(e) => return Err(e.into()),
                    }
                }
                scheduled.set(peer, input.deadline().map(tokio::time::Instant::from_std));
            }
        }
        Ok(None)
    }
    pub(super) fn household_leave(&self, ieee: u64) {
        for (&peer, b) in self.devices.range((ieee, 0)..=(ieee, u8::MAX)) {
            self.deadlines
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set(peer, None);
            if let Some(input) = &b.household.input {
                input.lock().unwrap_or_else(|e| e.into_inner()).reset();
            }
            if let Some(protection) = &b.household.protection {
                protection.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
            if let Some(meter) = &b.household.electricity {
                meter.lock().unwrap_or_else(|e| e.into_inner()).reset();
            }
        }
    }
}

#[cfg(test)]
impl Household {
    pub(super) fn coverage(&self, props: &mut std::collections::BTreeSet<String>) {
        if let Some(settings) = &self.settings {
            props.extend(settings.fields().map(|(k, _)| k.to_string()));
        }
        if self.outlet.is_some() { props.insert("state".into()); }
        props.extend(self.readings().iter().map(|r| r.property().to_string()));
        if let Some(i) = &self.input {
            let i = i.lock().unwrap();
            if !i.actions().is_empty() {
                props.insert("action".into());
            }
            props.extend(i.binary().iter().map(|&(key, _)| key.to_string()));
            props.extend(i.other_properties().iter().map(|key| key.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zigbee_devices::input::Inputs;
    #[test]
    fn device_target_selects_only_the_sonoff_relay() -> Result<()> {
        use crate::mqtt::discovery;
        let ep = Endpoint { id: 1, profile: 260, servers: &[0, 6, 0xfc11], clients: &[] };
        let relay = Household::resolve(&Outlets::builtin()?, &Inputs::builtin()?, Some(("SONOFF", "ZBMINIR2")), &ep, 2)?;
        let endpoint = discovery::Endpoint { ieee: 1, endpoint: 1, device_availability: false,
            device_name: "fixture", topic_name: "fixture" };
        let mut switches = std::collections::BTreeMap::new();
        for feature in relay.features() {
            for (topic, payload) in discovery::configs("base", "ha", &endpoint, feature, false)? {
                if topic.starts_with("ha/switch/") {
                    let config: Json = serde_json::from_slice(&payload)?;
                    switches.insert(topic.split('/').nth(3).unwrap().to_owned(), config);
                }
            }
        }
        assert_eq!(switches.len(), 5);
        assert!(switches["switch"].get("entity_category").is_none());
        for setting in ["detach_relay_mode", "turbo_mode", "network_indicator", "delayed_power_on_state"] {
            assert_eq!(switches[&format!("switch_{setting}")]["entity_category"], "config", "{setting}");
        }
        Ok(())
    }

    #[test]
    fn input_read_uses_device_topic_and_only_the_correlated_server_response() -> Result<()> {
        use crate::{bridge::Bindings, interview_log::{AttributeResult, Descriptor, Entry,
            IdentitySource, InterviewReport, Observation}, mqtt::Command};
        for (model, maker, property, cluster, value) in [
            ("VALLHORN Wireless Motion Sensor", "IKEA of Sweden", "occupancy", 0x0406, serde_json::json!(true)),
            ("SML004", "Signify Netherlands B.V.", "illuminance", 0x0400, serde_json::json!(10.0)),
        ] {
            let mut observations = vec![
                Observation::Target {ieee: "0000000000000001".into(), short: 5, identity_source: IdentitySource::Z2mDatabase},
                Observation::ActiveEndpoints {sequence: 1, endpoints: vec![1,2,3]},
            ];
            for id in [1,2,3] {
                let mut servers = if id == 1 {vec![0,1,3,0xfc81]} else {vec![0,0x0406]};
                if id == 2 && model == "SML004" {servers.push(0x0400);}
                observations.push(Observation::SimpleDescriptor {sequence:2, endpoint:id,
                    descriptor:Descriptor{profile:260,device:if id == 1 {2128} else {263},version:0,servers,clients:vec![]}});
            }
            for (attribute, text) in [(4,maker),(5,model)] {
                let mut bytes=vec![text.len() as u8]; bytes.extend(text.bytes());
                observations.push(Observation::Attribute {sequence:3,endpoint:1,cluster:0,attribute,
                    result:AttributeResult::Value{data_type:0x42,bytes}});
            }
            observations.push(Observation::Finished);
            let report=InterviewReport {finished:true,entries:observations.into_iter()
                .map(|observation| Entry{unix_ms:0,observation}).collect()};
            let mut bindings=Bindings::default();
            bindings.add(&report,&[(1,"sensor".to_owned())].into_iter().collect())?;
            let get=|device:&str, key:&str| bindings.plan(&Command {device:device.into(),operation:Operation::Get,
                properties:[(key.to_owned(),Json::Null)].into_iter().collect()},23);
            let (id,steps)=get("sensor",property)?;
            assert_eq!(id,(1,2));
            let (request,Some(Read::Household(query)))=&steps[0] else {panic!("uncorrelated query")};
            assert_eq!((request.endpoint,request.cluster,request.frame.as_slice()), (2,cluster,&[0x10,23,0,0,0][..]));
            let body=if cluster==0x0406 {vec![0x18,23,1,0,0,0,0x18,1]} else {vec![0x18,23,1,0,0,0,0x21,0x11,0x27]};
            let frame=Frame::decode(&body)?;
            let house=&bindings.devices[&id].household;
            assert!(house.completes(*query,cluster,frame));
            assert!(house.frame(cluster,frame,None)?.unwrap().is_empty());
            assert_eq!(house.frame(cluster,frame,Some(*query))?,Some(vec![(property,value)]));
            assert!(!house.completes(*query,cluster,Frame::decode(&[0x18,23,1,9,0,0x86])?));
            if model.starts_with("VALLHORN") {
                assert!(get("sensor","illuminance").is_err());
                assert!(get("sensor/ep3","illuminance").is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn vendor_setting_uses_its_header_and_shares_reports_with_motion() -> Result<()> {
        let ep = Endpoint {id:2, profile:260, servers:&[0,1,3,1024,1026,1030], clients:&[]};
        let motion = Household::resolve(&Outlets::builtin()?, &Inputs::builtin()?,
            Some(("Signify Netherlands B.V.", "SML004")), &ep, 263)?;
        let (wire, Some(Read::Household(write))) = motion.plan(2, Operation::Set, "motion_sensitivity", &"high".into(), 5)?.unwrap()
            else { panic!("untracked write"); };
        let frame = Frame::decode(&wire.frame)?;
        assert_eq!(frame.header.manufacturer, Some(0x100b));
        let wrong = Frame::decode(&[0x18,5,4,0])?;
        let right = Frame::decode(&[0x1c,0x0b,0x10,5,4,0])?;
        assert!(!motion.completes(write, 0x0406, wrong));
        assert!(motion.completes(write, 0x0406, right));
        assert_eq!(motion.frame(0x0406, right, Some(write))?.unwrap(), vec![("motion_sensitivity", "high".into())]);
        // Both handlers own attributes in the same standard cluster.
        let report = Frame::decode(&[0x18,7,10,0,0,0x18,1,0x10,0,0x21,30,0])?;
        assert_eq!(motion.frame(0x0406, report, None)?.unwrap(),
            vec![("occupancy", true.into()), ("occupancy_timeout", 30.0.into())]);
        Ok(())
    }

    #[test]
    fn swv_controls_use_confirmed_strings_and_keep_the_original_discovery_entities() -> Result<()> {
        use crate::mqtt::discovery::{self, Endpoint as DiscoveryEndpoint};
        let endpoint = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 1028, 0xfc11],
            clients: &[],
        };
        let h = Household::resolve(
            &Outlets::builtin()?,
            &Inputs::builtin()?,
            Some(("SONOFF", "SWV")),
            &endpoint,
            2,
        )?;
        let shortage = "auto_close_when_water_shortage";
        let working = "valve_work_state";
        let plan = |op, key, value| h.plan(1, op, key, &value, 9);
        let (get, _) = plan(Operation::Get, shortage, Json::Null)?.unwrap();
        assert_eq!(
            (get.cluster, get.frame.as_slice()),
            (0xfc11, &[0x10, 9, 0, 0x11, 0x50][..])
        );
        for (label, wire) in [("DISABLE", 0), ("ENABLE", 30)] {
            let (set, Some(Read::Household(query))) =
                plan(Operation::Set, shortage, label.into())?.unwrap()
            else {
                panic!("write must await a response")
            };
            assert_eq!(set.frame, [0x10, 9, 2, 0x11, 0x50, 0x21, wire, 0]);
            let ok = Frame::decode(&[0x18, 9, 4, 0])?;
            assert!(h.completes(query, 0xfc11, ok));
            assert_eq!(
                h.frame(0xfc11, ok, Some(query))?,
                Some(vec![(shortage, label.into())])
            );
            let denied = Frame::decode(&[0x18, 9, 4, 0x86, 0x11, 0x50])?;
            assert!(h.frame(0xfc11, denied, Some(query))?.is_none());
        }
        assert!(plan(Operation::Set, shortage, true.into()).is_err());
        assert!(plan(Operation::Set, working, "working".into()).is_err());
        assert!(plan(Operation::Get, working, Json::Null).is_err());
        let report = Frame::decode(&[0x18, 11, 10, 0x10, 0x50, 0x10, 1, 0x11, 0x50, 0x21, 0, 0])?;
        assert_eq!(
            h.frame(0xfc11, report, None)?,
            Some(vec![
                (working, "working".into()),
                (shortage, "DISABLE".into())
            ])
        );
        let disc = DiscoveryEndpoint {
            ieee: 1,
            endpoint: 1,
            device_name: "Fixture",
            topic_name: "Fixture",
            device_availability: false,
        };
        let mut configs = std::collections::BTreeMap::new();
        for feature in h.features() {
            for (topic, payload) in discovery::configs("test", "ha", &disc, feature, false)? {
                configs.insert(topic, serde_json::from_slice::<Json>(&payload)?);
            }
        }
        let config = |route| &configs[&format!("ha/{route}/config")];
        assert_eq!(config("switch/0x0000000000000001/switch")["state_on"], "ON");
        for (field, device_class, state_class) in [
            ("flow", Some("volume_flow_rate"), Some("measurement")),
            ("daily_irrigation_volume", Some("water"), Some("total_increasing")),
            ("real_time_irrigation_volume", Some("volume"), None),
            ("real_time_irrigation_duration", Some("duration"), None),
            ("irrigation_start_time", None, None),
            ("irrigation_end_time", None, None),
        ] {
            let sensor = &configs[&format!("ha/sensor/0x0000000000000001/{field}/config")];
            assert_eq!(sensor.get("device_class").and_then(Json::as_str), device_class, "{field}");
            assert_eq!(sensor.get("state_class").and_then(Json::as_str), state_class, "{field}");
        }
        let switch = config("switch/0x0000000000000001/switch_auto_close_when_water_shortage");
        assert_eq!(
            switch["command_topic"],
            "test/Fixture/set/auto_close_when_water_shortage"
        );
        assert_eq!(switch["payload_on"], "ENABLE");
        assert_eq!(switch["payload_off"], "DISABLE");
        let binary = config("binary_sensor/0x0000000000000001/valve_work_state");
        assert_eq!(binary["payload_on"], "working");
        assert_eq!(binary["payload_off"], "idle");
        assert!(binary.get("command_topic").is_none());
        Ok(())
    }
    #[test]
    fn swv_irrigation_programs_keep_progress_read_only_and_correlate_device_replies() -> Result<()>
    {
        use serde_json::json;
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 1028, 0xfc11],
            clients: &[],
        };
        let h = Household::resolve(
            &Outlets::builtin()?,
            &Inputs::builtin()?,
            Some(("SONOFF", "SWV")),
            &ep,
            2,
        )?;
        for (property, attr, amount, max) in [
            ("cyclic_timed_irrigation", 8, "irrigation_duration", 86_400),
            (
                "cyclic_quantitative_irrigation",
                9,
                "irrigation_capacity",
                6_500,
            ),
        ] {
            let program = json!({"total_number":3,(amount):239,"irrigation_interval":300});
            let Some((set, Some(Read::Household(write)))) =
                h.plan(1, Operation::Set, property, &program, 11)?
            else {
                panic!("untracked irrigation write")
            };
            assert_eq!(set.cluster, 0xfc11);
            assert_eq!(
                set.frame,
                [
                    0, 11, 2, attr, 0x50, 0x42, 10, 0, 3, 0, 0, 0, 239, 0, 0, 1, 44
                ]
            );
            let ok = Frame::decode(&[0x18, 11, 4, 0])?;
            assert!(h.completes(write, 0xfc11, ok));
            assert_eq!(
                h.frame(0xfc11, ok, Some(write))?,
                Some(vec![(property, program.clone())])
            );
            assert!(h.frame(0xfc11, ok, None)?.is_none());
            let Some((get, Some(Read::Household(query)))) =
                h.plan(1, Operation::Get, property, &Json::Null, 12)?
            else {
                panic!("untracked irrigation read")
            };
            assert_eq!(get.frame, [0, 12, 0, attr, 0x50]);
            let mut readback = program.clone();
            readback["current_count"] = json!(2);
            // A different attribute precedes the program, and a read response has an extra status byte.
            let bytes = [
                0x18, 12, 1, 0x10, 0x50, 0, 0x10, 1, attr, 0x50, 0, 0x42, 10, 2, 3, 0, 0, 0, 239,
                0, 0, 1, 44,
            ];
            let response = Frame::decode(&bytes)?;
            assert!(h.completes(query, 0xfc11, response));
            assert!(h.frame(0xfc11, response, None)?.is_none());
            assert_eq!(
                h.frame(0xfc11, response, Some(query))?,
                Some(vec![(property, readback.clone())])
            );
            let mut bytes = vec![0x18, 12, 10, attr, 0x50, 0x42];
            bytes.extend_from_slice(&[10, 2, 3, 0, 0, 0, 239, 0, 0, 1, 44]);
            assert_eq!(
                h.frame(0xfc11, Frame::decode(&bytes)?, None)?,
                Some(vec![(property, readback.clone())])
            );
            for invalid in [
                json!({}),
                readback,
                json!({"total_number":101,(amount):1,"irrigation_interval":300}),
                json!({"total_number":1,(amount):max+1,"irrigation_interval":300}),
                json!({"total_number":1,(amount):1,"irrigation_interval":86401}),
                json!({"total_number":1,(amount):-1,"irrigation_interval":0}),
            ] {
                assert!(h.plan(1, Operation::Set, property, &invalid, 13).is_err());
            }
            let mut malformed = vec![0x18, 12, 10, attr, 0x50, 0x42, 9];
            malformed.extend_from_slice(&[0; 9]);
            assert!(h.frame(0xfc11, Frame::decode(&malformed)?, None)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn startup_setting_composes_with_light_state_and_waits_for_write_confirmation() {
        use zigbee_devices::lighting::{Lighting, Setting};
        let ep = Endpoint {
            id: 11,
            profile: 260,
            servers: &[0, 6, 8],
            clients: &[],
        };
        let h = Household::resolve(
            &Outlets::builtin().unwrap(),
            &Inputs::builtin().unwrap(),
            Some(("Signify Netherlands B.V.", "LWE004")),
            &ep,
            257,
        )
        .unwrap();
        assert!(h.active());
        assert!(!h.owns_primary_function());
        let mixed = Frame::decode(&[0x18, 7, 10, 0, 0, 0x10, 1, 3, 0x40, 0x30, 255]).unwrap();
        assert_eq!(
            h.frame(6, mixed, None).unwrap().unwrap(),
            vec![("power_on_behavior", "previous".into())]
        );
        assert_eq!(
            Lighting::new(&ep, None).observations(6, mixed).unwrap(),
            vec![Setting::State(true)]
        );
        let (_, Some(Read::Household(query))) = h
            .plan(11, Operation::Set, "power_on_behavior", &"on".into(), 12)
            .unwrap()
            .unwrap()
        else {
            panic!("write is untracked")
        };
        let ok = Frame::decode(&[0x18, 12, 4, 0]).unwrap();
        assert!(h.completes(query, 6, ok));
        assert!(h.frame(6, ok, None).unwrap().unwrap().is_empty());
        assert_eq!(
            h.frame(6, ok, Some(query)).unwrap().unwrap(),
            vec![("power_on_behavior", "on".into())]
        );
        let denied = Frame::decode(&[0x18, 12, 4, 0x86, 3, 0x40]).unwrap();
        assert!(h.completes(query, 6, denied));
        assert!(h.frame(6, denied, Some(query)).unwrap().unwrap().is_empty());
    }
    #[test]
    fn s60_protection_readback_is_a_typed_device_response() -> Result<()> {
        let ep = Endpoint { id:1,profile:260,servers:&[0,6,1794,2820,0xfc11],clients:&[] };
        let household = Household::resolve(&Outlets::builtin()?, &Inputs::builtin()?,Some(("SONOFF","S60ZBTPG")),&ep,81)?;
        let key="overload_protection";
        let value=serde_json::json!({"max_current":14,"max_power":3250});
        let (set,Some(Read::Household(write)))=household.plan(1,Operation::Set,key,&value,11)?.unwrap() else {panic!("expected registered write")};
        assert_eq!(set.frame,[0,11,2,3,0x70,0x42,13,4,11,1,0,1,0xb0,0x36,0,0,0x50,0x97,0x31,0]);
        let ok=Frame::decode(&[0x18,11,4,0])?;
        assert!(household.completes(write,0xfc11,ok));
        assert_eq!(household.frame(0xfc11,ok,Some(write))?,Some(vec![]));
        let (get,Some(Read::Household(read)))=household.plan(1,Operation::Get,key,&Json::Null,12)?.unwrap() else {panic!("expected registered read")};
        assert_eq!(get.frame,[0,12,0,3,0x70]);
        let frame=Frame::decode(&[0x18,12,1,3,0x70,0,0x42,14,0,4,11,1,0,1,0xb0,0x36,0,0,0x50,0x97,0x31,0])?;
        assert!(household.completes(read,0xfc11,frame));
        assert!(household.frame(0xfc11,frame,None)?.unwrap().is_empty());
        let confirmed=household.frame(0xfc11,frame,Some(read))?.unwrap();
        assert_eq!(confirmed[0].0,key);
        assert_eq!(confirmed[0].1["max_power"],3250.);
        assert_eq!(confirmed[0].1["enable_min_power"],"DISABLE");
        let partial=Frame::decode(&[0x18,13,10,3,0x70,0x42,7,2,5,2,0x88,0x84,2,0])?;
        let merged=household.frame(0xfc11,partial,None)?.unwrap();
        assert_eq!(merged[0].1["max_power"],3250.);
        assert_eq!(merged[0].1["min_voltage"],165.);
        assert_eq!(merged[0].1["enable_min_voltage"],"ENABLE");
        Ok(())
    }

    #[test]
    fn metered_plug_routes_mains_and_requires_confirmed_relay_state() {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 1794, 2820, 0xfc11],
            clients: &[],
        };
        let h = Household::resolve(
            &Outlets::builtin().unwrap(),
            &Inputs::builtin().unwrap(),
            Some(("SONOFF", "S60ZBTPG")),
            &ep,
            81,
        )
        .unwrap();
        let power = Frame::decode(&[0x18, 1, 0x0a, 6, 0x70, 0x23, 0xd0, 7, 0, 0]).unwrap();
        assert!(h.frame(0xfc11, power, None).unwrap().unwrap().is_empty());
        let on = Frame::decode(&[0x18, 1, 0x0a, 0, 0, 0x10, 1]).unwrap();
        assert_eq!(
            h.frame(6, on, None).unwrap().unwrap(),
            vec![("state", "ON".into())]
        );
        assert_eq!(
            h.frame(0xfc11, power, None).unwrap().unwrap(),
            vec![("power", 2.0.into())]
        );
        let off = Frame::decode(&[0x18, 1, 0x0a, 0, 0, 0x10, 0]).unwrap();
        assert_eq!(h.frame(6, off, None).unwrap().unwrap().len(), 3);
        assert_eq!(
            h.frame(0xfc11, power, None).unwrap().unwrap(),
            vec![("power", 0.0.into())]
        );
        assert!(h.plan(1, Operation::Set, "voltage", &1.into(), 1).is_err());
        assert!(h.plan(1, Operation::Get, "energy", &"".into(), 1).is_err());
        assert!(
            h.plan(1, Operation::Get, "voltage", &"".into(), 1)
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn scale_in_same_frame_precedes_conversion_and_replies_need_correlation() {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 1794, 2820],
            clients: &[],
        };
        let h = Household::resolve(
            &Outlets::builtin().unwrap(),
            &Inputs::builtin().unwrap(),
            Some(("IKEA of Sweden", "INSPELNING Smart plug")),
            &ep,
            266,
        )
        .unwrap();
        let raw = Frame::decode(&[
            0x18, 8, 0x0a, 11, 5, 0x29, 42, 0, 4, 6, 0x21, 1, 0, 5, 6, 0x21, 2, 0,
        ])
        .unwrap();
        assert_eq!(
            h.frame(2820, raw, None).unwrap().unwrap(),
            vec![("power", 21.0.into())]
        );
        let reply = Frame::decode(&[0x18, 8, 1, 11, 5, 0, 0x29, 2, 0]).unwrap();
        assert!(h.frame(2820, reply, None).unwrap().unwrap().is_empty());
        assert!(!h.completes(Query::Meter(Reading::Voltage), 2820, reply));
        assert!(h.completes(Query::Meter(Reading::Power), 2820, reply));
        let denied = Frame::decode(&[0x18, 8, 1, 11, 5, 0x86]).unwrap();
        assert!(h.completes(Query::Meter(Reading::Power), 2820, denied));
        assert!(
            h.frame(2820, reply, Some(Query::Meter(Reading::Voltage)))
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            h.frame(2820, reply, Some(Query::Meter(Reading::Power)))
                .unwrap()
                .unwrap(),
            vec![("power", 1.0.into())]
        );
    }
    #[test]
    fn anonymous_meter_requires_complete_evidence_and_updates_ratios_before_samples() -> Result<()> {
        use zigbee_devices::metering::{ELECTRICAL_MEASUREMENT as AC, METERING};
        let ep = Endpoint { id: 9, profile: 0x0104, servers: &[AC, METERING], clients: &[] };
        let mut meter = Household::default();
        let initial = [
            (AC, 0x50b, 0x29, &[20, 0][..]), (AC, 0x604, 0x21, &[1, 0]), (AC, 0x605, 0x21, &[2, 0]),
            (AC, 0x505, 0x21, &[9, 0]), (AC, 0x600, 0x21, &[1, 0]),
            (AC, 0x508, 0x21, &[255, 255]), (AC, 0x602, 0x21, &[1, 0]), (AC, 0x603, 0x21, &[1, 0]),
            (METERING, 0, 0x25, &[1; 6]), (METERING, 0x300, 0x30, &[0x80]),
            (METERING, 0x301, 0x22, &[1, 0, 0]), (METERING, 0x302, 0x22, &[1, 0, 0]),
        ];
        let values = initial.iter().map(|&(c, a, ty, b)| (c, a, Value::from_bytes(ty.try_into().unwrap(), b).unwrap()));
        let client = Endpoint { id: ep.id, profile: ep.profile, servers: &[], clients: ep.servers };
        meter.add_standard_meter(&client, values.clone(), true);
        assert!(!meter.active());
        meter.add_standard_meter(&ep, values.clone(), false);
        assert_eq!(meter.readings(), &[Reading::Power]);
        let raw = [8, 3, 10, 11, 5, 0x29, 20, 0];
        assert!(meter.frame(AC, Frame::decode(&raw)?, None)?.unwrap().is_empty());
        meter = Household::default();
        meter.add_standard_meter(&ep, values, true);
        assert_eq!(meter.readings(), &[Reading::Power]);
        let report = |bytes| meter.frame(AC, Frame::decode(bytes)?, None);
        assert_eq!(report(&raw)?.unwrap(), vec![("power", 10.0.into())]);
        let output = report(&[8, 3, 10, 11, 5, 0x29, 20, 0, 5, 6, 0x21, 4, 0])?;
        assert_eq!(output.unwrap(), vec![("power", 5.0.into())]);
        assert!(report(&[8, 3, 10, 11, 5, 0x29, 0, 128])?.unwrap().is_empty());
        let get = Some(Query::Meter(Reading::Power));
        // Failed divisor, followed by a perfectly ordinary report.
        let incomplete = [8, 3, 1, 11, 5, 0, 0x29, 20, 0, 4, 6, 0, 0x21, 1, 0, 5, 6, 0x86];
        assert!(meter.frame(AC, Frame::decode(&incomplete)?, get)?.unwrap().is_empty());
        assert!(report(&raw)?.unwrap().is_empty());
        assert!(report(&[8, 3, 10, 5, 6, 0x21, 0, 0, 11, 5, 0x29, 20, 0])?.unwrap().is_empty());
        assert_eq!(report(&[8, 3, 10, 5, 6, 0x21, 5, 0, 11, 5, 0x29, 20, 0])?.unwrap(), vec![("power", 4.0.into())]);
        Ok(())
    }

}
