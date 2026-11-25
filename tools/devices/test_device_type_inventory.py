import unittest

from device_type_inventory import fingerprint_groups, unindexed_selectors
from zha import Module
from zha_type_evidence import Signatures


class DescriptorEvidence(unittest.TestCase):
    def test_unindexed_refs_keep_alternatives_and_never_promote_a_partial_endpoint(self):
        ep = {"ID": 2, "profileID": 0xc05e, "deviceID": 0x1000}
        groups = [
            {"endpoints": [{"profileID": 0xc05e}, ep]},
            {"endpoints": [ep, {"ID": 1, "profileID": 0x0104, "deviceID": 0x0100}]},
        ]
        refs = list(unindexed_selectors(groups, {(0x0104, 0x0100)}, "zha", 42))
        self.assertEqual(refs, [
            ((0xc05e, 0x1000), {"source":"zha", "source_row":42, "selector":0, "endpoint":1}),
            ((0xc05e, 0x1000), {"source":"zha", "source_row":42, "selector":1, "endpoint":0}),
        ])

    def test_selector_alternatives_and_cluster_list_absence_survive(self):
        row = {"source":"z2m", "fingerprints":[{"modelID":"A", "endpoints":[{"ID":1,"inputClusters":[]}] }],
               "white_labels":[{"vendor":"Alias","fingerprint":[{"modelID":"B", "endpoints":[{"ID":2}]}]}]}
        groups, warnings = fingerprint_groups(row, None)
        self.assertEqual(warnings, [])
        self.assertEqual([g["conditions"]["modelID"] for g in groups], ["A", "B"])
        self.assertEqual(groups[1]["context"]["vendor"], "Alias")
        self.assertEqual(groups[0]["endpoints"][0]["inputClusters"], [])
        self.assertNotIn("outputClusters", groups[0]["endpoints"][0])

    def test_v1_signature_is_separate_from_replacement_and_dynamic_mutation(self):
        import ast
        mod = Module.parse("quirks/demo.py", """
class Product:
    signature = {"endpoints": {1: {"profile_id": 260, "device_type": 6, "input_clusters": [6, dynamic()]}}}
    replacement = {"endpoints": {1: {"device_type": 1000}}}
class Derived(Product):
    pass
class Edited(Product):
    signature = Product.signature.copy()
    signature["endpoints"][1]["device_type"] = 99
""")
        source = Signatures.__new__(Signatures)
        source.modules = {mod.name:mod}
        source.module_aliases = {}
        source.classes = {(mod.name, cls.name):cls for cls in mod.body if isinstance(cls,ast.ClassDef)}
        endpoints, warnings = source.endpoints({"kind":"v1","path":mod.path,"symbol":"Derived"})
        self.assertEqual(endpoints, [{"ID":1,"profileID":260,"deviceID":6,"inputClustersKnown":[6]}])
        self.assertTrue(warnings)
        self.assertIsNone(source.endpoints({"kind":"v1","path":mod.path,"symbol":"Edited"})[0])

    def test_deconz_dynamic_endpoint_remains_unidentified(self):
        row = {"source":"deconz","match":{},"subdevices":[
            {"fingerprint":{"endpoint":"$endpoint","profile":"0x0104","in":["0x0006"]}}]}
        ep = fingerprint_groups(row, None)[0][0]["endpoints"][0]
        self.assertEqual(ep, {"ID":None, "IDExpression":"$endpoint", "profileID":260, "inputClusters":[6]})
