#!/usr/bin/env python3
"""Inventory declared device behaviour from the three locked source listings.

The output is an implementation worklist. Counts measure references, never
runtime compatibility. Upstream code and scripts are not executed.
"""
import argparse
import ast
from collections import defaultdict
from hashlib import sha256
import io
import json
import subprocess
import tarfile
from pathlib import Path


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def expose_tree(node, parent=()):
    # A list's item type is a schema and need not have a property of its own.
    shape = {k: v for k, v in node.items() if k not in ("features", "item_type")}
    yield {"parents": parent, **shape}
    path = (*parent, node.get("property") or node.get("name") or node["type"])
    for child in node.get("features", []):
        yield from expose_tree(child, path)
    if "item_type" in node:
        yield from expose_tree(node["item_type"], (*path, "[]"))


def z2m_items(row):
    for expose in row["exposes"]:
        for shape in expose_tree(expose):
            yield "expose", shape
    for direction in ("from_zigbee", "to_zigbee"):
        for i, converter in enumerate(row[direction]):
            # Two anonymous closures can have identical wire selectors and
            # different effects. Keep an addressable reference for each.
            item = dict(converter)
            if item["symbol"] is None:
                item["declaration"] = [row["path"], row["symbol"], i]
            yield direction, item
    for i, dp in enumerate(row["tuya_datapoints"]):
        yield "tuya_datapoint", dict(dp, declaration=[row["path"], row["symbol"], i])
    for hook in row["hooks"]:
        yield "hook", {"operation": hook, "declaration": [row["path"], row["symbol"]]}
    if row["dynamic_exposes"]:
        yield "dynamic_exposes", {"declaration": [row["path"], row["symbol"]]}
    if "turns_off_at_brightness_1" in row:
        yield "brightness_1_quirk", {"active": row["turns_off_at_brightness_1"]}


def deconz_items(row):
    if row["match"].get("requires_review"):
        yield "match_predicate", {"path": row["path"], "match": row["match"].get("predicate")}
    for sub in row["subdevices"]:
        for item in sub["items"]:
            yield "resource", {"type": sub["type"], "name": item["name"]}
            for op in ("parse", "read", "write"):
                if op in item:
                    yield op, {"item": item["name"], **item[op]}
            if "static" in item:
                yield "static", {"item": item["name"], "value": item["static"]}


class Zha:
    """Resolve direct class or builder declarations, without importing quirks."""
    def __init__(self, checkout, revision):
        archive = subprocess.check_output(["git", "-C", str(checkout), "archive", revision, "--", "zhaquirks"])
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            self.sources = {m.name: tar.extractfile(m).read() for m in tar
                            if m.isfile() and m.name.endswith(".py")}
        self.nodes = {}

    def load(self, path):
        if path not in self.nodes:
            data = self.sources[path]
            module = ast.parse(data, filename=path)
            nodes = {}
            for n in ast.walk(module):
                if isinstance(n, ast.ClassDef):
                    nodes[n.lineno, n.name] = n
                elif isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr == "add_to_registry":
                    nodes[n.lineno, "builder"] = n
            self.nodes[path] = nodes
        return self.nodes[path]

    def items(self, row):
        loc = [row["path"], row["line"], row["symbol"]]
        yield "quirk", {"kind": row["kind"], "declaration": loc}
        nodes = self.load(row["path"])
        node = nodes[row["line"], row["symbol"] if row["kind"].startswith("v1") else "builder"]
        if isinstance(node, ast.ClassDef):
            for base in node.bases:
                yield "base_class", {"expression": ast.unparse(base), "declaration": loc}
            for stmt in node.body:
                if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    yield "override", {"method": stmt.name, "declaration": loc}
                elif isinstance(stmt, (ast.Assign, ast.AnnAssign)):
                    targets = stmt.targets if isinstance(stmt, ast.Assign) else [stmt.target]
                    for target in targets:
                        if isinstance(target, ast.Name) and target.id != "signature":
                            yield "class_attribute", {"name": target.id, "declaration": loc}
        else:
            # Follow the registration's receiver only. A nested lambda or
            # conversion helper in an argument is not another builder step.
            while isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
                if node.func.attr != "add_to_registry":
                    yield "builder_step", {"method": node.func.attr, "declaration": loc,
                                           "line": node.func.end_lineno}
                node = node.func.value


# A family pointer is a candidate reusable implementation. No converter or
# complete definition receives parity credit until an independent test covers
# its register/endpoint semantics, hooks, access and MQTT behaviour.
RUST_SUBSETS = {
    "fz.on_off": "zigbee_devices::observe",
    "fz.brightness": "zigbee_devices::observe",
    "tz.on_off": "zigbee_devices::command",
    "tz.light_onoff_brightness": "zigbee_devices::lighting",
    "fz.color_colortemp": "zigbee_devices::lighting",
    "tz.light_colortemp": "zigbee_devices::lighting",
    "fz.metering": "zigbee_devices::metering",
    "fz.electrical_measurement": "zigbee_devices::metering",
    "tuya.fz.datapoints": "zigbee_devices::tuya",
    "tuya.tz.datapoints": "zigbee_devices::tuya",
    "tuya.fz.on_off_action": "zigbee_devices::tuya::Button",
    "fz.temperature": "zigbee_devices::measurement",
    "fz.humidity": "zigbee_devices::measurement",
    "fz.pressure": "zigbee_devices::measurement",
    "fz.battery": "zigbee_devices::measurement",
}


def implementation(source, kind, spec):
    if source == "z2m" and kind in ("from_zigbee", "to_zigbee"):
        family = RUST_SUBSETS.get(spec["symbol"])
        if family:
            return {"status": "partial_family", "rust": family}
    return {"status": "requires_review"}


def inventory(directory, zha_checkout):
    lock = json.loads(Path(__file__).with_name("sources.lock.json").read_text())
    result = {}
    for pin in lock:
        source = pin["source"]
        data = (directory / (source + ".jsonl")).read_bytes()
        if sha256(data).hexdigest() != pin["jsonl_sha256"]:
            raise ValueError(f"{source}: digest differs from lock")
        scan = {"z2m": z2m_items, "deconz": deconz_items}.get(source)
        if source == "zha":
            scan = Zha(zha_checkout, pin["revision"]).items
        by_kind = defaultdict(dict)
        definitions = []
        for raw in data.splitlines():
            row = json.loads(raw)
            if (row["source"], row["revision"]) != (source, pin["revision"]):
                raise ValueError(f"{source}: wrong provenance")
            ref = len(definitions)
            definitions.append({"path": row["path"], "symbol": row["symbol"], "models": row["models"],
                                "manufacturers": row["manufacturers"]})
            seen = set()
            # Record references once per definition, including distinct names
            # sharing a function. Count repeated invocations separately.
            for kind, spec in scan(row):
                key = canonical(spec)
                entries = by_kind[kind]
                if key not in entries:
                    entries[key] = {"signature": spec, **implementation(source, kind, spec),
                                    "definitions": [], "uses": 0}
                entry = entries[key]
                entry["uses"] += 1
                if (kind, key) not in seen:
                    entry["definitions"].append(ref)
                    seen.add((kind, key))
        if len(definitions) != pin["records"]:
            raise ValueError(f"{source}: wrong record count")
        result[source] = {
            "revision": pin["revision"], "definitions": definitions,
            "requirements": {kind: [entries[k] for k in sorted(entries)]
                             for kind, entries in sorted(by_kind.items())},
        }
    return result


def summary(data):
    for source, result in data.items():
        print(f"{source}: {len(result['definitions'])} source rows")
        for kind, entries in result["requirements"].items():
            refs = {ref for e in entries for ref in e["definitions"]}
            print(f"  {kind}: {len(entries)} signatures, {len(refs)} rows")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path, help="directory containing the pinned JSONL listings")
    parser.add_argument("zha_checkout", type=Path)
    parser.add_argument("--output", type=Path, help="write complete inventory as JSON")
    args = parser.parse_args()
    data = inventory(args.directory, args.zha_checkout)
    summary(data)
    if args.output:
        args.output.write_text(canonical(data) + "\n")
