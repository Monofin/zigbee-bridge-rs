//! Local bootstrap for reaching the broker. Secrets are read from credential
//! files at runtime and never form part of an MQTT configuration message.
use super::MqttOptions;
use anyhow::{Context, Result, ensure};
use native_tls::{Certificate, Protocol, TlsConnector};
use rumqttc::Transport;
use serde::Deserialize;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerConfig {
    host: String,
    port: u16,
    client_id: String,
    pub base_topic: String,
    pub discovery_prefix: String,
    #[serde(default = "enabled")]
    tls: bool,
    ca_file: Option<PathBuf>,
    username: Option<String>,
    password_file: Option<PathBuf>,
}
fn enabled() -> bool {
    true
}

impl BrokerConfig {
    pub fn load(path: &Path) -> Result<(MqttOptions, String, String)> {
        let c: Self = serde_json::from_reader(crate::store::open_regular(
            path,
            File::options().read(true),
        )?)
        .map_err(|e| {
            anyhow::anyhow!(
                "invalid MQTT configuration at line {}, column {}",
                e.line(),
                e.column()
            )
        })?;
        ensure!(
            !c.host.trim().is_empty()
                && !c.host.contains(['/', '@'])
                && c.port != 0
                && !c.client_id.is_empty(),
            "invalid MQTT host, port or client ID"
        );
        ensure!(c.tls || c.ca_file.is_none(), "CA file requires TLS");
        let root = path.parent().context("invalid MQTT configuration path")?;
        let mut opts = MqttOptions::new(c.client_id, c.host, c.port);
        if c.tls {
            let mut tls = TlsConnector::builder();
            tls.min_protocol_version(Some(Protocol::Tlsv12));
            if let Some(path) = c.ca_file {
                let pem = fs::read(root.join(path)).context("reading MQTT CA certificate")?;
                let roots = Certificate::stack_from_pem(&pem).context("parsing MQTT CA bundle")?;
                ensure!(!roots.is_empty(), "empty MQTT CA bundle");
                tls.disable_built_in_roots(true);
                for root in roots {
                    tls.add_root_certificate(root);
                }
            }
            opts.set_transport(Transport::tls_with_config(tls.build()?.into()));
        }
        match (c.username, c.password_file) {
            (Some(user), Some(p)) => {
                let mut pass =
                    fs::read_to_string(root.join(p)).context("reading MQTT password credential")?;
                if pass.ends_with('\n') {
                    pass.pop();
                    if pass.ends_with('\r') {
                        pass.pop();
                    }
                }
                ensure!(
                    !user.is_empty() && !pass.is_empty(),
                    "empty MQTT credential"
                );
                opts.set_credentials(user, pass);
            }
            (None, None) => (),
            _ => anyhow::bail!("provide both username and password_file"),
        }
        Ok((opts, c.base_topic, c.discovery_prefix))
    }
}
