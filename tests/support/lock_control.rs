//! Synthetic keypad credential and a lock whose bolt stays locked on the first read.
use super::*;
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

fn test_pin() -> String {
    format!("{:06}", std::process::id() % 1_000_000)
}

pub fn enable(manager: &mut Management) -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))?;
    let file = dir.path().join(format!(
        "{:016x}-{}.pin",
        ieee(equipment::DEVICE),
        endpoint(equipment::DEVICE)
    ));
    let mut f = fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)?;
    writeln!(f, "{}", test_pin())?;
    manager.set_lock_credentials(dir.path())?;
    Ok(dir)
}

pub fn respond(args: &[u8]) -> Result<Vec<Vec<u8>>> {
    let id = equipment::DEVICE;
    ensure!(
        args[1..3] == short(id).to_le_bytes() && args[7..11] == [2, endpoint(id), 0x60, 1],
        "wrong lock address or APS protection"
    );
    let f = Frame::decode(&args[16..])?;
    let seq = f.header.sequence;
    let wire = match (f.header.frame_type, f.header.command_id, f.payload) {
        (FrameType::General, 0, [0x34, 0]) => vec![0x18, seq, 1, 0x34, 0, 0, 0x30, 0],
        (FrameType::ClusterSpecific, 0 | 1, [6, pin @ ..]) => {
            ensure!(pin == test_pin().as_bytes(), "credential mismatch");
            vec![0x19, seq, f.header.command_id, 0]
        }
        (FrameType::General, 0, [0, 0]) => vec![0x18, seq, 1, 0, 0, 0, 0x30, 1],
        _ => bail!("unexpected lock operation"),
    };
    let mut correct = incoming(id, 0x101, &wire);
    correct[11] = 2;
    // A failure with matching peer, sequence and operation must not complete the
    // request when its APS header says it was received without link-key encryption.
    let mut clear = correct.clone();
    if f.header.frame_type == FrameType::ClusterSpecific {
        *clear.last_mut().unwrap() = 1;
    }
    correct[12] |= 0x20;
    Ok(vec![clear, correct])
}

pub async fn verify(client: &AsyncClient, net: &mut rumqttc::EventLoop, base: &str) -> Result<()> {
    for action in ["lock", "unlock"] {
        let body = json!({"id":"equipment","endpoint":endpoint(equipment::DEVICE),"action":action,"transaction":action});
        client
            .publish(
                format!("{base}/bridge/v1/request/device/lock"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&body)?,
            )
            .await?;
        loop {
            if let Event::Incoming(Incoming::Publish(p)) = net.poll().await?
                && p.topic == format!("{base}/bridge/v1/response/device/lock")
            {
                let result: Value = serde_json::from_slice(&p.payload)?;
                ensure!(
                    result
                        == json!({"status":"ok","transaction":action,"data":{"lock_state":"locked","matches_request":action=="lock"}}),
                    "unexpected lock reply: {result}"
                );
                break;
            }
        }
    }
    Ok(())
}
