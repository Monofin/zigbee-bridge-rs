//! Translate one complete interview into supported endpoint functions.
use super::{Binding, Bindings, Identity, Planned, Read, SENSORS, endpoint, sensor_kind};
use crate::interview_log::{AttributeResult, IdentitySource, InterviewReport, Observation};
use crate::mqtt::{Command, Operation, Publisher, discovery::Feature};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use zigbee_devices::{
    StandardLights,
    light_index::LightIndex,
    lighting::{LightingQuirk, Property, Request, Setting},
    measurement,
    tuya::Button,
};

impl Bindings {
    /// Secure locks also require APS protection for Groups and Scenes, even on a second endpoint.
    pub(super) fn allows_network_security(&self, ieee:u64)->bool {
        self.peers.contains_key(&ieee) && self.network_security_members.contains(&ieee)
    }

    pub fn load_availability(&mut self, dir: &std::path::Path) -> Result<()> {
        self.availability = crate::availability::Catalogue::load(dir)?.into();
        for (&(ieee, _), binding) in &mut self.devices {
            binding.device_availability = self.availability.enabled(ieee);
        }
        Ok(())
    }
    /// Load the stored per-device settings before adding the associated interviews.
    pub fn load_device_options<'a>(&mut self, devices: impl Iterator<Item = &'a crate::registry::Device>) {
        self.temperature_options.clear();
        self.vibration_timeouts.clear();
        for device in devices {
            if let Some(force) = device.temperature_force_update {
                self.temperature_options.insert(device.ieee, force);
            }
            if let Some(seconds) = device.vibration_timeout {
                self.vibration_timeouts.insert(device.ieee, seconds);
            }
        }
    }

    pub fn reviewed_lights(index: LightIndex) -> Self {
        Self {
            light_index: Some(index.into()),
            ..Default::default()
        }
    }

    pub(super) async fn advertise(&self, out:&Publisher)->Result<()> {
        for &ieee in self.peers.keys() { self.advertise_member(out, ieee).await?; }
        Ok(())
    }

    pub(super) async fn advertise_member(&self, out: &Publisher, ieee: u64) -> Result<()> {
        for (ep, feature) in self.member_features(ieee)? {
            out.discover_when_ready(out.discovery_prefix(), &ep, feature).await?;
        }
        Ok(())
    }

    fn member_features(&self, ieee: u64) -> Result<Vec<(crate::mqtt::discovery::Endpoint<'_>, Feature)>> {
        let mut features = Vec::new();
        for (&id, binding) in self.devices.range((ieee, 0)..=(ieee, u8::MAX)) {
            for feature in self.discovery_features(id)? {
                features.push((binding.discovery_for(id, feature), feature));
            }
        }
        features.extend(self.remote_features(ieee));
        Ok(features)
    }

    pub(super) fn member_topics(&self, ieee: u64, out: &Publisher) -> Result<std::collections::BTreeSet<String>> {
        let mut topics = std::collections::BTreeSet::new();
        for (endpoint, feature) in self.member_features(ieee)? {
            topics.extend(out.discovery_topics(&endpoint, feature)?);
        }
        Ok(topics)
    }

    fn discovery_features(&self, id: Identity) -> Result<Vec<Feature>> {
        let mut features = self.devices[&id].discovery_features(id)?;
        if self.lock_controls.contains(&id) && self.allows_network_security(id.0) {features.push(Feature::DoorLock); }
        // These capabilities are advertised once per physical device.
        if self.devices.range((id.0, 0)..=id).next().is_some_and(|(first, _)| *first == id) {
            if self.devices[&id].declares_ota {
                features.push(Feature::FirmwareUpdate);
            }
            features.push(Feature::Sensor(crate::mqtt::discovery::Sensor::LinkQuality));
            if self.last_seen_enabled(){
                features.push(Feature::LastSeen{timestamp:self.last_seen.timestamp()});
            }
        }
        Ok(features)
    }

    /// Generate precisely the configs this coordinator would advertise at startup.
    /// Runtime-only observations are unavailable until captured in a member report.
    pub fn discovery_snapshot(
        &self,
        base: &str,
        prefix: &str,
        coordinator_ieee: Option<u64>,
    ) -> Result<BTreeMap<String, serde_json::Value>> {
        use crate::mqtt::discovery::{admin_configs, configs_named};
        let mut result = BTreeMap::new();
        let mut add = |(topic, bytes): (String, Vec<u8>)| -> Result<()> {
            ensure!(
                crate::mqtt::publish_fits(&topic, bytes.len()),
                "discovery PUBLISH exceeds transport budget for {topic}"
            );
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            if let Some(old) = result.insert(topic, value.clone()) {
                ensure!(
                    old == value,
                    "conflicting discovery definitions for one topic"
                );
            }
            Ok(())
        };
        if let Some(ieee)=coordinator_ieee {for entry in admin_configs(base,prefix,ieee)? {add(entry)?;}}
        for &ieee in self.peers.keys() {
            for (ep, feature) in self.member_features(ieee)? {
                for entry in configs_named(base, prefix, &ep, feature, false, self.naming.display(ieee))? { add(entry)?; }
            }
        }
        Ok(result)
    }

    pub(super) fn mqtt_routes(&self, out: &Publisher) -> Result<()> {
        let mut routes = self.topics.clone();
        for (&ieee, (name, _)) in &self.peers {
            routes.entry(name.clone()).or_insert((ieee, 0));
            routes
                .entry(format!("0x{ieee:016x}"))
                .or_insert((ieee, 255));
        }
        out.set_routes(routes)?;
        out.set_display_names(self.naming.displays());
        Ok(())
    }

    /// Use the Basic identity and descriptors from one complete interview. An
    /// archived short address is never used to route a command or a report.
    pub fn add(&mut self, report: &InterviewReport, names: &BTreeMap<u64, String>) -> Result<()> {
        self.add_interview(report, names, false)
    }

    pub(super) fn add_fresh(&mut self, report: &InterviewReport, names: &BTreeMap<u64, String>) -> Result<()> {
        self.add_interview(report, names, true)
    }

    fn add_interview(&mut self, report: &InterviewReport, names: &BTreeMap<u64, String>, fresh: bool) -> Result<()> {
        ensure!(report.finished, "incomplete device interview");
        let mut ieee = None;
        let mut descriptors = BTreeMap::new();
        let mut endpoints = None;
        let mut io_access = BTreeMap::new();
        let mut lock_security = BTreeMap::new();
        let mut commands = BTreeMap::new();
        let mut attributes = BTreeMap::new();
        let mut configured = std::collections::BTreeSet::new();
        for entry in &report.entries {
            if let Observation::Attribute{endpoint, cluster:zigbee_devices::equipment::DOOR_LOCK,
                attribute:zigbee_devices::equipment::SECURITY_LEVEL, result,..}=&entry.observation {
                let evidence=lock_security.entry(*endpoint).or_insert(Some(result));
                if *evidence != Some(result) { *evidence=None; }
            }
            match &entry.observation {
                Observation::Target {
                    ieee: address,
                    identity_source,
                    ..
                } => {
                    ensure!(
                        !matches!(identity_source, IdentitySource::LastKnownAddress),
                        "unverified device identity"
                    );
                    ensure!(
                        ieee.replace(u64::from_str_radix(address, 16)?).is_none(),
                        "multiple interview targets"
                    );
                }
                Observation::ActiveEndpoints { endpoints: eps, .. } => {
                    ensure!(endpoints.replace(eps).is_none(), "duplicate endpoint list");
                }
                Observation::SimpleDescriptor {
                    endpoint,
                    descriptor,
                    ..
                } => {
                    ensure!(
                        descriptors.insert(*endpoint, descriptor).is_none(),
                        "duplicate endpoint"
                    );
                }
                Observation::AttributeAccess { endpoint, cluster, attribute: zigbee_devices::basic_io::PRESENT, data_type, access } => {
                    io_access.insert((*endpoint, *cluster), (*data_type,*access));
                }
                Observation::ReportingConfigured { endpoint, cluster, attribute } => {
                    configured.insert((*endpoint, *cluster, *attribute));
                }
                Observation::CoverCommandsReviewed{endpoint,commands:ids}=>{
                    let known = commands.entry((*endpoint,zigbee_devices::cover::CLUSTER)).or_insert_with(Vec::new);
                    known.retain(|id|!(5..=8).contains(id));
                    known.extend(ids.iter().copied().filter(|id|(5..=8).contains(id)));
                }
                Observation::CommandsReceived { endpoint, cluster, commands: ids } => {
                    commands.entry((*endpoint, *cluster)).or_insert_with(Vec::new).extend_from_slice(ids);
                }
                Observation::Attribute {
                    endpoint,
                    cluster,
                    attribute,
                    result: AttributeResult::Value { data_type, bytes },
                    ..
                } => {
                    let value = (*data_type, bytes.as_slice());
                    ensure!(
                        attributes
                            .insert((*endpoint, *cluster, *attribute), value)
                            .is_none_or(|old| old == value),
                        "conflicting interview attribute"
                    );
                }
                _ => {}
            }
        }
        let ieee = ieee.context("missing interview identity")?;
        let name = self.naming.route(ieee, names);
        ensure!(!self.peers.iter().any(|(&id, (other, _))| id != ieee && *other == name)
            && self.topics.get(&name).is_none_or(|id| id.0 == ieee), "duplicate device route");
        let lights = StandardLights::builtin()?;
        let outlets = zigbee_devices::outlet::Outlets::builtin()?;
        let inputs = zigbee_devices::input::Inputs::builtin()?;
        // Basic identity is device-wide. Reuse only unambiguous, confirmed
        // Basic reads when a second application endpoint omits those values.
        let basic = |attribute| {
            let mut values = attributes.iter().filter_map(|((_, cluster, id), (ty, data))| {
                if (*cluster, *id, *ty) != (0, attribute, 0x42) {
                    return None;
                }
                let (&length, bytes) = data.split_first()?;
                if usize::from(length) != bytes.len() || length == 0xff {
                    return None;
                }
                std::str::from_utf8(bytes).ok()
            });
            let first = values.next()?;
            values.all(|value| value == first).then_some(first)
        };
        let manufacturer = basic(4);
        let model = basic(5);
        let identity = manufacturer.zip(model);
        let network_security = descriptors.iter().all(|(ep,d)|
            !d.servers.contains(&zigbee_devices::equipment::DOOR_LOCK) ||
            lock_security.get(ep).copied().flatten().is_some_and(AttributeResult::allows_lock_network_security));
        let peer = (name.clone(), descriptors.iter().map(|(&ep, d)| (ep, (*d).clone())).collect());
        let declares_ota = model.is_some_and(|model| zigbee_devices::ota::declared(model, manufacturer))
            && descriptors.iter().any(|(ep, d)| {
                (1..=240).contains(ep) && d.profile == 0x0104 && d.clients.contains(&0x19)
            });
        let mut ota_only = None;
        let mut found = Vec::new();
        let complete = endpoints.is_some_and(|list| {
            list.len() == descriptors.len() && descriptors.keys().all(|id| list.contains(id))
        });
        let light_count = if complete {
            descriptors
                .iter()
                .filter(|(id, d)| lights.matches(&endpoint(**id, d), d.device))
                .count()
        } else {
            0
        };
        for (id, d) in descriptors {
            let ep = endpoint(id, d);
            let string = |attr| match attributes.get(&(id, 0, attr)) {
                Some((0x42, [len, bytes @ ..])) if usize::from(*len) == bytes.len() => {
                    std::str::from_utf8(bytes).ok()
                }
                _ => None,
            };
            let word = |attr, kind| match attributes.get(&(id, 0x300, attr)) {
                Some((ty, [lo, hi])) if *ty == kind => Some(u16::from_le_bytes([*lo, *hi])),
                _ => None,
            };
            let datapoints = match string(4).zip(string(5)) {
                Some((maker,model)) => zigbee_devices::tuya::household::Device::resolve(maker,model,&ep)?,
                None => None,
            };
            let limits=word(0x400b, 0x21).zip(word(0x400c, 0x21));
            let controls=zigbee_devices::standard_controls::Controls::resolve(identity,&ep,d.device,limits)?;
            let mut household = super::household::Household::resolve(&outlets,&inputs, identity, &ep, d.device)?;
            if let (Some(input), Some(seconds)) = (&household.input, self.vibration_timeouts.get(&ieee)) {
                input.lock().map_err(|_| anyhow::anyhow!("input state poisoned"))?.set_vibration_timeout(*seconds);
            }
            if datapoints.is_none() {
                let values = attributes.range((id, 0, 0)..=(id, u16::MAX, u16::MAX)).filter_map(
                    |(&(_, c, a), &(ty, bytes))| Some((c, a, zigbee_zcl::value::Value::from_bytes(ty.try_into().ok()?, bytes).ok()?))
                );
                household.add_standard_meter(&ep, values, fresh);
            }
            if let (Some(input), Some(model), Some(build)) = (&household.input, model, basic(0x4000)) {
                input.lock().unwrap_or_else(|e| e.into_inner()).set_software_build(model, build);
            }
            let button = string(4)
                .zip(string(5))
                .and_then(|(m, s)| Button::resolve(m, s, &ep));
            let quirk = string(4)
                .zip(string(5))
                .and_then(|(m, s)| LightingQuirk::resolve(m, s, &ep, d.device));
            let reviewed = string(5).and_then(|model| {
                self.light_index
                    .as_ref()?
                    .resolve(model, &ep, d.device, light_count)
            });
            let is_light = datapoints.is_none() && !household.owns_primary_function() && button.is_none()
                && lights.matches(&ep, d.device)
                && (self.light_index.is_none() || reviewed.is_some() || quirk.is_some());
            let cover_byte = |attr, ty| match attributes.get(&(id, zigbee_devices::cover::CLUSTER, attr)) {
                Some((t, [v])) if *t == ty => Some(*v),
                _ => None,
            };
            let cover = if datapoints.is_none() && !household.owns_primary_function() && !is_light && button.is_none() {
                zigbee_devices::cover::Cover::resolve(&ep, cover_byte(0, 0x30), cover_byte(7, 0x18),
                    commands.get(&(id, zigbee_devices::cover::CLUSTER)).map(Vec::as_slice).unwrap_or_default())
            } else { None };
            let mut measured = Vec::new();
            for (_, kind) in SENSORS {
                let (cluster, attr) = kind.address();
                let observed = attributes.get(&(id, cluster, attr)).is_some_and(|(ty, bytes)| {
                    (*ty).try_into().ok().and_then(|ty| zigbee_zcl::value::Value::from_bytes(ty, bytes).ok())
                        .and_then(|v| measurement::observe(&ep, cluster, attr, None, v)).is_some()
                });
                // MeasuredValue is mandatory on a standard environmental measurement server.
                // Power Configuration's battery attributes are optional, so require evidence.
                if kind.available(&ep) && (observed || kind.declared(&ep, identity)
                    || configured.contains(&(id, cluster, attr))) {
                    measured.push(sensor_kind(kind));
                }
            }
            let temperature_force_update = self.temperature_options.get(&ieee).copied()
                .filter(|_| id == 1 && ep.profile == 0x0104 && ep.servers.contains(&0x0402));
            let sensor = ep.profile == 0x0104
                && ep
                    .servers
                    .iter()
                    .any(|c| matches!(*c, 0x402 | 0x403 | 0x405));
            let mut hvac = zigbee_devices::hvac::Capabilities::default();
            if datapoints.is_none() && !household.owns_primary_function() {
                hvac = zigbee_devices::hvac::Capabilities::from_descriptor(&ep, d.device);
                for ((_, cluster, attr), (ty, bytes)) in attributes.range((id,0x0201,0)..=(id,0x0202,u16::MAX)) {
                    if let Some(value) = (*ty).try_into().ok().and_then(|ty| zigbee_zcl::value::Value::from_bytes(ty,bytes).ok()) {
                        hvac.record(*cluster,*attr,value);
                    }
                }
            }
            let mut equipment_commands = if super::standard::application(id,d) {
                super::equipment::Commands::resolve(&d.servers,|cluster,attr| attributes.get(&(id,cluster,attr)).copied())
            } else { Default::default() };
            if !is_light && datapoints.is_none() && !household.owns_primary_function() && button.is_none() {
                equipment_commands.output=zigbee_devices::equipment::level_output::Output::resolve(&ep,d.device,|attr|attributes.get(&(id,8,attr)).copied());
            }
            let mut equipment: Vec<_> = zigbee_devices::equipment::FIELDS.iter().filter(|field| {
                super::standard::application(id, d) && d.servers.contains(&field.cluster) &&
                    (field.cluster != zigbee_devices::equipment::DOOR_LOCK || network_security) &&
                    (field.mandatory || attributes.get(&(id, field.cluster, field.id))
                        .and_then(|(ty, bytes)| zigbee_zcl::value::Value::from_bytes((*ty).try_into().ok()?, bytes).ok())
                        .and_then(|value| field.decode(value)).is_some())
            }).collect();
            if equipment_commands.output.is_some(){
                use zigbee_devices::equipment::level_output::{STATE,LEVEL};
                equipment.extend([&STATE,&LEVEL]);
            }
            let zone = if datapoints.is_none() && household.input.is_none() && button.is_none() {
                let kind = match attributes.get(&(id, zigbee_devices::ias::CLUSTER, 1)) {
                    Some((0x31, [lo, hi])) => Some(u16::from_le_bytes([*lo, *hi])),
                    _ => None,
                };
                zigbee_devices::ias::Zone::resolve(&ep, kind)
            } else { None };
            // A named profile may already own the property and route its reports to the device topic.
            let environment: Vec<_> = zigbee_devices::measurement::environment::for_endpoint(&ep)
                .filter(|r| !household.input_property(r.property)
                    && !datapoints.is_some_and(|dp| dp.fields().iter().any(|f| f.key == r.property)))
                .collect();
            let io: Vec<_> = d.servers.iter().filter(|_| super::standard::application(id,d))
                .filter_map(|&c| zigbee_devices::basic_io::Channel::resolve(c, io_access.get(&(id,c)).copied(),
                    |a| attributes.get(&(id,c,a)).copied())).collect();
            let previously_unmapped = zone.is_none() && !hvac.active() && cover.is_none() && datapoints.is_none() && !household.active() && button.is_none() && !is_light && !sensor && measured.is_empty() && !controls.identify && controls.startup_mireds.is_none();
            let otherwise_unused = previously_unmapped && io.is_empty() && environment.is_empty()
                && equipment.is_empty() && !equipment_commands.active();
            // A device may expose only an OTA client. Keep one endpoint so its
            // device-wide update entity can use the normal discovery path.
            let ota_fallback = declares_ota && ep.profile == 0x0104 && ep.clients.contains(&0x19) && ota_only.is_none();
            if otherwise_unused && !ota_fallback {
                continue;
            }
            let binding = (id, Binding {
                    preserve_raw_reports: previously_unmapped && !ota_fallback,
                    declares_ota,
                    device_availability:self.availability.enabled(ieee),
                    temperature_force_update,
                    controls,
                    level_state: Default::default(),
                    confirmed_hue_scene: None,
                    needs_light_power: false,
                    name: name.clone(),
                    device_name: name.clone(),
                    access: reviewed.map(|r| (r.state, r.brightness)),
                    io,
                    measured,
                    environment,
                    hvac,
                    equipment, equipment_commands,
                    zone,
                    household,
                    cover,
                    datapoints,
                    descriptor: d.clone(),
                    button,
                    is_light,
                    quirk,
                    color: word(0x400a, 0x19),
                    limits,
                },
            );
            if otherwise_unused {
                ota_only = Some(binding);
            } else {
                found.push(binding);
            }
        }
        if found.is_empty() {
            found.extend(ota_only);
        }
        let multi = found.len() > 1;
        for (id, b) in &mut found {
            if multi && *id != 1 {
                b.name = format!("{name}/ep{id}");
            }
            ensure!(
                crate::mqtt::device_name(&b.name),
                "invalid expanded MQTT topic for device {name}, endpoint {id}"
            );
            ensure!(
                self.topics
                    .get(&b.name)
                    .is_none_or(|(owner, _)| *owner == ieee),
                "duplicate endpoint topic"
            );
        }
        self.household_leave(ieee);
        if network_security {self.network_security_members.insert(ieee);}
        else {self.network_security_members.remove(&ieee);}
        if complete { self.complete_descriptors.insert(ieee); } else { self.complete_descriptors.remove(&ieee); }
        if let Some(enabled) = report.entries.iter().find_map(|entry| match entry.observation {
            Observation::ReceiverOnWhenIdle { enabled } => Some(enabled),
            _ => None,
        }) {
            self.receiver_on_when_idle.insert(ieee, enabled);
        } else {
            self.receiver_on_when_idle.remove(&ieee);
        }
        self.peers.insert(ieee,peer);
        self.devices.retain(|(peer, _), _| *peer != ieee);
        self.topics.retain(|_, (peer, _)| *peer != ieee);
        for (id, b) in found {
            self.topics.insert(b.name.clone(), (ieee, id));
            self.devices.insert((ieee, id), b);
        }
        Ok(())
    }

    pub(super) fn plan(&self, cmd: &Command, seq: u8) -> Result<(Identity, Vec<Planned>)> {
        if cmd.properties.keys().any(|key| key.starts_with("zcl_")) {
            return self.standard_plan(cmd, seq);
        }
        // Inputs publish on the device topic even when the server lives on a
        // secondary endpoint. Explicit endpoint topics stay endpoint-scoped.
        let direct = self.topics.get(&cmd.device).copied();
        let input_read = cmd.operation == Operation::Get && !cmd.properties.is_empty()
            && cmd.properties.keys().all(|k| zigbee_devices::input::Reading::from_property(k).is_some());
        let target = if input_read && let Ok(ieee) = self.peer(&cmd.device) {
            self.devices.range((ieee, 0)..=(ieee, u8::MAX))
                .find(|(_, binding)| cmd.properties.keys().all(|k| binding.household.input_reading(k).is_some()))
                .map(|(&id, _)| id)
        } else { None };
        let id = target.or(direct).context("unknown MQTT device or unsupported input read")?;
        let b = &self.devices[&id];
        if let Some(steps) = super::basic_io::plan(&b.io, id.1, seq, cmd)? {
            return Ok((id, steps));
        }
        if let Some(steps) = super::equipment::plan(b, id.1,cmd,seq)? { return Ok((id,steps)); }
        if let Some(device) = b.datapoints
            && !cmd.properties.keys().all(|key| b.environment.iter().any(|r| r.property == key)) {
            return Ok((id,super::tuya::plan(device,cmd,seq)?));
        }
        let ep = endpoint(id.1, &b.descriptor);
        if b.hvac.dual_mode() && cmd.operation == Operation::Set {
            use zigbee_devices::hvac::{Property::{CoolingSetpoint, HeatingSetpoint}, Setpoints};
            let heat = cmd.properties.get(HeatingSetpoint.name());
            let cool = cmd.properties.get(CoolingSetpoint.name());
            if heat.is_some() || cool.is_some() {
                ensure!(cmd.properties.len() == usize::from(heat.is_some()) + usize::from(cool.is_some()),
                    "send thermostat setpoints in their own request");
                let proposed = Setpoints::new(heat,cool)?;
                return Ok((id, vec![(Setpoints::preflight(ep.id,seq)?,
                    Some(Read::Setpoints(super::hvac::SetpointOperation::Preflight(proposed))))]));
            }
        }
        let mut steps = Vec::new();
        let mut transition = 0;
        if let Some(v) = cmd.properties.get("transition") {
            ensure!(
                matches!(cmd.operation, Operation::Set),
                "transition applies only to set"
            );
            let ds = v.as_f64().context("transition must be seconds")? * 10.;
            ensure!((0.0..=65534.).contains(&ds), "transition out of range");
            transition = ds.round() as u16;
        }
        let brightness = cmd.properties.get("brightness");
        let compound = if let (Operation::Set, Some(q), Some(state), Some(level)) = (
            cmd.operation,
            b.quirk,
            cmd.properties.get("state"),
            brightness,
        ) {
            let Setting::State(on) = Property::State.setting(state)? else {
                unreachable!()
            };
            let Setting::Brightness(level) = Property::Brightness.setting(level)? else {
                unreachable!()
            };
            Some(q.combined_setting(on, level, cmd.properties.contains_key("transition"))?)
        } else {
            None
        };
        let hue_keys = ["effect", "effect_speed", "effect_color"];
        let hue_request = b.controls.hue_effects && hue_keys.iter().any(|key| cmd.properties.contains_key(*key));
        if hue_request {
            ensure!(cmd.operation == Operation::Set && b.is_light, "Hue effects require a light set command");
            let effect = cmd.properties.get("effect").map(|v| v.as_str().context("expected effect name")).transpose()?;
            let speed = cmd.properties.get("effect_speed").map(|v| v.as_f64().context("expected numeric effect speed")).transpose()?;
            let requests = zigbee_devices::standard_controls::hue::set_fields(
                ep.id, seq, effect, speed, cmd.properties.get("effect_color"), b.confirmed_hue_scene)?;
            // An activated scene may reset brightness. Apply an explicit brightness afterwards.
            steps.extend(requests.into_iter().map(|request| (request,None)));
        }
        for (name, value) in &cmd.properties {
            if name == "transition" {
                continue;
            }
            let seq = seq.wrapping_add(steps.len() as u8);
            if let Some(attribute) = b.zone.and_then(|zone| zone.property(name)) {
                ensure!(cmd.operation == Operation::Get && !cmd.properties.contains_key("transition"), "IAS Zone attributes are read-only");
                steps.push((attribute.request(ep.id, seq), Some(Read::Ias(attribute))));
                continue;
            }
            if let Some(cover) = b.cover && matches!(name.as_str(), "state" | "position" | "tilt") {
                ensure!(!cmd.properties.contains_key("transition"), "cover does not accept a transition");
                steps.push(match cmd.operation {
                    Operation::Get => {
                        let axis = cover.axis(name)?;
                        (cover.read(ep.id, seq, axis)?, Some(Read::Cover(axis)))
                    }
                    Operation::Set => (cover.set(ep.id, seq, name, value)?, None),
                });
                continue;
            }
            if hue_request && hue_keys.contains(&name.as_str()) {
                continue;
            }
            if name == zigbee_devices::standard_controls::level_config::PROPERTY {
                use zigbee_devices::standard_controls::level_config::{Field, Patch};
                use super::level_config::Operation as Level;
                ensure!(b.is_light && b.controls.level_config && !cmd.properties.contains_key("transition"), "level configuration unavailable");
                match cmd.operation {
                    Operation::Get => {
                        for (offset,field) in [Field::Options,Field::Startup].into_iter().enumerate() {
                            steps.push((field.read(ep.id, seq.wrapping_add(offset as u8))?,Some(Read::LevelConfig(Level::Read(field)))));
                        }
                    }
                    Operation::Set => {
                        let patch=Patch::parse(value)?;
                        if patch.execute_if_off.is_some() {
                            steps.push((Field::Options.read(ep.id,seq)?,Some(Read::LevelConfig(Level::ReadOptions(patch)))));
                        } else {
                            steps.push((patch.write(ep.id,seq,None)?,Some(Read::LevelConfig(Level::Write))));
                        }
                    }
                }
                continue;
            }
            if name == "effect" {
                ensure!(b.is_light && cmd.operation==Operation::Set, "effect requires a supported light");
                let effects=b.controls.effects.context("effects unavailable on this light")?;
                let effect=value.as_str().context("effect requires a name")?;
                let period=cmd.properties.get("transition").and_then(|v|v.as_f64());
                let request=effects.set(ep.id,seq,effect,period)?;
                let follow=effect.eq_ignore_ascii_case("stop_colorloop").then_some(Read::LoopStop);
                steps.push((request,follow));
                continue;
            }
            if matches!(name.as_str(), zigbee_devices::tuya::lamp::STARTUP | zigbee_devices::tuya::lamp::DND) {
                ensure!(matches!(cmd.operation, Operation::Set) && !cmd.properties.contains_key("transition"), "lamp configuration requires set without transition");
                let setting = b.controls.tuya_lamp.context("unsupported Tuya lamp")?.command(name, value)?;
                steps.push((setting.request(ep.id, seq), Some(Read::TuyaLamp(setting))));
                continue;
            }
            if name == "identify" || name == zigbee_devices::standard_controls::STARTUP {
                ensure!(!cmd.properties.contains_key("transition"), "configuration controls have no transition");
                if name == "identify" {
                    ensure!(cmd.operation == Operation::Set, "identify is set-only");
                    steps.push((b.controls.identify(ep.id, seq, value)?, None));
                } else {
                    let (req, operation)=b.controls.startup(ep.id,seq,(cmd.operation==Operation::Set).then_some(value))?;
                    steps.push((req,Some(Read::Startup(operation))));
                }
                continue;
            }
            if b.hvac.active() && let Some(property) = zigbee_devices::hvac::PROPERTIES.into_iter().find(|p|p.name()==name) {
                ensure!(!cmd.properties.contains_key("transition"), "HVAC attributes have no transition");
                let write = cmd.operation == Operation::Set;
                let req = b.hvac.request(property,ep.id,seq,write.then_some(value))?;
                steps.push((req,Some(Read::Hvac(super::hvac::Operation{property,write}))));
                continue;
            }
            if let Some(plan) = b.household.plan(ep.id, cmd.operation, name, value, seq)? {
                ensure!(!cmd.properties.contains_key("transition"), "relay controls have no transition");
                steps.push(plan);
                continue;
            }
            if let Some(r) = b.environment.iter().copied().find(|r| r.property == name) {
                ensure!(!cmd.properties.contains_key("transition"), "measurement has no transition");
                let write = cmd.operation == Operation::Set;
                steps.push((r.request(ep.id, seq, write.then_some(value))?, Some(Read::Environment(r, write))));
                continue;
            }
            if let Some((_, attr)) = SENSORS.iter().find(|(key, _)| key == name) {
                ensure!(cmd.operation == Operation::Get, "measurement is read-only");
                let frame = attr.read(&ep, seq)?;
                steps.push((
                    Request {
                        endpoint: ep.id,
                        cluster: attr.address().0,
                        frame,
                    },
                    Some(Read::Sensor(*attr)),
                ));
                continue;
            }
            let property = Property::from_name(name).context("unsupported device property")?;
            ensure!(
                b.permits(property, |a| match cmd.operation {
                    Operation::Get => a.read,
                    Operation::Set => a.write,
                }),
                "operation unavailable for this model"
            );
            let lamp = b.light(&ep)?;
            let step = match cmd.operation {
                Operation::Get => (lamp.get(property, seq)?, Some(Read::Light(property))),
                Operation::Set => {
                    if compound.is_some() && property == Property::State {
                        continue;
                    }
                    let setting =
                        if let (Property::Brightness, Some(setting)) = (property, compound) {
                            setting
                        } else {
                            property.setting(value)?
                        };
                    if compound.is_none()
                        && let (Setting::State(on), Some(level)) = (setting, brightness)
                    {
                        ensure!(
                            lamp.supports(Property::State)
                                && level.as_u64().is_some_and(|n| (n != 0) == on),
                            "state and brightness disagree"
                        );
                        continue;
                    }
                    let read = match setting {
                        Setting::State(on) if transition == 0 && b.permits(Property::State, |a| a.read) => Some(Read::ConfirmOnOff(on)),
                        _ => None,
                    };
                    (lamp.set(setting, transition, seq)?, read)
                }
            };
            steps.push(step);
        }
        ensure!(!steps.is_empty(), "empty control operation");
        Ok((id, steps))
    }
}


impl Binding {
    fn discovery_for(&self, id: Identity, feature: Feature) -> crate::mqtt::discovery::Endpoint<'_> {
        let mut ep = self.discovery(id);
        let property = match feature {
            Feature::Actions(_) => "action",
            Feature::Binary(property, _) | Feature::Numeric { property, .. } | Feature::Measurement { property, .. } => property,
            Feature::Sensor(super::Sensor::Illuminance) => "illuminance",
            _ => return ep,
        };
        if self.household.input_property(property) {
            ep.topic_name = ep.device_name;
        }
        ep
    }
    fn discovery_features(&self,id:Identity)->Result<Vec<Feature>> {
        let mut features=self.household.features();
        for channel in &self.io {
            features.push(Feature::BasicIo(channel.layout));
            if let Some(key) = channel.label_property() { features.push(Feature::TextSensor(key)); }
        }
        features.extend(super::hvac::features(&self.hvac));
        features.extend(self.equipment_commands.features());
        features.extend(self.equipment.iter().map(|f| super::equipment::feature(f,self.equipment_commands)));
        if let Some(kind) = self.zone.and_then(|zone| zone.kind()) {
            features.push(super::ias::feature(kind));
        }
        if let Some(lamp) = self.controls.tuya_lamp {
            use zigbee_devices::tuya::lamp::{DND, STARTUP, MODES};
            features.push(Feature::BooleanSetting(DND));
            if lamp.supports_color() {
                features.push(Feature::Select { property: STARTUP, options: &*MODES });
            }
        }
        if self.controls.identify {features.push(Feature::Identify);}
        if let Some(cover) = self.cover { features.push(Feature::StandardCover(cover)); }
        if let Some(device)=self.datapoints {features.extend(super::tuya::features(device));}
        if self.button.is_some(){features.push(Feature::Button);}
        if self.is_light && self.permits(Property::State,|a|a.write) {
            let ep=endpoint(id.1,&self.descriptor);
            let light=self.light(&ep)?;
            let effects = if self.controls.hue_effects {
                let options = zigbee_devices::standard_controls::hue::NAMES;
                features.push(Feature::ReportedEffect(options));
                features.push(Feature::WriteOnlyText("effect_color"));
                features.push(Feature::Number {property:"effect_speed", min:0.0, max:1.0, step:0.01, unit:None});
                options
            } else if let Some(effects) = self.controls.effects {
                features.push(Feature::Effect(effects.names()));
                effects.names()
            } else {
                &[]
            };
            features.push(Feature::Light{
                effects,
                xy:light.xy() && self.permits(Property::Color,|a|a.write),
                hs:light.hs() && self.permits(Property::Color,|a|a.write),
                brightness:light.supports(Property::Brightness) && self.permits(Property::Brightness,|a|a.write),
                color_temperature:light.supports(Property::ColorTemperature) && self.permits(Property::ColorTemperature,|a|a.write),
                mired_range:self.controls.temperature_via_xy.or(self.limits),
            });
        }
        features.extend(self.environment.iter().copied().map(super::environment::feature));
        features.extend(self.measured.iter().copied().map(|sensor| Feature::Sensor(self.configured_sensor(sensor))));
        Ok(features)
    }
}
