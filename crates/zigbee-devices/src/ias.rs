//! IAS Zone server observations. The zone type defines the meaning of each alarm bit.
pub mod ace;
pub mod enrollment;
use crate::{Endpoint, lighting::Request, standard_controls::request};
use zigbee_zcl::{
    Direction, Frame, FrameType,
    attributes::Status,
    cluster::{Command, IasZone},
    foundation::Payload,
    value::Value,
};

pub const CLUSTER: u16 = 0x0500;
pub const PROBE: [u16; 2] = [1, 2];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribute {
    Type,
    Status,
}

impl Attribute {
    pub fn id(self) -> u16 {
        match self {
            Self::Type => 1,
            Self::Status => 2,
        }
    }
    pub fn property(self) -> &'static str {
        match self {
            Self::Type => "zone_type",
            Self::Status => "zone_status",
        }
    }
    pub fn request(self, endpoint: u8, sequence: u8) -> Request {
        request(
            endpoint,
            sequence,
            CLUSTER,
            FrameType::General,
            0,
            &self.id().to_le_bytes(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Motion,
    Contact,
    Fire,
    Water,
    CarbonMonoxide,
    Vibration,
}

impl Kind {
    pub fn from_type(code: u16) -> Option<Self> {
        Some(match code {
            0x000d => Self::Motion,
            0x0015 => Self::Contact,
            0x0028 => Self::Fire,
            0x002a => Self::Water,
            0x002b => Self::CarbonMonoxide,
            0x002d => Self::Vibration,
            _ => return None,
        })
    }
    pub fn property(self) -> &'static str {
        match self {
            Self::Motion => "occupancy",
            Self::Contact => "contact",
            Self::Fire => "fire_alarm",
            Self::Water => "water_leak",
            Self::CarbonMonoxide => "carbon_monoxide",
            Self::Vibration => "vibration",
        }
    }
    pub fn device_class(self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::Contact => "opening",
            Self::Fire => "safety",
            Self::Water => "moisture",
            Self::CarbonMonoxide => "carbon_monoxide",
            Self::Vibration => "vibration",
        }
    }
    /// Alarm1 is the primary alarm. Alarm2 can identify a second portal, a
    /// second sensor or a different event (such as cooking on a CO sensor).
    pub fn mqtt_state(self, status: u16) -> bool {
        let alarm = status & 1 != 0;
        if self == Self::Contact { !alarm } else { alarm }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Zone {
    pub zone_type: Option<u16>,
    /// A notification can precede the first successful zone type read.
    pub untyped_status: Option<u16>,
}

impl Zone {
    pub fn resolve(ep: &Endpoint<'_>, zone_type: Option<u16>) -> Option<Self> {
        ep.has_standard_server(CLUSTER).then_some(Self {
            zone_type,
            untyped_status: None,
        })
    }
    pub fn kind(self) -> Option<Kind> {
        self.zone_type.and_then(Kind::from_type)
    }
    pub fn property(self, name: &str) -> Option<Attribute> {
        match name {
            "zone_type" => Some(Attribute::Type),
            "zone_status" => Some(Attribute::Status),
            s if self.kind().is_some_and(|kind| kind.property() == s) => Some(Attribute::Status),
            _ => None,
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Observations {
    pub zone_type: Option<u16>,
    pub status: Option<u16>,
    pub read_complete: bool,
}

impl Observations {
    fn record(&mut self, id: u16, value: Value<'_>) {
        let [lo, hi] = value.as_bytes() else { return };
        let number = u16::from_le_bytes([*lo, *hi]);
        match (id, value.data_type().id()) {
            (1, 0x31) => self.zone_type = Some(number),
            (2, 0x19) => self.status = Some(number),
            _ => {}
        }
    }
}

/// Read replies require an endpoint-, cluster- and sequence-correlated request.
pub fn observe(frame: Frame<'_>, requested: Option<Attribute>) -> Observations {
    let mut obs = Observations::default();
    if frame.header.manufacturer.is_some() || frame.header.direction != Direction::ServerToClient {
        return obs;
    }
    if frame.header.frame_type == FrameType::ClusterSpecific {
        if let Ok(Command::IasZone(IasZone::ZoneStatusChangeNotification { zone_status, .. })) =
            Command::decode(CLUSTER, frame)
        {
            obs.status = Some(zone_status);
        }
        return obs;
    }
    match Payload::decode(frame) {
        Ok(Payload::ReportAttributes(records)) => {
            for record in records {
                obs.record(record.id, record.value);
            }
        }
        Ok(Payload::ReadAttributesResponse(records)) => {
            if let [record] = records.as_slice()
                && requested.is_some_and(|attr| record.id == attr.id())
            {
                match record.result {
                    Status::Success(value) => {
                        obs.record(record.id, value);
                        obs.read_complete = obs.zone_type.is_some() || obs.status.is_some();
                    }
                    Status::Error(_) => obs.read_complete = true,
                }
            }
        }
        _ => {}
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(bytes: &[u8], query: Option<Attribute>) -> Observations {
        observe(Frame::decode(bytes).unwrap(), query)
    }

    #[test]
    fn reports_require_exact_types_and_notifications_require_the_standard_frame() {
        let report = [0x18, 7, 0x0a, 2, 0, 0x19, 4, 0, 1, 0, 0x31, 0x15, 0];
        assert_eq!(
            observation(&report, None),
            Observations {
                status: Some(4),
                zone_type: Some(0x15),
                read_complete: false
            }
        );
        let status = [0x19, 8, 0, 0x0a, 0, 0, 7, 0, 0];
        assert_eq!(observation(&status, None).status, Some(10));
        for invalid in [
            &status[..8],
            &[0x11, 8, 0, 10, 0, 0, 7, 0, 0],
            &[0x1d, 0x23, 1, 8, 0, 10, 0, 0, 7, 0, 0],
            &[0x19, 8, 1, 0x15, 0, 1, 0],
            &[0x18, 8, 10, 2, 0, 0x21, 10, 0],
        ] {
            assert_eq!(observation(invalid, None), Observations::default());
        }
    }

    #[test]
    fn read_correlation_and_type_control_alarm_semantics() {
        let read = [0x18, 7, 1, 2, 0, 0, 0x19, 1, 0];
        assert!(observation(&read, Some(Attribute::Status)).read_complete);
        assert_eq!(observation(&read, None), Observations::default());
        assert_eq!(
            observation(&read, Some(Attribute::Type)),
            Observations::default()
        );
        assert!(observation(&[0x18, 7, 1, 2, 0, 0x86], Some(Attribute::Status)).read_complete);
        assert!(
            !observation(&[0x18, 7, 1, 2, 0, 0, 0x21, 1, 0], Some(Attribute::Status)).read_complete
        );
        assert_eq!(Kind::from_type(0xffff), None);
        assert!(!Kind::CarbonMonoxide.mqtt_state(2));
        assert!(!Kind::Motion.mqtt_state(4));
        assert!(Kind::Contact.mqtt_state(0));
        assert!(!Kind::Contact.mqtt_state(1));
    }
}
