//! Replay the Hall capture's ON → SET OFF → activity → OFF → SET ON → activity → ON.
use super::*;

pub const LIGHT: u16 = 3;
const BASE: &str = "hall-feedback";

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn activity_cannot_confirm_an_on_off_command() -> Result<()> {
    let dir = tempfile::tempdir()?;
    run_session(13, BASE, dir.path(), Scenario::Feedback).await
}

// The negative control carries the opposite value on an unrelated transaction.
pub fn respond(frame: Frame<'_>, switched_off: &mut BTreeSet<u16>) -> Result<Vec<Vec<u8>>> {
    let seq = frame.header.sequence;
    let cmd = frame.header.command_id;
    if frame.header.frame_type == FrameType::ClusterSpecific {
        ensure!(frame.payload.is_empty() && cmd <= 1);
        if cmd == 0 {
            switched_off.insert(LIGHT);
        } else {
            switched_off.remove(&LIGHT);
        }
        return Ok(vec![incoming(LIGHT, 6, &[0x18, seq, 11, cmd, 0])]);
    }
    ensure!(cmd == 0 && frame.payload == [0, 0]);
    let state = u8::from(!switched_off.contains(&LIGHT));
    Ok(vec![
        incoming(LIGHT, 6, &[0x18, seq.wrapping_add(1), 1, 0, 0, 0, 0x10, 1-state]),
        incoming(LIGHT, 6, &[0x18, seq, 1, 0, 0, 0, 0x10, state]),
    ])
}

pub async fn before_delivery(message: &[u8]) {
    if message.get(3..5) == Some(&[0x45, 0])
        && message.get(19..21) == Some(short(LIGHT).to_le_bytes().as_slice())
        && let Some([0x18, seq, 1, 0, 0, 0, 0x10, state]) = message.get(24..)
    {
        // Hold the simulated reply so the broker can publish earlier diagnostics.
        tokio::time::sleep(Duration::from_millis(300)).await;
        eprintln!("fixture ZCL read response tsn={seq} value={state} at {}", jiff::Timestamp::now());
    }
}

pub async fn observe() -> Result<()> {
    let (client, mut net) = AsyncClient::new(test_broker("hall-witness".into())?, 16);
    client.subscribe(format!("{BASE}/#"), QoS::AtLeastOnce).await?;
    let device = format!("{BASE}/lamp-{LIGHT}");
    let diagnostics = format!("{BASE}/bridge/diagnostics/lamp-{LIGHT}");
    let mut started = false;
    let mut step = 0;
    let mut activity = false;
    loop {
        let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? else { continue };
        let body: Value = serde_json::from_slice(&msg.payload)?;
        if !started && msg.topic == format!("{BASE}/bridge/state") && body["state"] == "online" {
            started = true;
            // Establish one measured ON. Both subsequent reads must come from the bridge.
            client.publish(format!("{device}/get"), QoS::AtLeastOnce, false, r#"{"state":""}"#).await?;
        } else if msg.topic == diagnostics {
            ensure!(body.get("state").is_none());
            activity |= body.get("last_seen").is_some();
        } else if msg.topic == device {
            ensure!(body.get("last_seen").is_none() && body.get("linkquality").is_none());
            let expected = ["ON", "OFF", "ON"][step];
            ensure!(activity && body["state"] == expected, "expected measured {expected}, got {body}");
            eprintln!("MQTT measured {expected} at {}", jiff::Timestamp::now());
            step += 1;
            if step == 3 { return Ok(()); }
            activity = false;
            let next = if step == 1 { "OFF" } else { "ON" };
            client.publish(format!("{device}/set"), QoS::AtLeastOnce, false,
                json!({"state": next}).to_string()).await?;
        }
    }
}
