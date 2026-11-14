# Client/daemon IPC and connection-scoped locks

<!-- markdownlint-disable-next-line MD036 -->
_Last verified: 2026-11-02 against the unmanaged-process working tree_

## Summary

The Rust client and daemon use a `SOCK_STREAM` Unix socket, defaulting to
`/run/gpudog/gpudogd.sock`. Kernel peer credentials identify the user, and
the connection's lifetime defines the reservation. tokio drives framed
protocol-v6 I/O.

## Details

### Topology and credentials

```text
host clients                    daemon container
/run/gpudog/gpudogd.sock <----> bind-mounted /run/gpudog/gpudogd.sock
```

Clients run on the same host, natively or in separate containers without
user namespace separation. Share the pathname socket and preserve the
invoking host UID/GID. See `notes/container-clients.md`. The
daemon container does not need host networking or host PID visibility for
reservation service. It needs NVML and the GPUs it manages.

The socket directory must be owned by root or the daemon's uid and not
group/world-writable. `bind` applies the umask, so the service explicitly
sets socket permissions afterward (default `0666`). Directory write access
would permit replacing the daemon socket with an impostor.

`SO_PEERCRED` user IDs are translated into the receiver's user namespace.
The daemon rejects overflow/unmapped UIDs. User namespace remapping is not
part of the current deployment contract.

Peer PIDs are namespace-dependent; do not assume they are universally host
PIDs or resolve them through an unrelated `/proc`. The service uses PID as
display metadata, not as a signal target or authorization input. The
daemon process observer uses host PID/procfs and start-time identities.
Client process tables consume daemon snapshots, never unrelated local PIDs.

### Lifecycle and resource bounds

```text
client                            daemon
hello(version) -----------------> hello_ack or denied
ping/status --------------------> pong/status_reply
acquire ------------------------> granted, denied, or queued then granted/denied
child inherits the open fd
last fd closes -----------------> release/cancel and pump queue
```

One connection holds at most one reservation. Every pre-acquire read/write
is bounded by the handshake timeout; held and queued connections wait as
long as needed. Connection, per-user held-GPU and queued-request caps bound
resource use. `SessionGuard::drop` releases state even on task cancellation.

Queued `oneshot` senders are registered under the same mutex as scheduler
submission. The receiver retains an early grant. `tokio::select!` races
grant delivery against peer closure; no mutex is held across `.await`.
The child inherits the reservation fd with `FD_CLOEXEC` cleared, so killing
only the wrapper does not release GPUs still used by its child.

### Wire format and display safety

```text
[u32 body_size][u16 message_type][payload...]
```

Integers are little-endian. Bodies are capped at 64 KiB before allocation;
string/list bounds and field relationships are validated by the codec.
The ten message types and exact fields are in
`crates/gpudog-lib/src/protocol/mod.rs` and `docs/developer/protocol.md`.
Existing layout or enum-value changes require a protocol version bump.
Unknown deny reasons can still carry a readable detail.

Requests use UUIDs or non-empty UUID candidates. The client resolves CUDA
ordinals before I/O; empty candidates and numeric wire requests are rejected.
Grants/status retain logical daemon indices as metadata alongside UUIDs.
See `notes/device-identity.md`.
Clients can read all reservations; command text is advisory, sanitized at
the daemon boundary and again by renderers. `dash` is terminal-only and has
no HTTP server. Its local NVML process query is not a daemon scheduling input.

## Why

Unix sockets combine kernel identity, EOF-based release, deferred grants
and no network exposure. TCP plus a shared credential would add identity
and credential-lifecycle problems. HTTP/gRPC would add framing dependencies
without replacing the need for peer credentials and connection lifetime.
Shared-memory/lock-file designs complicate deferred replies and cleanup.

The daemon orders the queue itself and never changes OS nice values.
GPUDog coordinates cooperating users; `CUDA_VISIBLE_DEVICES` is advisory,
not GPU access control.

## See also

*   `notes/development_guide.md`: retained decisions.
*   `notes/design-notes.md`: codec and service implementation choices.
