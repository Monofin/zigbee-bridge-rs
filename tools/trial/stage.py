"""Prepare a private, offline standby from a Docker-hosted Ember Z2M installation."""
import hashlib
import json
import subprocess
import shutil
import tempfile
from pathlib import Path
from urllib.parse import urlsplit

import yaml


def check(ok, message):
    if not ok:
        raise ValueError(message)


def json_file(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def yaml_file(data):
    try:
        return yaml.safe_load(data)
    except yaml.YAMLError:
        raise ValueError("invalid YAML in migration source") from None


def capture(source, destination):
    # Compare all input bytes on both sides of the copy. A cut repeats this
    # procedure after Z2M has stopped and any final database write has finished.
    config = (source / "configuration.yaml").read_bytes()
    parsed = yaml_file(config)
    refs = ["configuration.yaml", "database.db", "coordinator_backup.json"]
    for key in ("groups", "devices"):
        value = parsed.get(key)
        if isinstance(value, str):
            refs.append(value)
        elif isinstance(value, list):
            refs.extend(value)
    for key in ("stack_config.json", "state.json"):
        if (source / key).exists():
            refs.append(key)
    files = {}
    for filename in refs:
        path = Path(filename)
        check(not path.is_absolute() and ".." not in path.parts, "invalid split configuration path")
        src = source / path
        check(not src.is_symlink() and src.is_file() and src.resolve().is_relative_to(source), "invalid source file")
        data = src.read_bytes()
        dest = destination / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)
        files[filename] = hashlib.sha256(data).hexdigest()
    check(config == (destination / "configuration.yaml").read_bytes() and
          all(hashlib.sha256((source / k).read_bytes()).hexdigest() == v for k, v in files.items()),
          "Z2M changed during the snapshot; retry staging")
    return parsed, files


def prepare(root, container, bindir):
    meta, = json.loads(subprocess.check_output(["docker", "inspect", container]))
    check(not any(x.startswith("ZIGBEE2MQTT_CONFIG_") for x in meta["Config"]["Env"]), "Z2M environment overrides require review")
    source, = [Path(m["Source"]).resolve() for m in meta["Mounts"] if m["Destination"] == "/app/data"]
    run = Path(tempfile.mkdtemp(dir=root / "runs", prefix="stage-"))
    try:
        return prepare_in(run, source, meta, bindir)
    except Exception:
        shutil.rmtree(run)
        raise


def prepare_in(run, source, meta, bindir):
    snap = run / "source"
    snap.mkdir()
    conf, manifest = capture(source, snap)
    json_file(run / "source-sha256.json", manifest)
    backup = json.loads((snap / "coordinator_backup.json").read_bytes())
    adv, serial, mqtt = (conf[k] for k in ("advanced", "serial", "mqtt"))
    check(serial["adapter"] == "ember" and backup["metadata"]["format"] == "zigpy/open-coordinator-backup",
          "expected Ember with an Open Coordinator Backup")
    cap = bytes(adv["ext_pan_id"])
    check(cap == bytes.fromhex(backup["extended_pan_id"]) and len(cap) == 8 and
          adv["channel"] == backup["channel"] and adv["pan_id"] == int(backup["pan_id"], 16) and
          bytes(adv["network_key"]) == bytes.fromhex(backup["network_key"]["key"]), "configuration and backup disagree")
    rows = [json.loads(line) for line in (snap / "database.db").read_text().splitlines() if line.strip()]
    coordinator, = [r for r in rows if r.get("type") == "Coordinator"]
    ieee = coordinator["ieeeAddr"].lower().removeprefix("0x")
    check(ieee == bytes.fromhex(backup["coordinator_ieee"])[::-1].hex(), "database and coordinator backup disagree")
    port, = [v["PathOnHost"] for v in meta["HostConfig"]["Devices"] if v["PathInContainer"] == serial["port"]]
    url = urlsplit(mqtt["server"])
    check(url.scheme in ("mqtt", "mqtts") and url.hostname and not url.username and not url.password and
          url.path in ("", "/") and not url.query and not url.fragment, "unsupported broker URL")
    check(not set(mqtt).difference({"server", "user", "password", "base_topic", "version", "client_id"}) and
          mqtt.get("version", 4) in (4, 5), "unsupported MQTT settings require review")
    ha = conf.get("homeassistant", {})
    check(ha is True or (isinstance(ha, dict) and ha.get("enabled", True)), "HA discovery is disabled")
    broker = dict(host=url.hostname, port=url.port or (8883 if url.scheme == "mqtts" else 1883),
                  tls=url.scheme == "mqtts", client_id="zigbee-bridge-trial-" + ieee,
                  base_topic=mqtt.get("base_topic", "zigbee2mqtt"),
                  discovery_prefix=ha.get("discovery_topic", "homeassistant") if isinstance(ha, dict) else "homeassistant")
    if "user" in mqtt or "password" in mqtt:
        check(all(isinstance(mqtt.get(k), str) and mqtt[k] for k in ("user", "password")), "unsupported broker credentials")
        broker.update(username=mqtt["user"], password_file="broker.password")
        (run / "broker.password").write_text(mqtt["password"])
    json_file(run / "broker.json", broker)
    state = run / "state"
    for mode, extra in (("inspect", []), ("stage", [state])):
        with (run / (mode + ".json")).open("w") as out:
            subprocess.run([bindir / "z2m_migrate", mode, snap, *extra], stdout=out, check=True)
    check((run / "inspect.json").read_bytes() == (run / "stage.json").read_bytes(), "import counts differ")
    by_ieee = {int(r["ieeeAddr"], 16): r for r in rows if r.get("ieeeAddr")}
    members = json.loads((state / "devices.json").read_bytes())["devices"]
    check(all(d["ieee"] in by_ieee and d["short"] is None for d in members), "membership or addressing differs")
    if isinstance(conf.get("devices"), dict):
        opts = {int(k, 16): v for k, v in conf["devices"].items()}
        check(all(d["name"] == (opts.get(d["ieee"]) or {}).get("friendly_name", f"0x{d['ieee']:016x}")
                  for d in members), "imported friendly names differ")
    # The Ember API accepts the config list in wire order. The Rust CLI parses
    # the argument as a u64 then uses to_le_bytes(), hence little here too.
    args = ["listen", port, str(state), str(adv["channel"]), format(adv["pan_id"], "04x"),
            format(int.from_bytes(cap, "little"), "016x"), "--baud", str(serial.get("baudrate", 115200)),
            "--flow", "rtscts" if serial.get("rtscts") else "none", "--mqtt-config", str(run / "broker.json")]
    if "transmit_power" in adv:
        args += ["--tx-power", str(adv["transmit_power"])]
    if (snap / "stack_config.json").exists():
        stack = json.loads((snap / "stack_config.json").read_bytes())
        check(not set(stack).difference({"MAX_END_DEVICE_CHILDREN"}), "unsupported stack overrides require review")
        if "MAX_END_DEVICE_CHILDREN" in stack:
            args += ["--max-children", str(stack["MAX_END_DEVICE_CHILDREN"])]
    json_file(run / "listener.json", args)
    with (run / "candidate.jsonl").open("w") as out:
        subprocess.run([bindir / "zigbee-discovery", state, broker["base_topic"], broker["discovery_prefix"],
                        "--coordinator-ieee", ieee, "--jsonl"], stdout=out, check=True)
    json_file(run / "owners.json", sorted({r["ieeeAddr"].lower() for r in rows if r.get("ieeeAddr")}))
    json_file(run / "deployment.json", dict(container_id=meta["Id"], image=meta["Image"], coordinator=ieee,
              host_port=port, transmit_power_config=adv.get("transmit_power")))
    return run
