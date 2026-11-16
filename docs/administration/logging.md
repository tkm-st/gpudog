# Logging

`gpudogd` logs to standard error. There are no `access.log` or `error.log`
files and no log directory to configure.

| Level | Contents |
| -- | -- |
| Access / info | Startup (the first line is `gpudogd <version> (protocol v<N>) starting`) and shutdown, loaded config, configuration reloads, GPUs, acquire, grant, denial and release, and notes on GPU process observation (a job with too many processes to follow, a kernel without child lists). |
| Warning | Config warnings, a missing default config file, a new socket directory left with the umask's mode, keys that need a restart after a reload, connection limits reached, rejected connections, version mismatches, handshake timeouts, a client whose process the daemon cannot read, a request for a GPU the daemon does not manage, and GPU processes without a readable owner. |
| Error | Protocol violations, transport errors, startup failures, failed GPU observations (such as `cannot read procfs: ...`) and refused reloads ([Changing the configuration](configuration.md#changing-the-configuration)). |

Untrusted text is sanitised and bounded, so one event stays on one log line.

## Repeated warnings

A user can cause some warnings and errors as often as they like, for
example by connecting in a loop over their connection limit. So that one
user cannot crowd other lines out of the log, each of these is logged at
most once every 10 seconds per user:

*   connection limits reached, for the whole daemon or for one user;
*   rejected connections and version mismatches;
*   handshake timeouts, transport errors and protocol violations;
*   `peer process identity unavailable` and `requested unmanaged device`
    (see below).

The next line of the same kind ends with, for example,
`(12 similar lines not logged)`; if no later line of that kind comes, the
count is not logged. The daemon-wide connection limit and connections
rejected before the user is known are counted for all users together.
Acquire, grant, release and `denied:` lines are never left out. Only the log is throttled: the client still
gets its error every time. The line that is logged shows the uid, gid and
pid of that connection only, not those of the lines left out.

Two notes on GPU process observation are limited separately, without a
count: a job with too many processes to follow is logged at most once an
hour, and a kernel without child lists once at startup. Both are info lines that need no action, so
`journalctl -p warning` does not show them.

## Request lines

Each request line starts with the client's kernel-reported
`uid=... gid=... pid=...`, where the pid is that of the `gpudog` process:

```text
uid=1000 gid=1000 pid=4242 acquire num_gpus=1 candidates=[GPU-...] command=...
uid=1000 gid=1000 pid=4242 granted gpus=[0:GPU-...]
uid=1000 gid=1000 pid=4242 released its reservation
uid=1002 gid=1002 pid=4400 granted gpus=[1:GPU-...] hold_limit_ms=86400000
uid=1002 gid=1002 pid=4400 still holds its reservation after its hold limit of 86400000 ms; it is kept until the client closes it
uid=1001 gid=1001 pid=4300 denied: no GPU became available before the deadline
```

With [`max_hold_ms`](configuration.md#hold-limit) set, a grant line ends
with `hold_limit_ms=`. The `still holds its reservation` line is logged once,
when the limit ends and the GPUs are still held. Normally `gpudog run` is
about to stop its command, a few milliseconds later, and the `released`
line follows within about 30 seconds. If it does not, a process that left the command's process
tree, or a client other than `gpudog run`, still holds the GPUs; see
[A job holds GPUs too long](daemon.md#a-job-holds-gpus-too-long).

`released its reservation` also ends a request that was cancelled while it
waited. A request whose `-t` ran out normally ends with the `denied` line
above, or with `released its reservation` if the client disconnected at the
same moment. A client that disconnects before or right after the daemon
replies is not an error: only the usual acquire, grant, denied or released
lines appear, with no transport error. If a user waits well past `-t` and neither the
`denied` nor the `released` line appears, the daemon is probably stuck: restart it when the GPUs are idle.
Outside the journal, each line also starts with `gpudogd: access:` (see
[Reading logs](#reading-logs)).

A request can also log a warning, which starts with the same
`uid=... gid=... pid=...` (and with `gpudogd: warning:` outside the journal):
`peer process identity unavailable; GPU processes cannot be matched to this reservation (if gpudogd runs in a container, start it with --pid=host)`.
The daemon could not read the client's process. Until that job ends,
`gpudog dash` shows GPU processes on every GPU that no reservation accounts
for as `unknown` instead of `not managed`. GPUs in use are still not
granted. A Docker daemon started without `--pid=host` logs this for every
client; restart it with that flag (see
[With Docker](daemon.md#with-docker-recommended)). A daemon on the host
logs it only when a client exits right after connecting, which needs no
action.

Another request warning, `requested unmanaged device GPU-...`, means a
request named a GPU that this daemon does not manage. The request's
`denied: GPU-... is not managed by this daemon` line is always logged; the
warning is limited as described in [Repeated warnings](#repeated-warnings).
The user gets the same message; tell them which GPUs to use, for example
the ones `gpudog dash` lists (see
[`GPU-... is not managed by this daemon`](../troubleshooting.md#gpu-is-not-managed-by-this-daemon)).

## Reading logs

```shell-session
$ docker logs gpudogd
$ journalctl -u gpudogd
$ journalctl -u gpudogd -p warning
```

Under systemd, `JOURNAL_STREAM` enables journal severity prefixes. Otherwise
lines are prefixed with `gpudogd: <level>:`. When running manually, stderr
can be redirected through ordinary shell tools.

Logs pass through a bounded queue to a writer thread, so a slow stderr pipe
does not block scheduling. If the queue fills, events are dropped and the writer reports
`N log line(s) dropped: the log output was not keeping up`. Rotation and retention are handled by
Docker or journald, not GPUDog.

## Obsolete log directory settings

The `log_directory` key and the `-l/--log-directory` option date from
before logs went to stderr. They are still accepted, but warn and are
ignored. Remove them, and any `/var/log/gpudog` mounts, from `config.toml`,
unit files and container commands.
