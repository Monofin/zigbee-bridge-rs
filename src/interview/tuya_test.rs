//! Exercise configuration through the same Basic read path as a fresh interview.
use super::*;

struct BasicIo<'a> {
    model: &'a str,
    maker: &'a str,
    reads: Vec<Vec<u16>>,
    response: Option<Callback>,
}

impl InterviewIo for BasicIo<'_> {
    async fn send(&mut self, args: &[u8]) -> Result<Vec<u8>> {
        let frame = Frame::decode(&args[16..])?;
        let Payload::ReadAttributes(ids) = Payload::decode(frame)? else {
            bail!("expected Basic read")
        };
        assert_eq!(&args[3..9], &[4, 1, 0, 0, 1, 1]);
        let mut zcl = vec![0x18, frame.header.sequence, 1];
        for &id in &ids {
            zcl.extend(id.to_le_bytes());
            let text = match id {
                4 => self.maker,
                5 => self.model,
                _ => {
                    zcl.push(0x86);
                    continue;
                }
            };
            zcl.extend([0, 0x42, text.len() as u8]);
            zcl.extend(text.as_bytes());
        }
        self.reads.push(ids);
        let mut payload = vec![
            0,
            4,
            1,
            0,
            0,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            150,
            0,
            0x34,
            0x12,
            0,
            0,
            zcl.len() as u8,
        ];
        payload.extend(zcl);
        self.response = Some(Callback { id: 0x45, payload });
        Ok(vec![0, 1])
    }

    async fn receive(&mut self, _: Instant) -> Result<Option<Callback>> {
        Ok(self.response.take())
    }
}

#[tokio::test]
async fn setup_reads_follow_only_a_matching_fresh_identity() -> Result<()> {
    let descriptor = Descriptor {
        profile: HA,
        device: 0x51,
        version: 1,
        servers: vec![0, 0xef00],
        clients: vec![],
    };
    for (maker, admitted, setup) in [
        ("_TZE284_oitavov2", true, true),
        ("_TZE284_oitavov2", false, false),
        ("_TZE200_cowvfni3", true, false),
        ("unlisted", true, false),
    ] {
        let mut io = BasicIo {
            maker,
            model: "TS0601",
            reads: vec![],
            response: None,
        };
        let mut target = Target {
            node: 0x1234,
            ieee: [0; 8],
            seq: 1,
            record: &mut |_| Ok(()),
        };
        target.basic(&mut io, 1, &descriptor, admitted).await?;
        assert_eq!(io.reads[0], [4, 5, 0x4000]);
        assert_eq!(io.reads.len(), if setup { 2 } else { 1 });
        if setup {
            assert_eq!(io.reads[1], [4, 0, 1, 5, 7, 0xfffe]);
        }
    }
    Ok(())
}
