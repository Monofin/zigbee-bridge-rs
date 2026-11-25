#[path = "../src/bin/zigbee-discovery/command.rs"]
mod command;

fn main() -> anyhow::Result<()> {
    command::run()
}
