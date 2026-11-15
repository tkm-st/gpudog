# Architecture

## Repository layout

```text
crates/gpudog-core/    protocol, NVML loading and CUDA device visibility
crates/gpudog-client/  client sessions, gpudog run options and dashboard
crates/gpudog-daemon/  scheduler, service, config, logging and GPU observation
crates/gpudog/         client binary and CLI integration tests
crates/gpudogd/        daemon binary and daemon integration tests
packaging/             Debian metadata support and systemd unit
docs/                  this documentation
notes/                 design notes for maintainers
```

## Modules

The modules live in three library crates. `gpudog-core` (`protocol`,
`nvml`, `device_visibility`) depends on neither of the others.
`gpudog-client` (`client`, `cli_options`, `cuda_visible_devices`,
`dashboard`) and `gpudog-daemon` (the rest) depend on `gpudog-core` and
never on each other; only tests that run a real client against an
in-process daemon use both.

| Module | Responsibility |
| -- | -- |
| `protocol` | Message types and pure encode/decode over byte buffers. No I/O. |
| `protocol::codec` | Length-prefixed framing through tokio-util's codec. |
| `nvml` | Loads NVML at runtime, for both the client and the daemon. |
| `device_visibility` | The CUDA view of the devices: ordinals and UUIDs without a CUDA context, and checking a grant against it. |
| `peer_credentials` | Reads the client's uid and pid with `SO_PEERCRED`. |
| `gpu_inventory` | The daemon's list of GPUs through NVML. Includes a static double for tests and `--num-gpus`. |
| `scheduler` | The reservation lifecycle: grant, queue, timeout, release and per-user limits. Pure logic; the caller passes in the current time. |
| `service` | The listener and one session state machine per connection. Connects the socket layer to the scheduler. |
| `client` | The client side of a session: connect, handshake, ping, acquire and status. |
| `cli_options` | Parses and checks the `gpudog run` options. |
| `cuda_visible_devices` | Writes and reads UUID lists in `CUDA_VISIBLE_DEVICES`. |
| `config` | Loads `config.toml`, merges it with the defaults and checks it. |
| `daemon_log` | Bounded queue and writer thread for stderr/journal logging. |
| `gpu_processes` | Managed-UUID NVML process queries and bounded host procfs facts. |
| `process_watcher` | PID/start-time root/descendant tracking; labels are separate from observed usage. |
| `dashboard` | Renders the daemon's status snapshot, including its process list, as text. |

## The connection is the lock

The most important design rule: **a reservation lives exactly as long as the
connection that asked for it.** The client:

1.  connects and completes the handshake,
2.  sends `acquire` and waits for `granted`,
3.  forks the user's command, leaving the socket open and inherited (not
    close-on-exec),
4.  waits for the command and then closes the socket.

When every copy of the descriptor is closed, the daemon sees end-of-file and
releases the GPUs. A crash or `SIGKILL` therefore releases the GPUs with no
extra cleanup. Code that closes the descriptor early, or marks it
close-on-exec, breaks the lock.

## Scheduling policy

*   The daemon orders the queue; clients send no priority. Each walk of the
    queue visits waiting requests by their user's load, lowest first, then
    by arrival. Load is the GPUs the user holds, plus free managed GPUs
    observed busy with that user's processes, plus what its earlier
    waiting requests want.
*   A request that cannot be granted reserves the slots it could use for
    the rest of the walk, so nobody behind it takes them; requests that need
    other slots are still granted. If it cannot be met without slots its own
    user is using (held, or busy with at least one of that user's processes)
    or slots busy outside GPUDog, it reserves only those: one user waiting on
    itself, or on an outside process that has no release to wait for, never
    holds up the free slots.
*   Once a user's request waits, or is held back by its own
    `max_held_per_uid`, that user's later requests are passed over in the
    same walk. More requests therefore reserve nothing more.
*   `submit` queues a request and runs the same walk as `tick`, so the two
    never disagree.
*   Requests that can never succeed are denied at once, not queued.
*   Timeouts are enforced by the daemon. A request that times out is denied
    with `timed_out`.
*   Per-user limits on connections, held GPUs and queued requests.

## Device identity

Grants carry logical indices together with the matching `GPU-<uuid>`
strings, and the client exports the UUIDs as `CUDA_VISIBLE_DEVICES`. A UUID
names the same GPU everywhere, so the daemon may see only some of the host's
GPUs, either because its container was given a subset or because `devices`
in `config.toml` restricts it. Logical indices are renumbered `0..k-1` over
the managed devices in PCI bus order. Internally the scheduler uses fixed
slots, one per GPU found at startup; a reload that changes `devices` only
marks slots managed or not, and recomputes the logical indices. See
[the protocol](protocol.md#device-identity).

## Why some things are not done

*   **No client-chosen priority.** Any user could pick the most favoured
    value, so a priority only let one user go ahead of everyone. Host PID
    visibility is for observation only, never process signaling or OS
    priority changes.
*   **No TCP and no tokens.** A Unix socket with `SO_PEERCRED` gives
    kernel-verified identity, release on death, server-pushed grants and no
    network exposure at once.
*   **A custom binary protocol, not JSON or gRPC.** The message set is small
    and fixed, and a bounds-checked fixed-layout decoder is a much smaller
    attack surface for a root daemon than a general-purpose parser.

## Async ownership

The tokio service shares state behind a `Mutex`, never held across `.await`.
Queued sessions register a `oneshot` under the same lock as submission,
then race the grant against peer closure with `tokio::select!`.
`SessionGuard::drop` releases state when a session ends or its task is
cancelled during shutdown. NVML handles and reservation descriptors use
RAII/Drop.

The detailed reasoning is in `notes/ipc-design.md` in the repository, and
the implementation choices behind the service, logging, configuration and
packaging are in `notes/design-notes.md`.

## Older versions

What remains from earlier versions:

*   Protocol versions 2 to 6 are listed under
    [Compatibility rules](protocol.md#compatibility-rules). The v3 to v6
    golden fixtures in `crates/gpudog-core/tests/golden/` document earlier
    wire layouts and are not tested. Their generators
    (`scripts/gen-golden-v3.py` to `gen-golden-v6.py`) stay because
    `gen-golden-v7.py` builds on them. Only version 7 is spoken and tested.
*   Earlier daemons wrote log files. The `log_directory` key and
    `--log-directory` option are accepted with a warning and ignored (see
    [Logging](../administration/logging.md)).
*   `crates/gpudog-daemon/tests/config_compat.rs` checks that existing config
    files still load as they did: the loader must give the results in
    `config_recorded_results.txt`, a frozen record that is never
    regenerated, apart from the deliberate differences it lists in
    `DIVERGENCES`. See
    [Protocol regression fixtures](building.md#protocol-regression-fixtures).
