"""Generate reviewed R8 wire records from the Gecko SDK XML package."""
import argparse
from collections import Counter, defaultdict
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re
from tempfile import TemporaryDirectory
from urllib.request import urlopen
import xml.etree.ElementTree as ET

HERE = Path(__file__).resolve().parent
OUTPUT = HERE.parents[1] / "crates/zigbee-zcl/src/cluster/generated.rs"
LOCK = json.loads((HERE / "source.lock.json").read_text())

# Review boundaries, not a parallel list of commands or layouts.
FAMILIES = {
    0x0000: "Basic", 0x0003: "Identify", 0x0004: "Groups", 0x0005: "Scenes",
    0x0006: "OnOff", 0x0008: "LevelControl", 0x0009: "Alarms",
    0x0015: "Commissioning", 0x0019: "Ota", 0x0020: "PollControl",
    0x0102: "WindowCovering", 0x0103: "BarrierControl", 0x0201: "Thermostat",
    0x0300: "ColorControl", 0x0500: "IasZone", 0x0502: "IasWarningDevice",
    0x0600: "GenericTunnel", 0x0601: "BacnetProtocolTunnel",
}
# Absent from R8 Tables 3-16, 3-32 and 8-11. These are standard-namespace SDK additions.
EXCLUDED = {(0, "GetLocalesSupported"), (0, "GetLocalesSupportedResponse"),
            (3, "EZModeInvoke"), (3, "UpdateCommissionState"),
            (0x0500, "InitiateNormalOperationModeResponse"), (0x0500, "InitiateTestModeResponse")}
PRIMITIVES = {"INT8U": "u8", "ENUM8": "u8", "BITMAP8": "u8", "BOOLEAN": "bool",
              "INT8S": "i8", "BITMAP16": "u16", "ENUM16": "u16", "INT16U": "u16", "CLUSTER_ID": "u16", "INT32U": "u32", "INT16S": "i16",
              "UTC_TIME": "u32", "IEEE_ADDRESS": "u64", "CHAR_STRING": "CharacterString", "OCTET_STRING": "OctetString"}


def verified_files(root, download):
    """Hash upstream source before parsing; never access the network in --source mode."""
    data = {}
    for name, expected in LOCK["files"].items():
        path = root / name
        if download:
            url = f"https://raw.githubusercontent.com/{LOCK['repository']}/{LOCK['commit']}/app/zcl/{name}"
            with urlopen(url, timeout=30) as response:
                path.write_bytes(response.read())
        content = path.read_bytes()
        if hashlib.sha256(content).hexdigest() != expected:
            raise ValueError(f"{name}: source digest mismatch")
        data[name] = content
    manifest = json.loads(data.pop("zcl-zap.json"))
    if set(manifest["xmlFile"]) != set(data):
        raise ValueError("lock does not match the Zigbee package manifest")
    return {name: ET.fromstring(data[name]) for name in manifest["xmlFile"]}


@dataclass
class Field:
    name: str
    ty: str
    guard: str = ""


def snake(name):
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", name).lower()


def apply_r8_rules(cluster, name, fields):
    """Only deltas against the SDK belong here. See the cited R8 clauses."""
    by_name = {f.name: f for f in fields}
    if (cluster, name) == (9, "GetAlarmResponse") or (cluster == 0x19 and name in
            {"QueryNextImageResponse", "QuerySpecificFileResponse", "ImageBlockResponse"}):
        # §3.11.2.5.2 and §§11.13.5, 11.13.8, 11.13.11.
        if fields[0].name != "status":
            raise ValueError(f"{name}: missing status discriminator")
        for field in fields[1:]:
            field.guard = "status Equal 0"
    if (cluster, name) == (5, "EnhancedViewSceneResponse"):
        # §3.7.2.5: same success-only tail as ViewSceneResponse.
        for key in ("transition_time", "scene_name", "extension_field_sets"):
            by_name[key].guard = "status Equal 0"
    if cluster in (8, 0x300) and name != "MoveToClosestFrequency":
        # §§3.10.2.3 and 5.2.2.3.3: send both options, default each missing
        # field on legacy input. All four with-On/Off forms share the layout.
        if cluster == 8 and name.endswith("WithOnOff"):
            fields.extend([Field("option_mask", "u8"), Field("option_override", "u8")])
        if [f.name for f in fields[-2:]] not in (["option_mask","option_override"],
                                                  ["options_mask","options_override"]):
            raise ValueError(f"{name}: expected trailing option fields")
        for f in fields[-2:]:
            f.guard = "DefaultZero"
    if cluster != 0x19:
        return fields
    if name == "ImageNotify":
        # Table 11-18: increasing payload types add cumulative fields.
        for n, key in enumerate(("manufacturer_id", "image_type", "new_file_version"), 1):
            by_name[key].guard = f"payload_type AtLeast {n}"
    elif name == "QueryNextImageRequest":
        by_name["hardware_version"].guard = "field_control Bit 1"  # Table 11-19
    elif name in {"ImageBlockRequest", "ImagePageRequest"}:
        by_name["request_node_address"].guard = "field_control Bit 1"  # Tables 11-20, 11-21
        if name == "ImageBlockRequest":
            fields.append(Field("minimum_block_period", "u16", "field_control Bit 2"))
    elif name == "ImageBlockResponse":
        # Figures 11-15 to 11-17. Fold count + bytes into one checked length-prefixed value.
        if [f.name for f in fields[-2:]] != ["data_size", "image_data"]:
            raise ValueError("unexpected block-data layout")
        fields[-2:] = [Field("image_data", "BlockData", "status Equal 0"),
                       Field("current_time", "u32", "status Equal 0x97"),
                       Field("request_time", "u32", "status Equal 0x97"),
                       Field("minimum_block_period", "u16", "status TrailingEqual 0x97")]
    return fields


def argument_guard(cluster, attrs, fields, last):
    """Translate only presence conditions supported by the Rust wire codec."""
    meta = {k: v for k, v in attrs.items()
            if k not in {"name", "type", "introducedIn", "array", "arrayLength"}}
    if not meta:
        return ""
    if set(meta) == {"presentIf"}:
        match = re.fullmatch(r"([A-Za-z][A-Za-z0-9]*)==([0-9]+)", meta["presentIf"])
        controls = {f.name for f in fields if f.ty == "u8" and not f.guard}
        if match and snake(match[1]) in controls and int(match[2]) <= 255:
            return f"{snake(match[1])} Equal {int(match[2])}"
    if last and meta == {"optional": "1"}:
        return "Trailing"
    if (cluster in (8, 0x300) and attrs["name"] in
            {"optionMask", "optionOverride", "optionsMask", "optionsOverride"}
            and meta == {"optional": "1", "default": "0x00"}):
        return "DefaultZero"
    raise ValueError(f"unhandled argument metadata: {attrs}")


def scalar_type(name, aliases):
    bases = aliases.get(name, {name})
    if len(bases) != 1:
        raise ValueError(f"ambiguous field type {name}: {bases}")
    base, = bases
    return PRIMITIVES[base]


def rust_fields(cluster, cmd, aliases):
    if cluster == 0x0201 and cmd.attrib["name"] in {"SetWeeklySchedule", "CurrentWeeklySchedule"}:
        # §§6.3.2.3.2 and 6.3.2.4.1: count and mode index a single list of
        # transitions. The SDK describes the entries as an opaque byte array.
        if [arg.attrib for arg in cmd.findall("arg")] != [
            {"name": "numberOfTransitionsForSequence", "type": "ENUM8"},
            {"name": "dayOfWeekForSequence", "type": "DayOfWeek"},
            {"name": "modeForSequence", "type": "ModeForSequence"},
            {"name": "payload", "type": "INT8U", "array": "true"},
        ]:
            raise ValueError("unexpected SDK weekly schedule fields")
        return [Field("schedule", "WeeklySchedule")]
    fields = []
    count = None
    args = cmd.findall("arg")
    for arg in args:
        attrs = arg.attrib
        key, ty = attrs["name"], attrs["type"]
        guard = argument_guard(cluster, attrs, fields, arg is args[-1])
        if any(attrs.get(k, "true") != "true" for k in ("array", "arrayLength")):
            raise ValueError(f"unhandled array metadata: {attrs}")
        is_array = "array" in attrs
        if count is not None and not is_array:
            raise ValueError(f"expected array after {count[0]}")
        if "arrayLength" in attrs:
            if ty != "INT8U" or is_array:
                raise ValueError(f"unsupported array length: {attrs}")
            count = (key, guard)
            continue
        if is_array and count is not None:
            if guard != count[1]:
                raise ValueError(f"array and count presence differ: {attrs}")
            rust_type = f"Counted<{scalar_type(ty, aliases)}>"
            count = None
        elif is_array and (cluster, cmd.get("name"), key, ty) == (
                0x0601, "TransferNpdu", "npdu", "DATA8") and arg is args[-1]:
            rust_type = "Vec<u8>"  # R8 Figure 9-4: all remaining octets, no length prefix.
        elif is_array and ty == "SceneExtensionFieldSet" and arg is args[-1]:
            rust_type = "Vec<SceneExtensionFieldSet>"
        elif is_array and (cluster, cmd.get("name"), key, ty) == (
                0x19, "ImageBlockResponse", "imageData", "INT8U"):
            rust_type = "u8"  # Folded into BlockData by the R8 status layout.
        elif is_array:
            raise ValueError(f"unhandled array: {attrs}")
        else:
            rust_type = scalar_type(ty, aliases)
            if ty == "ModeForSequence":
                rust_type = "ScheduleMode"
        fields.append(Field(snake(key), rust_type, guard))
    if count is not None:
        raise ValueError(f"missing array after {count[0]}")
    return apply_r8_rules(cluster, cmd.attrib["name"], fields)


def scene_extension_record(types):
    # §3.7.2.4.2.1: the SDK models value as a scalar. Its preceding length
    # bounds the entire field set, even when this cluster is unknown.
    name = "SceneExtensionFieldSet"
    items = types.findall(f"struct[@name='{name}']/item")
    if [i.get("type") for i in items] != ["CLUSTER_ID", "INT8U", "INT8U"]:
        raise ValueError(f"unexpected SDK layout for {name}")
    cluster, _, value = (snake(i.attrib["name"]) for i in items)
    return f"record!({name} {{ {cluster}: u16, {value}: CountedBytes }});"


def generate(docs):
    aliases = defaultdict(set)
    for root in docs.values():
        for el in list(root.findall("enum")) + list(root.findall("bitmap")):
            name, base = el.attrib["name"], el.attrib["type"]
            aliases[name].add(base)
    families = defaultdict(dict)
    inventory = Counter()
    for filename, root in docs.items():
        for owner in list(root.findall("cluster")) + list(root.findall("clusterExtension")):
            cluster = int(owner.get("code") or owner.findtext("code"), 0)
            for cmd in owner.findall("command"):
                name = cmd.attrib["name"]
                if (filename == "sample-extensions.xml" or cluster >= 0xfc00 or
                        owner.get("manufacturerCode") or cmd.get("manufacturerCode")):
                    inventory["vendor/sample"] += 1
                    continue
                if cluster not in FAMILIES:
                    inventory["outside reviewed families"] += 1
                    continue
                if (cluster, name) in EXCLUDED:
                    inventory["reviewed SDK-only additions"] += 1
                    continue
                direction = {"client":"ClientToServer","server":"ServerToClient"}[cmd.attrib["source"]]
                key = (direction, int(cmd.attrib["code"], 0))
                if key in families[cluster]:
                    raise ValueError(f"duplicate command {cluster:#06x} {key}")
                if any(child.tag not in {"arg","description"} for child in cmd):
                    raise ValueError(f"unhandled command element in {name}")
                families[cluster][key] = (name, rust_fields(cluster, cmd, aliases))
                inventory["generated"] += 1
    if set(families) != set(FAMILIES):
        raise ValueError("a reviewed family disappeared")
    lines = ["// @generated by tools/zcl/generate.py. Edit the generator or update its source pin.",
             "// Derived from Silicon Labs Gecko SDK XML, Copyright (c) 2008,2020 Silicon Labs.",
             "// SPDX-License-Identifier: Apache-2.0", "// Modified to model ZCL R8 wire presence rules.", "",
             scene_extension_record(docs["types.xml"]), "", "clusters! {"]
    for cluster, name in sorted(FAMILIES.items()):
        lines.append(f"    {name} = 0x{cluster:04x} {{")
        for (direction, cmd_id), (variant, fields) in sorted(families[cluster].items()):
            args = ", ".join(f"{f.name}: {f.ty}" + (f" [{f.guard}]" if f.guard else "") for f in fields)
            lines.append(f"        {variant} = {direction} 0x{cmd_id:02x} {{ {args} }},")
        lines.append("    },")
    lines.append("}")
    return "\n".join(lines)+"\n", inventory


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, help="offline upstream app/zcl directory; otherwise fetch the pin")
    parser.add_argument("--check", action="store_true", help="compare generated bytes with the committed module")
    args = parser.parse_args()
    with TemporaryDirectory() as temp:
        docs = verified_files(args.source or Path(temp), args.source is None)
        output, inventory = generate(docs)
    if args.check:
        if OUTPUT.read_bytes() != output.encode():
            parser.exit(1, "generated Rust differs from the committed module\n")
    else:
        OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT.write_bytes(output.encode())
    print(", ".join(f"{label}: {count}" for label, count in sorted(inventory.items())))

if __name__ == "__main__":
    main()
