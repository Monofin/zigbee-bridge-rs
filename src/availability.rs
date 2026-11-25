//! Typed per-device presence policy. All persisted durations follow Z2M's
//! units: timeout in minutes and max_jitter in milliseconds.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};
#[cfg(unix)]
pub(crate) mod tracker;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Active {
    pub timeout: f64,
    pub max_jitter: u32,
    pub backoff: bool,
    pub pause_on_backoff_gt: u32,
}
impl Default for Active {
    fn default() -> Self {
        Self {
            timeout: 10.,
            max_jitter: 30_000,
            backoff: true,
            pause_on_backoff_gt: 0,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Passive {
    pub timeout: f64,
}
impl Default for Passive {
    fn default() -> Self {
        Self { timeout: 1500. }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Policy {
    pub enabled: bool,
    pub active: Active,
    pub passive: Passive,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    timeout: Option<f64>,
    max_jitter: Option<u32>,
    backoff: Option<bool>,
    pause_on_backoff_gt: Option<u32>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum Override {
    Enabled(bool),
    Options(Options),
}
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
struct Member {
    #[serde(default)]
    router: bool,
    active: bool,
    last_seen: Option<u64>,
    policy: Option<Override>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Catalogue {
    global: Policy,
    devices: BTreeMap<u64, Member>,
    #[serde(skip)]
    pub(crate) directory: Option<PathBuf>,
}
#[derive(Clone, Copy)]
pub(crate) struct Settings {
    pub active: bool,
    pub timeout: Duration,
    pub jitter: u32,
    pub backoff: bool,
    pub pause: u32,
}

fn valid_timeout(minutes: f64) -> Result<()> {
    let delay =
        Duration::try_from_secs_f64(minutes * 60.).context("invalid availability timeout")?;
    ensure!(
        !delay.is_zero() && std::time::Instant::now().checked_add(delay).is_some(),
        "availability timeout exceeds the local timer range or is zero"
    );
    Ok(())
}
impl Catalogue {
    pub fn import(value: Option<&Value>) -> Result<Self> {
        let global = match value {
            None | Some(Value::Null) => Policy::default(),
            Some(Value::Bool(v)) => Policy {
                enabled: *v,
                ..Default::default()
            },
            Some(v) => {
                serde_json::from_value(v.clone()).context("invalid availability settings")?
            }
        };
        let c = Self {
            global,
            ..Default::default()
        };
        c.validate()?;
        Ok(c)
    }
    pub fn import_member(
        &mut self,
        id: u64,
        role: &str,
        power: Option<&str>,
        last_seen: Option<u64>,
        option: Option<&Value>,
    ) -> Result<()> {
        let active = (role == "Router" && power != Some("Battery"))
            || power.is_some_and(|p| !matches!(p, "Unknown" | "Battery"));
        let policy = option
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .context("invalid device availability override")?;
        if let Some(Override::Options(Options {
            timeout: Some(m), ..
        })) = &policy
        {
            valid_timeout(*m)?;
        }
        self.devices.insert(
            id,
            Member {
                router: role == "Router",
                active,
                last_seen,
                policy,
            },
        );
        Ok(())
    }
    fn validate(&self) -> Result<()> {
        valid_timeout(self.global.active.timeout)?;
        valid_timeout(self.global.passive.timeout)?;
        for d in self.devices.values() {
            if let Some(Override::Options(Options {
                timeout: Some(t), ..
            })) = &d.policy
            {
                valid_timeout(*t)?;
            }
        }
        Ok(())
    }
    pub fn write(&self, dir: &Path) -> Result<()> {
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(dir.join("availability.json"))?;
        serde_json::to_writer(&file, self)?;
        file.sync_all()?;
        Ok(())
    }
    pub fn load(dir: &Path) -> Result<Self> {
        let mut c: Self = match crate::store::open_regular(
            &dir.join("availability.json"),
            File::options().read(true),
        ) {
            Ok(f) => serde_json::from_reader(f).context("invalid availability policy")?,
            Err(e) if e.kind() == ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        c.validate()?;
        c.directory = Some(dir.to_owned());
        Ok(c)
    }
    pub(crate) fn walk_enabled(&self, id: u64, receiver_on: Option<bool>) -> bool {
        let member = self.devices.get(&id);
        !matches!(member.and_then(|m| m.policy.as_ref()), Some(Override::Enabled(false)))
            && receiver_on.unwrap_or_else(|| member.is_some_and(|m| m.router))
    }

    pub fn enabled(&self, id: u64) -> bool {
        match self.devices.get(&id).and_then(|d| d.policy.as_ref()) {
            Some(Override::Options(_)) => true,
            Some(Override::Enabled(v)) => *v,
            None => self.global.enabled,
        }
    }
    pub(crate) fn settings(&self, id: u64, receiver_on: Option<bool>, hint: bool) -> Option<Settings> {
        if !self.enabled(id) {
            return None;
        }
        let active = receiver_on.unwrap_or_else(|| self.devices.get(&id).map_or(hint, |d| d.router || d.active));
        let global = &self.global.active;
        let opts = match self.devices.get(&id).and_then(|d| d.policy.as_ref()) {
            Some(Override::Options(o)) => o.clone(),
            _ => Options::default(),
        };
        let minutes = opts.timeout.unwrap_or(if active {
            global.timeout
        } else {
            self.global.passive.timeout
        });
        Some(Settings {
            active,
            timeout: Duration::from_secs_f64(minutes * 60.).max(Duration::from_millis(1)),
            jitter: opts.max_jitter.unwrap_or(global.max_jitter),
            backoff: opts.backoff.unwrap_or(global.backoff),
            pause: opts
                .pause_on_backoff_gt
                .unwrap_or(global.pause_on_backoff_gt),
        })
    }
}
