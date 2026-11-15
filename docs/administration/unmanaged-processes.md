# Observing processes outside GPUDog

The production daemon observes processes using its managed GPUs, but does
not stop, signal, requeue or change the OS priority of external processes.
Waiting for observed usage to end is **always enabled**; there is no switch
to ignore external usage.

## Scheduling and failures

`watch_unmanaged_ms` controls the observation interval (default `1000`,
integer `1..=3600000`). NVML compute and graphics process lists are queried
only for managed UUIDs. GPUs with observed processes cannot receive a new
reservation, regardless of their ownership labels. Existing reservations
are not revoked; labels are not an allocation authorization decision.

A GPU that GPUDog has not reserved but that is busy counts as the GPU of
every user with a process on it: it counts towards how many GPUs each such
user already uses, which orders [the queue](../concepts.md#the-queue).
Processes of root count as uid 0's.

Whoever's it is, a request that cannot be met without GPUs busy outside
GPUDog, or without GPUs its own user is using, keeps only those GPUs from
the requests behind it: a process outside GPUDog may never end, and a
user's own jobs should not keep GPUs idle that nobody is using (see
[The queue](../concepts.md#the-queue)).

If no observed process has a readable owner, as happens with a Docker
daemon started without `--pid=host`, busy GPUs count as nobody's, and the daemon
logs `no observed GPU process has a readable owner; busy GPUs cannot be
counted as anyone's (run a containerized gpudogd with --pid=host)` once.

The daemon starts with new grants paused until its first successful sample.
Observation runs in the background, so the daemon keeps answering clients
(`health`, `dash`, releasing GPUs when jobs end) while it runs. New grants,
however, are paused during queries and when the latest sample is stale.
Failed observations are logged to stderr, keep grants paused, and are
retried. A successful sample resumes allocation and grants the waiting
requests that now fit. Queue deadlines continue to expire while paused, and
running jobs are not terminated.

Only the latest successful sample decides which GPUs are busy. A GPU can become
busy after that sample, so observation cannot guarantee exclusion from
processes that bypass GPUDog. Device access controls would be a separate
feature. Longer intervals delay usage/release detection.

## GPUs that drive a display

A display server (Xorg, Xwayland or a Wayland compositor) keeps a process
on the GPU it draws with for as long as it runs, so the daemon never grants
that GPU. `gpudog dash` usually shows it as a `not managed` row on that
GPU. A display server started by root counts as uid 0's; one started by a
logged-in user counts as that user's, so that user always counts as using
one GPU more and is served later in the queue.

Leave such a GPU out of `devices` (see
[Managing some GPUs](configuration.md#managing-some-gpus)) and
[reload](configuration.md#changing-the-configuration), or
move the display to another GPU. Researchers can still name the left-out GPU
by its CUDA number and then get `GPU-... is not managed by this daemon`;
tell them which GPUs to use, or set `CUDA_VISIBLE_DEVICES` for them.

## GPU compute modes

GPUDog expects every managed GPU in the `Default` compute mode, in which
several processes may use a GPU at once. The daemon neither reads nor
changes the compute mode. Check it with:

```shell-session
$ nvidia-smi --query-gpu=index,uuid,compute_mode --format=csv
index, uuid, compute_mode
0, GPU-8f0c3c4e-..., Default
```

*   `Exclusive_Process` lets only one process at a time use a GPU through
    CUDA, while a GPUDog reservation covers the command and all its
    descendants. Use it only if every job uses each of its GPUs from a
    single process. Otherwise use `Default`.
    *   A job that uses one GPU from several processes, such as data loader
        workers that each use CUDA, fails in the second process with
        `CUDA-capable device(s) is/are busy or unavailable`.
    *   The same error reaches a job if a program started without `gpudog`
        takes the GPU between the grant and the job's first CUDA call.
        Conversely, once a job is using a GPU, such a program cannot use it
        either.
    *   `gpudog` and `gpudogd` themselves never take a GPU in this mode:
        they find GPUs without opening them through CUDA.
*   `Prohibited` lets no process use the GPU through CUDA. The daemon still
    grants such a GPU, and every job on it fails. Leave it out of `devices`
    (see [Managing some GPUs](configuration.md#managing-some-gpus)).

To go back to the default, run `sudo nvidia-smi -i <GPU> -c DEFAULT`, where
`<GPU>` is the index or UUID that `nvidia-smi` shows. The mode does not
survive a reboot or a driver reload; if a boot script or service sets it,
change it there too.

## WSL2

GPUDog cannot share GPUs safely under WSL2, because the daemon cannot see
which processes use the GPU there. NVML under WSL2 does not report the CUDA
processes of Linux programs.

*   With WSLg, NVML reports the WSLg display server on the GPU while WSLg
    runs, so the GPU is never granted. Its process is not visible to the
    daemon, so `gpudog dash` shows it as `unknown`, and the daemon logs
    `no observed GPU process has a readable owner`; `--pid=host` does not
    help here.
*   Without WSLg, GPUs are granted, but programs started outside GPUDog are
    not detected, so they can share a GPU with a job.

Use a native Linux host.

## Process identities and namespaces

The native daemon observes the host `/proc`. A daemon container must run
with **`--pid=host`**, without user namespace separation. Clients remain
in their own PID namespaces and send no claimed user/process identities.
Kernel peer credentials and `/proc/<pid>/stat` start-time ticks establish
the reservation root; PID alone is not treated as a stable identity.

The observer does not read every process on the host. It reads the
processes NVML reports on the managed GPUs and their parent chains, each
reservation's `gpudog run` process and that process's descendants, found
through the kernel's per-thread child lists
(`/proc/<pid>/task/<tid>/children`), and processes it already knows belong
to a job. It retains previously observed
child/descendant identities while the reservation remains alive, even
after reparenting. Reused PIDs are not inherited as owners. Labels are
best-effort: a process whose parent chain could not be read shows as
`unknown`, and a process of a job that was reparented before the daemon
saw it (an unobserved orphan) shows as `not managed`.

This keeps the cost bounded however many processes the host has:

*   With R reservations held, each job's processes and threads are followed
    up to about 4,096 / R per observation, the processes under its
    `gpudog run` process first. Processes the daemon already knows of are
    read again every time; only their threads and children count against
    that. A job with more logs
    `a job has more than N processes to follow; some of its processes may show as not managed`,
    where N is that share at the time, at most once an hour for the whole
    daemon. The line does not name the job; `gpudog dash` usually shows which
    jobs have `not managed` processes on their GPUs. Those of that job that the
    daemon had not seen before they were reparented show as `not managed`.
*   The parent chains of GPU processes are read up to 16,384 processes in
    total per observation; a GPU process past that shows as `unknown`.
*   These limits are fixed; there is no setting, and no action is needed.
    Only labels are affected: usage still blocks allocation, and nothing is
    paused.
*   A kernel without child lists (`CONFIG_PROC_CHILDREN`, enabled on
    Debian, Ubuntu and RHEL kernels; the kernel has them if
    `ls /proc/self/task/*/children` lists a file) makes the daemon log
    `this kernel does not list child processes; processes a job leaves behind may show as not managed`
    at startup. Processes are then matched to a reservation only through
    their parents, so a process reparented to init after its parent exits is
    not matched, unless the daemon saw it, or a GPU process under it, before
    that.
*   The kernel's child lists can miss a process that is being started at
    that moment; the next observation finds it.
*   Only a procfs that cannot be read at all is an observation failure,
    logged as `GPU observation failed; new grants paused: cannot read procfs: ...`;
    it pauses new grants as any observation failure does. Check that `/proc`
    is mounted and readable by the daemon, and that a Docker daemon runs
    with `--pid=host`.

Host PID sharing increases daemon visibility. No `--privileged`, host
networking, process signaling or nice-value changes are added. Keep Docker
administration privileged; it is not an unprivileged GPU-only capability.

## Dashboard

The daemon supplies process rows, busy daemon GPU numbers, observation age/error,
interval and paused state from the daemon. The client does not read its local
NVML or `/proc`; full rows work even in a private client PID namespace.
`--no-gpu-processes` hides rows only and cannot disable daemon observation.

`free` excludes the union of reserved and observed-busy GPUs, without double
counting. Before the first success, or on stale/failed/truncated observations,
the view shows `unknown free`. A sample is fresh for one observation
interval, and the next query starts only after that interval, so `dash`
also shows `unknown free` with a grant-paused note while a query is running,
on a healthy daemon too. It clears when the query finishes. The
`GPU observation ...; new grants paused` line and the last sample age
explain availability. When there are too many entries for one status message,
`dash` shows incomplete tables and `unknown free`. Which GPUs are busy is
always sent in full.

## Testing override

`gpudogd --num-gpus` fabricates static devices and bypasses CUDA/NVML/process
observation for wire/service tests. It is not a production option or a way to
turn off external-process waiting on real GPUs. Unit tests inject explicit
process/procfs providers for usage, failure, recovery and PID-reuse cases.
