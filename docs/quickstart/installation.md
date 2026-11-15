# Installation

You need the daemon, `gpudogd`, running once per machine, and the client,
`gpudog`, on the path of every user.

## Option A: Docker

No image is published, so build it from a clone of the repository, then run
it with access to every GPU and the socket directory shared with the host:

```shell-session
$ git checkout v0.0.1      # the release your clients come from
$ docker build -t gpudogd .
$ docker run -d --name gpudogd --restart unless-stopped --pid=host --userns=host --gpus all \
    -v /run/gpudog:/run/gpudog \
    gpudogd
```

!!! warning
    A rootful Docker engine creates `/run/gpudog` on the host, owned by
    root with mode `0755`, whenever it is missing. If your engine does
    not (for example with user namespace remapping enabled), add the line
    `d /run/gpudog 0755 root root -` in `/etc/tmpfiles.d/gpudog.conf`, which
    recreates it at every boot. The directory must be owned by
    root and not writable by group or others. The daemon refuses to bind
    otherwise, because a socket directory that others can write to would
    let any user hijack the connection.

The default Docker image contains only the daemon (a `client` image for jobs
in containers is described in
[Container clients](../user-guide/container-clients.md)). For a Docker
deployment, install the host client from a release tarball (see
[From a GitHub release](../installation.md#from-a-github-release)) or by
building it:

```shell-session
$ cargo build --locked --release -p gpudog
$ sudo install -m 0755 target/release/gpudog /usr/local/bin/gpudog
```

Do not install the combined `.deb` while a Docker daemon uses the same
socket. Its maintainer scripts also enable and start a systemd daemon, which
can conflict with Docker. The unit removes its runtime directory on stop;
do not share that directory with an independently managed Docker daemon.

## Option B: Debian package

Use this option instead of the Docker daemon. The package ships `gpudog`,
`gpudogd`, a commented-out `/etc/gpudog/config.toml`, and a systemd unit that
creates `/run/gpudog` and starts the daemon. Download it from a GitHub
release and check it first (see
[From a GitHub release](../installation.md#from-a-github-release)), then:

```shell-session
$ sudo apt install ./gpudog_0.0.1-1_amd64.deb
$ systemctl status gpudogd
```

Before you remove it, read
[Debian package](../installation.md#debian-package), and before you upgrade
it, read [Upgrading the package](../installation.md#upgrading-the-package):
removing it ends every reservation, and an upgrade may ask about
`/etc/gpudog/config.toml`.

To build the package yourself, install
[cargo-deb](https://github.com/kornelski/cargo-deb) and run:

```shell-session
$ cargo build --locked --release --workspace
$ cargo deb -p gpudogd --no-build
```

## Option C: From source

See [Building from source](../developer/building.md) for what the build
needs. Then start the daemon as in
[Without Docker](../administration/daemon.md#without-docker).

```shell-session
$ cargo build --locked --release --workspace
$ sudo install target/release/gpudog target/release/gpudogd /usr/local/bin/
```

Remove these copies before you install the Debian package later
(`sudo rm -f /usr/local/bin/gpudog /usr/local/bin/gpudogd`); they would come
first in `PATH`.

## Check the installation

```shell-session
$ gpudog health
```
