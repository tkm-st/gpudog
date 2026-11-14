# Daemon configuration

`gpudogd` reads `/etc/gpudog/config.toml`, or the file given with
`--config`. A missing default file uses the built-in defaults, with a
warning in the log; a missing file explicitly named by `--config` stops
startup.

```toml
max_held_per_uid = 4
max_queued_per_uid = 4
```

## Keys

Every key, its default and its range are listed in
[Configuration](../administration/configuration.md#settings).

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
    numbers clients pass to `run -g`. The startup
    log shows both, for example
    `gpu 0 NVIDIA A100 (GPU-1c2d..., nvml index 3)`, where `nvml index` is
    NVML's own number for the GPU where the daemon runs.
*   Grants name GPUs by UUID, so clients always get the right devices, even
    when the daemon runs in a container that sees only some GPUs.
*   The daemon refuses to start if a listed UUID does not exist or is outside
    its CUDA-visible view:
    `configured device GPU-... was not found`.
*   A reload applies a changed list, within the GPUs the daemon found at
    startup; see
    [Changing the configuration](../administration/configuration.md#changing-the-configuration).
*   Unlike other keys, an invalid `devices` value stops startup instead of
    being ignored: falling back to "every GPU" would hand out GPUs the
    administrator meant to keep out. That covers a value that is not an array
    of strings, an empty array, an entry that is not a `GPU-<uuid>`
    (`MIG-` instances are not supported), and a repeated entry.
*   `-n, --num-gpus` applies the list too, to its fake `GPU-static-N` UUIDs.

Production enumeration requires CUDA driver and NVML libraries. Visible
MIG-enabled GPUs, MIG instances and failed MIG inspection are fatal.
The explicit `--num-gpus` testing override bypasses CUDA/NVML/MIG discovery.

## Precedence and errors

Command-line options override the file, and the file overrides the defaults.

*   An unknown key, or a key with the wrong type, is logged as a warning and
    ignored (except `devices`, see above). A config written for a newer version still starts an older
    daemon.
*   A file that does not parse, or a value that cannot be honoured, stops the
    daemon from starting. It never applies only part of a configuration.
*   `SIGHUP` reloads the file. A reload refuses, instead of ignoring, a key
    with the wrong type or out of range and a missing file, and keeps the previous
    configuration. See
    [Changing the configuration](../administration/configuration.md#changing-the-configuration).

The obsolete `log_directory` key is accepted but only produces a warning.
The daemon logs to stderr, which goes to the systemd journal or to
`docker logs`.

## Command-line options

| Option | Meaning |
| -- | -- |
| `-c, --config <FILE>` | Configuration file. Default: `/etc/gpudog/config.toml`. |
| `-s, --socket <PATH>` | Socket to listen on. Overrides `socket_path`. |
| `--max-connections <N>` | Overrides `max_connections`. |
| `--handshake-timeout-ms <MS>` | Overrides `handshake_timeout_ms`. |
| `--allow-insecure-socket-directory` | Same as `require_secure_directory = false`. Unsafe: permits socket hijacking. |
| `-n, --num-gpus <N>` | Skip NVML and assume N GPUs. For testing only. |

## Using a config file with Docker

Mount its directory into the container:

```shell-session
$ docker run -d --name gpudogd --pid=host --userns=host --gpus all \
    -v /run/gpudog:/run/gpudog \
    -v /etc/gpudog:/etc/gpudog:ro \
    gpudogd
```

!!! note
    The image passes `--socket /run/gpudog/gpudogd.sock` by default, so
    `socket_path` in the file has no effect in Docker.

!!! warning
    Mount the directory, not the file alone. A single-file mount keeps
    showing the old file to the container when an editor saves by replacing
    it (vim, `sed -i` and most configuration tools), so a reload logs
    `configuration reloaded; nothing changed`.
