#!/usr/bin/env python3
"""Reproduce the reviewed EP1 dimmable Innr selector list from the pinned sources."""
import argparse
import json
import re
from pathlib import Path
from subprocess import check_output


ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "crates/zigbee-devices/profiles/innr-level-one.txt"
BASE_MODEL = "BF 263"  # Basic identity and EP1 verified by bench interview.
COMPAT = ("exposes", "from_zigbee", "to_zigbee", "hooks", "white_labels", "fingerprints")


def models(inputs):
    pins = {r["source"]: r["revision"] for r in json.loads(
        (ROOT / "tools/devices/sources.lock.json").read_text())}

    def rows(source):
        for line in (inputs / f"{source}.jsonl").read_text().splitlines():
            row = json.loads(line)
            if row["revision"] != pins[source]:
                raise ValueError(f"{source} input revision differs from lock")
            yield row

    source = inputs / "z2m-source"
    if check_output(["git", "-C", source, "rev-parse", "HEAD"], text=True).strip() != pins["z2m"]:
        raise ValueError("converter checkout revision differs from lock")
    data = check_output(["git", "-C", source, "show",
                         f"{pins['z2m']}:src/devices/innr.ts"], text=True)
    # Accept this literal factory call alone; explicit meta or a second extension
    # needs another review even if it happens to expose the same property names.
    simple = set()
    for text in re.findall(r"^    \{\n(.*?)^    \},", data, re.S | re.M):
        fields = dict(re.findall(r"^        (\w+): ([^\n]+),$", text, re.M))
        if len(fields) != len(text.splitlines()):
            continue
        if set(fields) - {"zigbeeModel", "model", "vendor", "description", "ota", "extend"}:
            continue
        if fields.get("vendor") == '"Innr"' and fields.get("extend") == "[m.light({turnsOffAtBrightness1: true})]":
            if re.fullmatch(r'\["[^"\\]+"\]', fields.get("zigbeeModel", "")):
                simple.add(json.loads(fields["zigbeeModel"])[0])

    z2m = list(rows("z2m"))
    base, = (r for r in z2m if r["zigbee_models"] == [BASE_MODEL])
    plain = {m for m in simple
             if sum(m in row["zigbee_models"] or any(f.get("modelID") == m for f in row["fingerprints"])
                    for row in z2m) == 1
             and any(r["path"] == base["path"] and r["zigbee_models"] == [m]
                     and r.get("turns_off_at_brightness_1") is True
                     and all(r[k] == base[k] for k in COMPAT) for r in z2m)}
    # A model-only Z2M match and vendor display label do not establish the Basic
    # manufacturer string. Require independent wire identity and endpoint evidence.
    corroborated = {BASE_MODEL}
    for r in rows("deconz"):
        if (r["status"] == "Gold" and r["manufacturers"] == ["innr"]
                and not r["match"]["requires_review"] and len(r["subdevices"]) == 1):
            device = r["subdevices"][0]
            if device["type"] == "Dimmable light" and device["uuid"] == ["$address.ext", "0x01"]:
                corroborated.update(r["models"])
    reviewed = plain & corroborated
    for r in rows("zha"):
        reviewed.difference_update(r["models"])
    if BASE_MODEL not in reviewed:
        raise ValueError("baseline signature changed")
    return "\n".join(sorted(reviewed)) + "\n"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inputs", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    content = models(args.inputs)
    if args.check:
        if OUTPUT.read_text() != content:
            raise SystemExit(f"{OUTPUT.name} differs; review selectors before updating")
    else:
        OUTPUT.write_text(content)
