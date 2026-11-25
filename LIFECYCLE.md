# Shutdown and resource reclamation

[MQTT delivery and recovery](MQTT-DESIGN.md) defines the broker, radio and rollback ownership contract.

The production contract is that a successful shutdown receipt permits a successor to open the same store, coordinator and MQTT identity. A request to stop, a cancelled future and a timeout are each weaker events. Only the terminal receipt confirms that all owned work has finished. A failed receipt records why shutdown failed. Reopening after failure or process death also requires startup reconciliation.

## Order of operations

1. Close admission under the same gate used to dispatch work and accept completions. Invalidate the session for new commands, retries, publications and timer rearming. This step is synchronous and idempotent. A previously committed send may still arrive at the device.
2. Wake workers using a sticky stop signal. Drop queued operations and resolve their pending callers as cancelled. A delayed reply cannot commit a device update unless its session, device membership and transaction are all still current.
3. Drain the owners. Stop serial writes, finish or cancel outstanding serial I/O, release the port, and join the radio task. Stop accepting MQTT commands and publishing device updates, publish retained bridge offline while the broker connection is available, drain the protocol exchange, then disconnect and join the broker task. If delivery is uncertain, record that outcome and rely on the broker will and next-session reconciliation. A timeout alone never confirms that a write, task or port has finished.
4. With all worker tasks joined, finish persistence and release the store lock last. The device store currently commits synchronously, so there is no delayed writer to flush. Return the receipt only after dropping the last owned resource.

Stopping must also work during partial startup. Take the exclusive store lock first. For each additional resource, have an owned handle and a registered release path before exposing it to other tasks. A startup failure follows the same drain path for the subset acquired. Production workers start only after all startup checks succeed; the radio must identify the existing network before it may transmit normal application traffic.

## Ownership by resource

| Resource | Revocation | Evidence of release |
|---|---|---|
| Store, snapshot and process lock | One owner commits under the store lock. Each commit writes and syncs a temporary file, atomically replaces the snapshot, then syncs the directory. | The supervisor joins all workers, explicitly unlocks the file in its owning process, then closes it. The successor can acquire the lock and read the last committed snapshot. |
| Device address and delayed work | IEEE identity is durable. A membership handle identifies one incarnation in one process. Removal, replacement and reassignment revoke the old route. A new process reconstructs fresh handles. | Resolving an old handle fails, even after the same IEEE and short address recur. Only the current owner can erase an address mapping. |
| Radio request and sequence number | The future radio owner registers its expected peer, endpoints, cluster, response type and sequence before sending. Cancellation removes its waiter. A wire transaction that cannot be disambiguated after reuse requires a quiet interval or transport reset before reuse. | The owner has finished I/O and joined. An application generation counter alone cannot distinguish two identical frames on the wire. |
| MQTT client and retained state | The MQTT worker owns broker publication and session teardown. The managed listener reconnects independently of the serial worker. Device removal requires persistent cleanup of state, availability and discovery topics if the broker is unavailable. | Successful broker acknowledgements or a persisted cleanup obligation. An empty local queue is insufficient. |
| Timers and background tasks | Each task belongs to a session. A timer queues an intent, and the owner checks session and membership again before dispatch. A worker exits on stop or asks the supervisor to terminate the session on failure. | Every registered task has returned. Detached resource owners and untracked blocking I/O violate the contract. |

## Failure semantics

Dropping the public controller requests shutdown. Dropping a caller's wait for shutdown leaves the supervisor draining and retaining its lock. An uncooperative worker delays release; the caller can time out its wait and inspect the service without authorizing a replacement. On abrupt process death the OS releases local handles, the next process loads the atomic snapshot, and MQTT and radio state must be reconciled with their actual peers. A write already transmitted before a crash has an unknown outcome until read back. The bridge must not replay a non-idempotent command on that assumption.

A late leave for an old short address is distinguishable. A delayed leave with the exact same IEEE and short address after a rejoin can be indistinguishable on the wire. The adapter must use available transaction context or reconcile membership before treating such an ambiguous event as an authoritative local removal.

## Handover across independent clocks

The broker, coordinator and lamps can all keep working while the host is stopped. Do not derive peer completion from elapsed host time. Keep the following rules for each adapter:

- A missed shutdown deadline leaves the session draining. Record the owner and its outstanding operation. If the process must be terminated, classify the next start as crash recovery. A fresh process must open the same canonical state directory, take exclusive ownership of the serial device, identify the coordinator and verify its network before enabling commands. A state-directory lock by itself cannot exclude a second configuration pointing at the same serial device.
- Stop admitting application traffic before teardown traffic. An MQTT offline publication uses the existing connection under the broker owner's shutdown authority. For QoS 1 wait for PUBACK before clean disconnect. If that acknowledgement cannot be established, close the transport without suppressing the configured Last Will and reconcile retained state on reconnect. Never interpret TCP close as the broker confirming what subscribers saw.
- Record which commands crossed the transport boundary. Complete unsent work as cancelled. Mark sent but unanswered work as outcome unknown. An old reply or timer cannot publish a new observation merely because the same device and address exist again. For ambiguous on-air responses, require protocol evidence or a verified bound on response lifetime before reusing the transaction identifier. Resetting a local counter cannot flush replies still in the mesh.
- Resources requiring asynchronous cleanup must be acquired inside supervised work with a release path. Before activation, capture only handles that can be released synchronously on drop. Release implementations must avoid waiting on peers whose owners have already stopped. Any additional resource acquisition must identify its owner, revocation point, handover proof and crash recovery before implementation.

## First implementation boundary

The library exercises the store, membership, session-worker and Unix serial transport contracts. The radio transaction and MQTT rows specify acceptance criteria for their future implementations. No firmware, device-specific converter or production connection is required to run these tests.

## Files and Unix pipes

The supervisor revokes admission, wakes workers, then joins them. Dropping a handle requests shutdown. Callers awaiting the terminal result can distinguish clean release from a worker error. This follows the ownership and join ordering in [The Rust Book](https://doc.rust-lang.org/book/ch21-03-graceful-shutdown-and-cleanup.html).

The store assumes a private directory on a local filesystem supporting file locks, atomic rename and directory sync. Keep that directory and its lock file in place while a controller owns them. On Unix, the store opens the snapshot and sidecar lock with nonblocking, no-follow flags and checks the type of the opened file. A FIFO at either name therefore produces an error even when there is no peer. On a clean handover the acquiring process explicitly unlocks before close, so a child between fork and exec cannot delay its successor. If the owner exits abruptly the kernel releases the lock once the last inherited descriptor closes. The lock file stays in place across handovers. Removing it could let two processes lock different inodes at the same pathname. [Rust file locking](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)

A snapshot commit explicitly flushes the buffered writer, syncs the temporary file, renames it, then syncs the directory. If directory sync fails after rename, the new snapshot is visible but its crash durability is uncertain. The owner refuses further edits until recovery. The next owner validates the committed snapshot, removes the reserved staging file and syncs the directory before serving. A process-exit test exercises OS handle release. Physical power-loss durability depends on the filesystem and storage device honouring sync. [Rust buffered writes](https://doc.rust-lang.org/std/io/struct.BufWriter.html) · [Linux fsync](https://man7.org/linux/man-pages/man2/fsync.2.html)

A pipe carries bytes with no message boundaries. Own every descriptor, including inherited or duplicated ends. A reader sees EOF after queued bytes are drained and every writer is closed. A full pipe can stall a writer indefinitely. A supervised Unix pipe worker uses readiness-based I/O and observes the stop signal while waiting. Ordinary file I/O can use a blocking worker, provided that worker remains owned and joined until the call returns. [Linux pipe semantics](https://man7.org/linux/man-pages/man7/pipe.7.html) · [Tokio file I/O](https://docs.rs/tokio/latest/tokio/fs/index.html)

Track the offset of a partial stream write. Cancelling an async `write_all` midway and retrying the original buffer can duplicate its prefix. On teardown, a partially emitted command has an uncertain delivery outcome. The peer may still consume bytes after local shutdown. [Tokio write cancellation](https://docs.rs/tokio/latest/tokio/io/trait.AsyncWriteExt.html#method.write_all)

## Serial port handover

A startup owns the exclusive tty as soon as it opens. The serial worker holds both halves of the same descriptor. Its public handle holds only channels. Each outgoing frame has a single owner, a current byte offset and a completion receipt. Every nonblocking write checks the session gate, including writes for the remainder of a frame. Reads can progress while a write waits for room.

On stop, release both halves and join the worker. Resolve every queued receipt as unsent and report the accepted byte count for an interrupted frame. A fully accepted frame still needs its own protocol acknowledgement. Never carry pending serial bytes into the next session. The future coordinator protocol must resynchronise the peer before issuing commands if the preceding session ended mid-frame.

The Linux handover test sends a large synthetic frame to fill the tty. After reading half, the peer stops consuming output. It sends a short reply and waits for the worker to deliver it before shutdown. The test drains the master to hangup, compares every byte with the accepted prefix, then reopens the same slave and checks the next session's marker through its own hangup. This distinguishes bytes already buffered by the kernel from replay by the next owner.
