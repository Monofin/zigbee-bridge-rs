"""Index committed deCONZ DDFs and reference their handlers without running them.

Usage: python3 tools/devices/deconz.py CHECKOUT > catalogue.jsonl

Each row is one positional manufacturer/model pair. Subdevice fingerprints,
item operations and optional JavaScript match expressions remain scoped to
their source DDF. Paths and JSON pointers refer to the recorded commit.
Upstream's root LICENSE.txt is BSD-3-Clause, including its notice conditions.
"""

import argparse
import hashlib
import io
import json
import posixpath
import subprocess
import sys
import tarfile


def committed_devices(checkout):
    """Read from Git so dirty and untracked files cannot acquire a false revision."""
    git = ["git", "-C", str(checkout)]
    revision = subprocess.check_output([*git, "rev-parse", "HEAD^{commit}"], text=True).strip()
    archive = subprocess.check_output([*git, "archive", "--format=tar", revision, "devices/"])
    documents = {}
    scripts = set()
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as tree:
        for entry in tree:
            if not entry.isfile():
                continue
            if entry.name.endswith(".json"):
                try:
                    documents[entry.name] = json.load(tree.extractfile(entry))
                except ValueError as error:
                    raise ValueError(f"{entry.name}: {error}") from error
            elif entry.name.endswith(".js"):
                scripts.add(entry.name)
    return revision, documents, scripts


def sha256_text(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def strings(value):
    if isinstance(value, str):
        return [value]
    if isinstance(value, list) and all(isinstance(item, str) for item in value):
        return value
    raise ValueError("expected a string or an array of strings")


def resource_item(item, pointer, path, generic_items, scripts):
    name = item["name"]
    generic_path, generic = generic_items.get(name, (None, {}))
    result = {"name": name, "generic_item": generic_path}
    # DDF_ParseItem tests for the key's presence; false and zero are valid
    # static values that suppress inheritance of all three protocol operations.
    if "static" in item:
        result["static"] = item["static"]
        return result
    for key in ("parse", "read", "write"):
        if key in item:
            handler, origin = item[key], path
            location = f"{pointer}/{key}"
        elif key in generic:
            handler, origin = generic[key], generic_path
            location = f"/{key}"
        else:
            continue
        operation = {"path": origin, "pointer": location}
        # The path/pointer preserves arbitrary parameters. The selector and
        # declared wire fields are useful for finding reusable Rust functions.
        for param in ("fn", "ep", "cl", "at", "dt", "mf", "cmd", "dpid", "cppsrc"):
            if param in handler:
                operation[param] = handler[param]
        if "script" in handler:
            # DDF_LoadScripts runs after the generic-item merge and resolves
            # script paths relative to the consuming device description.
            ref = posixpath.normpath(posixpath.join(posixpath.dirname(path), handler["script"]))
            operation["script"] = {"path": ref, "present": ref in scripts}
        elif "eval" in handler:
            operation["inline_eval_sha256"] = sha256_text(handler["eval"])
        result[key] = operation
    return result


def rows(revision, documents, scripts):
    constants = {}
    definitions = {}
    for path, document in documents.items():
        schema = document["schema"]
        if schema == "constants1.schema.json":
            for group in document.values():
                if isinstance(group, dict):
                    constants.update(group)
        elif schema == "resourceitem1.schema.json":
            name = document["id"]
            if name in definitions:
                raise ValueError(f"ambiguous generic item {name!r} at {path}")
            definitions[name] = (path, document)

    for path, profile in sorted(documents.items()):
        if profile["schema"] != "devcap1.schema.json":
            continue
        manufacturers = strings(profile["manufacturername"])
        models = strings(profile["modelid"])
        if not models or len(manufacturers) != len(models):
            raise ValueError(f"{path}: unequal or empty manufacturer/model lists")
        subdevices = []
        for index, sub in enumerate(profile["subdevices"]):
            entry = {"type": constants.get(sub["type"], sub["type"]),
                     "uuid": sub["uuid"], "restapi": sub["restapi"]}
            if "fingerprint" in sub:
                entry["fingerprint"] = sub["fingerprint"]
            entry["items"] = [
                resource_item(item, f"/subdevices/{index}/items/{i}", path, definitions, scripts)
                for i, item in enumerate(sub["items"])
            ]
            subdevices.append(entry)
        for index, (manufacturer, model) in enumerate(zip(manufacturers, models)):
            manufacturer = constants.get(manufacturer, manufacturer)
            model = constants.get(model, model)
            match = {"pair_index": index}
            if "matchexpr" in profile:
                match["predicate"] = {"pointer": "/matchexpr",
                                      "sha256": sha256_text(profile["matchexpr"])}
            # Keep source tokens for investigation; don't index unresolved
            # constants as literal on-air identities or infer a wildcard.
            unresolved = [v for v in (manufacturer, model) if v.startswith("$")]
            if unresolved:
                match["unresolved_constants"] = unresolved
            match["requires_review"] = bool(unresolved or "predicate" in match)
            yield {"source": "deconz", "revision": revision, "path": path,
                   "symbol": f"{profile['uuid']}[{index}]", "kind": "ddf",
                   "manufacturers": [] if manufacturer.startswith("$") else [manufacturer],
                   "models": [] if model.startswith("$") else [model],
                   "match": match, "status": profile["status"], "product": profile.get("product"),
                   "subdevices": subdevices}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout")
    args = parser.parse_args()
    for row in rows(*committed_devices(args.checkout)):
        print(json.dumps(row, ensure_ascii=True, separators=(",", ":")))


if __name__ == "__main__":
    try:
        main()
    except (KeyError, ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(f"deCONZ catalogue: {error}")
