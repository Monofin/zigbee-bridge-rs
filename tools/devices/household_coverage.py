#!/usr/bin/env python3
"""Compare a private projection against all locked catalogues and a real bridge replay.

prepare produces a portable scratch input; finish joins the Rust planner output
and writes an aggregate. Input/output contain only model, capability and count.
"""
import argparse
from collections import Counter
from hashlib import sha256
import json
from pathlib import Path


def samples(shape):
    if shape["type"] == "composite":
        children=[c for c in shape.get("features", []) if c.get("property") and c.get("access", 2)&2]
        keys={c["property"] for c in children}
        if keys=={"x","y"}: return [{"x":0.3,"y":0.3}]
        if keys=={"hue","saturation"}: return [{"hue":120,"saturation":60}]
        example={}
        for child in children:
            values=samples(child)
            if values: example[child["property"]]=values[0]
        if shape.get("property") == "overload_protection":
            # Each enabled minimum must be strictly below its paired maximum.
            # Keep the limits from this product's exposure.
            for child in children:
                if child["property"].startswith("max_") and "value_max" in child:
                    example[child["property"]] = child["value_max"]
        return [example] if example else []
    if shape["type"] == "list" and shape.get("item_type"):
        values=samples(shape["item_type"])
        return [[values[0]]] if values else []

    if shape.get("type") == "binary":
        return [shape.get("value_on", True), shape.get("value_off", False)]
    if shape.get("type") == "enum":
        return shape.get("values", [])[:4]
    if shape.get("type") == "numeric":
        return list(dict.fromkeys([shape.get("value_min", 1), shape.get("value_max", 1), 1]))
    if shape.get("type") == "text":
        return ["#ff8000"] if shape.get("property") == "effect_color" else ["example"]
    return []


def properties(node, parents=(), mask=7):
    name = node.get("property")
    access = mask & node.get("access", 7)
    if name and "access" in node:
        yield {"property": name, "endpoint": node.get("endpoint"), "path": list(parents),
               "mask": access, "values": samples(node), "unit": node.get("unit"),
               "shape":node if node["type"] in ("list","composite") else None}
        # A named composite or list is one MQTT property. Its children
        # describe members of the value, not additional top-level commands.
        return
    for child in node.get("features", []):
        yield from properties(child, (*parents, name or node["type"]), access)


def match_fingerprint(fp, house):
    known = {"modelID": house["model"], "manufacturerName": house["manufacturer"], "type":house["type"]}
    missing = []
    for key, expected in fp.items():
        if key in known and isinstance(expected, str):
            if known[key] != expected:
                return None
        elif key == "endpoints":
            endpoints = {e["id"]:e for e in house["endpoints"]}
            for e in expected:
                observed = endpoints.get(e.get("ID"))
                if observed is None:
                    return None
                fields={"profileID":"profile","deviceID":"device","inputClusters":"servers","outputClusters":"clients"}
                for wire,val in e.items():
                    if wire=="ID":
                        continue
                    if wire not in fields or fields[wire] not in observed:
                        missing.append("endpoints/"+wire)
                    elif isinstance(val,list):
                        if sorted(val)!=sorted(observed[fields[wire]]):
                            return None
                    elif val!=observed[fields[wire]]:
                        return None
        else:
            missing.append(key)
    return "conditional" if missing else "fingerprint"


def references(row, house):
    m=house["model"]; maker=house["manufacturer"]
    if row["source"] == "deconz":
        if m in row["models"] and maker in row["manufacturers"]:
            return "conditional" if row["match"].get("requires_review") else "identity"
        return None
    exact = [match_fingerprint(f,house) for f in row.get("fingerprints",[]) if f]
    if "fingerprint" in exact:
        return "fingerprint"
    if row["source"] == "z2m" and m in row["zigbee_models"]:
        return "model"
    if row["source"] == "zha" and m in row["models"] and (not row["manufacturers"] or maker in row["manufacturers"]):
        return "model_candidate"
    if "conditional" in exact:
        return "conditional"
    return None


def prepare(data, models, live=None):
    live_by_identity={}
    if live:
        for rec in json.loads(live.read_text()):
            key=(rec["manufacturer"],rec["model"])
            live_by_identity.setdefault(key,[]).append(rec)
    listings={}
    for pin in json.loads(Path(__file__).with_name("sources.lock.json").read_text()):
        src=pin["source"];raw=(data/(src+".jsonl")).read_bytes()
        if sha256(raw).hexdigest()!=pin["jsonl_sha256"]:
            raise ValueError("digest differs from pin: "+src)
        listings[src]=list(map(json.loads, raw.splitlines()))
    result=[]
    for group in json.loads(models.read_text()):
        # Reject extra identity/topology fields when creating the portable projection.
        group={k:group[k] for k in ("model","manufacturer","type","count","endpoints")}
        group["endpoints"]=[{k:v for k,v in e.items() if k in ("id","profile","device","servers","clients","color_caps")} for e in group["endpoints"]]
        refs={}; candidates=[]
        for src,rows in listings.items():
            refs[src]=[]
            for row in rows:
                reason=references(row,group)
                if reason:
                    refs[src].append({"path":row["path"],"symbol":row["symbol"],"match":reason})
                    if src=="z2m":
                        candidates.append((reason,row))
        # Apply the highest evidenced descriptor class; ties remain ambiguous.
        # Conditional selectors might outrank a model-only selector on the real
        # device. Never assign their exposures silently to this group.
        exact=[r for reason,r in candidates if reason=="fingerprint"]
        possible=exact or [r for reason,r in candidates if reason=="model"]
        proven=(len(possible)==1 and not any(why=="conditional" for why,_ in candidates))
        row=possible[0] if proven else None
        exposed=[p for x in row["exposes"] for p in properties(x)] if row else []
        state="ambiguous" if row is None else "dynamic" if row["dynamic_exposes"] else "static"
        expected_origin="pinned_catalogue"
        if live:
            candidates=live_by_identity.get((group["manufacturer"],group["model"]),[])
            if len(candidates)!=1 or candidates[0]["count"]!=group["count"]:
                raise ValueError("live product/exposure signature differs from projected group")
            exposed=[p for x in candidates[0]["exposes"] for p in properties(x)]
            expected_origin="live_retained_definition"
        result.append({**group,"expected_origin":expected_origin,"source_status":state,"references":refs,"expected":exposed})
    return result


def finish(prepared,replayed):
    if len(prepared)!=len(replayed):
        raise ValueError("projection/replay group counts differ")
    result=[]; totals=Counter()
    for i,(group,replay) in enumerate(zip(prepared,replayed)):
        if replay["index"]!=i:
            raise ValueError("replay row order differs")
        eps=replay["endpoints"]
        union={op:{name for e in eps for name in e.get(op,[])} for op in ("read","write","report")}
        # The upstream STATE bit also includes publication after an accepted
        # command. Keep that evidence distinct from an unsolicited wire report.
        after_write = {name for ep in eps for name in ep.get("state_after_write", [])}
        missing={}; matched={}
        for p in group["expected"]:
            for bit,op in [(1,"report"),(2,"write"),(4,"read")]:
                if p["mask"] & bit:
                    key=p["property"]
                    # Keep ambiguous endpoint-specific contracts for review.
                    present = key in union[op] or (op == "report" and key in after_write)
                    covered=present and p["endpoint"] is None
                    (matched if covered else missing).setdefault(op,set()).add(key)
        for k in ("read","write","report"):
            totals[k+"_matched"]+=len(matched.get(k,()))
            totals[k+"_requires_review"]+=len(missing.get(k,()))
        totals["devices"]+=group["count"];totals["groups"]+=1
        totals["with_bound_endpoint"]+=bool(eps);totals["bound_devices"]+=group["count"] if eps else 0
        totals["source_"+group["source_status"]]+=1
        result.append({"manufacturer":group["manufacturer"],"model":group["model"],"count":group["count"],
                      "expected_origin":group["expected_origin"],"source_status":group["source_status"],"references":group["references"],"source_properties":group["expected"],
                      "bound_endpoints":eps,"state_after_write":sorted(after_write),"matched":{k:sorted(v) for k,v in matched.items()},
                      "requires_review":{k:sorted(v) for k,v in missing.items()}})
    return {"counts":dict(totals),"groups":result}


if __name__=="__main__":
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("mode",choices=["prepare","finish"])
    p.add_argument("first",type=Path);p.add_argument("second",type=Path);p.add_argument("output",type=Path)
    p.add_argument("--live-exposes",type=Path,help="private model/manufacturer/definition.exposes projection for the same cohort")
    args=p.parse_args()
    if args.mode=="prepare": data=prepare(args.first,args.second,args.live_exposes)
    else: data=finish(json.loads(args.first.read_text()),json.loads(args.second.read_text()))
    args.output.write_text(json.dumps(data,sort_keys=True,indent=2)+"\n")
    if args.mode=="finish": print(json.dumps(data["counts"],sort_keys=True))
