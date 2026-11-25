//! Exercise optional-command discovery on the production interview transport.
use super::*;
use std::collections::VecDeque;

#[derive(Default)]
struct CoverIo {
    starts: Vec<u8>,
    incoming: VecDeque<Callback>,
}

impl CoverIo {
    fn reply(&mut self, seq: u8, command: u8, data: &[u8]) {
        let mut payload = vec![
            0,
            0x5e,
            0xc0,
            2,
            1,
            7,
            1,
            0,
            0,
            0,
            0,
            0,
            185,
            0,
            0x34,
            0x12,
            0,
            0,
            (3 + data.len()) as u8,
        ];
        payload.extend([0x18, seq, command]);
        payload.extend(data);
        self.incoming.push_back(Callback { id: 0x45, payload });
    }
}

impl InterviewIo for CoverIo {
    async fn send(&mut self, args: &[u8]) -> Result<Vec<u8>> {
        assert_eq!(&args[..9], &[0, 0x34, 0x12, 0x5e, 0xc0, 2, 1, 1, 7]);
        let frame = Frame::decode(&args[16..])?;
        let Payload::DiscoverCommandsReceived(query) = Payload::decode(frame)? else {
            panic!("unexpected request")
        };
        assert_eq!(query.maximum, 4);
        self.starts.push(query.start);
        let seq = frame.header.sequence;
        // An earlier read and a stale page arrive first.
        self.reply(seq, 1, &[0, 0, 0, 0x30, 8]);
        self.reply(seq.wrapping_sub(1), 0x12, &[1, 6]);
        match query.start {
            5 => self.reply(seq, 0x12, &[0, 5]),
            6 => self.reply(seq, 0x12, &[1, 7, 8]),
            _ => panic!("unexpected discovery cursor"),
        }
        Ok(vec![0, 1])
    }
    async fn receive(&mut self, _: Instant) -> Result<Option<Callback>> {
        Ok(self.incoming.pop_front())
    }
}

#[tokio::test]
async fn interview_records_only_current_standard_pages_and_advances_on_short_page() -> Result<()> {
    let mut commands = Vec::new();
    let mut record = |obs| {
        if let Observation::CommandsReceived {
            endpoint,
            cluster,
            commands: ids,
        } = obs
        {
            assert_eq!((endpoint, cluster), (7, 0x0102));
            commands.extend(ids);
        }
        Ok(())
    };
    let mut t = Target {
        record: &mut record,
        node: 0x1234,
        ieee: [0; 8],
        seq: 25,
    };
    let mut io = CoverIo::default();
    t.cover_commands(&mut io, 0xc05e, 7).await?;
    assert_eq!(io.starts, [5, 6]);
    assert_eq!(commands, [5, 7, 8]);
    Ok(())
}
