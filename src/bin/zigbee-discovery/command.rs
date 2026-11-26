//! Render a staged coordinator's startup discovery without broker or radio I/O.
#[cfg(unix)]
pub fn run() -> anyhow::Result<()> {
    use anyhow::{Context, Result, bail, ensure};
    use serde::Serialize;
    use std::{
        collections::BTreeMap,
        env,
        fs::File,
        io::{self, Write},
        path::PathBuf,
    };
    use zigbee_bridge_rs::{
        bridge::Bindings,
        interview_log::{InterviewReport, Observation},
        store::Store,
    };
    use zigbee_devices::light_index::LightIndex;

    #[derive(Serialize)]
    struct Entry {
        topic: String,
        payload: serde_json::Value,
    }
    let mut args = env::args().skip(1);
    let state = PathBuf::from(args.next().context("expected STATE MQTT_BASE PREFIX")?);
    let base = args.next().context("missing MQTT base")?;
    let prefix = args.next().context("missing discovery prefix")?;
    let mut index = None;
    let mut coordinator = None;
    let mut jsonl = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--coordinator-ieee" if coordinator.is_none() => {
                let value = args.next().context("missing coordinator IEEE")?;
                let hex = value.strip_prefix("0x").unwrap_or(&value);
                ensure!(
                    hex.len() == 16,
                    "coordinator IEEE must contain 16 hexadecimal digits"
                );
                coordinator = Some(u64::from_str_radix(hex, 16)?);
            }
            "--light-index" if index.is_none() => {
                let path = args.next().context("missing light-index path")?;
                index = Some(LightIndex::from_reader(File::open(path)?)?);
            }
            "--jsonl" if !jsonl => jsonl = true,
            _ => bail!("unknown or repeated option {arg}"),
        }
    }
    let entries = Store::inspect(&state, |registry| -> Result<Vec<Entry>> {
        let mut bindings = index.map(Bindings::reviewed_lights).unwrap_or_default();
        bindings.load_naming(&state)?;
        bindings.load_last_seen(&state)?;
        bindings.load_availability(&state)?;
        bindings.load_device_options(registry.devices());
        let names: BTreeMap<_, _> = registry
            .devices()
            .map(|d| (d.ieee, d.name.clone()))
            .collect();
        // Fresh admissions have a member record even without an imported devices.json.
        let members = state.join("members");
        let mut identities: std::collections::BTreeSet<u64> = names.keys().copied().collect();
        for entry in std::fs::read_dir(&members)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "member") {
                let stem = path.file_stem().and_then(|s| s.to_str()).context("invalid member filename")?;
                let ieee = u64::from_str_radix(stem, 16)?;
                ensure!(format!("{ieee:016x}") == stem, "invalid member filename");
                identities.insert(ieee);
            }
        }
        for ieee in identities {
            let path = members.join(format!("{ieee:016x}.member"));
            let report = InterviewReport::open(&path).context("opening a staged member report")?;
            ensure!(report.entries.iter().any(|e|matches!(&e.observation,Observation::Target{ieee:s,..} if u64::from_str_radix(s,16).ok()==Some(ieee))),
                "member filename and report IEEE differ");
            bindings.add(&report, &names)?;
        }
        Ok(bindings
            .discovery_snapshot(&base, &prefix, coordinator)?
            .into_iter()
            .map(|(topic, payload)| Entry { topic, payload })
            .collect())
    })?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    if jsonl {
        for entry in entries {
            serde_json::to_writer(&mut output, &entry)?;
            writeln!(output)?;
        }
    } else {
        serde_json::to_writer(&mut output, &entries)?;
        writeln!(output)?;
    }
    output.flush()?;
    Ok(())
}
#[cfg(not(unix))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("zigbee-discovery requires Unix")
}
