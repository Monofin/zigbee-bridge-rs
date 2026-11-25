//! Retained discovery rollback rehearsal using an isolated loopback broker.
//! Publications are additionally confined to a fresh test prefix.
use anyhow::{Context, Result, ensure};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, MqttOptions, QoS};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    time::Duration,
};
type Snapshot = BTreeMap<String, Vec<u8>>;

#[derive(Deserialize)]
struct Record {
    topic: String,
    payload: serde_json::Value,
}

fn snapshot(variable: &str) -> Result<Snapshot> {
    let mut entries = Snapshot::new();
    let file = File::open(std::env::var_os(variable).context("missing private snapshot path")?)?;
    for line in BufReader::new(file).lines() {
        let r: Record = serde_json::from_str(&line?)?;
        ensure!(
            r.topic.ends_with("/config")
                && !r.topic.contains(['#', '+', '\0'])
                && r.payload.is_object(),
            "invalid discovery record"
        );
        ensure!(
            entries
                .insert(r.topic, serde_json::to_vec(&r.payload)?)
                .is_none(),
            "duplicate discovery topic"
        );
    }
    ensure!(!entries.is_empty(), "empty input");
    Ok(entries)
}
fn connect(id: &str) -> (AsyncClient, EventLoop) {
    AsyncClient::new(MqttOptions::new(id, "127.0.0.1", 1883), 8)
}
async fn retain(
    client: &AsyncClient,
    events: &mut EventLoop,
    prefix: &str,
    batch: &Snapshot,
) -> Result<()> {
    let send = async {
        for (topic, data) in batch {
            client
                .publish(
                    format!("{prefix}/{topic}"),
                    QoS::AtLeastOnce,
                    true,
                    data.as_slice(),
                )
                .await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let acknowledge = async {
        let mut acknowledged = 0;
        while acknowledged < batch.len() {
            if let Event::Incoming(Incoming::PubAck(_)) = events.poll().await? {
                acknowledged += 1;
            }
        }
        Ok(())
    };
    tokio::try_join!(send, acknowledge)?;
    Ok(())
}
async fn seen(prefix: &str) -> Result<Snapshot> {
    let (c, mut events) = connect(&format!("{prefix}-read"));
    c.subscribe(format!("{prefix}/#"), QoS::AtMostOnce).await?;
    let mut rows = Snapshot::new();
    let marker = format!("{prefix}/fence");
    loop {
        match events.poll().await? {
            Event::Incoming(Incoming::SubAck(_)) => {
                c.publish(&marker, QoS::AtLeastOnce, false, "ok").await?
            }
            Event::Incoming(Incoming::Publish(p)) if p.topic == marker => return Ok(rows),
            Event::Incoming(Incoming::Publish(p)) => {
                ensure!(
                    p.retain,
                    "unexpected concurrent publication in private rehearsal"
                );
                rows.insert(p.topic[prefix.len() + 1..].to_owned(), p.payload.to_vec());
            }
            _ => (),
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated loopback Mosquitto and private old/candidate JSONL snapshots"]
async fn restore_the_previous_retained_payloads_and_remove_only_trial_additions() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(120),async{
        let original=snapshot("ZIGBEE_OLD_DISCOVERY")?;
        let candidate=snapshot("ZIGBEE_TRIAL_DISCOVERY")?;
        let scratch=tempfile::Builder::new().prefix("rollback-").tempdir()?;
        let root=scratch.path().file_name().unwrap().to_str().unwrap();
        let (client,mut events)=connect(root);
        let mut baseline=original.clone();
        baseline.insert("unrelated".into(),b"preserve me".to_vec());
        retain(&client,&mut events,root,&baseline).await?;
        let saved=seen(root).await?;
        ensure!(saved==baseline,"baseline retained capture differs");
        retain(&client,&mut events,root,&candidate).await?;
        let mut expected=baseline.clone();expected.extend(candidate.clone());
        ensure!(seen(root).await?==expected,"trial retained set differs");
        let mut removed=0;let mut restore=Snapshot::new();
        for topic in candidate.keys(){
            if let Some(bytes)=saved.get(topic){restore.insert(topic.clone(),bytes.clone());}
            else{restore.insert(topic.clone(),Vec::new());removed+=1;}
        }
        retain(&client,&mut events,root,&restore).await?;
        ensure!(seen(root).await?==saved,"rollback retained set differs");
        println!("Restored {} original configs, withdrew {removed} trial-only topics, preserved unrelated retained publication", original.len());
        Ok(())
    }).await?
}
