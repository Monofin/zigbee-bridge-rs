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
pub fn respond(cluster: u8, frame: Frame<'_>, switched_off: &mut BTreeSet<u16>) -> Result<Vec<Vec<u8>>> {
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
    if cluster == 8 {
        return Ok(vec![incoming(LIGHT, 8, &[0x18, seq, 1, 0, 0, 0, 0x20, 97])]);
    }
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
    let feedback = format!("{BASE}/bridge/feedback/lamp-{LIGHT}/@light");
    let diagnostics = format!("{BASE}/bridge/diagnostics/lamp-{LIGHT}");
    let mut started = false;
    let mut step = 0;
    let mut activity = false;
    loop {
        let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? else { continue };
        let body: Value = serde_json::from_slice(&msg.payload)?;
        if !started && msg.topic == format!("{BASE}/bridge/state") && body["state"] == "online" {
            started = true;
            // A level reply alone cannot confirm power. The bridge must read OnOff itself.
            client.publish(format!("{device}/get"), QoS::AtLeastOnce, false, r#"{"brightness":""}"#).await?;
        } else if msg.topic == diagnostics {
            ensure!(body.get("state").is_none());
            activity |= body.get("last_seen").is_some();
        } else if msg.topic == feedback {
            ensure!(body["brightness"] == 97);
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

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn seven_lamp_burst_recovers_missing_delivery_and_read_reply() -> Result<()> {
    let dir = tempfile::tempdir()?;
    run_session(19, "power-burst", dir.path(), Scenario::PowerBurst).await
}

pub async fn observe_burst(base: &str) -> Result<()> {
    let (client, mut net) = AsyncClient::new(test_broker("power-burst-witness".into())?, 32);
    client.subscribe(format!("{base}/#"), QoS::AtLeastOnce).await?;
    let mut started = false;
    let mut confirmed = BTreeSet::new();
    loop {
        let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? else { continue };
        if !started && msg.topic == format!("{base}/bridge/state")
            && serde_json::from_slice::<Value>(&msg.payload)?["state"] == "online" {
            started = true;
            for lamp in 2..=8 {
                client.publish(format!("{base}/lamp-{lamp}/set"), QoS::AtLeastOnce, false, r#"{"state":"OFF"}"#).await?;
            }
        } else if started && !msg.retain && let Some(lamp) = msg.topic.strip_prefix(&format!("{base}/bridge/feedback/lamp-"))
            .and_then(|tail| tail.strip_suffix("/@light")).and_then(|id| id.parse::<u16>().ok())
            && (2..=8).contains(&lamp) {
            let body: Value = serde_json::from_slice(&msg.payload)?;
            if let Some(state) = body.get("state") {
                ensure!(state == "OFF", "stale or unmeasured feedback for lamp {lamp}: {state}");
                confirmed.insert(lamp);
                if confirmed.len() == 7 { return Ok(()); }
            }
        }
    }
}
