# Running gpudog

## Run a command

Put the command after `--`:

```shell-session
$ gpudog run -- python train.py
gpudog: holding gpu(s) 0 (GPU-8f6a1c2d-0000-1111-2222-333344445555)
```

`gpudog` asks the daemon for one GPU, waits until one is free, and then runs
the command with `CUDA_VISIBLE_DEVICES` set to the UUIDs of the GPUs it was
granted. While it waits, it prints its position in the queue. The GPUs are
released when the command ends. The number in the `holding gpu(s)` line is
the daemon GPU number, as shown by `gpudog dash`; inside your command the GPU
is CUDA number `0`. See [GPU numbers](../concepts.md#gpu-numbers).

Some common variations:

```shell-session
$ gpudog run -n 2 -- python train.py      # two GPUs
$ gpudog run -g 0,2 -- python train.py    # exactly CUDA numbers 0 and 2
$ gpudog run -g GPU-8f6a1c2d-... -- python train.py   # a GPU by UUID
$ gpudog run -t 30m -- python train.py    # give up after 30 minutes
```

See [CLI options](../advanced/cli-options.md) for details.

## Check the daemon

```shell-session
$ gpudog health
```

## Watch the machine

`gpudog dash` shows a live view of which user and process holds each GPU,
what is waiting, and for how long. Press Ctrl-C to quit.

```shell-session
$ gpudog dash
GPUDog: 2 GPUs, 1 free, 0 waiting (refreshes every 5s)

In use
  GPUS  UID   PID    HELD   COMMAND
  0     1000  48213  2m 5s  python train.py
```

The dashboard also lists every process that holds a GPU context, labelled as
one of the following:

*   `managed`: started under `gpudog run` and using its reserved GPU.
*   `managed (wrong GPU)`: started under `gpudog run` but using a GPU it did
    not reserve.
*   `not managed`: not part of any current `gpudog run` reservation, for
    example a process started without `gpudog`.
*   `unknown`: the daemon could not tell.

Any GPU with a process on it counts as in use, whatever its label, so `free`
in the first line excludes both reserved GPUs and GPUs used by other
processes. If the daemon cannot check GPU usage, the count shows
`unknown free` and no new reservation is granted until it can.
See [Why a request waits](../user-guide/dashboard.md#why-a-request-waits-although-a-gpu-looks-free).

## Read the daemon logs

```shell-session
$ journalctl -u gpudogd               # systemd
$ journalctl -u gpudogd -p warning    # warnings and errors only
$ docker logs gpudogd                 # Docker
```
