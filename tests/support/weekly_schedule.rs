//! Distinct days prove command ordering and bounded partial outcomes on both EZSP ABIs.
use anyhow::{Result, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, QoS};
use serde_json::{Value as Json, json};
use zigbee_zcl::{
    Frame,
    cluster::{Command, Thermostat},
};

fn temps(day: u8) -> Vec<(u16, i16)> {
    match day {
        2 => vec![(360, 2100)], // Existing single-page query.
        4 | 8 => (0..6).map(|n| (120 * n, 1800 + n as i16 * 100)).collect(),
        16 => vec![(360, 2050)], // This thermostat constrained the requested setpoint.
        _ => vec![],
    }
}
pub fn respond(frame: Frame<'_>) -> Result<Vec<Vec<u8>>> {
    let seq = frame.header.sequence;
    match Command::decode(0x201, frame)? {
        Command::Thermostat(Thermostat::GetWeeklySchedule {
            days_to_return: day,
            mode_to_return: mode,
        }) => {
            ensure!(
                matches!(day, 2 | 4 | 8 | 16) && mode.bits() == 1,
                "unexpected schedule read"
            );
            let rows = temps(day);
            let mut response = vec![0x19, seq, 0, rows.len() as u8, day, 1];
            for (t, v) in rows {
                response.extend(t.to_le_bytes());
                response.extend(v.to_le_bytes());
            }
            Ok(vec![vec![0x19, seq, 0, 0, 128, 1], response]) // Wrong day must not advance the job.
        }
        Command::Thermostat(Thermostat::SetWeeklySchedule { schedule: s }) => {
            ensure!(
                s.mode.bits() == 1
                    && matches!(s.days, 4 | 8 | 16 | 32)
                    && !frame.header.disable_default_response,
                "unexpected Set Weekly Schedule"
            );
            let actual: Vec<_> = s
                .transitions
                .iter()
                .map(|r| (r.minutes_since_midnight, r.heat_setpoint.unwrap()))
                .collect();
            if matches!(s.days, 4 | 8) {
                ensure!(actual == temps(s.days));
            } else {
                ensure!(actual == [(360, 2100)]);
            }
            Ok(vec![
                vec![0x18, seq, 11, 2, 0],
                vec![0x18, seq, 11, 1, if s.days == 32 { 0x89 } else { 0 }],
            ])
        }
        _ => anyhow::bail!("unexpected thermostat command"),
    }
}

fn page(day: &str, rows: &[(u16, i16)]) -> Json {
    json!({"day":day,"mode":"heat","transitions":rows.iter().map(|(m,t)|json!({"minute":m,"setpoint":f64::from(*t)/100.})).collect::<Vec<_>>()})
}

pub async fn verify(client: &AsyncClient, net: &mut EventLoop, base: &str) -> Result<()> {
    let topic = format!("{base}/equipment");
    let tue = page("tuesday", &temps(4));
    let wed = page("wednesday", &temps(8));
    let later = page("saturday", &[(360, 2100)]);
    for (operation, pages, wanted) in [
        ("set", json!([tue, wed]), "verified"),
        (
            "get",
            json!([{"day":"tuesday","mode":"heat"},{"day":"wednesday","mode":"heat"}]),
            "read",
        ),
        (
            "set",
            json!([tue, page("thursday", &[(360, 2100)]), later]),
            "mismatch",
        ),
        (
            "set",
            json!([page("friday", &[(360, 2100)]), later]),
            "declined",
        ),
    ] {
        client
            .publish(
                format!("{topic}/{operation}"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&json!({"weekly_schedule":{"pages":pages}}))?,
            )
            .await?;
        loop {
            let Event::Incoming(Incoming::Publish(p)) = net.poll().await? else {
                continue;
            };
            if p.topic != topic {
                continue;
            }
            let state: Json = serde_json::from_slice(&p.payload)?;
            let v = &state["weekly_schedule"];
            if v["status"] != wanted {
                continue;
            }
            match wanted {
                "verified" | "read" => ensure!(v["completed"] == json!([tue, wed])),
                "mismatch" => ensure!(
                    v["completed"] == json!([tue])
                        && v["current"] == page("thursday", &[(360, 2050)])
                ),
                "declined" => {
                    ensure!(v["reason"] == "ZCL status 0x89" && v["completed"] == json!([]))
                }
                _ => unreachable!(),
            }
            break;
        }
    }
    let invalid = page("monday", &[(360, 3300)]);
    client
        .publish(
            format!("{topic}/set"),
            QoS::AtLeastOnce,
            false,
            serde_json::to_vec(&json!({"weekly_schedule":{"pages":[invalid]}}))?,
        )
        .await?;
    loop {
        let Event::Incoming(Incoming::Publish(p)) = net.poll().await? else {
            continue;
        };
        if p.topic == topic
            && serde_json::from_slice::<Json>(&p.payload)?["weekly_schedule_rejected"]
                .as_str()
                .is_some_and(|s| s.contains("setpoint outside thermostat limits"))
        {
            break;
        }
    }
    Ok(())
}
