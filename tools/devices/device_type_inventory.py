#!/usr/bin/env python3
"""Join declared endpoint evidence from all three pinned catalogues to application types."""
import argparse
from collections import Counter, defaultdict
import gzip
import hashlib
import json
from pathlib import Path
import sys

from zha_type_evidence import Signatures

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "zcl"))
from device_types import catalogue


def number(value):
    if type(value) is int:
        result = value
    elif isinstance(value, str):
        try:
            result = int(value, 0)
        except ValueError:
            return None
    else:
        return None
    return result if 0 <= result <= 0xffff else None


def fingerprint_groups(r, signatures):
    groups = []
    def add(origin, fp, extra=None):
        if not isinstance(fp, dict):
            raise ValueError(f"invalid {origin} fingerprint")
        groups.append({"origin": origin, "conditions": {k:v for k,v in fp.items() if k != "endpoints"},
                       **({"context": extra} if extra else {}), "endpoints": fp.get("endpoints", [])})
    if r["source"] == "deconz":
        for i, sub in enumerate(r["subdevices"]):
            if "fingerprint" in sub:
                fp = sub["fingerprint"]
                fields = {"endpoint":"ID", "profile":"profileID", "device":"deviceID",
                          "in":"inputClusters", "out":"outputClusters"}
                ep = {}
                for key, new in fields.items():
                    if key in fp:
                        value = fp[key]
                        ep[new] = [number(v) for v in value] if isinstance(value,list) else number(value)
                        if ep[new] is None or isinstance(ep[new], list) and None in ep[new]:
                            ep[new+"Expression"] = value
                add(f"subdevices[{i}].fingerprint",
                    {"endpoints": [ep], **{k:v for k,v in fp.items() if k not in fields}}, r["match"])
    else:
        for i, fp in enumerate(r.get("fingerprints", [])):
            add(f"fingerprints[{i}]", fp)
        for i, label in enumerate(r.get("white_labels", [])):
            for j, fp in enumerate(label.get("fingerprint", [])):
                add(f"white_labels[{i}].fingerprint[{j}]", fp,
                    {k:v for k,v in label.items() if k != "fingerprint"})
        if r["source"] == "zha" and not groups:
            eps, warnings = signatures.endpoints(r)
            return ([{"origin":r["kind"] + " match signature", "conditions":{}, "endpoints":eps}]
                    if eps is not None else []), warnings
    return groups, []


def unindexed_selectors(groups, known, source, row):
    """Point into the original selector, including its sibling endpoints and match conditions."""
    for selector, group in enumerate(groups):
        for endpoint, ep in enumerate(group["endpoints"]):
            pair = ep.get("profileID"), ep.get("deviceID")
            if all(type(v) is int for v in pair) and pair not in known:
                yield pair, {"source": source, "source_row": row, "selector": selector, "endpoint": endpoint}


def unindexed_evidence(profile, device):
    # Zigbee PRO 05-3474-21 §2.3.3.2.1 lists this profile range for MSPs,
    # with exceptions such as ZLL. Report observations without giving them
    # global type names from similarly numbered devices in another profile.
    if profile == 0xc05e:
        return "absent from ZLL device table; 0x1000 identifies a cluster in that namespace" if device == 0x1000 else "absent from ZLL device table"
    if 0x8000 <= profile <= 0xff00:
        return "manufacturer-specific profile range"
    if profile == 0x0104:
        if (0x0060 <= device <= 0x00ff or 0x0108 <= device <= 0x01ff
                or 0x0404 <= device <= 0xffff):
            return "reserved in HA 1.2 Table 5.1; no matching allocation found in reviewed later type tables"
        return "no matching allocation in reviewed HA, Lighting/Occupancy or ZLL type tables"
    return "profile outside reviewed public profile tables"


def inventory(args):
    lock = json.loads((HERE/"sources.lock.json").read_text())
    public = {s["source"]:s for s in lock}
    signatures = Signatures(args.zha, args.wheel, public["zha"]["revision"])
    types = catalogue(args.sdk)
    known = {(r["profile"],r["device"]) for r in types if not r["excluded"]}
    zigpy_ids = signatures.profile_type_ids()
    missing_enums = zigpy_ids.keys() - known
    if missing_enums:
        raise ValueError(f"pinned zigpy declares unindexed types: {sorted(missing_enums)}")
    examples = defaultdict(set)
    selector_refs = defaultdict(list)
    refs = defaultdict(Counter)
    result, totals = [], {}
    for src in lock:
        source = src["source"]
        raw = (args.listings/(source + ".jsonl")).read_bytes()
        if hashlib.sha256(raw).hexdigest() != src["jsonl_sha256"]:
            raise ValueError("snapshot digest mismatch: " + source)
        lines = raw.splitlines()
        if len(lines) != src["records"]:
            raise ValueError("snapshot count mismatch: " + source)
        counts = Counter()
        for i, line in enumerate(lines, start=1):
            row = json.loads(line)
            if row["revision"] != src["revision"] or row["source"] != source:
                raise ValueError("snapshot provenance mismatch")
            groups, warnings = fingerprint_groups(row, signatures)
            for pair, ref in unindexed_selectors(groups, known, source, i):
                selector_refs[pair].append(ref)
            refs_here = set()
            partial = False
            for group in groups:
                for ep in group["endpoints"]:
                    key = ep.get("profileID"), ep.get("deviceID")
                    if all(type(v) is int for v in key):
                        ep["indexed"] = key in known
                        refs_here.add(key)
                        counts["keyed_endpoint_occurrences"] += 1
                    else:
                        partial = True
                        counts["unkeyed_endpoint_occurrences"] += 1
            if refs_here:
                status = "has_profile_device"
            elif partial:
                status = "endpoint_without_complete_key"
            elif warnings:
                status = "unresolved_signature"
            else:
                status = "no_explicit_endpoint_key"
            counts[status] += 1
            counts["rows"] += 1
            if warnings:
                counts["rows_with_unresolved_source"] += 1
            for key in refs_here:
                refs[key][source] += 1
                if key not in known:
                    examples[key].add(row["path"])
            result.append({k: row[k] for k in ("source","path","symbol","line","kind","models","manufacturers")
                           if k in row} | {"source_row":i, "status":status, "selectors":groups, "unresolved":warnings})
        totals[source] = dict(counts)
    def key(k):
        return f"0x{k[0]:04x}/0x{k[1]:04x}"
    evidence = {key(k):dict(refs.get(k, {})) for k in sorted(known | refs.keys())}
    summary = {"schema":1,"sources":totals, "source_rows":len(result),
               "indexed_variants":sum(not t["excluded"] for t in types),"excluded_sdk_templates":sum(bool(t["excluded"]) for t in types),
               "indexed_keys":len(known),"indexed_keys_with_source_rows":len(known & refs.keys()),
               "pinned_zigpy_profile_enum_ids":len(zigpy_ids),
               "unindexed_keys_with_source_rows":{
                   key(k):{"rows":dict(refs[k]), "review":unindexed_evidence(*k),
                           "source_paths":sorted(examples[k]), "selector_refs":selector_refs[k]}
                   for k in sorted(refs.keys()-known)}}
    licence = {p.name:p.read_text() for p in sorted((HERE/"licenses").glob("*.txt"))}
    licpath = HERE.parents[1]/"crates/zigbee-zcl/src/cluster/LICENSE-APACHE"
    apache = licpath.read_text()
    archive = {"schema":1,"source_locks":lock,"sdk_lock":json.loads((HERE.parent/"zcl/source.lock.json").read_text()),
               "notices":licence | {"gecko_sdk": "Copyright (c) 2008,2020 Silicon Labs. Modified: indexed device types and roles.\n"+apache},
               "summary":summary,"device_types":types,"rows_per_type":evidence,"listings":result}
    args.output.mkdir(exist_ok=True,parents=True)
    with (args.output/"device-type-inventory.json.gz").open("wb") as out:
        with gzip.GzipFile(fileobj=out,filename="",mode="wb",mtime=0) as gz:
            gz.write(json.dumps(archive,sort_keys=True,separators=(",",":")).encode())
    (args.output/"device-type-summary.json").write_text(json.dumps(summary,indent=2,sort_keys=True)+"\n")
    return summary


if __name__=="__main__":
    p = argparse.ArgumentParser(description=__doc__)
    for arg in ("listings","zha","wheel","sdk","output"):
        p.add_argument("--"+arg, required=True, type=Path)
    print(json.dumps(inventory(p.parse_args()),indent=2))
