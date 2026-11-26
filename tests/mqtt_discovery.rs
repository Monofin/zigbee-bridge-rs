//! Run against a disposable loopback-only Mosquitto broker.
use anyhow::Result;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use zigbee_bridge_rs::mqtt::{
    self,
    discovery::{Endpoint, Feature, Sensor},
};

const PREFIX: &str = "ha-test";
const BASE: &str = "bench-discovery";
fn options(id: &str) -> mqtt::MqttOptions {
    mqtt::MqttOptions::new(id, "127.0.0.1", 1883)
}

async fn retained() -> Result<BTreeMap<String, Value>> {
    let (client, mut net) = AsyncClient::new(MqttOptions::new("ha-reader", "127.0.0.1", 1883), 8);
    client
        .subscribe(format!("{PREFIX}/#"), QoS::AtLeastOnce)
        .await?;
    while !matches!(net.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
    // Broker ordering on this connection fences retained subscription replay.
    let done = format!("{PREFIX}/done");
    client
        .publish(&done, QoS::AtLeastOnce, false, "done")
        .await?;
    let mut configs = BTreeMap::new();
    loop {
        if let Event::Incoming(Incoming::Publish(p)) = net.poll().await? {
            if p.topic == done {
                return Ok(configs);
            }
            assert!(p.retain);
            configs.insert(p.topic, serde_json::from_slice(&p.payload)?);
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated loopback Mosquitto on port 1883"]
async fn late_ha_replays_config_rename_keeps_ids_and_only_removal_retracts() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut lamp = Endpoint {
            device_availability:false,
            ieee: 1,
            endpoint: 1,
            device_name: "test lamp",
            topic_name: "lights/lamp",
        };
        let button = Endpoint {
            device_availability:false,
            ieee: 2,
            endpoint: 1,
            device_name: "test button",
            topic_name: "button",
        };
        let sensor = Endpoint {
            device_availability:false,
            ieee: 3,
            endpoint: 4,
            device_name: "test multisensor",
            topic_name: "sensor/ep4",
        };
        let light = Feature::Light {
            effects: &["blink", "breathe"],
            xy: false,
            hs: false,
            brightness: true,
            color_temperature: true,
            mired_range: Some((153, 500)),
        };
        let (p, worker, _) = mqtt::channel(options("ha-worker"), BASE, 3)?;
        // Ten configs cross a queue with only three slots. A trigger batch
        // must wait until all three fit while the MQTT driver continues draining.
        let running = tokio::spawn(worker.run());
        p.discover_when_ready(PREFIX, &lamp, light).await?;
        p.discover_when_ready(PREFIX, &button, Feature::Button)
            .await?;
        for s in [
            Sensor::LinkQuality,
            Sensor::Battery,
            Sensor::Voltage,
            Sensor::Temperature,
            Sensor::Humidity,
            Sensor::Pressure,
        ] {
            p.discover_when_ready(PREFIX, &sensor, Feature::Sensor(s))
                .await?;
        }
        p.close();
        running.await??;
        let initial = retained().await?;
        assert_eq!(initial.len(), 10);
        let lqi = &initial["ha-test/sensor/0x0000000000000003/linkquality/config"];
        assert_eq!(lqi["state_topic"], "bench-discovery/bridge/feedback/test multisensor/linkquality");
        assert_eq!(lqi["unit_of_measurement"], "lqi");
        assert_eq!(lqi["enabled_by_default"], false);
        assert_eq!(lqi["entity_category"], "diagnostic");
        assert!(lqi.get("device_class").is_none());
        let lamp_topic = "ha-test/light/0x0000000000000001/light/config";
        let id = "0x0000000000000001_light_bench-discovery";
        assert_eq!(initial[lamp_topic]["unique_id"], id);
        assert_eq!(initial[lamp_topic]["brightness_scale"], 254);
        assert_eq!(initial[lamp_topic]["effect"], true);
        assert_eq!(initial[lamp_topic]["effect_list"], serde_json::json!(["blink","breathe"]));
        assert_eq!(
            initial[lamp_topic]["supported_color_modes"],
            serde_json::json!(["color_temp"])
        );
        assert_eq!(initial[lamp_topic]["min_mireds"], 153);
        assert_eq!(
            initial[lamp_topic]["device"]["identifiers"][0],
            "zigbee2mqtt_0x0000000000000001"
        );
        for (prop, unit) in [
            ("battery", "%"),
            ("voltage", "mV"),
            ("temperature", "°C"),
            ("humidity", "%"),
            ("pressure", "hPa"),
        ] {
            let config = &initial[&format!("{PREFIX}/sensor/0x0000000000000003/{prop}_ep4/config")];
            assert_eq!(config["unit_of_measurement"], unit);
            assert_eq!(
                config["unique_id"],
                format!("0x0000000000000003_{prop}_ep4_{BASE}")
            );
        }
        for action in ["single", "double", "hold"] {
            let config = &initial
                [&format!("{PREFIX}/device_automation/0x0000000000000002/action_{action}/config")];
            assert_eq!(config["automation_type"], "trigger");
            assert_eq!(config["subtype"], action);
            assert_eq!(config["payload"], action);
            assert_eq!(config["topic"], "bench-discovery/button/action");
        }
        let (p, worker, _) = mqtt::channel(options("ha-worker"), BASE, 16)?;
        lamp.device_name = "renamed lamp";
        lamp.topic_name = "lights/new name";
        p.discover(PREFIX, &lamp, light)?;
        p.remove_discovery(PREFIX, &button, Feature::Button)?;
        p.close();
        worker.run().await?;
        let after = retained().await?;
        assert_eq!(after.len(), 7);
        assert_eq!(after[lamp_topic]["unique_id"], id);
        assert_eq!(
            after[lamp_topic]["command_topic"],
            "bench-discovery/lights/new name/set"
        );
        assert!(after.keys().all(|t| !t.contains("/device_automation/")));
        for (topic, body) in initial.iter().filter(|(k, _)| k.contains("/sensor/")) {
            assert_eq!(after[topic], *body);
        }
        Ok::<(), anyhow::Error>(())
    })
    .await?
}

#[test]
fn action_batch_is_atomic_under_capacity_pressure() -> Result<()> {
    let (p, _worker, _) = mqtt::channel(options("no-connect"), BASE, 3)?;
    let ep = Endpoint {
            device_availability:false,
        ieee: 1,
        endpoint: 1,
        device_name: "one",
        topic_name: "one",
    };
    p.discover(PREFIX, &ep, Feature::Sensor(Sensor::Battery))?;
    assert_eq!(
        p.discover(PREFIX, &ep, Feature::Button),
        Err(mqtt::SubmitError::Full)
    );
    p.discover(PREFIX, &ep, Feature::Sensor(Sensor::Voltage))?;
    p.discover(PREFIX, &ep, Feature::Sensor(Sensor::Temperature))?;
    Ok(())
}

#[tokio::test]
async fn cancelled_or_closed_waiter_cannot_publish_partial_config() -> Result<()> {
    let ep = Endpoint {
            device_availability:false,
        ieee: 1,
        endpoint: 1,
        device_name: "one",
        topic_name: "one",
    };
    let (small, _worker, _) = mqtt::channel(options("small"), BASE, 2)?;
    assert_eq!(
        small
            .discover_when_ready(PREFIX, &ep, Feature::Button)
            .await,
        Err(mqtt::SubmitError::Invalid)
    );
    let (p, _worker, _) = mqtt::channel(options("close"), BASE, 3)?;
    p.discover(PREFIX, &ep, Feature::Sensor(Sensor::Battery))?;
    let waiting = p.discover_when_ready(PREFIX, &ep, Feature::Button);
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        finished=&mut waiting => panic!("expected full queue, got {finished:?}"),
        _=tokio::task::yield_now()=>{}
    }
    p.close();
    assert_eq!(waiting.await, Err(mqtt::SubmitError::Closed));
    Ok(())
}

#[test]
fn relay_valve_meter_and_alarm_definitions_can_be_registered_together() -> Result<()> {
    let (p, _worker, _) = mqtt::channel(options("schema-test"), BASE, 16)?;
    let ep = Endpoint {
            device_availability:false,
        ieee: 11,
        endpoint: 1,
        device_name: "sample",
        topic_name: "sample",
    };
    for feature in [
        Feature::Valve,
        Feature::Switch,
        Feature::Binary("tamper", "tamper"),
        Feature::Actions(&["hold", "toggle"]),
        Feature::Sensor(Sensor::Power),
        Feature::Sensor(Sensor::Energy),
        Feature::Sensor(Sensor::MainsVoltage),
        Feature::Sensor(Sensor::Current),
    ] {
        p.discover(PREFIX, &ep, feature)?;
    }
    Ok(())
}
