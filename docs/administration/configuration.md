# Configuration

`gpudogd` reads its settings from a TOML file, by default
`/etc/gpudog/config.toml`. Use `--config` to choose a different file.

*   The Debian package installs this file with every setting commented out,
    showing its default (`devices` shows an example). Uncomment a line to
    change that setting. See
    [Debian package](../installation.md#debian-package) for what upgrades and
    `apt purge` do with it.
*   If the default file does not exist, the built-in defaults are used, and
    the daemon logs `no configuration file at /etc/gpudog/config.toml; using
    the defaults` as a warning.
*   If a file given with `--config` does not exist, the daemon refuses to
    start, even when it is the default path.
*   The file must be a regular file owned by root (or by the user running the
    daemon) and must not be writable by group or others. It controls the
    socket path and the security checks, so anyone who can write to it
    controls the daemon.
*   Files larger than 1 MiB are rejected.

## Settings

| Key | Type | Default | Description |
| -- | -- | -- | -- |
| `log_directory` | string | unused | Obsolete key; warns and is ignored. Logs go to stderr/journal. |
| `socket_path` | string | `"/run/gpudog/gpudogd.sock"` | Unix domain socket to listen on. |
| `socket_mode` | string | `"0666"` | Permissions set on the socket, as an octal string. |
| `require_secure_directory` | boolean | `true` | Refuse to start if the socket directory is writable by others. |
| `max_connections` | integer (1–1,000,000) | `256` | Maximum client connections at once. |
| `max_connections_per_uid` | integer (1–1,000,000) | `16` | Maximum connections at once from one user. |
| `handshake_timeout_ms` | integer (1–3,600,000) | `10000` | Milliseconds a client may take for each message before it asks for GPUs. |
| `watch_unmanaged_ms` | integer (1–3,600,000) | `1000` | GPU process observation interval; waiting is always enabled. Failures pause new grants, not running jobs. |
| `max_held_per_uid` | integer (1–1,000,000) | `8` | Maximum GPUs one user may hold at once. Set it below the number of GPUs if one user's large request must not be able to keep the whole machine idle while it waits (see [Security](security.md#untrusted-input)). |
| `max_queued_per_uid` | integer (1–1,000,000) | `8` | Maximum requests one user may have waiting at once. |
| `max_hold_ms` | integer (0–4,294,967,295, about 49.7 days) | `0` | Milliseconds one request may hold its GPUs, counted from the grant. `0` means no limit. See [Hold limit](#hold-limit). |
| `devices` | array of strings | all GPUs | GPUs this daemon manages, as `GPU-<uuid>` strings from `nvidia-smi -L`. See [Managing some GPUs](#managing-some-gpus). |

A value of `0` for a limit stops startup, except for `max_hold_ms`, where
`0` means no limit. A negative value, or one above the upper bound, is
ignored with a warning, and the default is used.

## Example

```toml
max_held_per_uid = 4
max_queued_per_uid = 4
```

## Hold limit

`max_hold_ms` limits how long one `gpudog run` may keep its GPUs. For
example, to stop jobs after 24 hours:

```toml
max_hold_ms = 86400000
```

A value above the upper bound is ignored with a warning at startup, which
leaves no limit at all; check the journal after setting it (a reload refuses
such a file instead).

Choose a value longer than the jobs your users normally run, and tell them
before you set it: jobs that reach it are stopped, with 30 seconds to save
their state, so users need to checkpoint and resume within it. Users can see
it only once their GPUs are granted, or in `gpudog dash` next to other jobs.

*   The daemon sends the limit with each grant. When it ends, `gpudog run`
    sends `SIGTERM` to the command and every process the command started,
    then `SIGKILL` to those still running 30 seconds later. The GPUs are
    free once they have exited.
*   The daemon itself never stops a process or takes GPUs back. A
    reservation still held after its limit (for example by a process that
    left the command's process tree, or by a client that does not stop its
    command) is logged once and kept until it is released; see
    [Request lines](logging.md#request-lines).
*   The limit applies to every user alike, whether or not anyone is waiting.
*   It counts from the grant, not from when the request was queued. Waiting
    time is limited by `gpudog run -t` instead.
*   A reload changes the limit for later grants only. Jobs that already
    hold GPUs keep the limit they were given.

## Managing some GPUs

By default the daemon manages every GPU it can see. To manage only some of
them, list their UUIDs:

```toml
devices = [
    "GPU-8f6a1c2d-0000-1111-2222-333344445555",
    "GPU-1c2d3e4f-aaaa-bbbb-cccc-ddddeeeeffff",
]
```

*   The managed GPUs are numbered `0`, `1`, ... in NVML (PCI bus) order.
    These are [daemon GPU numbers](../concepts.md#gpu-numbers), not the CUDA
    numbers clients pass to `run -g`. `dash` shows this numbering.
*   A reload can change the list, but only among the GPUs the daemon saw
    when it started; see
    [Changing the configuration](#changing-the-configuration).
*   Grants name GPUs by UUID, so clients always get the right devices.
*   The daemon enumerates its own CUDA view and refuses to start if a listed
    UUID does not exist or is outside that view. Numeric/empty visibility
    restrictions are not ignored. MIG-enabled visible GPUs are refused too.
*   Unlike other keys, an invalid `devices` value stops startup instead of
    being ignored: falling back to "every GPU" would hand out GPUs the
    administrator meant to keep out. That covers a value that is not an array
    of strings, an empty array, an entry that is not a `GPU-<uuid>`
    (`MIG-` instances are not supported), and a repeated entry.

## Precedence

From lowest to highest:

1.  Built-in defaults
2.  The configuration file
3.  Command-line options (see [Running the daemon](daemon.md#command-line-options))

Only options you actually type on the command line override the file.

## Errors and warnings

*   **Unknown keys**, **values of the wrong type** and **integers outside the
    range the setting can hold** are reported on standard error (the journal
    under systemd), and then ignored. This lets a configuration
    written for a newer version still start an older daemon.
*   **A file that is not valid TOML** stops startup.
*   **A value that cannot be used**, such as `max_held_per_uid = 0` or
    a `devices` list with a GPU that does not exist, stops startup. The daemon never
    runs with only part of a configuration applied.

## Changing the configuration

To apply changes without a restart, make the daemon reload the file with
`SIGHUP`:

```shell-session
$ sudo systemctl reload gpudogd    # packaged service
$ docker kill -s HUP gpudogd       # Docker
```

*   These settings take effect at once, for new connections, requests and
    observations: `max_connections`, `max_connections_per_uid`, `handshake_timeout_ms`, `watch_unmanaged_ms`,
    `max_held_per_uid`, `max_queued_per_uid` and `max_hold_ms` (for later
    grants only). The log shows what changed,
    for example `configuration reloaded: max_held_per_uid 8 -> 4`.
*   A changed `devices` takes effect at once too, as
    `configuration reloaded: devices`, with one log line per GPU added
    (`now managing GPU-...`) or removed (`no longer managing GPU-...`) and the
    new numbering (`gpu 0 ...`, as at startup):
    *   An added GPU is granted only after a GPU process observation that
        includes it succeeds, normally within `watch_unmanaged_ms`. Any GPU
        the daemon saw when it started can be added, including ones it was
        not managing.
    *   Removing the `devices` line, or the whole file's contents, means
        "every GPU the daemon saw at startup", including ones you left out
        on purpose. If another daemon manages those GPUs, this one reload
        makes the two daemons hand out the same GPUs.
    *   A removed GPU gets no new reservations. A job that holds it keeps it
        until the job ends. `dash` still lists it, until the first configuration reload after its job ends: held
        by that job, then with no holder, but never counted as free. In the
        `gpu N ...` lines of that reload, it ends with
        `removed, reserved until its job ends`; no action is needed.
    *   Daemon GPU numbers are recomputed only at a reload: the remaining
        GPUs are numbered `0`, `1`, ... again. A removed GPU stays in the
        numbering until the first reload after its job ends; that reload logs
        `configuration reloaded: daemon GPU numbers`. Like any other, its
        number can change at a reload before then. Compare GPUs across
        reloads by UUID.
    *   Only GPUs the daemon saw when it started can be added. A reload that
        lists another one is refused with
        `configured device GPU-... was not found when gpudogd started; restart gpudogd to use a GPU added since it started`.
        To use a GPU installed or made visible since then, restart the daemon
        when the GPUs are idle (see [Shutdown](daemon.md#shutdown)).
*   Nothing that is already running is cut off. Lowering a limit does not
    close connections or take GPUs back, and waiting requests keep their
    place, even if their user is now over `max_queued_per_uid`. A waiting request is still granted
    only within the new `max_held_per_uid`: if its user already holds GPUs,
    it waits until enough of them are released, without holding up other
    users' requests. A waiting request that could never be granted any more
    is denied at once, with the message it would get if it were new:
    *   It asks for more GPUs than the new `max_held_per_uid`:
        `requested N GPUs but the per-user cap is M GPUs`.
    *   It names a GPU that was removed:
        `GPU-... is not managed by this daemon`.
    *   It asks for more GPUs than remain: for example
        `requested 3 GPUs but this host has only 2 GPUs`, or
        `requested 1 GPU but only 0 GPUs visible to the client and managed by gpudogd`
        when every GPU it could use was removed.

    The waiting user sees the message after `request can never be satisfied:`
    or `invalid request:`.
*   `socket_path`, `socket_mode` and `require_secure_directory` still need a
    restart. A reload logs
    `<key> changed; restart gpudogd to apply it` and keeps the old value.
    The warning repeats on every reload until the daemon is restarted.
    Read the warning about restarts in
    [Running the daemon](daemon.md#shutdown).
*   Command-line options still override the file.
*   The file is read as a whole, as at startup: a key you remove goes back to
    its default.
*   A reload is stricter than startup. If the file is missing, is not valid
    TOML, fails the ownership and mode checks, has a value of the wrong type
    or out of range, or has a value that cannot be used, the daemon logs
    `configuration reload failed; keeping the previous configuration: <reason>`
    and changes nothing. Unknown keys are only reported, as at startup.
*   `systemctl reload` reports success as soon as the signal is sent, even if
    the daemon then refuses the file. Check the log after every reload:

    ```shell-session
    $ journalctl -u gpudogd --since -1min -g 'configuration reload|ignoring|restart gpudogd|managing|gpu [0-9]'
    $ docker logs --since 1m gpudogd 2>&1 | grep -E 'configuration reload|ignoring|restart gpudogd|managing|gpu [0-9]'
    ```

    `configuration reloaded: ...` or `configuration reloaded; nothing changed`
    means the file was applied; `configuration reload failed; ...` means it was
    not. The `ignoring '<key>': ...` warning just before a failure names the
    value that was refused; on a reload it means the whole file was refused,
    not just that key.

## Socket permissions

The default `socket_mode` of `0666` lets every local user connect. This is
intended: the daemon identifies users through the kernel, not through file
permissions, so the socket mode is not an access-control mechanism.
