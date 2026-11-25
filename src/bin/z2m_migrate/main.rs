//! Offline import of a copied Zigbee2MQTT data directory into a fresh store.
mod expectations;
mod source;
use anyhow::{Context, Result, bail, ensure};
use source::Plan;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
};
use zigbee_bridge_rs::{
    bridge::Bindings,
    interview_log::InterviewReport,
    registry::{Device, Registry},
    store::Store,
};

fn validate(plan: &Plan) -> Result<()> {
    Registry::from_snapshot(plan.members.iter().map(|m| m.device.clone()).collect())?;
    let mut bindings = Bindings::default();
    bindings.load_device_options(plan.members.iter().map(|m| &m.device));
    let names = names(plan.members.iter().map(|m| &m.device));
    for member in &plan.members {
        bindings.add(&member.report, &names)?;
    }
    Ok(())
}

fn names<'a>(devices: impl Iterator<Item = &'a Device>) -> BTreeMap<u64, String> {
    devices.map(|d| (d.ieee, d.name.clone())).collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stage(source: &Path, dest: &Path, plan: Plan) -> Result<()> {
    let parent = fs::canonicalize(
        dest.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    ensure!(
        !parent.starts_with(fs::canonicalize(source)?),
        "target must be outside the source directory"
    );
    let dest = parent.join(dest.file_name().context("invalid target directory name")?);
    ensure!(
        dest.symlink_metadata()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "target must be absent"
    );
    let tmp = tempfile::Builder::new()
        .prefix(".z2m-stage-")
        .tempdir_in(&parent)?;
    plan.last_seen.stage(tmp.path())?;
    plan.availability.write(tmp.path())?;
    plan.expectations
        .write(&tmp.path().join("reconciliation"))?;
    let mut store = Store::open(tmp.path())?;
    let members = tmp.path().join("members");
    fs::create_dir(&members)?;
    let mut restored = Bindings::default();
    store.update(|registry| {
        for member in &plan.members {
            registry.admit(member.device.clone())?;
        }
        Ok(())
    })?;
    restored.load_device_options(store.registry().devices());
    let names = names(store.registry().devices());
    for member in plan.members {
        let mut log = store.begin_interview()?;
        for entry in member.report.entries {
            log.record_entry(&entry)?;
        }
        let path = members.join(format!("{:016x}.member", member.device.ieee));
        fs::rename(log.path(), &path)?;
        restored.add(&InterviewReport::open(&path)?, &names)?;
    }
    File::open(&members)?.sync_all()?;
    File::open(tmp.path())?.sync_all()?;
    drop(store);
    // Validate through a new owner, then use the OS exclusive rename. An
    // existing empty directory is protected even if created during staging.
    let check = Store::open(tmp.path())?;
    ensure!(
        check.registry().devices().count() == names.len(),
        "staged device count mismatch"
    );
    drop(check);
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        tmp.path(),
        rustix::fs::CWD,
        &dest,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    let _published = tmp.keep();
    File::open(&parent)?
        .sync_all()
        .context("target published; parent directory sync failed")?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if let [mode, input] = args.as_slice()
        && mode == Path::new("check-adapter")
    {
        #[derive(serde::Deserialize)]
        struct Info {
            coordinator: Coordinator,
        }
        #[derive(serde::Deserialize)]
        struct Coordinator {
            #[serde(rename = "type")]
            kind: String,
            meta: Meta,
        }
        #[derive(serde::Deserialize)]
        struct Meta {
            ezsp: u8,
        }
        let info: Info = serde_json::from_reader(std::io::BufReader::new(File::open(input)?))
            .context("expected coordinator.type and coordinator.meta.ezsp from bridge/info")?;
        ensure!(
            info.coordinator.kind == "EmberZNet",
            "unsupported coordinator family"
        );
        let protocol = info.coordinator.meta.ezsp;
        ensure!(
            zigbee_bridge_rs::ash::supports_protocol(protocol),
            "adapter reports unsupported EZSP protocol {protocol}"
        );
        println!(
            "Adapter reports supported protocol EZSP {protocol}. Network resume still needs independent verification."
        );
        return Ok(());
    }
    let (source, target) = match args.as_slice() {
        [mode, source] if mode == Path::new("inspect") => (source, None),
        [mode, source, target] if mode == Path::new("stage") => (source, Some(target)),
        _ => bail!(
            "usage: z2m_migrate inspect SOURCE | z2m_migrate stage SOURCE NEW_STATE | z2m_migrate check-adapter BRIDGE_INFO_JSON"
        ),
    };
    let plan = Plan::load(source)?;
    validate(&plan)?;
    let summary = serde_json::to_string_pretty(&plan.counts)?;
    if let Some(target) = target {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        stage(source, target, plan)?;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        bail!("atomic stage is available on Linux and macOS");
    }
    println!("{summary}");
    Ok(())
}
