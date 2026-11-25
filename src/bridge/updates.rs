//! An immediate MQTT update owns one response until a subsequent device query
//! confirms its running version. A transfer receipt alone cannot establish that.
use super::zdo;
use crate::{ash::{Destination, Ezsp}, mqtt::admin::Reply, ota::Server};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;

const LIMIT: Duration = Duration::from_secs(24 * 60 * 60);
#[derive(Default)]
pub(super) struct Updates(BTreeMap<u64, Pending>);
struct Pending {
    name: String,
    before: u32,
    expected: u32,
    until: Instant,
    reply: Reply,
}

impl Updates {
    pub fn active(&self, id: u64) -> bool {
        self.0.contains_key(&id)
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.0.values().map(|p| p.until).min()
    }
    pub async fn begin(
        &self,
        server: &mut Server,
        link: &mut Ezsp<'_>,
        destination: &Destination,
        seq: u8,
    ) -> Result<(u32, u32)> {
        let ieee = destination.ieee();
        ensure!(!self.active(ieee), "an update is already running");
        ensure!(
            server.authorization(ieee)?.is_none_or(|(_, done)| done),
            "an update is already scheduled"
        );
        let check = server.check(ieee)?;
        let before = check.current.context("awaiting device OTA query")?;
        let expected = check.latest.context("no applicable publisher image")?;
        let endpoint = server
            .observed_endpoint(ieee)
            .context("unknown OTA endpoint")?;
        server.schedule(ieee, None, LIMIT)?;
        // ZCL Image Notify: unicast, jitter only, ask this client to query.
        if let Err(error) = zdo::unicast(
            link,
            destination,
            0x0104,
            0x19,
            (1, endpoint),
            0xff,
            &[0x19, seq, 0x00, 0x00, 100],
        )
        .await
        {
            server.cancel(ieee)?;
            return Err(error);
        }
        Ok((before, expected))
    }
    pub fn track(&mut self, ieee: u64, name: String, (before, expected): (u32, u32), reply: Reply) {
        self.0.insert(
            ieee,
            Pending {
                name,
                before,
                expected,
                until: Instant::now() + LIMIT,
                reply,
            },
        );
    }
    pub fn stop(&mut self, ieee: u64, reason: &str) -> Result<()> {
        if let Some(p) = self.0.remove(&ieee) {
            p.reply.indeterminate(reason)?;
        }
        Ok(())
    }
    pub fn observe(&mut self, server: &Server, ieee: u64) -> Result<()> {
        let Some(p) = self.0.get(&ieee) else {
            return Ok(());
        };
        match server.authorization(ieee)? {
            None => self.stop(
                ieee,
                "Device ended the upgrade or its authorization expired",
            )?,
            Some((version, true))
                if version == p.expected && server.check(ieee)?.current == Some(version) =>
            {
                let p = self.0.remove(&ieee).unwrap();
                p.reply.ok(json!({"id":p.name,"from":{"file_version":p.before},"to":{"file_version":version}}))?;
            }
            _ => (),
        }
        Ok(())
    }
    pub fn expire(&mut self, server: &mut Server) -> Result<()> {
        while let Some((&ieee, _)) = self.0.iter().find(|(_, p)| p.until <= Instant::now()) {
            if server.authorization(ieee)?.is_some_and(|(_, done)| !done) {
                server.cancel(ieee)?;
            }
            self.stop(
                ieee,
                "Update timed out before the device confirmed its running version",
            )?;
        }
        Ok(())
    }
}
