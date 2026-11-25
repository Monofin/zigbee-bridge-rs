//! Confirmed relay functions. A switch or valve advertising Level Control is
//! still exposed only with the properties reviewed for that product.
mod electricity;
mod inching;
use crate::{Endpoint, lighting::Request};
use anyhow::{Result, ensure};
pub use electricity::{Electricity, MeterKind, Reading};
use serde::Deserialize;
use zigbee_zcl::{Direction, Frame, FrameType, Header, foundation::Payload, value::Value};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Relay,
    Outlet,
    Valve,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    kind: Kind,
    manufacturer: String,
    model: String,
    profile: u16,
    endpoint: u8,
    device_type: u16,
    servers: Vec<u16>,
    source: String,
    meter: Option<MeterKind>,
    #[serde(default)]
    inching: bool,
}

pub struct Outlets(Vec<Profile>);
impl Outlets {
    pub fn builtin() -> Result<Self> {
        let rows: Vec<Profile> = serde_json::from_str(include_str!("../profiles/outlets.json"))?;
        for row in &rows {
            ensure!(
                !row.manufacturer.is_empty()
                    && !row.model.is_empty()
                    && !row.source.is_empty()
                    && row.servers.contains(&0)
                    && row.servers.contains(&6),
                "incomplete relay profile"
            );
        }
        Ok(Self(rows))
    }

    pub fn resolve(
        &self,
        manufacturer: &str,
        model: &str,
        ep: &Endpoint<'_>,
        device_type: u16,
    ) -> Option<Outlet> {
        let mut candidates = self.0.iter().filter(|p| {
            p.manufacturer == manufacturer
                && p.model == model
                && p.endpoint == ep.id
                && p.profile == ep.profile
                && p.device_type == device_type
                && p.servers.iter().all(|cluster| ep.servers.contains(cluster))
        });
        let p = candidates.next()?;
        candidates.next().is_none().then_some(Outlet {
            kind: p.kind,
            endpoint: ep.id,
            meter: p.meter,
            inching: p.inching && ep.servers.contains(&0xfc11),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Outlet {
    kind: Kind,
    inching: bool,
    endpoint: u8,
    meter: Option<MeterKind>,
}
impl Outlet {
    /// Home Automation On/Off Output, Mains Power Outlet, Smart Plug and
    /// the On/Off Plug-in Unit in the Lighting/Occupancy and Light Link profiles.
    /// A client-only switch or an unidentified On/Off server has different semantics.
    pub fn standard(ep: &Endpoint<'_>, device_type: u16) -> Option<Self> {
        let kind = match (ep.profile, device_type) {
            (0x0104, 0x0002) => Kind::Relay,
            (0x0104, 0x0009 | 0x0051 | 0x010a) | (0x0104 | 0xc05e, 0x0010) => Kind::Outlet,
            _ => return None,
        };
        ((1..=240).contains(&ep.id) && ep.servers.contains(&6))
            .then_some(Self { kind, endpoint: ep.id, meter: None, inching: false })
    }

    pub fn meter(&self) -> Option<MeterKind> {
        self.meter
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn toggle(&self, sequence:u8)->Request {self.request(FrameType::ClusterSpecific,2,sequence,&[])}
    pub fn set_state(&self, on: bool, sequence: u8) -> Request {
        self.request(FrameType::ClusterSpecific, u8::from(on), sequence, &[])
    }

    pub fn get_state(&self, sequence: u8) -> Result<Request> {
        let payload = Payload::ReadAttributes(vec![0]).encode()?;
        Ok(self.request(FrameType::General, 0, sequence, &payload))
    }

    fn request(
        &self,
        frame_type: FrameType,
        command_id: u8,
        sequence: u8,
        payload: &[u8],
    ) -> Request {
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type,
                command_id,
                direction: Direction::ClientToServer,
                manufacturer: None,
                disable_default_response: true,
                sequence,
            },
            payload,
        }
        .encode_into(&mut frame);
        Request {
            endpoint: self.endpoint,
            cluster: 6,
            frame,
        }
    }

    /// Caller has already correlated reports/read replies to this active membership.
    pub fn state_attribute(
        &self,
        cluster: u16,
        attribute: u16,
        manufacturer: Option<u16>,
        value: Value<'_>,
    ) -> Option<bool> {
        if (cluster, attribute) != (6, 0)
            || manufacturer.is_some()
            || value.data_type().id() != 0x10
        {
            return None;
        }
        match value.as_bytes() {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_onoff_cluster_can_be_a_valve_or_an_unrelated_device() {
        let eps = Outlets::builtin().unwrap();
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 1, 6, 1028, 64529],
            clients: &[],
        };
        let valve = eps.resolve("SONOFF", "SWV", &ep, 2).unwrap();
        assert_eq!(valve.kind(), Kind::Valve);
        assert_eq!(valve.set_state(false, 19).frame, [0x11, 19, 0]);
        assert_eq!(valve.get_state(24).unwrap().frame, [0x10, 24, 0, 0, 0]);
        assert!(eps.resolve("other", "SWV", &ep, 2).is_none());
        assert!(eps.resolve("SONOFF", "SWV", &ep, 256).is_none());
        assert!(
            eps.resolve(
                "SONOFF",
                "SWV",
                &Endpoint {
                    servers: &[0, 6],
                    ..ep
                },
                2
            )
            .is_none()
        );
    }
}
