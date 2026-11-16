# `gpudog run`

Runs a command while holding an exclusive GPU reservation.

```shell-session
$ gpudog run [OPTIONS] [--] COMMAND [ARGS...]
```

## Options

| Option | Description | Default |
| -- | -- | -- |
| `-n`, `--num_gpus`, `--num-gpus` | Number of GPUs to reserve. Any free GPUs visible to you are chosen. Cannot be used with `-g`. | `1` |
| `-g`, `--gpus` | Specific GPUs to reserve, separated by commas: CUDA numbers such as `0,2`, or UUIDs such as `GPU-8f6a...`. Cannot be used with `-n`. | none |
| `-t`, `--timeout` | How long to wait for the GPUs. | wait forever |
| `-s`, `--socket` | Daemon socket to connect to. | `/run/gpudog/gpudogd.sock` |

`gpudog -V` (or `--version`) prints the client and protocol versions.

!!! warning "Your command line is visible to other users"
    `gpudog run` sends up to 256 bytes of your command line to the
    daemon. Every user who can use GPUDog on this machine can see it in
    [`gpudog dash`](dashboard.md), and the daemon writes it to its log. Do
    not put passwords, tokens or other secrets in the command's arguments;
    pass them in a file or an environment variable instead.

The command starts at the first word that is not a `gpudog` option. The `--`
separator is optional, but it is clearer. In both of these examples, `-n 5` is
passed to `train`:

```shell-session
$ gpudog run -n 2 -- train -n 5
$ gpudog run -n 2 train -n 5
```

### `-n`, `--num_gpus`

Must be between `1` and `1024`. A request for more GPUs than are visible to
you or than the machine has, or more than the per-user limit (`8` by
default), is denied at once instead of waiting.

### `-g`, `--gpus`

*   Each entry is either a CUDA number between `0` and `1023`, or a full
    `GPU-<uuid>` as printed by `nvidia-smi -L` (case does not matter). One
    list cannot mix the two forms. Spaces around the entries are ignored.
*   Numbers are [CUDA numbers](../concepts.md#gpu-numbers) in the environment
    running `gpudog`. They follow `CUDA_VISIBLE_DEVICES`, `CUDA_DEVICE_ORDER`
    and container device restrictions, and need not match `nvidia-smi` or the
    daemon GPU numbers in `dash`. The client turns them into UUIDs before
    requesting a reservation.
*   A GPU cannot appear twice.
*   Empty entries such as `0,,1` or `0,` are rejected.
*   MIG instances and visible GPUs with MIG enabled are rejected. Failed MIG
    inspection is an error, not permission to use an unchecked GPU.
*   A CUDA number or UUID that is not visible to you fails before anything is
    requested. A visible UUID the daemon does not manage is denied by the
    daemon.

`gpudog run` needs the NVIDIA driver, even for UUID requests; it does not
use any GPU memory to check visibility. Without the driver it fails with
`CUDA driver unavailable or missing required symbols`; see
[CUDA driver errors](../troubleshooting.md#cuda-driver-or-mig-inspection-errors). An empty `CUDA_VISIBLE_DEVICES` or
`-1` means no GPU is visible, so requests fail; GPUDog never falls back to all
GPUs.

### `-t`, `--timeout`

A number with an optional unit. Units are case-insensitive:

| Unit | Meaning |
| -- | -- |
| none | seconds |
| `ms` | milliseconds |
| `s` | seconds |
| `m` | minutes |
| `h` | hours |
| `d` | days |

Examples: `30s`, `5m`, `2h`, `90`. The value must be greater than zero, and the
maximum is about 49 days. To wait forever, leave out `-t`. A request can wait
even when `dash` shows a GPU as not reserved: a GPU used by a process started
without `gpudog` is not granted (see
[Why a request waits](dashboard.md#why-a-request-waits-although-a-gpu-looks-free)). If the timeout runs
out, `gpudog` prints
`gpudog: timed out waiting for GPUs: no GPU became available before the deadline` and
exits with status `1` without starting the command.

`--timeout` is the only limit on waiting in the queue. The daemon measures
it from when it receives the request, so connecting does not count. If the
daemon itself stops responding after your request is queued, `gpudog run`
keeps waiting even with `-t`; press Ctrl-C to stop it (see
[Troubleshooting](../troubleshooting.md#gpudog-run-keeps-waiting-after-its-timeout)).
Separately, `gpudog` gives up after 10 seconds if the daemon does not answer
at all; see
[Troubleshooting](../troubleshooting.md#gpudogd-did-not-answer-within-10-seconds).

### No priority

You cannot choose a priority. The daemon serves users who use fewer GPUs
first; see [The queue](../concepts.md#the-queue). `-p` is no longer an
option, so a script that still passes it waits for and is granted GPUs as
usual, then tries to run a command called `-p` and exits with status `127`;
remove the option. A `gpudog run` inside
another `gpudog run` that asks for the GPUs its parent holds waits until the
parent ends, which never happens; give it `-t`, or do not nest them.

## Hold limit

The administrator can limit how long one `gpudog run` may keep its GPUs
(`max_hold_ms`). There is no limit by default, and it is the same for every
user. To know it before you start, ask your administrator, or look at the
`HELD` column of `gpudog dash` while other jobs run: a limit shows there after
a slash. Once your GPUs are granted, the `holding gpu(s)` line ends with it,
for example `; hold limit 1d` or `; hold limit 1h30m`. The limit counts from the grant; time spent waiting does
not count.

When the limit ends, `gpudog run` stops your command and every process it
started. A job that ignores `SIGTERM` prints, for example:

```text
gpudog: holding gpu(s) 0 (GPU-8f6a...); hold limit 1d
gpudog: the hold limit of 1d has ended; stopping the command (SIGKILL in 30s)
gpudog: the command did not stop after SIGTERM; sent SIGKILL
gpudog: the command was killed by signal 9 (SIGKILL)
```

*   The command and those processes first get `SIGTERM`. A command that then exits ends with
    `gpudog: the command was killed by signal 15 (SIGTERM)`, or with its own
    status if it handles the signal.
*   Those still running 30 seconds later get `SIGKILL`. The 30 seconds are
    fixed and cannot be changed, which may be too short to save a large
    model: save checkpoints regularly, and treat `SIGTERM` as the last
    chance to finish one that is already being written.
*   `gpudog` returns the command's status as usual: `143` for `SIGTERM`,
    `137` for `SIGKILL`, or the command's own status if it exits by itself
    (see [Exit status](#exit-status)). Only the `the hold limit of ... has
    ended` line tells a stop at the limit apart from a `SIGTERM` someone
    sent.
*   Processes the command started are stopped too, even if the command
    itself exits first at `SIGTERM`. `gpudog` then waits for them until the
    30 seconds end: a `SIGTERM` sent to `gpudog` meanwhile is not passed on,
    and Ctrl-C reaches only those still in the terminal's foreground job.
    Either way they get `SIGKILL` when the 30 seconds end. But the limit is enforced only while
    `gpudog` runs: if the command exits by itself before the limit, `gpudog`
    exits with it, and background processes it left behind are not stopped
    later (see
    [My GPUs were not released](../troubleshooting.md#my-gpus-were-not-released)).
*   The limit is kept by `gpudog run` itself, so it still applies if the
    daemon restarts while your command runs, although the GPUs are then no
    longer reserved for it (see [Signals](#signals)).
*   A process that has left your command's process tree is not stopped. It
    keeps the GPUs until it exits, and `gpudogd` logs it. This happens to a
    program that runs itself in the background by forking twice, and to a
    process started during the 30 seconds whose parent then exits before
    `SIGKILL` is sent.
*   A job stopped with Ctrl-Z keeps its GPUs, and `gpudog` is stopped with
    it. If the limit ends meanwhile, the command is stopped as soon as the
    job continues.
*   If `/proc` does not belong to the PID namespace `gpudog` runs in (for
    example under `unshare --pid --fork` without `--mount-proc`), only the
    command itself is stopped, not the processes it started.
*   For a shorter limit of your own, run the command under `timeout`, for
    example `gpudog run -- timeout 2h python train.py`.

## Output

`gpudog` writes its own messages to standard error, so they do not mix with
your program's standard output:

```text
gpudog: waiting for GPUs (queue position 0)
gpudog: holding gpu(s) 0,1 (GPU-8f6a...,GPU-1c2d...)
```

With a [hold limit](#hold-limit), the second line ends with it:
`gpudog: holding gpu(s) 0,1 (GPU-8f6a...,GPU-1c2d...); hold limit 1d`.

The waiting line is printed once, when the request is queued. The position
starts at `0`, the first request the daemon considers. It can change while
you wait, and a request that needs other GPUs can be served before yours;
see [The queue](../concepts.md#the-queue). Use `gpudog dash` to watch the
queue move.

The `holding gpu(s)` line shows [daemon GPU numbers](../concepts.md#gpu-numbers),
as `dash` does, not your CUDA numbers: `-g 1,3` can print `holding gpu(s) 0,2`.
The UUIDs in brackets are listed in the same order, so the first UUID belongs
to the first number. Your command sees these GPUs as its CUDA numbers `0`,
`1`, and so on, in that order.

## Exit status

| Situation | Exit status |
| -- | -- |
| The command ran | The command's own exit status |
| The command was killed by signal *N* | 128 + *N*, with `gpudog: the command was killed by signal N (SIGNAME)` (see below) |
| The command was stopped at its [hold limit](#hold-limit) | As above, usually `143` or `137`, after `gpudog: the hold limit of ... has ended` |
| The command was not found (`cannot run ...`) | `127` |
| The command is not executable | `126` |
| Invalid command-line syntax, missing command or conflicting options | `2` |
| Invalid GPU/timeout values, daemon unreachable, request denied or timed out | `1` |
| Cancelled while waiting by Ctrl-C, Ctrl-\\, `SIGTERM` or a closed terminal | Ended by that signal; the shell reports 130, 131, 143 or 129 |

`gpudog` does not run an executable script without a shebang through
`/bin/sh`; such a script fails with `ENOEXEC` and status `126`. Add a shebang or
explicitly run the script with its interpreter.

When the command is killed by a signal, `gpudog` prints a line such as
`gpudog: the command was killed by signal 9 (SIGKILL)`, adding `, core dumped`
when there is a core dump. As in bash, nothing is printed for `SIGINT`
(Ctrl-C) or `SIGPIPE` (for example `| head`). If the command dies of `SIGINT`,
`gpudog` also ends by `SIGINT` (unless `SIGINT` was ignored when `gpudog`
started), so a shell script loop stops on Ctrl-C as it would without `gpudog`. For every signal except `SIGINT` (including `SIGPIPE`,
which prints nothing), `gpudog` exits normally with status 128 + *N*.

## Signals

While the command runs, `gpudog` waits for it and returns its exit status. The
GPUs are released once the command, and every process it started that still
holds the reservation, has exited (see [Reservations](../concepts.md#reservations)).

*   Ctrl-C (`SIGINT`), Ctrl-\ (`SIGQUIT`) and a closed terminal (`SIGHUP`)
    reach the command directly. `gpudog` does not forward them again and does
    not exit on them; it keeps waiting while the command cleans up.
*   `SIGTERM` sent to `gpudog` is forwarded to the command itself, every time
    it arrives. Processes the command started do not receive it from
    `gpudog`; only the [hold limit](#hold-limit) signals them as well. If the command is a wrapper script, start the real program with
    `exec` (for example `exec python train.py`) so that it receives `SIGTERM`.
*   A signal that was already ignored when `gpudog` started, for example
    `SIGHUP` under `nohup gpudog run ...`, stays ignored for the command.
*   Ctrl-Z stops both `gpudog` and the command. The GPUs stay reserved while
    they are stopped; resume them with `fg` or end them.
*   If `gpudog` itself is killed (for example with `SIGKILL`), the command keeps
    running and keeps the GPUs reserved until it exits. Its exit status is
    then lost.

If the daemon stops or restarts while the command runs, `gpudog` prints
nothing and the command keeps running; its exit status is unaffected. Your
GPUs are no longer reserved, though, and may be granted to another job, for
example while yours is still loading data (see
[Running the daemon](../administration/daemon.md#shutdown)).

While the request is still waiting:

*   Ctrl-C, Ctrl-\\, `SIGTERM` or a closed terminal ends `gpudog` and cancels
    the request. Nothing was reserved and the command never starts.
*   Under `nohup`, a closed terminal does not cancel the request: `gpudog`
    keeps waiting and starts the command when the GPUs are granted.
*   Ctrl-Z stops `gpudog` but keeps the request queued. If the GPUs are granted
    while it is stopped, they stay reserved and unused until you resume it with
    `fg` or end it.

## Examples

Reserve one GPU:

```shell-session
$ gpudog run -- python train.py
```

Reserve two GPUs and give up after ten minutes:

```shell-session
$ gpudog run -n 2 -t 10m -- python train.py --ddp
```

Reserve CUDA numbers 1 and 3:

```shell-session
$ gpudog run -g 1,3 -- ./benchmark
```
