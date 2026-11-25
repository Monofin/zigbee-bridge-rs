//! Exact TS0601 profiles from the pinned Tuya definition. Most products use a
//! DP map; legacy curtain motors also derive position/state and change enums
//! by manufacturer (legacy.ts getCoverStateEnums and tuya_cover_control).
use super::{Datapoint, decode};
use crate::{Endpoint, lighting::Request};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::OnceLock};
use zigbee_zcl::{Direction, Frame, FrameType, Header};

pub const CLUSTER: u16 = 0xef00;

fn server_command(frame: Frame<'_>) -> Option<u8> {
    let h = frame.header;
    (h.frame_type == FrameType::ClusterSpecific && h.direction == Direction::ServerToClient
        && h.manufacturer.is_none()).then_some(h.command_id)
}

/// Report shapes eligible to wait for source identity. Device::accepts still
/// checks alternate report support after the sender has been identified.
pub fn unsolicited_report(frame: Frame<'_>) -> bool {
    matches!(server_command(frame), Some(2 | 5 | 6 | 0x0a))
}

#[derive(Clone, Copy, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Cover,
    Soil,
    Radar,
    MeteredSwitch,
    DoubleSocket,
}
#[derive(Deserialize)]
struct Model {
    family: Family,
    service_set: Option<String>,
    time_epoch: Option<super::TimeEpoch>,
    close: Option<u8>,
    open: Option<u8>,
    stop: Option<u8>,
}
#[derive(Deserialize)]
struct Data {
    manufacturers: BTreeMap<String, Model>,
    service_sets: BTreeMap<String, Vec<super::Service>>,
    soil: Vec<Field>,
    radar: Vec<Field>,
    metered: Vec<Field>,
    metered_switch: Vec<Field>,
    double_socket: Vec<Field>,
}
#[derive(Clone, Deserialize)]
pub struct Field {
    pub key: String,
    dp: u8,
    #[serde(default)]
    signed: bool,
    #[serde(default)]
    boolean: bool,
    #[serde(default)]
    pub choices: Vec<String>,
    pub switch: Option<[String; 2]>,
    #[serde(default)]
    pub writable: bool,
    #[serde(default = "one")]
    scale: u32,
    #[serde(default)]
    pub min: f64,
    pub max: Option<f64>,
    pub step: Option<f64>,
}
fn one() -> u32 {
    1
}
fn data() -> Result<&'static Data> {
    static DATA: OnceLock<Result<Data, String>> = OnceLock::new();
    DATA.get_or_init(|| {
        let mut data: Data =
            serde_json::from_str(include_str!("../../profiles/ts0601-household.json"))
                .map_err(|e| e.to_string())?;
        for model in data.manufacturers.values() {
            if let Some(key) = &model.service_set && !data.service_sets.contains_key(key) {
                return Err(format!("unknown Tuya service set: {key}"));
            }
        }
        data.metered_switch.extend_from_slice(&data.metered);
        data.double_socket.append(&mut data.metered);
        Ok(data)
    })
    .as_ref()
    .map_err(|e| anyhow::anyhow!("invalid TS0601 profile: {e}"))
}

#[derive(Clone, Copy)]
pub struct Device {
    model: &'static Model,
    fields: &'static [Field],
    services: &'static [super::Service],
}

impl Device {
    pub fn resolve(maker: &str, model: &str, ep: &Endpoint<'_>) -> Result<Option<Self>> {
        if model != "TS0601" || ep.id != 1 || ep.profile != 0x104 || !ep.servers.contains(&CLUSTER)
        {
            return Ok(None);
        }
        let data = data()?;
        Ok(data.manufacturers.get(maker).map(|model| Self {
            model,
            services: model.service_set.as_ref().map_or(&[][..], |key| &data.service_sets[key]),
            fields: match model.family {
                Family::Cover => &[],
                Family::Soil => &data.soil,
                Family::Radar => &data.radar,
                Family::MeteredSwitch => &data.metered_switch,
                Family::DoubleSocket => &data.double_socket,
            },
        }))
    }
    pub fn family(self) -> Family {
        self.model.family
    }
    pub fn supports(self, service: super::Service) -> bool {
        self.services.contains(&service)
    }
    pub fn time_epoch(self) -> Option<super::TimeEpoch> {
        self.model.time_epoch
    }
    pub fn fields(self) -> &'static [Field] {
        self.fields
    }

    /// A report can contain multiple DPs. Structural corruption invalidates
    /// the frame. Unknown IDs and unsupported wire types leave other DPs intact.
    pub fn accepts(self, frame: Frame<'_>) -> bool {
        match server_command(frame) {
            Some(1 | 2) => true,
            Some(5 | 6 | 0x0a) => self.supports(super::Service::AlternateReports),
            _ => false,
        }
    }

    pub fn observe(self, frame: Frame<'_>) -> Result<Map<String, Value>> {
        let mut result = Map::new();
        if !self.accepts(frame) {
            return Ok(result);
        }
        let records = frame.payload.get(2..).context("missing Tuya transaction")?;
        for dp in decode(records)? {
            if self.family() == Family::Cover {
                self.cover_report(&dp, &mut result);
                continue;
            }
            if let Some(field) = self.fields.iter().find(|f| f.dp == dp.id)
                && let Some(v) = field.read(&dp)
            {
                result.insert(field.key.clone(), v);
            }
        }
        Ok(result)
    }
    fn cover_report(self, dp: &Datapoint<'_>, out: &mut Map<String, Value>) {
        let Some(number) = number(dp) else {
            return;
        };
        match dp.id {
            2 | 3 => {
                // Both supported manufacturers use the upstream non-inverted default.
                let pos = number & 255;
                // A DP2 alone does not establish how long this motor runs.
                // Keep the confirmed position without synthesizing a stop timer.
                if pos <= 100 {
                    out.insert("position".into(), json!(pos));
                    out.insert(
                        "state".into(),
                        json!(if pos == 0 { "CLOSE" } else { "OPEN" }),
                    );
                }
            }
            105 if number <= 255 => {
                out.insert("motor_speed".into(), json!(number));
                // The two motors report speed on DP 105. Direction is a write
                // option and cannot be inferred from a movement or a set.
                out.insert("options".into(), json!({"motor_speed":number}));
            }
            _ => {}
        }
    }
    pub fn set(self, key: &str, value: &Value, seq: u8) -> Result<Vec<Request>> {
        if self.family() == Family::Cover && key == "options" {
            let opts = value
                .as_object()
                .context("cover options must be an object")?;
            ensure!(
                !opts.is_empty()
                    && opts
                        .keys()
                        .all(|k| matches!(k.as_str(), "reverse_direction" | "motor_speed")),
                "unknown cover option"
            );
            let mut steps = Vec::new();
            for property in ["reverse_direction", "motor_speed"] {
                if let Some(v) = opts.get(property) {
                    steps.push(self.single(
                        key,
                        &json!({(property):v}),
                        seq.wrapping_add(steps.len() as u8),
                    )?);
                }
            }
            return Ok(steps);
        }
        Ok(vec![self.single(key, value, seq)?])
    }
    fn single(self, key: &str, value: &Value, seq: u8) -> Result<Request> {
        // Legacy increments its MCU transaction, modern converters send 1.
        let transaction = if self.family() == Family::Cover {
            u16::from(seq)
        } else {
            1
        };
        let mut payload = transaction.to_le_bytes().to_vec();
        if self.family() == Family::Cover {
            match key {
                "state" => {
                    let state = value
                        .as_str()
                        .context("expected OPEN, CLOSE or STOP")?
                        .to_ascii_lowercase();
                    let n = match state.as_str() {
                        "open" => self.model.open,
                        "close" => self.model.close,
                        "stop" => self.model.stop,
                        _ => None,
                    }
                    .context("unknown cover state")?;
                    record(&mut payload, 1, 4, &[n]);
                }
                "position" => {
                    let n = bounded(value, 100)?;
                    record(&mut payload, 2, 2, &n.to_be_bytes());
                }
                "options" => {
                    let options = value
                        .as_object()
                        .context("cover options must be an object")?;
                    ensure!(
                        !options.is_empty()
                            && options
                                .keys()
                                .all(|k| matches!(k.as_str(), "reverse_direction" | "motor_speed")),
                        "unknown cover option"
                    );
                    if let Some(v) = options.get("reverse_direction") {
                        record(
                            &mut payload,
                            5,
                            4,
                            &[u8::from(
                                v.as_bool().context("reverse_direction must be boolean")?,
                            )],
                        );
                    }
                    if let Some(v) = options.get("motor_speed") {
                        record(&mut payload, 105, 2, &bounded(v, 255)?.to_be_bytes());
                    }
                }
                _ => bail!("unsupported cover property"),
            }
        } else {
            let field = self
                .fields
                .iter()
                .find(|f| f.key == key && f.writable)
                .context("Tuya property has no write mapping")?;
            if let Some(labels) = &field.switch {
                let label = value.as_str().context("expected a Tuya switch label")?;
                let code = labels
                    .iter()
                    .position(|candidate| candidate == label)
                    .context("unknown Tuya switch label")?;
                record(&mut payload, field.dp, 1, &[code as u8]);
            } else {
                let n = field.write(value)?;
                // Both the numeric lookup used by temperatureUnit and the numeric
                // radar converters take the JS number branch of tuya.ts datapoints.
                record(&mut payload, field.dp, 2, &n.to_be_bytes());
            }
        }
        let mut frame = vec![];
        Frame {
            header: Header {
                frame_type: FrameType::ClusterSpecific,
                manufacturer: None,
                direction: Direction::ClientToServer,
                disable_default_response: true,
                sequence: seq,
                command_id: 0,
            },
            payload: &payload,
        }
        .encode_into(&mut frame);
        Ok(Request {
            endpoint: 1,
            cluster: CLUSTER,
            frame,
        })
    }
}
fn number(dp: &Datapoint<'_>) -> Option<u32> {
    if dp.kind != 2 {
        return None;
    }
    Some(u32::from_be_bytes(dp.bytes.try_into().ok()?))
}
fn bounded(v: &Value, max: u32) -> Result<u32> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n <= max)
        .context("Tuya value outside supported range")
}
fn record(out: &mut Vec<u8>, dp: u8, kind: u8, data: &[u8]) {
    out.extend([dp, kind]);
    out.extend((data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}
impl Field {
    fn read(&self, dp: &Datapoint<'_>) -> Option<Value> {
        if let Some(labels) = &self.switch {
            return match (dp.kind, dp.bytes) {
                (1, [n @ 0..=1]) => Some(json!(labels[usize::from(*n)])),
                _ => None,
            };
        }
        if self.boolean || !self.choices.is_empty() {
            let n = match (dp.kind, dp.bytes) {
                (4, [x]) => u32::from(*x),
                _ => number(dp)?,
            };
            return if self.boolean {
                match n {
                    0 => Some(json!(false)),
                    1 => Some(json!(true)),
                    _ => None,
                }
            } else {
                self.choices.get(n as usize).map(|s| json!(s))
            };
        }
        let raw = number(dp)?;
        if !self.signed && raw > i32::MAX as u32 {
            return None;
        }
        let n = if self.signed {
            f64::from(raw as i32)
        } else {
            f64::from(raw)
        } / f64::from(self.scale);
        if (!self.signed && n < self.min) || self.max.is_some_and(|max| n > max) {
            return None;
        }
        if self.scale == 1 {
            Some(if self.signed {
                json!(raw as i32)
            } else {
                json!(raw)
            })
        } else {
            Some(json!(n))
        }
    }
    fn write(&self, v: &Value) -> Result<u32> {
        if !self.choices.is_empty() {
            let s = v.as_str().context("expected Tuya choice")?;
            return self
                .choices
                .iter()
                .position(|a| a == s)
                .map(|n| n as u32)
                .context("unknown Tuya choice");
        }
        let n = v.as_f64().context("expected a numeric Tuya setting")?;
        ensure!(
            n.is_finite() && n >= self.min && self.max.is_some_and(|max| n <= max),
            "Tuya setting outside supported bounds"
        );
        let scaled = n * f64::from(self.scale);
        ensure!(
            (scaled - scaled.round()).abs() < 1e-7,
            "Tuya setting not representable"
        );
        Ok(scaled.round() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ep() -> Endpoint<'static> {
        Endpoint {
            id: 1,
            profile: 0x104,
            servers: &[0xef00],
            clients: &[],
        }
    }
    fn matched(maker: &str) -> Device {
        Device::resolve(maker, "TS0601", &ep()).unwrap().unwrap()
    }
    fn report(payload: &[u8]) -> Frame<'_> {
        Frame {
            header: Header {
                frame_type: FrameType::ClusterSpecific,
                manufacturer: None,
                direction: Direction::ServerToClient,
                disable_default_response: false,
                sequence: 7,
                command_id: 2,
            },
            payload,
        }
    }
    #[test]
    fn only_datapoint_reports_can_wait_for_identity() {
        let mut frame = report(&[]);
        for (command, expected) in [(2, true), (5, true), (6, true), (10, true),
                                    (1, false), (0x11, false), (0x24, false), (0x25, false)] {
            frame.header.command_id = command;
            assert_eq!(unsolicited_report(frame), expected);
        }
        frame.header.command_id = 2;
        frame.header.direction = Direction::ClientToServer;
        assert!(!unsolicited_report(frame));
        frame.header.direction = Direction::ServerToClient;
        frame.header.manufacturer = Some(0x1234);
        assert!(!unsolicited_report(frame));
    }

    #[test]
    fn curtains_use_different_stop_and_close_codes_and_no_fake_fallback() -> Result<()> {
        assert!(Device::resolve("unknown", "TS0601", &ep())?.is_none());
        assert!(Device::resolve("_TZE200_rddyvrci", "TS0041", &ep())?.is_none());
        for (maker, close, stop) in [("_TZE200_cowvfni3", 0, 1), ("_TZE200_rddyvrci", 1, 0)] {
            let dev = matched(maker);
            for (name, code) in [("OPEN", 2), ("CLOSE", close), ("STOP", stop)] {
                let s = dev.set("state", &json!(name), 7)?;
                assert_eq!(s[0].frame, &[0x11, 7, 0, 7, 0, 1, 4, 0, 1, code]);
            }
            let s = dev.set("position", &json!(35), 8)?;
            assert_eq!(s[0].frame, &[0x11, 8, 0, 8, 0, 2, 2, 0, 4, 0, 0, 0, 35]);
            let options = dev.set(
                "options",
                &json!({"motor_speed":125,"reverse_direction":true}),
                8,
            )?;
            assert_eq!(options.len(), 2);
            assert_eq!(&options[0].frame[5..], &[5, 4, 0, 1, 1]);
            assert_eq!(&options[1].frame[5..], &[105, 2, 0, 4, 0, 0, 0, 125]);
            let observed = dev.observe(report(&[0, 0, 105, 2, 0, 4, 0, 0, 0, 125]))?;
            assert_eq!(observed["options"], json!({"motor_speed":125}));
            assert_eq!(observed["motor_speed"], 125);
            assert!(
                dev.observe(report(&[0, 0, 5, 4, 0, 1, 1]))?
                    .get("options")
                    .is_none()
            );
            let got = dev.observe(report(&[0, 0, 3, 2, 0, 4, 0, 0, 0, 0]))?;
            assert_eq!(got["state"], "CLOSE");
            assert_eq!(got["position"], 0);
            assert!(dev.observe(report(&[0, 0, 1, 4, 0, 1, 1]))?.is_empty());
            assert!(dev.set("position", &json!(101), 7).is_err());
        }
        Ok(())
    }
    #[test]
    fn metered_switches_share_measurements_but_keep_their_own_relay_map() -> Result<()> {
        let outlet = matched("_TZE284_lq0ffndf");
        for maker in ["_TZE200_gbagoilo", "_TZE284_xnwxmj8z", "_TZE284_lq0ffndf"] {
            let device = matched(maker);
            let common = [
                0x6a, 0x01, 21, 2, 0, 4, 0, 0, 0x04, 0xd2, // 1.234 A
                22, 2, 0, 4, 0, 0, 0x05, 0x19, // 130.5 W
                23, 2, 0, 4, 0, 0, 0x09, 0x03, // 230.7 V
                16, 1, 0, 1, 0, 14, 4, 0, 1, 2,
            ];
            assert_eq!(
                Value::Object(device.observe(report(&common))?),
                json!({
                    "current":1.234, "power":130.5, "voltage":230.7,
                    "backlight_mode":"OFF", "power_on_behavior":"previous"
                })
            );
            // The pinned VALUE converter reads a signed, big-endian 32-bit quantity.
            assert_eq!(
                device.observe(report(&[0, 0, 22, 2, 0, 4, 0xff, 0xff, 0xff, 0x85]))?["power"],
                -12.3
            );
            assert_eq!(
                device.set("backlight_mode", &json!("ON"), 9)?[0].frame,
                [0x11, 9, 0, 1, 0, 16, 1, 0, 1, 1]
            );
            assert_eq!(
                device.set("power_on_behavior", &json!("previous"), 9)?[0].frame,
                [0x11, 9, 0, 1, 0, 14, 2, 0, 4, 0, 0, 0, 2]
            );
            assert!(device.set("power", &json!(10), 9).is_err());
            assert!(device.set("backlight_mode", &json!("TOGGLE"), 9).is_err());
            if device.family() == Family::MeteredSwitch {
                assert_eq!(
                    Value::Object(
                        device.observe(report(&[0, 0, 1, 1, 0, 1, 1, 7, 2, 0, 4, 0, 0, 1, 44]))?
                    ),
                    json!({"state":"ON","countdown":300})
                );
                assert_eq!(
                    device.set("state", &json!("OFF"), 8)?[0].frame,
                    [0x11, 8, 0, 1, 0, 1, 1, 0, 1, 0]
                );
                assert_eq!(
                    device.set("countdown", &json!(43200), 8)?[0].frame,
                    [0x11, 8, 0, 1, 0, 7, 2, 0, 4, 0, 0, 0xa8, 0xc0]
                );
                assert!(device.set("state_right", &json!("ON"), 7).is_err());
                assert!(device.set("countdown", &json!(43201), 7).is_err());
                assert!(device.set("countdown", &json!(0.5), 7).is_err());
            }
        }
        let both = [
            0, 0, 1, 1, 0, 1, 0, 2, 1, 0, 1, 1, 101, 1, 0, 1, 1, 20, 2, 0, 4, 0, 0, 0x30, 0x39,
        ];
        assert_eq!(
            Value::Object(outlet.observe(report(&both))?),
            json!({
                "state_left":"OFF", "state_right":"ON", "child_lock":"LOCK", "energy":12.345
            })
        );
        for id in [1, 2, 5, 6, 0x0a] {
            let mut frame = report(&both);
            frame.header.command_id = id;
            assert_eq!(outlet.observe(frame)?["child_lock"], "LOCK");
        }
        for (id, direction) in [
            (0, Direction::ServerToClient),
            (2, Direction::ClientToServer),
        ] {
            let mut frame = report(&both);
            frame.header.command_id = id;
            frame.header.direction = direction;
            assert!(outlet.observe(frame)?.is_empty());
        }
        for (key, val, id, code) in [
            ("state_left", "ON", 1, 1),
            ("state_right", "OFF", 2, 0),
            ("child_lock", "UNLOCK", 101, 0),
        ] {
            let request = &outlet.set(key, &json!(val), 8)?[0];
            assert_eq!((request.endpoint, request.cluster), (1, CLUSTER));
            assert_eq!(request.frame, [0x11, 8, 0, 1, 0, id, 1, 0, 1, code]);
        }
        assert!(outlet.set("countdown", &json!(60), 10).is_err());
        assert!(outlet.observe(report(&both[..both.len() - 1])).is_err());
        assert!(
            outlet
                .observe(report(&[
                    0, 0, 1, 1, 0, 1, 2, 2, 4, 0, 1, 1, 101, 1, 0, 2, 0, 1, 23, 2, 0, 1, 0,
                ]))?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn metered_fingerprint_requires_model_maker_endpoint_profile_and_server() -> Result<()> {
        for (maker, model, endpoint) in [
            ("_TZE204_gbagoilo", "TS0601", ep()), // Similar ID, legacy relay with a different definition.
            ("", "TS0601", ep()),
            ("_TZE200_gbagoilo", "TS0001", ep()),
            ("_TZE200_gbagoilo", "TS0601", Endpoint { id: 2, ..ep() }),
            (
                "_TZE200_gbagoilo",
                "TS0601",
                Endpoint {
                    profile: 0xc05e,
                    ..ep()
                },
            ),
            (
                "_TZE200_gbagoilo",
                "TS0601",
                Endpoint {
                    servers: &[],
                    clients: &[CLUSTER],
                    ..ep()
                },
            ),
        ] {
            assert!(Device::resolve(maker, model, &endpoint)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn soil_and_radar_treat_same_dp_differently_and_validate_complete_frame() -> Result<()> {
        let soil = matched("_TZE284_oitavov2");
        let radar = matched("_TZE204_qasjif9e");
        let bytes = [
            1, 0, 3, 2, 0, 4, 0, 0, 0, 45, 5, 2, 0, 4, 255, 255, 255, 250, 15, 2, 0, 4, 0, 0, 0, 95,
        ];
        let s = soil.observe(report(&bytes))?;
        assert_eq!(s["soil_moisture"], 45);
        assert_eq!(s["temperature"], -6);
        assert_eq!(s["battery"], 95);
        assert_eq!(radar.observe(report(&bytes))?["minimum_range"], 0.45);
        assert!(soil.observe(report(&bytes[..bytes.len() - 1])).is_err());
        assert_eq!(
            radar.observe(report(&[1, 0, 1, 4, 0, 1, 1]))?["presence"],
            true
        );
        for dp in [12, 104] {
            assert_eq!(
                radar.observe(report(&[0, 0, dp, 2, 0, 4, 0, 0, 1, 0]))?["illuminance"],
                256
            );
        }
        assert_eq!(
            radar.set("maximum_range", &json!(4.65), 9)?[0].frame,
            [0x11, 9, 0, 1, 0, 4, 2, 0, 4, 0, 0, 1, 209]
        );
        assert!(radar.set("maximum_range", &json!(9.6), 9).is_err());
        assert!(soil.set("temperature", &json!(22), 9).is_err());
        assert_eq!(
            &soil.set("temperature_unit", &json!("fahrenheit"), 9)?[0].frame[5..],
            &[9, 2, 0, 4, 0, 0, 0, 1]
        );
        Ok(())
    }
}
