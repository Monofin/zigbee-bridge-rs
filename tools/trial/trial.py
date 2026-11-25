"""Stage, run or roll back one systemd-supervised Z2M handover."""
import base64
import contextlib
import fcntl
import json
import itertools
import multiprocessing
import os
import socket
from pathlib import Path
import subprocess
import sys
import time

from broker import Broker
from discovery_parity import compare, snapshot
from stage import check, json_file, prepare

ROOT = Path(os.environ["BRIDGE_TRIAL_ROOT"]).resolve()
BIN = Path(__file__).resolve().parent / "bin"
Z2M = os.environ.get("BRIDGE_Z2M_CONTAINER", "zigbee2mqtt")
MARKER = ROOT / "handover.json"


def running():
    return subprocess.check_output(["docker", "inspect", "-f", "{{.State.Running}}", Z2M], text=True).strip() == "true"


def audit(run):
    broker = Broker(run)
    try:
        baseline, info = broker.capture()
        second, _ = broker.capture()
        check(baseline == second, "retained discovery changed; repeat the capture")
    finally:
        broker.close()
    ieee = json.loads((run / "deployment.json").read_bytes())["coordinator"]
    co = info["coordinator"]
    check(co["ieee_address"].lower().removeprefix("0x") == ieee and co["type"] == "EmberZNet",
          "online coordinator differs from snapshot")
    network = info.get("network", {})
    backup = json.loads((run / "source/coordinator_backup.json").read_bytes())
    ext = bytes.fromhex(backup["extended_pan_id"])
    check(network.get("channel") == backup["channel"] and network.get("pan_id") == int(backup["pan_id"], 16)
          and network.get("extended_pan_id", "").lower().removeprefix("0x") == ext[::-1].hex(),
          "online channel or PAN differs from the saved network")
    # Retain only the adapter fields consumed by check-adapter.
    json_file(run / "adapter.json", {"coordinator": {"type": co["type"], "meta": {"ezsp":co["meta"]["ezsp"]}}})
    subprocess.run([BIN / "z2m_migrate", "check-adapter", run / "adapter.json"], check=True)
    check(info.get("permit_join") is False, "close pairing before staging the trial")
    json_file(run / "original-retained.json", baseline)
    original = run / "original.jsonl"
    original.write_text("".join(json.dumps({"topic": t, "payload": json.loads(base64.b64decode(b))}) + "\n" for t,b in baseline.items()))
    pair = [snapshot(run / name, broker.prefix) for name in ("original.jsonl", "candidate.jsonl")]
    summary, details = compare(*pair)
    json_file(run / "discovery-comparison.json", {"summary": summary, "differences": details})
    print("Discovery", json.dumps(summary), flush=True)
    return summary


def stage():
    r = prepare(ROOT, Z2M, BIN)
    audit(r)
    # Only a completely verified stage becomes the visible standby.
    tmp = ROOT / "standby.next"
    tmp.unlink(missing_ok=True)
    tmp.symlink_to(r.relative_to(ROOT))
    tmp.replace(ROOT / "standby")
    return r


def marker(run):
    with MARKER.open("x") as f:
        json.dump({"run": str(run)}, f)
        f.flush()
        os.fsync(f.fileno())
    fd = os.open(ROOT, os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def restore(run, *, verify_only=False):
    baseline = json.loads((run / "original-retained.json").read_bytes())
    # Only remove topics actually owned by the candidate (including fresh interviews).
    owned = {json.loads(l)["topic"] for l in (run / "candidate.jsonl").read_text().splitlines()}
    ledger = run / "runtime-state/discovery-topics.json"
    if ledger.exists():
        for members in json.loads(ledger.read_bytes()).values():
            for topics in members.values():
                owned.update(topics)
    broker = Broker(run)
    try:
        current, _ = broker.capture()
        actual = current
        if not verify_only:
            deletes = ((t, b"") for t in sorted((owned & current.keys()) - baseline.keys()))
            writes = ((t, base64.b64decode(v)) for t, v in baseline.items() if current.get(t) != v)
            broker.publish_retained_batch(itertools.chain(deletes, writes))
            actual, _ = broker.capture()
        check(all(actual.get(t) == b for t,b in baseline.items()) and
              not (owned & actual.keys() - baseline.keys()), "discovery restore readback differs")
    finally:
        broker.close()


def health(run, candidate, process=None):
    until = time.monotonic() + 55
    while True:
        if process is not None:
            check(process.poll() is None, "candidate stopped before reporting healthy")
        try:
            with contextlib.closing(Broker(run)) as broker:
                broker.health(candidate, 3)
            return
        except (OSError, ValueError):
            if time.monotonic() >= until:
                raise
            time.sleep(1)


def recover():
    if not MARKER.exists():
        return
    run = Path(json.loads(MARKER.read_bytes())["run"])
    check(run.is_relative_to(ROOT / "runs"), "invalid rollback directory")
    expected = json.loads((run / "deployment.json").read_bytes())["container_id"]
    actual = subprocess.check_output(["docker", "inspect", "-f", "{{.Id}}", Z2M], text=True).strip()
    check(actual == expected, "Z2M container was replaced since handover; inspect before recovery")
    verified = False
    if not running():
        # Kill and join the exclusive writer before handing its topics back to Z2M.
        try:
            repair = multiprocessing.get_context("spawn").Process(target=restore, args=(run,))
            repair.start()
            try:
                repair.join(15)
                verified = repair.exitcode == 0
            finally:
                if repair.is_alive():
                    repair.kill()
                repair.join()
                repair.close()
            if not verified:
                print("Discovery repair pending; restoring radio ownership", flush=True)
        finally:
            if not running():
                subprocess.run(["docker", "start", Z2M], check=True)
    health(run, False)
    if not verified:
        # Restarted Z2M may have republished. Never overwrite its active discovery.
        restore(run, verify_only=True)
    MARKER.unlink()


def diagnostic_move(change, base):
    old, new = change["old"], change["new"]
    if change["fields"] != ["routes"] or old["domain"] != "sensor":
        return False
    owners = old["device_ids"]
    if len(owners) != 1 or not owners[0].startswith("zigbee2mqtt_"):
        return False
    ieee = owners[0].removeprefix("zigbee2mqtt_")
    if old["unique_id"] not in {f"{ieee}_{key}_{base}" for key in ("linkquality", "last_seen")}:
        return False
    topic = old["routes"].get("state_topic", "")
    return (topic.startswith(base + "/") and old["routes"] == {"state_topic": topic}
            and new["routes"] == {"state_topic": base + "/bridge/diagnostics/" + topic[len(base) + 1:]})


def serve():
    check(running() and not MARKER.exists(), "original service must be running and no rollback pending")
    run = stage()
    original = json.loads((run / "discovery-comparison.json").read_bytes())["summary"]
    check(not any(original[k] for k in ("old_conflicts","new_conflicts","reassigned_discovery_slots")), "discovery identity conflict")
    differences = json.loads((run / "discovery-comparison.json").read_bytes())["differences"]
    base = json.loads((run / "broker.json").read_bytes())["base_topic"]
    for entry in differences["missing"] + [entry["old"] for entry in differences["changed"] if not diagnostic_move(entry, base)]:
        check(entry["device_ids"] and all(owner.startswith("zigbee2mqtt_bridge_") for owner in entry["device_ids"]),
              "household device discovery differs; inspect comparison before switching")
    marker(run)
    subprocess.run(["docker","stop","--timeout","50",Z2M], check=True)
    stopped = json.loads(subprocess.check_output(["docker", "inspect", "-f", "{{json .State}}", Z2M]))
    check(not stopped["Running"] and not stopped["OOMKilled"] and stopped["ExitCode"] in (0, 143),
          "Z2M did not stop cleanly; return to the original service")
    # The stopped source is authoritative; re-check membership and config after final writes.
    frozen = prepare(ROOT, Z2M, BIN)
    check((run / "candidate.jsonl").read_bytes() == (frozen / "candidate.jsonl").read_bytes() and
          (run / "broker.json").read_bytes() == (frozen / "broker.json").read_bytes() and
          (run / "deployment.json").read_bytes() == (frozen / "deployment.json").read_bytes() and
          (run / "source/configuration.yaml").read_bytes() == (frozen / "source/configuration.yaml").read_bytes(),
          "configuration or discovery changed at handover; restage")
    args = json.loads((frozen / "listener.json").read_bytes())
    # Use the stopped-state import for the session, and keep its ownership ledger for rollback.
    (run / "runtime-state").symlink_to(frozen / "state")
    device = Path(args[1]).resolve(strict=True)
    check(not running(), "original service resumed")
    probe = subprocess.run(["fuser", str(device)], capture_output=True)
    check(probe.returncode == 1 and not probe.stdout and not probe.stderr, "serial port has an owner or cannot be checked")
    proc = subprocess.Popen([BIN / "zigbee-bridge", *args])
    try:
        health(frozen, True, proc)
        with contextlib.closing(Broker(run)) as broker:
            for entry in differences["missing"]:
                broker.publish(entry["topic"], b"", retain=True)
        if notify := os.environ.get("NOTIFY_SOCKET"):
            addr = chr(0) + notify[1:] if notify.startswith("@") else notify
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as sock:
                sock.sendto(b"READY=1", addr)
        print("Candidate confirmed healthy; pairing closed at startup", flush=True)
        check(proc.wait() == 0, "candidate stopped unexpectedly")
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=45)


def main():
    os.umask(0o077)
    action, = sys.argv[1:]
    with (ROOT / "trial.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if action == "stage":
            check(running() and not MARKER.exists(), "stage requires an active Z2M owner")
            print(stage().name)
        elif action == "serve":
            serve()
        elif action == "recover":
            recover()
        else:
            raise ValueError("expected stage, serve or recover")


if __name__ == "__main__":
    main()
