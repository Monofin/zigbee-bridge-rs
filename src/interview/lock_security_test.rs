use super::*;
use std::collections::VecDeque;
use zigbee_devices::equipment::{DOOR_LOCK, SECURITY_LEVEL};

struct Lock {
    reject: bool,
    sent: u8,
    incoming: VecDeque<Callback>,
}

impl InterviewIo for Lock {
    async fn send(&mut self, aps: &[u8]) -> Result<Vec<u8>> {
        self.sent += 1;
        assert_eq!(aps[9] & APS_ENCRYPTION, APS_ENCRYPTION);
        assert_eq!(&aps[16 + 3..], SECURITY_LEVEL.to_le_bytes());
        if self.reject {
            return Ok(vec![0x66, 0]);
        }
        let zcl = Frame::decode(&aps[16..])?;
        // A lower-security answer claims the lock is network-only. The encrypted reply says APS.
        for (flags, level) in [(0, 0), (APS_ENCRYPTION, 1)] {
            let record = [0x18, zcl.header.sequence, 1, 0x34, 0, 0, 0x30, level];
            let mut payload = vec![
                0, 4, 1, 1, 1, 7, aps[7], flags, 0, 0, 0, 0, 190, 0, 0x34, 0x12, 0, 0, 8,
            ];
            payload.extend(record);
            self.incoming.push_back(Callback { id: 0x45, payload });
        }
        Ok(vec![0, 0])
    }
    async fn receive(&mut self, _: Instant) -> Result<Option<Callback>> {
        Ok(self.incoming.pop_front())
    }
}

#[tokio::test]
async fn equipment_lock_security_probe_requires_an_aps_reply_without_fallback() -> Result<()> {
    for reject in [false, true] {
        let mut target = Target {
            record: &mut |_| Ok(()),
            node: 0x1234,
            ieee: [2; 8],
            seq: 8,
        };
        let mut lock = Lock {
            reject,
            sent: 0,
            incoming: VecDeque::new(),
        };
        let mut levels = Vec::new();
        target
            .read_observing(&mut lock, 0x104, 7, DOOR_LOCK, &[SECURITY_LEVEL], |_, r| {
                levels.push(r.clone())
            })
            .await?;
        assert_eq!(lock.sent, 1);
        if reject {
            assert!(levels.is_empty());
        } else {
            assert_eq!(
                levels,
                [AttributeResult::Value {
                    data_type: 0x30,
                    bytes: vec![1]
                }]
            );
        }
    }
    Ok(())
}
