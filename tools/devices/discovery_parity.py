#!/usr/bin/env python3
"""Compare the full Home Assistant declarations in two retained HA discovery snapshots."""
import argparse
from collections import Counter, defaultdict
import json
import os
from pathlib import Path
import sys


# Abbreviation maps from Home Assistant Core 2026.11.3, Apache-2.0.
# homeassistant/components/mqtt/abbreviations.py. Unknown keys are compared too.
ABBREVIATIONS = {
    "act_t": "action_topic",
    "act_tpl": "action_template",
    "act_stat_t": "activity_state_topic",
    "act_val_tpl": "activity_value_template",
    "atype": "automation_type",
    "av_tones": "available_tones",
    "avty": "availability",
    "avty_mode": "availability_mode",
    "avty_t": "availability_topic",
    "avty_tpl": "availability_template",
    "b_tpl": "blue_template",
    "bri_cmd_tpl": "brightness_command_template",
    "bri_cmd_t": "brightness_command_topic",
    "bri_scl": "brightness_scale",
    "bri_stat_t": "brightness_state_topic",
    "bri_tpl": "brightness_template",
    "bri_val_tpl": "brightness_value_template",
    "cdevs": "child_devices",
    "cln_segmnts_cmd_t": "clean_segments_command_topic",
    "cln_segmnts_cmd_tpl": "clean_segments_command_template",
    "clr_temp_cmd_tpl": "color_temp_command_template",
    "clrm_stat_t": "color_mode_state_topic",
    "clrm_val_tpl": "color_mode_value_template",
    "clr_temp_cmd_t": "color_temp_command_topic",
    "clr_temp_k": "color_temp_kelvin",
    "clr_temp_stat_t": "color_temp_state_topic",
    "clr_temp_tpl": "color_temp_template",
    "clr_temp_val_tpl": "color_temp_value_template",
    "cmd_off_tpl": "command_off_template",
    "cmd_on_tpl": "command_on_template",
    "cmd_t": "command_topic",
    "cmd_tpl": "command_template",
    "cmps": "components",
    "cod_arm_req": "code_arm_required",
    "cod_dis_req": "code_disarm_required",
    "cod_form": "code_format",
    "cod_trig_req": "code_trigger_required",
    "cont_type": "content_type",
    "curr_hum_t": "current_humidity_topic",
    "curr_hum_tpl": "current_humidity_template",
    "curr_temp_t": "current_temperature_topic",
    "curr_temp_tpl": "current_temperature_template",
    "def_ent_id": "default_entity_id",
    "dev": "device",
    "dev_cla": "device_class",
    "dir_cmd_t": "direction_command_topic",
    "dir_cmd_tpl": "direction_command_template",
    "dir_stat_t": "direction_state_topic",
    "dir_val_tpl": "direction_value_template",
    "dsp_prc": "display_precision",
    "dock_cmd_t": "dock_command_topic",
    "dock_cmd_tpl": "dock_command_template",
    "e": "encoding",
    "en": "enabled_by_default",
    "ent_cat": "entity_category",
    "ent_pic": "entity_picture",
    "evt_typ": "event_types",
    "fanspd_lst": "fan_speed_list",
    "flsh": "flash",
    "flsh_tlng": "flash_time_long",
    "flsh_tsht": "flash_time_short",
    "fx_cmd_tpl": "effect_command_template",
    "fx_cmd_t": "effect_command_topic",
    "fx_list": "effect_list",
    "fx_stat_t": "effect_state_topic",
    "fx_tpl": "effect_template",
    "fx_val_tpl": "effect_value_template",
    "exp_aft": "expire_after",
    "fan_mode_cmd_tpl": "fan_mode_command_template",
    "fan_mode_cmd_t": "fan_mode_command_topic",
    "fan_mode_stat_tpl": "fan_mode_state_template",
    "fan_mode_stat_t": "fan_mode_state_topic",
    "frc_upd": "force_update",
    "g_tpl": "green_template",
    "grp": "group",
    "hs_cmd_t": "hs_command_topic",
    "hs_cmd_tpl": "hs_command_template",
    "hs_stat_t": "hs_state_topic",
    "hs_val_tpl": "hs_value_template",
    "ic": "icon",
    "img_e": "image_encoding",
    "img_t": "image_topic",
    "init": "initial",
    "hum_cmd_t": "target_humidity_command_topic",
    "hum_cmd_tpl": "target_humidity_command_template",
    "hum_stat_t": "target_humidity_state_topic",
    "hum_state_tpl": "target_humidity_state_template",
    "json_attr": "json_attributes",
    "json_attr_t": "json_attributes_topic",
    "json_attr_tpl": "json_attributes_template",
    "lrst_val_tpl": "last_reset_value_template",
    "max": "max",
    "min": "min",
    "max_hum": "max_humidity",
    "min_hum": "min_humidity",
    "max_mirs": "max_mireds",
    "min_mirs": "min_mireds",
    "max_k": "max_kelvin",
    "min_k": "min_kelvin",
    "max_temp": "max_temp",
    "min_temp": "min_temp",
    "migr_discvry": "migrate_discovery",
    "mode": "mode",
    "mode_cmd_tpl": "mode_command_template",
    "mode_cmd_t": "mode_command_topic",
    "mode_stat_t": "mode_state_topic",
    "mode_stat_tpl": "mode_state_template",
    "modes": "modes",
    "msg_exp_int": "message_expiry_interval",
    "name": "name",
    "o": "origin",
    "off_dly": "off_delay",
    "on_cmd_type": "on_command_type",
    "ops": "options",
    "opt": "optimistic",
    "osc_cmd_t": "oscillation_command_topic",
    "osc_cmd_tpl": "oscillation_command_template",
    "osc_stat_t": "oscillation_state_topic",
    "osc_val_tpl": "oscillation_value_template",
    "p": "platform",
    "pause_cmd_t": "pause_command_topic",
    "pause_mw_cmd_tpl": "pause_command_template",
    "pct_cmd_t": "percentage_command_topic",
    "pct_cmd_tpl": "percentage_command_template",
    "pct_stat_t": "percentage_state_topic",
    "pct_val_tpl": "percentage_value_template",
    "pl": "payload",
    "pl_arm_away": "payload_arm_away",
    "pl_arm_home": "payload_arm_home",
    "pl_arm_nite": "payload_arm_night",
    "pl_arm_vacation": "payload_arm_vacation",
    "pl_arm_custom_b": "payload_arm_custom_bypass",
    "pl_avail": "payload_available",
    "pl_cln_sp": "payload_clean_spot",
    "pl_cls": "payload_close",
    "pl_disarm": "payload_disarm",
    "pl_home": "payload_home",
    "pl_lock": "payload_lock",
    "pl_loc": "payload_locate",
    "pl_not_avail": "payload_not_available",
    "pl_not_home": "payload_not_home",
    "pl_off": "payload_off",
    "pl_on": "payload_on",
    "pl_open": "payload_open",
    "pl_osc_off": "payload_oscillation_off",
    "pl_osc_on": "payload_oscillation_on",
    "pl_paus": "payload_pause",
    "pl_prs": "payload_press",
    "pl_rst": "payload_reset",
    "pl_rst_hum": "payload_reset_humidity",
    "pl_rst_mode": "payload_reset_mode",
    "pl_rst_pct": "payload_reset_percentage",
    "pl_rst_pr_mode": "payload_reset_preset_mode",
    "pl_stop": "payload_stop",
    "pl_stop_tilt": "payload_stop_tilt",
    "pl_strt": "payload_start",
    "pl_ret": "payload_return_to_base",
    "pl_toff": "payload_turn_off",
    "pl_ton": "payload_turn_on",
    "pl_trig": "payload_trigger",
    "pl_unlk": "payload_unlock",
    "pos": "reports_position",
    "pos_clsd": "position_closed",
    "pos_open": "position_open",
    "pow_cmd_t": "power_command_topic",
    "pow_cmd_tpl": "power_command_template",
    "pr_mode_cmd_t": "preset_mode_command_topic",
    "pr_mode_cmd_tpl": "preset_mode_command_template",
    "pr_mode_stat_t": "preset_mode_state_topic",
    "pr_mode_val_tpl": "preset_mode_value_template",
    "pr_modes": "preset_modes",
    "ptrn": "pattern",
    "r_tpl": "red_template",
    "rel_s": "release_summary",
    "rel_u": "release_url",
    "ret": "retain",
    "rgb_cmd_tpl": "rgb_command_template",
    "rgb_cmd_t": "rgb_command_topic",
    "rgb_stat_t": "rgb_state_topic",
    "rgb_val_tpl": "rgb_value_template",
    "rgbw_cmd_tpl": "rgbw_command_template",
    "rgbw_cmd_t": "rgbw_command_topic",
    "rgbw_stat_t": "rgbw_state_topic",
    "rgbw_val_tpl": "rgbw_value_template",
    "rgbww_cmd_tpl": "rgbww_command_template",
    "rgbww_cmd_t": "rgbww_command_topic",
    "rgbww_stat_t": "rgbww_state_topic",
    "rgbww_val_tpl": "rgbww_value_template",
    "send_cmd_t": "send_command_topic",
    "send_if_off": "send_if_off",
    "set_fan_spd_t": "set_fan_speed_topic",
    "set_pos_tpl": "set_position_template",
    "set_pos_t": "set_position_topic",
    "pos_t": "position_topic",
    "pos_tpl": "position_template",
    "spd_rng_min": "speed_range_min",
    "spd_rng_max": "speed_range_max",
    "src_type": "source_type",
    "stat_cla": "state_class",
    "stat_clsd": "state_closed",
    "stat_closing": "state_closing",
    "stat_jam": "state_jammed",
    "stat_off": "state_off",
    "stat_on": "state_on",
    "stat_open": "state_open",
    "stat_opening": "state_opening",
    "stat_stopped": "state_stopped",
    "stat_locked": "state_locked",
    "stat_locking": "state_locking",
    "stat_unlocked": "state_unlocked",
    "stat_unlocking": "state_unlocking",
    "stat_t": "state_topic",
    "stat_tpl": "state_template",
    "stat_val_tpl": "state_value_template",
    "step": "step",
    "stop_cmd_t": "stop_command_topic",
    "stop_cmd_tpl": "stop_command_template",
    "strt_mw_cmd_t": "start_mowing_command_topic",
    "strt_mw_cmd_tpl": "start_mowing_command_template",
    "stype": "subtype",
    "sug_dsp_prc": "suggested_display_precision",
    "sup_dur": "support_duration",
    "sup_vol": "support_volume_set",
    "sup_feat": "supported_features",
    "sup_clrm": "supported_color_modes",
    "swing_h_mode_cmd_tpl": "swing_horizontal_mode_command_template",
    "swing_h_mode_cmd_t": "swing_horizontal_mode_command_topic",
    "swing_h_mode_stat_tpl": "swing_horizontal_mode_state_template",
    "swing_h_mode_stat_t": "swing_horizontal_mode_state_topic",
    "swing_h_modes": "swing_horizontal_modes",
    "swing_mode_cmd_tpl": "swing_mode_command_template",
    "swing_mode_cmd_t": "swing_mode_command_topic",
    "swing_mode_stat_tpl": "swing_mode_state_template",
    "swing_mode_stat_t": "swing_mode_state_topic",
    "swing_modes": "swing_modes",
    "temp_cmd_tpl": "temperature_command_template",
    "temp_cmd_t": "temperature_command_topic",
    "temp_hi_cmd_tpl": "temperature_high_command_template",
    "temp_hi_cmd_t": "temperature_high_command_topic",
    "temp_hi_stat_tpl": "temperature_high_state_template",
    "temp_hi_stat_t": "temperature_high_state_topic",
    "temp_lo_cmd_tpl": "temperature_low_command_template",
    "temp_lo_cmd_t": "temperature_low_command_topic",
    "temp_lo_stat_tpl": "temperature_low_state_template",
    "temp_lo_stat_t": "temperature_low_state_topic",
    "temp_stat_tpl": "temperature_state_template",
    "temp_stat_t": "temperature_state_topic",
    "temp_unit": "temperature_unit",
    "tilt_clsd_val": "tilt_closed_value",
    "tilt_cmd_t": "tilt_command_topic",
    "tilt_cmd_tpl": "tilt_command_template",
    "tilt_max": "tilt_max",
    "tilt_min": "tilt_min",
    "tilt_opnd_val": "tilt_opened_value",
    "tilt_opt": "tilt_optimistic",
    "tilt_status_t": "tilt_status_topic",
    "tilt_status_tpl": "tilt_status_template",
    "tit": "title",
    "t": "topic",
    "trns": "transition",
    "tz": "timezone",
    "uniq_id": "unique_id",
    "unit_of_meas": "unit_of_measurement",
    "url_t": "url_topic",
    "url_tpl": "url_template",
    "val_tpl": "value_template",
    "whit_cmd_t": "white_command_topic",
    "whit_scl": "white_scale",
    "xy_cmd_t": "xy_command_topic",
    "xy_cmd_tpl": "xy_command_template",
    "xy_stat_t": "xy_state_topic",
    "xy_val_tpl": "xy_value_template",
    "l_ver_t": "latest_version_topic",
    "l_ver_tpl": "latest_version_template",
    "pl_inst": "payload_install",
    "vis": "visible_by_default",
}

DEVICE_ABBREVIATIONS = {
    "cns": "connections",
    "cu": "configuration_url",
    "ids": "identifiers",
    "name": "name",
    "mf": "manufacturer",
    "mdl": "model",
    "mdl_id": "model_id",
    "hw": "hw_version",
    "sw": "sw_version",
    "sa": "suggested_area",
    "sn": "serial_number",
    "cdev": "child_device",
}

ORIGIN_ABBREVIATIONS = {
    "name": "name",
    "sw": "sw_version",
    "url": "support_url",
}


def expand(fields, aliases):
    return {aliases.get(k, k): v for k, v in fields.items()}


def device_fields(fields):
    device = expand(fields, DEVICE_ABBREVIATIONS)
    if isinstance(device.get("child_device"), dict):
        device["child_device"] = device_fields(device["child_device"])
    ids = device.get("identifiers")
    if isinstance(ids, str):
        device["identifiers"] = [ids]
    elif isinstance(ids, list):
        device["identifiers"] = sorted(set(ids))
    return device


def canonical(fields):
    result = expand(fields, ABBREVIATIONS)
    if isinstance(result.get("device"), dict):
        result["device"] = device_fields(result["device"])
    if isinstance(result.get("origin"), dict):
        result["origin"] = expand(result["origin"], ORIGIN_ABBREVIATIONS)
    if "availability" in result:
        items = result["availability"]
        result["availability"] = [expand(x, ABBREVIATIONS) if isinstance(x, dict) else x
                                  for x in (items if isinstance(items, list) else [items])]
    return result


def route(value, base):
    if not isinstance(value, str) or not value:
        raise ValueError("invalid MQTT route")
    if "~" in (value[:1], value[-1:]):
        if not isinstance(base, str) or not base:
            raise ValueError("relative MQTT route lacks base")
        if value.startswith("~"):
            value = base + value[1:]
        if value.endswith("~"):
            value = value[:-1] + base
    return value


def identifiers(fields):
    device = fields.get("device", {})
    if not isinstance(device, dict):
        raise ValueError("invalid device declaration")
    ids = device.get("identifiers", [])
    if isinstance(ids, str):
        ids = [ids]
    if not isinstance(ids, list) or any(not isinstance(i, str) or not i for i in ids):
        raise ValueError("invalid device identifiers")
    return sorted(set(ids))


class Declaration(dict):
    """Keep the full metadata available for comparison and out of serialized reports."""

    def __init__(self, fields, **identity):
        super().__init__(**identity)
        self.metadata = metadata_paths(fields)


def metadata_paths(fields):
    # Serialize leaves so JSON true and 1, null and absence remain distinguishable.
    def walk(value, path):
        if isinstance(value, dict) and value:
            for key, child in value.items():
                yield from walk(child, (*path, key))
        else:
            yield path, json.dumps(value, sort_keys=True, separators=(",", ":"))

    normalized = dict(fields)
    base = normalized.pop("~", None)
    for key in normalized:
        if key == "topic" or key.endswith("_topic"):
            normalized[key] = route(normalized[key], base)
    if "availability" in normalized:
        normalized["availability"] = [
            {**item, "topic": route(item["topic"], base)} if isinstance(item, dict) and "topic" in item else item
            for item in normalized["availability"]
        ]
    return dict(walk(normalized, ()))


def entity(topic, component, domain, fields):
    ids = identifiers(fields)
    uid = fields.get("unique_id")
    routes = {k: route(v, fields.get("~")) for k, v in fields.items()
              if k.endswith(("state_topic", "command_topic")) or k == "topic"}
    item = Declaration(fields, topic=topic, component=component, domain=domain, device_ids=ids, routes=routes)
    if domain == "device_automation":
        if ids and all(isinstance(fields.get(k), str) and fields[k] for k in
                       ("type", "subtype")):
            item["trigger"] = [fields.get("automation_type", "trigger"), fields["type"],
                               fields["subtype"]]
            return (domain, tuple(ids), *item["trigger"]), item
    elif isinstance(uid, str) and uid:
        item["unique_id"] = uid
        return (domain, uid), item
    return None, item


def entities(topic, domain, raw):
    fields = canonical(raw)
    if domain != "device":
        yield entity(topic, None, domain, fields)
        return
    groups = [(fields.pop("components", {}), None)]
    for child in fields.pop("child_devices", []):
        if not isinstance(child, dict):
            raise ValueError("invalid child device")
        child = expand(device_fields(child), ABBREVIATIONS)
        groups.append((child.pop("components", {}), child))
    if not any(components for components, _ in groups):
        raise ValueError("missing device discovery components")
    for components, child_device in groups:
        if not isinstance(components, dict):
            raise ValueError("invalid device components")
        inherited = fields
        if child_device is not None:
            inherited = {**fields, "device": {**fields.get("device", {}), "child_device": child_device}}
        for component, data in components.items():
            if not isinstance(data, dict):
                raise ValueError("invalid device component")
            child = canonical(data)
            if not isinstance(child.get("platform"), str):
                raise ValueError("missing component platform")
            yield entity(topic, component, child["platform"], {**inherited, **child})


def snapshot(path, prefix):
    text = Path(path).read_text()
    if text.lstrip().startswith("["):
        rows = json.loads(text)
    else:
        rows = (json.loads(line) for line in text.splitlines() if line.strip())
    retained = {}
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("topic"), str) or "payload" not in row:
            raise ValueError("invalid discovery snapshot record")
        topic = row["topic"]
        if not topic.startswith(prefix + "/") or not topic.endswith("/config"):
            continue
        rest = topic[len(prefix) + 1:].split("/")
        if len(rest) not in (3, 4) or not all(rest):
            raise ValueError("invalid discovery topic")
        data = row["payload"]
        if data == "":
            retained.pop(topic, None)
            continue
        if isinstance(data, str):
            data = json.loads(data)
        if not isinstance(data, dict):
            raise ValueError("invalid discovery payload")
        retained[topic] = (rest[0], data)
    by_id = defaultdict(list)
    by_slot = {}
    unknown = []
    for topic, (domain, data) in sorted(retained.items()):
        for key, item in entities(topic, domain, data):
            if key is None:
                unknown.append(item)
            else:
                by_id[key].append(item)
                by_slot[topic, item["component"]] = key
    return by_id, by_slot, unknown


def statistics_mode(item):
    state_class = json.loads(item.metadata.get(("state_class",), "null"))
    if state_class is None:
        return "none"
    if state_class in ("total", "total_increasing"):
        return "sum"
    if state_class == "measurement":
        return "mean"
    if state_class == "measurement_angle":
        return "circular_mean"
    return "unknown"


def compare(old, new):
    a, a_slots, unknown_a = old
    b, b_slots, unknown_b = new
    details = {"missing": [], "new": [], "changed": [], "metadata_changes": [],
               "old_unidentified": unknown_a, "new_unidentified": unknown_b}
    for label, x in (("old_conflicts", a), ("new_conflicts", b)):
        details[label] = [items for items in x.values() if len(items) > 1]
    for key in sorted(a.keys() - b.keys()):
        details["missing"].extend(a[key])
    for key in sorted(b.keys() - a.keys()):
        details["new"].extend(b[key])
    unchanged = 0
    for key in sorted(a.keys() & b.keys()):
        if len(a[key]) != 1 or len(b[key]) != 1:
            continue
        left, right = a[key][0], b[key][0]
        changed = [k for k in ("device_ids", "topic", "component", "routes") if left[k] != right[k]]
        if changed:
            details["changed"].append({"fields": changed, "old": left, "new": right})
        fields = sorted(".".join(path) for path in left.metadata.keys() | right.metadata.keys()
                        if left.metadata.get(path) != right.metadata.get(path))
        if fields:
            difference = {"identity": key, "fields": fields}
            if key[0] == "sensor" and "state_class" in fields:
                difference["statistics_from"] = statistics_mode(left)
                difference["statistics_to"] = statistics_mode(right)
            details["metadata_changes"].append(difference)
        if not changed and not fields:
            unchanged += 1
    details["reassigned_discovery_slots"] = [
        {"topic": slot[0], "component": slot[1], "old_identity": a_slots[slot],
         "new_identity": b_slots[slot]}
        for slot in sorted(a_slots.keys() & b_slots.keys()) if a_slots[slot] != b_slots[slot]
    ]
    def all_devices(groups, unknown):
        return {i for items in [*groups.values(), unknown] for item in items for i in item["device_ids"]}
    old_devices, new_devices = all_devices(a, unknown_a), all_devices(b, unknown_b)
    details["missing_device_ids"] = sorted(old_devices - new_devices)
    details["new_device_ids"] = sorted(new_devices - old_devices)
    summary = {k: len(v) for k, v in details.items()}
    summary.update(old_entities=sum(map(len, a.values())) + len(unknown_a),
                   new_entities=sum(map(len, b.values())) + len(unknown_b),
                   unchanged=unchanged,
                   parity=bool(a) and bool(b) and not any(details.values()))
    field_counts = Counter(field for change in details["metadata_changes"] for field in change["fields"])
    summary["metadata_field_counts"] = dict(sorted(field_counts.items()))
    for field in ("device_ids", "topic", "component", "routes"):
        summary["changed_" + field] = sum(field in c["fields"] for c in details["changed"])
    return summary, details


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("old")
    parser.add_argument("new")
    parser.add_argument("--prefix", default="homeassistant")
    parser.add_argument("--details", help="new, private JSON file for exact differences")
    parser.add_argument("--require-match", action="store_true")
    args = parser.parse_args()
    try:
        summary, details = compare(snapshot(args.old, args.prefix), snapshot(args.new, args.prefix))
        if args.details:
            fd = os.open(args.details, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as f:
                json.dump({"summary": summary, "differences": details}, f, indent=2)
                f.write("\n")
    except (ValueError, TypeError, OSError) as e:
        # Do not include source text, payload values or household topic names.
        parser.exit(2, f"discovery parity failed ({type(e).__name__}); check inputs and output path\n")
    print(json.dumps(summary, sort_keys=True, indent=2))
    return int(args.require_match and not summary["parity"])


if __name__ == "__main__":
    sys.exit(main())
