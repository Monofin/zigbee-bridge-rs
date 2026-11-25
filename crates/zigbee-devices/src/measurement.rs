//! Standard attribute observations in their exact engineering units. Battery
//! curves, nonstandard percentages and scaled-pressure reports need a device
//! profile. No estimate is derived from battery voltage.
pub mod environment;

use crate::Endpoint;
use zigbee_zcl::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measurement {
    BatteryHalfPercent(u8),
    BatteryMillivolts(u16),
    TemperatureCentidegreesCelsius(i16),
    HumidityHundredthsPercent(u16),
    PressureHectopascals(i16),
}

/// Standard attributes exposed by the measurement decoder. A server list
/// establishes where a read can be addressed; the device can decline it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribute {
    Battery,
    Voltage,
    Temperature,
    Humidity,
    Pressure,
}

#[derive(serde::Deserialize)]
struct BatteryProfile {
    manufacturer: String,
    model: String,
    endpoint: u8,
    voltage: bool,
}

static BATTERIES: std::sync::LazyLock<Vec<BatteryProfile>> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("../profiles/battery.json")).expect("valid built-in battery profiles")
});

impl Attribute {
    /// Mandatory MeasuredValue or an explicitly documented optional battery attribute.
    pub fn declared(self, ep: &Endpoint<'_>, identity: Option<(&str, &str)>) -> bool {
        if !self.available(ep) { return false; }
        match self {
            Self::Temperature | Self::Humidity | Self::Pressure => true,
            Self::Battery | Self::Voltage => BATTERIES.iter().any(|p| {
                p.endpoint == ep.id && identity == Some((&p.manufacturer, &p.model))
                    && (self == Self::Battery || p.voltage)
            }),
        }
    }

    pub fn address(self) -> (u16, u16) {
        match self {
            Self::Battery => (1, 0x21),
            Self::Voltage => (1, 0x20),
            Self::Temperature => (0x402, 0),
            Self::Humidity => (0x405, 0),
            Self::Pressure => (0x403, 0),
        }
    }
    pub fn available(self, endpoint: &Endpoint<'_>) -> bool {
        endpoint.has_standard_server(self.address().0)
    }
    pub fn read(self, endpoint: &Endpoint<'_>, sequence: u8) -> anyhow::Result<Vec<u8>> {
        use zigbee_zcl::{Direction, Frame, FrameType, Header, foundation::Payload};
        anyhow::ensure!(
            self.available(endpoint),
            "required measurement server missing"
        );
        let payload = Payload::ReadAttributes(vec![self.address().1]).encode()?;
        let mut bytes = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::General,
                manufacturer: None,
                direction: Direction::ClientToServer,
                disable_default_response: true,
                sequence,
                command_id: 0,
            },
            payload: &payload,
        }
        .encode_into(&mut bytes);
        Ok(bytes)
    }
}

/// Interpret a successful attribute read or report on an advertised server.
pub fn observe(
    endpoint: &Endpoint<'_>,
    cluster: u16,
    attribute: u16,
    manufacturer: Option<u16>,
    value: Value<'_>,
) -> Option<Measurement> {
    if manufacturer.is_some() || !endpoint.has_standard_server(cluster) {
        return None;
    }
    match (cluster, attribute, value.data_type().id(), value.as_bytes()) {
        (1, 0x20, 0x20, [raw @ 0..=254]) => {
            Some(Measurement::BatteryMillivolts(u16::from(*raw) * 100))
        }
        (1, 0x21, 0x20, [raw @ 0..=200]) => Some(Measurement::BatteryHalfPercent(*raw)),
        (0x0402, 0, 0x29, [lo, hi]) => {
            let raw = i16::from_le_bytes([*lo, *hi]);
            (raw >= -27315).then_some(Measurement::TemperatureCentidegreesCelsius(raw))
        }
        (0x0403, 0, 0x29, [lo, hi]) => {
            let raw = i16::from_le_bytes([*lo, *hi]);
            (raw != i16::MIN).then_some(Measurement::PressureHectopascals(raw))
        }
        (0x0405, 0, 0x21, [lo, hi]) => {
            let raw = u16::from_le_bytes([*lo, *hi]);
            (raw <= 10000).then_some(Measurement::HumidityHundredthsPercent(raw))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_units_and_malformed_observations() {
        let endpoint = Endpoint {
            id: 2,
            profile: 260,
            servers: &[1, 0x402, 0x403, 0x405],
            clients: &[],
        };
        let decode = |cluster, attr, kind: u8, bytes: &[u8]| {
            let value = Value::from_bytes(kind.try_into().unwrap(), bytes).unwrap();
            observe(&endpoint, cluster, attr, None, value)
        };
        assert_eq!(
            decode(1, 0x20, 0x20, &[30]),
            Some(Measurement::BatteryMillivolts(3000))
        );
        assert_eq!(
            decode(1, 0x21, 0x20, &[101]),
            Some(Measurement::BatteryHalfPercent(101))
        );
        assert_eq!(
            decode(0x402, 0, 0x29, &(-125i16).to_le_bytes()),
            Some(Measurement::TemperatureCentidegreesCelsius(-125))
        );
        assert_eq!(
            decode(0x403, 0, 0x29, &1013i16.to_le_bytes()),
            Some(Measurement::PressureHectopascals(1013))
        );
        assert_eq!(
            decode(0x405, 0, 0x21, &10000u16.to_le_bytes()),
            Some(Measurement::HumidityHundredthsPercent(10000))
        );
        assert_eq!(decode(0x405, 0, 0x21, &10001u16.to_le_bytes()), None);
        assert_eq!(decode(0x402, 0, 0x29, &i16::MIN.to_le_bytes()), None);
        assert_eq!(decode(0x403, 0, 0x29, &i16::MIN.to_le_bytes()), None);
        assert_eq!(decode(1, 0x21, 0x20, &[201]), None);
        assert_eq!(decode(1, 0x20, 0x20, &[255]), None);
        assert_eq!(decode(0x405, 0, 0x29, &[0, 0]), None);
        let value = Value::from_bytes(0x20.try_into().unwrap(), &[100]).unwrap();
        assert_eq!(observe(&endpoint, 1, 0x21, Some(1), value), None);
        let remote = Endpoint {
            servers: &[],
            clients: &[1],
            ..endpoint
        };
        assert_eq!(observe(&remote, 1, 0x21, None, value), None);
    }
}
