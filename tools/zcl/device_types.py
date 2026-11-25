#!/usr/bin/env python3
"""Generate the application device type catalogue from the verified SDK package."""
import argparse
from collections import defaultdict
import json
from pathlib import Path

from generate import verified_files


# These are present in the SDK tool's template menu. Their declared numbers
# collide with standard types or profiles. Keep them searchable as SDK evidence,
# outside the on-air standards lookup.
EXCLUDED = {
    ("cba-devices.xml", "CBA-onofflight"): "CBA template declares Smart Energy profile 0x0109",
    ("cba-devices.xml", "BACnet Tunneled Device"): "CBA template uses the Constructed BACnet device ID",
    ("green-power-devices.xml", "GP-test-device"): "test template shares GP Combo Basic ID",
    ("wwah-silabs-devices.xml", "SL-WWAH-door-lock"): "vendor example reuses HA On/Off Switch ID",
}
ROLES = {
    ("true", "true"): "Required",
    ("true", "false"): "OptionalDefaultOn",
    ("false", "false"): "Optional",
    ("false", "true"): "Excluded",
}
NORMALIZED_ZLL = 0xC05E
# Device ID only: ZigBee Alliance HA profile 05-3520-29, Table 5.1.
MINI_SPLIT_AC = (0x0104, 0x0307)


# ZigBee Building Automation 053516r12 Table 2 assigns these IDs in profile
# 0x0105. Reuse HA names only where both source tables independently agree.
ZBA_BASE = {
    0x000a: "Constructed BACnet Device", 0x000b: "BACnet Tunneled Device",
    0x0108: "On/Off Ballast", 0x0109: "Dimmable Ballast", 0x0307: "Humidity Sensor",
}
ZBA_SHARED_IDS = (*range(0x000a), *range(0x0100, 0x0108), 0x0200, 0x0201,
                  *range(0x0300, 0x0307), *range(0x0400, 0x0404))


def name_key(name):
    return " ".join(name.lower().split())


def catalogue(root):
    files = verified_files(root, False)
    cluster_ids = {}
    for doc in files.values():
        for cluster in doc.findall("cluster"):
            name = name_key(cluster.findtext("name"))
            code = int(cluster.findtext("code"), 0)
            if name in cluster_ids and cluster_ids[name] != code:
                raise ValueError(f"ambiguous cluster identifier: {name}")
            cluster_ids[name] = code

    records = []
    exceptions = set()
    for filename, doc in files.items():
        for template in doc.findall("deviceType"):
            key = template.findtext("name")
            raw_profile = int(template.findtext("profileId"), 0)
            profile = NORMALIZED_ZLL if filename == "zll-devices.xml" else raw_profile
            clusters = defaultdict(set)
            for inc in template.findall("clusters/include"):
                name = name_key(inc.get("cluster") or inc.text)
                roles = tuple(ROLES[inc.attrib[s], inc.attrib[s+"Locked"]] for s in ("server", "client"))
                clusters[cluster_ids[name]].add(roles)
            rules = []
            for code, claims in sorted(clusters.items()):
                server, client = zip(*claims)
                rule = [c[0] if len(set(c)) == 1 else "Conflicting" for c in (server, client)]
                rules.append((code, *rule))
            excluded = EXCLUDED.get((filename, key))
            if excluded:
                exceptions.add((filename, key))
            records.append(dict(profile=profile, device=int(template.findtext("deviceId"), 0),
                                name=template.findtext("typeName"), template=key, source=filename,
                                source_profile=raw_profile, clusters=rules, excluded=excluded))
    # ZLL PICS 14-0063-01 §8.1.4 PI1 explicitly admits either profile in
    # the endpoint descriptor. A common-profile identifier can be ambiguous.
    records.extend([dict(r, profile=r["source_profile"]) for r in records
                    if r["source"] == "zll-devices.xml"])
    if exceptions != EXCLUDED.keys():
        raise ValueError("SDK exceptions differ from reviewed source")
    if any((r["profile"], r["device"]) == MINI_SPLIT_AC for r in records):
        raise ValueError("the SDK now includes Mini Split AC; review the supplemental identifier")
    records.append(dict(profile=MINI_SPLIT_AC[0], device=MINI_SPLIT_AC[1], name="Mini Split AC",
                        template="", source="HA 05-3520-29 Table 5.1", source_profile=MINI_SPLIT_AC[0],
                        clusters=[], excluded=None))
    ha_names = {r["device"]: r["name"].removeprefix("HA ") for r in records if r["source"] == "ha-devices.xml"}
    zba_names = ZBA_BASE | {k: ha_names[k] for k in ZBA_SHARED_IDS}
    onoff = next(r for r in records if r["template"] == "CBA-onofflight")
    records.append(dict(onoff, profile=0x0105, excluded=None))
    indexed = {r["device"] for r in records if r["profile"] == 0x0105 and not r["excluded"]}
    for code, name in zba_names.items():
        if code not in indexed:
            records.append(dict(profile=0x0105, device=code, name=name, template="",
                                source="ZBA 053516r12 Table 2", source_profile=0x0105,
                                clusters=[], excluded=None))
    return sorted(records, key=lambda r: (r["profile"], r["device"], r["source"], r["template"]))


def render(rows):
    out = [
        "// @generated by tools/zcl/device_types.py from the locked Silicon Labs Gecko SDK.",
        "// Copyright (c) 2008,2020 Silicon Labs. SPDX-License-Identifier: Apache-2.0",
        "// Modified to index profile-scoped device types and preserve per-role template flags.",
        "use Role::{Conflicting, Excluded, Optional, OptionalDefaultOn, Required};", "",
    ]
    # Many variants share the exact same role table.
    table_ids = {}
    for row in rows:
        table = tuple(row["clusters"])
        if table not in table_ids:
            table_ids[table] = len(table_ids)
            out.append(f"const CLUSTERS_{table_ids[table]}: &[Cluster] = &[")
            for id_, server, client in table:
                out.append(f"    Cluster {{ id: 0x{id_:04x}, server: {server}, client: {client} }},")
            out.extend(["];", ""])
    for field, excluded in [("ALL", False), ("SDK_EXCEPTIONS", True)]:
        out.append(f"pub static {field}: &[DeviceType] = &[")
        for r in rows:
            if bool(r["excluded"]) != excluded:
                continue
            out.extend(["    DeviceType {", f'        profile: 0x{r["profile"]:04x}, device: 0x{r["device"]:04x},',
                        f'        name: {json.dumps(r["name"])}, template: {json.dumps(r["template"])},',
                        f'        source: {json.dumps(r["source"])}, source_profile: 0x{r["source_profile"]:04x},',
                        f'        clusters: CLUSTERS_{table_ids[tuple(r["clusters"])]},', '    },'])
        out.extend(["];", ""])
    return "\n".join(out)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parents[2] /
                        "crates/zigbee-zcl/src/device_type/generated.rs")
    args = parser.parse_args()
    args.output.write_text(render(catalogue(args.source)))
