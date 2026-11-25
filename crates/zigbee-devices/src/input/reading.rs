//! Readable attributes on standard input clusters. An unsolicited report can
//! identify a value from a profiled endpoint without proving it accepts reads.
use crate::lighting::Request;
use anyhow::Result;
use serde_json::Value as Json;
use zigbee_zcl::{Direction, Frame, FrameType, Header, foundation::Payload, value::Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    Occupancy,
    Illuminance,
}

impl Reading {
    pub fn from_property(name: &str) -> Option<Self> {
        match name {
            "occupancy" => Some(Self::Occupancy),
            "illuminance" => Some(Self::Illuminance),
            _ => None,
        }
    }

    pub fn cluster(self) -> u16 {
        match self {
            Self::Occupancy => 0x0406,
            Self::Illuminance => 0x0400,
        }
    }

    pub fn read(self, endpoint: u8, sequence: u8) -> Result<Request> {
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::General,
                direction: Direction::ClientToServer,
                manufacturer: None,
                disable_default_response: true,
                sequence,
                command_id: 0,
            },
            payload: &Payload::ReadAttributes(vec![0]).encode()?,
        }
        .encode_into(&mut frame);
        Ok(Request {
            endpoint,
            cluster: self.cluster(),
            frame,
        })
    }

    pub fn observe(self, value: Value<'_>) -> Option<(&'static str, Json)> {
        match (self, value.data_type().id(), value.as_bytes()) {
            (Self::Occupancy, 0x18, [bits]) => Some(("occupancy", (bits & 1 != 0).into())),
            (Self::Illuminance, 0x21, [lo, hi]) => match u16::from_le_bytes([*lo, *hi]) {
                u16::MAX => None,
                0 => Some(("illuminance", 0.0.into())),
                n => Some((
                    "illuminance",
                    10_f64.powf((f64::from(n) - 1.) / 10000.).round().into(),
                )),
            },
            _ => None,
        }
    }
}
