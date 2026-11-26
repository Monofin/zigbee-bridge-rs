//! A reinterview changes the HA domain of the same Binary Output property.
use super::*;
use crate::interview_log::{AttributeResult, Descriptor, Entry, IdentitySource, Observation};
use crate::mqtt::{MqttOptions, channel_with_prefix, discovery::Feature};
use rumqttc::{AsyncClient, Event, Incoming, QoS};
use std::collections::BTreeSet;

const MEMBER: u64 = 0x0200_0000_0000_0002;

fn report(writable: bool) -> InterviewReport {
    let servers = if writable {
        vec![0x10]
    } else {
        vec![0x10, 0x406]
    };
    let observations = [
        Observation::Target {
            ieee: format!("{MEMBER:016x}"),
            short: 32,
            identity_source: IdentitySource::Announcement,
        },
        Observation::ActiveEndpoints {
            sequence: 1,
            endpoints: vec![1],
        },
        Observation::SimpleDescriptor {
            sequence: 2,
            endpoint: 1,
            descriptor: Descriptor {
                profile: 0x104,
                device: 0xffff,
                version: 1,
                servers,
                clients: vec![],
            },
        },
        Observation::AttributeAccess {
            endpoint: 1,
            cluster: 0x10,
            attribute: 0x55,
            data_type: 0x10,
            access: if writable { 3 } else { 1 },
        },
        Observation::Attribute {
            sequence: 3,
            endpoint: 1,
            cluster: 0x10,
            attribute: 0x51,
            result: AttributeResult::Value {
                data_type: 0x10,
                bytes: vec![0],
            },
        },
        Observation::Attribute {
            sequence: 4,
            endpoint: 1,
            cluster: 0x10,
            attribute: 0x55,
            result: AttributeResult::Value {
                data_type: 0x10,
                bytes: vec![0],
            },
        },
        Observation::Finished,
    ];
    InterviewReport {
        finished: true,
        entries: observations
            .into_iter()
            .map(|observation| Entry {
                unix_ms: 0,
                observation,
            })
            .collect(),
    }
}

async fn retained(prefix: &str) -> Result<BTreeSet<String>> {
    let (observer, mut wire) = AsyncClient::new(
        rumqttc::MqttOptions::new("reinterview-witness", "127.0.0.1", 1883),
        8,
    );
    observer
        .subscribe(format!("{prefix}/#"), QoS::AtLeastOnce)
        .await?;
    while !matches!(wire.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
    let fence = format!("{prefix}/fence");
    observer
        .publish(&fence, QoS::AtLeastOnce, false, "done")
        .await?;
    let mut found = BTreeSet::new();
    loop {
        if let Event::Incoming(Incoming::Publish(p)) = wire.poll().await? {
            if p.topic == fence {
                return Ok(found);
            }
            assert!(p.retain);
            found.insert(p.topic);
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated loopback Mosquitto on port 1883"]
async fn reinterview_retires_obsolete_owned_configs_even_after_interruption() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(12), async {
        for interrupted in [false, true] {
            let dir = tempfile::tempdir()?;
            let prefix = if interrupted {
                "after-crash-ha"
            } else {
                "hot-ha"
            };
            let config =
                |domain: &str, key: &str| format!("{prefix}/{domain}/0x{MEMBER:016x}/{key}/config");
            let channel = || {
                channel_with_prefix(
                    MqttOptions::new("test-owner", "127.0.0.1", 1883),
                    "test-base",
                    prefix,
                    16,
                )
            };
            let mut bindings = Bindings::default();
            let mut manager = Management::new(dir.path(), Default::default(), None)?;
            manager.zones.start(MEMBER - 1, &bindings)?;
            let (out, worker, _) = channel()?;
            let running = tokio::spawn(worker.run());
            manager.commit_interview(&report(false), &mut bindings, &out)?;
            bindings.advertise_member(&out, MEMBER).await?;
            let binding = &bindings.devices[&(MEMBER, 1)];
            out.discover(
                prefix,
                &binding.discovery((MEMBER, 1)),
                Feature::TextSensor("external"),
            )?;
            out.close();
            running.await??;
            let original = retained(prefix).await?;
            assert!(original.contains(&config("binary_sensor", "occupancy")));
            assert!(original.contains(&config("binary_sensor", "binary_output")));

            let mut partial = report(false);
            partial.entries.retain(|e| !matches!(e.observation, Observation::SimpleDescriptor { .. }));
            let (out, worker, _) = channel()?;
            let running = tokio::spawn(worker.run());
            manager.commit_interview(&partial, &mut bindings, &out)?;
            manager.discovery.after_interview(MEMBER, &bindings, &out).await?;
            manager.discovery.startup(&bindings, &out).await?;
            out.close();
            running.await??;
            assert_eq!(retained(prefix).await?, original);

            let (out, worker, _) = channel()?;
            let running = tokio::spawn(worker.run());
            manager.commit_interview(&report(true), &mut bindings, &out)?;
            if interrupted {
                // Power loss between the durable member replacement and MQTT publication.
                drop(manager);
                manager = Management::new(dir.path(), Default::default(), None)?;
                bindings = Bindings::default();
                bindings.add(
                    &InterviewReport::open(&manager.dir.join(format!("{MEMBER:016x}.member")))?,
                    &Default::default(),
                )?;
                manager.discovery.startup(&bindings, &out).await?;
            } else {
                manager
                    .discovery
                    .after_interview(MEMBER, &bindings, &out)
                    .await?;
            }
            bindings.advertise_member(&out, MEMBER).await?;
            out.close();
            running.await??;
            assert_eq!(
                retained(prefix).await?,
                BTreeSet::from([
                    config("switch", "binary_output"),
                    config("sensor", "external"),
                    config("sensor", "linkquality"),
                ])
            );
        }
        Ok(())
    })
    .await?
}

#[test]
fn naming_preserves_ha_identity_across_label_route_and_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let member = report(true);
    let ieee = format!("0x{MEMBER:016x}");
    for imported in [false, true] {
        let old = if imported { "Old sensor" } else { &ieee };
        let mut names = BTreeMap::from([(MEMBER + 1, "Reserved route".into())]);
        if imported { names.insert(MEMBER, old.to_owned()); }
        let mut original = Bindings::default();
        original.add_fresh(&member, &names)?;
        let before = original.discovery_snapshot("zb", "ha", None)?;
        assert!(!before.is_empty());

        let labeled = original.named_candidate(MEMBER, "Van sensor", false, &names)?;
        let after_label = labeled.discovery_snapshot("zb", "ha", None)?;
        assert_eq!(before.keys().collect::<Vec<_>>(), after_label.keys().collect::<Vec<_>>());
        for (topic, original_config) in &before {
            let mut expected = original_config.clone();
            expected["device"]["name"] = json!("Van sensor");
            assert_eq!(after_label[topic], expected);
        }
        assert_eq!(labeled.name_info(MEMBER)["route"], old);
        for invalid in ["Reserved route", "bridge", "a/ep1", "bad+", "0x0200000000000003"] {
            assert!(labeled.named_candidate(MEMBER, invalid, true, &names).is_err());
        }
        let moved = labeled.named_candidate(MEMBER, "van-sensor", true, &names)?;
        assert_eq!(moved.peer("van-sensor")?, MEMBER);
        assert!(moved.peer(old).is_err() || old == ieee); // IEEE is always an alias.
        assert_eq!(moved.peer(&ieee)?, MEMBER);
        assert_eq!(moved.name_info(MEMBER), json!({"ieee":ieee,"display_name":"Van sensor","route":"van-sensor"}));
        let after_move = moved.discovery_snapshot("zb", "ha", None)?;
        for (topic, config) in &after_move {
            assert_eq!(config["unique_id"], before[topic]["unique_id"]);
            assert_eq!(config["device"]["identifiers"], before[topic]["device"]["identifiers"]);
            assert_eq!(config["device"]["name"], "Van sensor");
        }
        assert!(after_move.values().any(|c| c["command_topic"].as_str().is_some_and(|t| t.starts_with("zb/van-sensor/set"))));
        super::super::naming::commit(moved.naming.prepare(dir.path())?, dir.path())?;
        let mut restored = Bindings::default();
        restored.load_naming(dir.path())?;
        restored.add_fresh(&member, &names)?;
        assert_eq!(restored.discovery_snapshot("zb", "ha", None)?, after_move);
        let route_only = original.named_candidate(MEMBER,"moved",true,&names)?;
        assert_eq!(route_only.name_info(MEMBER)["display_name"],old);
    }
    Ok(())
}
