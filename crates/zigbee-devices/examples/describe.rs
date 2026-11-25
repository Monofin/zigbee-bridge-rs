//! Describe an interview supplied as JSON on stdin. No network or adapter.
use anyhow::Result;
use serde::Deserialize;
use std::io;
use zigbee_devices::{
    Endpoint, StandardLights,
    metering::{Meter, MeterProfiles},
    tuya::Profiles,
};

#[derive(Deserialize)]
struct Interview {
    manufacturer: Option<String>,
    model: Option<String>,
    endpoints: Vec<Descriptor>,
}
#[derive(Deserialize)]
struct Descriptor {
    id: u8,
    profile: u16,
    device_type: Option<u16>,
    servers: Vec<u16>,
    clients: Vec<u16>,
}
fn main() -> Result<()> {
    let input: Interview = serde_json::from_reader(io::stdin().lock())?;
    let meters = MeterProfiles::builtin()?;
    let lights = StandardLights::builtin()?;
    let profiles = Profiles::builtin()?;
    let mut rows = Vec::new();
    for d in input.endpoints {
        let ep = Endpoint {
            id: d.id,
            profile: d.profile,
            servers: &d.servers,
            clients: &d.clients,
        };
        if d.device_type.is_some_and(|dt| lights.matches(&ep, dt)) {
            rows.push(
                serde_json::json!({"endpoint":ep.id,"domain":"light","source":lights.source}),
            );
        }
        for f in ep.standard_features() {
            rows.push(serde_json::json!({"endpoint":ep.id,"property":f.property(),"server_cluster":f.cluster(),"source":"simple_descriptor"}));
        }
        if let Some(meter) = Meter::new(
            &meters,
            &ep,
            input.manufacturer.as_deref().unwrap_or(""),
            input.model.as_deref().unwrap_or(""),
        ) {
            for (supported, property, cluster) in [
                (meter.supports_watts(), "power", 0x0b04),
                (meter.supports_delivered_kwh(), "energy", 0x0702),
            ] {
                if supported {
                    rows.push(serde_json::json!({"endpoint":ep.id,"property":property,"server_cluster":cluster,"source":"interviewed_meter","scale_required":true}));
                }
            }
        }
        if let (Some(mfr), Some(model)) = (&input.manufacturer, &input.model)
            && let Some(profile) = profiles.resolve(mfr, model, &ep)
        {
            for (dp, feature, range) in profile.features() {
                rows.push(serde_json::json!({"endpoint":ep.id,"property":feature,"unit":feature.unit(),"datapoint":dp,"write_range":range,"source":profiles.source()}));
            }
        }
    }
    serde_json::to_writer_pretty(io::stdout().lock(), &rows)?;
    println!();
    Ok(())
}
