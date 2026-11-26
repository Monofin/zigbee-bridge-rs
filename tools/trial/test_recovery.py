"""Exercise the handover state machine without Docker or a physical coordinator."""
import base64
import json
import os
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, patch

os.environ.setdefault("BRIDGE_TRIAL_ROOT", "/tmp/bridge-test")
import trial


class DiagnosticRouteTests(unittest.TestCase):
    def test_only_same_device_diagnostic_route_can_move(self):
        base = "house/zigbee"
        ieee = "0x0000000000000007"
        old = dict(domain="sensor", device_ids=["zigbee2mqtt_" + ieee],
                   routes={"state_topic": base + "/room/light"})
        for key in ("linkquality", "last_seen"):
            old["unique_id"] = f"{ieee}_{key}_{base}"
            new = dict(old, routes={"state_topic": base + "/bridge/diagnostics/room/light"})
            change = dict(fields=["routes"], old=old, new=new)
            self.assertTrue(trial.diagnostic_move(change, base))
            self.assertFalse(trial.diagnostic_move(dict(change, fields=["device_ids", "routes"]), base))
            new["routes"]["state_topic"] += "-other"
            self.assertFalse(trial.diagnostic_move(change, base))
        old["unique_id"] = f"{ieee}_battery_{base}"
        self.assertFalse(trial.diagnostic_move(dict(change, new=dict(old)), base))


    def test_light_feedback_move_preserves_owner_property_and_control(self):
        base = "house/zigbee"
        route = base + "/room/light"
        old = dict(domain="light", device_ids=["zigbee2mqtt_0x0000000000000007"],
                   topic="ha/light/device/light/config",
                   routes={"state_topic": route, "command_topic": route + "/set"})
        feedback = base + "/bridge/feedback/room/light/@light"
        new = dict(old, routes=dict(old["routes"], state_topic=feedback))
        change = dict(fields=["routes"], old=old, new=new)
        self.assertTrue(trial.feedback_move(change, base))
        for key, value in [("state_topic", feedback.replace("/room/", "/hall/")),
                           ("state_topic", feedback.replace("@light", "update")),
                           ("command_topic", route + "/set/other")]:
            wrong = dict(new, routes=dict(new["routes"], **{key: value}))
            self.assertFalse(trial.feedback_move(dict(change, new=wrong), base))
        self.assertFalse(trial.feedback_move(dict(change, fields=["routes", "unique_id"]), base))


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        self.run_dir = root / "runs" / "fixture"
        self.run_dir.mkdir(parents=True)
        marker = root / "handover.json"
        marker.write_text(json.dumps({"run": str(self.run_dir)}))
        self.marker = marker
        (self.run_dir / "deployment.json").write_text('{"container_id":"fixture-id"}')
        self.enterContext(patch.multiple(trial, ROOT=root, MARKER=marker))
        self.enterContext(patch.object(trial.subprocess, "check_output", return_value="fixture-id"))
        self.start_owner = self.enterContext(patch.object(trial.subprocess, "run"))

    def test_interrupted_exclusive_repair_is_joined_before_owner_start(self):
        calls = []
        repair = Mock(exitcode=None)
        repair.is_alive.return_value = True
        repair.kill.side_effect = lambda: calls.append("kill")
        repair.join.side_effect = lambda *args: calls.append(("join", args))
        ctx = self.enterContext(patch.object(trial.multiprocessing, "get_context"))
        ctx.return_value.Process.return_value = repair
        self.enterContext(patch.object(trial, "running", return_value=False))
        self.start_owner.side_effect = lambda *a, **kw: calls.append("start")
        self.enterContext(patch.object(trial, "health", side_effect=lambda *a: calls.append("health")))
        def verify(run, *, verify_only):
            self.assertTrue(verify_only)
            calls.append("readback")
            raise ValueError("discovery repair outstanding")
        self.enterContext(patch.object(trial, "restore", side_effect=verify))
        with self.assertRaises(ValueError):
            trial.recover()
        self.assertEqual(calls, [("join",(15,)), "kill", ("join",()), "start", "health", "readback"])
        self.assertTrue(self.marker.exists())

    def test_recovery_reentry_with_active_owner_only_verifies(self):
        self.enterContext(patch.object(trial, "running", return_value=True))
        self.enterContext(patch.object(trial, "health"))
        readback = self.enterContext(patch.object(trial, "restore"))
        trial.recover()
        readback.assert_called_once_with(self.run_dir, verify_only=True)
        self.assertFalse(self.marker.exists())
        self.start_owner.assert_not_called()

    def test_household_size_and_payload_rejection(self):
        baseline = {f"ha/sensor/{i}/config": "eA==" for i in range(1038)}
        (self.run_dir / "original-retained.json").write_text(json.dumps(baseline))
        (self.run_dir / "candidate.jsonl").write_text('{"topic":"candidate-only"}\n')
        current = dict.fromkeys(baseline, "eQ==")
        current["candidate-only"] = "eQ=="
        broker = Mock()
        broker.capture.side_effect = lambda: (dict(current), {})
        calls = []
        def publish(changes):
            for topic, body in changes:
                calls.append(topic)
                if topic == "ha/sensor/0/config":
                    continue  # Successful v3 PUBACK with publication refused by the broker.
                if body:
                    current[topic] = base64.b64encode(body).decode()
                else:
                    del current[topic]
        broker.publish_retained_batch.side_effect = publish
        self.enterContext(patch.object(trial, "Broker", return_value=broker))
        with self.assertRaisesRegex(ValueError, "readback differs"):
            trial.restore(self.run_dir)
        self.assertEqual(len(calls), 1039)
        calls.clear()
        with self.assertRaises(ValueError):
            trial.restore(self.run_dir, verify_only=True)
        self.assertEqual(calls, [])
        self.assertEqual(broker.capture.call_count,3)


if __name__ == "__main__":
    unittest.main()
