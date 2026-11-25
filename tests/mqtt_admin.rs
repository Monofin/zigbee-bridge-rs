//! Wire-level bridge API against an isolated loopback Mosquitto broker.
use anyhow::Result;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, Publish, QoS};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use zigbee_bridge_rs::mqtt::{
    self,
    admin::{Action, Selector},
};

const BASE: &str = "admin-test";
fn options(id: &str) -> MqttOptions {
    MqttOptions::new(id, "127.0.0.1", 1883)
}

async fn matched(rx: &mut mpsc::Receiver<Publish>, topic: &str) -> Result<Publish> {
    while let Some(msg) = rx.recv().await {
        if msg.topic == topic {
            return Ok(msg);
        }
    }
    anyhow::bail!("MQTT observer stopped")
}
async fn response(rx: &mut mpsc::Receiver<Publish>, path: &str, tx: &str) -> Result<Value> {
    let topic = format!("{BASE}/bridge/response/{path}");
    let msg = matched(rx, &topic).await?;
    assert!(!msg.retain);
    let result: Value = serde_json::from_slice(&msg.payload)?;
    assert_eq!(result["transaction"], tx);
    Ok(result)
}
async fn send(client: &AsyncClient, path: &str, data: Value) -> Result<()> {
    client
        .publish(
            format!("{BASE}/bridge/request/{path}"),
            QoS::AtLeastOnce,
            false,
            serde_json::to_vec(&data)?,
        )
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated loopback Mosquitto on port 1883"]
async fn broker_dispatches_admin_and_switch_follows_confirmed_radio_state() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), admin()).await?
}
async fn admin() -> Result<()> {
    let (client, mut net) = AsyncClient::new(options("admin-observer"), 16);
    client.subscribe("#", QoS::AtLeastOnce).await?;
    while !matches!(net.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
    let request = format!("{BASE}/bridge/request/permit_join");
    client
        .publish(
            &request,
            QoS::AtLeastOnce,
            true,
            br#"{"transaction":"stale","time":254}"#.as_slice(),
        )
        .await?;
    while !matches!(net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
    let (tx, mut observed) = mpsc::channel(64);
    let observer = tokio::spawn(async move {
        loop {
            if let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? && tx.send(msg).await.is_err() {
                return Ok::<(), anyhow::Error>(());
            }
        }
    });
    let (p, mut worker, _) = mqtt::channel(mqtt::MqttOptions::new("admin-worker", "127.0.0.1", 1883), BASE, 1)?;
    let mut admin = worker.enable_admin()?;
    assert!(worker.enable_admin().is_err());
    p.discover_permit_join("test-ha", 7).await?;
    p.permit_state(false, None)?;
    let serving = tokio::spawn(worker.run());
    let info = format!("{BASE}/bridge/info");
    assert_eq!(
        serde_json::from_slice::<Value>(&matched(&mut observed, &info).await?.payload)?,
        json!({"api_version":1,"permit_join":false})
    );
    // The retained permit command from before subscription must be ignored.
    send(&client, "health_check", json!({"transaction":"health"})).await?;
    let health = admin.recv().await.unwrap();
    assert!(matches!(health.action, Action::HealthCheck));
    health.reply.ok(json!({"healthy":true}))?;
    assert_eq!(
        response(&mut observed, "health_check", "health").await?["data"],
        json!({"healthy":true})
    );
    // Exercise the versioned wire route through the same radio owner.
    client.publish(format!("{BASE}/bridge/v2/request/permit_join"), QoS::AtLeastOnce,
        false, r#"{"time":30}"#).await?;
    client.publish(format!("{BASE}/bridge/v1/request/health_check"), QoS::AtLeastOnce,
        false, r#"{"transaction":"probe"}"#).await?;
    let probe = admin.recv().await.unwrap();
    assert!(matches!(probe.action, Action::HealthCheck));
    probe.reply.ok(json!({"healthy":true}))?;
    let msg = matched(&mut observed, &format!("{BASE}/bridge/v1/response/health_check")).await?;
    assert_eq!(serde_json::from_slice::<Value>(&msg.payload)?,
        json!({"status":"ok","data":{"healthy":true},"transaction":"probe"}));
    send(
        &client,
        "permit_join",
        json!({"time":30,"transaction":"first"}),
    )
    .await?;
    let first = admin.recv().await.unwrap();
    assert!(matches!(
        first.action,
        Action::PermitJoin {
            time: 30,
            device: None
        }
    ));
    // A received command cannot update the retained info while the radio is working.
    send(
        &client,
        "permit_join",
        json!({"time":12,"transaction":"full"}),
    )
    .await?;
    let full = response(&mut observed, "permit_join", "full").await?;
    assert_eq!(full["status"], "error");
    assert_eq!(full["retry"], "same_transaction");
    assert_eq!(full["data"], json!({}));
    assert!(full["error"].as_str().unwrap().contains("full"));
    send(&client, "permit_join", json!({"time":30,"transaction":"first"})).await?;
    let duplicate = response(&mut observed, "permit_join", "first").await?;
    assert_eq!(duplicate["retry"], "same_transaction");
    assert!(duplicate["error"].as_str().unwrap().contains("same transaction ID"));
    p.permit_state(true, Some(123456))?;
    first.reply.ok(json!({"time":30}))?;
    assert_eq!(
        serde_json::from_slice::<Value>(&matched(&mut observed, &info).await?.payload)?,
        json!({"api_version":1,"permit_join":true,"permit_join_end":123456})
    );
    assert_eq!(
        response(&mut observed, "permit_join", "first").await?["data"],
        json!({"time":30})
    );
    send(
        &client,
        "permit_join",
        json!({"time":255,"transaction":"invalid"}),
    )
    .await?;
    assert_eq!(
        response(&mut observed, "permit_join", "invalid").await?["status"],
        "error"
    );
    for op in ["bind", "unbind"] {
        let path = format!("device/{op}");
        send(
            &client,
            &path,
            json!({"from":"button","to":"Coordinator","from_endpoint":1,
            "clusters":["genOnOff"],"transaction":"binding"}),
        )
        .await?;
        let req = admin.recv().await.unwrap();
        let bind = match req.action {
            Action::Bind(b) if op == "bind" => b,
            Action::Unbind(b) if op == "unbind" => b,
            _ => panic!("wrong binding operation"),
        };
        assert_eq!(bind.from_endpoint, Some(Selector::Number(1)));
        assert_eq!(bind.to, Selector::Name("Coordinator".into()));
        assert_eq!(bind.clusters.unwrap(), ["genOnOff"]);
        req.reply.error("Radio did not confirm binding")?;
        assert_eq!(
            response(&mut observed, &path, "binding").await?["error"],
            "Radio did not confirm binding"
        );
    }
    for op in ["check", "update", "schedule", "unschedule"] {
        let path = format!("device/ota_update/{op}");
        send(&client, &path, json!({"id":"lamp","transaction":"ota"})).await?;
        let req = admin.recv().await.unwrap();
        match req.action {
            Action::OtaUpdate(v) if op == "update" => assert_eq!(v.id, "lamp"),
            Action::OtaCheck(v) if op == "check" => assert_eq!(v.id, "lamp"),
            Action::OtaSchedule(v) if op == "schedule" => assert_eq!(v.id, "lamp"),
            Action::OtaUnschedule { id } if op == "unschedule" => assert_eq!(id, "lamp"),
            _ => panic!("wrong OTA operation"),
        }
        // Losing a handler cannot silently discard an accepted transaction.
        drop(req.reply);
        assert_eq!(
            response(&mut observed, &path, "ota").await?["status"],
            "error"
        );
    }
    let ota_endpoint = mqtt::discovery::Endpoint {
            device_availability:false,
        ieee: 9,
        endpoint: 11,
        device_name: "test-lamp",
        topic_name: "test-lamp/ep11",
    };
    p.discover_when_ready(
        "test-ha",
        &ota_endpoint,
        mqtt::discovery::Feature::FirmwareUpdate,
    )
    .await?;
    let config = matched(
        &mut observed,
        "test-ha/update/0x0000000000000009/update/config",
    )
    .await?;
    let data: Value = serde_json::from_slice(&config.payload)?;
    assert_eq!(data["unique_id"], "0x0000000000000009_update_admin-test");
    assert_eq!(data["state_topic"], "admin-test/test-lamp");
    assert_eq!(
        data["command_topic"],
        "admin-test/bridge/request/device/ota_update/update"
    );
    assert_eq!(
        serde_json::from_str::<Value>(data["payload_install"].as_str().unwrap())?,
        json!({"id":"0x0000000000000009"})
    );
    assert_eq!(
        p.update_state(
            "test-lamp",
            10,
            11,
            mqtt::admin::UpdatePhase::Idle,
            Some(20)
        ),
        Err(mqtt::SubmitError::Invalid)
    );
    p.update_state(
        "test-lamp",
        10,
        11,
        mqtt::admin::UpdatePhase::Updating,
        Some(37),
    )?;
    let progress = matched(&mut observed, "admin-test/test-lamp").await?;
    assert_eq!(
        serde_json::from_slice::<Value>(&progress.payload)?,
        json!({"update":{"installed_version":10,"latest_version":11,"state":"updating","progress":37}})
    );
    p.permit_state(false, None)?;
    p.close();
    serving.await??;
    observer.abort();
    // A fresh HA subscriber receives precisely the confirmed closed state and
    // switch config; successful/failed administrative responses do not replay.
    let (fresh, mut net) = AsyncClient::new(options("admin-fresh"), 8);
    fresh.subscribe("#", QoS::AtLeastOnce).await?;
    while !matches!(net.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
    fresh
        .publish("admin-fence", QoS::AtLeastOnce, false, "done")
        .await?;
    let mut switch = false;
    let mut confirmed = false;
    loop {
        if let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? {
            if msg.topic == "admin-fence" {
                break;
            }
            assert!(!msg.topic.starts_with("admin-test/bridge/response/"));
            if msg.topic.starts_with("test-ha/") && msg.topic.ends_with("/permit_join/config") {
                let conf: Value = serde_json::from_slice(&msg.payload)?;
                assert!(msg.retain);
                assert_eq!(conf["command_topic"], format!("{BASE}/bridge/v1/request/permit_join"));
                assert_eq!(conf["state_topic"], info);
                assert_eq!(
                    conf["unique_id"],
                    "bridge_0x0000000000000007_permit_join_admin-test"
                );
                switch = true;
            }
            if msg.topic == info {
                assert!(msg.retain);
                assert_eq!(
                    serde_json::from_slice::<Value>(&msg.payload)?,
                    json!({"api_version":1,"permit_join":false})
                );
                confirmed = true;
            }
        }
    }
    assert!(confirmed && switch);
    Ok(())
}
