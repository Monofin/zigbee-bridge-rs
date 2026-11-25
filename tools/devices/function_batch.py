#!/usr/bin/env python3
"""Select bounded review batches by advertised property, access and source selector."""
import argparse
from hashlib import sha256
import json
from pathlib import Path

ACCESS = {"publish": 1, "set": 2, "get": 4}
DDF_OPERATION = {"publish": "parse", "set": "write", "get": "read"}


def exposed(node, prop, access, inherited=7, path=()):
    mask = inherited & node.get("access", 7)
    current = (*path, node.get("property") or node.get("name") or node["type"])
    if node.get("property") == prop and "access" in node and mask & ACCESS[access]:
        yield {"path": current, "shape": node, "effective_access": mask}
    for feature in node.get("features", []):
        yield from exposed(feature, prop, access, mask, current)
    # Members of a list describe a payload element, not independent properties.


def z2m_selectors(row):
    for fp in row["fingerprints"]:
        if ((isinstance(fp.get("modelID"), str) and
             (isinstance(fp.get("manufacturerName"), str) or isinstance(fp.get("manufacturerID"), int)))
                or fp.get("endpoints")):
            yield {"fingerprint": fp}
    wire = row["zigbee_models"]
    if len(wire) == 1:
        for label in row["white_labels"]:
            for fp in label.get("fingerprint", []):
                if isinstance(fp.get("manufacturerName"), str):
                    yield {"wire_model": wire[0], "white_label_fingerprint": fp}


def candidate(row, prop, access):
    if row["source"] == "zha":
        # These rows identify quirk registrations. Builder method names alone
        # cannot establish entity access or inherited cluster behaviour.
        return None, "access_requires_quirk_review"
    if row["source"] == "z2m":
        shapes = [s for e in row["exposes"] for s in exposed(e, prop, access)]
        if not shapes:
            return None, "dynamic_exposes" if row["dynamic_exposes"] else "no_declared_access"
        selectors = list(z2m_selectors(row))
        if not selectors:
            return None, "model_only_or_incomplete_selector"
        if access == "publish":
            # Inbound converters do not advertise their output properties.
            # These are row-level references pending function-level review.
            operations = row["from_zigbee"]
        else:
            operations = [c for c in row["to_zigbee"] if c.get(access) and prop in (c.get("keys") or [])]
        if not operations:
            return None, "no_declared_converter"
        return {"selectors": selectors, "exposures": shapes, "converter_candidates": operations}, None

    op = DDF_OPERATION[access]
    operations = [
        {"subdevice": sub["type"], "uuid": sub["uuid"], "fingerprint": sub.get("fingerprint"),
         "operation": item[op]} for sub in row["subdevices"] for item in sub["items"]
        if item["name"] == prop and op in item
    ]
    if not operations:
        return None, "no_declared_access"
    if row["match"].get("requires_review"):
        return None, "match_expression_requires_review"
    if len(row["manufacturers"]) != 1 or len(row["models"]) != 1 or not all(
        row[field][0] for field in ("manufacturers", "models")
    ):
        return None, "incomplete_selector"
    return {"selectors": [{"manufacturer": row["manufacturers"][0], "model": row["models"][0]}],
            "ddf_operations": operations}, None


def batch(directory, source, prop, access, offset, limit):
    pins = json.loads(Path(__file__).with_name("sources.lock.json").read_text())
    result = {"source": source, "property": prop, "access": access, "offset": offset,
              "eligible": 0, "excluded": {}, "items": []}
    pin = next(item for item in pins if item["source"] == source)
    result["revision"] = pin["revision"]
    data = (directory / (source + ".jsonl")).read_bytes()
    if sha256(data).hexdigest() != pin["jsonl_sha256"]:
        raise ValueError(f"{source}: digest differs from lock")
    for raw in data.splitlines():
        row = json.loads(raw)
        item, reason = candidate(row, prop, access)
        if item is None:
            excluded = result["excluded"]
            excluded[reason] = excluded.get(reason, 0) + 1
            continue
        if offset <= result["eligible"] < offset + limit:
            result["items"].append({"path": row["path"], "symbol": row["symbol"], **item})
        result["eligible"] += 1
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("source", choices=["deconz", "z2m", "zha"])
    parser.add_argument("property")
    parser.add_argument("access", choices=ACCESS)
    parser.add_argument("--offset", type=int, default=0)
    parser.add_argument("--limit", type=int, default=20)
    args = parser.parse_args()
    if args.offset < 0 or args.limit <= 0:
        parser.error("offset must be nonnegative and limit must be positive")
    print(json.dumps(batch(args.directory, args.source, args.property, args.access, args.offset, args.limit),
                     sort_keys=True, indent=2))
