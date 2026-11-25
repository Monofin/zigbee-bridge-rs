//! A two-role test fixture acts as an authorised ACE reader and a separate-direction panel server.
use super::{endpoint, equipment::DEVICE, ieee, incoming};
use anyhow::{Result, ensure};
use std::path::Path;
use zigbee_zcl::{Direction, Frame};

const TEST_CODE: &[u8] = b"TestAceReader44";

pub fn check_private(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.windows(TEST_CODE.len()).any(|w| w == TEST_CODE),
        "test keypad code escaped to an output"
    );
    Ok(())
}

pub fn check_state(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            check_state(&entry.path())?;
        } else if ty.is_file() {
            check_private(&std::fs::read(entry.path())?)?;
        }
    }
    Ok(())
}

pub fn configure(dir: &Path) -> Result<()> {
    std::fs::write(
        dir.join("ias-ace-keypads.json"),
        serde_json::to_vec(&serde_json::json!([{
            "ieee":format!("{:016x}",ieee(DEVICE)), "endpoint":endpoint(DEVICE)
        }]))?,
    )?;
    Ok(())
}

pub fn exchange(args: &[u8], frame: Frame<'_>) -> Result<Vec<Vec<u8>>> {
    let seq = frame.header.sequence;
    let callback = |zcl: &[u8], secured: bool| {
        let mut msg = incoming(DEVICE, 0x501, zcl);
        msg[11] = args[7];
        if secured {
            msg[12] |= 0x20;
        }
        msg
    };
    if frame.header.direction == Direction::ClientToServer {
        ensure!(frame.header.command_id == 7 && frame.payload.is_empty() && args[9] & 0x20 != 0);
        return Ok(vec![
            callback(&[0x19, seq, 5, 7, 0, 1, 1], false), // unprotected status cannot answer the query
            callback(&[0x11, seq.wrapping_add(26), 6, 0], false), // Directory data needs APS protection.
            callback(&[0x19, seq.wrapping_add(1), 5, 7, 0, 0, 1], true), // unmatched panel response
            callback(&[0x11, seq, 5], true), // independent Get Zone ID Map from its keypad side
        ]);
    }
    ensure!(
        args[9] & 0x20 != 0 && args[7] == 2,
        "keypad reply lost APS protection or local endpoint"
    );
    match frame.header.command_id {
        1 => {
            ensure!(frame.payload.len() == 32 && frame.payload[31] & 0x80 == 0);
            // Opaque test input. A denial must not echo the code or arm the CIE.
            let mut arm = vec![0x11, seq.wrapping_add(1), 0, 3, TEST_CODE.len() as u8];
            arm.extend(TEST_CODE);
            arm.push(255);
            Ok(vec![callback(&arm, true)])
        }
        0 => {
            ensure!(frame.payload == [5], "unexpected arm result");
            // The real panel's answer is released only once both earlier replies were verified.
            Ok(vec![callback(
                &[0x19, seq.wrapping_sub(1), 5, 1, 0, 0, 0],
                true,
            )])
        }
        _ => anyhow::bail!("unexpected keypad reply"),
    }
}
