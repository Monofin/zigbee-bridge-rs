//! Reset one bench NCP and report its first EZSP VERSION response.
#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    use anyhow::{Context, bail, ensure};
    use std::{env, time::Duration};
    use tokio_serial::FlowControl;
    use zigbee_bridge_rs::{ash::probe_version, serial::Serial, session::Startup, store::Store};

    let args: Vec<_> = env::args().skip(1).collect();
    let [path, baud, flow, protocol, seconds] = &args[..] else {
        bail!(
            "Usage: ash_version PORT BAUD none|rtscts|xonxoff PREFERRED_EZSP TIMEOUT_SECONDS (resets the NCP)"
        );
    };
    let flow = match flow.as_str() {
        "none" => FlowControl::None,
        "rtscts" => FlowControl::Hardware,
        "xonxoff" => FlowControl::Software,
        _ => bail!("unknown flow control"),
    };
    let baud = baud.parse().context("invalid baud rate")?;
    let preferred = protocol.parse().context("invalid EZSP protocol number")?;
    let seconds: u32 = seconds.parse().context("invalid timeout")?;
    ensure!(seconds != 0, "timeout must be positive");
    let scratch = tempfile::tempdir()?;
    let mut startup = Startup::new(Store::open(scratch.path())?);
    let mut serial = Serial::attach(
        &mut startup,
        tokio_serial::new(path, baud).flow_control(flow),
    )?;
    let controller = startup.start();
    eprintln!("Sending an ASH software reset to {path} before probing EZSP VERSION");
    let result = probe_version(
        &mut serial,
        preferred,
        0,
        Duration::from_secs(seconds.into()),
    )
    .await;
    let shutdown = controller.shutdown().await;
    let version = result?;
    shutdown?;
    println!(
        "EZSP {} stack type {} stack version {:#06x}",
        version.protocol, version.stack_type, version.stack_version
    );
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("ash_version requires the Unix serial transport");
    std::process::exit(1);
}
