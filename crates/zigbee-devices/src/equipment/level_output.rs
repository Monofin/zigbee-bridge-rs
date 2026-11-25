//! HA 1.2 §7.4.4. The meaning of an output level is device-dependent.
use super::{Field, Format, field, number};
use crate::{Endpoint, lighting::Request};
use anyhow::{Context, Result, ensure};
use serde_json::Value as Json;
use zigbee_zcl::{
    Direction, Frame, FrameType, Header,
    cluster::{Command, LevelControl, OnOff},
};

pub static STATE: Field = field(6, 0, "state", Format::Enum(&["OFF", "ON"])).reportable();
pub static LEVEL: Field = field(8, 0, "level", number(0, 255, 1, None)).reportable();

#[derive(Clone, Copy)]
pub struct Output {
    pub bounds: Option<(u8, u8)>,
}

impl Output {
    pub fn resolve<'a>(
        ep: &Endpoint<'_>,
        device: u16,
        get: impl Fn(u16) -> Option<(u8, &'a [u8])>,
    ) -> Option<Self> {
        if ep.profile != 0x104
            || device != 3
            || !(1..=240).contains(&ep.id)
            || ![6, 8].iter().all(|c| ep.servers.contains(c))
        {
            return None;
        }
        // MinLevel/MaxLevel are optional, with R8 defaults of 0 and 255.
        let bound = |id, default| match get(id) {
            None => Some(default),
            Some((0x20, [v])) => Some(*v),
            _ => None,
        };
        let bounds = bound(2, 0).zip(bound(3, 255)).filter(|(lo, hi)| lo <= hi);
        Some(Self { bounds })
    }
    pub fn set(self, f: &Field, ep: u8, seq: u8, value: &Json) -> Result<Request> {
        let command = match (f.cluster, f.id) {
            (6, 0) => Command::OnOff(match value.as_str() {
                Some(s) if s.eq_ignore_ascii_case("on") => OnOff::On {},
                Some(s) if s.eq_ignore_ascii_case("off") => OnOff::Off {},
                Some(s) if s.eq_ignore_ascii_case("toggle") => OnOff::Toggle {},
                _ => anyhow::bail!("use ON, OFF or TOGGLE"),
            }),
            (8, 0) => {
                let level = u8::try_from(value.as_u64().context("expected integer output level")?)?;
                let (lo, hi) = self.bounds.context("invalid output level bounds")?;
                ensure!(
                    (lo..=hi).contains(&level),
                    "outside device output level bounds"
                );
                // Keep on/off independent and preserve the endpoint's ExecuteIfOff option.
                Command::LevelControl(LevelControl::MoveToLevel {
                    level,
                    transition_time: 0,
                    option_mask: 0,
                    option_override: 0,
                })
            }
            _ => anyhow::bail!("unknown level-output property"),
        };
        let key = command.key();
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::ClusterSpecific,
                direction: Direction::ClientToServer,
                manufacturer: None,
                disable_default_response: false,
                sequence: seq,
                command_id: key.command,
            },
            payload: &command.encode()?,
        }
        .encode_into(&mut frame);
        Ok(Request {
            cluster: key.cluster,
            endpoint: ep,
            frame,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn ha_output_requires_both_servers_and_preserves_the_on_off_option() -> Result<()> {
        let ep = Endpoint {
            id: 7,
            profile: 0x104,
            servers: &[0, 3, 4, 5, 6, 8],
            clients: &[3],
        };
        let o = Output::resolve(&ep, 3, |a| {
            Some((0x20, if a == 2 { &[12] } else { &[180] }))
        })
        .unwrap();
        assert_eq!(o.bounds, Some((12, 180)));
        assert_eq!(
            o.set(&LEVEL, 7, 4, &json!(100))?.frame,
            [1, 4, 0, 100, 0, 0, 0, 0]
        );
        assert_eq!(o.set(&STATE, 7, 4, &json!("OFF"))?.frame, [1, 4, 0]);
        for v in [json!(-1), json!(11), json!(181), json!(100.3)] {
            assert!(o.set(&LEVEL, 7, 4, &v).is_err());
        }
        assert!(
            Output::resolve(
                &Endpoint {
                    servers: &[6],
                    ..ep
                },
                3,
                |_| None
            )
            .is_none()
        );
        assert!(Output::resolve(&ep, 0x101, |_| None).is_none());
        assert!(
            Output::resolve(
                &Endpoint {
                    profile: 0xc05e,
                    ..ep
                },
                3,
                |_| None
            )
            .is_none()
        );
        assert_eq!(
            Output::resolve(&ep, 3, |_| None).unwrap().bounds,
            Some((0, 255))
        );
        Ok(())
    }
}
