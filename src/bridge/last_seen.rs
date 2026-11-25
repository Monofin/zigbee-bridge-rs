//! Host reception time of a message attributed to a current member.
use super::Bindings;
use crate::mqtt::Publisher;
use anyhow::Result;
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::File,
    io::ErrorKind,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const FILE: &str = "last-seen.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LastSeen {
    #[default]
    #[serde(rename = "disable")]
    Disabled,
    #[serde(rename = "epoch")]
    Epoch,
    #[serde(rename = "ISO_8601")]
    Utc,
    #[serde(rename = "ISO_8601_local")]
    Local,
    #[serde(rename = "relative")]
    Relative,
}

impl LastSeen {
    pub fn load(root: &Path) -> Result<Self> {
        match crate::store::open_regular(&root.join(FILE), File::options().read(true)) {
            Ok(file) => Ok(serde_json::from_reader(file)?),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::Disabled),
            Err(error) => Err(error.into()),
        }
    }

    /// The migration directory is private and has not yet been committed.
    pub fn stage(self, root: &Path) -> Result<()> {
        if self != Self::Disabled {
            let mut file = File::create_new(root.join(FILE))?;
            serde_json::to_writer(&mut file, &self)?;
            file.sync_all()?;
        }
        Ok(())
    }

    pub(super) fn timestamp(self) -> bool {
        matches!(self, Self::Utc | Self::Local)
    }

    fn value(self, time: SystemTime) -> Result<Option<Value>> {
        Ok(Some(match self {
            Self::Disabled => return Ok(None),
            Self::Epoch => {
                Value::from(u64::try_from(time.duration_since(UNIX_EPOCH)?.as_millis())?)
            }
            Self::Utc => DateTime::<Utc>::from(time)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
                .into(),
            Self::Local => DateTime::<Local>::from(time)
                .to_rfc3339_opts(SecondsFormat::Millis, false)
                .into(),
            // This field is published on receipt of the message itself.
            Self::Relative => "0 seconds ago".into(),
        }))
    }
}

impl Bindings {
    pub fn load_last_seen(&mut self, root: &Path) -> Result<()> {
        self.last_seen = LastSeen::load(root)?;
        Ok(())
    }

    pub(super) fn last_seen_enabled(&self) -> bool {
        self.last_seen != LastSeen::Disabled
    }

    pub(super) fn observed_last_seen(
        &self,
        out: &Publisher,
        ieee: u64,
        now: SystemTime,
    ) -> Result<()> {
        let Some((name, _)) = self.peers.get(&ieee) else {
            return Ok(());
        };
        if let Some(value) = self.last_seen.value(now)? {
            match out.state(name, "last_seen", value) {
                Ok(()) | Err(crate::mqtt::SubmitError::Full) => (),
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn settings_survive_staging_and_observations_keep_milliseconds() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(LastSeen::load(dir.path())?, LastSeen::Disabled);
        LastSeen::Utc.stage(dir.path())?;
        let policy = LastSeen::load(dir.path())?;
        let time = UNIX_EPOCH + Duration::from_millis(1234);
        assert_eq!(policy.value(time)?, Some("1970-01-01T00:00:01.234Z".into()));
        assert_eq!(LastSeen::Epoch.value(time)?, Some(1234u64.into()));
        assert_eq!(LastSeen::Disabled.value(time)?, None);
        Ok(())
    }
}
