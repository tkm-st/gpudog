# Same-host container clients

<!-- markdownlint-disable-next-line MD036 -->
_Last verified: 2026-11-02 against the unmanaged-process working tree_

## Summary

Container clients use protocol-v7 UUID requests and the same host pathname
Unix socket. No TCP transport, credential claim field or host network is
required. The daemon shares host PIDs for observation; clients do not.
Initial deployment assumes a
rootful Docker engine and no user namespace separation.

## Details

### Identity and device views

Run both containers with `--userns=host` and each client with its invoking
host UID/GID. `SO_PEERCRED` then preserves per-user accounting across host
and container processes. A Docker administrator can choose another UID;
kernel credentials are not a defense against that host-root capability.

Client CUDA ordinals come from that container's device view, not the daemon.
Requests use UUIDs/count candidates, and the daemon intersects them with its
own managed view. Jobs execute in the client container and inherit its
reservation socket. Container death closes its processes' descriptors;
existing `SessionGuard` cleanup releases or cancels the request.

Socket directory binds can be read-only in clients, but that limits
filesystem mutation only, not bidirectional socket protocol operations.
Production directory ownership/modes still apply.

### Images and procfs

The Dockerfile's last `runtime` stage remains daemon-only. `--target client`
contains the client and defaults to a non-root help-only identity; real
requests need the host UID/GID override. The optional `test-drivers` stage
contains isolated mock libraries and is never copied into production images.

Peer PIDs belong to the receiver's namespace and may be 0 between sibling
containers. NVML PIDs may not name processes in a client container's `/proc`.
The daemon now observes host procfs/NVML and sends process rows to clients.
`dash --no-gpu-processes` hides rows only; it does not disable observation.
Ownership labels use PID/start-time tracking and remain best-effort, while
any observed GPU use blocks new allocation independently of its label.

### Regression harness

`scripts/container-check.py` creates isolated client PID namespaces and
private networks, a host-PID daemon, root-created config/socket directories,
and caller UIDs.
The default profile mounts mock CUDA/NVML libraries compiled against the
image's own glibc, so no GPU or host ABI-compatible mock build is needed.
`--gpu` instead uses two actual GPUs; `--skip-build` reuses the test images.

The CPU and real-GPU profiles passed UUID/order/empty visibility, health,
exit status, same-UID held-GPU caps across containers, different-UID access,
queued-container cancellation, holder death, wakeup and reservation release.
The GPU profile also passed Docker's client-device subset selection. These
checks do not exercise a framework. The CPU profile additionally verifies
unmanaged usage, host child identity, observation failures, paused grants,
surviving jobs and recovery.
The harness removes only its own container IDs and keeps logs under `tmp/`.

## Why

UUID device identity and connection-scoped locks already cross these
container boundaries. Deployment/test coverage and a procfs-free dashboard
mode make that topology supportable without a larger transport or PID-control
surface. Keeping unmanaged-process observation separate avoids pretending
the daemon already has reliable host-process identities.

## See also

*   `docs/user-guide/container-clients.md` and `packaging/compose.yaml`.
*   `notes/device-identity.md`: CUDA view and UUID invariants.
*   `notes/ipc-design.md`: transport and credential boundaries.
