//! Run the Ember coordinator or inspect an interview.
#[cfg(unix)]
#[path = "catalogue.rs"]
mod catalogue;

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
pub async fn run() -> anyhow::Result<()> {
    use anyhow::{Result, bail, ensure};
    use std::{env, time::Duration};
    use tokio::signal::unix::{SignalKind, signal};
    use zigbee_bridge_rs::{
        ash::Ezsp,
        coordinator::{self, Network},
        serial::Serial,
        session::Startup,
        store::Store,
    };
    let mut args: Vec<_> = env::args().skip(1).collect();
    let mut database = None;
    let mut light_index = None;
    let mut standard_reports = None;
    let mut ota_manifest = None;
    let mut lock_credentials = None;
    let mut tuya_internet = None;
    let mut reports = Vec::new();
    let mut mqtt_port = None;
    let mut mqtt_config=None;
    let mut children = None;
    let mut tx_dbm = None;
    let mut baud=None;
    let mut flow=None;
    while args.len() >= 2 {
        let value = args.last().unwrap();
        match args[args.len()-2].as_str() {
            "--standard-reports" if standard_reports.is_none() => {
                standard_reports=Some(match value.as_str(){
                    "all"=>true,
                    "unmapped"=>false,
                    _=>bail!("expected --standard-reports unmapped or all"),
                });
            }
            "--max-children" if children.is_none() => children = Some(value.parse::<u8>()?),
            "--tx-power" if tx_dbm.is_none() => tx_dbm = Some(value.parse::<i8>()?),
            "--baud" if baud.is_none() => {
                let rate=value.parse::<u32>()?;
                ensure!(rate!=0,"baud rate must be positive");
                baud=Some(rate);
            }
            "--flow" if flow.is_none() => {
                flow=Some(match value.as_str(){
                    "none"=>tokio_serial::FlowControl::None,
                    "xonxoff"=>tokio_serial::FlowControl::Software,
                    "rtscts"=>tokio_serial::FlowControl::Hardware,
                    _=>bail!("expected --flow none, xonxoff or rtscts"),
                });
            }
            "--tuya-internet-status" if tuya_internet.is_none() => {
                tuya_internet = Some(match value.as_str() {
                    "online" => true,
                    "offline" => false,
                    _ => bail!("expected --tuya-internet-status online or offline"),
                });
            }
            "--catalogue" if database.is_none() => database = Some(std::path::PathBuf::from(value)),
            "--light-index" if light_index.is_none() => light_index = Some(std::path::PathBuf::from(value)),
            "--lock-credentials" if lock_credentials.is_none()=>lock_credentials=Some(std::path::PathBuf::from(value)),
            "--ota-manifest" if ota_manifest.is_none()=>ota_manifest=Some(std::path::PathBuf::from(value)),
            "--device-report" => reports.push(std::path::PathBuf::from(value)),
            "--mqtt-config" if mqtt_config.is_none()=>mqtt_config=Some(std::path::PathBuf::from(value)),
            "--mqtt-local-port" if mqtt_port.is_none() => mqtt_port = Some(value.parse::<u16>()?),
            _ => break,
        }
        args.truncate(args.len()-2);
    }
    if let [mode,path] = &args[..] && mode == "report" {
        let report=zigbee_bridge_rs::interview_log::InterviewReport::open(path.as_ref())?;
        if let Some(db) = database { return catalogue::print(&report, &db); }
        println!("{}",serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let (args, target) = match &args[..] {
        [mode, _, _, _, _, _, nwk, ieee] if mode == "probe" => {
            let node = u16::from_str_radix(nwk, 16)?;
            ensure!(
                node < 0xfff8 && node != 0 && ieee.len() == 16,
                "invalid probe address"
            );
            (
                &args[..6],
                Some((node, u64::from_str_radix(ieee, 16)?.to_le_bytes())),
            )
        }
        _ => (&args[..], None),
    };
    let (mode, path, state, expected) = match args {
        [mode, path, state] if mode == "inspect" => (mode, path, state, None),
        [mode, path, state, ch, pan, ext]
            if matches!(mode.as_str(), "tc-read" | "tc-restore-deny" | "form" | "run" | "interview" | "interview-listen" | "listen") || target.is_some() =>
        {
            ensure!(
                ext.len() == 16,
                "extended PAN must be 16 hexadecimal digits"
            );
            (
                mode,
                path,
                state,
                Some(Network {
                    channel: ch.parse()?,
                    pan: u16::from_str_radix(pan, 16)?,
                    extended_pan: u64::from_str_radix(ext, 16)?.to_le_bytes(),
                }),
            )
        }
        _ => bail!(
            "usage: zigbee-bridge inspect PORT STATE_DIR | form|run|interview|interview-listen|listen PORT STATE_DIR CHANNEL PAN_HEX EXT_PAN_HEX | probe PORT STATE_DIR CHANNEL PAN_HEX EXT_PAN_HEX LAST_NWK_HEX IEEE_HEX (resets NCP)"
        ),
    };
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let store=Store::open(state.as_ref())?;
    let admits = matches!(mode.as_str(), "interview" | "interview-listen");
    let mut admissions_closed = false;
    let mut log=if admits || target.is_some() {
        let log=store.begin_interview()?;
        println!("Interview record {}",log.path().display());
        Some(log)
    } else {None};
    ensure!(mqtt_port.is_none() || mqtt_config.is_none(),"choose one broker configuration");
    let mqtt_enabled=mqtt_port.is_some() || mqtt_config.is_some();
    ensure!(!mqtt_enabled || matches!(mode.as_str(), "listen" | "interview-listen"), "MQTT requires a listener");
    ensure!(reports.is_empty() || mqtt_enabled, "device reports require MQTT");
    let names = store.registry().devices().map(|device| (device.ieee, device.name.clone())).collect();
    let mut devices = match light_index {
        Some(path) => zigbee_bridge_rs::bridge::Bindings::reviewed_lights(
            zigbee_devices::light_index::LightIndex::from_reader(std::fs::File::open(path)?)?),
        None => zigbee_bridge_rs::bridge::Bindings::default(),
    };
    if let Some(online) = tuya_internet {
        ensure!(mqtt_enabled, "Tuya gateway status requires the managed bridge");
        devices.set_tuya_gateway_internet(online);
    }
    devices.set_all_standard_reports(standard_reports.unwrap_or(false));
    devices.load_naming(state.as_ref())?;
    devices.load_last_seen(state.as_ref())?;
    devices.load_device_options(store.registry().devices());
    for path in reports {
        devices.add(&zigbee_bridge_rs::interview_log::InterviewReport::open(&path)?, &names)?;
    }
    ensure!(ota_manifest.is_none() || mqtt_enabled,"OTA manifest requires MQTT");
    let mut management=if mqtt_enabled {Some(zigbee_bridge_rs::bridge::Management::new(state.as_ref(),names.clone(),ota_manifest.as_deref())?)} else {None};
    if let Some(dir) = lock_credentials {
        management.as_mut().ok_or_else(|| anyhow::anyhow!("lock credentials require the managed bridge"))?.set_lock_credentials(&dir)?;
    }
    let mut startup = Startup::new(store);
    let mut serial = Serial::attach(
        &mut startup,
        tokio_serial::new(path, baud.unwrap_or(115200)).flow_control(flow.unwrap_or(tokio_serial::FlowControl::Software)),
    )?;
    let mut broker = if mqtt_enabled {
        use zigbee_bridge_rs::mqtt;
        let (options,base,prefix)=match mqtt_config {
            Some(path)=>mqtt::config::BrokerConfig::load(&path)?,
            None=>(mqtt::MqttOptions::new("zigbee-bridge-bench","127.0.0.1",mqtt_port.unwrap()),"zigbee2mqtt".into(),"homeassistant".into()),
        };
        let (publisher, mut worker, _) = mqtt::channel_with_prefix(options, &base, &prefix, 1024)?;
        let commands = worker.enable_reconnect(state.as_ref())?;
        worker.wait_for_device_snapshot();
        let admin=worker.enable_admin()?;
        let shutdown = publisher.clone();
        let (ready, radio) = tokio::sync::oneshot::channel();
        startup.worker(move |session| async move {
            tokio::select! {
                biased;
                _ = session.cancelled() => return Ok(()),
                opened = radio => if opened.is_err() { return Ok(()); }
            }
            let running = worker.run();
            tokio::pin!(running);
            tokio::select! {
                result = &mut running => result,
                _ = session.cancelled() => { shutdown.close(); running.await }
            }
        });
        Some((publisher,commands,admin,Some(ready)))
    } else { None };
    let controller = startup.start();
    let work = async {
        let mut link = Ezsp::connect(&mut serial, Duration::from_secs(8)).await?;
        let Some(expected) = expected else {
            let saved = coordinator::inspect(&mut link).await?;
            println!(
                "EZSP {}: stored node role {}, channel {}, PAN assigned {}",
                link.version.protocol,
                saved.role,
                saved.network.channel,
                saved.network.pan != 0xffff
            );
            return Ok(());
        };
        let startup_mode = if mode == "form" {
            coordinator::StartupMode::Form
        } else {
            coordinator::StartupMode::Resume
        };
        if mode == "interview" || target.is_some() {
            zigbee_bridge_rs::interview::register(&mut link).await?;
        }
        if mode == "listen" || mode == "interview-listen" {
            zigbee_bridge_rs::interview::register_listener(&mut link).await?;
        }
        coordinator::start_configured(&mut link, expected, startup_mode, children, tx_dbm).await?;
        println!(
            "Ready: verified coordinator 0x0000, EZSP {}, channel {}, joining closed",
            link.version.protocol, expected.channel
        );
        if mode == "tc-read" {
            coordinator::tc_key_request_policy(&mut link).await?;
            return Ok(());
        }
        if mode == "tc-restore-deny" {
            zigbee_bridge_rs::interview::clear_transient_keys(&mut link).await?;
            let value = coordinator::tc_key_request_policy(&mut link).await?;
            if value == coordinator::SEND_CURRENT_TC_KEY {
                coordinator::set_tc_key_request_policy(&mut link, coordinator::DENY_TC_KEY_REQUESTS).await?;
            } else {
                ensure!(value == coordinator::DENY_TC_KEY_REQUESTS, "unexpected rollback value");
            }
            return Ok(());
        }
        if mode == "listen" {
            if let Some((out,commands,admin,ready)) = &mut broker {
                ready.take().unwrap().send(()).map_err(|_| anyhow::anyhow!("MQTT worker stopped"))?;
                return zigbee_bridge_rs::bridge::serve_managed(&mut link,&mut devices,out,commands,management.as_mut().unwrap(),admin).await;
            }
            return zigbee_bridge_rs::interview::listen(&mut link).await;
        }
        if let Some(log)=log.as_mut() {
            return if let Some((node,ieee))=target {
                zigbee_bridge_rs::interview::probe_observed(&mut link,node,ieee,|event|log.record(event)).await
            } else {
                let receipt=zigbee_bridge_rs::interview::interview_for_admission(&mut link,|event|log.record(event)).await?;
                admissions_closed = true;
                if let Some((out,commands,admin,ready)) = &mut broker {
                    let report = zigbee_bridge_rs::interview_log::InterviewReport::open(log.path())?;
                    management.as_mut().unwrap().accept_fresh_interview(report, receipt)?;
                    ready.take().unwrap().send(()).map_err(|_| anyhow::anyhow!("MQTT worker stopped"))?;
                    zigbee_bridge_rs::bridge::serve_managed(&mut link,&mut devices,out,commands,management.as_mut().unwrap(),admin).await
                } else if mode == "interview-listen" {
                    zigbee_bridge_rs::interview::listen(&mut link).await
                } else {
                    Ok(())
                }
            };
        }
        if mode == "form" {
            return Ok(());
        }
        loop {
            let cb = link
                .next_callback()
                .await
                .map_err(|e| anyhow::anyhow!("coordinator session ended: {e:?}"))?;
            coordinator::observe(cb)?;
        }
    };
    let completion = tokio::select! {
        result=work => Some(result),
        result=controller.finished() => Some(result.and_then(|_| Err(anyhow::anyhow!("worker stopped while serving")))),
        _=term.recv()=> None,
        _=interrupt.recv()=>None,
    };
    // A signal can cancel an EZSP exchange midway. Reacquire a fresh link
    // and explicitly close admissions before releasing the serial worker.
    let result = completion.unwrap_or(Ok(()));
    let recovery: Result<()> = if let Some(network) = expected && ((admits && !admissions_closed) || management.as_ref().is_some_and(|m|m.needs_cleanup())) {
        async {
            let mut link = Ezsp::connect(&mut serial, Duration::from_secs(8)).await?;
            coordinator::start_configured(&mut link, network, coordinator::StartupMode::Resume, children, tx_dbm).await?;
            if let Some(m)=management.as_mut(){m.recover(&mut link).await}
            else{zigbee_bridge_rs::interview::clear_transient_keys(&mut link).await}
        }.await
    } else { Ok(()) };
    // Release active and queued request reservations while the broker can
    // still send their interrupted responses.
    drop(management);
    drop(broker);
    let stopped = controller.shutdown().await;
    recovery?;
    stopped?;
    if let (Some(db), Some(log)) = (database, log) {
        let lookup = zigbee_bridge_rs::interview_log::InterviewReport::open(log.path())
            .and_then(|report| catalogue::print(&report, &db));
        if let Err(error) = lookup {
            eprintln!("Catalogue lookup failed: {error:#}");
            result?;
            return Err(error);
        }
    }
    result
}

#[cfg(not(unix))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("zigbee-bridge requires Unix")
}
