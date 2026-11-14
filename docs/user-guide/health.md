# `gpudog health`

Checks that `gpudogd` is running and responding.

```shell-session
$ gpudog health [-s SOCKET]
```

| Option | Description | Default |
| -- | -- | -- |
| `-s`, `--socket` | Daemon socket to connect to. | `/run/gpudog/gpudogd.sock` |

The client connects, completes the protocol handshake, sends a `ping` and
waits for a `pong`. A daemon that is running but does not answer within 10
seconds counts as not responding, so the check never hangs.

```shell-session
$ gpudog health
gpudogd is responding on /run/gpudog/gpudogd.sock
```

The exit status is `0` when the daemon responds and `1` otherwise, so the
command can be used in scripts and monitoring:

```shell-session
$ gpudog health
gpudog: cannot reach gpudogd at /run/gpudog/gpudogd.sock: No such file or directory
gpudog: is gpudogd running?
$ echo $?
1
```

A daemon that does not answer in time fails the check with
`gpudogd did not answer within 10 seconds`. A version mismatch between the
client and the daemon also makes the check fail. See [Troubleshooting](../troubleshooting.md).
