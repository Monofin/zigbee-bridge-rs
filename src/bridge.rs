//! Bind interviewed endpoint functions to MQTT properties and radio operations.
pub(crate) mod addresses;
mod bindings;
mod ace;
mod discovery_ownership;
mod basic_io;
mod remote;
mod standard;
mod environment;
mod last_seen;
mod level_config;
mod equipment;
mod weekly_schedule;
pub use last_seen::LastSeen;
mod household;
mod hvac;
mod ias;
mod enrollment;
mod tuya;
mod ember;
mod admin;
mod endpoint_admin;
mod door_lock;
mod joining;
mod reporting;
mod responses;
mod poll;
mod time;
mod updates;
mod zdo;
pub mod ota;
pub use ember::{CommandReceiver, serve, serve_managed};
pub use admin::Management;

use crate::{
    interview_log::Descriptor,
    mqtt::discovery::{self, Sensor},
};
use anyhow::{Result, ensure};
use std::collections::BTreeMap;
use zigbee_devices::{
    Endpoint,
    light_index::{Access, LightIndex},
    lighting::{Lighting, LightingQuirk, Property, Request},
    measurement::Attribute,
    tuya::Button,
};

type Identity = (u64, u8);

#[derive(Clone, Copy)]
enum Read {
    BasicIo { cluster: u16, write: bool },
    Standard(standard::Attribute),
    Hvac(hvac::Operation),
    Equipment(equipment::Operation),
    Setpoints(hvac::SetpointOperation),
    Ias(zigbee_devices::ias::Attribute),
    Cover(zigbee_devices::cover::Axis),
    LevelConfig(level_config::Operation),
    TuyaLamp(zigbee_devices::tuya::lamp::Command),
    LoopStop,
    ConfirmOnOff,
    Refresh(&'static str),
    Light(Property),
    Startup(zigbee_devices::standard_controls::StartupRequest),
    Household(household::Query),
    Sensor(Attribute),
    Environment(zigbee_devices::measurement::environment::Reading, bool),
}
type Planned = (Request, Option<Read>);
const SENSORS: [(&str, Attribute); 5] = [
    ("battery", Attribute::Battery),
    ("voltage", Attribute::Voltage),
    ("temperature", Attribute::Temperature),
    ("humidity", Attribute::Humidity),
    ("pressure", Attribute::Pressure),
];

fn sensor_kind(attr: Attribute) -> Sensor {
    match attr {
        Attribute::Battery => Sensor::Battery,
        Attribute::Voltage => Sensor::Voltage,
        Attribute::Temperature => Sensor::Temperature,
        Attribute::Humidity => Sensor::Humidity,
        Attribute::Pressure => Sensor::Pressure,
    }
}

#[derive(Clone)]
struct Binding {
    preserve_raw_reports: bool,
    device_availability:bool,
    temperature_force_update: Option<bool>,
    name: String,
    controls: zigbee_devices::standard_controls::Controls,
    level_state: zigbee_devices::standard_controls::level_config::State,
    confirmed_hue_scene: Option<u8>,
    device_name: String,
    access: Option<(Access, Access)>,
    io: Vec<zigbee_devices::basic_io::Channel>,
    measured: Vec<Sensor>,
    environment: Vec<zigbee_devices::measurement::environment::Reading>,
    zone: Option<zigbee_devices::ias::Zone>,
    hvac: zigbee_devices::hvac::Capabilities,
    equipment_commands: equipment::Commands,
    equipment: Vec<&'static zigbee_devices::equipment::Field>,
    household: household::Household,
    cover: Option<zigbee_devices::cover::Cover>,
    datapoints: Option<zigbee_devices::tuya::household::Device>,
    descriptor: Descriptor,
    button: Option<Button>,
    is_light: bool,
    declares_ota: bool,
    quirk: Option<LightingQuirk>,
    color: Option<u16>,
    limits: Option<(u16, u16)>,
}

fn endpoint(id: u8, d: &Descriptor) -> Endpoint<'_> {
    Endpoint {
        id,
        profile: d.profile,
        servers: &d.servers,
        clients: &d.clients,
    }
}

impl Binding {
    fn configured_sensor(&self, sensor: Sensor) -> Sensor {
        match (sensor, self.temperature_force_update) {
            (Sensor::Temperature, Some(force)) => Sensor::TemperatureWithForceUpdate(force),
            _ => sensor,
        }
    }
    fn discovery(&self, id: Identity) -> discovery::Endpoint<'_> {
        discovery::Endpoint {
            device_availability:self.device_availability,
            ieee: id.0,
            endpoint: id.1,
            device_name: &self.device_name,
            topic_name: &self.name,
        }
    }
    fn permits(&self, property: Property, allow: impl FnOnce(Access) -> bool) -> bool {
        match (self.access, property) {
            (None, _) => true,
            (Some((a, _)), Property::State) | (Some((_, a)), Property::Brightness) => allow(a),
            (Some(_), Property::ColorTemperature | Property::Color) => false,
        }
    }
    fn light<'a>(&self, ep: &'a Endpoint<'a>) -> Result<Lighting<'a>> {
        ensure!(self.is_light, "endpoint has no light controls");
        let mut lamp = Lighting::new(ep, self.color);
        if let Some(quirk) = self.quirk {
            lamp = lamp.with_quirk(quirk);
        }
        if let Some(range)=self.controls.temperature_via_xy { return lamp.with_temperature_via_xy(range); }
        match self.limits {
            Some((min, max)) => lamp.with_temperature_range(min, max),
            None => Ok(lamp),
        }
    }
}

#[derive(Clone,Default)]
pub struct Bindings {
    receiver_on_when_idle: BTreeMap<u64, bool>,
    complete_descriptors: std::collections::BTreeSet<u64>,
    all_standard_reports: bool,
    lock_controls: std::collections::BTreeSet<Identity>,
    network_security_members: std::collections::BTreeSet<u64>,
    tuya_gateway_internet: Option<bool>,
    last_seen: LastSeen,
    availability: std::sync::Arc<crate::availability::Catalogue>,
    temperature_options: BTreeMap<u64, bool>,
    vibration_timeouts: BTreeMap<u64, u32>,
    deadlines: household::Deadlines,
    light_index: Option<std::sync::Arc<LightIndex>>,
    devices: BTreeMap<Identity, Binding>,
    topics: BTreeMap<String, Identity>,
    peers: BTreeMap<u64,(String,BTreeMap<u8,Descriptor>)>,
}

#[cfg(test)]
mod coverage_replay;

#[cfg(test)]
mod standard_controls_test;

#[cfg(test)]
mod cover_test;
