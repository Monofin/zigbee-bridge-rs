//! Window Covering server semantics, ZCL r8 §7.4.2.
use crate::{Endpoint, lighting::Request, standard_controls::request};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value as Json;
use zigbee_zcl::{
    Direction, Frame, FrameType,
    attributes::Status,
    cluster::{Command, WindowCovering},
    foundation::{GeneralCommand, Payload},
    value::Value,
};

pub const CLUSTER: u16 = 0x0102;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Lift,
    Tilt,
}

impl Axis {
    pub fn property(self) -> &'static str {
        match self {
            Self::Lift => "position",
            Self::Tilt => "tilt",
        }
    }
    fn attribute(self) -> u16 {
        match self {
            Self::Lift => 8,
            Self::Tilt => 9,
        }
    }
    fn command(self, percent_closed: u8) -> WindowCovering {
        match self {
            Self::Lift => WindowCovering::WindowCoveringGoToLiftPercentage {
                percentage_lift_value: percent_closed,
            },
            Self::Tilt => WindowCovering::WindowCoveringGoToTiltPercentage {
                percentage_tilt_value: percent_closed,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Cover {
    primary: Axis,
    lift: bool,
    tilt: bool,
    set_lift: bool,
    set_tilt: bool,
}

impl Cover {
    pub fn resolve(
        ep: &Endpoint<'_>,
        kind: Option<u8>,
        config: Option<u8>,
        commands: &[u8],
    ) -> Option<Self> {
        if !(1..=240).contains(&ep.id)
            || !matches!(ep.profile, 0x104 | 0xc05e)
            || !ep.servers.contains(&CLUSTER)
        {
            return None;
        }
        // Percentage attributes are mandatory for the corresponding closed-loop
        // axis. Percentage commands are optional even when feedback is present.
        let config = config.unwrap_or(0);
        let lift = matches!(kind, Some(0..=5 | 8 | 9)) && config & 0x08 != 0;
        let tilt = matches!(kind, Some(6..=8)) && config & 0x10 != 0;
        Some(Self {
            primary: if matches!(kind, Some(6 | 7)) {
                Axis::Tilt
            } else {
                Axis::Lift
            },
            lift,
            tilt,
            set_lift: lift && commands.contains(&0x05),
            set_tilt: tilt && commands.contains(&0x08),
        })
    }
    pub fn reads(self, axis: Axis) -> bool {
        match axis {
            Axis::Lift => self.lift,
            Axis::Tilt => self.tilt,
        }
    }
    pub fn sets(self, axis: Axis) -> bool {
        match axis {
            Axis::Lift => self.set_lift,
            Axis::Tilt => self.set_tilt,
        }
    }
    pub fn primary(self) -> Axis {
        self.primary
    }
    pub fn axis(self, key: &str) -> Result<Axis> {
        let axis = match key {
            "state" => self.primary,
            "position" => Axis::Lift,
            "tilt" => Axis::Tilt,
            _ => bail!("unknown cover property"),
        };
        ensure!(
            self.reads(axis),
            "cover has no confirmed percentage feedback on this axis"
        );
        Ok(axis)
    }
    pub fn read(self, ep: u8, seq: u8, axis: Axis) -> Result<Request> {
        ensure!(self.reads(axis), "unsupported cover percentage read");
        let payload = Payload::ReadAttributes(vec![axis.attribute()]);
        Ok(request(
            ep,
            seq,
            CLUSTER,
            FrameType::General,
            payload.command() as u8,
            &payload.encode()?,
        ))
    }
    pub fn set(self, ep: u8, seq: u8, key: &str, value: &Json) -> Result<Request> {
        let command = if key == "state" {
            match value.as_str() {
                Some("OPEN") => WindowCovering::WindowCoveringUpOpen {},
                Some("CLOSE") => WindowCovering::WindowCoveringDownClose {},
                Some("STOP") => WindowCovering::WindowCoveringStop {},
                _ => bail!("expected OPEN, CLOSE or STOP"),
            }
        } else {
            let axis = self.axis(key)?;
            ensure!(
                self.sets(axis),
                "cover did not advertise the percentage command"
            );
            let open = value
                .as_u64()
                .filter(|n| *n <= 100)
                .context("expected integer percentage 0 through 100")?;
            axis.command(100 - open as u8)
        };
        let command = Command::WindowCovering(command);
        Ok(request(
            ep,
            seq,
            CLUSTER,
            FrameType::ClusterSpecific,
            command.key().command,
            &command.encode()?,
        ))
    }
    /// The caller correlates a GET to the current membership, endpoint and sequence.
    pub fn observe(self, frame: Frame<'_>, requested: Option<Axis>) -> Result<Feedback> {
        let mut result = Feedback::default();
        if frame.header.manufacturer.is_some()
            || frame.header.direction != Direction::ServerToClient
        {
            return Ok(result);
        }
        match frame.header.general_command() {
            Some(GeneralCommand::ReportAttributes) => {
                if let Payload::ReportAttributes(records) = Payload::decode(frame)? {
                    for record in records {
                        self.accept(&mut result, record.id, record.value);
                    }
                }
            }
            Some(GeneralCommand::ReadAttributesResponse) if requested.is_some() => {
                if let Payload::ReadAttributesResponse(records) = Payload::decode(frame)? {
                    for record in records
                        .into_iter()
                        .filter(|r| Some(r.id) == requested.map(Axis::attribute))
                    {
                        result.complete = true;
                        if let Status::Success(value) = record.result {
                            self.accept(&mut result, record.id, value);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(result)
    }
    fn accept(self, result: &mut Feedback, id: u16, value: Value<'_>) {
        let axis = match id {
            8 => Axis::Lift,
            9 => Axis::Tilt,
            _ => return,
        };
        if self.reads(axis)
            && value.data_type().id() == 0x20
            && let [closed @ 0..=100] = value.as_bytes()
        {
            result.positions.push((axis, 100 - closed));
        }
    }
}

#[derive(Default)]
pub struct Feedback {
    pub positions: Vec<(Axis, u8)>,
    pub complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_closed_loop_flags_and_optional_commands_are_independent() {
        let ep = Endpoint {
            id: 7,
            profile: 0x104,
            servers: &[CLUSTER],
            clients: &[],
        };
        for kind in 0..=10 {
            let cover = Cover::resolve(&ep, Some(kind), Some(0x1f), &[0x05]).unwrap();
            let lift = matches!(kind, 0..=5 | 8 | 9);
            let tilt = matches!(kind, 6..=8);
            assert_eq!(cover.reads(Axis::Lift), lift);
            assert_eq!(cover.reads(Axis::Tilt), tilt);
            assert_eq!(cover.sets(Axis::Lift), lift);
            assert!(!cover.sets(Axis::Tilt));
        }
        let cover = Cover::resolve(&ep, Some(8), Some(0x13), &[5, 8]).unwrap();
        assert!(!cover.reads(Axis::Lift));
        assert!(cover.sets(Axis::Tilt));
    }

    #[test]
    fn only_valid_positions_from_standard_reports_and_correlated_reads_confirm_state() -> Result<()>
    {
        let ep = Endpoint {
            id: 7,
            profile: 0xc05e,
            servers: &[CLUSTER],
            clients: &[],
        };
        let cover = Cover::resolve(&ep, Some(8), Some(0x18), &[5, 8]).unwrap();
        let report = [0x18, 7, 0x0a, 8, 0, 0x20, 0, 9, 0, 0x20, 100];
        assert_eq!(
            cover.observe(Frame::decode(&report)?, None)?.positions,
            [(Axis::Lift, 100), (Axis::Tilt, 0)]
        );
        for wire in [
            &[0x18, 7, 0x0a, 8, 0, 0x20, 255][..],
            &[0x18, 7, 0x0a, 8, 0, 0x30, 50],
            &[0x10, 7, 0x0a, 8, 0, 0x20, 50],
            &[0x1c, 8, 0, 7, 0x0a, 8, 0, 0x20, 50],
        ] {
            assert!(
                cover
                    .observe(Frame::decode(wire)?, None)?
                    .positions
                    .is_empty()
            );
        }
        let response = Frame::decode(&[0x18, 7, 1, 8, 0, 0, 0x20, 73])?;
        assert!(cover.observe(response, None)?.positions.is_empty());
        let wrong_axis = cover.observe(response, Some(Axis::Tilt))?;
        assert!(!wrong_axis.complete && wrong_axis.positions.is_empty());
        let success = cover.observe(response, Some(Axis::Lift))?;
        assert!(success.complete);
        assert_eq!(success.positions, [(Axis::Lift, 27)]);
        let failed = cover.observe(Frame::decode(&[0x18, 7, 1, 8, 0, 0x86])?, Some(Axis::Lift))?;
        assert!(failed.complete && failed.positions.is_empty());
        Ok(())
    }
}
