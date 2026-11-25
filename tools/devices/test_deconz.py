import unittest

from deconz import rows


class DdfCatalogueTest(unittest.TestCase):
    def test_positional_identity_pairs_and_conditional_handlers(self):
        path = "devices/test/fixture.json"
        generic = "devices/generic/items/on.json"
        docs = {
            generic: {"schema": "resourceitem1.schema.json", "id": "state/on",
                      "parse": {"fn": "zcl:attr", "script": "../generic/rules/on.js"}},
            "devices/generic/constants.json": {"schema": "constants1.schema.json",
                                               "manufacturers": {"$MF_ONE": "One"}},
            path: {"schema": "devcap1.schema.json", "uuid": "fixture", "status": "Draft",
                   "manufacturername": ["$MF_ONE", "Other"], "modelid": ["A", "B"],
                   "matchexpr": "R.endpoints.length === 2",
                   "subdevices": [{"type": "Switch", "restapi": "/lights", "uuid": ["0x01"],
                                   "fingerprint": {"endpoint": "0x01", "in": ["0x0006"]},
                                   "items": [{"name": "state/on", "read": {"fn": "none"}},
                                             {"name": "state/on", "static": False},
                                             {"name": "state/on", "parse": {"eval": "Item.val = 1"}}]}]},
        }
        first, second = rows("rev", docs, {"devices/generic/rules/on.js"})
        self.assertEqual((first["manufacturers"], first["models"]), (["One"], ["A"]))
        self.assertEqual((second["manufacturers"], second["models"]), (["Other"], ["B"]))
        self.assertTrue(first["match"]["requires_review"])
        configured, static, override = first["subdevices"][0]["items"]
        self.assertEqual(configured["parse"]["script"],
                         {"path": "devices/generic/rules/on.js", "present": True})
        self.assertEqual(configured["read"]["path"], path)
        self.assertNotIn("parse", static)
        self.assertEqual(override["parse"]["pointer"], "/subdevices/0/items/2/parse")
        self.assertNotIn("script", override["parse"])
        docs[path]["modelid"].append("C")
        with self.assertRaisesRegex(ValueError, "unequal"):
            list(rows("rev", docs, set()))


if __name__ == "__main__":
    unittest.main()
