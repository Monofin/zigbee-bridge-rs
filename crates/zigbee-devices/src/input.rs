//! Reports and cluster-specific commands for reviewed remote and motion
//! families. The owner passes monotonic time, drives expiry and drops this
//! state with the membership. This code owns no task, connection or timer.
mod reading;
pub use reading::Reading;
use crate::Endpoint;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value as Json;
use std::time::{Duration, Instant};
use zigbee_zcl::{
    Direction, Frame, FrameType,
    cluster::{Command, IasZone, LevelControl, OnOff},
    foundation::{GeneralCommand, Payload},
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Family {
    IasMotion,
    IasMotionReports,
    IasVibration,
    StandardMotion,
    TimedMotion,
    LumiMotion,
    Ewelink,
    IkeaSwitch,
    IkeaRemote,
    Somrig,
    SonoffRelay,
}
#[derive(Deserialize)]
struct Profile(Family, String, String, u8, u16, Vec<u16>, Vec<u16>, #[serde(default)] Reports);

/// An explicit model and endpoint can produce unsolicited reports for a cluster
/// absent from its descriptor. This does not grant read or binding capabilities.
#[derive(Default, Deserialize)]
struct Reports {
    #[serde(default)]
    illuminance: bool,
}
pub struct Inputs(Vec<Profile>);
impl Inputs {
    pub fn builtin() -> Result<Self> {
        Ok(Self(serde_json::from_str(include_str!(
            "../profiles/inputs.json"
        ))?))
    }
    pub fn resolve(
        &self,
        maker: &str,
        model: &str,
        ep: &Endpoint<'_>,
        device_type: u16,
    ) -> Option<Input> {
        if ep.profile != 0x104 {
            return None;
        }
        let mut candidates = self.0.iter().filter(|p| {
            p.1 == maker
                && p.2 == model
                && p.3 == ep.id
                && p.4 == device_type
                && p.5.iter().all(|c| ep.servers.contains(c))
                && p.6.iter().all(|c| ep.clients.contains(c))
        });
        let p = candidates.next()?;
        candidates.next().is_none().then_some(Input {
            lux_server: ep.servers.contains(&0x0400),
            extended_relay_actions: false,
            family: p.0,
            ep: ep.id,
            deadline: None,
            vibration_timeout: 90,
            left: None,
            lumi_seconds: 30,
            illuminance: ep.servers.contains(&0x400) || p.7.illuminance,
        })
    }
}
pub struct Input {
    family: Family,
    lux_server: bool,
    extended_relay_actions: bool,
    ep: u8,
    deadline: Option<Instant>,
    vibration_timeout: u32,
    left: Option<bool>,
    lumi_seconds: u16,
    illuminance: bool,
}
pub type Effects = Vec<(&'static str, Json)>;
const REMOTE: &[&str] = &[
    "toggle",
    "brightness_up_click",
    "brightness_down_click",
    "brightness_up_hold",
    "brightness_up_release",
    "brightness_down_hold",
    "brightness_down_release",
    "toggle_hold",
    "arrow_left_click",
    "arrow_left_hold",
    "arrow_left_release",
    "arrow_right_click",
    "arrow_right_hold",
    "arrow_right_release",
];
const SOMRIG_1: &[&str] = &[
    "1_initial_press",
    "1_long_press",
    "1_short_release",
    "1_long_release",
    "1_double_press",
];
const SOMRIG_2: &[&str] = &[
    "2_initial_press",
    "2_long_press",
    "2_short_release",
    "2_long_release",
    "2_double_press",
];

// The P1 reports millivolts. Its converter uses a linear 2850–3000 mV battery curve.
fn lumi_diagnostic(id: u16, value: zigbee_zcl::value::Value<'_>, out: &mut Effects) {
    match (id, value.data_type().id(), value.as_bytes()) {
        (1, 0x21, [lo, hi]) => {
            let mv = u16::from_le_bytes([*lo, *hi]);
            if mv != u16::MAX {
                out.push(("voltage", mv.into()));
                let percent = (f64::from(mv.clamp(2850, 3000) - 2850) * 100.0 / 150.0).round();
                out.push(("battery", percent.into()));
            }
        }
        (3, 0x28, [raw]) if *raw != 0x80 => {
            out.push(("device_temperature", i8::from_le_bytes([*raw]).into()));
        }
        _ => {}
    }
}

impl Input {
    /// SONOFF adds the detached-paddle event attribute from ZBMINIR2 1.1.0.
    pub fn set_software_build(&mut self, model: &str, build: &str) {
        self.extended_relay_actions = false;
        if self.family != Family::SonoffRelay || model != "ZBMINIR2" { return; }
        let mut components = build.split('.').map(str::parse::<u32>);
        let Some(Ok(major)) = components.next() else { return };
        let Ok(minor) = components.next().unwrap_or(Ok(0)) else { return };
        self.extended_relay_actions = components.all(|part| part.is_ok()) && (major, minor) >= (1, 1);
    }

    pub fn reading(&self, key: &str) -> Option<Reading> {
        let reading = Reading::from_property(key)?;
        (self.family == Family::StandardMotion && (reading == Reading::Occupancy || self.lux_server))
            .then_some(reading)
    }

    pub fn actions(&self) -> &'static [&'static str] {
        match self.family {
            Family::SonoffRelay if self.extended_relay_actions =>
                &["toggle", "double_click", "long_press", "switch_on", "switch_off"],
            Family::SonoffRelay => &["toggle"],
            Family::Ewelink => &["single", "double", "long"],
            Family::IkeaSwitch => &[
                "on",
                "off",
                "brightness_move_up",
                "brightness_move_down",
                "brightness_stop",
            ],
            Family::IkeaRemote => REMOTE,
            Family::Somrig => {
                if self.ep == 1 {
                    SOMRIG_1
                } else {
                    SOMRIG_2
                }
            }
            _ => &[],
        }
    }
    pub fn other_properties(&self) -> &'static [&'static str] {
        match self.family {
            Family::TimedMotion => &[
                "illuminance_above_threshold",
                "requested_brightness_level",
                "requested_brightness_percent",
            ],
            Family::LumiMotion => &["illuminance", "device_temperature", "battery", "voltage"],
            Family::StandardMotion if self.illuminance => &["illuminance"],
            Family::IkeaRemote => &["action_duration"],
            _ => &[],
        }
    }
    pub fn publishes(&self, key: &str) -> bool {
        (key == "action" && !self.actions().is_empty()) || self.other_properties().contains(&key)
            || self.binary().iter().any(|&(name, _)| name == key)
    }
    pub fn binary(&self) -> &'static [(&'static str, &'static str)] {
        match self.family {
            Family::IasMotion | Family::IasMotionReports => &[
                ("occupancy", "occupancy"),
                ("tamper", "tamper"),
                ("battery_low", "battery"),
            ],
            Family::IasVibration => &[("vibration", "vibration")],
            Family::TimedMotion | Family::LumiMotion | Family::StandardMotion => {
                &[("occupancy", "occupancy")]
            }
            _ => &[],
        }
    }
    pub fn handles(&self, cl: u16) -> bool {
        match self.family {
            Family::IasMotion | Family::IasMotionReports | Family::IasVibration => cl == 0x500,
            Family::LumiMotion => cl == 0xfcc0,
            Family::StandardMotion => cl == 0x406 || (self.illuminance && cl == 0x400),
            Family::TimedMotion | Family::IkeaSwitch => matches!(cl, 6 | 8),
            Family::SonoffRelay => cl == 6 || (self.extended_relay_actions && cl == 0xfc11),
            Family::Ewelink => cl == 6,
            Family::IkeaRemote => matches!(cl, 5 | 6 | 8),
            Family::Somrig => cl == 0xfc80,
        }
    }
    pub fn reset(&mut self) {
        self.deadline = None;
        self.left = None;
        self.lumi_seconds = 30;
    }
    /// The bridge owns retry scheduling when its bounded state queue is full.
    pub fn retry_clear(&mut self, at: Instant) {self.deadline=Some(at);}
    pub fn set_vibration_timeout(&mut self, seconds: u32) {
        self.vibration_timeout = seconds;
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub fn expire(&mut self, now: Instant) -> Option<(&'static str, Json)> {
        if self.deadline.is_some_and(|d| now >= d) {
            self.deadline = None;
            Some((
                if self.family == Family::IasVibration {
                    "vibration"
                } else {
                    "occupancy"
                },
                false.into(),
            ))
        } else {
            None
        }
    }
    fn ias(&mut self, status: u16, now: Instant) -> Effects {
        let alarm = status & 1 != 0;
        if self.family == Family::IasVibration {
            self.deadline = (alarm && self.vibration_timeout != 0)
                .then(|| now + Duration::from_secs(self.vibration_timeout.into()));
            vec![("vibration", alarm.into())]
        } else {
            vec![
                ("occupancy", alarm.into()),
                ("tamper", (status & 4 != 0).into()),
                ("battery_low", (status & 8 != 0).into()),
            ]
        }
    }
    pub fn handle(&mut self, cl: u16, frame: Frame<'_>, now: Instant) -> Result<Effects> {
        let mut out = vec![];
        if !self.handles(cl) {
            return Ok(out);
        }
        let h = frame.header;
        let attributed = h.direction == Direction::ServerToClient
            && h.general_command() == Some(GeneralCommand::ReportAttributes);
        if attributed {
            if h.manufacturer.is_some()
                && !(self.family == Family::LumiMotion && h.manufacturer == Some(0x115f))
                && !(self.family == Family::SonoffRelay && cl == 0xfc11 && h.manufacturer == Some(0x1286))
            {
                return Ok(out);
            }
            let Payload::ReportAttributes(attrs) = Payload::decode(frame)? else {
                unreachable!()
            };
            for r in attrs {
                let k = r.value.data_type().id();
                match (self.family, cl, r.id, k, r.value.as_bytes()) {
                    (Family::IasMotionReports, 0x500, 2, 0x19, [lo, hi]) => {
                        out.extend(self.ias(u16::from_le_bytes([*lo, *hi]), now))
                    }
                    (Family::StandardMotion, 0x0406, 0, _, _) => out.extend(Reading::Occupancy.observe(r.value)),
                    (Family::StandardMotion, 0x0400, 0, _, _) => out.extend(Reading::Illuminance.observe(r.value)),
                    (Family::SonoffRelay, 0xfc11, 0x28, 0x20, [n @ 2..=5]) if self.extended_relay_actions => {
                        out.push(("action", self.actions()[usize::from(*n) - 1].into()));
                    }
                    (Family::LumiMotion, 0xfcc0, 0xf7, 0x41 | 0x42, [len, bytes @ ..]) if usize::from(*len) == bytes.len() => {
                        let mut remaining = bytes;
                        while let Some((&id, typed)) = remaining.split_first() {
                            let (value, tail) = zigbee_zcl::value::Value::split_typed(typed)?;
                            lumi_diagnostic(u16::from(id), value, &mut out);
                            remaining = tail;
                        }
                    }
                    (Family::LumiMotion, 0xfcc0, id @ (1 | 3), _, _) => {
                        lumi_diagnostic(id, r.value, &mut out);
                    }
                    (Family::LumiMotion, 0xfcc0, 0x0102, 0x20, [n]) if *n > 0 => {
                        self.lumi_seconds = u16::from(*n)
                    }
                    (Family::LumiMotion, 0xfcc0, 0x0112, 0x23, bytes) => {
                        let n = u32::from_le_bytes(bytes.try_into()?);
                        self.deadline =
                            Some(now + Duration::from_secs(u64::from(self.lumi_seconds) + 2));
                        out.push(("occupancy", true.into()));
                        if n >= 65536 {
                            out.push((
                                "illuminance",
                                if n > 130536 { 0 } else { n - 65536 }.into(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            return Ok(out);
        }
        if h.frame_type != FrameType::ClusterSpecific {
            return Ok(out);
        }
        if self.family == Family::Somrig
            && h.direction == Direction::ClientToServer
            && h.manufacturer == Some(0x117c)
            && frame.payload.len() == 1
        {
            let id = match h.command_id {
                1 => 0,
                2 => 1,
                3 => 2,
                4 => 3,
                6 => 4,
                _ => return Ok(out),
            };
            return Ok(vec![("action", self.actions()[id].into())]);
        }
        if self.family == Family::IkeaRemote
            && cl == 5
            && h.direction == Direction::ClientToServer
            && matches!(h.manufacturer, None | Some(0x117c))
        {
            let action = match (h.command_id, frame.payload) {
                (7, [lo, hi, _, _]) => {
                    let value = u16::from_le_bytes([*lo, *hi]);
                    if value == 2 {
                        return Ok(out);
                    }
                    if value == 257 {
                        "arrow_left_click"
                    } else {
                        "arrow_right_click"
                    }
                }
                (8, [lo, hi]) => {
                    let left = u16::from_le_bytes([*lo, *hi]) == 3329;
                    self.left = Some(left);
                    if left {
                        "arrow_left_hold"
                    } else {
                        "arrow_right_hold"
                    }
                }
                (9, [lo, hi]) => {
                    let Some(left) = self.left.take() else {
                        return Ok(out);
                    };
                    out.push((
                        "action_duration",
                        (f64::from(u16::from_le_bytes([*lo, *hi])) / 1000.0).into(),
                    ));
                    if left {
                        "arrow_left_release"
                    } else {
                        "arrow_right_release"
                    }
                }
                _ => return Ok(out),
            };
            out.push(("action", action.into()));
            return Ok(out);
        }
        if h.manufacturer.is_some() {
            return Ok(out);
        }
        let Ok(command) = Command::decode(cl, frame) else {
            return Ok(out);
        };
        let action = match (self.family, command) {
            (Family::SonoffRelay, Command::OnOff(OnOff::Toggle {})) => "toggle",
            (
                Family::IasMotion | Family::IasMotionReports | Family::IasVibration,
                Command::IasZone(IasZone::ZoneStatusChangeNotification { zone_status, .. }),
            ) => return Ok(self.ias(zone_status, now)),
            (
                Family::TimedMotion,
                Command::OnOff(OnOff::OnWithTimedOff {
                    on_off_control,
                    on_time,
                    ..
                }),
            ) => {
                let bright = on_off_control & 1 != 0;
                if !bright || self.deadline.is_some_and(|end| end > now) {
                    self.deadline = (on_time != 0)
                        .then_some(now + Duration::from_millis(u64::from(on_time) * 100));
                    out.extend([
                        ("occupancy", true.into()),
                        ("illuminance_above_threshold", bright.into()),
                    ]);
                }
                return Ok(out);
            }
            (
                Family::TimedMotion,
                Command::LevelControl(LevelControl::MoveToLevelWithOnOff { level, .. }),
            ) => {
                out.extend([
                    ("requested_brightness_level", level.into()),
                    (
                        "requested_brightness_percent",
                        (f64::from(level) * 100.0 / 254.0).round().into(),
                    ),
                ]);
                return Ok(out);
            }
            (Family::Ewelink, Command::OnOff(OnOff::Toggle {})) => "single",
            (Family::Ewelink, Command::OnOff(OnOff::On {})) => "double",
            (Family::Ewelink, Command::OnOff(OnOff::Off {})) => "long",
            (Family::IkeaSwitch, Command::OnOff(OnOff::On {})) => "on",
            (Family::IkeaSwitch, Command::OnOff(OnOff::Off {})) => "off",
            (
                Family::IkeaSwitch,
                Command::LevelControl(
                    LevelControl::Move { move_mode, .. }
                    | LevelControl::MoveWithOnOff { move_mode, .. },
                ),
            ) => match move_mode {
                0 => "brightness_move_up",
                1 => "brightness_move_down",
                _ => return Ok(out),
            },
            (
                Family::IkeaSwitch,
                Command::LevelControl(
                    LevelControl::Stop { .. } | LevelControl::StopWithOnOff { .. },
                ),
            ) => "brightness_stop",
            (Family::IkeaRemote, Command::OnOff(OnOff::Toggle {})) => "toggle",
            (Family::IkeaRemote, Command::LevelControl(c)) => match c {
                LevelControl::StepWithOnOff { .. } => "brightness_up_click",
                LevelControl::Step { .. } => "brightness_down_click",
                LevelControl::MoveWithOnOff { .. } => "brightness_up_hold",
                LevelControl::StopWithOnOff { .. } => "brightness_up_release",
                LevelControl::Move { .. } => "brightness_down_hold",
                LevelControl::Stop { .. } => "brightness_down_release",
                LevelControl::MoveToLevelWithOnOff { .. } => "toggle_hold",
                _ => return Ok(out),
            },
            _ => return Ok(out),
        };
        out.push(("action", action.into()));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_relay_action_depends_on_model_build_and_frame() {
        let profiles = Inputs::builtin().unwrap();
        let ep = Endpoint { id: 1, profile: 260, servers: &[0,6,0xfc11], clients: &[6] };
        let now = Instant::now();
        for (model, build, extended) in [("ZBMINIR2","1.1.0",true), ("ZBMINIR2","1.0.9",false), ("MINI-ZBD","1.1.0",false)] {
            let mut input = profiles.resolve("SONOFF", model,&ep,2).unwrap();
            input.set_software_build(model,build);
            assert_eq!(input.actions().len(), if extended {5} else {1});
            let toggle = Frame::decode(&[0x11,1,2]).unwrap();
            assert_eq!(input.handle(6,toggle,now).unwrap(),vec![("action","toggle".into())]);
            for (value,action) in [(2,"double_click"),(3,"long_press"),(4,"switch_on"),(5,"switch_off")] {
                let raw=[0x18,1,10,0x28,0,0x20,value];
                let seen=input.handle(0xfc11,Frame::decode(&raw).unwrap(),now).unwrap();
                assert_eq!(seen,if extended {vec![("action",action.into())]} else {vec![]});
            }
            for raw in [&[0x18,1,10,0x28,0,0x30,2][..], &[0x1c,0x34,0x12,1,10,0x28,0,0x20,2], &[0x18,1,10,0x28,0,0x20,6]] {
                assert!(input.handle(0xfc11, Frame::decode(raw).unwrap(),now).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn vallhorn_illuminance_accepts_only_a_valid_report_on_the_profiled_endpoint() {
        let inputs = Inputs::builtin().unwrap();
        let ep = Endpoint {id:3, profile:260, servers:&[0,3,1030], clients:&[]};
        let resolve = |ep: &Endpoint<'_>| inputs.resolve("IKEA of Sweden", "VALLHORN Wireless Motion Sensor", ep, 263).unwrap();
        let mut allowed = resolve(&ep);
        let mut other_endpoint = resolve(&Endpoint {id:2, ..ep});
        let now = Instant::now();
        let observe = |input: &mut Input, payload: &[u8]| input.handle(0x0400, Frame::decode(payload).unwrap(), now).unwrap();
        let measured = [0x18,1,0x0a,0,0,0x21,0x11,0x27]; // 10001 logarithmic units = 10 lx
        assert!(observe(&mut other_endpoint, &measured).is_empty());
        assert!(inputs.resolve("other vendor", "VALLHORN Wireless Motion Sensor", &ep, 263).is_none());
        assert!(observe(&mut allowed, &[0x18,1,0x0a,0,0,0x21,0xff,0xff]).is_empty());
        assert!(observe(&mut allowed, &[0x18,1,0x0a,0,0,0x29,1,0]).is_empty());
        assert!(observe(&mut allowed, &[0x18,1,1,0,0,0,0x21,1,0]).is_empty());
        assert!(observe(&mut allowed, &[0x1c,0x7c,0x11,1,0x0a,0,0,0x21,1,0]).is_empty());
        assert_eq!(observe(&mut allowed, &measured), vec![("illuminance", 10.0.into())]);
    }

    #[test]
    fn lumi_heartbeat_uses_typed_records_and_rejects_truncated_tails() {
        let ep = Endpoint {id:1,profile:0x104,servers:&[0,0xfcc0],clients:&[]};
        let mut motion = Inputs::builtin().unwrap().resolve("LUMI","lumi.motion.ac02",&ep,263).unwrap();
        let packet = [0x1c,0x5f,0x11,1,0x0a,0xf7,0,0x41,7,1,0x21,0x6d,0x0b,3,0x28,0xfb];
        let values = motion.handle(0xfcc0,Frame::decode(&packet).unwrap(),Instant::now()).unwrap();
        assert_eq!(values, vec![("voltage", 2925.into()),("battery", 50.0.into()),("device_temperature", (-5).into())]);
        let mut bad = packet.to_vec();
        bad[8] += 1;
        bad.push(99);
        assert!(motion.handle(0xfcc0,Frame::decode(&bad).unwrap(),Instant::now()).is_err());
    }

    #[test]
    fn repeated_alarms_replace_one_deadline_and_leave_revokes_it() {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 1, 1280],
            clients: &[],
        };
        let mut sensor = Inputs::builtin()
            .unwrap()
            .resolve("_TZ3000_lqpt3mvr", "TS0210", &ep, 1026)
            .unwrap();
        let packet = [0x19, 10, 0, 1, 0, 0, 2, 0, 0];
        let now = Instant::now();
        assert_eq!(
            sensor
                .handle(0x500, Frame::decode(&packet).unwrap(), now)
                .unwrap(),
            vec![("vibration", true.into())]
        );
        sensor
            .handle(
                0x500,
                Frame::decode(&packet).unwrap(),
                now + Duration::from_secs(10),
            )
            .unwrap();
        assert!(sensor.expire(now + Duration::from_secs(90)).is_none());
        assert_eq!(
            sensor.expire(now + Duration::from_secs(100)),
            Some(("vibration", false.into()))
        );
        sensor
            .handle(0x500, Frame::decode(&packet).unwrap(), now)
            .unwrap();
        sensor.reset();
        assert!(sensor.expire(now + Duration::from_secs(200)).is_none());
        for seconds in [10, 0] {
            sensor.set_vibration_timeout(seconds);
            sensor.handle(0x500, Frame::decode(&packet).unwrap(), now).unwrap();
            assert_eq!(sensor.deadline(), (seconds != 0).then_some(now + Duration::from_secs(10)));
        }
    }
    #[test]
    fn ewelink_single_is_toggle_and_ikea_uses_on_not_double() {
        let ep = Endpoint {
            id: 1,
            profile: 260,
            servers: &[0, 1],
            clients: &[6, 8],
        };
        let inputs = Inputs::builtin().unwrap();
        let mut button = inputs.resolve("eWeLink", "SNZB-01P", &ep, 0).unwrap();
        let mut ikea = inputs
            .resolve("IKEA of Sweden", "TRADFRI on/off switch", &ep, 2080)
            .unwrap();
        let on = Frame::decode(&[1, 21, 1]).unwrap();
        let now = Instant::now();
        assert_eq!(
            button.handle(6, on, now).unwrap(),
            vec![("action", "double".into())]
        );
        assert_eq!(
            ikea.handle(6, on, now).unwrap(),
            vec![("action", "on".into())]
        );
        assert!(
            button
                .handle(6, Frame::decode(&[9, 1, 1]).unwrap(), now)
                .unwrap()
                .is_empty()
        );
    }
}
