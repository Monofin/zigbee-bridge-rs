"""Owner-scoped MQTT discovery snapshots and non-retained, correlated health checks."""
import base64
import json
from itertools import islice
import secrets
import time

import paho.mqtt.client as mqtt

from stage import check


class Broker:
    def __init__(self, run):
        self.run = run
        cfg = json.loads((run / "broker.json").read_bytes())
        self.base, self.prefix = cfg["base_topic"], cfg["discovery_prefix"]
        self.client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="bridge-trial-" + secrets.token_hex(8))
        if "username" in cfg:
            self.client.username_pw_set(cfg["username"], (run / cfg["password_file"]).read_text())
        if cfg["tls"]:
            self.client.tls_set()
        self.client.connect(cfg["host"], cfg["port"], keepalive=45)
        self.wait(self.client.is_connected, 8)
        self.acks = set()
        self.client.on_subscribe = self.subscribed

    def subscribed(self, client, userdata, mid, reasons, props):
        check(all(not r.is_failure for r in reasons), "broker denied subscription")
        self.acks.add(mid)

    def wait(self, done, seconds):
        deadline = time.monotonic() + seconds
        while not done():
            check(time.monotonic() < deadline, "MQTT operation timed out")
            check(self.client.loop(timeout=0.15) == mqtt.MQTT_ERR_SUCCESS, "MQTT connection lost")

    def sub(self, topics):
        rc, mid = self.client.subscribe([(t, 0) for t in topics])
        check(rc == 0, "subscribe failed")
        self.wait(lambda: mid in self.acks, 10)

    def capture(self):
        wanted = {json.loads(l)["topic"] for l in (self.run / "candidate.jsonl").read_text().splitlines()}
        owners = {"zigbee2mqtt_" + a for a in json.loads((self.run / "owners.json").read_bytes())}
        dep = json.loads((self.run / "deployment.json").read_bytes())
        owners.add("zigbee2mqtt_bridge_0x" + dep["coordinator"])
        configs, info = {}, {}
        def received(client, userdata, msg):
            if not msg.retain:
                return
            if msg.topic == self.base + "/bridge/info":
                info.update(json.loads(msg.payload))
                return
            if msg.topic.endswith("/config"):
                try:
                    doc = json.loads(msg.payload)
                    dev = doc.get("device", doc.get("dev", {}))
                    check(isinstance(dev, dict), "invalid discovery device")
                except (ValueError, AttributeError):
                    check(msg.topic not in wanted, "invalid retained document at a candidate topic")
                    return
                ids = dev.get("identifiers", dev.get("ids", []))
                if isinstance(ids, str):
                    ids = [ids]
                owned = bool(owners.intersection(ids))
                check(owned or msg.topic not in wanted, "candidate discovery topic already belongs to another publisher")
                if owned:
                    configs[msg.topic] = base64.b64encode(msg.payload).decode()
        self.client.on_message = received
        self.sub([self.prefix + "/+/+/config", self.prefix + "/+/+/+/config", self.base + "/bridge/info"])
        # Mosquitto sends retained QoS 0 packets before the SUBACK for the
        # following subscription on this connection. Nothing is published.
        self.sub(["bridge-trial-sync/" + secrets.token_hex(16)])
        check(configs and info, "broker did not return discovery and bridge information")
        return configs, info

    def publish_retained_batch(self, changes):
        # The same network loop owns each receipt. Cap the number outstanding.
        changes = iter(changes)
        while batch := list(islice(changes, 16)):
            receipts = []
            for topic, body in batch:
                receipt = self.client.publish(topic, body, qos=1, retain=True)
                check(receipt.rc == 0, "retained publish failed")
                receipts.append(receipt)
            self.wait(lambda: all(r.is_published() for r in receipts), 10)

    def publish(self, topic, body, retain=False):
        ack = self.client.publish(topic, body, qos=1, retain=retain)
        check(ack.rc == 0, "broker publish failed")
        self.wait(ack.is_published, 10)

    def health(self, candidate, timeout):
        namespace = "/bridge/v1/" if candidate else "/bridge/"
        suffix = "health_check"
        response = self.base + namespace + "response/" + suffix
        token = secrets.token_hex(12)
        answers = []
        def received(client, userdata, msg):
            if not msg.retain and msg.topic == response:
                doc = json.loads(msg.payload)
                if doc.get("transaction") == token:
                    answers.append(doc)
        self.client.on_message = received
        self.sub([response])
        self.publish(self.base + namespace + "request/" + suffix, json.dumps({"transaction": token}))
        self.wait(lambda: bool(answers), timeout)
        check(answers[0].get("status") == "ok" and answers[0].get("data", {}).get("healthy") is True,
              "coordinator health check failed")

    def close(self):
        self.client.disconnect()
