import ast
import unittest

import function_inventory as inventory
import function_batch as batches
import light_index


class InventoryTests(unittest.TestCase):
    def test_expose_access_and_list_paths_survive(self):
        exposure = {"type": "composite", "property": "value", "features": [
            {"type": "list", "property": "points", "access": 5, "item_type": {"type": "numeric", "value_min": 0}}
        ]}
        root, values, item = inventory.expose_tree(exposure)
        self.assertEqual(root["parents"], ())
        self.assertEqual(values["access"], 5)
        self.assertEqual(item, {"parents": ("value", "points", "[]"), "type": "numeric", "value_min": 0})

    def test_anonymous_converters_keep_their_own_identity(self):
        row = {"exposes": [], "from_zigbee": [{"symbol": None, "cluster": 6}] * 2,
               "to_zigbee": [], "hooks": [], "tuya_datapoints": [], "dynamic_exposes": True,
               "path": "a.ts", "symbol": "definitions[1]"}
        first, second, dynamic = inventory.z2m_items(row)
        self.assertNotEqual(first, second)
        self.assertEqual(dynamic[0], "dynamic_exposes")

    def test_zha_keeps_both_chained_operations(self):
        expr = "(QuirkBuilder('a', 'b')\n .sensor(1)\n .sensor(2)\n .add_to_registry())"
        scan = inventory.Zha.__new__(inventory.Zha)
        scan.nodes = {"a.py": {(1, "builder"): ast.parse(expr).body[0].value}}
        row = {"kind": "v2", "path": "a.py", "line": 1, "symbol": "QuirkBuilder@1"}
        lines = sorted(sig["line"] for kind, sig in scan.items(row) if kind == "builder_step")
        self.assertEqual(lines, [2, 3])

    def test_deconz_false_static_value_survives(self):
        row = {"match": {}, "subdevices": [{"type": "plug", "items": [{"name": "on", "static": False}]}]}
        self.assertIn(("static", {"item": "on", "value": False}), inventory.deconz_items(row))


    def test_batch_uses_effective_access_and_keeps_all_fingerprint_constraints(self):
        shape = {"type": "composite", "access": 1, "features": [{"type": "numeric", "access": 7, "property": "x"}]}
        fp = {"manufacturerName": "maker", "modelID": "x", "softwareBuildID": "42", "endpoints": [{"ID": 1}]}
        row = {"source": "z2m", "exposes": [shape], "dynamic_exposes": False, "fingerprints": [fp],
               "white_labels": [], "zigbee_models": [], "from_zigbee": [{"symbol": "fz.x"}]}
        self.assertEqual(batches.candidate(row, "x", "set")[1], "no_declared_access")
        result, reason = batches.candidate(row, "x", "publish")
        self.assertIsNone(reason)
        self.assertEqual(result["selectors"], [{"fingerprint": fp}])
        row["fingerprints"] = []
        self.assertEqual(batches.candidate(row, "x", "publish")[1], "model_only_or_incomplete_selector")

    def test_batch_does_not_infer_access_from_prose_or_quirk_name(self):
        self.assertEqual(batches.candidate({"source": "zha"}, "state", "set")[1], "access_requires_quirk_review")
        row = {"source": "deconz", "subdevices": [{"type": "plug", "items": [{"name": "state/on", "read": {}}],
                                                    "uuid": [1]}], "match": {"requires_review": True}}
        self.assertEqual(batches.candidate(row, "state/on", "set")[1], "no_declared_access")
        self.assertEqual(batches.candidate(row, "state/on", "get")[1], "match_expression_requires_review")


    def test_light_source_grammar_excludes_lookalike_quirk_and_side_effect(self):
        plain = light_index.HEADER + """
    { zigbeeModel: ["lamp "], model: "lamp", vendor: "Maker", description: "example", extend: [m.light()], },
];"""
        self.assertEqual(light_index.vanilla_module(plain), [(["lamp "], "lamp", "Maker")])
        self.assertFalse(light_index.vanilla_module(plain.replace("m.light()", "m.light({turnsOffAtBrightness1:true})")))
        self.assertFalse(light_index.vanilla_module(plain + "\ndefinitions[0].meta = {foo:true};"))
        self.assertFalse(light_index.vanilla_module(plain.replace("extend:", "onEvent: fn, extend:")))

    def test_light_access_needs_the_actual_converter_and_message_type(self):
        row = {"exposes": [{"type": "light", "features": [
                   {"property": "state", "access": 7, "value_on":"ON", "value_off":"OFF"},
                   {"property": "brightness", "access": 7, "value_min":0, "value_max":254}]}],
               "from_zigbee": [{"symbol":"fz.on_off","cluster":"genOnOff","message_types":"attributeReport"},
                               {"symbol":"fz.brightness","cluster":"genLevelCtrl","message_types":"readResponse"}],
               "to_zigbee": [{"symbol":"tz.light_onoff_brightness","keys":["state","brightness"],"set":True,"get":True}]}
        permission = light_index.access(row)
        self.assertEqual(permission["state"], {"report":True,"read":False,"write":True})
        self.assertEqual(permission["brightness"], {"report":False,"read":True,"write":True})


if __name__ == "__main__":
    unittest.main()
