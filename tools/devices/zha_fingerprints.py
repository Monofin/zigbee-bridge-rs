"""Read the endpoint and commissioning selectors reviewed at the ZHA catalogue pin.

Only signature declarations are data. Replacements, friendly names and entity
builders describe behaviour after a match and do not supply interview evidence.
"""
import ast


REVISION = "b07fa76876e081d48744968f9fb806e7f1fee0ee"

# Wire values used by these signatures, from zigpy 2.4.0 (upstream uv.lock)
# and the custom cluster classes in the same ZHA commit.
VALUES = {
    "zha.PROFILE_ID": 0x0104, "zll.PROFILE_ID": 0xc05e, "zgp.PROFILE_ID": 0xa1e0,
    "XBEE_PROFILE_ID": 0xc105, "zgp.DeviceType.PROXY_BASIC": 0x61,
    "DeviceType.EXTENDED_COLOR_LIGHT": 0x210, "DeviceType.TEMPERATURE_SENSOR": 0x302,
    "zha.DeviceType.ON_OFF_SWITCH": 0, "zha.DeviceType.LEVEL_CONTROL_SWITCH": 1,
    "zha.DeviceType.MAIN_POWER_OUTLET": 9, "zha.DeviceType.SIMPLE_SENSOR": 12,
    "zha.DeviceType.METER_INTERFACE": 0x53, "zha.DeviceType.DIMMABLE_LIGHT": 0x101,
    "zha.DeviceType.IAS_ZONE": 0x402, "DeviceID.OnOffSwitch": 2, "DeviceID.GenericSwitch": 7,
}
CLUSTERS = {
    "Basic": 0, "PowerConfiguration": 1, "Identify": 3, "Groups": 4, "Scenes": 5,
    "OnOff": 6, "LevelControl": 8, "Alarms": 9, "Time": 10, "TrackingCluster": 15,
    "Commissioning": 0x15, "Ota": 0x19, "PollControl": 0x20, "GreenPowerProxy": 0x21,
    "Fan": 0x202, "Color": 0x300, "TemperatureMeasurement": 0x402,
    "RelativeHumidity": 0x405, "IasZone": 0x500, "Metering": 0x702,
    "MeterIdentification": 0xb01, "ElectricalMeasurement": 0xb04, "Diagnostic": 0xb05,
    "LightLink": 0x1000, "FastPollingPowerConfigurationCluster": 1,
    "CustomPowerConfigurationCluster": 1, "SmartThingsAccelCluster": 0xfc02,
    "TuyaTemperatureHumidityAlarmCluster": 0xe002, "TuyaManufCluster": 0xef00,
    "ZLinkyTICManufacturerCluster": 0xff66,
}
FIELDS = {"endpoints": "endpoints", "device_type": "deviceID", "profile_id": "profileID",
          "input_clusters": "inputClusters", "output_clusters": "outputClusters"}


def value(node):
    if isinstance(node, ast.Constant):
        return node.value
    if isinstance(node, ast.List):
        return [value(el) for el in node.elts]
    if isinstance(node, ast.Attribute) and node.attr == "cluster_id":
        return CLUSTERS[node.value.id]
    return VALUES[ast.unparse(node)]


def key(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, int):
        return node.value
    name = node.id if isinstance(node, ast.Name) else node.value
    return FIELDS[name.lower()]


def descriptor(expr):
    fields = {key(k): value(v) for k, v in zip(expr.keys, expr.values)}
    # ZHA signature_matches treats omitted cluster lists as empty.
    return {"inputClusters": [], "outputClusters": [], **fields}


def endpoints(expr):
    for k, v in zip(expr.keys, expr.values):
        if (k.id if isinstance(k, ast.Name) else k.value).lower() == "endpoints":
            return {key(ep): descriptor(desc) for ep, desc in zip(v.keys, v.values)}
    raise ValueError("missing ENDPOINTS")


def subscript(node):
    path = []
    while isinstance(node, ast.Subscript):
        path.append(node.slice)
        node = node.value
    if not isinstance(node, ast.Name) or node.id != "signature":
        return []
    return [key(part) for part in reversed(path)]


def class_endpoints(mod, name):
    """Build a fresh descriptor for each class and apply its own signature edits."""
    cls = next(stmt for stmt in mod.body if isinstance(stmt, ast.ClassDef) and stmt.name == name)
    for stmt in cls.body:
        if isinstance(stmt, ast.Assign):
            target, = stmt.targets
            if isinstance(target, ast.Name) and target.id == "signature":
                expr = stmt.value
                if isinstance(expr, ast.Dict):
                    result = endpoints(expr)
                elif (isinstance(expr, ast.Call) and mod.imports.get(expr.func.id) == ("copy", "deepcopy")
                      and expr.args[0].attr == "signature"):
                    result = class_endpoints(mod, expr.args[0].value.id)
                else:
                    raise ValueError("unsupported signature declaration")
            elif path := subscript(target):
                field, ep, attr = path
                if field != "endpoints":
                    raise ValueError(path)
                result[ep][attr] = value(stmt.value)
        elif isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            call = stmt.value
            path = subscript(call.func.value) if isinstance(call.func, ast.Attribute) else []
            if path:
                field, ep, attr = path
                if field != "endpoints" or call.func.attr != "insert":
                    raise ValueError(path)
                result[ep][attr].insert(*map(value, call.args))
    return result


def fingerprints(mod, row):
    symbol = row["symbol"]
    if row["models"] and mod.name != "zhaquirks.lixee.zlinky":
        return []
    if "@" in symbol:
        line = row["line"]
        expr = next(n for n in ast.walk(ast.Module(body=mod.body, type_ignores=[]))
                    if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute)
                    and n.func.attr == "add_to_registry" and n.lineno == line)
        steps = []
        while isinstance(expr, ast.Call) and isinstance(expr.func, ast.Attribute):
            steps.append((expr.func.attr, expr))
            expr = expr.func.value
        if symbol.startswith("GreenPowerQuirkBuilder@"):
            selector, = (c for method, c in steps if method == "applies_to")
            device, = selector.keywords
            if device.arg != "device_id":
                raise ValueError(device.arg)
            return [{"greenPowerDeviceID": value(device.value), "sourceIDRange": list(map(value, c.args))}
                    for method, c in reversed(steps) if method == "src_id_range"]
        predicate, = (c.args[0] for method, c in steps if method == "filter")
        if predicate.func.id != "signature_matches":
            raise ValueError("unsupported builder filter")
        eps = endpoints(mod.bindings[predicate.args[0].id])
    else:
        eps = class_endpoints(mod, symbol)
    rule = {"endpoints": [{"ID": ep, **desc} for ep, desc in eps.items()]}
    if row["manufacturers"]:
        rule["manufacturerName"], = row["manufacturers"]
    if row["models"]:
        rule["modelID"], = row["models"]
    return [rule]
