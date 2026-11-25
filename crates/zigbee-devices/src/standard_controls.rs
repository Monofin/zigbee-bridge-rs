//! Optional device-wide controls, mapped to the reviewed application endpoint.
mod effect;
pub mod level_config;
pub mod hue;
pub use effect::Effects;
use crate::{Endpoint, lighting::Request};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value as Json;
use std::sync::OnceLock;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header,
    attributes::{AttributeValue, Status, WriteStatus},
    cluster::{Command, Identify},
    foundation::{GeneralCommand, Payload},
    value::Value,
};

pub const STARTUP: &str = "color_temp_startup";
pub const COLOR_CONTROL: u16 = 0x0300;
const STARTUP_ATTR: u16 = 0x4010;

#[derive(Deserialize)]
struct Product {
    manufacturer: String,
    model: String,
    endpoint: u8,
    device_type: u16,
    identify: bool,
    #[serde(default)]
    level_config: bool,
    temperature_via_xy: Option<(u16,u16)>,
    #[serde(default)]
    hue_effects: bool,
    tuya_lamp: Option<crate::tuya::lamp::Lamp>,
    effects: Option<Effects>,
    startup_mireds: Option<(u16, u16)>,
}

#[derive(Clone, Copy, Default)]
pub struct Controls {
    pub identify: bool,
    pub level_config: bool,
    pub temperature_via_xy: Option<(u16,u16)>,
    pub hue_effects: bool,
    pub tuya_lamp: Option<crate::tuya::lamp::Lamp>,
    pub effects: Option<Effects>,
    pub startup_mireds: Option<(u16, u16)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupRequest {
    Read,
    Write(u16),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StartupReply {
    pub value: Option<u16>,
    pub complete: bool,
}

impl Controls {
    pub fn resolve(
        identity: Option<(&str, &str)>,
        ep: &Endpoint<'_>,
        ty: u16,
        physical_range: Option<(u16, u16)>,
    ) -> Result<Self> {
        static PRODUCTS: OnceLock<Result<Vec<Product>, String>> = OnceLock::new();
        let products = PRODUCTS
            .get_or_init(|| {
                serde_json::from_str(include_str!("../profiles/standard_controls.json"))
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let Some(p) = products.iter().find(|p| {
            ep.profile == 0x104
                && p.endpoint == ep.id
                && p.device_type == ty
                && identity == Some((p.manufacturer.as_str(), p.model.as_str()))
        }) else {
            return Ok(Self::default());
        };
        let startup_mireds = p
            .startup_mireds
            .filter(|_| ep.servers.contains(&COLOR_CONTROL))
            .map(|fallback| {
                physical_range
                    .filter(|&(min, max)| min > 0 && min <= max && max <= 0xfeff)
                    .unwrap_or(fallback)
            });
        Ok(Self {
            tuya_lamp: p.tuya_lamp.filter(|_| ep.servers.contains(&COLOR_CONTROL)),
            hue_effects: p.hue_effects && ep.servers.contains(&3) && ep.servers.contains(&hue::CLUSTER),
            level_config: p.level_config && ep.servers.contains(&8),
            temperature_via_xy: p.temperature_via_xy.filter(|_| ep.servers.contains(&COLOR_CONTROL)),
            identify: p.identify && ep.servers.contains(&3),
            effects: p.effects.filter(|e| ep.servers.contains(&3) && (!e.has_loop() || ep.servers.contains(&COLOR_CONTROL))),
            startup_mireds,
        })
    }
    pub fn identify(self, ep: u8, seq: u8, value: &Json) -> Result<Request> {
        ensure!(
            self.identify && value.as_str() == Some("identify"),
            "expected supported identify command"
        );
        let command = Command::Identify(Identify::Identify { identify_time: 3 });
        let key = command.key();
        Ok(request(
            ep,
            seq,
            key.cluster,
            FrameType::ClusterSpecific,
            key.command,
            &command.encode()?,
        ))
    }
    pub fn startup(
        self,
        ep: u8,
        seq: u8,
        value: Option<&Json>,
    ) -> Result<(Request, StartupRequest)> {
        let (lo, hi) = self
            .startup_mireds
            .context("startup temperature unavailable")?;
        let (operation, code, bytes) = if let Some(value) = value {
            let n = match value.as_str() {
                Some("warmest") => i64::from(hi),
                Some("coolest") => i64::from(lo),
                Some("warm") => 454,
                Some("neutral") => 370,
                Some("cool") => 250,
                Some("previous") => 65535,
                Some(s) => s.parse::<i64>().context("invalid startup temperature")?,
                None => value
                    .as_i64()
                    .context("startup temperature requires an integer or preset")?,
            };
            let n = if n == 65535 {
                65535
            } else {
                n.clamp(i64::from(lo), i64::from(hi)) as u16
            };
            let wire = n.to_le_bytes();
            let payload = Payload::WriteAttributes(vec![AttributeValue {
                id: STARTUP_ATTR,
                value: Value::from_bytes(0x21.try_into()?, &wire)?,
            }]);
            (
                StartupRequest::Write(n),
                payload.command() as u8,
                payload.encode()?,
            )
        } else {
            let payload = Payload::ReadAttributes(vec![STARTUP_ATTR]);
            (
                StartupRequest::Read,
                payload.command() as u8,
                payload.encode()?,
            )
        };
        Ok((
            request(ep, seq, COLOR_CONTROL, FrameType::General, code, &bytes),
            operation,
        ))
    }
    /// The caller matches peer, current address, endpoint, cluster and sequence.
    pub fn observe(
        self,
        frame: Frame<'_>,
        pending: Option<StartupRequest>,
    ) -> Result<StartupReply> {
        let none = StartupReply::default();
        if self.startup_mireds.is_none()
            || frame.header.direction != Direction::ServerToClient
            || frame.header.manufacturer.is_some()
        {
            return Ok(none);
        }
        match (frame.header.general_command(), pending) {
            (Some(GeneralCommand::ReportAttributes), _) => {
                let Payload::ReportAttributes(records) = Payload::decode(frame)? else {
                    unreachable!()
                };
                let mut matching = records.into_iter().filter(|v| v.id == STARTUP_ATTR);
                let Some(r) = matching.next() else {
                    return Ok(none);
                };
                ensure!(matching.next().is_none(), "duplicate startup report");
                Ok(StartupReply {
                    value: temperature(r.value),
                    complete: false,
                })
            }
            (Some(GeneralCommand::ReadAttributesResponse), Some(StartupRequest::Read)) => {
                let Payload::ReadAttributesResponse(records) = Payload::decode(frame)? else {
                    unreachable!()
                };
                let mut matching = records.into_iter().filter(|v| v.id == STARTUP_ATTR);
                let Some(r) = matching.next() else {
                    return Ok(none);
                };
                ensure!(matching.next().is_none(), "duplicate startup read response");
                let value = match r.result {
                    Status::Success(v) => temperature(v),
                    Status::Error(_) => None,
                };
                Ok(StartupReply {
                    value,
                    complete: true,
                })
            }
            (Some(GeneralCommand::WriteAttributesResponse), Some(StartupRequest::Write(n))) => {
                match Payload::decode(frame)? {
                    Payload::WriteAttributesResponse(WriteStatus::Success) => Ok(StartupReply {
                        value: Some(n),
                        complete: true,
                    }),
                    Payload::WriteAttributesResponse(WriteStatus::Failures(f))
                        if f.iter().any(|r| r.key == STARTUP_ATTR) =>
                    {
                        Ok(StartupReply {
                            value: None,
                            complete: true,
                        })
                    }
                    _ => Ok(none),
                }
            }
            _ => Ok(none),
        }
    }
}

fn temperature(value: Value<'_>) -> Option<u16> {
    if value.data_type().id() != 0x21 {
        return None;
    }
    let n = u16::from_le_bytes(value.as_bytes().try_into().ok()?);
    (n <= 0xfeff || n == u16::MAX).then_some(n)
}

pub(crate) fn request(
    endpoint: u8,
    sequence: u8,
    cluster: u16,
    frame_type: FrameType,
    command_id: u8,
    payload: &[u8],
) -> Request {
    let mut frame = Vec::new();
    Frame {
        header: Header {
            frame_type,
            manufacturer: None,
            direction: Direction::ClientToServer,
            disable_default_response: true,
            sequence,
            command_id,
        },
        payload,
    }
    .encode_into(&mut frame);
    Request {
        endpoint,
        cluster,
        frame,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lamp() -> Endpoint<'static> {
        Endpoint {
            id: 11,
            profile: 260,
            servers: &[0, 3, 6, 8, COLOR_CONTROL],
            clients: &[],
        }
    }
    #[test]
    fn optional_features_require_reviewed_identity_endpoint_and_server() -> Result<()> {
        let ep = lamp();
        let gl = Some(("GLEDOPTO", "GL-FL-006P"));
        let controls = Controls::resolve(gl, &ep, 269, None)?;
        assert!(!controls.identify);
        assert_eq!(controls.startup_mireds, Some((158, 495)));
        assert_eq!(
            Controls::resolve(gl, &Endpoint { id: 1, ..ep }, 269, None)?.startup_mireds,
            None
        );
        assert_eq!(
            Controls::resolve(Some(("other", "GL-FL-006P")), &ep, 269, None)?.startup_mireds,
            None
        );
        let bad_ep = Endpoint {
            servers: &[0, 3],
            ..ep
        };
        assert_eq!(
            Controls::resolve(gl, &bad_ep, 269, None)?.startup_mireds,
            None
        );
        Ok(())
    }
    #[test]
    fn wire_and_confirmation_cover_clamped_presets_sentinel_and_mixed_reports() -> Result<()> {
        let m = Controls::resolve(
            Some(("Signify Netherlands B.V.", "LTA006")),
            &lamp(),
            268,
            Some((230, 440)),
        )?;
        let req = m.identify(11, 19, &json!("identify"))?;
        assert_eq!(
            (req.endpoint, req.cluster, req.frame.as_slice()),
            (11, 3, [0x11, 19, 0, 3, 0].as_slice())
        );
        assert!(m.identify(11, 1, &json!("ON")).is_err());
        for (input, value) in [
            (json!("coolest"), 230),
            (json!("warm"), 440),
            (json!(7), 230),
            (json!(275), 275),
            (json!("previous"), 65535),
            (json!("65535"), 65535),
        ] {
            let (req, op) = m.startup(11, 13, Some(&input))?;
            assert_eq!(op, StartupRequest::Write(value));
            let [lo, hi] = value.to_le_bytes();
            assert_eq!(req.frame, [0x10, 13, 2, 0x10, 0x40, 0x21, lo, hi]);
            let ack = Frame::decode(&[0x18, 13, 4, 0])?;
            assert_eq!(m.observe(ack, None)?, StartupReply::default());
            assert_eq!(
                m.observe(ack, Some(op))?,
                StartupReply {
                    value: Some(value),
                    complete: true
                }
            );
            let failure = Frame::decode(&[0x18, 13, 4, 0x86, 0x10, 0x40])?;
            assert_eq!(
                m.observe(failure, Some(op))?,
                StartupReply {
                    value: None,
                    complete: true
                }
            );
        }
        assert!(m.startup(11, 13, Some(&json!("nonsense"))).is_err());
        let (read, op) = m.startup(11, 6, None)?;
        assert_eq!(read.frame, [0x10, 6, 0, 0x10, 0x40]);
        let reply = Frame::decode(&[0x18, 6, 1, 0x10, 0x40, 0, 0x21, 0xff, 0xff])?;
        assert_eq!(m.observe(reply, None)?, StartupReply::default());
        assert_eq!(
            m.observe(reply, Some(op))?,
            StartupReply {
                value: Some(65535),
                complete: true
            }
        );
        let other = Frame::decode(&[0x18, 6, 1, 7, 0, 0, 0x21, 10, 1])?;
        assert_eq!(m.observe(other, Some(op))?, StartupReply::default());
        let mixed = Frame::decode(&[0x18, 5, 0x0a, 7, 0, 0x21, 10, 1, 0x10, 0x40, 0x21, 14, 1])?;
        assert_eq!(
            m.observe(mixed, Some(op))?,
            StartupReply {
                value: Some(270),
                complete: false
            }
        );
        Ok(())
    }
}
