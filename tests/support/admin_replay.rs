//! Repeat completed management operations after restoring the stopped state.
//! The simulated NCP rejects any attempt to send a network command in this session.
use super::*;
use std::{fs, path::Path};

pub fn copy_stopped(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir(&dest)?;
            copy_stopped(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn interrupted_bind() -> Value {
    json!({"from": "lamp-2", "to": "Coordinator", "clusters": ["genOnOff"], "transaction": "interrupted-bind"})
}

fn conflicted_bind() -> Value {
    json!({"from":"lamp-5", "to":"Coordinator","clusters":["genOnOff","genLevelCtrl"],"transaction":"conflict-bind"})
}

pub async fn bind_across_conflict(
    client: &AsyncClient,
    events: &mut rumqttc::EventLoop,
    base: &str,
) -> Result<()> {
    let unaffected = json!({"from":"lamp-6","to":"Coordinator","clusters":["genOnOff"],"transaction":"unaffected-bind"});
    for (request, expected) in [(conflicted_bind(), "error"), (unaffected, "ok")] {
        client
            .publish(
                format!("{base}/bridge/request/device/bind"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&request)?,
            )
            .await?;
        loop {
            if let Event::Incoming(Incoming::Publish(p)) = events.poll().await?
                && p.topic == format!("{base}/bridge/response/device/bind")
            {
                let result: Value = serde_json::from_slice(&p.payload)?;
                if result["transaction"] != request["transaction"] {
                    continue;
                }
                ensure!(result["status"] == expected, "wrong bind outcome: {result}");
                if expected == "error" {
                    ensure!(
                        result["error"]
                            .as_str()
                            .is_some_and(|s| s.contains("unknown")),
                        "bind outcome was not marked uncertain: {result}"
                    );
                } else {
                    ensure!(result["data"]["clusters"] == json!(["genOnOff"]));
                }
                break;
            }
        }
    }
    Ok(())
}

pub async fn interrupt_bind(
    client: &AsyncClient,
    events: &mut rumqttc::EventLoop,
    base: &str,
) -> Result<()> {
    client
        .publish(
            format!("{base}/bridge/request/device/bind"),
            QoS::AtLeastOnce,
            false,
            serde_json::to_vec(&interrupted_bind())?,
        )
        .await?;
    loop {
        if let Event::Incoming(Incoming::Publish(p)) = events.poll().await?
            && p.topic == format!("{base}/lamp-2")
            && serde_json::from_slice::<Value>(&p.payload)?["brightness"] == 78
        {
            return Ok(());
        }
    }
}

pub async fn verify(base: &str) -> Result<()> {
    let (client, mut events) =
        AsyncClient::new(test_broker(format!("{base}-restored-observer"))?, 16);
    client
        .subscribe(format!("{base}/bridge/#"), QoS::AtLeastOnce)
        .await?;
    loop {
        if let Event::Incoming(Incoming::Publish(p)) = events.poll().await?
            && p.topic == format!("{base}/bridge/state")
            && serde_json::from_slice::<Value>(&p.payload)?["state"] == "online"
        {
            break;
        }
    }
    for (op, body, expected) in [
        ("device/bind", interrupted_bind(), "error"),
        ("device/bind", conflicted_bind(), "error"),
        (
            "device/bind",
            json!({"transaction":"bind","from":"lamp-1","to":"Coordinator","clusters":["genOnOff"]}),
            "ok",
        ),
        (
            "device/ota_update/update",
            json!({"id":"lamp-1","transaction":"up"}),
            "ok",
        ),
        ("permit_join", json!({"time":3,"transaction":"join"}), "ok"),
        (
            "permit_join",
            json!({"time":12,"transaction":"join"}),
            "error",
        ),
        (
            "device/describe",
            json!({"id":format!("0x{:016x}",ieee(enrolment::NEW)),"endpoint":endpoint(enrolment::NEW),"transaction":"restored-zone"}),
            "ok",
        ),
    ] {
        client
            .publish(
                format!("{base}/bridge/request/{op}"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&body)?,
            )
            .await?;
        loop {
            if let Event::Incoming(Incoming::Publish(p)) = events.poll().await?
                && p.topic == format!("{base}/bridge/response/{op}")
            {
                let got: Value = serde_json::from_slice(&p.payload)?;
                ensure!(
                    got["transaction"] == body["transaction"] && got["status"] == expected,
                    "unexpected restored response: {got}"
                );
                match (op, expected) {
                    ("permit_join", "ok") => ensure!(got["data"]["time"] == 3),
                    ("permit_join", _) => ensure!(
                        got["error"]
                            .as_str()
                            .unwrap_or("")
                            .contains("different request")
                    ),
                    ("device/bind", "error") if body["transaction"] == "conflict-bind" => ensure!(
                        got["error"].as_str().is_some_and(|e| e.contains("unknown")),
                        "lost the original interrupted bind outcome: {got}"
                    ),
                    ("device/bind", "error") => ensure!(
                        got["error"]
                            .as_str()
                            .is_some_and(|e| e.contains("indeterminate"))
                    ),
                    ("device/bind", _) => ensure!(got["data"]["clusters"] == json!(["genOnOff"])),
                    ("device/ota_update/update", _) => {
                        ensure!(got["data"]["to"]["file_version"] == 8)
                    }
                    _ => ensure!(
                        got["data"]["ias_enrollment"] == json!({"state":"confirmed","zone_id":0})
                    ),
                }
                break;
            }
        }
    }
    // Get Alarm dequeues a record on the device. A broker retry after restoring
    // the coordinator must return the journalled record without dequeuing again.
    client
        .publish(
            format!("{base}/bridge/v1/request/device/command"),
            QoS::AtLeastOnce,
            false,
            serde_json::to_vec(&json!({"id":"lamp-1","endpoint":endpoint(1),
            "command":{"op":"alarm_pop"},"transaction":"alarm_pop"}))?,
        )
        .await?;
    loop {
        if let Event::Incoming(Incoming::Publish(p)) = events.poll().await?
            && p.topic == format!("{base}/bridge/v1/response/device/command")
        {
            ensure!(
                serde_json::from_slice::<Value>(&p.payload)?
                    == json!({"transaction":"alarm_pop","status":"ok","data":{"cluster":1,"code":7,"zigbee_time":2}})
            );
            break;
        }
    }
    // The restored coordinator has no PIN directory. Repeating a transaction returns
    // its original readback, and the simulated adapter rejects any additional send.
    super::lock_control::verify(&client, &mut events, base).await?;
    Ok(())
}
