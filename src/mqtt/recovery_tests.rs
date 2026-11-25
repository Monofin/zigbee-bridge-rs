use super::*;
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};

#[test]
fn legacy_checkpoint_and_live_activity_replay_on_independent_topics() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (p, mut worker, _) = channel(MqttOptions::new("feedback", "localhost", 1883), "zb", 1)?;
    let _commands = worker.enable_reconnect(dir.path())?;
    let id = (7, 4);
    let store = p.0.pending.lock().unwrap().store.clone().unwrap();
    store.stage(id, br#"{"state":"ON","last_seen":100,"linkquality":42}"#);
    let drain = || {
        let mut queue = p.0.pending.lock().unwrap();
        std::iter::from_fn(|| queue.take())
            .map(|item| (item.device, serde_json::from_slice::<Value>(&item.payload).unwrap()))
            .collect::<BTreeMap<_, _>>()
    };
    p.set_routes(BTreeMap::from([("room/light".into(), id)]))?;
    let initial = drain();
    assert_eq!(initial["room/light"], json!({"state":"ON"}));
    assert_eq!(initial[&diagnostic_route("room/light")], json!({"last_seen":100,"linkquality":42}));
    p.state("room/light", "last_seen", json!(101))?;
    assert_eq!(drain(), BTreeMap::from([(diagnostic_route("room/light"), json!({"last_seen":101,"linkquality":42}))]));
    p.state("room/light", "state", json!("OFF"))?;
    p.state("room/light", "linkquality", json!(99))?;
    store.checkpoint()?;
    p.set_routes(BTreeMap::from([("new".into(), id)]))?;
    let renamed = drain();
    assert_eq!(renamed.len(), 2);
    assert_eq!(renamed["new"], json!({"state":"OFF"}));
    assert_eq!(renamed[&diagnostic_route("new")], json!({"last_seen":101,"linkquality":99}));
    p.state("new", "last_seen", json!(102))?;
    p.membership_changed(id.0, false)?;
    assert!(drain().is_empty());
    Ok(())
}

#[test]
fn pressure_preserves_latest_state_across_checkpoint_and_rename() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let routes: subscriptions::Routes = (1..=12).map(|n| (format!("light/{n}"), (n, 1))).collect();
    let (p, mut worker, _) = channel(MqttOptions::new("audit", "localhost", 1883), "room", 2)?;
    worker.enable_reconnect(dir.path())?;
    p.set_routes(routes.clone())?;
    for i in 1..=12 {
        p.state(&format!("light/{i}"), "state", json!("ON"))?;
    }
    p.state("light/1", "state", json!("OFF"))?;
    let mut q = p.0.pending.lock().unwrap();
    assert_eq!(q.dirty.len(), 2);
    assert_eq!(q.waiting_names.len(), 10);
    let mut seen = BTreeMap::new();
    while let Some(item) = q.take() {
        seen.insert(item.device, serde_json::from_slice::<Value>(&item.payload)?);
        assert!(q.dirty.len() <= 2);
    }
    assert_eq!(seen.len(), 12);
    assert_eq!(seen["light/1"], json!({"state":"OFF"}));
    drop(q);
    let mut renamed = routes;
    renamed.remove("light/1");
    renamed.insert("new/name".into(), (1, 1));
    p.set_routes(renamed.clone())?;
    {
        let q = p.0.pending.lock().unwrap();
        assert_eq!(q.snapshots["new/name"]["state"], "OFF");
        let store = q.store.as_ref().unwrap();
        assert_eq!(store.checkpoint()?, 12);
        assert_eq!(store.checkpoint()?, 0);
    }
    drop(worker);
    drop(p);
    let (p, mut worker, _) = channel(MqttOptions::new("restored", "localhost", 1883), "room", 2)?;
    worker.enable_reconnect(dir.path())?;
    p.set_routes(renamed)?;
    let q = p.0.pending.lock().unwrap();
    assert_eq!(q.snapshots["new/name"]["state"], "OFF");
    assert!(!q.snapshots.contains_key("light/1"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn resilient_broker() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(35), outage()).await?
}

async fn outage() -> Result<()> {
    use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
    let dir = tempfile::tempdir()?;
    let base = "resilient";
    let (observer, mut net) =
        AsyncClient::new(MqttOptions::new("resilient-check", "127.0.0.1", 1883), 16);
    observer
        .subscribe("resilient/bridge/state", QoS::AtLeastOnce)
        .await?;
    observer
        .subscribe("resilient/floor/light", QoS::AtLeastOnce)
        .await?;
    observer
        .subscribe("resilient/bank/+", QoS::AtLeastOnce)
        .await?;
    let mut acks = 0;
    while acks < 3 {
        if matches!(net.poll().await?, Event::Incoming(Incoming::SubAck(_))) {
            acks += 1;
        }
    }
    // Inventory messages share the base with the device commands.
    let mut opts = MqttOptions::new("inventory", "127.0.0.1", 1883);
    opts.set_max_packet_size(MAX_INBOUND_PACKET, MAX_OUTBOUND_PACKET);
    let (seed, mut seed_net) = AsyncClient::new(opts, 4);
    for (suffix, size) in [
        ("bridge/info", 40573),
        ("bridge/devices", 521461),
        ("bridge/definitions", 468996),
    ] {
        seed.publish(
            format!("{base}/{suffix}"),
            QoS::AtLeastOnce,
            true,
            vec![b'x'; size],
        )
        .await?;
        while !matches!(seed_net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
    }
    seed.publish(
        "resilient/floor/light/set",
        QoS::AtLeastOnce,
        true,
        br#"{"state":"OFF"}"#.as_slice(),
    )
    .await?;
    while !matches!(seed_net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
    let proxy = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = proxy.local_addr()?.port();
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::AsyncWriteExt;
    let intercept = Arc::new(AtomicBool::new(false));
    let armed = intercept.clone();
    let (withheld, proof) = oneshot::channel();
    let (resume, again) = oneshot::channel();
    let (retry, disconnected) = oneshot::channel();
    let network = tokio::spawn(async move {
        let (mut client, _) = proxy.accept().await?;
        let mut broker = TcpStream::connect(("127.0.0.1", 1883)).await?;
        {
            let (mut cr, mut cw) = client.split();
            let (mut br, mut bw) = broker.split();
            let downstream = async {
                loop {
                    let (packet, offset) = read_packet(&mut br).await?;
                    let first = packet[0];
                    assert!(
                        first >> 4 != 3 || packet.len() - offset < 40573,
                        "an inventory PUBLISH reached the managed command connection"
                    );
                    if first >> 4 == 4 && armed.load(Ordering::Acquire) {
                        let _ = withheld.send(());
                        break;
                    }
                    cw.write_all(&packet).await?;
                }
                Ok::<_, std::io::Error>(())
            };
            tokio::select! {
                result = downstream => result?,
                result = tokio::io::copy(&mut cr,&mut bw) => {result?;}
            }
        }
        drop(client);
        drop(broker);
        tokio::pin!(again);
        let mut retry = Some(retry);
        loop {
            tokio::select! {
                result = &mut again => { result?; break; },
                connection = proxy.accept() => {
                    drop(connection?);
                    if let Some(first) = retry.take() { let _ = first.send(()); }
                }
            }
        }
        let (mut client, _) = proxy.accept().await?;
        let mut broker = TcpStream::connect(("127.0.0.1", 1883)).await?;
        tokio::io::copy_bidirectional(&mut client, &mut broker).await?;
        Ok::<_, anyhow::Error>(())
    });
    let (p, mut worker, _) = channel(
        super::MqttOptions::new("resilient-worker", "127.0.0.1", port),
        base,
        4,
    )?;
    let mut commands = worker.enable_reconnect(dir.path())?;
    let mut routes = BTreeMap::from([("floor/light".into(), (42, 1))]);
    for i in 0..128 {
        routes.insert(format!("bank/{i}"), (1000 + i, 1));
    }
    p.set_routes(routes.clone())?;
    let running = tokio::spawn(worker.run());
    async fn receive(net: &mut rumqttc::EventLoop, topic: &str, value: Value) -> Result<()> {
        loop {
            if let Event::Incoming(Incoming::Publish(msg)) = net.poll().await?
                && msg.topic == topic
                && serde_json::from_slice::<Value>(&msg.payload)? == value
            {
                return Ok(());
            }
        }
    }
    receive(
        &mut net,
        "resilient/bridge/state",
        json!({"state":"online"}),
    )
    .await?;
    assert!(commands.try_recv().is_err());
    observer
        .publish(
            "resilient/floor/light/get/state",
            QoS::AtLeastOnce,
            false,
            "",
        )
        .await?;
    while !matches!(net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
    let cmd = commands.recv().await.unwrap();
    assert_eq!(cmd.device, "floor/light");
    assert!(p.is_current(&cmd));
    let followup = ReceivedCommand::followup(Command {
        device: cmd.device.clone(), operation: Operation::Get, properties: Map::new(),
    }, cmd.epoch());
    assert!(p.is_current(&followup));
    p.membership_changed(42, true)?;
    assert!(!p.is_current(&cmd));
    assert!(!p.is_current(&followup));
    intercept.store(true, Ordering::Release);
    p.state("floor/light", "brightness", json!(60))?;
    proof.await?;

    receive(
        &mut net,
        "resilient/bridge/state",
        json!({"state":"offline"}),
    )
    .await?;
    // The broker's will can arrive before the old client notices its EOF.
    // A reconnect attempt proves the worker has released the old session.
    disconnected.await?;
    p.event("floor/light", "action", json!("toggle"))?;
    p.state("floor/light", "brightness", json!(80))?;
    let submit_start = std::time::Instant::now();
    for state in ["ON", "OFF"] {
        for i in 0..128 {
            p.state(&format!("bank/{i}"), "state", json!(state))?;
        }
    }
    eprintln!(
        "Queued 256 state updates during the MQTT outage in {:?}",
        submit_start.elapsed()
    );
    // Expire this gesture before the proxy lets the next MQTT session start.
    tokio::time::sleep(EVENT_TTL + Duration::from_millis(100)).await;
    resume.send(()).unwrap();
    let mut lights = std::collections::BTreeSet::new();
    let mut dimmer = false;
    while !dimmer || lights.len() < 128 {
        if let Event::Incoming(Incoming::Publish(msg)) = net.poll().await? {
            let state: Value = serde_json::from_slice(&msg.payload)?;
            if msg.topic.starts_with("resilient/bank/") {
                assert_eq!(state, json!({"state":"OFF"}));
                lights.insert(msg.topic);
            } else if msg.topic == "resilient/floor/light" {
                assert_eq!(state, json!({"brightness":80}));
                dimmer = true;
            }
        }
    }
    assert_eq!(p.overflow_counts().events, 1);
    assert!(commands.try_recv().is_err());
    // Read-only commands serve as barriers while the broker installs new filters.
    routes.remove("floor/light");
    routes.insert("renamed/light".into(), (42, 1));
    p.set_routes(routes)?;
    observer
        .publish(
            "resilient/floor/light/set",
            QoS::AtLeastOnce,
            false,
            br#"{"state":"TOGGLE"}"#.as_slice(),
        )
        .await?;
    loop {
        observer
            .publish(
                "resilient/renamed/light/get/state",
                QoS::AtLeastOnce,
                false,
                "",
            )
            .await?;
        while !matches!(net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
        if let Ok(Some(new)) =
            tokio::time::timeout(Duration::from_millis(100), commands.recv()).await
        {
            assert_eq!(new.device, "renamed/light");
            assert_eq!(new.operation, Operation::Get);
            assert!(p.is_current(&new));
            break;
        }
    }
    p.close();
    running.await??;
    network.await??;
    Ok(())
}

#[test]
fn a_confirmed_leave_survives_restart_and_name_reuse_without_reviving_old_state() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let routes = BTreeMap::from([("switch".into(), (1, 1)), ("switch/b".into(), (1, 2))]);
    {
        let (p, mut worker, _) = channel(MqttOptions::new("leave", "localhost", 1883), "zb", 4)?;
        let _commands = worker.enable_reconnect(dir.path())?;
        p.set_routes(routes.clone())?;
        p.state("switch", "state", json!("ON"))?;
        let store = p.0.pending.lock().unwrap().store.clone().unwrap();
        store.checkpoint()?;
        p.state("switch/b", "state", json!("ON"))?;
        p.membership_changed(1, false)?;
        assert!(store.get((1,1))?.is_none() && store.get((1,2))?.is_none());
    }
    {
        let (p, mut worker, _) = channel(MqttOptions::new("restore", "localhost", 1883), "zb", 4)?;
        p.set_routes(routes.clone())?;
        let _commands = worker.enable_reconnect(dir.path())?;
        assert!(p.departed_members().contains(&1));
        assert!(p.0.pending.lock().unwrap().route_epochs.is_empty());
        p.set_routes(BTreeMap::from([("switch".into(), (2, 1))]))?;
        p.state("switch", "state", json!("OFF"))?;
        p.membership_changed(1, true)?;
        p.set_routes(BTreeMap::from([("fresh-name".into(), (1,1)), ("switch".into(), (2,1))]))?;
        let pending = p.0.pending.lock().unwrap();
        assert!(pending.route_epochs.contains_key("fresh-name"));
        assert!(pending.snapshots.get("fresh-name").is_none());
        assert_eq!(pending.snapshots["switch"]["state"], "OFF");
    }
    let (p, mut worker, _) = channel(MqttOptions::new("again", "localhost", 1883), "zb", 4)?;
    let _commands = worker.enable_reconnect(dir.path())?;
    assert!(p.departed_members().is_empty());
    Ok(())
}

#[test]
fn prior_availability_checkpoint_migrates_once_and_cannot_undo_a_later_readmission() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Simulate a previous release with a shadow database and a separate presence snapshot.
    {
        let store = shadow::Shadow::open(dir.path())?;
        store.stage((1, 1), br#"{"state":"ON"}"#);
        store.checkpoint()?;
        drop(store);
        let db = redb::Database::open(dir.path().join("mqtt-state.redb"))?;
        let tx = db.begin_write()?;
        tx.delete_table(redb::TableDefinition::<u64, ()>::new("departed_v1"))?;
        tx.commit()?;
    }
    std::fs::write(dir.path().join("availability-seen.json"),
        br#"{"1":{"last_ms":null,"active":true,"departed":true},"2":{"last_ms":22,"active":false,"departed":false}}"#)?;
    {
        let store = shadow::Shadow::open(dir.path())?;
        assert_eq!(store.departed()?, std::collections::BTreeSet::from([1]));
        assert!(store.get((1, 1))?.is_none());
        store.membership(1, true)?;
    }
    // An abrupt stop can leave that JSON snapshot uncheckpointed after readmission.
    assert!(shadow::Shadow::open(dir.path())?.departed()?.is_empty());
    Ok(())
}

#[test]
fn attribute_responses_leave_room_for_state_and_gestures() -> Result<()> {
    let (p, _worker, _) = channel(
        MqttOptions::new("attributes", "localhost", 1883),
        "zb",
        1024,
    )?;
    p.state("meter", "linkquality", json!(89))?;
    let keys = [
        0x0702, 0x0b04, 0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0b05,
    ]
    .into_iter()
    .flat_map(|cluster| {
        zigbee_zcl::standard_attributes::attributes(cluster)
            .map(move |(id, _, _)| format!("zcl_01_{cluster:04x}_{id:04x}"))
    });
    let mut count = 0;
    for key in keys.take(482) {
        p.event("meter", &key, json!({"status":134}))?;
        count += 1;
    }
    assert_eq!(count, 482);
    p.event("meter", "action", json!("single"))?;
    let mut q = p.0.pending.lock().unwrap();
    assert_eq!(q.snapshots[&diagnostic_route("meter")].len(), 1);
    let last = std::iter::from_fn(|| q.take()).last().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&last.payload)?,
        json!({"action":"single"})
    );
    Ok(())
}

#[tokio::test]
async fn rejected_subscription_never_signals_ready() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (p, mut worker, _) = channel(MqttOptions::new("routes", "localhost", 1883), "zb", 2)?;
    let _commands = worker.enable_reconnect(dir.path())?;
    p.set_routes(BTreeMap::new())?;
    let (client, _network) = AsyncClient::new(worker.options.clone(), 4);
    let (ack, rx) = mpsc::channel(1);
    let (ready, confirmed) = oneshot::channel();
    ack.send(vec![
        SubscribeReasonCode::Failure,
        SubscribeReasonCode::Success(QoS::AtLeastOnce),
        SubscribeReasonCode::Success(QoS::AtLeastOnce),
    ])
    .await?;
    assert!(
        subscriptions::maintain(
            &worker,
            &client,
            &subscriptions::Subscriptions::new(rx),
            ready
        )
        .await
        .is_err()
    );
    assert!(confirmed.await.is_err());
    Ok(())
}

#[tokio::test]
async fn deletion_obligations_survive_closed_broker_and_restore() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let topic = "ha/sensor/old/config";
    let open = || -> Result<_> {
        let (pubs, mut worker, _) =
            channel(MqttOptions::new("restore", "127.0.0.1", 1883), "zb", 2)?;
        let _commands = worker.enable_reconnect(dir.path())?;
        Ok((pubs, worker))
    };
    {
        let (p, _worker) = open()?;
        p.retire_topic(topic.into()).await?;
    }
    {
        let (p, _worker) = open()?;
        assert_eq!(
            p.0.pending.lock().unwrap().take_config().unwrap().1.bytes,
            b""
        );
        p.enqueue_configs(&mut vec![(topic.into(), b"new".to_vec())])?;
    }
    {
        let (p, _worker) = open()?;
        assert!(p.0.pending.lock().unwrap().take_config().is_none());
    }
    std::fs::remove_file(dir.path().join("mqtt-state.redb"))?;
    assert!(open().is_err());
    Ok(())
}

#[tokio::test]
async fn offline_checkpoints_and_clean_shutdown_preserve_latest_readings() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Keep the broker connection silent so the transport cannot complete its handshake.
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let (p, mut worker, _) = channel(MqttOptions::new("silent", "127.0.0.1", port), "zb", 2)?;
    let _commands = worker.enable_reconnect(dir.path())?;
    p.set_routes(BTreeMap::from([("lamp".into(), (1, 1))]))?;
    let store = p.0.pending.lock().unwrap().store.as_ref().unwrap().clone();
    p.state("lamp", "brightness", json!(20))?;
    assert!(store.has_changes());
    let running = tokio::spawn(worker.run_with_checkpoint_interval(Duration::from_millis(100)));
    let (socket, _) = listener.accept().await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.has_changes() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    p.state("lamp", "brightness", json!(80))?;
    p.close();
    drop(socket);
    drop(listener);
    // The broker never completed CONNACK. The final checkpoint still runs.
    let _transport = tokio::time::timeout(Duration::from_secs(5), running).await??;
    drop(store);
    drop(p);
    let reopened = shadow::Shadow::open(dir.path())?;
    assert_eq!(
        serde_json::from_slice::<Value>(&reopened.get((1, 1))?.unwrap())?,
        json!({"brightness":80})
    );
    Ok(())
}

#[test]
fn abrupt_exit_recovers_checkpoint_and_immediate_deletions() -> Result<()> {
    const CHILD_DIR: &str = "ZIGBEE_MQTT_CRASH_TEST_DIR";
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        let store = shadow::Shadow::open(std::path::Path::new(&dir))?;
        for n in 1..=2 {
            store.stage((n, 1), b"old");
        }
        assert_eq!(store.checkpoint()?, 2);
        store.stage((1, 1), b"new");
        store.stage((2, 1), b"new");
        store.remove((2, 1))?;
        store.update_tombstones("scope", &[("discovery/topic", true)])?;
        // Bypass destructors and the normal shutdown checkpoint.
        std::process::exit(42);
    }
    let dir = tempfile::tempdir()?;
    let result = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "mqtt::recovery_tests::abrupt_exit_recovers_checkpoint_and_immediate_deletions",
        ])
        .env(CHILD_DIR, dir.path())
        .status()?;
    assert_eq!(result.code(), Some(42));
    let store = shadow::Shadow::open(dir.path())?;
    assert_eq!(store.get((1, 1))?.as_deref(), Some(b"old".as_slice()));
    assert_eq!(store.get((2, 1))?, None);
    assert_eq!(store.tombstones("scope")?, ["discovery/topic"]);
    // Returning to the checkpointed value does not require a new durable revision.
    store.stage((1, 1), b"other");
    store.stage((1, 1), b"old");
    assert_eq!(store.checkpoint()?, 0);
    Ok(())
}

// Preserve packet boundaries when withholding one delivery on the test proxy.
async fn read_packet(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<(Vec<u8>, usize)> {
    use tokio::io::AsyncReadExt;
    let mut wire = vec![stream.read_u8().await?];
    let mut length = 0usize;
    for shift in [0, 7, 14, 21] {
        let byte = stream.read_u8().await?;
        wire.push(byte);
        length |= usize::from(byte & 127) << shift;
        if byte & 128 == 0 {
            let offset = wire.len();
            wire.resize(offset + length, 0);
            stream.read_exact(&mut wire[offset..]).await?;
            return Ok((wire, offset));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid MQTT length",
    ))
}

/// Hold the broker's live copy of the first deletion in flight while the name
/// is reused. The test clock never decides which version wins.
#[tokio::test]
#[ignore = "requires isolated loopback MQTT broker on port 1883"]
async fn verified_deletion_and_name_reuse() -> Result<()> {
    use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
    use tokio::io::AsyncWriteExt;
    tokio::time::timeout(Duration::from_secs(15), async {
        let proxy = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = proxy.local_addr()?.port();
        let (held, reached) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        let topic = "delete-test/lamp/availability";
        let proxy_task = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await?;
            let mut broker = TcpStream::connect(("127.0.0.1", 1883)).await?;
            let (mut cr, mut cw) = socket.split();
            let (mut br, mut bw) = broker.split();
            let mut gate = Some((held, resume));
            let downstream = async {
                loop {
                    let (wire, offset) = match read_packet(&mut br).await {
                        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                        packet => packet?,
                    };
                    if wire[0] == 0x32 && wire.len() - offset == topic.len() + 5 &&
                        wire.get(offset+2..offset+2+topic.len()) == Some(topic.as_bytes()) &&
                        let Some((held, resume)) = gate.take() {
                        let _ = held.send(());
                        resume.await?;
                    }
                    cw.write_all(&wire).await?;
                }
                Ok::<(),anyhow::Error>(())
            };
            tokio::select! { v=downstream=>v, v=tokio::io::copy(&mut cr,&mut bw)=>v.map(|_|()).map_err(Into::into) }
        });
        let dir = tempfile::tempdir()?;
        let open = || -> Result<_> {
            let (p, mut worker, _) = channel(super::MqttOptions::new("deletion-test", "127.0.0.1", port), "delete-test", 8)?;
            let _commands = worker.enable_reconnect(dir.path())?;
            p.set_routes(BTreeMap::new())?;
            Ok((p, worker))
        };
        let names = ["retired-one", "retired-two", "retired-three"];
        let (seed,mut seed_net) = AsyncClient::new(MqttOptions::new("delete-seed","127.0.0.1",1883),4);
        for name in names.into_iter().chain(["lamp"]) {
            seed.publish(format!("delete-test/{name}/availability"),QoS::AtLeastOnce,true,ONLINE).await?;
            while !matches!(seed_net.poll().await?, Event::Incoming(Incoming::PubAck(_))) {}
        }
        let (p, worker) = open()?;
        let live = tokio::spawn(worker.run());
        p.retire_availability("lamp").await?;
        reached.await?;
        p.device_availability("lamp", true)?;
        release.send(()).unwrap();
        for name in names {
            p.retire_availability(name).await?;
        }
        p.close();
        live.await??;
        proxy_task.abort();
        let _ = proxy_task.await;
        {
            let q = p.0.pending.lock().unwrap();
            assert_eq!(q.desired_configs.len(), 1);
            assert_eq!(q.desired_configs[topic], ONLINE);
            assert!(q.store.as_ref().unwrap().tombstones(&p.retained_scope())?.is_empty());
        }
        drop(p);
        let (p, _worker) = open()?;
        assert!(p.0.pending.lock().unwrap().configs.is_empty());

        // Fresh QoS 0 readback on Mosquitto, fenced by a second SUBACK.
        let (reader, mut stream) = AsyncClient::new(MqttOptions::new("delete-reader","127.0.0.1",1883),4);
        let mut records=BTreeMap::new();
        for filter in ["delete-test/+/availability","delete-test/readback-barrier"] {
            reader.subscribe(filter,QoS::AtMostOnce).await?;
            loop {
                match stream.poll().await? {
                    Event::Incoming(Incoming::SubAck(_)) => break,
                    Event::Incoming(Incoming::Publish(m)) if m.retain => {records.insert(m.topic,m.payload.to_vec());}
                    _ => {}
                }
            }
        }
        assert_eq!(records, BTreeMap::from([(topic.to_owned(),ONLINE.to_vec())]));
        Ok::<(),anyhow::Error>(())
    }).await?
}

#[tokio::test]
#[ignore = "requires isolated loopback Mosquitto on port 1883"]
async fn home_assistant_birth_replays_only_current_state_after_no_match() -> Result<()> {
    use rumqttc::{AsyncClient as Witness, Event, Incoming, MqttOptions as V3Options, QoS};

    tokio::time::timeout(Duration::from_secs(12), async {
        let dir = tempfile::tempdir()?;
        let (publisher, mut worker, _) =
            channel(MqttOptions::new("birth-worker", "127.0.0.1", 1883), "birth-test", 8)?;
        let _commands = worker.enable_reconnect(dir.path())?;
        publisher.set_routes(BTreeMap::from([("room".into(), (1, 1))]))?;

        let (witness, mut events) = Witness::new(V3Options::new("birth-ha", "127.0.0.1", 1883), 8);
        witness.subscribe("birth-test/bridge/state", QoS::AtLeastOnce).await?;
        while !matches!(events.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
        let running = tokio::spawn(worker.run());
        while !matches!(events.poll().await?, Event::Incoming(Incoming::Publish(p)) if p.payload == ONLINE) {}

        publisher.state("room", "temperature", json!(19))?;
        // Wait for actual receipts so the first publish cannot coalesce with the next.
        let wait_for_count = |count| {
            let publisher = publisher.clone();
            async move {
                while publisher.no_matching_subscribers() < count {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        };
        wait_for_count(1).await;
        publisher.state("room", "humidity", json!(63))?;
        publisher.event("room", "action", json!("single"))?;
        wait_for_count(4).await; // merged state, event JSON and raw action
        publisher.state("room", "temperature", json!(21.5))?;
        wait_for_count(5).await;

        witness.subscribe("birth-test/#", QoS::AtLeastOnce).await?;
        while !matches!(events.poll().await?, Event::Incoming(Incoming::SubAck(_))) {}
        witness.publish(HA_STATUS_TOPIC, QoS::AtLeastOnce, false, "online").await?;
        let mut received = Vec::new();
        loop {
            if let Event::Incoming(Incoming::Publish(p)) = events.poll().await? {
                if p.topic == "birth-test/bridge/state" {
                    if p.payload == OFFLINE { break; }
                    continue;
                }
                assert!(!p.retain);
                received.push((p.topic, serde_json::from_slice::<Value>(&p.payload)?));
                // Closing flushes everything admitted before this point.
                publisher.close();
            }
        }
        running.await??;
        assert_eq!(received, [("birth-test/room".into(), json!({"temperature":21.5,"humidity":63}))]);
        assert_eq!(publisher.no_matching_subscribers(), 5);
        Ok(())
    }).await?
}

#[tokio::test]
#[ignore = "requires loopback broker ACL granting reads and bridge writes while denying the device tombstone"]
async fn refused_tombstone_remains_durable_and_session_drains() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (p, mut worker, _) = channel(MqttOptions::new("acl-check", "127.0.0.1", 1883), "deny-test", 4)?;
    let _commands = worker.enable_reconnect(dir.path())?;
    p.set_routes(BTreeMap::new())?;
    p.retire_availability("old").await?;
    p.close();
    tokio::time::timeout(Duration::from_secs(8), worker.run()).await??;
    let q = p.0.pending.lock().unwrap();
    let store = q.store.as_ref().unwrap();
    assert_eq!(store.tombstones(&p.retained_scope())?, ["deny-test/old/availability"]);
    Ok(())
}
