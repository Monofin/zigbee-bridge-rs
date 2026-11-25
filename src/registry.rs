//! A route belongs to one membership. A late reply must present the original
//! membership, even when the same physical device has already rejoined.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    pub ieee: u64,
    pub name: String,
    pub endpoint: u8,
    pub short: Option<u16>,
    /// Named Home Assistant temperature sensor discovery option, retained across readdressing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_force_update: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vibration_timeout: Option<u32>,
}

impl Device {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.ieee != 0 && self.ieee != u64::MAX,
            "invalid IEEE address"
        );
        ensure!(
            (1..=240).contains(&self.endpoint),
            "invalid application endpoint"
        );
        ensure!(
            self.short.is_none_or(|n| n < 0xfff8 && n != 0),
            "reserved network address"
        );
        ensure!(
            crate::mqtt::device_name(&self.name),
            "unsupported MQTT device name"
        );
        Ok(())
    }
}

/// A process-local generation. A retained handle keeps its generation unique
/// through release, reuse of the same address, and creation of a new session.
#[derive(Clone, Debug, Default)]
pub(crate) struct Incarnation(Arc<()>);

impl PartialEq for Incarnation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Incarnation {}

#[derive(Clone, Debug)]
pub struct Membership {
    ieee: u64,
    identity: Incarnation,
}

#[derive(Default)]
pub struct Registry {
    members: BTreeMap<u64, (Device, Membership)>,
    addresses: HashMap<u16, u64>,
}

impl Registry {
    pub(crate) fn staged(&self) -> Self {
        Self {
            members: self.members.clone(),
            addresses: self.addresses.clone(),
        }
    }

    pub fn from_snapshot(devices: Vec<Device>) -> Result<Self> {
        let mut registry = Self::default();
        for device in devices {
            ensure!(
                !registry.members.contains_key(&device.ieee),
                "duplicate saved IEEE address"
            );
            ensure!(
                device
                    .short
                    .is_none_or(|n| !registry.addresses.contains_key(&n)),
                "duplicate saved network address"
            );
            registry.admit(device)?;
        }
        Ok(registry)
    }

    /// A confirmed admission starts a new membership. Duplicate announces
    /// go through announce() so they do not invalidate an ordinary read.
    pub fn admit(&mut self, device: Device) -> Result<Membership> {
        device.validate()?;
        ensure!(
            !self
                .devices()
                .any(|d| d.ieee != device.ieee && d.name == device.name),
            "duplicate device name"
        );
        self.remove(device.ieee);
        if let Some(short) = device.short
            && let Some(old_owner) = self.addresses.insert(short, device.ieee)
        {
            let (old, _) = self.members.get_mut(&old_owner).expect("address index");
            old.short = None;
        }
        let membership = Membership {
            ieee: device.ieee,
            identity: Incarnation::default(),
        };
        self.members
            .insert(device.ieee, (device, membership.clone()));
        Ok(membership)
    }

    pub fn announce(&mut self, ieee: u64, short: u16) -> Result<Membership> {
        let (device, membership) = self
            .members
            .get(&ieee)
            .ok_or_else(|| anyhow::anyhow!("unknown device"))?;
        if device.short == Some(short) {
            return Ok(membership.clone());
        }
        let mut current = device.clone();
        current.short = Some(short);
        self.admit(current)
    }

    pub fn remove(&mut self, ieee: u64) -> Option<Device> {
        let (device, _) = self.members.remove(&ieee)?;
        if let Some(short) = device.short
            && self.addresses.get(&short) == Some(&ieee)
        {
            self.addresses.remove(&short);
        }
        Some(device)
    }

    /// Use the membership captured when the leave operation was issued or reconciled.
    pub fn left(&mut self, membership: &Membership, short: u16) -> Option<Device> {
        let (_, address) = self.resolve(membership)?;
        if address == short {
            self.remove(membership.ieee)
        } else {
            None
        }
    }

    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.members.values().map(|(d, _)| d)
    }
    pub fn membership(&self, ieee: u64) -> Option<Membership> {
        self.members.get(&ieee).map(|(_, m)| m.clone())
    }
    pub fn resolve(&self, membership: &Membership) -> Option<(&Device, u16)> {
        let (device, current) = self.members.get(&membership.ieee)?;
        let address = device.short?;
        (current.identity == membership.identity
            && self.addresses.get(&address) == Some(&device.ieee))
        .then_some((device, address))
    }
}
