# MQTT delivery and recovery for the Zigbee bridge

Accepted design · 23 November 2026 · updated 24 November 2026 for MQTT 5 transport

## Evidence from existing implementations

| Source | Pattern worth using |
| --- | --- |
| Eclipse Mosquitto | Independent controls govern whole packets, publish bodies, queued bytes, queued messages and in-flight work. A broker can complete a v3 QoS 1 exchange for a publish body it declines to forward. Choose a limit for the resource it protects and check retained repair by reading it back. [Broker configuration](https://mosquitto.org/man/mosquitto-conf-5.html) |
| Home Assistant | Its MQTT client records desired subscriptions, groups duplicate requests, batches ordinary filters, waits for subscription acknowledgements and rebuilds subscriptions on reconnect. HA also documents pacing discovery after its birth announcement. These patterns preserve our existing entity identifiers and work with slash-containing names. [Client implementation](https://github.com/home-assistant/core/blob/dev/homeassistant/components/mqtt/client.py) · [Discovery lifecycle](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery) |
| Zigbee2MQTT | The state cache excludes actions and button fields and has separate controls for persistence and startup publication. Its MQTT client also subscribes to the whole base tree, which would recreate our trial failure with our current reader. Use device definitions to classify fields once and reuse that classification for caching and publishing. [State cache](https://github.com/Koenkk/zigbee2mqtt/blob/master/lib/state.ts) · [Client](https://github.com/Koenkk/zigbee2mqtt/blob/master/lib/mqtt.ts) · [Cache settings](https://www.zigbee2mqtt.io/guide/configuration/all-settings.html#cache-state) |
| AWS IoT Device Shadows | Reported state has a version, and a newer cumulative version supersedes an older one. We can use that rule internally in redb without changing a single HA topic or JSON field. [Version and message ordering](https://docs.aws.amazon.com/iot/latest/developerguide/iot-device-shadows.html) |
| Eclipse Paho and rumqttc | Paho stops admitting work and allows time for operations already in flight to complete. Our pinned rumqttc supports reconnect through continued polling and exposes pending requests. The bridge must own the lifetime of that poll loop and decide which application work remains valid across a new session. [Paho disconnect](https://eclipse.dev/paho/files/javadoc/org/eclipse/paho/client/mqttv3/MqttAsyncClient.html) · [rumqttc 0.25.1 event loop](https://docs.rs/rumqttc/0.25.1/rumqttc/struct.EventLoop.html) |

## Shutdown and handover

1. Close command admission. Finish accepted operations where possible. Persist an indeterminate outcome for any journaled physical action interrupted after dispatch. Stop radio callbacks, join the serial worker and verify port release before starting the next owner.
2. Give pending MQTT receipts a bounded drain. Track the last forward progress and enforce an absolute deadline. Flush the latest reported values on shutdown. Record owned discovery deletions when accepted. Have the active session publish its offline marker before a clean disconnect, and keep its retained Last Will for abnormal disconnects. Fence the retiring publisher before its replacement can publish online on the shared topic.
3. Restart Z2M as soon as it has exclusive radio and publisher ownership. Track radio recovery and discovery reconciliation separately. Let Z2M own shared discovery topics once active. Do candidate-only tombstones only where exclusive ownership can be established. Readback is verification, not an atomic compare-and-set. Where ownership can overlap, use an exclusive handover phase or the active owner's republish path. A timeout leaves a durable repair record for a later pass.
4. During ordinary operation, a broker outage leaves the joined radio running and MQTT visibly degraded. Use cancellable backoff for connection failures. For a persistent packet or configuration error, surface the topic/filter if known, size, direction and policy. Avoid an immediate reconnect loop on the same retained packet. Keep metrics available locally while MQTT is down.

## Device activity and light state

The device topic carries merged appliance readings. Radio reception updates `last_seen` and `linkquality` on `<base>/bridge/diagnostics/<device>`. These topics drain independently and use the same checkpoint and device identity. Home Assistant discovery binds each state subscription to `<base>/bridge/feedback/<device-or-endpoint>/<field>`. A new report publishes only the affected fields. For example, a firmware query publishes `update`, and a humidity measurement publishes `humidity`. Consumers of the aggregate device and diagnostic topics keep their existing routes. The bridge keeps command topics, entity IDs and action payloads stable.

A JSON light consumes the compound `@light` feed. Its first publication requires an On/Off confirmation in this radio session. Brightness, colour or an effect received first waits for power readback. Once power is known, each lighting report carries the observed lighting fields together. Firmware queries and radio diagnostics leave that feed quiet. Thermostat mode, temperature and setpoint have independent feedback routes. The `@barrier` feed combines movement and position. A Home Assistant birth or broker reconnect replays the available state feeds, while button actions and other events remain transient. The compatibility snapshots supply the durable checkpoint for entity fields; individual subscriptions add no disk writes.

After successful APS delivery of an immediate On/Off command to a readable light, the coordinator requests OnOff (cluster 0x0006, attribute 0x0000) from the same device and endpoint. MQTT publishes the observed value from a matching response or a supported attribute report.

## Inputs and resource budgets

Derive device subscriptions from admitted names, endpoint aliases and supported group routes. For each commandable name use its literal path followed by `/set`, `/get`, `/set/+` and `/get/+` as applicable. Add the existing versioned and legacy management request filters. Deduplicate and batch within the negotiated or configured packet budget, account for the result of every filter in SUBACK, and handle packets that arrive before SUBACK. On join, rename or removal, update the route table and subscriptions together. Check current membership at dispatch so a superseded name cannot direct a queued command at a replacement device. Ignore command replays flagged retained. With v3.1.1, an established subscriber sees the retain bit clear for a live publication even if its publisher set retain. Command producers should publish with retain disabled. [MQTT 3.1.1 §§3.3.1.3, 3.8.4 and 4.7](https://docs.oasis-open.org/mqtt/mqtt/v3.1.1/os/mqtt-v3.1.1-os.html)

Make inbound packet bytes, per-command body bytes, outbound encoded packet bytes, in-flight receipts and total queued bytes/items independent policies. Count topic, identifier and encoding overhead when a budget refers to the whole packet. Preflight the intended subscriptions and largest generated publications against the migration snapshot. A matching command too large for the decoder can still end a v3 connection before the application sees its topic. Filtering addresses irrelevant inventories. An independent transport budget and an observable recovery policy address matching traffic. MQTT 5 advertises packet size and QoS 1/2 Receive Maximum separately and can report more failure detail. Observers and command producers can use MQTT 3.1.1 on the same broker. [MQTT 5 standard](https://docs.oasis-open.org/mqtt/mqtt/v5.0/os/mqtt-v5.0-os.pdf)

## What survives congestion and restart

| Work | Behaviour |
| --- | --- |
| Reported light or sensor state | Keep the merged HA-facing state by stable device and endpoint identity. Queue changes for immediate publication. Checkpoint the latest values together every 30 seconds and on shutdown. Bound broker work by bytes and count. A PUBACK advances the connection’s delivery window without writing to disk. Coalesce intermediate values and fairly revisit dirty devices. Keep known state separate from radio availability. |
| Button press or other ordered event | Emit each occurrence independently into the existing compatible action outputs. Bound the FIFO by bytes, count and age. A large diagnostic snapshot must never prevent emitting a small action. Make expiration and overload observable. After an outage discard expired gestures. QoS 1 permits duplicates, so consumers needing deduplication require a stable event ID in a versioned interface. |
| Raw attribute reads and failures | Return diagnostics to the requesting transaction. Preserve documented standard-attribute MQTT outputs for existing clients, and reserve durable merged HA state for actual exposed values. Provide bounded or paginated diagnostic reads. If a legitimate HA snapshot itself cannot fit, report the affected device and size, preserve its state and continue serving other devices. Never silently truncate an existing contract. |
| Physical commands | Reply with an application result when available. A transport PUBACK acknowledges one protocol hop. Existing transaction IDs support safe retry of tagged requests. For an interrupted untagged toggle or an operation with indeterminate outcome, automatic replay could actuate twice. Refuse busy work explicitly on interfaces that have a reply path, and expose refusals locally for legacy untagged commands. |
| Availability and discovery | Track the desired retained value or deletion by topic, with ownership and revision. Reconcile idempotently using a bounded acknowledgement window and pace discovery bursts. Preserve ownership through removal until cleanup is verified. Scope receipt tracking to the connection, packet ID and publication revision so an old receipt cannot clear new work. |

## Bench gate before the next trial

Exercise these cases with an isolated broker and the production Rust worker and supervisor.

1. Replay the original retained inventory and deliver a command to a nested device and an endpoint. Cover startup, rename, a rejected subscription and reconnection. Assert a correlated command result and zero unintended radio writes from replayed commands.
2. Use the audit's 482-attribute case, a burst exceeding the dirty queue and an independent button. Verify final state from a second subscriber, event order during a healthy session, bounded memory and continued radio service.
3. Drop the connection after publish and before PUBACK, then accept a newer state. Restart the publish worker and interrupt it again during cleanup. Verify eventual latest state and single execution of a tagged physical request.
4. Rehearse a 1,038-topic repair with delayed acknowledgements, a broker payload rejection and a supervisor interruption. Measure time until Z2M owns the radio separately from time until retained readback agrees. Verify that no recovery worker overwrites fresh output from Z2M.

Implement rollback ownership first, then subscription and packet budgets, followed by durable state and event delivery.

## Initial implementation budgets

The listener connects with MQTT 5 and a clean session. Its receive and transmit ceilings each default to 2 MiB. The command decoder accepts 16 KiB bodies and administration replies have a separate 256 KiB budget. The merged device snapshot budget is 1 MiB. Confirmed state is keyed by IEEE address and endpoint in redb.

Ready state and retained publications each have a 4 MiB payload queue. Ordered events have a 1 MiB queue and transient events expire after ten seconds. Per-topic coalescing keeps just the latest outstanding state. Membership and discovery determine how many distinct topics the process must track. The managed CLI provisions 1,024 queue slots and a 32-packet MQTT send window. These budgets exclude the latest state per admitted device, the discovery inventory, object overhead and the MQTT library buffers.

The shutdown drain allows five seconds without acknowledgement progress, subject to a thirty-second absolute deadline. Recovery gives the exclusive discovery writer fifteen seconds, including its readback. Persistent deletion obligations are scoped to the broker, base topic and discovery prefix. A changed active-owner topic requires fresh ownership evidence before any further write.

For a managed discovery or availability deletion, the bridge temporarily subscribes to the exact topic before it publishes the empty retained value. MQTT 5 requires the broker to remove the stored value and forward the deletion to existing subscribers. The bridge retires the matching local obligation after PUBACK and delivery of that empty publication on its own connection. A replacement queued during verification takes precedence. If verification is unavailable, the obligation survives for reconnect. This assumes the bridge has exclusive publication ownership of the topic during handover. [MQTT 5 §3.3.1.3](https://docs.oasis-open.org/mqtt/mqtt/v5.0/os/mqtt-v5.0-os.html)

Legacy standalone channel users retain their existing session lifecycle. The installed managed listener opts into registry-derived filters, broker reconnect and durable state.

The managed listener writes changed readings in one redb transaction per checkpoint, even while MQTT is disconnected. Quiet intervals and readings that return to their previous checkpoint value need no commit. After sudden power loss the bridge can replay the last successful checkpoint until a device reports again. A failed checkpoint keeps the staged changes for retry. Discovery deletions and the management transaction journal keep their existing immediate durability. Zigbee2MQTT periodically saves its state cache and saves it again on shutdown. Node-RED uses a 30-second default filesystem flush interval to limit storage wear. [Zigbee2MQTT state cache](https://github.com/Koenkk/zigbee2mqtt/blob/master/lib/state.ts) · [Node-RED filesystem store](https://nodered.org/docs/api/context/store/localfilesystem)

On a live `online` birth at Home Assistant’s default `homeassistant/status` topic, the managed bridge queues each active device’s latest merged state through its existing bounded drain. Discovery and availability are retained. Reported state and actions are published without retain. A birth only replays reported state. [Home Assistant MQTT birth messages](https://www.home-assistant.io/integrations/mqtt/#birth-and-last-will-messages)

A QoS 1 PUBACK with reason `0x10` completes delivery to the broker and increments a process-local diagnostic counter. The worker logs cumulative totals at powers of two to bound log volume. A refused managed deletion keeps its durable repair record. Other publish refusals fail the session before the delivery fence advances. A broker can return ordinary success even with zero subscribers. Delivery, replay and checkpoint decisions are independent of that diagnostic. [MQTT 5 §3.4.2.1](https://docs.oasis-open.org/mqtt/mqtt/v5.0/os/mqtt-v5.0-os.html)

The standard Temperature Measurement feed pairs `temperature` in degrees Celsius
with `temperature_observation` for a decoded MeasuredValue report or a successful
matched read. It contains `ieee`, `endpoint`, `kind` (`attributeReport` or
`readResponse`) and `observed_at_ms` (Unix milliseconds at coordinator receipt).
A new equal reading has its own observation time. Recovery and replay preserve
the original time. Consumers must treat values without an observation as
undated, including snapshots from older versions.
