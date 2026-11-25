#!/usr/bin/env python3
"""Check zigbee-discovery --jsonl output in the target Home Assistant Core environment."""
from collections import Counter
from importlib import import_module
import asyncio
import json
import tempfile
import sys

from homeassistant.core import HomeAssistant
from homeassistant.components.mqtt.discovery import (
    MQTTDiscoveryPayload,
    MQTT_ORIGIN_INFO_SCHEMA,
    _replace_all_abbreviations,
    _replace_topic_base,
)
from homeassistant.components.mqtt.device_trigger import TRIGGER_DISCOVERY_SCHEMA
from homeassistant.components.sensor.const import DEVICE_CLASS_STATE_CLASSES


def validate(domain, fields):
    payload = MQTTDiscoveryPayload(fields)
    _replace_all_abbreviations(payload)
    if "~" in payload:
        _replace_topic_base(payload)
    if "origin" in payload:
        MQTT_ORIGIN_INFO_SCHEMA(payload["origin"])
    module = import_module("homeassistant.components.mqtt." + domain)
    config = module.DISCOVERY_SCHEMA(payload)
    if domain == "device_automation":
        config = TRIGGER_DISCOVERY_SCHEMA(config)
    if domain == "sensor" and (cls := config.get("device_class")):
        state = config.get("state_class")
        if state and state not in DEVICE_CLASS_STATE_CLASSES[cls]:
            raise ValueError("unsupported statistics for device class")
    # Discovery handles origin and the topic base before the platform schema.
    return payload.keys() - config.keys() - {"origin", "~"}


async def main(config_dir):
    HomeAssistant(config_dir)  # Template compilation uses the current HA event loop.
    counts = Counter()
    invalid = Counter()
    omitted = Counter()
    for line in sys.stdin:
        row = json.loads(line)
        # The bridge emits a single component per config topic, with a node ID.
        prefix, domain, node, obj, config = row["topic"].rsplit("/", 4)
        if not prefix or not domain.isidentifier() or not node or not obj or config != "config":
            raise ValueError("invalid rendered discovery topic")
        counts[domain] += 1
        try:
            omitted.update(validate(domain, row["payload"]))
        except Exception as exc:
            # Never print private topic names, payloads or exception messages.
            invalid[(domain, type(exc).__name__, ".".join(map(str, getattr(exc, "path", ()))))] += 1
    print(json.dumps({"checked": counts, "omitted_by_platform_schema": omitted, "errors": [[*key, n] for key, n in sorted(invalid.items())]}))
    return int(bool(invalid) or not counts)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as root:
        sys.exit(asyncio.run(main(root)))
