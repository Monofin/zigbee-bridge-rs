//! Independent application endpoint with a panel, a thermostat and a humidity controller.
#[path="equipment_commissioning.rs"]
mod commissioning;
pub use commissioning::Replay;
#[path="weekly_schedule.rs"]
mod weekly_schedule;
use super::{endpoint, ieee, incoming, short};
use anyhow::{Result, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, QoS};
use serde_json::{Value, json};
use zigbee_bridge_rs::{
    bridge::Bindings,
    interview_log::{
        AttributeResult, Descriptor, Entry, IdentitySource, InterviewReport, Observation,
    },
};
use zigbee_zcl::{Frame, foundation::Payload};

pub const DEVICE: u16 = super::basic_io::REMOTE + 1;

pub fn bind(bindings: &mut Bindings) -> Result<()> {
    let ep = endpoint(DEVICE);
    let mut facts = vec![
        Observation::Target {
            ieee: format!("{:016x}", ieee(DEVICE)),
            short: short(DEVICE),
            identity_source: IdentitySource::Announcement,
        },
        Observation::ActiveEndpoints {
            sequence: 1,
            endpoints: vec![ep],
        },
        Observation::SimpleDescriptor {
            sequence: 2,
            endpoint: ep,
            descriptor: Descriptor {
                profile: 0x104,
                device: 0xfffe,
                version: 1,
                servers: vec![0x0100, 0x0301, 0x0101, 0x0103, 0x0200, 0x0201, 0x0203, 0x0501, 0x0502],
                clients: vec![0x501],
            },
        },
    ];
    for (cluster, attribute, data_type, bytes) in [
        (0x101, 0x34, 0x30, vec![0]),
        (0x100,0,0x21,600u16.to_le_bytes().to_vec()),
        (0x301,0,0x20,vec![10]),
        (0x301,1,0x20,vec![200]),
        (0x200,0x21,0x30,vec![0]),
        (0x200,0x12,0x30,vec![0]),
        (0x200,3,0x29,(-50i16).to_le_bytes().to_vec()),
        (0x200,4,0x29,500i16.to_le_bytes().to_vec()),
        (0x200, 0x15, 0x22, vec![99, 0, 0]),
        (0x201, 0x21, 0x20, vec![20]),
        (0x201, 0x22, 0x20, vec![6]),
        (0x201, 0x1b, 0x30, vec![2]),
        (0x201,3,0x29,1500i16.to_le_bytes().to_vec()),
        (0x201,4,0x29,2700i16.to_le_bytes().to_vec()),
        (0x103, 3, 0x18, vec![1]),
    ] {
        facts.push(Observation::Attribute {
            sequence: 3,
            endpoint: ep,
            cluster,
            attribute,
            result: AttributeResult::Value { data_type, bytes },
        });
    }
    facts.push(Observation::Finished);
    bindings.add(
        &InterviewReport {
            finished: true,
            entries: facts
                .into_iter()
                .map(|observation| Entry {
                    unix_ms: 0,
                    observation,
                })
                .collect(),
        },
        &[(ieee(DEVICE), "equipment".to_owned())]
            .into_iter()
            .collect(),
    )?;
    let cfg = bindings.discovery_snapshot("test", "ha", None)?;
    let lookup = |domain: &str, name: &str| {
        cfg.get(&format!(
            "ha/{domain}/0x{:016x}/{name}/config",
            ieee(DEVICE)
        ))
    };
    ensure!(
        lookup("sensor", "panel_status").is_some()
            && lookup("sensor", "weekly_schedule_page").is_some()
    );
    let range=lookup("number","ballast_minimum_level").expect("confirmed ballast limits");
    ensure!(range["min"]==10. && range["max"]==200.);
    ensure!(lookup("select","pump_control_mode").is_some_and(|v|v["options"]==json!(["constant_speed","constant_pressure"])));
    let hours=lookup("sensor","pump_lifetime_running_hours").expect("sampled running counter");
    ensure!(hours["unit_of_measurement"] == "h"
        && hours["state_class"] == "total_increasing" && hours["device_class"] == "duration");
    ensure!(lookup("sensor", "lock_state").is_some() && lookup("sensor", "door_state").is_none());
    ensure!(
        lookup("cover", "barrier")
            .is_some_and(|c| c["set_position_topic"] == "test/equipment/set/barrier_command")
    );
    Ok(())
}

pub fn respond(replay:&mut Replay,args: &[u8]) -> Result<Vec<Vec<u8>>> {
    ensure!(args[8] == endpoint(DEVICE));
    let frame = Frame::decode(&args[16..])?;
    let seq = frame.header.sequence;
    let cluster = u16::from_le_bytes(args[5..7].try_into()?);
    let callback = |bytes: &[u8]| {
        let mut msg = incoming(DEVICE, cluster, bytes);
        msg[11] = args[7];
        msg
    };
    match cluster {
        0x100|0x200|0x301=>Ok(vec![callback(&commissioning::response(replay,cluster,frame)?)]),
        0x501 => super::ace::exchange(args, frame),
        0x201 => Ok(weekly_schedule::respond(frame)?.iter().map(|r|callback(r)).collect()),
        0x203 => match Payload::decode(frame)? {
            Payload::WriteAttributes(rows) => {
                ensure!(rows.len() == 1 && rows[0].id == 0x10 && rows[0].value.as_bytes() == [46]);
                Ok(vec![callback(&[0x18, seq, 4, 0])])
            }
            Payload::ReadAttributes(ids) => {
                ensure!(ids == [0x10]);
                Ok(vec![callback(&[0x18, seq, 1, 0x10, 0, 0, 0x20, 50])])
            }
            _ => anyhow::bail!("unexpected humidity request"),
        },
        _ => anyhow::bail!("unexpected equipment cluster {cluster:#06x}"),
    }
}

pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    let topic = format!("{base}/equipment");
    for (verb, key, value, expected) in [
        ("set","shade_closed_limit",json!(450),json!(448)),
        ("set","ballast_minimum_level",json!(36),json!(37)),
        ("set","pump_control_mode",json!("constant_pressure"),json!("constant_pressure")),
        ("get", "panel_status", json!(""), json!("armed_stay")),
        (
            "get",
            "weekly_schedule_page",
            json!({"day":"monday","mode":"heat"}),
            json!({"day":"monday","mode":"heat","transitions":[{"minute":360,"setpoint":21.0}]}),
        ),
        ("set", "dehumidification_setpoint", json!(46), json!(50)),
    ] {
        client
            .publish(
                format!("{topic}/{verb}/{key}"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&value)?,
            )
            .await?;
        loop {
            if let Event::Incoming(Incoming::Publish(p)) = net.poll().await? {
                super::ace::check_private(&p.payload)?;
                if p.topic != topic {continue;}
                let state: Value = serde_json::from_slice(&p.payload)?;
                if let Some(v) = state.get(key) {
                    ensure!(*v == expected, "unexpected equipment {key}: {v}");
                    if key == "panel_status" {
                        ensure!(state["panel_alarm_status"]=="none" && state["panel_seconds_remaining"]==0
                            && state["panel_audible_notification"]==0, "missing corroborating panel fields");
                    }
                    break;
                }
            }
        }
    }
    weekly_schedule::verify(client,net,base).await?;
    Ok(())
}
