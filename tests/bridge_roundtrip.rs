//! MQTT broker + actual serial worker and ASH link over a Linux PTY.
//! The simulated NCP resumes a saved network, handles device traffic, OTA and admission.
#![cfg(target_os = "linux")]

#[path = "../src/ash/frame.rs"]
mod wire;

use anyhow::{Context, Result, bail, ensure};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPort, SerialStream};
use zigbee_bridge_rs::{
    ash::Ezsp,
    bridge::{Bindings, Management, serve_managed},
    interview_log::{
        AttributeResult, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
    },
    mqtt,
    serial::Serial,
    session::Startup,
    store::Store,
};
use zigbee_zcl::{Direction, Frame, FrameType};

#[path = "support/ace.rs"]
mod ace;
#[path = "support/enrolment.rs"]
mod enrolment;
#[path = "support/endpoint_admin.rs"]
mod endpoint_admin;
#[path = "support/lock_control.rs"]
mod lock_control;
#[path = "support/admin_replay.rs"]
mod admin_replay;
#[path = "support/ota.rs"]
mod fixture;
#[path = "support/ikea_light.rs"]
mod ikea_light;
#[path = "support/hvac.rs"]
mod hvac;
#[path = "support/cover.rs"]
mod cover;
#[path = "support/cover_interview.rs"]
mod cover_interview;
#[path = "support/basic_io.rs"]
mod basic_io;
#[path = "support/environment.rs"]
mod environment;
#[path="support/equipment.rs"]
mod equipment;
#[path="support/template_output.rs"]
mod template_output;
#[path = "support/meter.rs"]
mod meter;
#[path = "support/state_feedback.rs"]
mod state_feedback;

const COUNT: u16 = 48;
#[path = "support/ezsp19.rs"]
mod ezsp19;
#[path = "support/cold_observations.rs"]
mod cold_observations;

const NETWORK: zigbee_bridge_rs::coordinator::Network = zigbee_bridge_rs::coordinator::Network {
    channel: 20,
    pan: 0x729c,
    extended_pan: [1, 2, 3, 4, 5, 6, 7, 8],
};
const ENDPOINT: u8 = 4;
const BUTTON: u16 = COUNT + 1;
fn endpoint(device: u16) -> u8 {
    if device == BUTTON || device == COUNT { 1 } else { ENDPOINT }
}

fn ieee(light: u16) -> u64 {
    0x0200_0000_0000_0000 + u64::from(light)
}

fn short(light: u16) -> u16 {
    0x4300 + light
}

fn bindings(cold: bool) -> Result<Bindings> {
    let mut bindings = Bindings::default();
    for light in 1..=BUTTON {
        if cold && light == cold_observations::MOTION {
            cold_observations::bind_sensor(&mut bindings)?;
            continue;
        }
        let mut observations = vec![
            Observation::Target {
                ieee: format!("{:016x}", ieee(light)),
                // A previous interview's short address must never be used for TX.
                short: 0x1200 + light,
                identity_source: IdentitySource::Announcement,
            },
            Observation::SimpleDescriptor {
                sequence: 3,
                endpoint: endpoint(light),
                descriptor: Descriptor {
                    profile: 0x0104,
                    device: match light {BUTTON=>0, COUNT=>0x0102, _=>0x0101},
                    version: 1,
                    servers: if light == BUTTON {
                        vec![0, 1, 6, 0xe000]
                    } else if light==COUNT {
                        vec![0,3,6,8,0x300]
                    } else if light == 1 {
                        vec![4,5,6,8,9]
                    } else {
                        vec![6, 8]
                    },
                    clients: vec![0x19],
                },
            },
        ];
        if matches!(light, BUTTON|COUNT) {
            let (maker,model)=if light==BUTTON {("_TZ3000_qgwcxxws","TS0041")} else {("IKEA of Sweden","TRADFRI bulb E27 CWS opal 600lm")};
            for (attribute, text) in [(4,maker), (5,model)] {
                let mut bytes = vec![text.len().try_into()?];
                bytes.extend(text.bytes());
                observations.push(Observation::Attribute {
                    sequence: 4,
                    endpoint: 1,
                    cluster: 0,
                    attribute,
                    result: AttributeResult::Value {
                        data_type: 0x42,
                        bytes,
                    },
                });
            }
        }
        if light==COUNT {
            observations.push(Observation::Attribute {sequence:5, endpoint:1, cluster:0x300, attribute:0x400a,
                result:AttributeResult::Value {data_type:0x19,bytes:vec![15,0]}});
        }
        observations.push(Observation::Finished);
        let report = InterviewReport {
            finished: true,
            entries: observations
                .into_iter()
                .map(|observation| Entry {
                    unix_ms: 0,
                    observation,
                })
                .collect(),
        };
        bindings.add(
            &report,
            &BTreeMap::from([(
                ieee(light),
                if light == BUTTON {
                    "button".to_owned()
                } else {
                    format!("lamp-{light}")
                },
            )]),
        )?;
    }
    meter::bind(&mut bindings)?;
    environment::bind(&mut bindings)?;
    basic_io::bind(&mut bindings)?;
    equipment::bind(&mut bindings)?;
    template_output::bind(&mut bindings)?;
    hvac::bind(&mut bindings)?;
    cover_interview::bind(&mut bindings, (ieee(cover::DEVICE), endpoint(cover::DEVICE)), 0xc05e, true, 8, 0x18, &[5,8])?;
    Ok(bindings)
}

fn incoming(light: u16, cluster: u16, zcl: &[u8]) -> Vec<u8> {
    let mut callback = vec![0, 0x90, 1, 0x45, 0]; // EZSP incomingMessageHandler
    let mut args = [0; 19];
    args[1..3].copy_from_slice(&0x0104u16.to_le_bytes());
    args[3..5].copy_from_slice(&cluster.to_le_bytes());
    args[5] = endpoint(light);
    args[6] = 1;
    args[12] = 142;
    args[14..16].copy_from_slice(&short(light).to_le_bytes());
    args[18] = zcl.len().try_into().unwrap();
    callback.extend(args);
    callback.extend(zcl);
    callback
}

// Return the identity in a unicast ZDO response, independent of the host codec.
fn address_response(light: u16, cluster: u16, seq: u8) -> Vec<u8> {
    let mut body = vec![seq, 0];
    body.extend(ieee(light).to_le_bytes());
    body.extend(short(light).to_le_bytes());
    let mut packet = incoming(light, cluster, &body);
    packet[6..8].fill(0); // ZDO profile
    packet[10..12].fill(0); // source and destination endpoints
    packet
}

struct Lamps {
    output:template_output::Output,
    commissioning:equipment::Replay,
    registered_endpoints: BTreeSet<u8>,
    restored: bool,
    ikea: ikea_light::Lamp,
    resumed: bool,
    concentrator_enabled: bool,
    discovery_off: bool,
    child_limit: u16,
    tx_power: u8,
    permit_broadcast_open: bool,
    enrolment: enrolment::Radio,
    switched: BTreeSet<u16>,
    buttons_sent: bool,
    scenario: Scenario,
    walk_address_requested: bool,
    battery_read: Option<u8>,
    poll_stopped: bool,
    upgraded: bool,
    bindings_seen: BTreeSet<(u16, u8)>,
    bind_conflict_sent: bool,
}

impl Lamps {
    fn command(&mut self, data: &[u8], queue: &mut VecDeque<Vec<u8>>) -> Result<()> {
        if data == [0, 0, 0, 13] {
            queue.push_back(vec![0, 0x80, 0, 13, 2, 0x40, 0x74]);
            return Ok(());
        }
        let [seq, 0, 1, id, high, args @ ..] = data else {
            bail!("invalid host command")
        };
        let mut response = vec![*seq, 0x80, 1, *id, *high];
        if self.discovery_off {
            ensure!((*id, *high, args) == (0x5a, 0, &[1][..]), "reported ready with discovery off");
        }
        if self.restored {
            let command = u16::from_le_bytes([*id, *high]);
            // Resume always denies admissions and explicitly closes the local window.
            let repeated_effect = match command {
                0x22 => args != [0],
                0x55 => args != [0, 0],
                // The sole durable member of the restored fixture announced RX-on-idle.
                0x34 => {
                    let id = enrolment::NEW;
                    let profile = if self.enrolment.profile == 0xc05e { [0x5e, 0xc0] } else { [4, 1] };
                    let walk = args.get(1..3) == Some(&short(id).to_le_bytes())
                        && args.get(3..5) == Some(profile.as_slice())
                        && args.get(5..9) == Some(&[0, 0, 1, endpoint(id)])
                        && args.get(14) == Some(&0xff)
                        && matches!(args.get(16..), Some([0, _, 0, 0, 0]));
                    if walk {
                        response.extend([0, 1]);
                        queue.push_back(response);
                        let mut reply = incoming(id, 0, &[0x18, args[17], 1, 0, 0, 0, 0x20, 3]);
                        reply[6..8].copy_from_slice(&profile);
                        queue.push_back(reply);
                        return Ok(());
                    }
                    true
                }
                0x36 | 0x111 => true,
                _ => false,
            };
            ensure!(!repeated_effect, "restored request issued EZSP {command:#06x}");
        }
        // A peer can request a default response during any of the scripted
        // device transactions, including the enrollment ownership survey.
        if u16::from_le_bytes([*id, *high]) == 0x34 && args.get(14) == Some(&0xff)
            && matches!(args.get(3..5), Some([4,1] | [0x5e,0xc0]))
            && let Ok(frame) = Frame::decode(&args[16..])
            && frame.header.general_command() == Some(zigbee_zcl::foundation::GeneralCommand::DefaultResponse)
        {
            ensure!(frame.header.disable_default_response && frame.payload.len() == 2 && (1..=4).contains(&args[7]));
            response.extend([0, 8]);
            queue.push_back(response);
            return Ok(());
        }
        if u16::from_le_bytes([*id, *high]) == 0x22 && args != [0] && !self.restored {
            ensure!(self.poll_stopped, "Poll Control session must end after the battery read");
        }
        if let Some((result, callbacks)) = self
            .enrolment
            .command(u16::from_le_bytes([*id, *high]), args)?
        {
            response.extend(result);
            queue.push_back(response);
            queue.extend(callbacks);
            return Ok(());
        }
        let mut callbacks = Vec::new();
        match u16::from_le_bytes([*id, *high]) {
            0x10 => {
                ensure!(self.resumed && args == [1, 0xf9, 0xff, 5, 0, 60, 0, 3, 1, 0]);
                self.concentrator_enabled = true;
                response.push(0);
            }
            0x5a => {
                ensure!(self.concentrator_enabled);
                let delay = match args {
                    [2] if self.scenario == Scenario::Walk => {
                        self.discovery_off = true;
                        u32::MAX
                    }
                    [1] if self.discovery_off => {
                        self.discovery_off = false;
                        1249
                    }
                    [2] => 1249,
                    _ => bail!("unexpected route discovery mode"),
                };
                response.extend(delay.to_le_bytes());
                if self.scenario == Scenario::Observations { callbacks.extend(cold_observations::frames()); }
            }
            0x102 => {
                ensure!(args == [0x64, 0xee, 1, 0, 0, 0, 0, 0] && !self.resumed);
                response.push(0);
                response.extend(16_u32.to_le_bytes());
                response.extend(NETWORK.pan.to_le_bytes());
                response.extend([0, NETWORK.channel, 2, 1, 0, 0]);
                response.extend(NETWORK.extended_pan);
            }
            0x52 if args == [0x11] => {
                response.push(0);
                response.extend(self.child_limit.to_le_bytes());
            }
            0x53 if args == [0x11, 64, 0] && !self.resumed => {
                self.child_limit = 64;
                response.push(0);
            }
            0x99 if args == [20] && self.resumed => {
                self.tx_power = 20;
                response.push(0);
            }
            0x53 => {
                ensure!(matches!(args, [12, 2, 0] | [13, 5, 0] | [42, 1, 0]));
                response.push(0);
            }
            0x02 => {
                let [ep @ 1..=4,4,1,7,0,0,n_in,n_out,clusters @ ..]=args else {bail!("bad coordinator endpoint")};
                ensure!(clusters.len()==2*(usize::from(*n_in)+usize::from(*n_out)));
                ensure!(self.registered_endpoints.insert(*ep) && 8 + clusters.len() <= 64);
                let (servers,clients)=clusters.split_at(2*usize::from(*n_in));
                let required: &[u16] = match ep {
                    1 => {
                        ensure!(servers.chunks_exact(2).any(|c| c==[0x0a,0]), "missing Time server");
                        &[0x0020,0x0400,0x0404,0x0406,0x0500]
                    }
                    2 => &[0x0009,0x000c,0x0014,0x0204,0x0502],
                    3 => &[0x0401,0x0408,0x040d],
                    _ => &[0x042a,0x042b],
                };
                for cluster in required {
                    ensure!(clients.chunks_exact(2).any(|c| c==cluster.to_le_bytes()),
                        "local endpoint {ep} missing client {cluster:#06x}");
                }
                response.push(0);
            }
            0x17 => {
                ensure!(!self.resumed && args == [0, 0], "resume must run once");
                ensure!(self.registered_endpoints.len()==4, "resume before endpoint registration");
                self.resumed = true;
                response.push(0);
                callbacks.push(vec![0, 0x90, 1, 0x19, 0, 0x90]);
            }
            0x28 => {
                ensure!(self.resumed && args.is_empty());
                response.extend([0, 1]);
                response.extend(NETWORK.extended_pan);
                response.extend(NETWORK.pan.to_le_bytes());
                response.extend([self.tx_power, NETWORK.channel, 0, 0, 0, 0]);
                response.extend((1u32 << NETWORK.channel).to_le_bytes());
            }
            0x27 => {
                ensure!(self.resumed && args.is_empty());
                response.extend([0, 0]);
            }
            0x36 if args.starts_with(&[0xfd, 0xff]) => {
                ensure!(args.len() == 27 && args[2..14] == [0; 12] && args[14..16] == [0xff,11]);
                let peer = if self.scenario == Scenario::Walk {
                    self.walk_address_requested = true;
                    1
                } else { 3 };
                ensure!(args[17..25] == ieee(peer).to_le_bytes() && args[25..] == [0,0]);
                let mut reply = address_response(peer,0x8000,args[16]);
                if self.scenario == Scenario::Walk { reply[19..21].copy_from_slice(&0x7777u16.to_le_bytes()); }
                callbacks.push(reply);
                response.extend([0,1]);
            }
            0x34 if args.get(3..9) == Some(&[0,0,1,0,0,0]) => {
                let peer = if self.scenario == Scenario::Observations {
                    let id = u16::from_le_bytes([args[1], args[2]]) - short(0);
                    ensure!(matches!(id, BUTTON | cold_observations::MOTION));
                    id
                } else { 4 };
                ensure!(args.len() == 21 && args[1..3] == short(peer).to_le_bytes());
                ensure!(args[15] == 5 && args[17..19] == short(peer).to_le_bytes() && args[19..] == [0,0]);
                let mut reply = address_response(peer,0x8001,args[16]);
                if self.scenario == Scenario::Observations {
                    // An awake router answers for its sleeping neighbor.
                    reply[19..21].copy_from_slice(&0x7777u16.to_le_bytes());
                }
                callbacks.push(reply);
                if peer == BUTTON {
                    callbacks.push(incoming(BUTTON, 6, &[0x11, 0x34, 0xfd, 0]));
                }
                if peer == 4 {callbacks.push(incoming(4,8,&[8,0xee,10,0,0,0x20,4]));}
                response.extend([0,1]);
            }
            0x36 => {
                ensure!(
                    self.resumed
                        && matches!(
                            args,
                            [
                                0xfc,
                                0xff,
                                0,
                                0,
                                0x36,
                                0,
                                0,
                                0,
                                0,
                                0,
                                0,
                                0,
                                0,
                                0,
                                0xff,
                                3,
                                _,
                                0 | 3,
                                1
                            ]
                        ),
                    "invalid network-wide permit join broadcast"
                );
                let open = args[17] != 0;
                ensure!(
                    open != self.permit_broadcast_open,
                    "duplicate broadcast permit transition"
                );
                self.permit_broadcast_open = open;
                response.extend([0, 4]);
            }
            0x26 => response.extend(ieee(0).to_le_bytes()),
            0x18 => response.push(if self.resumed { 2 } else { 0 }),
            0x60 => {
                let eui = u64::from_le_bytes(args.try_into()?);
                let light = eui
                    .checked_sub(ieee(0))
                    .and_then(|n| u16::try_from(n).ok())
                    .context("unknown IEEE")?;
                ensure!((1..=template_output::DEVICE).contains(&light), "unregistered light");
                // The adapter forgets the forward lookup between a successful
                // write and its readback. The original transaction remains current.
                let miss = light == 3
                    || (light == template_output::DEVICE && self.output.awaiting_readback())
                    || (self.scenario == Scenario::Observations && matches!(light, BUTTON | cold_observations::MOTION))
                    || (self.scenario == Scenario::Walk && light == 1);
                response.extend(if miss { u16::MAX } else { short(light) }.to_le_bytes());
            }
            0x75 => {
                ensure!(args.len()==9 && args[..8]==ieee(equipment::DEVICE).to_le_bytes() && args[8]==1);
                response.push(3);
            }
            0x61 => {
                let node = u16::from_le_bytes(args.try_into()?);
                let light = node.checked_sub(short(0)).context("unknown sender")?;
                ensure!((1..=template_output::DEVICE).contains(&light), "bad source");
                if light == 5 && self.bindings_seen.contains(&(5,0x21)) && !self.bind_conflict_sent {
                    // The source has already acknowledged the first cluster. Its
                    // conflict callback precedes the NCP's lookup response.
                    let mut conflict = vec![0,0x90,1,0x7c,0];
                    conflict.extend(node.to_le_bytes());
                    queue.push_back(conflict);
                    self.bind_conflict_sent = true;
                }
                response.push(u8::from(light == 4 || (self.scenario == Scenario::Observations && matches!(light, BUTTON | cold_observations::MOTION))));
                response.extend(ieee(light).to_le_bytes());
            }
            0x34 if args.get(3..5) == Some(&[0, 0]) => {
                let [
                    0,
                    lo,
                    hi,
                    0,
                    0,
                    cl,
                    0,
                    0,
                    0,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    len,
                    tsn,
                    body @ ..,
                ] = args
                else {
                    bail!("malformed ZDO TX")
                };
                let device = u16::from_le_bytes([*lo, *hi]).checked_sub(short(0)).context("unknown binding source")?;
                ensure!(
                    matches!((device, cl), (1, 0x21 | 0x22) | (2 | 5 | 6, 0x21))
                        && *len == 22,
                    "wrong ZDO destination or length"
                );
                let mut expected = ieee(device).to_le_bytes().to_vec();
                expected.extend([endpoint(device), 6, 0, 3]);
                expected.extend(ieee(0).to_le_bytes());
                expected.push(1);
                ensure!(body == expected, "wrong ZDO binding tuple");
                ensure!(self.bindings_seen.insert((device, *cl)), "duplicate ZDO bind or unbind");
                response.extend([0, 7]);
                if device == 2 {
                    // Accept the bind on air and withhold its ZDO response. An unrelated
                    // level report lets the observer stop at this precise point.
                    queue.push_back(response);
                    queue.push_back(incoming(2, 8, &[8, 33, 10, 0, 0, 0x20, 78]));
                    return Ok(());
                }
                if matches!(device,5|6) {
                    // Reaching the other lamp proves the blocked operation
                    // released its slot and unrelated work could proceed.
                    ensure!(device!=6 || self.bind_conflict_sent);
                    let mut cb = incoming(device, 0x8021, &[*tsn,0]);
                    cb[6..8].fill(0);
                    cb[10..12].fill(0);
                    queue.push_back(response);
                    queue.push_back(cb);
                    return Ok(());
                }
                for (n, seq, status) in [
                    (2, *tsn, 0x84),
                    (1, tsn.wrapping_sub(1), 0x84),
                    (1, *tsn, 0),
                ] {
                    let mut cb = incoming(n, 0x8000 | u16::from(*cl), &[seq, status]);
                    cb[6..8].fill(0);
                    cb[10..12].fill(0);
                    callbacks.push(cb);
                }
                if *cl == 0x22 {
                    callbacks.push(incoming(1, 0x19, &fixture::query(3, None)));
                }
            }
            0x34 if args.get(3..7)==Some(&[4,1,1,1]) && args.get(14)==Some(&0xff) => {
                callbacks.extend(lock_control::respond(args)?);
                response.extend([0,1]);
            }
            0x34 if args.get(3..5)==Some(&[4,1]) && matches!(args.get(5..7),Some([4|5|9,0])) => {
                callbacks.extend(endpoint_admin::command(args)?);
                response.extend([0,1]);
            }
            0x34 if args.get(3..7) == Some(&[4,1,0x20,0]) => {
                ensure!(args[1..3] == short(BUTTON).to_le_bytes() && args[7..9] == [1,endpoint(BUTTON)]);
                let frame = Frame::decode(&args[16..])?;
                ensure!(frame.header.disable_default_response && frame.header.direction == Direction::ClientToServer);
                match (frame.header.command_id, frame.payload) {
                    (0, [1,32,0]) if frame.header.sequence == 0xa7 => {
                        let tag = self.battery_read.take().context("unexpected fast-poll request")?;
                        // The simulated sleepy peer releases its read reply after entering fast poll.
                        callbacks.push(incoming(BUTTON,1,&[8,tag,1,0x21,0,0,0x20,173]));
                    }
                    (1, []) if self.battery_read.is_none() && !self.poll_stopped => self.poll_stopped=true,
                    _=> bail!("unexpected poll command {frame:?}"),
                }
                response.extend([0,0]);
            }
            0x34 if args.get(3..7) == Some(&[4,1,0,0]) => {
                ensure!(args[1..3] == short(1).to_le_bytes() && args[7..9] == [1, endpoint(1)], "availability probe sent to wrong device");
                if self.scenario == Scenario::Walk {
                    ensure!(self.walk_address_requested, "ping sent before address discovery");
                }
                let frame=Frame::decode(&args[16..])?;
                ensure!(frame.header.command_id==0 && frame.payload==[0,0],"expected Basic.zclVersion read");
                callbacks.push(incoming(1,0,&[8,frame.header.sequence,1,0,0,0,0x20,3]));
                response.extend([0,8]);
            }
            0x34 if args.get(1..3)==Some(&short(template_output::DEVICE).to_le_bytes())=>{
                response.extend([0,0]);callbacks.extend(self.output.respond(args)?);
            }
            0x34 if args.get(1..3)==Some(&short(equipment::DEVICE).to_le_bytes()) => {
                response.extend([0,0]);
                callbacks.extend(equipment::respond(&mut self.commissioning,args)?);
            }
            0x34 if args.get(1..3)==Some(&short(basic_io::PANEL).to_le_bytes()) => {
                response.extend([0,0]);
                callbacks.extend(basic_io::respond(args)?);
            }
            0x34 if args.get(1..3)==Some(&short(environment::DEVICE).to_le_bytes()) => {
                response.extend([0,0]);
                callbacks.extend(environment::respond(args)?);
            }
            0x34 if (meter::FIRST..=meter::LAST).any(|d| args.get(1..3) == Some(&short(d).to_le_bytes())) => {
                let device = u16::from_le_bytes(args[1..3].try_into()?) - short(0);
                response.extend([0, 0]);
                callbacks.extend(meter::respond(device, args)?);
            }
            0x34 if args.get(1..3)==Some(&short(cover::DEVICE).to_le_bytes()) => {
                response.extend([0,0]);
                callbacks.extend(cover::respond(args)?);
            }
            0x34 if args.get(1..3)==Some(&short(hvac::DEVICE).to_le_bytes()) => {
                response.extend([0,0]);
                callbacks.extend(hvac::respond(args)?);
            }
            0x34 if args.get(1..3)==Some(&short(COUNT).to_le_bytes()) && matches!(args.get(5..7),Some([8,0]|[0,3])) => {
                response.extend([0,0]);
                callbacks.extend(self.ikea.command(args)?);
            }
            0x34 if args.get(5..7) == Some(&[0x19, 0]) => {
                ensure!(
                    args[1..3] == short(1).to_le_bytes() && args[7..9] == [1, endpoint(1)],
                    "OTA reply to wrong destination"
                );
                let frame = Frame::decode(&args[16..])?;
                response.extend([0, 8]);
                let image = fixture::image(8, None, None);
                match (frame.header.command_id, frame.payload) {
                    (2, [0x98]) if !self.upgraded => {
                        callbacks.push(incoming(1, 8, &[8, 91, 10, 0, 0, 0x20, 77]))
                    }
                    (0, [0, 100]) => callbacks.push(incoming(1, 0x19, &fixture::query(3, None))),
                    (2, offer) if offer.first() == Some(&0) => {
                        ensure!(offer[5..9] == 8u32.to_le_bytes(), "wrong offer version");
                        callbacks.push(incoming(1, 0x19, &fixture::block(8, 0, 32)));
                    }
                    (5, block) => {
                        let offset = u32::from_le_bytes(block[9..13].try_into()?) as usize;
                        let bytes = &block[14..];
                        ensure!(
                            block[0] == 0
                                && !bytes.is_empty()
                                && bytes == &image[offset..offset + bytes.len()],
                            "incorrect OTA bytes"
                        );
                        let next = offset + bytes.len();
                        let request = if next < image.len() {
                            fixture::block(8, next as u32, 32)
                        } else {
                            vec![1, 10, 6, 0, 0x34, 0x12, 0x56, 0, 8, 0, 0, 0]
                        };
                        callbacks.push(incoming(1, 0x19, &request));
                    }
                    (7, _) => {
                        self.upgraded = true;
                        callbacks.push(incoming(1, 0x19, &fixture::query(8, None)));
                    }
                    (2, [0x98]) if self.upgraded => (),
                    _ => bail!("unexpected OTA reply: {frame:?}"),
                }
            }
            0x34 => {
                let [
                    0,
                    lo,
                    hi,
                    4,
                    1,
                    cl,
                    ch,
                    1,
                    ep,
                    _,
                    _,
                    _,
                    _,
                    _,
                    tag,
                    len,
                    payload @ ..,
                ] = args
                else {
                    bail!("invalid unicast")
                };
                let light = u16::from_le_bytes([*lo, *hi])
                    .checked_sub(short(0))
                    .context("stale TX address")?;
                ensure!((1..=BUTTON).contains(&light), "unexpected TX destination");
                ensure!(
                    u16::from_le_bytes([*cl, *ch]) == (if light == BUTTON { 1 } else { 6 })
                        && *ep == endpoint(light)
                        && *len as usize == payload.len(),
                    "wrong cluster or payload length: {args:02x?}"
                );
                let frame = Frame::decode(payload)?;
                ensure!(
                    frame.header.direction == Direction::ClientToServer
                        && frame.header.manufacturer.is_none()
                        && (frame.header.command_id == 0
                            || (self.scenario == Scenario::Feedback && light == state_feedback::LIGHT
                                && frame.header.frame_type == FrameType::ClusterSpecific
                                && frame.header.command_id == 1))
                        && frame.header.sequence == *tag,
                    "unexpected ZCL command"
                );
                response.extend([0, 0]); // Accepted by the adapter
                let mut sent = vec![0, 0x90, 1, 0x3f, 0];
                sent.extend([
                    0, *lo, *hi, 4, 1, *cl, *ch, 1, *ep, 0x40, 1, 0, 0, 0, *tag, 0, 0,
                ]);
                callbacks.push(sent);
                if self.scenario == Scenario::Feedback && light == state_feedback::LIGHT {
                    callbacks.extend(state_feedback::respond(frame, &mut self.switched)?);
                    queue.push_back(response);
                    queue.extend(callbacks);
                    return Ok(());
                }
                match frame.header.frame_type {
                    FrameType::ClusterSpecific => {
                        ensure!(
                            frame.payload.is_empty() && self.switched.insert(light),
                            "OFF sent twice"
                        );
                        // Unsolicited brightness interleaved with the command burst.
                        let level = if light == 4 { 240 } else { light as u8 };
                        callbacks.push(incoming(light, 8, &[8, 0xee, 10, 0, 0, 0x20, level]));
                    }
                    FrameType::General if light == BUTTON => {
                        ensure!(frame.payload == [0x21, 0], "wrong battery attribute");
                        callbacks.push(incoming(
                            BUTTON,
                            1,
                            &[8, tag.wrapping_add(1), 1, 0x21, 0, 0, 0x20, 20],
                        ));
                        callbacks.push(incoming(BUTTON, 1, &[8, *tag, 1, 0x20, 0, 0, 0x20, 30]));
                        self.battery_read=Some(*tag);
                        callbacks.push(incoming(BUTTON,0x20,&[0x19,0xa7,0]));
                    }
                    FrameType::General => {
                        ensure!(self.switched.contains(&light) && frame.payload == [0, 0], "unexpected state read");
                        if light != 1 {
                            callbacks.push(incoming(light, 6, &[8, *tag, 1, 0, 0, 0, 0x10, 0]));
                            queue.push_back(response);
                            queue.extend(callbacks);
                            return Ok(());
                        }
                        // Wrong sequence, different device, and wrong attribute on the exact same
                        // device/endpoint/cluster/sequence. None can satisfy this read.
                        callbacks.push(incoming(
                            1,
                            6,
                            &[8, tag.wrapping_add(1), 1, 0, 0, 0, 0x10, 1],
                        ));
                        callbacks.push(incoming(2, 6, &[8, *tag, 1, 0, 0, 0, 0x10, 1]));
                        callbacks.push(incoming(1, 6, &[8, *tag, 1, 1, 0, 0, 0x10, 1]));
                        callbacks.push(incoming(1, 6, &[8, *tag, 1, 0, 0, 0, 0x10, 0]));
                        // Two distinct singles back-to-back, followed by the double and hold.
                        for packet in [
                            [1, 145, 253, 0],
                            [1, 147, 253, 0],
                            [1, 146, 253, 1],
                            [1, 148, 253, 2],
                        ].into_iter().filter(|_| !self.buttons_sent) {
                            callbacks.push(incoming(BUTTON, 6, &packet));
                        }
                        self.buttons_sent = true;
                    }
                }
            }
            _ => bail!("unexpected EZSP id {id:#04x}"),
        }
        queue.push_back(response);
        queue.extend(callbacks);
        Ok(())
    }
}

async fn ncp(port: &mut SerialStream, protocol: u8, restored: bool) -> Result<()> {
    ncp_with_offline_reports(port, protocol, restored, None, Scenario::Commands).await
}

// During the outage rehearsal the broker stays gated until the coordinator
// has resolved both incoming report senders through the same live ASH session.
async fn ncp_with_offline_reports(port: &mut SerialStream, protocol: u8, restored: bool,
    mut observed: Option<tokio::sync::oneshot::Sender<()>>, scenario: Scenario) -> Result<()> {
    let mut injected = false;
    let mut lookups = 0;
    let mut native = ezsp19::Native::default();
    let mut decoder = wire::Decoder::default();
    let mut lamps = Lamps {
        output:Default::default(),
        commissioning:Default::default(),
        registered_endpoints: BTreeSet::new(),
        restored,
        ikea: Default::default(),
        resumed: false,
        concentrator_enabled: false,
        discovery_off: false,
        child_limit: 16,
        tx_power: 0,
        permit_broadcast_open: false,
        enrolment: enrolment::Radio::new(if protocol == 19 { 0xc05e } else { 0x0104 }),
        upgraded: false,
        bindings_seen: BTreeSet::new(),
        bind_conflict_sent:false,
        switched: BTreeSet::new(),
        buttons_sent: false,
        scenario,
        walk_address_requested: false,
        battery_read: None,
        poll_stopped: false,
    };
    let mut rx = 0;
    let mut tx = 0;
    let mut waiting = false;
    let mut callbacks_paused = false;
    let mut queue = VecDeque::new();
    loop {
        let Some(frame) = decoder.push(port.read_u8().await?) else {
            continue;
        };
        let f = frame.map_err(|()| anyhow::anyhow!("invalid ASH frame"))?;
        if f.control == 0xc0 {
            port.write_all(&wire::Frame::encode(0xc1, &[2, 0x0b]))
                .await?;
            continue;
        }
        if matches!(f.control, 0x80..=0x8f | 0xa0..=0xaf) {
            callbacks_paused = f.control & 8 != 0;
        }
        if f.control & 7 == tx {
            waiting = false;
        }
        if f.control & 0x80 == 0 {
            ensure!(f.control >> 4 == rx, "unexpected ASH sequence");
            rx = (rx + 1) & 7;
            if protocol == 19 {
                native.command(&f.data, &mut lamps, &mut queue)?;
            } else {
                lamps.command(&f.data, &mut queue)?;
                if observed.is_some() && !injected && f.data.get(3..5) == Some(&[0x26,0]) {
                    for level in [60,80] {
                        queue.push_back(incoming(2,8,&[0x18,level,10,0,0,0x20,level]));
                    }
                    injected = true;
                }
                if injected && f.data.get(3..5) == Some(&[0x61,0])
                    && f.data.get(5..) == Some(&short(2).to_le_bytes()) {
                    lookups += 1;
                    if lookups == 2 && let Some(seen) = observed.take() {let _ = seen.send(());}
                }
            }
            port.write_all(&wire::Frame::encode(0x80 | rx, &[])).await?;
        }
        let ready = if callbacks_paused {
            queue.iter().position(|message| message.get(1).is_some_and(|control| control & 0x18 == 0))
        } else { Some(0) };
        if !waiting && let Some(message) = ready.and_then(|i|queue.remove(i)) {
            if scenario == Scenario::Feedback { state_feedback::before_delivery(&message).await; }
            port.write_all(&wire::Frame::encode(tx << 4 | rx, &message))
                .await?;
            tx = (tx + 1) & 7;
            waiting = true;
        }
    }
}

fn bridge_options(id: String) -> Result<mqtt::MqttOptions> {
    if let Some(path) = std::env::var_os("ZIGBEE_TEST_TLS_CONFIG") {
        let (mut options, _, _) = mqtt::config::BrokerConfig::load(std::path::Path::new(&path))?;
        let (host, _) = options.broker_address();
        ensure!(
            matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"),
            "test broker must use loopback"
        );
        options.set_client_id(id);
        return Ok(options);
    }
    Ok(mqtt::MqttOptions::new(id, "127.0.0.1", 1883))
}

// Keep the independent witnesses on MQTT 3.1.1, including on the TLS bench.
fn test_broker(id: String) -> Result<MqttOptions> {
    let bridge = bridge_options(id)?;
    let (host, port) = bridge.broker_address();
    let mut options = MqttOptions::new(bridge.client_id(), host, port);
    options.set_transport(bridge.transport());
    if let Some(login) = bridge.credentials() {
        options.set_credentials(login.username, login.password);
    }
    Ok(options)
}

async fn client_work(member: &std::path::Path, base: &str) -> Result<()> {
    let (client, mut net) = AsyncClient::new(test_broker(format!("{base}-observer"))?, 128);
    client
        .subscribe(format!("{base}/#"), QoS::AtLeastOnce)
        .await?;
    let mut seen = BTreeSet::new();
    let mut requested = false;
    let mut confirmed_off = false;
    let expected = ["single", "single", "double", "hold"];
    let mut actions = 0;
    let mut battery_requested = false;
    let mut admin_test = false;
    let mut update_requested = false;
    let mut read_on_off = false;
    let mut unbind_sent = false;
    let mut closed_contact = false;
    let mut enrolled_contact = false;
    let mut new_light_on = false;
    loop {
        // A cold adapter can spend eight seconds draining old address responses
        // before its first on-air discovery for an imported device.
        let event = tokio::time::timeout(Duration::from_secs(12), net.poll())
            .await
            .with_context(|| {
                format!(
                    "radio stalled after {} brightness reports, pending state read={requested}",
                    seen.len()
                )
            })??;
        let Event::Incoming(Incoming::Publish(message)) = event else {
            continue;
        };
        let topic = message.topic.strip_prefix(base).context("unknown topic")?;
        if topic == "/button/action" {
            ensure!(
                !message.retain && expected.contains(&std::str::from_utf8(&message.payload)?),
                "invalid raw action"
            );
            continue;
        }
        let mut body: Value = serde_json::from_slice(&message.payload)?;
        if let Some(fields) = body.as_object_mut()
            && let Some(lqi) = fields.remove("linkquality")
        {
            ensure!(lqi == 142, "incorrect last-hop LQI: {lqi}");
            if fields.is_empty() {
                continue;
            }
        }
        if topic == "/bridge/state" && body == json!({"state":"online"}) {
            for light in 1..=COUNT {
                client
                    .publish(
                        format!("{base}/lamp-{light}/set"),
                        QoS::AtLeastOnce,
                        false,
                        br#"{"state":"OFF"}"#.as_slice(),
                    )
                    .await?;
            }
        } else if topic == "/button" {
            if let Some(action) = body.get("action") {
                ensure!(!message.retain, "retained gesture");
                ensure!(
                    expected.get(actions).copied() == action.as_str(),
                    "actions changed order or duplicated"
                );
                actions += 1;
            } else {
                ensure!(
                    battery_requested && body == json!({"battery":86.5}),
                    "unconfirmed battery: {body}"
                );
                admin_test = true;
                // A client can retry before the binding confirmation has made it back over MQTT.
                for _ in 0..2 {
                    client.publish(format!("{base}/bridge/request/device/bind"),QoS::AtLeastOnce,false,br#"{"from":"lamp-1","to":"Coordinator","clusters":["genOnOff"],"transaction":"bind"}"#.as_slice()).await?;
                }
            }
        } else if matches!(
            topic,
            "/bridge/response/device/bind" | "/bridge/response/device/unbind"
        ) {
            ensure!(
                admin_test
                    && body["status"] == "ok"
                    && body["data"]["clusters"] == json!(["genOnOff"])
                    && body["data"]["failed"] == json!([]),
                "unconfirmed binding: {body}"
            );
            if body["transaction"] == "bind" {
                if unbind_sent {continue;}
                unbind_sent = true;
                client.publish(format!("{base}/bridge/request/device/unbind"),QoS::AtLeastOnce,false,br#"{"from":"lamp-1","to":"Coordinator","clusters":["genOnOff"],"transaction":"unbind"}"#.as_slice()).await?;
            } else {
                ensure!(body["transaction"] == "unbind");
            }
        } else if admin_test && !update_requested && topic == "/lamp-1" && body["brightness"] == 77
        {
            update_requested = true;
            client
                .publish(
                    format!("{base}/bridge/request/device/ota_update/update"),
                    QoS::AtLeastOnce,
                    false,
                    br#"{"id":"lamp-1","transaction":"up"}"#.as_slice(),
                )
                .await?;
        } else if topic == "/bridge/response/device/ota_update/update" {
            ensure!(
                update_requested
                    && body["status"] == "ok"
                    && body["transaction"] == "up"
                    && body["data"]["from"]["file_version"] == 3
                    && body["data"]["to"]["file_version"] == 8,
                "failed confirmed OTA: {body}"
            );
            client
                .publish(
                    format!("{base}/bridge/request/permit_join"),
                    QoS::AtLeastOnce,
                    false,
                    br#"{"time":3,"transaction":"join"}"#.as_slice(),
                )
                .await?;
        } else if topic == "/bridge/response/permit_join" && body["transaction"] == "join" {
            ensure!(body["status"] == "ok");
            tokio::time::timeout(Duration::from_secs(5), async {
                while !member.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .context("joined device did not finish its interview")?;
            let report = InterviewReport::open(member)?;
            ensure!(report.finished && report.entries.iter().any(|entry|matches!(entry.observation,Observation::SimpleDescriptor{endpoint:ep,..} if ep==endpoint(enrolment::NEW))),"incomplete persisted member");
            ensure!(report.entries.iter().any(|entry|matches!(&entry.observation,
                Observation::Attribute{endpoint:ep,cluster:6,attribute:0,result:AttributeResult::Value{data_type:0x10,bytes},..}
                if *ep==endpoint(enrolment::NEW) && bytes==&[1])), "missing confirmed interview On/Off value");
            client.publish(format!("{base}/bridge/request/device/describe"), QoS::AtLeastOnce, false,
                serde_json::to_vec(&serde_json::json!({"id":format!("0x{:016x}", ieee(enrolment::NEW)),
                    "endpoint":endpoint(enrolment::NEW),"cluster":6,"transaction":"describe"}))?).await?;
        } else if topic == "/bridge/response/device/describe" && body["transaction"] == "describe" {
            let key=format!("zcl_{:02x}_0006_0000",endpoint(enrolment::NEW));
            ensure!(body["status"]=="ok" && body["data"]["catalogue_attributes"].as_array()
                .is_some_and(|rows| rows.iter().any(|r| r["key"]==key && r["set_on_off"]==true)),
                "missing descriptor-backed On/Off attribute");
            client.publish(format!("{base}/0x{:016x}/get",ieee(enrolment::NEW)),QoS::AtLeastOnce,false,
                serde_json::to_vec(&serde_json::json!({(key):""}))?).await?;
        } else if !read_on_off && topic == format!("/0x{:016x}",ieee(enrolment::NEW)) &&
            body[format!("zcl_{:02x}_0006_0000",endpoint(enrolment::NEW))]["raw"]==true {
            read_on_off = true;
            client
                .publish(
                    format!("{base}/0x{:016x}/get", ieee(enrolment::NEW)),
                    QoS::AtLeastOnce,
                    false,
                    br#"{"state":""}"#.as_slice(),
                )
                .await?;
        } else if topic == format!("/0x{:016x}", ieee(enrolment::NEW))
            && (body["state"]=="ON" || body["contact"].is_boolean()) {
            new_light_on |= body["state"]=="ON";
            if !enrolled_contact && body["contact"]==false {
                enrolled_contact=true;
                // Poll the zone after observing it open. The fixture closes while answering.
                let key = format!("zcl_{:02x}_0500_0002",endpoint(enrolment::NEW));
                client.publish(format!("{base}/0x{:016x}/get",ieee(enrolment::NEW)),QoS::AtLeastOnce,false,
                    serde_json::to_vec(&json!({(key):""}))?).await?;
            }
            closed_contact |= enrolled_contact && body["contact"]==true;
            if !new_light_on || !closed_contact {continue}
            client.publish(format!("{base}/bridge/request/device/describe"),QoS::AtLeastOnce,false,
                serde_json::to_vec(&json!({"id":format!("0x{:016x}",ieee(enrolment::NEW)),
                    "endpoint":endpoint(enrolment::NEW),"transaction":"zone-confirmation"}))?).await?;
        } else if topic=="/bridge/response/device/describe" && body["transaction"]=="zone-confirmation" {
            ensure!(body["data"]["ias_enrollment"]==json!({"state":"confirmed","zone_id":0}),
                "zone did not confirm its assigned ID");
            // A late independent client must replay both retained states. Only
            // the router was polled. The sleepy button became online by reporting.
            let (late,mut wire)=AsyncClient::new(test_broker(format!("{base}-late"))?,8);
            late.subscribe(format!("{base}/+/availability"),QoS::AtLeastOnce).await?;
            let mut topics=BTreeSet::new();
            while topics.len()<2 {
                if let Event::Incoming(Incoming::Publish(p))=wire.poll().await? {
                    ensure!(p.retain && serde_json::from_slice::<Value>(&p.payload)?==json!({"state":"online"}),"missing retained online state");
                    topics.insert(p.topic);
                }
            }
            ensure!(topics==[format!("{base}/button/availability"),format!("{base}/lamp-1/availability")].into(),"unexpected availability topic");
            endpoint_admin::verify(&client,&mut net,base).await?;
            lock_control::verify(&client,&mut net,base).await?;
            ikea_light::verify(&client,&mut net,base).await?;
            meter::verify(&client, &mut net, base).await?;
            environment::verify(&client, &mut net, base).await?;
            equipment::verify(&client, &mut net, base).await?;
            template_output::verify(&client,&mut net,base).await?;
            basic_io::verify(&client, &mut net, base).await?;
            hvac::verify(&client,&mut net,base).await?;
            cover::verify(&client,&mut net,base).await?;
            admin_replay::bind_across_conflict(&client,&mut net,base).await?;
            return admin_replay::interrupt_bind(&client,&mut net,base).await;

        } else if let Some(suffix) = topic.strip_prefix("/lamp-")
            && !suffix.contains('/')
        {
            let light: u16 = suffix.parse()?;
            if body.get("state").is_some() {
                ensure!(
                    body["state"] == "OFF" && (body["brightness"] == light || (light == 1 && admin_test && body["brightness"] == 77)),
                    "unconfirmed state: {body}"
                );
                seen.insert(light);
                if light == 1 { confirmed_off = true; }
            } else if let Some(brightness) = body.get("brightness") {
                // Lamp 4 sent 240 before its reverse lookup completed. Its
                // next report is the fence for the final value on this route.
                if light == 4 && *brightness == 240 { continue; }
                ensure!(*brightness == light, "misrouted brightness on {topic}: {body}");
                seen.insert(light);
            } else {
                ensure!(light == 1 && body.get("update").is_some(), "unexpected lamp state {body}");
            }
            if !requested && seen.len() == usize::from(COUNT) {
                requested = true;
                client
                    .publish(
                        format!("{base}/lamp-1/get"),
                        QoS::AtLeastOnce,
                        false,
                        br#"{"state":""}"#.as_slice(),
                    )
                    .await?;
            }
        }
        if requested && confirmed_off && actions == expected.len() && !battery_requested {
            battery_requested = true;
            client
                .publish(
                    format!("{base}/button/get"),
                    QoS::AtLeastOnce,
                    false,
                    br#"{"battery":""}"#.as_slice(),
                )
                .await?;
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn controls_and_reports_cross_real_mqtt_and_ash() -> Result<()> {
    roundtrip(13).await
}

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn ezsp19_controls_and_reports_cross_real_mqtt_and_ash() -> Result<()> {
    roundtrip(19).await
}

async fn roundtrip(protocol: u8) -> Result<()> {
    let base=format!("rust-radio-test-v{protocol}");
    let original=tempfile::tempdir()?;
    let restored=tempfile::tempdir()?;
    run_session(protocol,&base,original.path(),Scenario::Commands).await?;
    admin_replay::copy_stopped(original.path(),restored.path())?;
    run_session(protocol,&base,restored.path(),Scenario::Restored).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario { Commands, Restored, Feedback, Walk, Observations }

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn startup_walk_discovers_a_fresh_router_before_reading_its_basic_cluster() -> Result<()> {
    let dir = tempfile::tempdir()?;
    run_session(19, "startup-walk", dir.path(), Scenario::Walk).await
}

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn cold_button_and_motion_notifications_reach_mqtt_in_order() -> Result<()> {
    for version in [13, 19] {
        let dir = tempfile::tempdir()?;
        run_session(version, &format!("cold-observations-{version}"), dir.path(), Scenario::Observations).await?;
    }
    Ok(())
}

async fn observe_startup_walk(base: &str) -> Result<()> {
    let (client, mut events) = AsyncClient::new(test_broker("walk-witness".into())?, 16);
    let topic = format!("{base}/bridge/diagnostics/lamp-1");
    client.subscribe(&topic, QoS::AtLeastOnce).await?;
    loop {
        if let Event::Incoming(Incoming::Publish(message)) = events.poll().await?
            && message.topic == topic
            && serde_json::from_slice::<Value>(&message.payload)?["linkquality"] == 142
        {
            return Ok(());
        }
    }
}

async fn run_session(protocol:u8,base:&str,dir:&std::path::Path,session:Scenario)->Result<()> {
    let restored = matches!(session, Scenario::Restored);
    tokio::time::timeout(Duration::from_secs(40),async {
        let (mut peer, slave) = SerialStream::pair()?;
        let path = slave.name().context("PTY name")?;
        drop(slave);
        let mut policy=zigbee_bridge_rs::availability::Catalogue::import(None)?;
        let last_seen = (session == Scenario::Walk).then(|| chrono::Utc::now().timestamp_millis() as u64);
        policy.import_member(ieee(1),"Router",None,last_seen,Some(&json!(true)))?;
        policy.import_member(ieee(BUTTON),"EndDevice",Some("Battery"),None,Some(&json!(true)))?;
        if !restored {policy.write(dir)?;}
        let mut startup = Startup::new(Store::open(dir)?);
        let mut port = Serial::attach(&mut startup, tokio_serial::new(path, 115200))?;
        let owner = startup.start();
        let (publisher, mut worker, _) =
            mqtt::channel(bridge_options(format!("{base}-worker"))?, base, 128)?;
        let mut commands = worker.enable_reconnect(dir)?;
        worker.wait_for_device_snapshot();
        let mut requests = worker.enable_admin()?;
        let catalog=if restored {None} else {
            let image=fixture::image(8,None,None);
            Some(fixture::manifest(dir,vec![fixture::entry(dir,"v8",&image)?])?)
        };
        if !restored {ace::configure(dir)?;}
        let mut management=Management::new(dir,BTreeMap::new(),catalog.as_deref())?;
        let _credentials=if restored {None} else {Some(lock_control::enable(&mut management)?)};
        let mut devices=if restored {Bindings::default()} else {bindings(session == Scenario::Observations)?};
        let feedback_trial = matches!(session, Scenario::Feedback);
        if feedback_trial {
            zigbee_bridge_rs::bridge::LastSeen::Utc.stage(dir)?;
            devices.load_last_seen(dir)?;
        }
        let run = async {
            let mut link = Ezsp::connect(&mut port, Duration::from_secs(4)).await?;
            ensure!(
                link.version.protocol == protocol,
                "unexpected negotiated protocol"
            );
            zigbee_bridge_rs::interview::register_listener(&mut link).await?;
            zigbee_bridge_rs::coordinator::start(
                &mut link,
                NETWORK,
                zigbee_bridge_rs::coordinator::StartupMode::Resume,
            )
            .await?;
            serve_managed(
                &mut link,
                &mut devices,
                &publisher,
                &mut commands,
                &mut management,
                &mut requests,
            )
            .await
        };
        let mqtt = tokio::spawn(worker.run());
        let member = dir
            .join("members")
            .join(format!("{:016x}.member", ieee(enrolment::NEW)));
        let observer=async {
            match session {
                Scenario::Feedback => state_feedback::observe().await,
                Scenario::Observations => cold_observations::observe(base).await,
                Scenario::Walk => observe_startup_walk(base).await,
                Scenario::Restored => admin_replay::verify(base).await,
                Scenario::Commands => client_work(&member,base).await,
            }
        };
        tokio::select! {
            result=run=>result.context("coordinator exited")?,
            result=ncp_with_offline_reports(&mut peer,protocol,restored,None,session)=>result.context("simulated adapter exited")?,
            result=observer=>result?,
        }
        // The coordinator drops active and queued requests before MQTT drains.
        drop(management);
        drop(requests);
        publisher.close();
        mqtt.await??;
        owner.shutdown().await?;
        if matches!(session, Scenario::Commands | Scenario::Restored) { ace::check_state(dir)?; }
        Ok(())
    })
    .await
    .context("MQTT/PTY test timed out")?
}

#[tokio::test]
#[ignore = "requires isolated MQTT and a pseudo-terminal"]
async fn installed_listener_resumes_with_radio_settings() -> Result<()> {
    for protocol in [13, 19] {
        let state = tempfile::tempdir()?;
        let (mut peer, slave) = SerialStream::pair()?;
        let (client, mut broker) = AsyncClient::new(MqttOptions::new("trial-verifier", "127.0.0.1", 1883), 16);
        let topic = "zigbee2mqtt/bridge/v1/response/health_check";
        client.subscribe(topic, QoS::AtLeastOnce).await?;
        while !matches!(broker.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
        let binary = std::env::var_os("ZIGBEE_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_zigbee-bridge").into());
        let mut child = std::process::Command::new(binary)
            .args(["listen", &slave.name().context("PTY name")?]).arg(state.path())
            .args(["20", "729c", "0807060504030201", "--flow", "none", "--baud", "460800",
                   "--max-children", "64", "--tx-power", "20", "--mqtt-local-port", "1883"]).spawn()?;
        let verify = async {
            client.subscribe("zigbee2mqtt/bridge/state", QoS::AtLeastOnce).await?;
            let transaction = format!("test-{protocol}");
            loop {
                let Event::Incoming(Incoming::Publish(message)) = broker.poll().await? else { continue };
                if message.retain { continue; }
                let body: Value = serde_json::from_slice(&message.payload)?;
                if message.topic.ends_with("/state") && body["state"] == "online" {
                    client.publish("zigbee2mqtt/bridge/v1/request/health_check", QoS::AtLeastOnce, false,
                        serde_json::to_vec(&json!({"transaction":transaction}))?).await?;
                } else if message.topic == topic && body["transaction"] == transaction {
                    ensure!(body["data"]["healthy"] == true);
                    return Ok(());
                }
            }
        };
        let radio = ncp(&mut peer, protocol, true);
        tokio::pin!(radio);
        let result = tokio::select! {
            v = &mut radio => v.and_then(|_| anyhow::bail!("PTY ended before health reply")),
            v = tokio::time::timeout(Duration::from_secs(15), verify) => v.context("missing health response").and_then(|r|r),
        };
        unsafe {libc::kill(child.id() as i32, libc::SIGTERM)};
        let drained = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(status) = child.try_wait()? {return anyhow::Ok(status.success());}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await;
        if drained.is_err() {let _ = child.kill(); let _ = child.wait();}
        result?;
        ensure!(drained??, "listener failed to drain");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated MQTT and a pseudo-terminal"]
async fn radio_receives_reports_before_broker_is_available() -> Result<()> {
    use tokio::net::{TcpListener, TcpStream};
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = tempfile::tempdir()?;
        let (mut peer, slave) = SerialStream::pair()?;
        let path = slave.name().context("PTY name")?;
        drop(slave);
        let mut startup = Startup::new(Store::open(dir.path())?);
        let mut port = Serial::attach(&mut startup, tokio_serial::new(path, 115200))?;
        let owner = startup.start();
        let gate = TcpListener::bind(("127.0.0.1",0)).await?;
        let (observed, reports) = tokio::sync::oneshot::channel();
        let (out, mut worker, _) = mqtt::channel(mqtt::MqttOptions::new("offline-collector",
            "127.0.0.1",gate.local_addr()?.port()),"offline-radio",16)?;
        let mut commands = worker.enable_reconnect(dir.path())?;
        let mut admin = worker.enable_admin()?;
        let mut control = Management::new(dir.path(),BTreeMap::new(),None)?;
        let mut devices = bindings(false)?;
        let (reader,mut net)=AsyncClient::new(test_broker("offline-readback".into())?,8);
        reader.subscribe("offline-radio/lamp-2",QoS::AtLeastOnce).await?;
        while !matches!(net.poll().await?,Event::Incoming(Incoming::SubAck(_))) {}
        let proxy = tokio::spawn(async move {
            reports.await?;
            let (mut client,_) = gate.accept().await?;
            let mut broker = TcpStream::connect(("127.0.0.1",1883)).await?;
            tokio::io::copy_bidirectional(&mut client,&mut broker).await?;
            Ok::<_,anyhow::Error>(())
        });
        let publish = tokio::spawn(worker.run());
        let radio = async {
            let mut link = Ezsp::connect(&mut port,Duration::from_secs(4)).await?;
            zigbee_bridge_rs::interview::register_listener(&mut link).await?;
            zigbee_bridge_rs::coordinator::start(&mut link,NETWORK,
                zigbee_bridge_rs::coordinator::StartupMode::Resume).await?;
            serve_managed(&mut link,&mut devices,&out,&mut commands,&mut control,&mut admin).await
        };
        let check = async {
            loop {
                if let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? {
                    let state: Value = serde_json::from_slice(&msg.payload)?;
                    if state["brightness"]==80 {return Ok::<_,anyhow::Error>(());}
                }
            }
        };
        tokio::select! {
            v=radio=>{v?; bail!("radio stopped before readback")},
            v=ncp_with_offline_reports(&mut peer,13,false,Some(observed),Scenario::Commands)=>{v?; bail!("NCP stopped before readback")},
            v=check=>v?,
        }
        drop(control);
        out.close();
        publish.await??;
        proxy.await??;
        owner.shutdown().await?;
        Ok::<_,anyhow::Error>(())
    }).await.context("offline radio report did not reach MQTT")?
}
