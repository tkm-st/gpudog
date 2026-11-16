# `gpudog dash`

Shows a live view in the terminal of which processes hold GPUs and which are
waiting.

```shell-session
$ gpudog dash [-r SECONDS] [-s SOCKET] [--no-gpu-processes]
```

| Option | Description | Default |
| -- | -- | -- |
| `-r`, `--refresh` | Seconds between refreshes, from `1` to `3600`. | `5` |
| `-s`, `--socket` | Daemon socket to connect to. | `/run/gpudog/gpudogd.sock` |
| `--no-gpu-processes` | Hide daemon-observed process rows; observation and scheduling remain enabled. | off |

The view is redrawn until you press Ctrl-C. When standard output is not a
terminal, each snapshot is printed one after another without clearing the
screen.

## Example

```text
GPUDog: 3 GPUs, 1 free, 1 waiting (refreshes every 5s)
Observation: 412ms ago (interval 1000ms).

In use
  GPUS  UID   PID    HELD             COMMAND
  0     1001  52114  1h 12m / 1d 0h  python train.py

Waiting
  #  WANTS  UID   PID    WAITING  COMMAND
  0  2      1002  52380  3m 4s    python eval.py --ddp

GPU processes
  GPU  PID    UID   MEMORY   STATUS       COMMAND
  0    52131  1001  38.2GiB  managed      python
  2    60417  1003  11.0GiB  not managed  python

Devices
  INDEX  HELD BY             NAME                   UUID
  0      uid 1001            NVIDIA A100-SXM4-80GB  GPU-5f0e...
  1      -                   NVIDIA A100-SXM4-80GB  GPU-9a2c...
  2      - (+1 not managed)  NVIDIA A100-SXM4-80GB  GPU-3d7b...
```

The `GPUS`, `GPU` and `INDEX` columns show [daemon GPU numbers](../concepts.md#gpu-numbers),
not the CUDA numbers you pass to `-g`; compare GPUs by UUID.

The sections are:

*   **First line**: how many GPUs the daemon manages (including GPUs just
    removed from it, until a reload after their jobs end), how many are free for a
    new request, and how many requests are waiting.
*   **In use**: reservations that hold GPUs, with how long they have held them.
    With a [hold limit](run.md#hold-limit), `HELD` shows it after a slash,
    in the same units as the time held: `1h 12m / 1d 0h` has held its GPUs
    for 1h 12m of a 1-day limit. Without one, only the time held is shown.
*   **Waiting**: queued requests in queue order, with how many GPUs each wants.
*   **GPU processes**: every process the daemon sees using a GPU, including
    processes started without `gpudog`.
*   **Devices**: every GPU the daemon manages or has just removed, who holds it, and how many
    processes not started by `gpudog` use it.

## Why a request waits although a GPU looks free

In the example, GPU 2 has no reservation, but a process that is not part of
any reservation is using it, so the first line counts only GPU 1 as free. GPUDog does
not grant a GPU that another process is using. The request waiting for 2 GPUs
can start only when GPU 0 or GPU 2 becomes free as well. GPUDog never stops
other processes; ask their owner, or request fewer GPUs.

A GPU with no holder and no process can also be one the administrator just
removed from the daemon. It is left out of the first line's free count, and
`gpudog run -g` with its UUID is refused with
`GPU-... is not managed by this daemon`. It disappears from the list at the
first configuration reload after its job ended.

A request can also wait while GPUs are free: because you are at your own
per-user GPU limit, or because a request ahead of it is waiting for those
GPUs. Waiting requests are listed in the order the daemon considers them, which
follows how many GPUs each user already uses, so the `#` column can change
while you wait (see [The queue](../concepts.md#the-queue)).

The `STATUS` column says how a process relates to the reservations:

| Status | Meaning |
| -- | -- |
| `managed` | Started by `gpudog run` on a GPU that its reservation holds. |
| `managed (wrong GPU)` | Started by `gpudog run`, but using a GPU outside its reservation. |
| `not managed` | Not found to be part of any current `gpudog run` reservation, for example a process started without `gpudog`, or one left running after its reservation ended. Rarely, it is a process of a running job that the daemon lost track of (see [Observing processes outside GPUDog](../administration/unmanaged-processes.md#process-identities-and-namespaces)): check its uid and command before you act on it. |
| `unknown` | The daemon could not tell. |

Any GPU with a process on it is treated as in use, whatever its status.

If the daemon cannot check GPU usage, the dashboard shows
`GPU observation failed; new grants paused: ...` or
`GPU observation pending or stale; new grants paused.`, and the free count becomes
`unknown free`. While that line is shown, no new reservation is granted, but
running jobs continue. The pending line also appears for a moment during
each routine check on a healthy daemon: a request that arrives then is
granted when the check finishes. It matters only if it stays. Tell
your administrator if it does not go away; see
[Unmanaged processes](../administration/unmanaged-processes.md).

`--no-gpu-processes` hides the `GPU processes` rows and the `(+N not
managed)` counts under `Devices`. GPU usage is still checked, and the free
count still includes it.

## Notes

*   Every user can see every reservation, including the uid, pid and up
    to 256 bytes of the command line, and the daemon logs them. This is
    visible even where `ps aux` would not show it, for example between a
    container and the host, or between containers. Do not put secrets in a
    command's arguments.
*   The `COMMAND` column is what the client reported; the daemon does not
    verify the content. The client shortens a command line longer than 256
    bytes to at most 256 bytes including a final `...` (fewer characters for
    non-ASCII text).
*   Every string is reduced to printable ASCII before it is shown, so a
    crafted command line cannot inject terminal escape sequences.
*   Reservation `PID` values come from `SO_PEERCRED` in the daemon's PID
    namespace. They can be `0` when a host client is invisible to a
    containerized daemon. Do not assume they are host PIDs. In that case
    the process table cannot match the reservation's descendants reliably.
*   When an administrator removes a GPU from the daemon while a job holds
    it, it stays listed, held by that job. After the job ends it is listed
    with no holder until the next configuration reload. Its processes are
    no longer listed. It is not counted as free and cannot be requested.
    Daemon GPU numbers can change at that reload.
*   On a very busy machine the daemon has too many entries to send at once.
    The dashboard then shows `Note: this machine has more entries than fit
    in one status message, so the tables below are incomplete.` and the
    free count shows `unknown free`. This affects only what the dashboard
    shows: requests are still granted, and nothing needs to be done.
*   If the daemon cannot be reached, the dashboard shows the error and keeps
    retrying.
*   The dashboard opens a new connection for each refresh and closes it
    afterwards, so it does not keep one of your per-user connections open
    between refreshes. Each `gpudog run`, running or still waiting, also
    holds one. A refresh is refused while you already have the maximum
    number open (16 unless your administrator changed it): the dashboard
    shows `Cannot reach gpudogd: invalid request: too many concurrent
    connections for this user` (see [Troubleshooting](../troubleshooting.md))
    and keeps retrying. Stop a `gpudog` command you no longer need, or wait
    for one to end.
