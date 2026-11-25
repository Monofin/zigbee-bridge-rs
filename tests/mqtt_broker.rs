//! Run with an empty loopback Mosquitto broker in an isolated network namespace.
use anyhow::Result;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use zigbee_bridge_rs::mqtt::{self, Operation};

fn options(id: &str) -> MqttOptions {
    MqttOptions::new(id, "127.0.0.1", 1883)
}

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn broker_burst_gestures_commands_and_shutdown() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), run()).await?
}

async fn run() -> Result<()> {
    let base = "rust-test";
    let (observer, mut net) = AsyncClient::new(options("mqtt-observer"), 8);
    observer.subscribe("rust-test/#", QoS::AtLeastOnce).await?;
    while !matches!(net.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
    observer
        .publish(
            "rust-test/kitchen/test/set",
            QoS::AtLeastOnce,
            true,
            br#"{"state":"ON"}"#.as_slice(),
        )
        .await?;
    while !matches!(net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
    let (p, worker, mut commands) = mqtt::channel(mqtt::MqttOptions::new("mqtt-worker", "127.0.0.1", 1883), base, 256)?;
    // Simulate a simultaneous event burst before the publisher gets a timeslice.
    for level in 0..20 {
        for light in 0..256 {
            p.state(&format!("light-{light}"), "brightness", json!(level))?;
        }
    }
    for press in 0..100 {
        p.event("switch", "action", json!(press))?;
    }
    let task = tokio::spawn(worker.run());
    let mut lights = BTreeSet::new();
    let mut presses = Vec::new();
    let mut raw_presses = Vec::new();
    let mut online = false;
    while lights.len() < 256 || presses.len() < 100 || raw_presses.len() < 100 || !online {
        if let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? {
            let body: Value = serde_json::from_slice(&msg.payload)?;
            if msg.topic.starts_with("rust-test/light-") {
                assert_eq!(body, json!({"brightness":19}));
                assert!(lights.insert(msg.topic));
            } else if msg.topic == "rust-test/switch" {
                assert!(!msg.retain);
                presses.push(body["action"].as_u64().unwrap());
            } else if msg.topic == "rust-test/switch/action" {
                assert!(!msg.retain);
                raw_presses.push(std::str::from_utf8(&msg.payload)?.parse::<u64>()?);
            } else if msg.topic == "rust-test/bridge/state" {
                online = body == json!({"state":"online"});
            }
        }
    }
    assert_eq!(presses, (0..100).collect::<Vec<_>>());
    assert_eq!(raw_presses, presses);
    for (op, body) in [
        ("set", json!({"state":"OFF"})),
        ("get", json!({"brightness":""})),
    ] {
        observer
            .publish(
                format!("{base}/kitchen/test/{op}"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&body)?,
            )
            .await?;
        while !matches!(net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
        let cmd = commands.recv().await.unwrap();
        assert_eq!(cmd.device, "kitchen/test");
        assert_eq!(
            cmd.operation,
            if op == "set" {
                Operation::Set
            } else {
                Operation::Get
            }
        );
        assert_eq!(cmd.properties, body.as_object().unwrap().clone());
    }
    for (op, prop, payload, value) in [
        ("set", "temperature_unit", "fahrenheit", json!("fahrenheit")),
        ("set", "maximum_range", "4.65", json!(4.65)),
        ("set", "enabled", "true", json!(true)),
        ("get", "state", "", json!("")),
    ] {
        observer
            .publish(
                format!("{base}/kitchen/test/{op}/{prop}"),
                QoS::AtLeastOnce,
                false,
                payload,
            )
            .await?;
        while !matches!(net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
        let cmd = commands.recv().await.unwrap();
        assert_eq!(cmd.device, "kitchen/test");
        assert_eq!(
            cmd.properties,
            serde_json::Map::from_iter([(prop.to_owned(), value)])
        );
    }
    // Stall the command consumer while a second client sends 320 writes.
    // Broker I/O and publication of all 256 concurrent device reports must continue.
    let (writer, mut writer_net) = AsyncClient::new(options("mqtt-flood"), 8);
    let write = tokio::spawn(async move {
        let send = async {
            for n in 0..320 {
                writer
                    .publish(
                        "rust-test/bank/set",
                        QoS::AtLeastOnce,
                        false,
                        serde_json::to_vec(&json!({"brightness":n}))?,
                    )
                    .await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let poll = async {
            let mut acks = 0;
            while acks < 320 {
                if matches!(
                    writer_net.poll().await?,
                    Event::Incoming(Incoming::PubAck(_))
                ) {
                    acks += 1;
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::try_join!(send, poll)
    });
    let events = p.clone();
    let reports = tokio::spawn(async move {
        for n in 0..256 {
            events.state(&format!("light-{n}"), "state", json!("OFF"))?;
            tokio::task::yield_now().await;
        }
        Ok::<(), mqtt::SubmitError>(())
    });
    let mut rejected = 0;
    lights.clear();
    while rejected < 64 || lights.len() < 256 {
        if let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? {
            if msg.topic.starts_with("rust-test/light-") {
                assert_eq!(
                    serde_json::from_slice::<Value>(&msg.payload)?,
                    json!({"state":"OFF","brightness":19})
                );
                assert!(lights.insert(msg.topic));
            } else if msg.topic == "rust-test/bridge/logging" {
                let body: Value = serde_json::from_slice(&msg.payload)?;
                assert_eq!(body["meta"]["latest_device"], "bank");
                let totals = &body["meta"]["totals"];
                assert_eq!(totals["states"], 0);
                assert_eq!(totals["events"], 0);
                let current = totals["commands"].as_u64().unwrap();
                assert!(current > rejected);
                rejected = current;
            }
        }
    }
    write.await??;
    reports.await??;
    assert_eq!(p.rejected_commands(), 64);
    for n in 0..256 {
        let accepted = commands.try_recv()?;
        assert_eq!(accepted.properties["brightness"], n);
    }
    assert!(commands.try_recv().is_err());
    p.close();
    task.await??;
    // A fresh observer gets only the offline state and the command retained
    // before startup. No stale button action is present on resubscription.
    let (fresh, mut net) = AsyncClient::new(options("mqtt-fresh"), 8);
    fresh.subscribe("rust-test/#", QoS::AtLeastOnce).await?;
    while !matches!(net.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
    fresh
        .publish("rust-test/end", QoS::AtLeastOnce, false, "end")
        .await?;
    let mut offline = false;
    loop {
        if let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? {
            match msg.topic.as_str() {
                "rust-test/end" => break,
                "rust-test/kitchen/test/set" => assert!(msg.retain),
                "rust-test/bridge/state" => {
                    assert!(msg.retain);
                    assert_eq!(
                        serde_json::from_slice::<Value>(&msg.payload)?,
                        json!({"state":"offline"})
                    );
                    offline = true;
                }
                other => panic!("unexpected retained topic {other}"),
            }
        }
    }
    assert!(offline);
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn lost_connection_revokes_sender_and_broker_publishes_will() -> Result<()> {
    use tokio::net::{TcpListener, TcpStream};
    tokio::time::timeout(Duration::from_secs(10), async {
        let (observer, mut net) = AsyncClient::new(options("fault-observer"), 8);
        observer
            .subscribe("mqtt-fault/bridge/state", QoS::AtLeastOnce)
            .await?;
        while !matches!(net.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
        let proxy = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = proxy.local_addr()?.port();
        let (cut, cut_rx) = tokio::sync::oneshot::channel();
        let splice = tokio::spawn(async move {
            let (mut worker, _) = proxy.accept().await?;
            let mut broker = TcpStream::connect(("127.0.0.1", 1883)).await?;
            tokio::select! {
                _ = cut_rx => Ok::<(),std::io::Error>(()),
                r = tokio::io::copy_bidirectional(&mut worker, &mut broker) => r.map(|_| ()),
            }
        });
        let (publisher, worker, _commands) = mqtt::channel(
            mqtt::MqttOptions::new("fault-worker", "127.0.0.1", port),
            "mqtt-fault",
            2,
        )?;
        let running = tokio::spawn(worker.run());
        while !matches!(net.poll().await?, Event::Incoming(Incoming::Publish(p)) if
            serde_json::from_slice::<Value>(&p.payload)? == json!({"state":"online"}))
        {}
        cut.send(()).unwrap();
        splice.await??;
        assert!(running.await?.is_err());
        assert_eq!(
            publisher.event("button", "action", json!("single")),
            Err(mqtt::SubmitError::Closed)
        );
        while !matches!(net.poll().await?, Event::Incoming(Incoming::Publish(p)) if
            serde_json::from_slice::<Value>(&p.payload)? == json!({"state":"offline"}))
        {}
        Ok::<(), anyhow::Error>(())
    })
    .await?
}
