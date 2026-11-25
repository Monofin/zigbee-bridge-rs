//! Standard lighting operations for a single interviewed endpoint.
//! The coordinator owns address resolution, transaction sequences and delivery.
mod innr;
pub use innr::LightingQuirk;

use crate::{Endpoint, State, command, observe};
use anyhow::{Result, bail, ensure};
use serde_json::Value as Json;
use std::ops::RangeInclusive;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header,
    attributes::Status,
    cluster::{ColorControl, Command, LevelControl},
    foundation::{GeneralCommand, Payload},
    value::Value,
};

mod color;
mod temperature_via_xy;
const COLOR_CONTROL: u16 = 0x0300;
const COLOR_TEMPERATURE_CAPABILITY: u16 = 1 << 4;
const VALID_MIREDS: RangeInclusive<u16> = 1..=0xfeff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    State,
    Brightness,
    ColorTemperature,
    Color,
}

impl Property {
    pub const fn name(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Brightness => "brightness",
            Self::ColorTemperature => "color_temp", Self::Color=>"color",
        }
    }

    fn address(self) -> (u16, u16) {
        match self {
            Self::State => (6, 0),
            Self::Brightness => (8, 0),
            Self::ColorTemperature => (COLOR_CONTROL, 7), Self::Color=>(COLOR_CONTROL,8),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [Self::State, Self::Brightness, Self::ColorTemperature,Self::Color]
            .into_iter()
            .find(|p| p.name() == name)
    }

    /// Decode one absolute set value. The caller interprets compound payloads.
    pub fn setting(self, value: &Json) -> Result<Setting> {
        Ok(match self {
            Self::Color=>color::setting(value)?,
            Self::State => match value.as_str() {
                Some(v) if v.eq_ignore_ascii_case("on") => Setting::State(true),
                Some(v) if v.eq_ignore_ascii_case("off") => Setting::State(false),
                _ => bail!("state must be ON or OFF"),
            },
            Self::Brightness => {
                let Some(level @ 0..=254) = value.as_u64() else {
                    bail!("brightness must be an integer from 0 to 254")
                };
                Setting::Brightness(level as u8)
            }
            Self::ColorTemperature => {
                let Some(mireds) = value
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|n| VALID_MIREDS.contains(n))
                else {
                    bail!("color_temp must be a valid integer in mireds")
                };
                Setting::ColorTemperature(mireds)
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    State(bool),
    Brightness(u8),
    ColorTemperature(u16),
    Xy(u16,u16),
    Hs(u8,u8),
}

impl Setting {
    pub const fn property(self) -> Property {
        match self {
            Self::State(_) => Property::State,
            Self::Brightness(_) => Property::Brightness,
            Self::ColorTemperature(_) => Property::ColorTemperature,
            Self::Xy(..)|Self::Hs(..)=>Property::Color,
        }
    }

    pub fn value(self) -> Json {
        match self {
            Self::State(on) => Json::from(if on { "ON" } else { "OFF" }),
            Self::Brightness(level) => Json::from(level),
            Self::ColorTemperature(mireds) => Json::from(mireds),
            Self::Xy(x,y)=>serde_json::json!({"x":f64::from(x)/65536.,"y":f64::from(y)/65536.}),
            Self::Hs(h,s)=>serde_json::json!({"hue":f64::from(h)*360./254.,"saturation":f64::from(s)*100./254.}),
        }
    }
}

/// The caller addresses this endpoint on the previously resolved membership.
#[derive(Debug)]
pub struct Request {
    pub endpoint: u8,
    pub cluster: u16,
    pub frame: Vec<u8>,
}

pub struct Lighting<'a> {
    endpoint: &'a Endpoint<'a>,
    color_capabilities: Option<u16>,
    mireds: RangeInclusive<u16>,
    quirk: Option<LightingQuirk>,
    temperature_via_xy: bool,
}

impl<'a> Lighting<'a> {
    /// ColorCapabilities is attribute 0x400a from this endpoint's Color Control server.
    pub fn new(endpoint: &'a Endpoint<'a>, color_capabilities: Option<u16>) -> Self {
        Self {
            endpoint,
            color_capabilities,
            mireds: VALID_MIREDS,
            quirk: None,
            temperature_via_xy: false,
        }
    }

    pub fn with_quirk(mut self, quirk: LightingQuirk) -> Self {
        self.quirk = Some(quirk);
        self
    }

    pub fn with_temperature_via_xy(mut self, range: (u16, u16)) -> Result<Self> {
        self.temperature_via_xy = self.xy();
        self.with_temperature_range(range.0, range.1)
    }

    /// A report from this lamp retains its XY mode. The temperature is a
    /// derived, clamped value for consumers displaying a mired slider.
    pub fn temperature_from_color(&self, color: Setting) -> Option<Setting> {
        self.temperature_via_xy.then(|| temperature_via_xy::mireds(color, &self.mireds)).flatten()
    }

    /// Restrict outbound setpoints to the lamp's reported physical limits.
    pub fn with_temperature_range(mut self, min: u16, max: u16) -> Result<Self> {
        ensure!(
            min <= max && VALID_MIREDS.contains(&max),
            "invalid temperature limits"
        );
        self.mireds = min.max(1)..=max;
        Ok(self)
    }

    pub fn supports(&self, property: Property) -> bool {
        if property==Property::Color {return self.xy() || self.hs();}
        let (cluster, _) = property.address();
        self.endpoint.has_standard_server(cluster)
            && (property != Property::ColorTemperature || self.temperature_via_xy
                || self
                    .color_capabilities
                    .is_some_and(|c| c != u16::MAX && c & COLOR_TEMPERATURE_CAPABILITY != 0))
    }

    fn request(
        &self,
        cluster: u16,
        kind: FrameType,
        command_id: u8,
        sequence: u8,
        payload: &[u8],
    ) -> Request {
        let mut frame = Vec::with_capacity(3 + payload.len());
        Frame {
            header: Header {
                frame_type: kind,
                manufacturer: None,
                direction: Direction::ClientToServer,
                disable_default_response: false,
                sequence,
                command_id,
            },
            payload,
        }
        .encode_into(&mut frame);
        Request {
            endpoint: self.endpoint.id,
            cluster,
            frame,
        }
    }

    /// Absolute brightness couples level to On/Off, as a Z2M brightness set does.
    /// A state-only OFF can fade to zero. A faded ON also needs a target level.
    pub fn set(&self, setting: Setting, transition_ds: u16, sequence: u8) -> Result<Request> {
        ensure!(
            self.supports(setting.property()),
            "lighting property unavailable on this endpoint"
        );
        let level = |level| {
            Command::LevelControl(LevelControl::MoveToLevelWithOnOff {
                level,
                transition_time: transition_ds,
                option_mask: 0,
                option_override: 0,
            })
        };
        let op = match setting {
            Setting::State(on) if transition_ds == 0 => command(self.endpoint, State::On(on), 0)?,
            Setting::State(false) if self.supports(Property::Brightness) => level(0),
            Setting::State(_) => bail!("this state transition needs an explicit target brightness"),
            Setting::Brightness(brightness) => {
                ensure!(brightness < 255, "reserved brightness");
                ensure!(
                    self.supports(Property::State),
                    "brightness set requires On/Off"
                );
                level(
                    self.quirk
                        .map_or(brightness, |q| q.absolute_level(brightness)),
                )
            }
            Setting::Xy(x,y)=>{ensure!(self.xy() && x<=0xfeff && y<=0xfeff,"unsupported XY colour");
                Command::ColorControl(ColorControl::MoveToColor{color_x:x,color_y:y,transition_time:transition_ds,options_mask:0,options_override:0})}
            Setting::Hs(h,s)=>{ensure!(self.hs() && h<255 && s<255,"unsupported hue/saturation");
                Command::ColorControl(ColorControl::MoveToHueAndSaturation{hue:h,saturation:s,transition_time:transition_ds,options_mask:0,options_override:0})}
            Setting::ColorTemperature(mireds) => {
                ensure!(
                    self.mireds.contains(&mireds),
                    "setpoint outside temperature range"
                );
                if self.temperature_via_xy {
                    let (x,y) = temperature_via_xy::coordinates(mireds)
                        .ok_or_else(||anyhow::anyhow!("setpoint outside the XY lookup"))?;
                    return self.set(Setting::Xy(x, y), transition_ds, sequence);
                }
                Command::ColorControl(ColorControl::MoveToColorTemperature {
                    color_temperature: mireds,
                    transition_time: transition_ds,
                    options_mask: 0,
                    options_override: 0,
                })
            }
        };
        let key = op.key();
        Ok(self.request(
            key.cluster,
            FrameType::ClusterSpecific,
            key.command,
            sequence,
            &op.encode()?,
        ))
    }

    pub fn get(&self, property: Property, sequence: u8) -> Result<Request> {
        ensure!(
            self.supports(property),
            "lighting property unavailable on this endpoint"
        );
        let (cluster, attr) = property.address();
        let mut attrs=if property==Property::ColorTemperature && self.temperature_via_xy {
            vec![8,3,4]
        } else {vec![attr]};
        if property==Property::Color {
            if self.xy(){attrs.extend([3,4]);}
            if self.hs(){attrs.extend([0,1]);}
        }
        let read=Payload::ReadAttributes(attrs);
        Ok(self.request(
            cluster,
            FrameType::General,
            read.command() as u8,
            sequence,
            &read.encode()?,
        ))
    }

    fn attribute(&self, cluster: u16, id: u16, value: Value<'_>) -> Option<Setting> {
        if cluster == COLOR_CONTROL
            && !self.temperature_via_xy
            && id == 7
            && self.supports(Property::ColorTemperature)
            && value.data_type().id() == 0x21
            && let [lo, hi] = *value.as_bytes()
        {
            let mireds = u16::from_le_bytes([lo, hi]);
            return VALID_MIREDS
                .contains(&mireds)
                .then_some(Setting::ColorTemperature(mireds));
        }
        match observe(self.endpoint, cluster, id, None, value)? {
            State::On(on) => Some(Setting::State(on)),
            State::Brightness(n) => Some(Setting::Brightness(n)),
        }
    }

    /// Accept unsolicited reports. For read responses use read_response.
    pub fn observations(&self, cluster: u16, frame: Frame<'_>) -> Result<Vec<Setting>> {
        if frame.header.direction != Direction::ServerToClient
            || frame.header.manufacturer.is_some()
            || frame.header.general_command() != Some(GeneralCommand::ReportAttributes)
            || !self.endpoint.has_standard_server(cluster)
        {
            return Ok(Vec::new());
        }
        let Payload::ReportAttributes(values) = Payload::decode(frame)? else {
            unreachable!()
        };
        let color=(cluster==COLOR_CONTROL).then(||self.coordinates(values.iter().map(|v|(v.id,v.value)))).flatten();
        Ok(values
            .into_iter()
            .filter_map(|v| self.attribute(cluster, v.id, v.value)).chain(color)
            .collect())
    }

    /// The caller first checks the originating membership and source endpoint.
    pub fn read_response(
        &self,
        property: Property,
        sequence: u8,
        cluster: u16,
        frame: Frame<'_>,
    ) -> Result<Option<Setting>> {
        let (expected, attr) = property.address();
        if expected != cluster
            || !self.supports(property)
            || frame.header.sequence != sequence
            || frame.header.direction != Direction::ServerToClient
            || frame.header.manufacturer.is_some()
            || frame.header.general_command() != Some(GeneralCommand::ReadAttributesResponse)
        {
            return Ok(None);
        }
        let Payload::ReadAttributesResponse(records) = Payload::decode(frame)? else {
            unreachable!()
        };
        if property==Property::Color || (property==Property::ColorTemperature && self.temperature_via_xy) {
            return Ok(self.coordinates(records.into_iter().filter_map(|r|match r.result {Status::Success(v)=>Some((r.id,v)),_=>None})));

        }
        let mut matching = records.into_iter().filter(|r| r.id == attr);
        let Some(record) = matching.next() else {
            return Ok(None);
        };
        ensure!(
            matching.next().is_none(),
            "duplicate requested lighting attribute"
        );
        Ok(match record.result {
            Status::Success(value) => self.attribute(cluster, attr, value),
            Status::Error(_) => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn light() -> Endpoint<'static> {
        Endpoint {
            id: 4,
            profile: 0x104,
            servers: &[6, 8, COLOR_CONTROL],
            clients: &[],
        }
    }
    #[test]
    fn brightness_has_on_off_semantics_and_never_creates_an_observation() {
        let ep = light();
        let lamp = Lighting::new(&ep, Some(0x10));
        for level in [0, 1, 254] {
            let req = lamp.set(Setting::Brightness(level), 12, 91).unwrap();
            assert_eq!((req.endpoint, req.cluster), (4, 8));
            let frame = Frame::decode(&req.frame).unwrap();
            assert_eq!(frame.header.sequence, 91);
            assert_eq!(frame.header.command_id, 4);
            assert_eq!(frame.payload, [level, 12, 0, 0, 0]);
            assert!(lamp.observations(8, frame).unwrap().is_empty());
        }
        assert!(lamp.set(Setting::Brightness(255), 0, 1).is_err());
        let client = Endpoint {
            servers: &[],
            clients: &[6, 8],
            ..ep
        };
        assert!(
            Lighting::new(&client, None)
                .set(Setting::State(false), 0, 1)
                .is_err()
        );
    }
    #[test]
    fn temperature_requires_advertised_mode_and_respects_physical_limits() {
        let ep = light();
        for caps in [None, Some(8), Some(0xffff)] {
            assert!(
                Lighting::new(&ep, caps)
                    .get(Property::ColorTemperature, 2)
                    .is_err()
            );
        }
        let lamp = Lighting::new(&ep, Some(0x18))
            .with_temperature_range(153, 500)
            .unwrap();
        assert!(lamp.set(Setting::ColorTemperature(501), 0, 1).is_err());
        let req = lamp.set(Setting::ColorTemperature(250), 3, 4).unwrap();
        assert_eq!((req.endpoint, req.cluster), (4, COLOR_CONTROL));
        assert_eq!(&req.frame, [1, 4, 10, 250, 0, 3, 0, 0, 0].as_slice());
        let read = lamp.get(Property::ColorTemperature, 4).unwrap();
        assert_eq!(read.frame, [0, 4, 0, 7, 0]);
        let report = Frame::decode(&[8, 1, 10, 7, 0, 0x21, 0xfa, 0]).unwrap();
        assert_eq!(
            lamp.observations(COLOR_CONTROL, report).unwrap(),
            [Setting::ColorTemperature(250)]
        );
    }
    #[test]
    fn observations_require_a_report_or_the_matching_read() {
        let ep = light();
        let lamp = Lighting::new(&ep, None);
        let response = Frame::decode(&[8, 17, 1, 0, 0, 0, 0x10, 1]).unwrap();
        assert!(lamp.observations(6, response).unwrap().is_empty());
        assert_eq!(
            lamp.read_response(Property::State, 17, 6, response)
                .unwrap(),
            Some(Setting::State(true))
        );
        assert_eq!(
            lamp.read_response(Property::State, 18, 6, response)
                .unwrap(),
            None
        );
        for bytes in [
            &[8, 17, 10, 0, 0, 0x20, 255][..],
            &[0, 17, 10, 0, 0, 0x20, 0],
            &[12, 0, 0, 17, 10, 0, 0, 0x20, 1],
        ] {
            assert!(
                lamp.observations(8, Frame::decode(bytes).unwrap())
                    .unwrap()
                    .is_empty()
            );
        }
    }
    #[test]
    fn absolute_mqtt_values_round_trip_without_coercion() {
        for setting in [
            Setting::State(false),
            Setting::Brightness(0),
            Setting::ColorTemperature(250),
        ] {
            assert_eq!(
                setting.property().setting(&setting.value()).unwrap(),
                setting
            );
        }
        assert!(Property::Brightness.setting(&Json::from(-1)).is_err());
        assert!(Property::Brightness.setting(&Json::from("100")).is_err());
        assert!(
            Property::ColorTemperature
                .setting(&Json::from(65535))
                .is_err()
        );
    }
}
