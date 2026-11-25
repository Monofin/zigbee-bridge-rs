#!/usr/bin/env python3
"""Materialize the reviewed subset of vanilla state/brightness declarations."""
import argparse
from collections import Counter, defaultdict
from hashlib import sha256
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile

# Deliberately a small, closed grammar for *whole* modules. Any extra object
# member, computed expression, import, trailing statement or factory option
# takes the module back to source review. No TS/JS code runs at import time.
HEADER = ('import * as m from "../lib/modernExtend";\n'
          'import type {DefinitionWithExtend} from "../lib/types";\n\n'
          'export const definitions: DefinitionWithExtend[] = [')
STRING = r'"(?:[^"\\\x00-\x1f]|\\.)*"'
PLAIN = re.compile(r'\s*\{\s*zigbeeModel:\s*(\[' + STRING + r'(?:,\s*' + STRING + r')*\]),'
                   r'\s*model:\s*(' + STRING + r'),\s*vendor:\s*(' + STRING + r'),'
                   r'\s*description:\s*' + STRING + r',\s*extend:\s*\[m\.light\(\)\],\s*\},')
FIELDS = {"state": ("fz.on_off", "genOnOff"), "brightness": ("fz.brightness", "genLevelCtrl")}


def vanilla_module(text):
    if not text.startswith(HEADER):
        return []
    body = text[len(HEADER):]
    items = []
    pos = 0
    while match := PLAIN.match(body, pos):
        items.append(tuple(json.loads(field) for field in match.groups()))
        pos = match.end()
    return items if body[pos:].strip() == "];" else []


def key(model):
    # Conservative collision check. Runtime lookup still requires an exact
    # Basic value, including whitespace. Strip embedded NUL suffixes too.
    return model.split("\0")[0].strip().casefold()


def access(row):
    lights = [e for e in row["exposes"] if e["type"] == "light"]
    if len(lights) != 1 or lights[0].get("endpoint") is not None:
        return None
    features = lights[0].get("features", [])
    if len(features) != 2 or {e.get("property") for e in features} != set(FIELDS):
        return None
    result = {}
    for feature in features:
        prop, mask = feature["property"], feature["access"]
        if feature.get("endpoint") is not None:
            return None
        if prop == "state" and (feature.get("value_on"), feature.get("value_off")) != ("ON", "OFF"):
            return None
        if prop == "brightness" and (feature.get("value_min"), feature.get("value_max")) != (0, 254):
            return None
        symbol, cluster = FIELDS[prop]
        incoming = {t for c in row["from_zigbee"] if (c["symbol"], c["cluster"]) == (symbol, cluster)
                    for t in (c["message_types"] if isinstance(c["message_types"], list) else [c["message_types"]])}
        outgoing = [c for c in row["to_zigbee"] if c["symbol"] == "tz.light_onoff_brightness" and prop in c["keys"]]
        result[prop] = {
            "report": bool(mask & 1 and "attributeReport" in incoming),
            "read": bool(mask & 4 and "readResponse" in incoming and any(c["get"] for c in outgoing)),
            "write": bool(mask & 2 and any(c["set"] for c in outgoing)),
        }
    return result


def build(directory, checkout):
    rows = {}
    revisions = {}
    for pin in json.loads(Path(__file__).with_name("sources.lock.json").read_text()):
        src = pin["source"]
        data = (directory / (src + ".jsonl")).read_bytes()
        if sha256(data).hexdigest() != pin["jsonl_sha256"]:
            raise ValueError(f"{src}: digest differs from lock")
        revisions[src] = pin["revision"]
        rows[src] = [json.loads(line) for line in data.splitlines()]
    archive = subprocess.check_output(["git", "-C", str(checkout), "archive", revisions["z2m"], "--", "src/devices"])
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        modules = {m.name: vanilla_module(tar.extractfile(m).read().decode()) for m in tar if m.isfile() and m.name.endswith(".ts")}
    others = {key(m) for src in ("zha", "deconz") for r in rows[src] for m in r["models"]}
    owners = defaultdict(set)
    for i, row in enumerate(rows["z2m"]):
        for model in row["models"]:
            owners[key(model)].add(i)
    index = {"schema": 1, "revisions": revisions, "families": {}, "selectors": {}}
    excluded = Counter()
    count = 0
    for i, row in enumerate(rows["z2m"]):
        if row.get("turns_off_at_brightness_1"):
            excluded["turns_off_at_brightness_1"] += 1
            continue
        defs = modules.get(row["path"], [])
        slot = re.fullmatch(r"definitions\[(\d+)\]", row["symbol"])
        if not slot or int(slot[1]) >= len(defs):
            excluded["outside_vanilla_source_grammar"] += 1
            continue
        if tuple((row["zigbee_models"], row["display_model"], row["vendor"])) != defs[int(slot[1])]:
            raise ValueError(f"source/index disagreement at {row['path']} {row['symbol']}")
        if (row["fingerprints"] or row["white_labels"] or row["dynamic_exposes"] or row["tuya_datapoints"]
                or set(row["hooks"]) - {"configure"}):
            excluded["additional_behaviour"] += 1
            continue
        if any(not key(m) or key(m) in others or owners[key(m)] != {i} for m in row["models"]):
            excluded["ambiguous_or_other_source_variant"] += 1
            continue
        grants = access(row)
        if not grants or not any(any(g.values()) for g in grants.values()):
            excluded["no_supported_access"] += 1
            continue
        # Keep distinct access contracts distinct even if later pins declare
        # only a subset. The family ID is independent of the selected model.
        family = "dimmable_" + "".join(str(int(grants[p][op])) for p in FIELDS for op in ("report", "read", "write"))
        index["families"][family] = grants
        for model in row["models"]:
            index["selectors"][model] = {"family": family, "path": row["path"], "symbol": row["symbol"]}
        count += 1
    index["counts"] = {"eligible_rows": count, "rows_requiring_review": dict(excluded)}
    return index


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    output = build(args.directory, args.checkout)
    args.output.write_text(json.dumps(output, sort_keys=True, indent=2) + "\n")
    print(len(output["selectors"]), "wire models;", output["counts"])
