//! Single-channel CoolKit inching command (pinned SONOFF converter, protocolData).
use super::{Outlet, Request};
use anyhow::{Result, ensure};
use serde::Deserialize;
use zigbee_zcl::{Direction, Frame, FrameType, Header};

#[derive(Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Enable {
    Enable,
    Disable,
}
#[derive(Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Mode {
    On,
    Off,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inching {
    inching_control: Enable,
    inching_mode: Mode,
    inching_time: f64,
}

impl Outlet {
    pub fn set_inching(self, value: &serde_json::Value, sequence: u8) -> Result<Request> {
        ensure!(self.inching, "inching unavailable on this outlet");
        let setting: Inching = serde_json::from_value(value.clone())?;
        let ticks = setting.inching_time * 2.0;
        ensure!(
            (1.0..=7199.0).contains(&ticks) && ticks.fract() == 0.0,
            "inching_time requires a half-second step in 0.5..=3599.5 seconds"
        );
        let mode = (u8::from(matches!(setting.inching_control, Enable::Enable)) << 7)
            | u8::from(matches!(setting.inching_mode, Mode::On));
        let [low, high] = (ticks as u16).to_le_bytes();
        let mut payload = [1, 0x17, 7, 0x80, mode, 0, low, high, 0, 0, 0];
        payload[10] = payload[..10].iter().fold(0, |sum, b| sum ^ b);
        let mut frame = Vec::new();
        Frame {
            header: Header {
                frame_type: FrameType::ClusterSpecific,
                direction: Direction::ClientToServer,
                manufacturer: Some(0x1286),
                disable_default_response: false,
                sequence,
                command_id: 1,
            },
            payload: &payload,
        }
        .encode_into(&mut frame);
        Ok(Request {
            endpoint: self.endpoint,
            cluster: 0xfc11,
            frame,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{Endpoint, outlet::Outlets};
    use serde_json::json;

    #[test]
    fn inching_is_a_set_only_command_with_typed_half_second_ticks() {
        let catalogue = Outlets::builtin().unwrap();
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 6, 1794, 0xfc11],
            clients: &[],
        };
        let settings =
            |time| json!({"inching_control":"ENABLE","inching_mode":"OFF","inching_time":time});
        for (model, ty) in [("ZBMINIR2", 2), ("MINI-ZBD", 2), ("S60ZBTPG", 81)] {
            let outlet = catalogue.resolve("SONOFF", model, &ep, ty).unwrap();
            let r = outlet.set_inching(&settings(128.5), 9).unwrap();
            assert_eq!((r.endpoint, r.cluster), (1, 0xfc11));
            assert_eq!(
                r.frame,
                [
                    5, 0x86, 0x12, 9, 1, 1, 0x17, 7, 0x80, 0x80, 0, 1, 1, 0, 0, 0x11
                ]
            );
            let both = outlet
                .set_inching(
                    &json!({"inching_control":"DISABLE","inching_mode":"ON","inching_time":3599.5}),
                    1,
                )
                .unwrap();
            assert_eq!(&both.frame[9..13], &[1, 0, 0x1f, 0x1c]);
            for invalid in [0.0, 0.7, 3600.0] {
                assert!(outlet.set_inching(&settings(invalid), 1).is_err());
            }
            assert!(outlet.set_inching(&json!({"inching_time":1}), 1).is_err());
        }
    }
}
