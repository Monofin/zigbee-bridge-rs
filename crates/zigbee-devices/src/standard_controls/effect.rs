//! Standard Z2M effect vocabularies. Hue scenes use a different converter.
use super::{Request, request};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use zigbee_zcl::{
    FrameType,
    cluster::{ColorControl, Command, Identify},
};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effects {
    Identify,
    Colorloop,
}

const IDENTIFY: &[&str] = &[
    "blink",
    "breathe",
    "okay",
    "channel_change",
    "finish_effect",
    "stop_effect",
];
const WITH_LOOP: &[&str] = &[
    "blink",
    "breathe",
    "okay",
    "channel_change",
    "finish_effect",
    "stop_effect",
    "colorloop",
    "stop_colorloop",
];
impl Effects {
    pub(super) fn has_loop(self) -> bool {
        matches!(self, Self::Colorloop)
    }
    pub fn names(self) -> &'static [&'static str] {
        if self.has_loop() { WITH_LOOP } else { IDENTIFY }
    }

    /// The colour loop is Continuous Move Hue, rather than the Color Loop Set
    /// command. Z2M accepts the period in seconds and clamps the wire rate.
    pub fn set(
        self,
        endpoint: u8,
        sequence: u8,
        name: &str,
        period: Option<f64>,
    ) -> Result<Request> {
        let name = name.to_ascii_lowercase();
        let command = match name.as_str() {
            "colorloop" | "stop_colorloop" if self.has_loop() => {
                let (move_mode, rate) = if name == "stop_colorloop" {
                    (0, 1)
                } else {
                    let seconds = period.unwrap_or(15.0);
                    anyhow::ensure!(
                        seconds.is_finite() && seconds >= 0.0,
                        "invalid colorloop period"
                    );
                    (1, (255.0 / seconds).round().clamp(1.0, 255.0) as u8)
                };
                Command::ColorControl(ColorControl::MoveHue {
                    move_mode,
                    rate,
                    options_mask: 0,
                    options_override: 0,
                })
            }
            other => {
                let Some(index) = IDENTIFY.iter().position(|&s| s == other) else {
                    bail!("effect not advertised for this lamp")
                };
                let id = *([0, 1, 2, 11, 254, 255]
                    .get(index)
                    .context("invalid Identify effect")?);
                Command::Identify(Identify::TriggerEffect {
                    effect_id: id,
                    effect_variant: 0,
                })
            }
        };
        let key = command.key();
        Ok(request(
            endpoint,
            sequence,
            key.cluster,
            FrameType::ClusterSpecific,
            key.command,
            &command.encode()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identify_and_loop_use_different_clusters_and_payloads() {
        for (name, id) in IDENTIFY.iter().zip([0, 1, 2, 11, 254, 255]) {
            let r = Effects::Identify.set(11, 7, name, None).unwrap();
            assert_eq!(r.cluster, 3);
            assert_eq!(r.frame, [0x11, 7, 0x40, id, 0]);
        }
        assert!(Effects::Identify.set(1, 1, "colorloop", None).is_err());
        assert!(Effects::Colorloop.set(1, 1, "candle", None).is_err());
        for (period, rate) in [
            (None, 17),
            (Some(2.0), 128),
            (Some(0.0), 255),
            (Some(9999.0), 1),
        ] {
            let r = Effects::Colorloop.set(11, 2, "colorloop", period).unwrap();
            assert_eq!(r.cluster, 768);
            assert_eq!(r.frame, [0x11, 2, 1, 1, rate, 0, 0]);
        }
        assert_eq!(
            Effects::Colorloop
                .set(11, 2, "stop_colorloop", None)
                .unwrap()
                .frame,
            [0x11, 2, 1, 0, 1, 0, 0]
        );
    }
}
