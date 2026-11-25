//! Verify a managed deletion while this session owns the retained topic.
use super::{subscriptions::Subscriptions, *};

#[derive(Default)]
pub(super) struct DeletionWitness {
    waiting: Mutex<Option<(String, oneshot::Sender<()>)>>,
    // A rejected deletion stays durable and must not disconnect unrelated devices.
    in_flight: Mutex<Option<(u64, Option<PubAckReason>)>>,
}

impl DeletionWitness {
    pub(super) fn rejected(&self, sequence: u64, reason: PubAckReason) -> bool {
        let mut in_flight = self.in_flight.lock().unwrap();
        if let Some((expected, rejected)) = in_flight.as_mut() && *expected == sequence {
            *rejected = Some(reason);
            return true;
        }
        false
    }

    pub(super) fn received(&self, msg: &Publish) {
        if msg.retain || !msg.payload.is_empty() {
            return;
        }
        let mut waiting = self.waiting.lock().unwrap();
        if waiting
            .as_ref()
            .is_some_and(|(topic, _)| topic.as_bytes() == msg.topic.as_ref())
        {
            let (_, receipt) = waiting.take().unwrap();
            let _ = receipt.send(());
        }
    }

    pub(super) async fn delete(
        &self,
        client: &AsyncClient,
        subscriptions: &Subscriptions,
        publisher: &Publisher,
        topic: String,
        acked: &mut watch::Receiver<u64>,
        sequence: u64,
    ) -> Result<()> {
        // MQTT 5 §3.3.1.3 requires an accepted retained deletion to be
        // forwarded to existing subscribers and the old retained value removed.
        let granted = subscriptions.subscribe(client, &[&topic]).await?;
        let (sent, observed) = oneshot::channel();
        if granted {
            *self.waiting.lock().unwrap() = Some((topic.clone(), sent));
        }
        *self.in_flight.lock().unwrap() = Some((sequence, None));
        client
            .publish(&topic, QoS::AtLeastOnce, true, Vec::new())
            .await?;
        acked.wait_for(|&n| n >= sequence).await?;
        let rejected = self.in_flight.lock().unwrap().take().and_then(|(_, reason)| reason);
        let confirmed = if granted && rejected.is_none() {
            let echo = tokio::time::timeout(Duration::from_secs(2), observed).await;
            matches!(echo, Ok(Ok(())))
        } else {
            false
        };
        if granted {
            self.waiting.lock().unwrap().take();
            client.unsubscribe(&topic).await?;
        }
        if let Some(reason) = rejected {
            eprintln!("MQTT broker declined deletion of {topic}: {reason:?}. Keeping it for reconnect");
        } else if confirmed {
            publisher.finish_deletion(&topic)?;
        } else {
            eprintln!("MQTT deletion of {topic} unverified; keeping it for reconnect");
        }
        Ok(())
    }
}

impl Publisher {
    fn finish_deletion(&self, topic: &str) -> Result<()> {
        let mut q = self.0.pending.lock().unwrap();
        // The sole drain is still handling this deletion. Any later update
        // (including delete/create/delete) must still occupy a pending slot.
        if q.configs.contains_key(topic)
            || q.waiting_configs.contains(topic)
            || !q.desired_configs.get(topic).is_some_and(Vec::is_empty)
        {
            return Ok(());
        }
        let store = q.store.as_ref().context("missing managed MQTT store")?;
        store.update_tombstones(&self.retained_scope(), &[(topic, false)])?;
        q.desired_configs.remove(topic);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn superseded_receipt_preserves_replacement_and_later_deletion() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (p, mut worker, _) = channel(MqttOptions::new("race", "localhost", 1883), "zb", 1)?;
        let _commands = worker.enable_reconnect(dir.path())?;
        let topic = "zb/lamp/availability";
        let take = || p.0.pending.lock().unwrap().take_config().unwrap();
        let durable = || {
            p.0.pending
                .lock()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .tombstones(&p.retained_scope())
        };

        p.enqueue_configs(&mut vec![
            (topic.into(), Vec::new()),
            (topic.into(), ONLINE.to_vec()),
        ])?;
        assert!(durable()?.is_empty());
        assert_eq!(take().1.bytes, ONLINE);

        p.retire_availability("lamp").await?;
        assert!(take().1.bytes.is_empty()); // The first deletion is in flight.
        p.device_availability("lamp", true)?;
        p.finish_deletion(topic)?;
        assert_eq!(take().1.bytes, ONLINE);
        assert!(durable()?.is_empty());

        p.retire_availability("lamp").await?;
        assert!(take().1.bytes.is_empty());
        // Force the replacement deletion into the overflow queue.
        p.device_availability("another", true)?;
        p.device_availability("lamp", true)?;
        p.retire_availability("lamp").await?;
        p.finish_deletion(topic)?;
        assert_eq!(durable()?, [topic]);
        assert_eq!(take().1.bytes, ONLINE);
        assert!(take().1.bytes.is_empty());
        p.finish_deletion(topic)?;
        assert!(durable()?.is_empty());
        assert!(
            !p.0.pending
                .lock()
                .unwrap()
                .desired_configs
                .contains_key(topic)
        );
        Ok(())
    }
}
