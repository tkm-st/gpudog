# Design notes

Decisions and pitfalls behind the implementation that the code alone does
not explain. The decision record is `notes/development_guide.md`; the wire
format is in `notes/ipc-design.md`.

<!-- markdownlint-disable-next-line MD036 -->
_Last verified: 2026-11-13 against `main`_

## Summary

The service is built so that its lifetime bugs cannot occur rather than
being worked around, untrusted input is bounded in bytes, and packaging
relies on a few cargo-deb behaviours that are easy to break.

## Details

### Wire and codec

*   Wire strings are `Vec<u8>`, not `String`, and the bounds
    (`MAX_COMMAND_SIZE` and so on) count bytes. Lossy UTF-8 decoding was
    rejected: U+FFFD is 3 bytes, so a 256-byte hostile command could grow
    past the bound, and the daemon's `StatusReply` echo would then fail to
    encode. `sanitize_display_text(&[u8]) -> String` is the only way to turn
    one into display text.
*   `DenyReason` is a `u16` newtype, not an enum, because unknown reasons
    must decode for forward compatibility.
*   Read deadlines go at the call site with `tokio::time::timeout`; dropping
    the future cancels the read.
*   A stream that ends partway through a frame surfaces as `CodecError::Io`
    with `io::ErrorKind::UnexpectedEof`, built by `FrameCodec::decode_eof`
    (tokio_util's default returns a different error), not as
    `ProtocolError::Truncated`. The service treats it as a quiet hang-up.

### Service

*   `tokio::select!` (biased toward the wakeup) races the grant against peer
    closure and drops the losing branch however it finished.
*   Each queued session gets a `oneshot` registered under the same `Mutex`
    as `scheduler.submit`. A value sent before the receiver is awaited is
    kept, so there is no cancel-before-wait window.
*   All session cleanup (release, per-uid counters, waiter and session maps,
    pump) is `SessionGuard::drop`. The guard is created in the accept loop
    before `tokio::spawn` and owns an `Arc<Shared>`, so it also runs when the
    task is dropped at runtime shutdown (test
    `shutdown_with_a_live_session_still_releases_it`).
*   The `std::sync::Mutex` is never held across `.await`, so the runtime may
    be multi-threaded (test
    `every_client_is_eventually_granted_under_concurrent_churn`).
*   Deadlines: a task sleeps until `next_deadline()` or a
    `Notify::notify_one` rearm; `notify_one` stores a permit, so a rearm
    cannot be missed.
*   `State::pump` releases a grant whose waiter is gone (the session is
    unwinding) and then ticks again, so the next queue head is granted in
    the same pump instead of waiting for the guard's own pump. The loop ends
    because each reclaimed id has left the queue for good (test
    `pump_hands_a_reclaimed_grant_to_the_next_waiter_at_once`).
*   `snapshot` truncates each device name and UUID to `MAX_STRING_SIZE`
    bytes. `encode` rejects longer strings, so one oversized NVML string
    would otherwise fail the whole `StatusReply`.
*   Pre-acquire **writes** are bounded by `handshake_timeout` as well as
    reads, so a client sending `Status` without reading cannot pin a
    connection slot. The deadline is per message, not per phase, because a
    dashboard polls `Status` on one connection indefinitely.
*   An existing `socket_path` that is not a socket is refused (`exists and
    is not a socket`) rather than deleted.
*   A panicking session is logged as `<peer> session failed: panic` and is
    still unwound by the guard.
*   The listen backlog is `SOMAXCONN`, through nix
    `socket`/`bind`/`listen`, instead of std's 128.

### Logging

*   There are no log files. `gpudogd` logs to stderr for the systemd journal
    (`crates/gpudog-daemon/src/daemon_log.rs`).
*   When `JOURNAL_STREAM` is set, each line is `<N>message` (6 access,
    4 warning, 3 error), so `journalctl -u gpudogd -p warning` works;
    otherwise `gpudogd: <level>: message`.
*   Control characters become `?` and messages are capped at 4096
    characters, so one event is always one journal line. journald splits
    lines over `LineMax` (48 KiB) and would parse the tail as a new
    prefixed line.
*   Writes go through a bounded queue (4096 lines) to one writer thread;
    when it is full, lines are dropped and a count is reported. A
    synchronous `write_all` on stderr froze every session when the pipe
    filled, which would stop all grants and expiries. `DaemonLog::flush`
    runs at exit so the fatal reason is not lost.
*   The `log_directory` key and `-l/--log-directory` flag are still accepted
    but only warn, so older configs and unit files start.
    `config_compat.rs` normalises `log_directory` out of both sides and
    separately asserts the obsolete warning.

### Configuration

*   `crates/gpudog-daemon/tests/config_compat.rs` checks every fragment in
    `tests/golden/config_cases.txt` against `config_recorded_results.txt`:
    the same warnings and values, or the same error line. The results are
    a frozen record of an earlier loader and are never regenerated.
    Deliberate differences are listed in `DIVERGENCES` and asserted to
    still differ:
    *   `socket_mode` accepts only octal digits, not `" 0666"`, `"+0666"`
        or `"-0"`.
    *   A NUL in `socket_path` is fatal in `validate()` instead of
        truncating the path.
*   `config_cases.txt` intentionally contains CRLF test input; a
    path-specific `.gitattributes` rule keeps `git diff --check` quiet.
*   A config file with invalid UTF-8 says `not valid UTF-8`. `apply_file`
    opens the file once and runs the ownership/mode check on that handle
    (`fstat`), so there is no window between check and read, and a FIFO is
    refused without blocking. Symlinks are still followed and their target
    checked. A permission error on a parent directory is fatal, not "file
    missing, use defaults".

### Client

*   `gpudog run` keeps the reservation alive through the child:
    `Client::into_reservation` clears `FD_CLOEXEC`, and the child inherits
    the descriptor. std/tokio `Command` do not close inherited fds, so
    killing `gpudog` (even SIGKILL) keeps the GPU held until the child exits
    (test `killing_gpudog_does_not_release_gpus_the_child_still_uses`).
*   The command is `Vec<OsString>`, so non-UTF-8 arguments reach the child
    byte for byte; only the display copy sent to the daemon is lossy.
*   Exit codes: 127 not found, 126 permission denied or `ENOEXEC`, 128+N for
    death by signal N, 1 for other spawn failures. Unlike glibc `execvp`,
    std does not retry an `ENOEXEC` script without a shebang through
    `/bin/sh`, so such a script gets 126.
*   Denial details from the daemon are sanitised before printing, so a
    hostile daemon cannot put escape sequences into the user's terminal.
*   `gpudog dash` is terminal only (a user decision). It redraws plain ASCII
    every `-r` seconds (1..=3600), on a fresh connection per poll, and
    sanitises every daemon string again. There is no `--once` mode
    (declined for now).

### NVML

*   nvml-wrapper loads the bare `libnvidia-ml.so` by default, which exists
    only with the -dev package. `gpudog_core::nvml::load_nvml`, used by the
    daemon's inventory and process observation and by the client's CUDA
    view, tries `libnvidia-ml.so.1` (what the container runtime injects)
    first, through `Nvml::builder().lib_path(..)`.

### Packaging

*   `cargo build --release --workspace && cargo deb -p gpudogd --no-build`
    gives `target/debian/gpudog_<version>-1_amd64.deb` with
    `/usr/bin/gpudog`, `/usr/bin/gpudogd` and
    `/usr/lib/systemd/system/gpudogd.service`.
*   The systemd maintainer scripts (enable, start, stop on remove; no stop or
    restart on upgrade) are generated by cargo-deb from
    `[package.metadata.deb.systemd-units]`, which only happens when
    `maintainer-scripts` names a directory (`packaging/debian/`, whose
    `postinst` adds the upgrade notice). cargo-deb substitutes `#DEBHELPER#`
    wherever it appears, even inside a comment. `Depends` is computed from
    the build host's glibc, so a `.deb` built on Ubuntu 24.04 needs
    glibc 2.39.
*   cargo-deb 3.8 reads only fixed names (`config`, `preinst`, `postinst`,
    `prerm`, `postrm`, `templates`) from the `maintainer-scripts` directory,
    so `packaging/debian/copyright` there is just a file. An asset that
    targets `usr/share/doc/gpudog/copyright` replaces cargo-deb's generated
    copyright, whose `license-file` text is appended unindented and so
    breaks the machine-readable format; the array form `["LICENSE", 0]` is
    rejected.
*   cargo-deb marks every asset under `etc/` as a conffile without a
    `conf-files` key, so `packaging/config.toml` becomes the conffile
    `/etc/gpudog/config.toml`. dpkg prompts when that path already exists on
    a first install (or an upgrade from a package that did not ship it), and
    without a tty the install fails with `end of file on stdin at conffile
    prompt`; `--force-confold` keeps the existing file. `dpkg-deb
    --contents` shows the owner as `0/0`, not `root/root`.
    `Dockerfile.release` copies `packaging/config.toml` before its
    `cargo test`, which reads it. The config test
    `the_packaged_template_changes_nothing_...` keeps its commented values
    equal to the defaults.
*   `packaging/gpudogd.service` uses `RuntimeDirectory=gpudog` (root,
    0755), which passes `directory_complaint`. Elsewhere `gpudogd` creates
    a missing socket directory itself (`create_socket_directory`: last
    component only, 0755 restored only under a parent that passes
    `directory_complaint`). The operator still creates a missing parent, and
    the host directory behind a bind that does not create it (Compose long
    syntax). The unit sets `ProtectSystem=strict` (checked
    2026-11-15 on WSL2 with an RTX A5000: start, NVML enumeration, socket,
    reload; GPU process observation could not be checked there). Other
    hardening options are not added yet.

### Development

*   Unix socket paths are limited to 108 bytes (`sun_path`). Tests that bind
    sockets under a deep checkout or scratch directory fail with `File name
    too long`; run them from a shorter path.
*   The development host (WSL2) has no Docker, and its `sha256sum` and other
    coreutils are uutils, not GNU. `scripts/release-check.py` and
    `scripts/container-check.py` therefore first run in CI, and docs claims
    about GNU-specific behaviour need a GNU system (a Debian container) to
    check, or should be left out.

## Why

These choices remove whole classes of failure (lost wakeups, cleanup
skipped at shutdown, a stalled log freezing the daemon, a hostile string
breaking a reply) by construction, so they are recorded here to keep them
from being undone by a change that looks simpler.

## See also

*   `notes/ipc-design.md`: wire format and transport.
*   `notes/development_guide.md`: the decision record.
*   `docs/developer/architecture.md`: current module layout.
