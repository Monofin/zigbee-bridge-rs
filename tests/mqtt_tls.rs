//! Exercise the same file-backed broker bootstrap as the coordinator.
use anyhow::{Context, Result, ensure};
use rumqttc::v5::{AsyncClient, Event, Incoming, mqttbytes::QoS};
use std::{path::PathBuf, time::Duration};
use zigbee_bridge_rs::mqtt::config::BrokerConfig;

#[tokio::test]
#[ignore = "requires an isolated TLS broker and ZIGBEE_TEST_TLS_CONFIG"]
async fn private_ca_bootstrap_reaches_an_authenticated_broker() -> Result<()> {
    let path = PathBuf::from(
        std::env::var_os("ZIGBEE_TEST_TLS_CONFIG").context("set test broker config")?,
    );
    let (options, base, _) = BrokerConfig::load(&path)?;
    let (client, mut loop_) = AsyncClient::new(options, 8);
    let topic = format!("{base}/test");
    client.subscribe(&topic, QoS::AtLeastOnce).await?;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match loop_.poll().await? {
                Event::Incoming(Incoming::SubAck(_)) => {
                    client
                        .publish(&topic, QoS::AtLeastOnce, false, "confirmed")
                        .await?
                }
                Event::Incoming(Incoming::Publish(message)) => {
                    ensure!(
                        message.topic.as_ref() == topic.as_bytes() && message.payload.as_ref() == b"confirmed",
                        "unexpected TLS test payload"
                    );
                    return Ok(());
                }
                _ => (),
            }
        }
    })
    .await?
}
