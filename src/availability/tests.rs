use super::*;
use serde_json::json;

fn enabled() -> Catalogue {
    Catalogue::import(Some(
        &json!({"enabled":true,"active":{"timeout":1,"max_jitter":0},"passive":{"timeout":3}}),
    ))
    .unwrap()
}

#[test]
fn sleeping_device_expires_without_a_probe_and_recovers_on_activity() -> Result<()> {
    let mut cfg = enabled();
    cfg.import_member(1, "EndDevice", Some("Battery"), Some(1000), None)?;
    let start = Instant::now();
    let mut tracker = Tracker::new(&cfg, start)?;
    tracker.register(
        &cfg,
        Device {
            ieee: 1,
            name: "sleepy",
            endpoint: 1,
            profile: 0x104,
            receiver_on: None, mains: Some(true),
        },
        start,
        2000,
    );
    assert!(tracker.peers[&1].online);
    tracker.dirty.clear();
    assert!(tracker.due(start + Duration::from_secs(178), |_| true)?.is_none());
    assert!(tracker.peers[&1].online);
    assert!(tracker.due(start + Duration::from_secs(179), |_| true)?.is_none());
    assert!(!tracker.peers[&1].online);
    let now = start + Duration::from_secs(200);
    tracker.seen(1, now, 202000);
    assert!(tracker.peers[&1].online);
    tracker.dirty.clear();
    tracker.seen(1, now, 202000);
    assert!(tracker.dirty.is_empty());
    assert!(tracker.active.is_empty());
    Ok(())
}

#[test]
fn offline_after_two_failed_active_reads_then_report_resumes_probing() -> Result<()> {
    let mut cfg = enabled();
    cfg.import_member(
        1,
        "Router",
        None,
        Some(1000),
        Some(&json!({"pause_on_backoff_gt":1})),
    )?;
    let start = Instant::now();
    let mut t = Tracker::new(&cfg, start)?;
    t.register(
        &cfg,
        Device {
            ieee: 1,
            name: "lamp",
            endpoint: 3,
            profile: 0x104,
            receiver_on: None, mains: Some(false),
        },
        start,
        1000,
    );
    let first = start + Duration::from_secs(60);
    let p = t.due(first, |_| true)?.unwrap();
    assert_eq!((p.ieee, p.endpoint, p.profile), (1, 3, 0x104));
    let target = Destination::fixture(1, 0x1234);
    t.submitted(target.clone(), first);
    let expiry = first + PROBE_LIMIT;
    assert!(t.due(expiry, |_| true)?.is_none());
    assert!(t.peers[&1].online);
    let second = expiry + Duration::from_secs(3);
    assert!(t.due(second, |_| true)?.is_some());
    t.submitted(target, second);
    let timeout = second + PROBE_LIMIT;
    assert!(t.due(timeout, |_| true)?.is_none());
    assert!(!t.peers[&1].online);
    assert!(t.active.is_empty());
    t.seen(1, timeout, 72000);
    assert!(t.peers[&1].online);
    assert!(t.due(timeout + Duration::from_secs(61), |_| true)?.is_some());
    Ok(())
}

#[test]
fn a_probe_that_never_reached_the_adapter_cannot_mark_a_peer_offline() -> Result<()> {
    let mut cfg = enabled();
    cfg.import_member(1, "Router", None, Some(1000), None)?;
    let start = Instant::now();
    let mut t = Tracker::new(&cfg, start)?;
    t.register(&cfg, Device {
        ieee: 1, name: "lamp", endpoint: 1, profile: 0x104, receiver_on: None, mains: Some(true),
    }, start, 1000);
    t.dirty.clear();

    let mut due = t.deadline().unwrap();
    for _ in 0..3 {
        assert_eq!(t.due(due, |_| true)?.unwrap().ieee, 1);
        t.not_submitted(1, due);
        assert!(t.peers[&1].online && t.peers[&1].retry.is_none());
        assert!(t.dirty.is_empty());
        let next = t.deadline().unwrap();
        assert!(next > due);
        due = next;
    }
    // A missing submission receipt expires as an unsent probe too.
    assert_eq!(t.due(due, |_| true)?.unwrap().ieee, 1);
    assert!(t.due(due + PROBE_LIMIT, |_| true)?.is_none());
    assert!(t.peers[&1].online && t.peers[&1].retry.is_none());
    Ok(())
}

#[test]
fn unanswered_probes_across_assignments_never_combine_to_mark_a_device_offline() -> Result<()> {
    let mut cfg = enabled();
    cfg.import_member(1, "Router", None, Some(1000), None)?;
    let start = Instant::now();
    let mut t = Tracker::new(&cfg, start)?;
    t.register(&cfg, Device {
        ieee: 1, name: "lamp", endpoint: 1, profile: 0x104, receiver_on: None, mains: Some(true),
    }, start, 1000);

    let old = Destination::fixture(1, 0x1234);
    let new = Destination::fixture(1, 0x1234);
    let first = t.deadline().unwrap();
    assert!(t.due(first, |_| true)?.is_some());
    t.submitted(old.clone(), first);
    // A leave and readmission can reuse the very same short address.
    assert!(t.due(first + PROBE_LIMIT, |to| new.same_assignment(to))?.is_none());
    assert!(t.peers[&1].retry.is_none());

    for assignment in [old, new] {
        let when = t.deadline().unwrap();
        assert!(t.due(when, |_| true)?.is_some());
        t.submitted(assignment, when);
        assert!(t.due(when + PROBE_LIMIT, |_| true)?.is_none());
        assert!(t.peers[&1].online);
    }
    Ok(())
}

#[test]
fn current_address_probes_are_serial_and_leave_rejects_late_reports_across_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut cfg = enabled();
    cfg.directory = Some(dir.path().into());
    let now = Instant::now();
    let mut t = Tracker::new(&cfg, now)?;
    for i in 1..=2 {
        t.register(
            &cfg,
            Device {
                ieee: i,
                name: "router",
                endpoint: 1,
                profile: 0x104,
                receiver_on: None, mains: Some(true),
            },
            now,
            100,
        );
    }
    assert_eq!(t.due(now, |_| true)?.unwrap().ieee, 1);
    assert!(t.due(now, |_| true)?.is_none());
    t.left(1, now);
    t.seen(1, now, 100);
    assert!(!t.peers[&1].online);
    assert!(t.due(now, |_| true)?.is_none());
    assert_eq!(t.due(now + PROBE_SPACING, |_| true)?.unwrap().ieee, 2);
    drop(t);
    let mut next = Tracker::new(&cfg, now)?;
    next.register(
        &cfg,
        Device {
            ieee: 1,
            name: "renamed",
            endpoint: 1,
            profile: 0x104,
            receiver_on: None, mains: None,
        },
        now,
        110,
    );
    assert!(!next.peers[&1].online);
    assert!(next.peers[&1].settings.active);
    assert!(next.due(now, |_| true)?.is_none());
    next.admit(1, now);
    assert_eq!(next.due(now, |_| true)?.unwrap().ieee, 1);
    next.seen(1, now, 110);
    drop(next);
    let mut restarted = Tracker::new(&cfg, now)?;
    restarted.register(
        &cfg,
        Device {
            ieee: 1,
            name: "renamed",
            endpoint: 1,
            profile: 0x104,
            receiver_on: None, mains: None,
        },
        now,
        120,
    );
    assert!(restarted.peers[&1].online);
    Ok(())
}

#[test]
fn policy_overrides_enabled_state_role_and_timeout() -> Result<()> {
    let mut cfg = Catalogue::import(None)?;
    cfg.import_member(
        1,
        "Router",
        Some("Battery"),
        None,
        Some(&json!({"timeout":2})),
    )?;
    cfg.import_member(
        2,
        "EndDevice",
        Some("Mains (single phase)"),
        None,
        Some(&json!(true)),
    )?;
    cfg.import_member(3, "Router", None, None, Some(&json!(false)))?;
    assert_eq!(
        cfg.settings(1, None, true).map(|p| (p.active, p.timeout)),
        Some((true, Duration::from_secs(120)))
    );
    assert!(
        !cfg.settings(1, Some(false), true).unwrap().active,
        "the reported receiver mode takes precedence over the imported role"
    );
    assert!(cfg.settings(2, None, false).unwrap().active);
    assert!(cfg.settings(3, None, true).is_none());
    assert!(cfg.settings(4, None, true).is_none());
    assert!(
        cfg.import_member(5, "Router", None, None, Some(&json!({"timeout":-1})))
            .is_err()
    );
    Ok(())
}

mod broker {
    use super::*;
    use crate::mqtt;
    use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
    use serde_json::json;

    #[tokio::test]
    #[ignore = "requires isolated loopback Mosquitto on port 1883"]
    async fn long_outage_reseeds_retained_devices_before_bridge_online() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let base = "presence-bootstrap";
            let (old, worker, _) =
                mqtt::channel(mqtt::MqttOptions::new("old-owner", "127.0.0.1", 1883), base, 4)?;
            for name in ["one", "two", "three"] {
                old.device_availability(name, true)?;
            }
            old.close();
            worker.run().await?;

            let (witness, mut wire) =
                AsyncClient::new(MqttOptions::new("boot-witness", "127.0.0.1", 1883), 8);
            witness
                .subscribe(format!("{base}/#"), QoS::AtLeastOnce)
                .await?;
            while !matches!(wire.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
            let (out, mut worker, _) =
                mqtt::channel(mqtt::MqttOptions::new("next-owner", "127.0.0.1", 1883), base, 1)?;
            worker.wait_for_device_snapshot();
            let running = tokio::spawn(worker.run());
            let cfg = Catalogue::import(Some(&json!(true)))?;
            let now = Instant::now();
            let mut tracker = Tracker::new(&cfg, now)?;
            for (i, name) in ["one", "two", "three"].into_iter().enumerate() {
                tracker.register(
                    &cfg,
                    Device {
                        ieee: i as u64 + 1,
                        name,
                        endpoint: 1,
                        profile: 0x104,
                        receiver_on: None, mains: Some(false),
                    },
                    now,
                    500000,
                );
            }

            let mut stale = BTreeSet::new();
            loop {
                if let Event::Incoming(Incoming::Publish(p)) = wire.poll().await? {
                    let state = serde_json::from_slice::<serde_json::Value>(&p.payload)?;
                    if p.retain && p.topic.ends_with("/availability") && state["state"] == "online"
                    {
                        stale.insert(p.topic);
                    } else if !p.retain && p.topic == format!("{base}/bridge/state") {
                        assert_eq!(state["state"], "offline");
                        break;
                    }
                }
            }
            assert_eq!(stale.len(), 3);
            while !tracker.all_states_queued() {
                tracker.publish_due(&out, Instant::now())?;
                if !tracker.all_states_queued() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            out.device_snapshot_queued();
            let mut restored = BTreeSet::new();
            loop {
                if let Event::Incoming(Incoming::Publish(p)) = wire.poll().await? {
                    let state = serde_json::from_slice::<serde_json::Value>(&p.payload)?;
                    if p.topic.ends_with("/availability") {
                        assert_eq!(state["state"], "offline");
                        restored.insert(p.topic);
                    } else if p.topic == format!("{base}/bridge/state") {
                        assert_eq!(state["state"], "online");
                        break;
                    }
                }
            }
            assert_eq!(restored, stale);
            out.close();
            running.await??;
            Ok(())
        })
        .await?
    }

    #[tokio::test]
    #[ignore = "requires isolated loopback Mosquitto on port 1883"]
    async fn retires_only_owned_topics_across_restored_sessions() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir()?;
            let mut cfg = Catalogue::import(Some(&json!(true)))?;
            cfg.directory = Some(dir.path().to_owned());
            let prefix = "presence-reconcile";
            for (index, names) in [
                vec![(1, "old/alias"), (2, "removed"), (3, "reused")],
                vec![(1, "new/alias"), (4, "reused")],
            ]
            .into_iter()
            .enumerate()
            {
                let now = Instant::now();
                let mut t = Tracker::new(&cfg, now)?;
                let (out, worker, _) =
                    mqtt::channel(mqtt::MqttOptions::new("owner", "127.0.0.1", 1883), prefix, 8)?;
                let running = tokio::spawn(worker.run());
                for (ieee, name) in names {
                    t.register(
                        &cfg,
                        Device {
                            ieee,
                            name,
                            endpoint: 1,
                            profile: 0x104,
                            receiver_on: None, mains: None,
                        },
                        now,
                        100,
                    );
                    t.seen(ieee, now, 100);
                }
                if index == 0 {
                    out.device_availability("unrelated", true)?;
                }
                t.reconcile_topics(&out).await?;
                t.publish_due(&out, now)?;
                out.close();
                running.await??;
            }
            let (late, mut wire) =
                AsyncClient::new(MqttOptions::new("witness", "127.0.0.1", 1883), 8);
            late.subscribe(format!("{prefix}/#"), QoS::AtLeastOnce)
                .await?;
            while !matches!(wire.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
            let fence = format!("{prefix}/fence");
            late.publish(&fence, QoS::AtLeastOnce, false, "done")
                .await?;
            let mut retained = BTreeSet::new();
            loop {
                if let Event::Incoming(Incoming::Publish(p)) = wire.poll().await? {
                    if p.topic == fence {
                        break;
                    }
                    if p.topic.ends_with("/availability") {
                        assert!(p.retain);
                        retained.insert(p.topic);
                    }
                }
            }
            assert_eq!(
                retained,
                ["new/alias", "reused", "unrelated"]
                    .map(|name| format!("{prefix}/{name}/availability"))
                    .into()
            );
            Ok(())
        })
        .await?
    }
}

#[test]
fn startup_walk_is_paced_skips_sleepers_and_honours_device_opt_out() -> Result<()> {
    let mut cfg = Catalogue::default(); // No periodic monitoring.
    for (id, role, power, policy) in [
        (1, "Router", None, None),
        (2, "EndDevice", Some("Battery"), None),
        (3, "EndDevice", Some("Mains (single phase)"), None),
        (4, "Router", None, Some(json!(false))),
        (5, "EndDevice", Some("Battery"), None),
    ] {
        cfg.import_member(id, role, power, Some(10_000), policy.as_ref())?;
    }
    let start = Instant::now();
    let mut tracker = Tracker::new(&cfg, start)?;
    for (ieee, receiver_on) in [(1,None), (2,Some(false)), (3,None), (4,None), (5,Some(true))] {
        tracker.register(&cfg, Device { ieee, name: "fixture", endpoint: 1, profile: 0x104,
            mains: None, receiver_on }, start, 10_000);
    }
    assert_eq!(tracker.due(start, |_| true)?.unwrap().ieee, 1);
    tracker.seen(1, start, 10_000);
    assert!(tracker.due(start, |_| true)?.is_none());
    let next = start + PROBE_SPACING;
    assert_eq!(tracker.due(next, |_| true)?.unwrap().ieee, 5);
    tracker.submitted(Destination::fixture(5, 0x1234), next);
    assert!(tracker.due(next + PROBE_LIMIT, |_| true)?.is_none());
    assert!(tracker.deadline().is_none());
    assert!(tracker.dirty.is_empty());
    Ok(())
}

#[test]
fn a_fresh_monitored_router_is_walked_and_a_leave_cancels_discovery() -> Result<()> {
    let mut cfg = enabled();
    cfg.import_member(1, "Router", None, Some(10_000), None)?;
    let now = Instant::now();
    let mut t = Tracker::new(&cfg, now)?;
    t.register(&cfg, Device { ieee: 1, name: "lamp", endpoint: 1, profile: 0x104,
        mains: None, receiver_on: None }, now, 10_000);
    assert!(t.peers[&1].online);
    assert_eq!(t.due(now, |_| true)?.unwrap().ieee, 1);
    t.wait_for_address(1, now + PROBE_LIMIT * 2);
    assert!(t.due(now + PROBE_LIMIT, |_| true)?.is_none() && t.awaiting_address(1));
    t.left(1, now + PROBE_LIMIT);
    assert!(!t.awaiting_address(1));
    assert!(t.due(now + PROBE_LIMIT * 2, |_| true)?.is_none());
    Ok(())
}
