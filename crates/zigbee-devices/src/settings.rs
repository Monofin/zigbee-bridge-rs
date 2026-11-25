//! Typed, explicitly reviewed settings. A successful write does not create a
//! device observation. All exposed ranges and units come from the product definition.
mod irrigation;
mod overload;
use irrigation::Cycle;
pub use irrigation::Program;
use crate::{Endpoint, lighting::Request};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use std::{collections::BTreeMap, sync::OnceLock};
use zigbee_zcl::{
    Direction, Frame, FrameType, Header, attributes::AttributeValue, foundation::Payload,
    value::Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    Scalar(i32),
    Irrigation(Program),
    VerifyProtection,
}

#[derive(Deserialize)]
struct Catalogue {
    fields: BTreeMap<String, Field>,
    products: Vec<Product>,
}
#[derive(Deserialize)]
struct Product {
    manufacturer: String,
    model: String,
    #[serde(default = "primary_endpoint")]
    endpoint: u8,
    device_type: u16,
    servers: Vec<u16>,
    fields: Vec<String>,
}
#[derive(Deserialize)]
pub struct Field {
    pub property: Option<String>,
    pub cluster: u16,
    pub attribute: u16,
    pub manufacturer_code: Option<u16>,
    #[serde(default = "all")]
    access: u8,
    #[serde(default)]
    pub boolean: bool,
    #[serde(default)]
    string_binary: bool,
    bitmap_member: Option<String>,
    cycle: Option<Cycle>,
    overload_limits: Option<overload::Limits>,
    invalid: Option<i64>,
    #[serde(rename = "type")]
    ty: u8,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub unit: Option<String>,
    pub device_class: Option<String>,
    pub state_class: Option<String>,
    scale: Option<u16>,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    codes: Vec<u16>,
}
fn primary_endpoint() -> u8 {
    1
}
fn all() -> u8 {
    7
}
#[derive(Clone)]
pub struct Settings {
    fields: std::sync::Arc<[(&'static str, &'static Field)]>,
}
impl Settings {
    pub fn resolve(maker: &str, model: &str, ep: &Endpoint<'_>, dev: u16) -> Result<Option<Self>> {
        static DATA: OnceLock<Result<Catalogue, String>> = OnceLock::new();
        let data = DATA
            .get_or_init(|| {
                serde_json::from_str(include_str!("../profiles/settings.json"))
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if ep.profile != 260 {
            return Ok(None);
        }
        for product in &data.products {
            if product.manufacturer == maker
                && product.model == model
                && product.endpoint == ep.id
                && product.device_type == dev
                && product.servers.iter().all(|s| ep.servers.contains(s))
            {
                ensure!(
                    product.fields.iter().all(|k| data.fields.contains_key(k)),
                    "unknown setting in profile"
                );
                for key in &product.fields {
                    let field = &data.fields[key];
                    ensure!(
                        !field.string_binary
                            || (field.choices.len() == 2
                                && field.codes.len() == 2
                                && field.choices[0] != field.choices[1]
                                && field.codes[0] != field.codes[1]),
                        "invalid binary choices in settings profile"
                    );
                }
                let fields = product.fields.iter().filter_map(|key| {
                    let field = &data.fields[key];
                    ep.servers.contains(&field.cluster).then_some((field.property.as_deref().unwrap_or(key), field))
                }).collect();
                return Ok(Some(Self { fields }));
            }
        }
        Ok(None)
    }
    pub fn fields(&self) -> impl Iterator<Item = (&'static str, &'static Field)> + '_ {
        self.fields.iter().copied()
    }
    pub fn field(&self, key: &str) -> Option<(&'static str, &'static Field)> {
        self.fields().find(|(k, _)| *k == key)
    }
}
impl Field {
    /// Off/on strings in MQTT order (the wire values are in the same order).
    pub fn binary_labels(&self) -> Option<(&str, &str)> {
        if self.string_binary
            && let [off, on] = self.choices.as_slice()
        {
            Some((off, on))
        } else {
            None
        }
    }
    fn boolean_codes(&self) -> Option<[u16; 2]> {
        match self.codes.as_slice() {
            [] => Some([0, 1]),
            [off, on] if off != on => Some([*off, *on]),
            _ => None,
        }
    }
    pub fn is_composite(&self) -> bool {
        self.bitmap_member.is_some() || self.cycle.is_some() || self.overload_limits.is_some()
    }
    pub fn writable(&self) -> bool {
        self.access & 2 != 0
    }
    pub fn readable(&self) -> bool {
        self.access & 4 != 0
    }
    pub fn decode(&self, value: Value<'_>) -> Option<Json> {
        if value.data_type().id() != self.ty {
            return None;
        }
        if self.overload_limits.is_some() {
            return Some(Json::Object(overload::report(value.as_bytes())?));
        }
        if let Some(cycle) = self.cycle {
            return cycle.report(value.as_bytes());
        }
        let n = match (self.ty, value.as_bytes()) {
            (0x30 | 0x10 | 0x18 | 0x20, [n]) => i64::from(*n),
            (0x21, [a, b]) => i64::from(u16::from_le_bytes([*a, *b])),
            (0x23, [a, b, c, d]) => i64::from(u32::from_le_bytes([*a, *b, *c, *d])),
            (0x29, [a, b]) => i64::from(i16::from_le_bytes([*a, *b])),
            _ => return None,
        };
        self.confirmed(n)
    }
    /// The same numerical value is used for accepted writes and reports.
    pub fn confirmed(&self, n: i64) -> Option<Json> {
        if self.invalid == Some(n) {
            return None;
        }
        if let Some(key) = &self.bitmap_member {
            return Some(json!({(key): n & 1 != 0}));
        }
        if self.boolean {
            let [off, on] = self.boolean_codes()?;
            return if n == i64::from(off) {
                Some(json!(false))
            } else if n == i64::from(on) {
                Some(json!(true))
            } else {
                None
            };
        }
        if !self.codes.is_empty() {
            let i = self.codes.iter().position(|v| i64::from(*v) == n)?;
            return Some(json!(self.choices.get(i)?));
        }
        let x = n as f64 / f64::from(self.scale?);
        (self.min.is_none_or(|a| x >= a) && self.max.is_none_or(|b| x <= b)).then(|| json!(x))
    }
    pub fn confirmed_write(&self, value: Written) -> Option<Json> {
        match value {
            Written::Scalar(n) => self.confirmed(i64::from(n)),
            Written::Irrigation(program) => Some(self.cycle?.state(program)),
            Written::VerifyProtection => None,
        }
    }
    pub fn plan(&self, ep: u8, seq: u8, set: Option<&Json>) -> Result<Request> {
        self.prepare(ep, seq, set).map(|(request, _)| request)
    }
    fn encode_scalar(&self, v: &Json) -> Result<(Vec<u8>, Written)> {
        let n = if let Some(key) = &self.bitmap_member {
            let object = v.as_object().context("setting requires an object")?;
            ensure!(object.len() == 1, "unexpected option fields");
            i32::from(
                object
                    .get(key)
                    .and_then(Json::as_bool)
                    .context("expected boolean option")?,
            )
        } else if self.boolean {
            let setting = v.as_bool().context("setting requires boolean")?;
            i32::from(self.boolean_codes().context("invalid boolean codes")?[usize::from(setting)])
        } else if !self.choices.is_empty() {
            let text = v.as_str().context("setting requires a named option")?;
            let i = self
                .choices
                .iter()
                .position(|s| s == text)
                .context("unknown option")?;
            i32::from(*self.codes.get(i).context("missing option code")?)
        } else {
            let x = v.as_f64().context("setting requires a number")?;
            ensure!(
                x >= self.min.context("missing lower bound")?
                    && x <= self.max.context("missing upper bound")?,
                "setting outside supported range"
            );
            let step = self.step.context("missing step")?;
            let units = x / step;
            ensure!(
                (units - units.round()).abs() < 1e-7,
                "setting not representable at required precision"
            );
            (x * f64::from(self.scale.context("missing attribute scale")?)).round() as i32
        };
        let wire = match self.ty {
            0x21 => u16::try_from(n)?.to_le_bytes().to_vec(),
            0x29 => i16::try_from(n)?.to_le_bytes().to_vec(),
            0x30 | 0x10 | 0x18 | 0x20 => vec![u8::try_from(n)?],
            _ => anyhow::bail!("unhandled setting type"),
        };
        Ok((wire, Written::Scalar(n)))
    }
    /// Return the validated setting with the request. The coordinator may
    /// publish it only after a correlated successful Write Attributes response.
    pub fn prepare(
        &self,
        ep: u8,
        seq: u8,
        set: Option<&Json>,
    ) -> Result<(Request, Option<Written>)> {
        ensure!(
            if set.is_some() {
                self.writable()
            } else {
                self.readable()
            },
            "operation not advertised for setting"
        );
        let (command, bytes, written) = if let Some(v) = set {
            let (wire, written) = if let Some(limits) = self.overload_limits {
                (limits.write(v)?, Written::VerifyProtection)
            } else if let Some(cycle) = self.cycle {
                let program = cycle.parse(v)?;
                (program.encode(), Written::Irrigation(program))
            } else {
                self.encode_scalar(v)?
            };
            let value = Value::from_bytes(self.ty.try_into()?, &wire)?;
            let cmd = Payload::WriteAttributes(vec![AttributeValue {
                id: self.attribute,
                value,
            }]);
            (cmd.command() as u8, cmd.encode()?, Some(written))
        } else {
            let cmd = Payload::ReadAttributes(vec![self.attribute]);
            (cmd.command() as u8, cmd.encode()?, None)
        };
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::General,
                manufacturer: self.manufacturer_code,
                direction: Direction::ClientToServer,
                disable_default_response: self.cycle.is_none() && self.overload_limits.is_none(),
                sequence: seq,
                command_id: command,
            },
            payload: &bytes,
        }
        .encode_into(&mut frame);
        Ok((
            Request {
                endpoint: ep,
                cluster: self.cluster,
                frame,
            },
            written,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn colour_options_are_a_named_bitmap_member_and_use_bit_zero() {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 8, 768],
            clients: &[],
        };
        let s = Settings::resolve("IKEA of Sweden", "TRADFRI bulb E14 WS 470lm", &ep, 268)
            .unwrap()
            .unwrap();
        let (_, field) = s.field("color_options").unwrap();
        assert_eq!(
            field
                .plan(1, 9, Some(&json!({"execute_if_off":true})))
                .unwrap()
                .frame,
            [0x10, 9, 2, 15, 0, 0x18, 1]
        );
        assert_eq!(field.plan(1, 9, None).unwrap().frame, [0x10, 9, 0, 15, 0]);
        assert_eq!(
            field.decode(Value::from_bytes(0x18.try_into().unwrap(), &[2]).unwrap()),
            Some(json!({"execute_if_off":false}))
        );
        assert_eq!(
            field.decode(Value::from_bytes(0x18.try_into().unwrap(), &[3]).unwrap()),
            Some(json!({"execute_if_off":true}))
        );
        for value in [
            json!(true),
            json!({"execute_if_off":"false"}),
            json!({"other":false}),
        ] {
            assert!(field.plan(1, 1, Some(&value)).is_err());
        }
    }
    #[test]
    fn startup_onoff_is_distinct_from_current_onoff_and_from_tuya_startup() {
        let ep = Endpoint {
            id: 11,
            profile: 260,
            servers: &[0, 6, 8],
            clients: &[],
        };
        let doc = Settings::resolve("Signify Netherlands B.V.", "LWE004", &ep, 257)
            .unwrap()
            .unwrap();
        let (_, field) = doc.field("power_on_behavior").unwrap();
        assert_eq!(
            field.plan(11, 3, Some(&json!("previous"))).unwrap().frame,
            [0x10, 3, 2, 3, 0x40, 0x30, 255]
        );
        assert_eq!(
            field.plan(11, 4, None).unwrap().frame,
            [0x10, 4, 0, 3, 0x40]
        );
        assert_eq!(
            field.decode(Value::from_bytes(0x30.try_into().unwrap(), &[2]).unwrap()),
            Some(json!("toggle"))
        );
        assert!(
            Settings::resolve(
                "Signify Netherlands B.V.",
                "LWE004",
                &Endpoint { id: 1, ..ep },
                257
            )
            .unwrap()
            .is_none()
        );
        assert!(
            Settings::resolve("Other", "LWE004", &ep, 257)
                .unwrap()
                .is_none()
        );
        assert!(
            Settings::resolve(
                "Signify Netherlands B.V.",
                "LWE004",
                &Endpoint {
                    servers: &[0, 6],
                    ..ep
                },
                257
            )
            .unwrap()
            .is_none()
        );
        let plug = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 0xe000, 0xe001],
            clients: &[],
        };
        let tuya = Settings::resolve("_TZ3000_o4cjetlm", "TS0001", &plug, 256)
            .unwrap()
            .unwrap();
        let (_, field) = tuya.field("power_on_behavior").unwrap();
        assert_eq!(
            field.plan(1, 3, Some(&json!("previous"))).unwrap().frame,
            [0x10, 3, 2, 2, 0x80, 0x30, 2]
        );
        assert!(field.plan(1, 3, Some(&json!("toggle"))).is_err());
    }
    #[test]
    fn relay_uses_signed_radio_power_levels_and_half_seconds() {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 64529],
            clients: &[],
        };
        let settings = Settings::resolve("SONOFF", "ZBMINIR2", &ep, 2)
            .unwrap()
            .unwrap();
        let (_, radio) = settings.field("turbo_mode").unwrap();
        assert_eq!(
            radio.plan(1, 1, Some(&json!(true))).unwrap().frame,
            [0x10, 1, 2, 0x12, 0, 0x29, 20, 0]
        );
        assert_eq!(
            radio.decode(Value::from_bytes(0x29.try_into().unwrap(), &[9, 0]).unwrap()),
            Some(json!(false))
        );
        assert_eq!(
            radio.decode(Value::from_bytes(0x29.try_into().unwrap(), &[1, 0]).unwrap()),
            None
        );
        let (_, delay) = settings.field("delayed_power_on_time").unwrap();
        assert_eq!(
            delay.plan(1, 7, Some(&json!(1.5))).unwrap().frame,
            [0x10, 7, 2, 0x15, 0, 0x21, 3, 0]
        );
        assert!(delay.plan(1, 7, Some(&json!(1.25))).is_err());
        let (_, trigger) = settings.field("external_trigger_mode").unwrap();
        assert_eq!(
            trigger
                .plan(1, 7, Some(&json!("following(on)")))
                .unwrap()
                .frame,
            [0x10, 7, 2, 0x16, 0, 0x20, 130]
        );
    }
    #[test]
    fn screen_settings_keep_signed_type_step_and_variant_scope() {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 1, 1026, 1029, 64529],
            clients: &[],
        };
        let screen = Settings::resolve("SONOFF", "SNZB-02D", &ep, 770)
            .unwrap()
            .unwrap();
        let (_, field) = screen.field("temperature_calibration").unwrap();
        assert_eq!(
            field.plan(1, 9, Some(&json!(-1.5))).unwrap().frame,
            [0x10, 9, 2, 3, 32, 0x29, 0x6a, 0xff]
        );
        assert!(field.plan(1, 9, Some(&json!(0.01))).is_err());
        assert!(field.plan(1, 9, Some(&json!(60))).is_err());
        let (_, units) = screen.field("temperature_units").unwrap();
        assert_eq!(
            units.plan(1, 1, Some(&json!("fahrenheit"))).unwrap().frame,
            [0x10, 1, 2, 7, 0, 0x21, 1, 0]
        );
        let sensor = Settings::resolve("eWeLink", "SNZB-02P", &ep, 770)
            .unwrap()
            .unwrap();
        assert!(sensor.field("temperature_units").is_none());
    }
}
