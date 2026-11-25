//! Admission, coalescing and fair draining of reported state and ordered events.
use super::*;

impl Pending {
    // Keep the existing checkpoint format. Split old and new checkpoints into
    // independent MQTT snapshots before either is admitted for publication.
    pub(super) fn restore_snapshot(&mut self, device: &str, bytes: &[u8]) -> Result<()> {
        let mut state: Map<String, Value> = serde_json::from_slice(bytes)?;
        let mut diagnostics = Map::new();
        for key in RADIO_FIELDS {
            if let Some(value) = state.remove(key) {
                diagnostics.insert(key.into(), value);
            }
        }
        if !state.is_empty() {
            self.snapshots.insert(device.into(), state);
        }
        if !diagnostics.is_empty() {
            self.snapshots.insert(diagnostic_route(device), diagnostics);
        }
        Ok(())
    }

    pub(super) fn put(
        &mut self,
        device: &str,
        fields: Map<String, Value>,
        delivery: Delivery,
        raw_action: Option<Vec<u8>>,
        cap: usize,
    ) -> Result<(), SubmitError> {
        if self.closed {
            return Err(SubmitError::Closed);
        }
        let owner = device.strip_prefix(RADIO_DIAGNOSTICS).unwrap_or(device);
        if delivery == Delivery::State {
            if !self.identities.contains_key(owner)
                && !self.dirty.contains_key(device)
                && self.dirty.len() == cap
            {
                self.overflows.states += 1;
                self.latest_rejection = Some((device.to_owned(), "report"));
                return Err(SubmitError::Full);
            }
        }

        let mut snapshot = if delivery == Delivery::Event {
            Map::new()
        } else {
            self.snapshots.get(device).cloned().unwrap_or_default()
        };
        snapshot.extend(fields);
        let payload = serde_json::to_vec(&snapshot).map_err(|_| SubmitError::Invalid)?;
        let bytes = payload.len() + raw_action.as_ref().map_or(0, Vec::len);
        let state_changed = self.snapshots.get(device) != Some(&snapshot);
        if delivery != Delivery::Event {
            if state_changed
                && let (Some(store), Some(&id)) = (&self.store, self.identities.get(owner))
            {
                let sibling = if device == owner { diagnostic_route(owner) } else { owner.to_owned() };
                if let Some(other) = self.snapshots.get(&sibling) {
                    let checkpoint: BTreeMap<_, _> = other.iter().chain(&snapshot).collect();
                    store.stage(id, &serde_json::to_vec(&checkpoint).map_err(|_| SubmitError::Invalid)?);
                } else {
                    store.stage(id, &payload);
                }
            }
            self.snapshots.insert(device.to_owned(), snapshot);
        }
        if delivery != Delivery::State
            && (self.queued.len() - self.dirty.len() >= cap
                || self.event_bytes + bytes > QUEUED_EVENT_BYTES)
        {
            self.overflows.events += 1;
            self.latest_rejection = Some((device.to_owned(), "event"));
            if delivery == Delivery::Observation && state_changed {
                self.remove_state_slot(device);
                self.defer_state(device);
            }
            return Err(SubmitError::Full);
        }
        if payload.len() > SNAPSHOT_LIMIT {
            self.overflows.states += 1;
            eprintln!(
                "MQTT snapshot for {device} is {} bytes (limit {SNAPSHOT_LIMIT})",
                payload.len()
            );
            self.latest_rejection = Some((device.to_owned(), "snapshot bytes"));
            // Revoke a previously queued version. The current state is still in the store.
            self.remove_state_slot(device);
            return Err(SubmitError::Full);
        }
        if delivery == Delivery::State {
            let old_size = self
                .dirty
                .get(device)
                .map_or(0, |d| self.queued[&d.slot].payload.len());
            if (self.dirty.len() >= cap && !self.dirty.contains_key(device))
                || self.state_bytes - old_size + bytes > QUEUED_STATE_BYTES
            {
                self.remove_state_slot(device);
                self.defer_state(device);
                return Ok(());
            }
            if self.waiting_names.remove(device) {
                self.waiting.retain(|name| name != device);
            }
            self.state_bytes = self.state_bytes - old_size + bytes;
        }
        if delivery != Delivery::State {
            self.event_bytes += bytes;
        }
        let mut slot = self.sequence;
        self.sequence += 1;
        if delivery == Delivery::State {
            let dirty = self.dirty.entry(device.to_owned()).or_insert(Dirty {
                slot,
                followed_by_event: false,
            });
            // A hot device keeps its place unless an ordered observation for it
            // intervened. In that case an older snapshot must never follow it.
            if dirty.followed_by_event {
                self.queued.remove(&dirty.slot);
                dirty.slot = slot;
                dirty.followed_by_event = false;
            } else {
                slot = dirty.slot;
            }
        } else if let Some(dirty) = self.dirty.get_mut(device) {
            dirty.followed_by_event = true;
        }
        self.queued.insert(
            slot,
            Item {
                device: device.to_owned(),
                delivery,
                payload,
                raw_action,
                expires: (delivery == Delivery::Event)
                    .then(|| tokio::time::Instant::now() + EVENT_TTL),
            },
        );
        Ok(())
    }

    pub(super) fn take(&mut self) -> Option<Item> {
        loop {
            self.fill_state_slots();
            let (_, item) = self.queued.pop_first()?;
            if item.delivery == Delivery::State {
                self.dirty.remove(&item.device);
                self.state_bytes -= item.payload.len();
                self.fill_state_slots();
            } else {
                self.event_bytes -=
                    item.payload.len() + item.raw_action.as_ref().map_or(0, Vec::len);
            }
            if item
                .expires
                .is_some_and(|t| t <= tokio::time::Instant::now())
            {
                self.overflows.events += 1;
                self.latest_rejection = Some((item.device, "expired event"));
                continue;
            }
            return Some(item);
        }
    }

    pub(super) fn queue_config(&mut self, topic: &str) {
        let bytes = &self.desired_configs[topic];
        if let Some(old) = self.configs.remove(topic) {
            self.config_bytes -= old.bytes.len();
        }
        if self.configs.len() >= self.capacity
            || self.config_bytes + bytes.len() > QUEUED_RETAINED_BYTES
        {
            self.waiting_configs.insert(topic.to_owned());
            return;
        }
        self.waiting_configs.remove(topic);
        self.config_bytes += bytes.len();
        self.configs.insert(
            topic.to_owned(),
            Retained {
                bytes: bytes.clone(),
                receipt: None,
            },
        );
    }

    pub(super) fn take_config(&mut self) -> Option<(String, Retained)> {
        let item = self.configs.pop_first()?;
        if self.store.is_some() {
            self.config_bytes -= item.1.bytes.len();
            while self.configs.len() < self.capacity {
                let Some(topic) = self.waiting_configs.first().cloned() else {
                    break;
                };
                if self.config_bytes + self.desired_configs[&topic].len() > QUEUED_RETAINED_BYTES {
                    break;
                }
                self.queue_config(&topic);
            }
        }
        Some(item)
    }

    fn remove_state_slot(&mut self, device: &str) {
        if let Some(dirty) = self.dirty.remove(device)
            && let Some(item) = self.queued.remove(&dirty.slot)
        {
            self.state_bytes -= item.payload.len();
        }
    }

    // HA has resubscribed after discovery. Reuse the bounded state drain so a
    // returning consumer gets the most recent values without replaying gestures.
    pub(super) fn queue_birth_snapshot(&mut self) {
        for name in self.snapshots.keys() {
            if self.identities.get(name.strip_prefix(RADIO_DIAGNOSTICS).unwrap_or(name)).is_some_and(|(id, _)| !self.inactive.contains(id))
                && !self.dirty.contains_key(name) && self.waiting_names.insert(name.clone())
            {
                self.waiting.push_back(name.clone());
            }
        }
    }

    fn defer_state(&mut self, device: &str) {
        if self.waiting_names.insert(device.to_owned()) {
            self.waiting.push_back(device.to_owned());
        }
    }

    fn fill_state_slots(&mut self) {
        while self.dirty.len() < self.capacity
            && self.state_bytes <= QUEUED_STATE_BYTES - SNAPSHOT_LIMIT
        {
            let Some(name) = self.waiting.pop_front() else {
                break;
            };
            self.waiting_names.remove(&name);
            self.replay_state(&name)
                .expect("in-memory JSON values serialize");
        }
    }

    pub(super) fn replay_state(&mut self, device: &str) -> Result<()> {
        if self.dirty.contains_key(device) {
            return Ok(());
        }
        let Some(fields) = self.snapshots.get(device) else {
            return Ok(());
        };
        if self.dirty.len() >= self.capacity
            || self.state_bytes > QUEUED_STATE_BYTES - SNAPSHOT_LIMIT
        {
            self.defer_state(device);
            return Ok(());
        }
        let payload = serde_json::to_vec(fields)?;
        if payload.len() > SNAPSHOT_LIMIT {
            return Ok(());
        }
        self.state_bytes += payload.len();
        let slot = self.sequence;
        self.sequence += 1;
        self.dirty.insert(
            device.into(),
            Dirty {
                slot,
                followed_by_event: false,
            },
        );
        self.queued.insert(
            slot,
            Item {
                device: device.into(),
                delivery: Delivery::State,
                payload,
                expires: None,
                raw_action: None,
            },
        );
        Ok(())
    }

    pub(super) fn clear_device(&mut self, device: &str) {
        self.clear_snapshot(&diagnostic_route(device));
        self.clear_snapshot(device);
    }

    fn clear_snapshot(&mut self, device: &str) {
        self.snapshots.remove(device);
        self.remove_state_slot(device);
        self.waiting_names.remove(device);
        self.waiting.retain(|name| name != device);
        self.queued.retain(|_, item| {
            if item.device != device {
                return true;
            }
            if item.delivery != Delivery::State {
                self.event_bytes -=
                    item.payload.len() + item.raw_action.as_ref().map_or(0, Vec::len);
            }
            false
        });
    }
}
