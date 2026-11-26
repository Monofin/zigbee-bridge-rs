#![cfg(any(target_os = "linux", target_os = "macos"))]
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::tempdir;
use zigbee_bridge_rs::{
    bridge::Bindings,
    interview_log::{IdentitySource, InterviewReport, Observation},
    store::Store,
};

const IEEE: u64 = 0x0200910000000001;
const CANARY: &str = "PRIVATE_TEST_SENTINEL";

fn input(dir: &Path) -> Result<()> {
    fs::create_dir(dir)?;
    fs::write(
        dir.join("configuration.yaml"),
        format!(
            "mqtt:\n  password: {CANARY}\nadvanced:\n  network_key: '!secret key'\ndevices: [first.yaml, second.yaml]\ngroups:\n  2:\n    friendly_name: Test Group\n"
        ),
    )?;
    fs::write(
        dir.join("first.yaml"),
        "'0x0200910000000001':\n  friendly_name: Temp\n",
    )?;
    fs::write(
        dir.join("second.yaml"),
        "'0x0200910000000001':\n  friendly_name: Bench Light\n",
    )?;
    fs::write(dir.join("coordinator_backup.json"), CANARY)?;
    let ep = |id: u8| {
        json!({"epId":id,"profId":260,"devId":257,"inClusterList":[0,6,8],"outClusterList":[25],
        "clusters":{"genBasic":{"attributes":{"manufacturerName":"Vendor","modelId":"Model-A"}}},"binds":[{"type":"endpoint","deviceIeeeAddress":"0x0200910000000002","endpointID":1,"cluster":6,"secret":CANARY}],"configuredReportings":[{"cluster":8,"attrId":0,"minRepIntval":3,"maxRepIntval":600,"repChange":0.5,"manufacturerCode":1988,"extra":CANARY}],"meta":{"private":CANARY}})
    };
    let row = json!({"id":2,"type":"Router","ieeeAddr":format!("0x{IEEE:016x}"),"nwkAddr":5000,"interviewState":"SUCCESSFUL",
        "endpoints":{"1":ep(1),"2":ep(2)},"epList":[1,2],"gpSecurityKey":CANARY});
    fs::write(
        dir.join("database.db"),
        format!(
            "{}\n{}\n{}\n",
            json!({"id":1,"type":"Coordinator"}),
            row,
            json!({"type":"Group","members":[{},{}]})
        ),
    )?;
    Ok(())
}
fn run(args: &[&Path]) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_z2m_migrate"))
        .args(args)
        .output()?)
}

#[test]
fn stage_reopens_normal_store_and_member_paths_without_reusing_addresses_or_credentials()
-> Result<()> {
    let tmp = tempdir()?;
    let src = tmp.path().join("source");
    input(&src)?;
    let dest = tmp.path().join("candidate");
    let original = fs::read(src.join("database.db"))?;
    let inspect = run(&[Path::new("inspect"), &src])?;
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    assert!(!dest.exists());
    let report: Value = serde_json::from_slice(&inspect.stdout)?;
    assert_eq!(report["imported_devices"], 1);
    assert_eq!(report["imported_application_endpoints"], 2);
    assert_eq!(report["configured_groups_not_imported"], 1);
    assert_eq!(report["bindings_not_imported"], 2);
    assert_eq!(report["group_memberships_not_imported"], 2);
    let stage = run(&[Path::new("stage"), &src, &dest])?;
    assert!(
        stage.status.success(),
        "{}",
        String::from_utf8_lossy(&stage.stderr)
    );
    assert_eq!(inspect.stdout, stage.stdout);
    let notes = fs::read(dest.join("reconciliation/expectations.json"))?;
    let expected: Value = serde_json::from_slice(&notes)?;
    assert_eq!(
        expected["bindings"][0]["target"]["ieee"],
        "0x0200910000000002"
    );
    assert_eq!(expected["reporting"][0]["direction"], 0);
    assert_eq!(expected["reporting"][0]["change"], 0.5);
    assert_eq!(expected["reporting"][0]["manufacturer_code"], 1988);
    assert!(!String::from_utf8_lossy(&notes).contains(CANARY));
    let store = Store::open(&dest)?;
    let device = store.registry().devices().next().unwrap();
    assert_eq!(device.ieee, IEEE);
    assert_eq!(device.name, "Bench Light");
    assert_eq!(device.short, None);
    let file = dest.join(format!("members/{IEEE:016x}.member"));
    let facts = InterviewReport::open(&file)?;
    assert!(facts.finished);
    assert!(facts.entries.iter().all(|entry| entry.unix_ms == 0));
    assert!(matches!(
        facts.entries[0].observation,
        Observation::Target {
            identity_source: IdentitySource::Z2mDatabase,
            ..
        }
    ));
    Bindings::default().add(&facts, &BTreeMap::from([(IEEE, device.name.clone())]))?;
    for bytes in [
        fs::read(&file)?,
        fs::read(dest.join("devices.json"))?,
        stage.stdout,
        stage.stderr,
    ] {
        assert!(!String::from_utf8_lossy(&bytes).contains(CANARY));
    }
    assert_eq!(original, fs::read(src.join("database.db"))?);
    assert!(!dest.join("coordinator_backup.json").exists());
    assert!(!run(&[Path::new("stage"), &src, &dest])?.status.success());
    let empty = tmp.path().join("empty");
    fs::create_dir(&empty)?;
    assert!(!run(&[Path::new("stage"), &src, &empty])?.status.success());
    Ok(())
}
#[test]
fn bad_input_leaves_no_target_and_yaml_errors_do_not_echo_secrets() -> Result<()> {
    let tmp = tempdir()?;
    let src = tmp.path().join("source");
    input(&src)?;
    let dest = tmp.path().join("candidate");
    let extra = fs::read_to_string(src.join("database.db"))?;
    fs::write(src.join("database.db"), format!("{extra}{extra}"))?;
    assert!(!run(&[Path::new("stage"), &src, &dest])?.status.success());
    assert!(!dest.exists());
    fs::write(src.join("configuration.yaml"), format!("mqtt: [{CANARY}"))?;
    let bad = run(&[Path::new("inspect"), &src])?;
    assert!(!bad.status.success());
    assert!(!String::from_utf8_lossy(&bad.stderr).contains(CANARY));
    Ok(())
}

#[test]
fn missing_backup_and_unrecognized_expectations_are_explicit() -> Result<()> {
    let tmp = tempdir()?;
    let src = tmp.path().join("src");
    fs::create_dir(&src)?;
    fs::write(src.join("configuration.yaml"), "{}\n")?;
    let coordinator = json!({"type":"Coordinator","ieeeAddr":format!("0x{IEEE:016x}"),
        "endpoints":{"1":{"epId":1,"binds":[{"cluster":6,"type":"group","groupID":123},{"type":"future"}],
        "configuredReportings":[{"cluster":6,"dir":0}]}}});
    fs::write(src.join("database.db"), coordinator.to_string())?;
    let dest = tmp.path().join("stage");
    let result = run(&[Path::new("stage"), &src, &dest])?;
    assert!(result.status.success());
    let report: Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(report["coordinator_backup_absent_from_snapshot"], 1);
    assert_eq!(report["binding_expectations_saved"], 1);
    assert_eq!(report["unrecognized_binding_expectations"], 1);
    assert_eq!(report["unrecognized_reporting_expectations"], 1);
    let notes: Value =
        serde_json::from_slice(&fs::read(dest.join("reconciliation/expectations.json"))?)?;
    assert_eq!(notes["bindings"][0]["source_role"], "Coordinator");
    assert_eq!(
        notes["bindings"][0]["target"],
        json!({"type":"group","group":123})
    );
    Ok(())
}

#[test]
fn adapter_preflight_checks_the_deployed_protocol_without_a_radio() {
    let tmp = tempfile::tempdir().unwrap();
    let info = tmp.path().join("info.json");
    for (ezsp, accepted) in [(13, true), (19, true), (18, false)] {
        std::fs::write(
            &info,
            format!(r#"{{"coordinator":{{"type":"EmberZNet","meta":{{"ezsp":{ezsp}}}}}}}"#),
        )
        .unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_z2m_migrate"))
            .arg("check-adapter")
            .arg(&info)
            .output()
            .unwrap();
        assert_eq!(out.status.success(), accepted);
    }
}

#[test]
fn temperature_override_survives_stage_and_only_changes_the_named_sensor() -> Result<()> {
    let tmp = tempdir()?;
    let source = tmp.path().join("source");
    input(&source)?;
    let path = source.join("database.db");
    let mut rows: Vec<Value> = fs::read_to_string(&path)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    for ep in ["1", "2"] {
        rows[1]["endpoints"][ep]["inClusterList"] = json!([0, 0x0402, 0x0405]);
    }
    fs::write(
        path,
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )?;
    for (suffix, force) in [("yes", true), ("no", false)] {
        let dest = tmp.path().join(suffix);
        fs::write(
            source.join("second.yaml"),
            format!(
                "'0x{IEEE:016x}':\n  friendly_name: Probe\n  homeassistant:\n    temperature:\n      force_update: {force}\n      unknown_future_field: {CANARY}\n    humidity:\n      force_update: true\n"
            ),
        )?;
        let result = run(&[Path::new("stage"), &source, &dest])?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: Value = serde_json::from_slice(&result.stdout)?;
        assert_eq!(report["temperature_force_update_overrides_imported"], 1);
        assert_eq!(report["device_homeassistant_overrides_not_imported"], 1);
        let rendered = Store::inspect(&dest, |registry| {
            let mut bindings = Bindings::default();
            bindings.load_device_options(registry.devices());
            let device = registry.devices().next().unwrap();
            assert_eq!(device.temperature_force_update, Some(force));
            bindings.add(
                &InterviewReport::open(&dest.join(format!("members/{IEEE:016x}.member")))?,
                &BTreeMap::from([(IEEE, device.name.clone())]),
            )?;
            bindings.discovery_snapshot("bridge", "ha", None)
        })?;
        let options: Vec<_> = rendered
            .iter()
            .filter(|(_, v)| v.get("force_update").is_some())
            .collect();
        assert_eq!(options.len(), 1);
        assert!(options[0].0.ends_with("/temperature/config"));
        assert_eq!(options[0].1["force_update"], force);
        assert_eq!(options[0].1["state_topic"], "bridge/bridge/feedback/Probe/temperature");
        assert!(!serde_json::to_string(&rendered)?.contains(CANARY));
    }
    Ok(())
}

#[test]
fn migration_restores_sensor_routes_without_inventing_measurements() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("configuration.yaml"), "advanced:\n  last_seen: ISO_8601\n")?;
    let record = |attr, manufacturer| json!({"cluster":1,"attrId":attr,"minRepIntval":30,"maxRepIntval":600,"manufacturerCode":manufacturer});
    let endpoint = json!({"epId":11,"profId":260,"devId":770,"inClusterList":[0,1,1026,1029],
        "outClusterList":[],"configuredReportings":[record(33,None), record(32,Some(1234))]});
    let member = json!({"type":"EndDevice","ieeeAddr":format!("0x{IEEE:016x}"),"interviewState":"SUCCESSFUL","nwkAddr":1234,"endpoints":{"11":endpoint},"epList":[11]});
    fs::write(source.join("database.db"), member.to_string())?;
    let dest = temp.path().join("candidate");
    let stage = run(&[Path::new("stage"), &source, &dest])?;
    assert!(stage.status.success(), "{}", String::from_utf8_lossy(&stage.stderr));
    let report = InterviewReport::open(&dest.join(format!("members/{IEEE:016x}.member")))?;
    assert!(!report.entries.iter().any(|r| matches!(r.observation, Observation::Attribute { cluster: 1|1026|1029, .. })));
    let names = BTreeMap::new();
    let mut members = Bindings::default();
    members.load_last_seen(&dest)?;
    members.add(&report, &names)?;
    let configs = members.discovery_snapshot("zb", "homeassistant", None)?;
    for property in ["battery","temperature","humidity","last_seen"] {
        let config = &configs[&format!("homeassistant/sensor/0x{IEEE:016x}/{property}/config")];
        assert_eq!(config["state_topic"], format!("zb/bridge/feedback/0x{IEEE:016x}/{property}"));
        if property == "last_seen" { assert_eq!(config["device_class"], "timestamp"); }
    }
    assert!(!configs.keys().any(|t| t.contains("/voltage/")));
    Ok(())
}
