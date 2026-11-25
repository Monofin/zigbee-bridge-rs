import json
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

import discovery_parity as parity


class DiscoveryParity(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.prefix = "ha/test"

    def write(self, name, records, array=False):
        path = self.root / name
        docs = [{"topic": self.prefix + "/" + topic + "/config", "payload": payload}
                for topic, payload in records]
        path.write_text(json.dumps(docs) if array else "\n".join(map(json.dumps, docs)))
        return path

    def compare(self, old, new):
        return parity.compare(parity.snapshot(self.write("old", old), self.prefix),
                              parity.snapshot(self.write("new", new, True), self.prefix))

    def test_composite_routes_identifiers_and_tombstones(self):
        old = [("device/c", {"dev": {"ids": ["b", "a"]}, "~": "z/room", "stat_t": "~",
                             "cmps": {"light": {"p": "light", "uniq_id": "id1",
                                                 "bri_cmd_t": "~/set"}}}),
               ("sensor/removed", {"unique_id": "gone"}), ("sensor/removed", "")]
        new = [("device/c", {"device": {"identifiers": ["a", "b"]}, "components": {
            "light": {"platform": "light", "unique_id": "id1", "state_topic": "z/room",
                      "brightness_command_topic": "z/room/set"}}})]
        summary, _ = self.compare(old, new)
        self.assertTrue(summary["parity"])
        self.assertEqual(summary["unchanged"], 1)
        new[0][1]["components"]["light"]["brightness_command_topic"] += "/oops"
        summary, _ = self.compare(old, new)
        self.assertFalse(summary["parity"])
        self.assertEqual(summary["changed_routes"], 1)

    def test_changed_slot_and_conflicts_are_visible(self):
        old = [("switch/test", {"uniq_id": "old", "dev": {"ids": "d"}}),
               ("sensor/dup", {"unique_id": "old"}),
               ("sensor/another", {"unique_id": "old"})]
        new = [("switch/test", {"uniq_id": "new", "dev": {"ids": "d2"}})]
        s, _ = self.compare(old, new)
        self.assertEqual((s["missing"], s["new"], s["reassigned_discovery_slots"]), (3, 1, 1))
        self.assertEqual((s["old_conflicts"], s["missing_device_ids"], s["new_device_ids"]), (1, 1, 1))
        self.assertFalse(s["parity"])

    def test_semantic_trigger_and_unidentified(self):
        trigger = {"atype": "trigger", "dev": {"ids": "remote"}, "type": "press",
                   "stype": "left", "t": "z/remote/action"}
        old = [("device_automation/x", trigger), ("sensor/incomplete", {})]
        new = [("device_automation/y", trigger)]
        s, _ = self.compare(old, new)
        self.assertEqual((s["old_unidentified"], s["changed_topic"], s["changed_device_ids"]), (1, 1, 0))

    def test_metadata_changes_survive_aliases_and_redact_values(self):
        before = {"uniq_id": "valve", "stat_cla": "measurement", "dev": {"mf": "maker"},
                  "o": {"sw": "one"}, "avty": [{"t": "z/status"}]}
        after = {"unique_id": "valve", "state_class": "measurement", "device": {"manufacturer": "maker"},
                 "origin": {"sw_version": "one"}, "availability": [{"topic": "z/status"}]}
        def compare():
            return self.compare([("sensor/v", before)], [("sensor/v", after)])
        self.assertTrue(compare()[0]["parity"])
        del after["state_class"]
        after["origin"]["sw_version"] = "PRIVATE_SENTINEL"
        after["future_option"] = None
        summary, details = compare()
        self.assertEqual(summary["metadata_field_counts"],
                         {"future_option": 1, "origin.sw_version": 1, "state_class": 1})
        self.assertEqual((summary["unchanged"], summary["changed_routes"]), (0, 0))
        change, = details["metadata_changes"]
        self.assertEqual((change["statistics_from"], change["statistics_to"]), ("mean", "none"))
        self.assertNotIn("PRIVATE_SENTINEL", json.dumps(details))

    def test_child_component_is_compared(self):
        def config(cls):
            return [("device/parent", {"dev": {"ids": "parent"}, "child_devices": [
                {"ids": "child", "cmps": {"counter": {"p": "sensor", "uniq_id": "counter", "stat_cla": cls}}}]})]
        summary, _ = self.compare(config("measurement"), config("total_increasing"))
        self.assertEqual(summary["metadata_field_counts"], {"state_class": 1})

    def test_private_details_and_gate(self):
        old = self.write("a", [("switch/a", {"uniq_id": "s", "cmd_t": "z/set",
                                               "password": "PRIVATE_SENTINEL"})])
        new = self.write("b", [])
        details = self.root / "private.json"
        cmd = [sys.executable, str(Path(parity.__file__)), str(old), str(new),
               "--prefix", self.prefix, "--details", str(details), "--require-match"]
        result = subprocess.run(cmd, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("z/set", result.stdout)
        self.assertNotIn("PRIVATE_SENTINEL", details.read_text())
        self.assertEqual(stat.S_IMODE(details.stat().st_mode), 0o600)
        self.assertEqual(subprocess.run(cmd, capture_output=True).returncode, 2)


if __name__ == "__main__":
    unittest.main()
