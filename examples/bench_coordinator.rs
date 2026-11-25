// Compatibility entry point for existing bench scripts.
#[path = "../src/bin/zigbee-bridge/command.rs"]
mod command;

fn main() -> anyhow::Result<()> {
    command::run()
}
