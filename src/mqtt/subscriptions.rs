//! Derive broker filters from the same names used by the radio dispatcher.
use super::*;
use rumqttc::v5::mqttbytes::v5::Filter;
use std::collections::BTreeSet;

pub(crate) type Routes = BTreeMap<String, (u64, u8)>;

// Route changes and deletion witnesses share one MQTT connection. Keep the
// request and its SUBACK together even when both callers subscribe at once.
pub(super) struct Subscriptions(tokio::sync::Mutex<mpsc::Receiver<Vec<SubscribeReasonCode>>>);

impl Subscriptions {
    pub(super) fn new(replies: mpsc::Receiver<Vec<SubscribeReasonCode>>) -> Self {
        Self(tokio::sync::Mutex::new(replies))
    }

    pub(super) async fn subscribe(&self, client: &AsyncClient, topics: &[&String]) -> Result<bool> {
        let mut replies = self.0.lock().await;
        client
            .subscribe_many(
                topics
                    .iter()
                    .map(|topic| Filter::new((*topic).clone(), QoS::AtLeastOnce)),
            )
            .await?;
        let codes = replies
            .recv()
            .await
            .context("subscription connection ended")?;
        ensure!(codes.len() == topics.len(), "unexpected MQTT SUBACK length");
        Ok(codes
            .iter()
            .all(|code| matches!(code, SubscribeReasonCode::Success(_))))
    }
}

pub(super) async fn maintain(
    worker: &Worker,
    client: &AsyncClient,
    subscriptions: &Subscriptions,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let mut routes = worker.publisher.0.routes.subscribe();
    let mut installed = BTreeSet::new();
    let mut ready = Some(ready);
    loop {
        let desired = if worker.routed.is_some() {
            let names = routes
                .wait_for(|table| table.is_some())
                .await?
                .clone()
                .unwrap();
            let mut filters = BTreeSet::from([
                HA_STATUS_TOPIC.to_owned(),
                format!("{}/bridge/request/#", worker.base),
                format!("{}/bridge/v1/request/#", worker.base),
            ]);
            for name in names.keys() {
                for suffix in ["set", "get", "set/+", "get/+"] {
                    filters.insert(format!("{}/{name}/{suffix}", worker.base));
                }
            }
            filters
        } else {
            BTreeSet::from([format!("{}/#", worker.base)])
        };
        for old in installed.difference(&desired) {
            client.unsubscribe(old).await?;
        }
        let new: Vec<_> = desired.difference(&installed).collect();
        // Each name and base is bounded to 512 bytes. This batch fits the wire budget.
        for chunk in new.chunks(32) {
            ensure!(
                subscriptions.subscribe(client, chunk).await?,
                "broker rejected command subscription"
            );
        }
        installed = desired;
        if worker.routed.is_some() && routes.has_changed()? {
            continue;
        }
        if let Some(ready) = ready.take() {
            let _ = ready.send(());
        }
        routes.changed().await?;
    }
}
