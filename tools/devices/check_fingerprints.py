#!/usr/bin/env python3
"""Regression check for the pinned listings: check_fingerprints.py BINARY DB MERGED_JSON."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def check(lookup, merged):
    records = merged["records"]
    ids = merged["unkeyed"]
    assert {s: len(v) for s, v in ids.items()} == {"zha": 13, "z2m": 5}
    for group in ids.values():
        for ref in group:
            rules = records[str(ref)]["fingerprints"]
            assert rules
            for rule in rules:
                if "sourceIDRange" in rule:
                    low, high = rule["sourceIDRange"]
                    def matches(source):
                        return ref in lookup({"greenPowerDeviceID": rule["greenPowerDeviceID"],
                                              "sourceID": source})
                    assert matches(low) and matches(high)
                    assert not matches(low - 1) and not matches(high + 1)
                else:
                    assert ref in lookup(rule), (ref, rule)

    # Independent on-wire descriptors. Reverse the endpoint/cluster order so
    # this also checks set matching. Firmware 12 and 13 share one signature.
    entry = merged["models"]["ZLinky_TIC"]["candidates"]
    assert {s: len(v) for s, v in entry.items()} == {"zha": 4, "deconz": 5, "z2m": 1}
    ep = {"id": 1, "profile": 260, "device_type": 0x53,
          "servers": [0xff66, 0xb04, 0xb01, 0x702, 3, 0], "clients": [25]}
    gp = {"id": 242, "profile": 0xa1e0, "device_type": 0x61, "servers": [33], "clients": [33]}
    observed = {"manufacturer": "LiXee", "model": "ZLinky_TIC", "endpoints": [gp, ep]}
    def expect(symbol):
        assert [records[str(i)]["symbol"] for i in lookup(observed)] == [symbol], symbol

    expect("ZLinkyTIC")
    ep["servers"].append(1)
    expect("ZLinkyTICFWV12")
    ep["servers"].extend([10, 0xef00])
    assert not lookup(observed)  # Both server and client evidence are required.
    ep["clients"].append(0xef00)
    expect("ZLinkyTICFWV14")
    ep["device_type"] = 0x0101
    expect("ZLinkyTICFWV15")
    assert not lookup({**observed, "manufacturer": "Other"})
    assert not lookup({**observed, "endpoints": [ep]})
    del ep["device_type"]
    assert not lookup(observed)

    # Both sources legitimately describe this interview. Return both references.
    xbee = {"manufacturerID": 4126, "type": "Router", "endpoints": [
        {"id": ep, "profile": 0xc105, "device_type": 1, "servers": [], "clients": []}
        for ep in (230, 232)]}
    assert {records[str(i)]["source"] for i in lookup(xbee)} == {"zha", "z2m"}
    del xbee["endpoints"][0]["clients"]
    assert not lookup(xbee)  # Unknown differs from an observed empty cluster list.


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("binary", "database", "merged"):
        parser.add_argument(arg, type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as temp:
        probe = Path(temp) / "interview.json"
        def lookup(data):
            probe.write_text(json.dumps(data))
            out = subprocess.check_output([args.binary.resolve(), "fingerprint", args.database, probe])
            return [json.loads(line)["record"] for line in out.splitlines()]
        check(lookup, json.loads(args.merged.read_bytes()))
    print("All 18 model-less records and all four ZHA ZLinky signatures verified.")
