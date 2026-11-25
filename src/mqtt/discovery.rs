//! Home Assistant single-component discovery from reviewed endpoint functions.
//! The identity formulas for the unqualified objects follow Zigbee2MQTT
//! homeassistant.ts at 9ff009282a931239c106ce0ad7cbea3e2da30704.
use super::{Publisher, SubmitError, device_name, topic_part};
use serde_json::{Value, json};

pub struct Endpoint<'a> {
    pub device_availability:bool,
    pub ieee: u64,
    pub endpoint: u8,
    pub device_name: &'a str,
    pub topic_name: &'a str,
}

#[derive(Clone, Copy)]
pub enum Sensor {
    LinkQuality,
    Battery,
    Voltage,
    Temperature,
    TemperatureWithForceUpdate(bool),
    Humidity,
    Pressure,
    Power,
    Current,
    Energy,
    PumpLifetimeRunningHours,
    MainsVoltage,
    SoilMoisture,
    Illuminance,
    TargetDistance,
}

impl Sensor {
    /// HA force_update turns repeated equal reports into distinct state events.
    pub(crate) fn report(self, out: &Publisher, name: &str, value: Value) -> Result<(), SubmitError> {
        let property = self.metadata().0;
        if matches!(self, Self::TemperatureWithForceUpdate(true)) {
            out.submit_field(name, property, value, super::Delivery::Observation)
        } else {
            out.state(name, property, value)
        }
    }

    fn metadata(self) -> (&'static str, Option<&'static str>, &'static str) {
        match self {
            Self::LinkQuality => ("linkquality", None, "lqi"),
            Self::SoilMoisture => ("soil_moisture", Some("moisture"), "%"),
            Self::Illuminance => ("illuminance", Some("illuminance"), "lx"),
            Self::TargetDistance => ("target_distance", Some("distance"), "m"),
            Self::Power => ("power", Some("power"), "W"),
            Self::Current => ("current", Some("current"), "A"),
            Self::Energy => ("energy", Some("energy"), "kWh"),
            Self::PumpLifetimeRunningHours => ("pump_lifetime_running_hours", Some("duration"), "h"),
            Self::MainsVoltage => ("voltage", Some("voltage"), "V"),
            Self::Battery => ("battery", Some("battery"), "%"),
            Self::Voltage => ("voltage", Some("voltage"), "mV"),
            Self::Temperature | Self::TemperatureWithForceUpdate(_) => ("temperature", Some("temperature"), "°C"),
            Self::Humidity => ("humidity", Some("humidity"), "%"),
            Self::Pressure => ("pressure", Some("atmospheric_pressure"), "hPa"),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Feature {
    DoorLock,
    Button,
    FirmwareUpdate,
    LastSeen { timestamp: bool },
    Effect(&'static [&'static str]),
    ReportedEffect(&'static [&'static str]),
    Identify,
    Actions(&'static [&'static str]),
    Binary(&'static str, &'static str),
    Contact,
    Cover,
    BasicIo(zigbee_devices::basic_io::Layout),
    RemoteTrigger { cluster: u16, name: &'static str },
    Barrier {partial:bool},
    StandardCover(zigbee_devices::cover::Cover),
    Number {
        property: &'static str,
        min: f64,
        max: f64,
        step: f64,
        unit: Option<&'static str>,
    },
    Select {
        property: &'static str,
        options: &'static [String],
    },
    Thermostat {
        modes: &'static [String],
        setpoint: &'static str,
        min: f64,
        max: f64,
    },
    TextSensor(&'static str),
    WriteOnlyText(&'static str),
    CompositeSensor(&'static str),
    Measurement {
        property: &'static str,
        unit: Option<&'static str>,
    },
    Numeric {
        property: &'static str,
        unit: Option<&'static str>,
        device_class: Option<&'static str>,
        state_class: Option<&'static str>,
    },
    BooleanSetting(&'static str),
    StringBinary {
        property: &'static str,
        off: &'static str,
        on: &'static str,
        writable: bool,
    },
    Switch,
    Valve,
    Light {
        effects: &'static [&'static str],
        xy: bool,
        hs: bool,
        brightness: bool,
        color_temperature: bool,
        mired_range: Option<(u16, u16)>,
    },
    Sensor(Sensor),
}

impl Feature {
    /// HA uses the category to exclude secondary entities from device-targeted actions.
    fn entity_category(self) -> Option<&'static str> {
        match self {
            Self::BooleanSetting(_) | Self::Identify | Self::FirmwareUpdate
            | Self::StringBinary { writable: true, property: "child_lock" | "led_enable" | "outlet_control_protect", .. }
            | Self::Select { property: "power_on_behavior" | "external_trigger_mode" | "indicator_mode"
                | "temperature_units" | "temperature_unit" | "motion_sensitivity" | "color_power_on_behavior", .. }
            | Self::Number { property: "temperature_calibration" | "humidity_calibration" | "comfort_temperature_min"
                | "comfort_temperature_max" | "comfort_humidity_min" | "comfort_humidity_max" | "effect_speed"
                | "delayed_power_on_time" | "minimum_range" | "maximum_range" | "detection_delay" | "fading_time"
                | "occupancy_timeout" | "detection_interval" | "sensitivity", .. }
            | Self::WriteOnlyText("effect_color") => Some("config"),
            Self::LastSeen { .. } | Self::Sensor(Sensor::Battery | Sensor::Voltage | Sensor::LinkQuality)
            | Self::CompositeSensor("color_options") | Self::TextSensor("battery_state")
            | Self::Binary("battery_low" | "illuminance_above_threshold", _)
            | Self::Numeric { property: "device_temperature" | "requested_brightness_level" | "requested_brightness_percent", .. }
                => Some("diagnostic"),
            _ => None,
        }
    }
}

fn safe_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

impl Publisher {
    pub(crate) fn discovery_topics(&self, endpoint: &Endpoint<'_>, feature: Feature)
        -> Result<impl Iterator<Item = String>, SubmitError> {
        Ok(configs(&self.0.base, self.discovery_prefix(), endpoint, feature, true)?
            .into_iter().map(|(topic, _)| topic))
    }

    pub(crate) async fn remove_member_topic(&self, ieee: u64, topic: &str, await_ack: bool) -> anyhow::Result<()> {
        let address = format!("0x{ieee:016x}");
        let mut levels = topic.strip_prefix(self.discovery_prefix()).and_then(|s| s.strip_prefix('/'))
            .ok_or(SubmitError::Invalid)?.split('/');
        anyhow::ensure!(levels.next().is_some_and(safe_key) && levels.next() == Some(address.as_str())
            && levels.next().is_some_and(safe_key) && levels.next() == Some("config") && levels.next().is_none(),
            "invalid owned discovery topic");
        if await_ack {
            self.retire_topic(topic.to_owned()).await
        } else {
            Ok(self.enqueue_when_ready(vec![(topic.to_owned(), Vec::new())]).await?)
        }
    }

    /// Replay the active bindings at coordinator startup. HA can replay these
    /// retained configs directly when it restarts or subscribes later.
    pub fn discover(
        &self,
        prefix: &str,
        endpoint: &Endpoint<'_>,
        feature: Feature,
    ) -> Result<(), SubmitError> {
        self.enqueue_configs(&mut configs(
            &self.0.base,
            prefix,
            endpoint,
            feature,
            false,
        )?)
    }

    /// A startup producer can await bounded queue space without occupying the
    /// radio or MQTT driver. Cancellation before a batch fits submits nothing.
    pub async fn discover_when_ready(
        &self,
        prefix: &str,
        endpoint: &Endpoint<'_>,
        feature: Feature,
    ) -> Result<(), SubmitError> {
        let configs = configs(&self.0.base, prefix, endpoint, feature, false)?;
        self.enqueue_when_ready(configs).await
    }

    async fn enqueue_when_ready(
        &self,
        mut configs: Vec<(String, Vec<u8>)>,
    ) -> Result<(), SubmitError> {
        loop {
            let space = self.0.config_space.notified();
            tokio::pin!(space);
            // Register before checking capacity: both a drain and shutdown
            // can otherwise arrive between the failed check and the await.
            space.as_mut().enable();
            match self.enqueue_configs(&mut configs) {
                Err(SubmitError::Full) => space.await,
                result => return result,
            }
        }
    }

    pub async fn discover_permit_join(
        &self,
        prefix: &str,
        coordinator_ieee: u64,
    ) -> Result<(), SubmitError> {
        self.enqueue_when_ready(vec![permit_config(&self.0.base, prefix, coordinator_ieee)?])
            .await
    }

    pub(crate) async fn discover_admin(&self, prefix: &str, ieee: u64) -> Result<(), SubmitError> {
        for entry in admin_configs(&self.0.base, prefix, ieee)? {
            self.enqueue_when_ready(vec![entry]).await?;
        }
        Ok(())
    }

    /// Empty retained configs retract just the named function. A timeout or
    /// offline transition does not invalidate an entity's persistent identity.
    pub fn remove_discovery(
        &self,
        prefix: &str,
        endpoint: &Endpoint<'_>,
        feature: Feature,
    ) -> Result<(), SubmitError> {
        self.enqueue_configs(&mut configs(&self.0.base, prefix, endpoint, feature, true)?)
    }
}
pub(crate) fn permit_config(
    base: &str,
    prefix: &str,
    coordinator_ieee: u64,
) -> Result<(String, Vec<u8>), SubmitError> {
    if !topic_part(prefix)
        || prefix.split('/').any(str::is_empty)
        || matches!(coordinator_ieee, 0 | u64::MAX)
    {
        return Err(SubmitError::Invalid);
    }
    let ieee = format!("0x{coordinator_ieee:016x}");
    let encoded: String = base.encode_utf16().map(|c| c.to_string()).collect();
    let topic = format!("{prefix}/switch/{encoded}_{ieee}/permit_join/config");
    let payload = json!({
        "name":"Permit join","icon":"mdi:human-greeting-proximity",
        "device":{"identifiers":[format!("zigbee2mqtt_bridge_{ieee}")],"name":"Zigbee2MQTT Bridge"},
        "unique_id":format!("bridge_{ieee}_permit_join_{base}"),
        "availability_mode":"all",
        "availability":[{"topic":format!("{base}/bridge/state"),"value_template":"{{ value_json.state }}"},
            {"topic":format!("{base}/bridge/info"),"value_template":"{{ 'online' if value_json.api_version | default(0) == 1 else 'offline' }}"}],
        "state_topic":format!("{base}/bridge/info"),"value_template":"{{ value_json.permit_join | lower }}",
        "command_topic":format!("{base}/bridge/v1/request/permit_join"),
        "state_on":"true","state_off":"false","payload_on":"{\"time\": 254}","payload_off":"{\"time\": 0}"
    });
    Ok((
        topic,
        serde_json::to_vec(&payload).expect("valid switch config"),
    ))
}

/// Bridge diagnostics share the existing coordinator device and migration identity.
pub(crate) fn admin_configs(
    base: &str, prefix: &str, coordinator: u64,
) -> Result<Vec<(String, Vec<u8>)>, SubmitError> {
    let permit = permit_config(base, prefix, coordinator)?;
    let ieee = format!("0x{coordinator:016x}");
    let node: String = base.encode_utf16().map(|c| c.to_string()).collect();
    let info = format!("{base}/bridge/info");
    let root = format!("{base}/bridge/v1");
    let online = json!({"topic":format!("{base}/bridge/state"),"value_template":"{{ value_json.state }}"});
    let compatible = json!({"topic":info,"value_template":"{{ 'online' if value_json.api_version | default(0) == 1 else 'offline' }}"});
    let mut entries = vec![permit];
    let mut add = |domain: &str, key: &str, mut config: Value| {
        config["device"] = json!({"identifiers":[format!("zigbee2mqtt_bridge_{ieee}")],"name":"Zigbee2MQTT Bridge"});
        config["unique_id"] = json!(format!("bridge_{ieee}_{key}_{base}"));
        config["entity_category"] = json!("diagnostic");
        if domain == "button" {
            config["availability"] = json!([online, compatible]);
            config["availability_mode"] = json!("all");
        } else if key != "connection" {
            config["availability"] = json!([online]);
        }
        entries.push((format!("{prefix}/{domain}/{node}_{ieee}/{key}/config"),
            serde_json::to_vec(&config).expect("valid discovery config")));
    };
    add("binary_sensor", "connection", json!({"name":"Bridge connection","device_class":"connectivity",
        "state_topic":format!("{base}/bridge/state"),"value_template":"{{ value_json.state }}",
        "payload_on":"online","payload_off":"offline"}));
    add("sensor", "api_version", json!({"name":"Admin API version","state_topic":info,
        "json_attributes_topic":info,
        "json_attributes_template":format!("{{{{ dict(mqtt_base={}) | to_json }}}}", json!(base)),
        "value_template":"{{ value_json.api_version | default(0) }}"}));
    add("button", "health_check", json!({"name":"Check coordinator","icon":"mdi:heart-pulse",
        "command_topic":format!("{root}/request/health_check"),"payload_press":"{}"}));
    add("sensor", "health_result", json!({"name":"Last coordinator check",
        "state_topic":format!("{root}/response/health_check"),
        "value_template":"{{ 'Healthy' if value_json.status == 'ok' and value_json.data.healthy | default(false) else 'Failed' }}"}));
    for (op, name) in [("bind", "Last binding"), ("unbind", "Last unbinding")] {
        add("sensor", op, json!({"name":name,"state_topic":format!("{root}/response/device/{op}"),
            "json_attributes_topic":format!("{root}/response/device/{op}"),
            "value_template":"{{ 'Failed' if value_json.status != 'ok' else ('Partially completed' if value_json.data.failed | default([]) else 'Completed') }}"}));
    }
    Ok(entries)
}

fn numeric_sensor(
    property: &str,
    unit: Option<&str>,
    device_class: Option<&str>,
    state_class: Option<&str>,
) -> Result<Value, SubmitError> {
    if !safe_key(property) {
        return Err(SubmitError::Invalid);
    }
    let mut spec = json!({"name":property,
            "value_template":format!("{{{{ value_json.get('{property}', '') }}}}")});
    if let Some(class) = device_class {
        spec["device_class"] = json!(class);
    }
    if let Some(class) = state_class {
        spec["state_class"] = json!(class);
    }
    if let Some(unit) = unit {
        spec["unit_of_measurement"] = json!(unit);
    }
    Ok(spec)
}

pub(crate) fn configs(
    base: &str,
    prefix: &str,
    ep: &Endpoint<'_>,
    feature: Feature,
    remove: bool,
) -> Result<Vec<(String, Vec<u8>)>, SubmitError> {
    if !topic_part(prefix)
        || prefix.split('/').any(str::is_empty)
        || !device_name(ep.topic_name)
        || !(1..=240).contains(&ep.endpoint)
        || ep.ieee == 0
        || ep.ieee == u64::MAX
        || ep.device_name.is_empty()
        || ep.device_name.len() > 512
    {
        return Err(SubmitError::Invalid);
    }
    let ieee = format!("0x{:016x}", ep.ieee);
    let state_topic = format!("{base}/{}", ep.topic_name);
    let device = json!({"identifiers":[format!("zigbee2mqtt_{ieee}")], "name":ep.device_name});
    let mut availability =
        json!([{"topic":format!("{base}/bridge/state"),"value_template":"{{ value_json.state }}"}]);
    if ep.device_availability {
        availability.as_array_mut().unwrap().push(json!({"topic":format!("{base}/{}/availability",ep.device_name),"value_template":"{{ value_json.state }}"}));
    }
    // A named multi-endpoint Z2M expose uses definition-specific labels.
    // A single application endpoint uses the device topic, whatever its number.
    let suffix = if ep.topic_name == ep.device_name || ep.endpoint == 1 {
        String::new()
    } else {
        format!("_ep{}", ep.endpoint)
    };
    let mut configs = Vec::new();
    let mut add = |domain: &str, object: &str, mut payload: Value| -> Result<(), SubmitError> {
        let device_level = matches!(domain, "update" | "button") || (domain == "sensor" && matches!(object, "linkquality" | "last_seen"));
        let object = if device_level {
            object.to_owned()
        } else {
            format!("{object}{suffix}")
        };
        let topic = format!("{prefix}/{domain}/{ieee}/{object}/config");
        payload["device"] = device.clone();
        if let Some(category) = feature.entity_category() {
            payload["entity_category"] = json!(category);
        }
        if domain != "device_automation" {
            payload["unique_id"] = json!(format!("{ieee}_{object}_{base}"));
            payload["availability"] = availability.clone();
            if ep.device_availability { payload["availability_mode"]=json!("all"); }
            if !matches!(domain,"button"|"climate") && !matches!(feature, Feature::Effect(_) | Feature::WriteOnlyText(_)) {
                payload["state_topic"] = if domain == "sensor" && matches!(object.as_str(), "linkquality" | "last_seen") {
                    json!(format!("{base}/{}", super::diagnostic_route(ep.device_name)))
                } else if device_level {
                    json!(format!("{base}/{}", ep.device_name))
                } else {
                    json!(state_topic)
                };
            }
        }
        let bytes = if remove {
            Vec::new()
        } else {
            serde_json::to_vec(&payload).map_err(|_| SubmitError::Invalid)?
        };
        configs.push((topic, bytes));
        Ok(())
    };
    match feature {
        Feature::DoorLock => {
            let template=format!(r#"{{"id":"{ieee}","endpoint":{},"action":"{{{{ value }}}}","transaction":"{ieee}-{}-{{{{ now().isoformat() }}}}"}}"#,ep.endpoint,ep.endpoint);
            add("lock","door_lock",json!({"name":"Lock","command_topic":format!("{base}/bridge/v1/request/device/lock"),
                "command_template":template, "payload_lock":"lock","payload_unlock":"unlock","state_locked":"locked",
                "state_unlocked":"unlocked","value_template":"{{ value_json.lock_state if value_json.get('lock_state') in ['locked','unlocked'] else 'None' if 'lock_state' in value_json else '' }}",
                "qos":1,"retain":false,"optimistic":false}))?;
        }
        Feature::RemoteTrigger { cluster, name } => {
            let property = format!("zcl_{:02x}_command", ep.endpoint);
            let subtype = format!("ep_{:02x}_{name}", ep.endpoint);
            add("device_automation", &subtype, json!({
                "automation_type":"trigger","type":"action","subtype":subtype,
                "topic":format!("{base}/{}",ep.device_name),"payload":cluster.to_string(),
                "value_template":format!("{{{{ value_json.get('{property}', {{}}).get('cluster', '') }}}}"),
            }))?;
        }
        Feature::BasicIo(io) => {
            let key = io.property();
            let mut spec = json!({"name":key,"value_template":format!("{{% if '{key}' in value_json %}}{{{{ value_json['{key}'] }}}}{{% endif %}}")});
            let binary = (0x000f..=0x0011).contains(&io.cluster());
            if binary {
                spec["value_template"]=json!(format!("{{% if '{key}' in value_json %}}{{{{ 'ON' if value_json['{key}'] else 'OFF' }}}}{{% endif %}}"));
            }
            if let Some(unit)=io.unit {spec["unit_of_measurement"]=json!(unit);}
            let mut domain = if binary { "binary_sensor" } else { "sensor" };
            if io.writable {
                spec["command_topic"]=json!(format!("{state_topic}/set/{key}"));
                domain = if binary { "switch" }
                else if let Some(count)=io.count.filter(|n| *n>1) {
                    spec["min"]=json!(1);spec["max"]=json!(count);spec["step"]=json!(1);"number"
                } else if let (Some((min,max)),Some(step))=(io.bounds,io.resolution) {
                    spec["min"]=json!(min);spec["max"]=json!(max);spec["step"]=json!(step);"number"
                } else {
                    // An arbitrary default slider range would misrepresent this output.
                    spec.as_object_mut().unwrap().remove("unit_of_measurement");
                    "text"
                };
            }
            add(domain,key,spec)?;
        }
        Feature::LastSeen { timestamp } => {
            let mut payload = json!({
                "name":"Last seen","enabled_by_default":false,"icon":"mdi:clock",
                "value_template":"{% if 'last_seen' in value_json %}{{ value_json.last_seen }}{% endif %}"
            });
            if timestamp { payload["device_class"] = json!("timestamp"); }
            add("sensor", "last_seen", payload)?;
        }
        Feature::ReportedEffect(options) => {
            if options.is_empty() { return Err(SubmitError::Invalid); }
            add("select", "effect", json!({
                "name":"Effect", "options":options, "icon":"mdi:palette", "enabled_by_default":false,
                "command_topic":format!("{state_topic}/set/effect"),
                "value_template":"{{ value_json.get('effect', states(entity_id)) }}"
            }))?;
        }
        Feature::Effect(options) => {
            if options.is_empty() { return Err(SubmitError::Invalid); }
            add("select", "effect", json!({
                "name":"Effect","icon":"mdi:palette","enabled_by_default":false,"options":options,
                "command_topic":format!("{state_topic}/set/effect")
            }))?;
        }
        Feature::Identify => add("button","identify",json!({
            "name":"Identify","device_class":"identify",
            "command_topic":format!("{state_topic}/set/identify"),"payload_press":"identify"
        }))?,
        Feature::FirmwareUpdate => add(
            "update",
            "update",
            json!({
                "name":Value::Null,"device_class":"firmware",
                "command_topic":format!("{base}/bridge/request/device/ota_update/update"),
                "payload_install":json!({"id":ieee}).to_string(),
                "value_template":"{% if 'update' in value_json %}{{ {'latest_version': value_json['update'].latest_version | string, 'installed_version': value_json['update'].installed_version | string, 'in_progress': value_json['update'].state == 'updating', 'update_percentage': value_json['update'].get('progress')} | to_json }}{% endif %}"
            }),
        )?,
        Feature::Barrier{partial} => {
            let action=format!("{state_topic}/set/barrier_command");
            let mut config=json!({"name":"Barrier","command_topic":action,"payload_open":"open",
                "payload_close":"close","payload_stop":"stop","position_topic":state_topic,
                "position_template":"{{ value_json.barrier_position | default(none) }}",
                "value_template":"{{ 'opening' if value_json.get('barrier_moving_state') == 'opening' else 'closing' if value_json.get('barrier_moving_state') == 'closing' else 'closed' if value_json.get('barrier_position') == 0 else 'open' if value_json.get('barrier_position') is number else 'unknown' }}",
                "optimistic":false});
            if partial {config["set_position_topic"]=json!(action);}
            add("cover","barrier",config)?;
        }
        Feature::Cover | Feature::StandardCover(_) => {
            use zigbee_devices::cover::Axis;
            let standard = match feature { Feature::StandardCover(cover) => Some(cover), _ => None };
            let set_topic = format!("{state_topic}/set");
            let mut config = json!({
                "name":Value::Null,"command_topic":set_topic,
                "value_template":"{{ value_json.get('state', '') }}",
                "state_open":"OPEN","state_closed":"CLOSE",
                "payload_open":r#"{"state":"OPEN"}"#,"payload_close":r#"{"state":"CLOSE"}"#,
                "payload_stop":r#"{"state":"STOP"}"#,"optimistic":false
            });
            if standard.is_none_or(|cover| cover.reads(Axis::Lift)) {
                config["position_topic"] = json!(state_topic);
                config["position_template"] = json!("{{ value_json.get('position', '') }}");
            }
            if standard.is_none_or(|cover| cover.sets(Axis::Lift)) {
                config["set_position_topic"] = json!(set_topic);
                config["set_position_template"] = json!(r#"{"position": {{ position }} }"#);
            }
            if standard.is_some_and(|cover| cover.reads(Axis::Tilt)) {
                config["tilt_status_topic"] = json!(state_topic);
                config["tilt_status_template"] = json!("{{ value_json.get('tilt', '') }}");
            }
            if standard.is_some_and(|cover| cover.sets(Axis::Tilt)) {
                config["tilt_command_topic"] = json!(set_topic);
                config["tilt_command_template"] = json!(r#"{"tilt": {{ tilt_position }} }"#);
                config["payload_stop_tilt"] = json!(r#"{"state":"STOP"}"#);
            }
            add("cover", "cover", config)?;
        }
        Feature::Contact => {
            // MQTT contact=true means closed; Home Assistant opening=on means open.
            add("binary_sensor", "contact", json!({"name":"contact", "device_class":"opening",
                "payload_on":"false", "payload_off":"true",
                "value_template":"{{ value_json.get('contact', '') | lower }}"}))?;
        }
        Feature::Binary(property, class) => {
            if !safe_key(property) || !safe_key(class) {
                return Err(SubmitError::Invalid);
            }
            add(
                "binary_sensor",
                property,
                json!({"name":property,"device_class":class,
                    "payload_on":"true","payload_off":"false","value_template":format!("{{{{ value_json.get('{property}', '') | lower }}}}")}),
            )?;
        }
        Feature::Numeric { property, unit, device_class, state_class } => {
            add("sensor", property, numeric_sensor(property, unit, device_class, state_class)?)?;
        }
        Feature::Measurement { property, unit } => {
            add("sensor", property, numeric_sensor(property, unit, None, Some("measurement"))?)?;
        }
        Feature::StringBinary { property, off, on, writable } => {
            if !safe_key(property) || off.is_empty() || on.is_empty() || off == on {
                return Err(SubmitError::Invalid);
            }
            let mut spec = json!({"name":property, "payload_on":on, "payload_off":off,
                "value_template":format!("{{% if '{property}' in value_json %}}{{{{ value_json['{property}'] }}}}{{% endif %}}")});
            if writable {
                spec["command_topic"] = json!(format!("{state_topic}/set/{property}"));
                spec["state_on"] = json!(on);
                spec["state_off"] = json!(off);
                add("switch", &format!("switch_{property}"), spec)?;
            } else {
                add("binary_sensor", property, spec)?;
            }
        }
        Feature::BooleanSetting(property) => {
            if !safe_key(property) {
                return Err(SubmitError::Invalid);
            }
            add(
                "switch",
                &format!("switch_{property}"),
                json!({"name":property,
                    "value_template":format!("{{{{ value_json.get('{property}', '') | lower }}}}"),
                    "command_topic":format!("{state_topic}/set/{property}"),
                    "payload_on":"true","payload_off":"false",
                    "state_on":"true","state_off":"false"}),
            )?;
        }
        Feature::CompositeSensor(property) => {
            if !safe_key(property) {
                return Err(SubmitError::Invalid);
            }
            let template = format!(
                "{{% if '{property}' in value_json %}}{{{{ value_json['{property}'] | string | truncate(254, true, '', 0) }}}}{{% else %}}{{{{ states(entity_id) }}}}{{% endif %}}"
            );
            add("sensor", property, json!({"name":property, "value_template":template}))?;
        }
        Feature::WriteOnlyText(property) => {
            if !safe_key(property) { return Err(SubmitError::Invalid); }
            add("text",property,json!({"name":property,"command_topic":format!("{state_topic}/set/{property}")}))?;
        }
        Feature::Thermostat {modes,setpoint,min,max} => {
            let field = |key| format!("{{{{ value_json.get('{key}', '') }}}}");
            add("climate","climate",json!({
                "name":null, "modes":modes, "optimistic":false, "temperature_unit":"C",
                "min_temp":min, "max_temp":max, "temp_step":0.01,
                "current_temperature_topic":state_topic, "current_temperature_template":field("local_temperature"),
                "mode_state_topic":state_topic,"mode_state_template":field("system_mode"),
                "mode_command_topic":format!("{state_topic}/set/system_mode"),
                "temperature_state_topic":state_topic,"temperature_state_template":field(setpoint),
                "temperature_command_topic":format!("{state_topic}/set/{setpoint}"),
            }))?;
        }
        Feature::TextSensor(property) => {
            if !safe_key(property) {
                return Err(SubmitError::Invalid);
            }
            add(
                "sensor",
                property,
                json!({"name":property,
                    "value_template":format!("{{{{ value_json.get('{property}', states(entity_id)) }}}}")}),
            )?;
        }
        Feature::Number {
            property,
            min,
            max,
            step,
            unit,
        } => {
            if !(safe_key(property)
                && min.is_finite()
                && max.is_finite()
                && step.is_finite()
                && min < max
                && step > 0.)
            {
                return Err(SubmitError::Invalid);
            }
            let mut spec = json!({"name":property,"min":min,"max":max,"step":step,
                    "command_topic":format!("{state_topic}/set/{property}"),
                    "value_template":format!("{{{{ value_json.get('{property}', '') }}}}")});
            if let Some(unit) = unit {
                spec["unit_of_measurement"] = json!(unit);
            }
            add("number", property, spec)?;
        }
        Feature::Select { property, options } => {
            if !safe_key(property) || options.is_empty() {
                return Err(SubmitError::Invalid);
            }
            add(
                "select",
                property,
                json!({"name":property,"options":options,
                    "command_topic":format!("{state_topic}/set/{property}"),
                    "value_template":format!("{{{{ value_json.get('{property}', '') }}}}")}),
            )?;
        }
        Feature::Switch => add(
            "switch",
            "switch",
            json!({
                "name":Value::Null, "command_topic":format!("{state_topic}/set"),
                "value_template":"{{ value_json.get('state', '') }}",
                "payload_on":r#"{"state":"ON"}"#, "payload_off":r#"{"state":"OFF"}"#,
                "state_on":"ON","state_off":"OFF","optimistic":false
            }),
        )?,
        Feature::Valve => add(
            "valve",
            "valve",
            json!({
                "name":Value::Null,"command_topic":format!("{state_topic}/set"),
                "value_template":"{{ value_json.get('state', '') }}",
                "state_open":"ON","state_closed":"OFF",
                "payload_open":r#"{"state":"ON"}"#,"payload_close":r#"{"state":"OFF"}"#,
                "optimistic":false
            }),
        )?,
        Feature::Actions(_) | Feature::Button => {
            let actions = match feature {
                Feature::Actions(a) => a,
                _ => &["single", "double", "hold"],
            };
            if actions.is_empty() || actions.len() > 256 {
                return Err(SubmitError::Invalid);
            }
            for &action in actions {
                if !safe_key(action) {
                    return Err(SubmitError::Invalid);
                }
                add(
                    "device_automation",
                    &format!("action_{action}"),
                    json!({
                        "automation_type":"trigger", "type":"action", "subtype":format!("{action}{suffix}"),
                        "payload":action,"topic":format!("{state_topic}/action")
                    }),
                )?;
            }
        }
        Feature::Light {
            effects,
            xy,
            hs,
            brightness,
            color_temperature,
            mired_range,
        } => {
            let mut light = json!({
                "name":Value::Null,"schema":"json","command_topic":format!("{state_topic}/set"),
                "brightness":brightness,"optimistic":false
            });
            if !effects.is_empty() {
                light["effect"] = json!(true);
                light["effect_list"] = json!(effects);
            }
            if brightness {
                light["brightness_scale"] = json!(254);
            }
            let mut modes = Vec::new();
            if xy {
                modes.push("xy");
            }
            if hs {
                modes.push("hs");
            }
            if color_temperature {
                modes.push("color_temp");
            }
            if !modes.is_empty() {
                light["supported_color_modes"] = json!(modes);
            }
            if color_temperature && let Some((min, max)) = mired_range {
                if min == 0 || min > max || max >= 0xff00 {
                    return Err(SubmitError::Invalid);
                }
                light["min_mireds"] = json!(min);
                light["max_mireds"] = json!(max);
            }
            add("light", "light", light)?;
        }
        Feature::Sensor(sensor) => {
            let (property, class, unit) = sensor.metadata();
            // HA numeric sensors ignore an empty string when another property changes.
            let mut payload = json!({
                "name":property, "unit_of_measurement":unit,
                "state_class":if matches!(sensor, Sensor::Energy | Sensor::PumpLifetimeRunningHours) {"total_increasing"} else {"measurement"},
                "value_template":format!("{{{{ value_json.get('{property}', '') }}}}")
            });
            if let Sensor::TemperatureWithForceUpdate(force) = sensor {
                payload["force_update"] = json!(force);
            }
            if let Some(class) = class {
                payload["device_class"] = json!(class);
            }
            if matches!(sensor, Sensor::LinkQuality) {
                payload["enabled_by_default"] = json!(false);
                payload["icon"] = json!("mdi:signal");
            }
            add("sensor", property, payload)?;
        }
    }
    Ok(configs)
}

impl Publisher {
    pub(super) fn enqueue_configs(
        &self,
        configs: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<(), SubmitError> {
        if configs
            .iter()
            .any(|(topic, body)| !super::publish_fits(topic, body.len()))
        {
            return Err(SubmitError::Invalid);
        }
        let mut pending = self.0.pending.lock().unwrap();
        if pending.closed {
            return Err(SubmitError::Closed);
        }
        if pending.store.is_none() && configs.len() > self.0.capacity {
            return Err(SubmitError::Invalid);
        }
        let additional = configs
            .iter()
            .filter(|(k, _)| !pending.configs.contains_key(k))
            .count();
        if pending.store.is_none() && pending.configs.len() + additional > self.0.capacity {
            return Err(SubmitError::Full);
        }
        if let Some(store) = &pending.store {
            // A batch may replace a deletion before it ever reaches the broker.
            // Persist the final obligation for each affected topic.
            let mut changes = std::collections::BTreeMap::new();
            for (topic, bytes) in configs.iter() {
                if bytes.is_empty()
                    || changes.contains_key(topic.as_str())
                    || pending.desired_configs.get(topic).is_some_and(Vec::is_empty)
                {
                    changes.insert(topic.as_str(), bytes.is_empty());
                }
            }
            let changes: Vec<_> = changes.into_iter().collect();
            store
                .update_tombstones(&self.retained_scope(), &changes)
                .map_err(|error| {
                    eprintln!("Saving MQTT discovery obligations: {error:#}");
                    SubmitError::Storage
                })?;
            for (topic, bytes) in configs.drain(..) {
                pending.desired_configs.insert(topic.clone(), bytes);
                pending.queue_config(&topic);
            }
        } else {
            pending
                .configs
                .extend(configs.drain(..).map(|(topic, bytes)| {
                    (
                        topic,
                        super::Retained {
                            bytes,
                            receipt: None,
                        },
                    )
                }));
        }
        drop(pending);
        self.0.wake.notify_one();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::{MqttOptions, channel};

    #[test]
    fn force_update_keeps_duplicate_observations_even_before_the_publisher_runs() {
        let (out, _worker, _) = channel(MqttOptions::new("test", "localhost", 1883), "zb", 2).unwrap();
        let force = Sensor::TemperatureWithForceUpdate(true);
        let regular = Sensor::TemperatureWithForceUpdate(false);
        regular.report(&out, "sensor", json!(21.5)).unwrap();
        regular.report(&out, "sensor", json!(21.5)).unwrap();
        force.report(&out, "sensor", json!(21.5)).unwrap();
        force.report(&out, "sensor", json!(21.5)).unwrap();
        assert_eq!(force.report(&out, "sensor", json!(21.5)), Err(SubmitError::Full));
        let mut queue = out.0.pending.lock().unwrap();
        assert_eq!(queue.dirty.len(), 1);
        assert_eq!(queue.queued.len(), 3);
        assert!(std::iter::from_fn(|| queue.take()).all(|report| report.payload == br#"{"temperature":21.5}"#));
    }
}
