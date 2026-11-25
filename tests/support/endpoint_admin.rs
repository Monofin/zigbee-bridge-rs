//! Wire-level checks for endpoint administration through the versioned MQTT route.
use super::*;

pub(super) fn command(args: &[u8]) -> Result<Vec<Vec<u8>>> {
    ensure!(args.get(1..3) == Some(&short(1).to_le_bytes()) && args.get(8) == Some(&endpoint(1)));
    let frame = Frame::decode(&args[16..])?;
    ensure!(
        frame.header.direction == Direction::ClientToServer
            && !frame.header.disable_default_response
    );
    let seq = frame.header.sequence;
    let cluster = u16::from_le_bytes([args[5], args[6]]);
    let body = match (cluster, frame.header.command_id, frame.payload) {
        (4, 2, [0]) => vec![0x19, seq, 2, 8, 1, 3, 0],
        (5, 4, [3, 0, 2]) => vec![0x19, seq, 4, 0, 3, 0, 2],
        (9, 2, []) => vec![0x19, seq, 1, 0, 7, 1, 0, 2, 0, 0, 0],
        other => bail!("unexpected endpoint command: {other:?}"),
    };
    let local = if cluster == 9 { 2 } else { 1 };
    ensure!(args[7] == local);
    let mut correct = incoming(1, cluster, &body);
    let wrong_endpoint = correct.clone();
    correct[11] = local;
    let mut replies = vec![incoming(2, cluster, &body)];
    if local != 1 {
        replies.push(wrong_endpoint);
    }
    replies.push(correct);
    Ok(replies)
}

pub(super) async fn verify(
    client: &AsyncClient,
    net: &mut rumqttc::EventLoop,
    base: &str,
) -> Result<()> {
    for (command, expected) in [
        (
            json!({"op":"group_list"}),
            json!({"capacity":8,"groups":[3]}),
        ),
        (
            json!({"op":"scene_store","group":3,"scene":2}),
            json!({"group":3,"scene":2}),
        ),
        (
            json!({"op":"alarm_pop"}),
            json!({"cluster":1,"code":7,"zigbee_time":2}),
        ),
    ] {
        let tag = command["op"].clone();
        client.publish(format!("{base}/bridge/v1/request/device/command"),QoS::AtLeastOnce,false,
            serde_json::to_vec(&json!({"id":"lamp-1","endpoint":endpoint(1),"command":command,"transaction":tag}))?).await?;
        loop {
            if let Event::Incoming(Incoming::Publish(p)) = net.poll().await?
                && p.topic == format!("{base}/bridge/v1/response/device/command")
            {
                ensure!(
                    serde_json::from_slice::<Value>(&p.payload)?
                        == json!({"transaction":tag,"data":expected,"status":"ok"})
                );
                break;
            }
        }
    }
    Ok(())
}
