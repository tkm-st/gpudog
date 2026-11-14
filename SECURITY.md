# Security policy

`gpudogd` runs as root and serves unprivileged local users, so a flaw in it
can let one user act as root or as another user. Please report such flaws
privately.

## Reporting a vulnerability

Report it through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**. Do not
open a public issue, pull request or discussion for it.

Include what you can of:

*   the GPUDog version (`gpudog --version`, `gpudogd --version`) and how it
    was installed (Debian package, release tarball, Docker image or source);
*   the steps or a program that shows the problem, and what an attacker
    gains;
*   relevant log lines (`journalctl -u gpudogd` or `docker logs gpudogd`),
    with other users' `uid`, `pid` and `command=` text removed: command
    lines can hold paths and tokens.

The report stays private until a fix is released. You will get a reply in
the advisory thread, and you are credited in the advisory unless you ask not
to be.

## Supported versions

Only the latest release gets security fixes. Before 1.0, a fix may come in
a release that also changes other behaviour; see
[`CHANGELOG.md`](CHANGELOG.md).

## Scope

In scope is anything that crosses the daemon's trust boundary, including:

*   the Unix socket and its directory, peer credentials and per-user limits;
*   parsing of requests, `config.toml`, command-line options and a
    configuration reload (`SIGHUP`), which must never widen access;
*   process observation through NVML and procfs, and the escaping of the
    text it shows in `gpudog dash` and in messages;
*   the `gpudog` client's device selection and the environment it gives the
    command: a command must only ever see the GPUs it was granted;
*   daemon logging, the systemd unit and the Debian maintainer scripts;
*   the Dockerfile images and the `packaging/compose.yaml` example;
*   the release tarball and Debian package.

Crashing, hanging or starving the daemon, or getting around the per-user
limits through the socket, is in scope even when done on purpose.

Out of scope is what GPUDog does not try to prevent, or documents as
intended (see the [security model](docs/administration/security.md)):

*   a user who ignores `CUDA_VISIBLE_DEVICES` or runs GPU work without
    `gpudog`, or a process that starts on a GPU just after it was checked;
*   every socket user seeing all reservations and observed GPU processes,
    with their uid, pid and command;
*   anything root or the owner of the configuration file can do;
*   a setup made insecure on purpose, such as
    `require_secure_directory = false`,
    `--allow-insecure-socket-directory`, or a socket directory,
    configuration file or a directory above either that others can write;
*   releases other than the latest, and the Debian 10 build and test stages
    in `Dockerfile.release`, which are not shipped.
