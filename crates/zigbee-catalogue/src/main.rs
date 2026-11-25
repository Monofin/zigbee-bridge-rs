use anyhow::{Result, bail};
use std::env;
use zigbee_catalogue::{build, converters, fingerprints, identify, merged, query};

fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, dest, zha, deconz, z2m] if cmd == "build" => {
            build(dest.as_ref(), [zha.as_ref(), deconz.as_ref(), z2m.as_ref()])
        }
        [cmd, file] if cmd == "z2m-converters" => converters(file.as_ref()),
        [cmd, file] if cmd == "merged" => merged(file.as_ref(), None),
        [cmd, file, model] if cmd == "merged" => merged(file.as_ref(), Some(model)),
        [cmd, file, input] if cmd == "fingerprint" => fingerprints(file.as_ref(), input.as_ref()),
        [cmd, file, input] if cmd == "identify" => {
            let evidence = serde_json::from_reader(std::fs::File::open(input)?)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&identify(file.as_ref(), &evidence)?)?
            );
            Ok(())
        }
        [cmd, file] if cmd == "stats" => query(file.as_ref(), None, false),
        [cmd, file, model] if cmd == "model" => query(file.as_ref(), Some(model), false),
        [cmd, file, model, flag] if cmd == "model" && flag == "--full" => {
            query(file.as_ref(), Some(model), true)
        }
        _ => bail!(
            "build NEW_DB ZHA_JSONL DECONZ_JSONL Z2M_JSONL | identify DB INTERVIEW.json | fingerprint DB INTERVIEW.json | stats DB | merged DB [WIRE_MODEL] | z2m-converters DB | model DB WIRE_MODEL [--full]"
        ),
    }
}
