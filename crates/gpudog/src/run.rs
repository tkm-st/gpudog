//! GPU acquisition and child-process lifetime for `gpudog run`.

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitCode;

use gpudog_client::cli_options::{build_acquire, RunOptions};
use gpudog_client::client::ClientError;
use gpudog_client::cuda_visible_devices;
use gpudog_core::device_visibility::VisibleDevices;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::signal::unix::{signal, SignalKind};

use crate::common::{complain, connect, note};
use crate::hold_limit::{self, HoldLimit};
use crate::idle_stop;

/// Shell conventions for a command that could not be run, and the offset a
/// shell adds when a process dies from a signal.
const EXIT_COMMAND_NOT_FOUND: u8 = 127;
const EXIT_COMMAND_NOT_EXECUTABLE: u8 = 126;
const EXIT_SIGNAL_BASE: i32 = 128;

/// Reports a command killed by `number`, as a shell would. SIGINT and
/// SIGPIPE stay quiet, as in bash: Ctrl-C and `| head` are routine.
fn report_signal(number: i32, core_dumped: bool) {
    let signal = Signal::try_from(number).ok();
    if matches!(signal, Some(Signal::SIGINT | Signal::SIGPIPE)) {
        return;
    }
    let name = signal.map_or_else(|| "unknown signal".to_owned(), |s| s.as_str().to_owned());
    let core = if core_dumped { ", core dumped" } else { "" };
    note(format!(
        "the command was killed by signal {number} ({name}{core})"
    ));
}

/// The caller's environment with the two CUDA variables replaced, so a
/// client cannot widen its reservation by pre-setting them. `visible` is a
/// list of UUIDs, so order no longer decides which device is meant. The
/// child's CUDA numbers follow the order of that UUID list (the requested
/// order for `-g`), not the daemon's numbering shown by `gpudog dash`.
/// `CUDA_DEVICE_ORDER` is replaced with `PCI_BUS_ID` so a pre-set value
/// cannot change how the list is interpreted; it does not change that order.
fn child_command(command: &[OsString], visible: &str) -> tokio::process::Command {
    let mut child = tokio::process::Command::new(&command[0]);
    child
        .args(&command[1..])
        .env_remove("CUDA_VISIBLE_DEVICES")
        .env_remove("CUDA_DEVICE_ORDER")
        .env("CUDA_VISIBLE_DEVICES", visible)
        .env("CUDA_DEVICE_ORDER", "PCI_BUS_ID");
    child
}

/// The signals ignored (SIG_IGN) on entry, from the `SigIgn:` mask in
/// `/proc/self/status`; bit `n - 1` is signal `n`. Unreadable means none:
/// then all four are caught, as before this check existed.
fn ignored_signals() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| parse_sig_ign(&status))
        .unwrap_or(0)
}

fn parse_sig_ign(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
}

fn signal_bit(signal: Signal) -> u64 {
    1 << (signal as i32 - 1)
}

/// The next delivery on `stream`; never, for a signal left ignored.
async fn recv(stream: &mut Option<tokio::signal::unix::Signal>) {
    match stream {
        Some(stream) => {
            stream.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// The daemon's idle stop: `Some` once it arrives, `None` if the
/// connection ended without one; never, without an idle limit.
async fn idle_stop_of(idle: &mut Option<tokio::sync::oneshot::Receiver<u64>>) -> Option<u64> {
    match idle {
        Some(receiver) => receiver.await.ok(),
        None => std::future::pending().await,
    }
}

/// Sleeps until `deadline`; never, without one.
async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn run(socket: &Path, command: &[OsString], options: RunOptions) -> ExitCode {
    // Built before connecting: a request that cannot even be encoded should
    // not cost the daemon a connection slot.
    let request = match build_acquire(&options) {
        Ok(request) => request,
        Err(complaint) => return complain(complaint),
    };
    let visible_devices = match tokio::task::spawn_blocking(VisibleDevices::probe).await {
        Ok(Ok(view)) => view,
        Ok(Err(error)) => return complain(error),
        Err(_) => return complain("CUDA visibility worker failed"),
    };
    let request = match visible_devices.prepare_acquire(&request) {
        Ok(request) => request,
        Err(error) => return complain(error),
    };
    let mut client = match connect(socket).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let granted = match client
        .acquire_with_visibility(&request, &visible_devices, |position| {
            note(format!("waiting for GPUs (queue position {position})"));
        })
        .await
    {
        Ok(granted) => granted,
        Err(error @ ClientError::Denied { .. }) => return complain(error),
        Err(error @ (ClientError::Visibility(_) | ClientError::Unresponsive(_))) => {
            return complain(error)
        }
        Err(error) => return complain(format!("lost the connection to gpudogd: {error}")),
    };
    if granted.gpus.is_empty() {
        // Never expected: the codec refuses an empty grant. Running anyway
        // would hand the child CUDA_VISIBLE_DEVICES= and a baffling failure.
        return complain("the daemon granted no GPUs");
    }

    // Client validated the grant against the resolved CUDA view. Only the
    // granted UUIDs are exported; inherited restrictions are never widened.
    let visible = cuda_visible_devices::render(&granted.uuids);

    // From here the descriptor, not the client, is the reservation. It is
    // inheritable, so the child holds it too.
    let reservation = match client.into_reservation() {
        Ok(fd) => fd,
        Err(error) => {
            return complain(format!("cannot take ownership of the reservation: {error}"))
        }
    };
    let indices = granted
        .gpus
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut limit_note = String::new();
    if granted.hold_limit_ms > 0 {
        limit_note.push_str(&format!(
            "; hold limit {}",
            hold_limit::describe(granted.hold_limit_ms)
        ));
    }
    if granted.idle_limit_ms > 0 {
        limit_note.push_str(&format!(
            "; idle limit {}",
            hold_limit::describe(granted.idle_limit_ms)
        ));
    }
    // Before the command starts, so it cannot miss an idle stop; and only
    // with an idle limit, since without one the daemon sends nothing.
    let mut idle = if granted.idle_limit_ms > 0 {
        match idle_stop::watch(&reservation) {
            Ok(receiver) => Some(receiver),
            Err(error) => {
                return complain(format!(
                    "cannot watch the reservation for an idle stop: {error}"
                ))
            }
        }
    } else {
        None
    };
    note(format!("holding gpu(s) {indices} ({visible}){limit_note}"));
    let mut hold = HoldLimit::new(granted.hold_limit_ms);

    // Installed before the child starts, so no SIGTERM is missed. Only
    // SIGTERM is forwarded: a terminal already delivers SIGINT, SIGQUIT and
    // SIGHUP to the whole foreground process group, child included, and
    // forwarding them as well would deliver them twice. Those three are
    // still caught, only so that `gpudog` outlives them and returns the
    // command's own status after it has cleaned up. Exec resets caught
    // signals to the default action, so the command sees default
    // dispositions. Before this point (while waiting) they keep their
    // default action, which cancels the request.
    //
    // A signal already ignored when `gpudog` started (`nohup`, or `&` in a
    // non-interactive shell) is left ignored: catching it would replace
    // SIG_IGN, and the command would lose the protection on exec.
    let ignored = ignored_signals();
    let mut streams = Vec::new();
    for (kind, name) in [
        (SignalKind::terminate(), Signal::SIGTERM),
        (SignalKind::interrupt(), Signal::SIGINT),
        (SignalKind::quit(), Signal::SIGQUIT),
        (SignalKind::hangup(), Signal::SIGHUP),
    ] {
        if ignored & signal_bit(name) != 0 {
            streams.push(None);
            continue;
        }
        match signal(kind) {
            Ok(stream) => streams.push(Some(stream)),
            Err(error) => return complain(format!("cannot handle {}: {error}", name.as_str())),
        }
    }
    let [terminate, interrupt, quit, hangup] = &mut streams[..] else {
        unreachable!("four entries were pushed")
    };
    let mut child = match child_command(command, &visible).spawn() {
        Ok(child) => child,
        Err(error) => {
            // Shell conventions where they apply; anything else (a failed
            // fork, say) is a plain failure.
            let code = match error.kind() {
                std::io::ErrorKind::NotFound => EXIT_COMMAND_NOT_FOUND,
                std::io::ErrorKind::PermissionDenied => EXIT_COMMAND_NOT_EXECUTABLE,
                _ if error.raw_os_error() == Some(nix::errno::Errno::ENOEXEC as i32) => {
                    EXIT_COMMAND_NOT_EXECUTABLE
                }
                _ => return complain(format!("cannot start the requested command: {error}")),
            };
            note(format!("cannot run the requested command: {error}"));
            return ExitCode::from(code);
        }
    };
    let pid = child.id().map(|id| Pid::from_raw(id as i32));

    let status = loop {
        tokio::select! {
            // First, so a command that exits as its hold limit ends is not
            // reported as stopped.
            biased;
            status = child.wait() => break status,
            () = until(hold.next_deadline()) => hold.on_deadline(pid),
            stop = idle_stop_of(&mut idle) => {
                idle = None;
                if stop.is_some() {
                    hold.stop_idle(pid, granted.idle_limit_ms);
                }
            }
            _ = recv(terminate) => {
                // Passed on rather than dying first: the child owns the GPUs,
                // and exiting now would not release them anyway (the child
                // holds the descriptor), only orphan the child.
                if let Some(pid) = pid {
                    let _ = kill(pid, Signal::SIGTERM);
                }
            }
            // Already delivered to the command by the terminal; outlive it.
            _ = recv(interrupt) => {}
            _ = recv(quit) => {}
            _ = recv(hangup) => {}
        }
    };
    // A command stopped at its hold limit may leave processes it started;
    // wait for them, or kill them when the grace period ends.
    hold.finish().await;
    // The reservation ends once the child has exited too, and, with an idle
    // limit, once the process exits: the idle-stop reader holds a duplicate.
    drop(reservation);

    match status {
        Err(error) => complain(format!("cannot wait for the child: {error}")),
        Ok(status) => match (status.code(), status.signal()) {
            (Some(code), _) => ExitCode::from(code as u8),
            (None, Some(number)) => {
                report_signal(number, status.core_dumped());
                // Not if SIGINT was ignored on entry: the caller asked
                // for it not to end this process.
                if number == Signal::SIGINT as i32 && ignored & signal_bit(Signal::SIGINT) == 0 {
                    // Die of SIGINT too, not exit 130: bash only stops a
                    // script loop on Ctrl-C when the job died of SIGINT
                    // ("wait and cooperative exit"). signal-hook aborts if
                    // it cannot re-raise; it returns only for a signal
                    // missing from its table, which SIGINT is not.
                    let _ = signal_hook::low_level::emulate_default_handler(number);
                }
                ExitCode::from((EXIT_SIGNAL_BASE + number) as u8)
            }
            (None, None) => ExitCode::FAILURE,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_signals_are_read_from_the_sig_ign_mask() {
        let status = "Name:\tgpudog\nSigBlk:\t0000000000000000\nSigIgn:\t0000000000000007\n";
        let mask = parse_sig_ign(status).unwrap();
        assert_ne!(mask & signal_bit(Signal::SIGHUP), 0);
        assert_ne!(mask & signal_bit(Signal::SIGINT), 0);
        assert_ne!(mask & signal_bit(Signal::SIGQUIT), 0);
        assert_eq!(mask & signal_bit(Signal::SIGTERM), 0);
        assert_eq!(parse_sig_ign("Name:\tgpudog\n"), None);
    }

    #[test]
    fn the_child_environment_pins_the_cuda_variables() {
        let command = child_command(&[OsString::from("env")], "GPU-b,GPU-a");
        let envs: Vec<_> = command.as_std().get_envs().collect();
        let value = |key: &str| {
            envs.iter()
                .rev()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| *v)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        assert_eq!(
            value("CUDA_VISIBLE_DEVICES").as_deref(),
            Some("GPU-b,GPU-a")
        );
        assert_eq!(value("CUDA_DEVICE_ORDER").as_deref(), Some("PCI_BUS_ID"));
    }
}
