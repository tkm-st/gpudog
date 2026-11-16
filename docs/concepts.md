# Concepts

This page explains how GPUDog reserves GPUs and defines the terms the other
pages use.

## Reservations

A reservation gives one process exclusive use of one or more whole GPUs. The
client asks the daemon for GPUs over a Unix domain socket. When the daemon
grants them, the client starts your process.

The open socket connection is the reservation. The client keeps it open for as
long as your process runs, and your process inherits it too. When the last
holder of the connection exits, even after a crash or `SIGKILL`, the kernel
closes it and the daemon frees the GPUs. Nothing has to clean up stale
reservations.

This also means that a reservation lasts as long as your process, or anything
it started that is still running, such as a server started in the background.
If `gpudog` itself is killed while your process keeps running, the GPUs stay
reserved until your process exits.

A process that closes every inherited descriptor, as some programs do when
they detach into the background, gives up its share of the reservation. Once
no holder is left, the GPUs are released even though that process may still
use them. The daemon still waits before granting a GPU it observes in use,
but `gpudog dash` cannot attribute the process to your reservation.

## GPU numbers

Two numberings appear in GPUDog, and they often differ:

| Term | What it is | Where you see it |
| -- | -- | -- |
| **CUDA number** | The device number CUDA uses in the environment where you run `gpudog`. It follows `CUDA_VISIBLE_DEVICES`, `CUDA_DEVICE_ORDER` and the GPUs your container can access. | What you pass to `-g`; the device numbers your program uses |
| **daemon GPU number** | The daemon's own number for each GPU it manages: `0`, `1`, ... in the order of their PCI bus addresses. | `gpudog dash` (`GPUS`, `GPU` and `INDEX` columns), the `holding gpu(s)` line, daemon logs |

For example, with `CUDA_VISIBLE_DEVICES=2,0`, `-g 0` requests the GPU that is
CUDA number 0 in your environment, which may be daemon GPU number 2. Neither
numbering has to match `nvidia-smi`. Daemon GPU numbers usually follow the
host's `nvidia-smi` order when the daemon manages every GPU on the host.
They can change when an administrator changes which GPUs the daemon manages.
To name one GPU in every environment, and over time, use its UUID (`GPU-...`,
as printed by `nvidia-smi -L`).

## How your process sees the GPUs

The client starts your process with two environment variables set:

| Variable | Value |
| -- | -- |
| `CUDA_VISIBLE_DEVICES` | The UUIDs of the granted GPUs, for example `GPU-8f6a...,GPU-1c2d...` |
| `CUDA_DEVICE_ORDER` | `PCI_BUS_ID` |

UUIDs are used because a UUID names the same GPU in every container and on
the host. Inside your process the granted GPUs appear as CUDA numbers `0`,
`1`, and so on, in the order of that UUID list. With `-g`, that is the order
you asked for. These numbers need not match daemon GPU numbers.

Before requesting a reservation, `gpudog run` asks CUDA which GPUs your
environment can see. It honours `CUDA_VISIBLE_DEVICES` (numbers or UUIDs),
`CUDA_DEVICE_ORDER` and container device access. If no GPU is visible, for
example because `CUDA_VISIBLE_DEVICES` is empty or `-1`, the request fails;
GPUDog never falls back to all GPUs.

*   With `-g`, each CUDA number is turned into the UUID of that GPU. A UUID you
    give must be visible to you, and the daemon must manage it.
*   With `-n` or no option, the daemon picks from the GPUs visible to you that
    it manages; visible GPUs it does not manage are skipped. If too few remain,
    the request fails at once instead of waiting.

`CUDA_DEVICE_ORDER` and `CUDA_VISIBLE_DEVICES` are always replaced for your
process, so you cannot widen a reservation by setting them.

## The queue

When the GPUs you asked for are not free, your request waits in a queue.

*   **Users who use fewer GPUs are served first.** The daemon puts each
    waiting request in order by how many GPUs its user already uses: the
    GPUs the user holds through GPUDog, GPUs the user's own programs use
    outside GPUDog, and what the user's earlier waiting requests ask for.
    Requests from users who use the same number are served in arrival
    order. You cannot choose a priority.
*   **The order can change while you wait.** When another user's job ends,
    that user moves forward, so the queue position `gpudog` and
    `gpudog dash` show can go up or down.
*   **A waiting request keeps the GPUs it needs from the requests behind it,
    and only those.** A small request does not take a GPU that a larger one
    ahead of it is waiting for, so large requests do not wait forever.
    Requests that need other GPUs are still served. For example, while one
    request waits for GPU 0, someone behind it can still get GPU 1.
*   **Waiting for your own GPUs, or for GPUs in use outside GPUDog, holds
    up only those GPUs.** If a request cannot be met without GPUs you are
    using or GPUs in use outside GPUDog, it keeps only those GPUs, of either
    kind, and the free ones go to the requests behind it. A program
    started without `gpudog run` may never end, and your own jobs should not
    keep GPUs idle that nobody is using.
    *   If you run `gpudog run -g 0` again while a job of yours holds GPU 0,
        only you wait: the second job starts when the first ends. A
        `gpudog run` started inside another `gpudog run` that asks for the
        same GPUs waits until its own parent ends, that is, forever; give
        it `-t`, or do not nest them.
    *   Smaller requests may take the free GPUs meanwhile, so a large
        request can wait for their jobs as well once the GPUs it waits for
        are free.
*   **Your own requests are served in order.** While one of your requests
    waits, your later requests wait behind it.
*   **A request held back by your own per-user limit does not hold up other
    users.** If granting it would put you over the number of GPUs one user
    may hold (see [Per-user limits](#per-user-limits)), it waits until your
    other jobs release GPUs, while other users' requests are served.
*   **Requests that can never succeed are rejected at once.** For example,
    asking for more GPUs than are visible to you or than the machine has, or
    for a CUDA number that does not exist.

A large request from a user who uses nothing still keeps every free GPU it
needs from the requests behind it while it waits for another user's job to
end. That is what keeps large requests from waiting forever, and
`max_held_per_uid` limits how large it can be. If that job runs for days,
those GPUs stay idle for days, unless the administrator has set a
[hold limit](#hold-limit). Someone who already uses many GPUs can wait
a long time while users who use fewer keep arriving.

## Per-user limits

To stop one account from taking the whole machine, the daemon limits each
user by default to:

*   16 open connections
*   8 GPUs held at once
*   8 requests waiting at once

A new request over a limit is rejected at once with a message that names
the limit, including one that would take you over the held limit together
with the GPUs you already hold. The
administrator can change these values in
[the configuration](administration/configuration.md).

## Hold limit

The administrator can also set a hold limit: how long one `gpudog run` may
keep its GPUs, the same for every user. There is none by default. With one, `gpudog run` stops your command when it ends; see
[Hold limit](user-guide/run.md#hold-limit).

## Identity

The daemon identifies users by the uid that the kernel reports for the socket
connection (`SO_PEERCRED`). A client cannot claim to be another user.

## What GPUDog does not do

GPUDog helps users who agree to cooperate. It prevents accidental sharing of
a GPU, but it cannot stop a user who deliberately bypasses it:

*   `CUDA_VISIBLE_DEVICES` is only a hint to CUDA. A process can unset it and
    use any GPU. Real enforcement would need cgroup device controls.
*   Processes started without `gpudog run` are observed but not controlled.
    A GPU in use by such a process is not granted, so your request waits even
    if no reservation holds it. If the daemon cannot check GPU usage, it stops
    granting new reservations until it can; running jobs are not affected.
    The daemon never stops any process; only `gpudog run` stops its own
    command, at a [hold limit](user-guide/run.md#hold-limit). See
    [Unmanaged processes](administration/unmanaged-processes.md).
*   GPUs are allocated whole. GPU sharing is not supported, and MIG instances
    or GPUs with MIG enabled are rejected.
*   The client must run on the same host as the daemon. Clients in separate
    containers are supported with some limits; see
    [Container clients](user-guide/container-clients.md).
