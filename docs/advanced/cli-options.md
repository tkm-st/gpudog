# CLI options

All subcommands accept `-s, --socket <PATH>` to reach a daemon on a
non-default socket. The default is `/run/gpudog/gpudogd.sock`.

Every `gpudog` command waits at most 10 seconds for the daemon to accept the
connection and complete the handshake, and at most 10 seconds for each answer
after that. The daemon answers at once even when it puts a request in the
queue, so this does not limit how long `run` waits for GPUs; that is what
`--timeout` is for.

## `gpudog run`

```shell-session
$ gpudog run [OPTIONS] -- COMMAND [ARGS...]
```

| Option | Default | Meaning |
| -- | -- | -- |
| `-n, --num_gpus <N>` | `1` | Number of GPUs to reserve. Cannot be combined with `-g`. Also accepted as `--num-gpus`. |
| `-g, --gpus <LIST>` | none | Exact GPUs, comma separated: CUDA numbers such as `0,2`, or UUIDs such as `GPU-8f6a...`. Cannot be combined with `-n`. |
| `-t, --timeout <DURATION>` | wait forever | How long to wait: `30s`, `5m` or `2h`. A bare number means seconds. The maximum is about 49 days. |

There is no priority option: the daemon orders the queue itself (see
[The queue](../concepts.md#the-queue)). `-p` is not an option any more,
so it starts the command, like any other word that is not an option: the
request waits for its GPUs as usual, and only then fails to run `-p`.

`-g` rules:

*   Each entry is either a CUDA number between `0` and `1023`, or a full
    `GPU-<uuid>` as printed by `nvidia-smi -L` (case does not matter). One
    list cannot mix the two forms. Spaces around the entries are ignored.
*   Numbers are [CUDA numbers](../concepts.md#gpu-numbers), following
    visibility and ordering restrictions. They are turned into UUIDs before
    the request is sent; `dash` shows daemon GPU numbers.
*   A GPU cannot appear twice.
*   MIG instances and visible MIG-enabled parent GPUs are rejected.
*   A CUDA number that does not exist, or a UUID the daemon does not manage, is
    rejected at once rather than waiting.

The command runs with `CUDA_VISIBLE_DEVICES` set to the UUIDs of the granted
GPUs, comma separated, and `CUDA_DEVICE_ORDER=PCI_BUS_ID`. It inherits the
reservation. If `gpudog` itself is killed, the GPUs stay
reserved until the command also exits. `gpudog run` exits with the
command's exit status.

`run` always asks CUDA which GPUs are visible, even when
`CUDA_VISIBLE_DEVICES` is unset. Numbers and UUIDs in that variable are
honoured; an empty value or `-1` makes every GPU invisible, so requests fail.
Invalid entries behave as they do in CUDA; GPUDog never falls back to all
GPUs. `-n` requests send the UUIDs of the visible GPUs, and the granted GPUs
are checked against the same list before the command starts. `run` needs the
NVIDIA driver (CUDA and NVML libraries).

## `gpudog health`

Checks that the daemon is responding. It takes only `--socket`.

## `gpudog dash`

| Option | Default | Meaning |
| -- | -- | -- |
| `-r, --refresh <SECONDS>` | `5` | Seconds between refreshes, from 1 to 3600. |
| `--no-gpu-processes` | off | Hide daemon process rows; does not disable GPU observation or waiting. |

Each refresh opens a new connection, so the dashboard never occupies one of
your connection slots. A refresh the daemon does not answer within 10
seconds shows an error, and the next refresh tries again. All text from the
daemon is reduced to printable ASCII before it is displayed.
