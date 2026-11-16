//! Runs the real `gpudog` binary against an in-process daemon.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpudog_daemon::gpu_inventory::StaticInventory;
mod support;
use gpudog_daemon::scheduler::Scheduler;
use gpudog_daemon::service::{discard_log, Service, ServiceConfig};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

fn scratch_socket() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/gpudog");
    std::fs::create_dir_all(&directory).unwrap();
    // Canonical, so `crates/x/../../` does not eat into the 108-byte
    // `sun_path` limit, which a worktree path already strains.
    let directory = directory.canonicalize().unwrap();
    directory.join(format!(
        "{}-{}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn start(gpus: u32) -> Service {
    let config = ServiceConfig {
        socket_path: scratch_socket(),
        require_secure_directory: false,
        ..ServiceConfig::default()
    };
    Service::start(
        Arc::new(StaticInventory::new(
            (0..gpus)
                .map(|index| gpudog_daemon::gpu_inventory::GpuDevice {
                    index,
                    nvml_index: index,
                    uuid: support::uuid(index),
                    name: "mock device".into(),
                })
                .collect(),
        )),
        None,
        config,
        discard_log(),
    )
    .unwrap()
}

fn gpudog(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gpudog"));
    command.args(args).kill_on_drop(true);
    command
        .env("LD_LIBRARY_PATH", support::driver_directory())
        .env_remove("CUDA_VISIBLE_DEVICES")
        .env("CUDA_DEVICE_ORDER", "PCI_BUS_ID");
    command
}

fn run_on(service: &Service, options: &[&str], child: &[&str]) -> Command {
    let socket = service.socket_path().to_str().unwrap().to_owned();
    let mut args = vec!["run", "-s", &socket];
    args.extend_from_slice(options);
    args.push("--");
    args.extend_from_slice(child);
    gpudog(&args)
}

struct Outcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn output(mut command: Command) -> Outcome {
    let output = command.output().await.unwrap();
    Outcome {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// Quotes `path` for an `sh -c` script, so a checkout path with spaces or
/// quotes still names one file.
fn sh_quote(path: impl AsRef<Path>) -> String {
    format!(
        "'{}'",
        path.as_ref().display().to_string().replace('\'', "'\\''")
    )
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    for _ in 0..600 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition never became true");
}

#[tokio::test]
async fn run_hands_the_child_its_gpus_through_the_cuda_variables() {
    let service = start(4);
    let mut command = run_on(
        &service,
        &["-g", "2,0"],
        &["sh", "-c", "echo $CUDA_VISIBLE_DEVICES $CUDA_DEVICE_ORDER"],
    );
    // A pre-set value must not widen the reservation.
    command
        .env("CUDA_VISIBLE_DEVICES", "0,1,2,3")
        .env("CUDA_DEVICE_ORDER", "FASTEST_FIRST");
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    // FASTEST_FIRST is reversed in the mock: local ordinals 2 and 0 name
    // physical UUIDs 1 and 3, not daemon indices 2 and 0.
    assert_eq!(
        outcome.stdout,
        format!("{},{} PCI_BUS_ID\n", support::uuid(1), support::uuid(3))
    );
    assert!(!outcome.stderr.contains("replacing CUDA_VISIBLE_DEVICES"));
    assert!(outcome.stderr.contains(&format!(
        "gpudog: holding gpu(s) 1,3 ({},{})\n",
        support::uuid(1),
        support::uuid(3)
    )));
}

#[tokio::test]
async fn run_selects_gpus_by_uuid_within_the_callers_uuid_list() {
    let service = start(4);
    let mut command = run_on(
        &service,
        &["-g", &support::uuid(3).to_ascii_uppercase()],
        &["sh", "-c", "echo $CUDA_VISIBLE_DEVICES"],
    );
    command.env(
        "CUDA_VISIBLE_DEVICES",
        format!("{},{}", support::uuid(1), support::uuid(3)),
    );
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, format!("{}\n", support::uuid(3)));
    assert!(outcome.stderr.contains(&format!(
        "gpudog: holding gpu(s) 3 ({})\n",
        support::uuid(3)
    )));
}

#[tokio::test]
async fn run_refuses_a_grant_outside_the_callers_uuid_list() {
    let service = start(2);
    let mut command = run_on(&service, &["-g", "1"], &["sh", "-c", "echo ran"]);
    command.env("CUDA_VISIBLE_DEVICES", support::uuid(0));
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(1));
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        "gpudog: CUDA number 1 is not visible (1 visible in this CUDA environment)\n"
    );
    // The connection closed with the process, so the reservation is gone.
    eventually(|| service.with_scheduler(|s| s.free_device_count()) == 2).await;
}

#[tokio::test]
async fn run_denies_a_uuid_the_daemon_does_not_manage() {
    let service = start(2);
    let outcome = output(run_on(&service, &["-g", &support::uuid(3)], &["true"])).await;
    assert_eq!(outcome.code, Some(1));
    assert!(
        outcome.stderr.contains("is not managed by this daemon"),
        "{}",
        outcome.stderr
    );
}

#[tokio::test]
async fn run_exits_with_the_childs_status() {
    let service = start(1);
    let outcome = output(run_on(&service, &[], &["sh", "-c", "exit 7"])).await;
    assert_eq!(outcome.code, Some(7));
    if runner_ignores(Signal::SIGTERM) {
        return; // sh would ignore its own SIGTERM and exit 0
    }
    // Death by signal N is 128 + N, as a shell reports it.
    let outcome = output(run_on(&service, &[], &["sh", "-c", "kill -TERM $$"])).await;
    assert_eq!(outcome.code, Some(128 + 15));
}

#[tokio::test]
async fn a_command_that_does_not_exist_exits_127() {
    let service = start(1);
    let outcome = output(run_on(&service, &[], &["definitely-not-a-command-xyz"])).await;
    assert_eq!(outcome.code, Some(127));
    assert!(outcome.stderr.contains("cannot run the requested command"));
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
}

#[tokio::test]
async fn the_reservation_lasts_exactly_as_long_as_the_child() {
    let service = start(1);
    let mut child = run_on(&service, &[], &["sleep", "0.4"]).spawn().unwrap();
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 0).await;
    assert!(child.wait().await.unwrap().success());
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
}

#[tokio::test]
async fn killing_gpudog_does_not_release_gpus_the_child_still_uses() {
    // The child inherits the reservation descriptor: releasing the GPU while
    // it computes would hand it to someone else mid-job.
    let service = start(1);
    let marker = scratch_socket().with_extension("done");
    let started = marker.with_extension("started");
    let script = format!(
        "touch {}; sleep 0.5; touch {}",
        sh_quote(&started),
        sh_quote(&marker)
    );
    let mut child = run_on(&service, &[], &["sh", "-c", &script])
        .spawn()
        .unwrap();
    // Wait for the command itself, not just the grant: killed between the
    // grant and the spawn, `gpudog` would rightly release the GPUs.
    eventually(|| started.exists()).await;
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 0);

    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGKILL).unwrap();
    child.wait().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 0);

    eventually(|| marker.exists()).await;
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
    std::fs::remove_file(&marker).unwrap();
    std::fs::remove_file(&started).unwrap();
}

#[tokio::test]
async fn sigterm_is_forwarded_to_the_child() {
    if runner_ignores(Signal::SIGTERM) {
        return;
    }
    let service = start(1);
    // sh touches the marker once its trap is installed, so the signal can
    // never arrive before it.
    let trapped = scratch_socket().with_extension("trapped");
    let script = format!(
        "trap 'exit 42' TERM; touch {}; sleep 5 & wait",
        sh_quote(&trapped)
    );
    let mut child = run_on(&service, &[], &["sh", "-c", &script])
        .spawn()
        .unwrap();
    eventually(|| trapped.exists()).await;
    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    assert_eq!(child.wait().await.unwrap().code(), Some(42));
    std::fs::remove_file(&trapped).unwrap();
}

/// True when this test process started with `signal` ignored, for example
/// `cargo test &` in a non-interactive shell. `gpudog` then rightly passes
/// SIG_IGN on, and sh cannot trap a signal ignored on entry, so tests that
/// rely on the default disposition skip rather than fail.
fn runner_ignores(signal: Signal) -> bool {
    let ignored = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("SigIgn:"))
                .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        })
        .unwrap_or(0);
    let skip = ignored & (1 << (signal as i32 - 1)) != 0;
    if skip {
        eprintln!("skipped: the test runner started with {signal} ignored");
    }
    skip
}

/// Sends `signal` to the whole process group led by `leader`, as a terminal
/// does to its foreground job on Ctrl-C, Ctrl-\ or hangup.
fn signal_group(leader: &tokio::process::Child, signal: Signal) {
    nix::sys::signal::killpg(Pid::from_raw(leader.id().unwrap() as i32), signal).unwrap();
}

#[tokio::test]
async fn terminal_signals_wait_for_the_command_and_return_its_status() {
    // The command traps the signal and takes a while to clean up; `gpudog`
    // must outlive the signal and report the command's own status. A
    // non-interactive sh cannot trap a signal ignored on entry, so this
    // also proves the child did not inherit SIG_IGN.
    for (signal, name) in [
        (Signal::SIGINT, "INT"),
        (Signal::SIGQUIT, "QUIT"),
        (Signal::SIGHUP, "HUP"),
    ] {
        if runner_ignores(signal) {
            continue;
        }
        let service = start(1);
        let ready = scratch_socket().with_extension("trapped");
        let script = format!(
            "trap 'kill $!; sleep 0.3; exit 5' {name}; touch {}; sleep 5 & wait",
            sh_quote(&ready)
        );
        let mut command = run_on(&service, &[], &["sh", "-c", &script]);
        command.process_group(0).stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        eventually(|| ready.exists()).await; // the trap is installed
        signal_group(&child, signal);
        let status = child.wait().await.unwrap();
        assert_eq!(status.code(), Some(5), "{name}: {status:?}");
        eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
        std::fs::remove_file(&ready).unwrap();
    }
}

#[tokio::test]
async fn signals_ignored_on_entry_stay_ignored_for_the_command() {
    // `nohup gpudog run ...` must still protect the command from SIGHUP:
    // catching an ignored signal would reset it to the default on exec.
    let service = start(1);
    let script = format!(
        "trap '' HUP INT QUIT; exec {} run -s {} -- sh -c 'grep SigIgn /proc/self/status'",
        sh_quote(env!("CARGO_BIN_EXE_gpudog")),
        sh_quote(service.socket_path())
    );
    // The environment `gpudog()` sets, but started through sh to ignore first.
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&script)
        .kill_on_drop(true)
        .env("LD_LIBRARY_PATH", support::driver_directory())
        .env_remove("CUDA_VISIBLE_DEVICES")
        .env("CUDA_DEVICE_ORDER", "PCI_BUS_ID");
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let mask = outcome
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))
        .map(|mask| u64::from_str_radix(mask.trim(), 16).unwrap())
        .expect("the command printed its SigIgn mask");
    // Bits 0, 1, 2: SIGHUP, SIGINT, SIGQUIT.
    assert_eq!(mask & 0x7, 0x7, "{}", outcome.stdout);
}

#[tokio::test]
async fn a_command_killed_by_sigint_ends_gpudog_by_sigint_quietly() {
    if runner_ignores(Signal::SIGINT) {
        return;
    }
    // bash stops a script loop on Ctrl-C only if the job died of SIGINT.
    use std::os::unix::process::ExitStatusExt;
    let service = start(1);
    let mut command = run_on(&service, &[], &["sh", "-c", "kill -INT $$"]);
    command.stderr(Stdio::piped());
    let output = command.output().await.unwrap();
    assert_eq!(output.status.signal(), Some(Signal::SIGINT as i32));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("killed by signal"), "{stderr}");
}

#[tokio::test]
async fn a_command_killed_by_a_signal_is_reported_except_sigpipe() {
    let service = start(1);
    let outcome = output(run_on(&service, &[], &["sh", "-c", "kill -KILL $$"])).await;
    assert_eq!(outcome.code, Some(128 + 9));
    assert!(
        outcome
            .stderr
            .contains("gpudog: the command was killed by signal 9 (SIGKILL)\n"),
        "{}",
        outcome.stderr
    );
    let outcome = output(run_on(&service, &[], &["sh", "-c", "kill -PIPE $$"])).await;
    assert_eq!(outcome.code, Some(128 + 13));
    assert!(
        !outcome.stderr.contains("killed by signal"),
        "{}",
        outcome.stderr
    );
}

#[tokio::test]
async fn ctrl_c_while_waiting_still_cancels_the_request() {
    if runner_ignores(Signal::SIGINT) {
        return;
    }
    use std::os::unix::process::ExitStatusExt;
    let service = start(1);
    // Long enough that the waiting request cannot be granted under load.
    let mut holder = run_on(&service, &[], &["sleep", "30"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 0).await;
    let marker = scratch_socket().with_extension("ran");
    let script = format!("touch {}", sh_quote(&marker));
    let mut command = run_on(&service, &[], &["sh", "-c", &script]);
    command.process_group(0).stderr(Stdio::piped());
    let mut waiting = command.spawn().unwrap();
    let mut lines = BufReader::new(waiting.stderr.take().unwrap()).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    assert!(line.contains("waiting for GPUs"), "{line}");
    signal_group(&waiting, Signal::SIGINT);
    let status = waiting.wait().await.unwrap();
    assert_eq!(status.signal(), Some(Signal::SIGINT as i32));
    kill(Pid::from_raw(holder.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    holder.wait().await.unwrap();
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
    assert!(!marker.exists(), "the command must never start");
}

#[tokio::test]
async fn a_queued_run_says_so_and_then_runs() {
    if runner_ignores(Signal::SIGTERM) {
        return; // the holder's sleep would inherit SIG_IGN and run its 30 s
    }
    let service = start(1);
    // The holder runs until the waiting line is read, so the second run
    // queues however slowly it starts.
    let mut holder = run_on(&service, &[], &["sleep", "30"]).spawn().unwrap();
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 0).await;
    let mut waiting = run_on(&service, &[], &["true"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(waiting.stderr.take().unwrap()).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    assert_eq!(line, "gpudog: waiting for GPUs (queue position 0)");
    kill(Pid::from_raw(holder.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    holder.wait().await.unwrap();
    assert_eq!(waiting.wait().await.unwrap().code(), Some(0));
}

#[tokio::test]
async fn a_queued_run_that_times_out_reports_the_daemon_denial() {
    if runner_ignores(Signal::SIGTERM) {
        return; // the holder's sleep would inherit SIG_IGN and run its 30 s
    }
    let service = start(1);
    let mut holder = run_on(&service, &[], &["sleep", "30"]).spawn().unwrap();
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 0).await;
    let outcome = output(run_on(
        &service,
        &["-t", "300ms"],
        &["sh", "-c", "echo ran"],
    ))
    .await;
    kill(Pid::from_raw(holder.id().unwrap() as i32), Signal::SIGTERM).unwrap();
    holder.wait().await.unwrap();
    assert_eq!(outcome.code, Some(1));
    assert!(outcome.stdout.is_empty(), "the command must not run");
    assert_eq!(outcome.stderr,
        "gpudog: waiting for GPUs (queue position 0)\ngpudog: timed out waiting for GPUs: no GPU became available before the deadline\n");
}

#[tokio::test]
async fn a_denial_is_reported_and_fails() {
    let service = start(2);
    let outcome = output(run_on(&service, &["-n", "3"], &["true"])).await;
    assert_eq!(outcome.code, Some(1));
    assert_eq!(
        outcome.stderr,
        "gpudog: request can never be satisfied: requested 3 GPUs but this host has only 2 GPUs\n"
    );
}

#[tokio::test]
async fn a_bad_option_is_refused_before_connecting() {
    // No daemon at this path: the option error must come first.
    let socket = scratch_socket();
    let outcome = output(gpudog(&[
        "run",
        "-s",
        socket.to_str().unwrap(),
        "-t",
        "0",
        "--",
        "true",
    ]))
    .await;
    assert_eq!(outcome.code, Some(1));
    assert_eq!(
        outcome.stderr,
        "gpudog: --timeout must be greater than zero; omit --timeout to wait indefinitely\n"
    );
}

#[tokio::test]
async fn health_reports_a_responding_daemon() {
    let service = start(1);
    let socket = service.socket_path().to_str().unwrap().to_owned();
    let outcome = output(gpudog(&["health", "-s", &socket])).await;
    assert_eq!(outcome.code, Some(0));
    assert_eq!(
        outcome.stdout,
        format!("gpudogd is responding on {socket}\n")
    );
}

#[tokio::test]
async fn a_missing_daemon_is_explained() {
    let socket = scratch_socket();
    let outcome = output(gpudog(&["health", "-s", socket.to_str().unwrap()])).await;
    assert_eq!(outcome.code, Some(1));
    assert_eq!(
        outcome.stderr,
        format!(
            "gpudog: cannot reach gpudogd at {}: No such file or directory\ngpudog: is gpudogd running?\n",
            socket.display()
        )
    );
}

#[tokio::test]
async fn dash_draws_the_machine_and_keeps_polling() {
    let service = start(2);
    let _holder = run_on(&service, &[], &["sleep", "5"]).spawn().unwrap();
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;

    let socket = service.socket_path().to_str().unwrap().to_owned();
    let mut dash = gpudog(&["dash", "-s", &socket, "-r", "1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(dash.stdout.take().unwrap()).lines();
    let mut seen = Vec::new();
    let mut frames = 0;
    while let Ok(Ok(Some(line))) =
        tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await
    {
        if line.starts_with("GPUDog:") {
            frames += 1;
            if frames == 2 {
                break;
            }
        }
        seen.push(line);
    }
    assert_eq!(frames, 2, "dash stopped polling:\n{}", seen.join("\n"));
    let text = seen.join("\n");
    assert!(text.contains("GPUDog: 2 GPUs, 1 free, 0 waiting"), "{text}");
    assert!(text.contains("sleep 5"), "{text}");
    // Not a terminal, so no screen-clearing escape codes.
    assert!(!text.contains('\x1b'));
}

#[tokio::test]
async fn reservation_only_dash_hides_daemon_process_rows() {
    let service = start(2);
    let socket = service.socket_path().to_str().unwrap();
    let mut process = gpudog(&["dash", "--no-gpu-processes", "-s", socket, "-r", "1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(process.stdout.take().unwrap()).lines();
    let found = tokio::time::timeout(Duration::from_secs(3), async {
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("GPU process rows hidden (--no-gpu-processes)") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    process.kill().await.unwrap();
    process.wait().await.unwrap();
    assert!(found, "dashboard did not disable process inspection");
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 2);
}

#[tokio::test]
async fn a_file_that_is_not_executable_exits_126() {
    let service = start(1);
    let script = scratch_socket().with_extension("sh");
    std::fs::write(&script, "#!/bin/sh\ntrue\n").unwrap();
    let outcome = output(run_on(&service, &[], &[script.to_str().unwrap()])).await;
    assert_eq!(outcome.code, Some(126), "{}", outcome.stderr);
    std::fs::remove_file(&script).unwrap();
}

#[tokio::test]
async fn arguments_that_are_not_utf8_reach_the_child_unchanged() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let service = start(1);
    let socket = service.socket_path().to_str().unwrap().to_owned();
    let raw = OsStr::from_bytes(b"caf\xe9");
    let mut command = gpudog(&["run", "-s", &socket, "--", "printf", "%s"]);
    command.arg(raw);
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"caf\xe9");
}

#[tokio::test]
async fn run_narrows_a_count_request_to_the_callers_uuid_list() {
    let service = start(4);
    let mut command = run_on(
        &service,
        &["-n", "2"],
        &["sh", "-c", "echo $CUDA_VISIBLE_DEVICES"],
    );
    command.env(
        "CUDA_VISIBLE_DEVICES",
        format!(
            "{},{},{}",
            support::uuid(3),
            support::uuid(1),
            support::uuid(3)
        ),
    );
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        format!("{},{}\n", support::uuid(1), support::uuid(3))
    );

    let mut command = run_on(&service, &["-n", "2"], &["true"]);
    command.env("CUDA_VISIBLE_DEVICES", support::uuid(3));
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(1));
    assert!(
        outcome
            .stderr
            .contains("requested 2 GPUs but only 1 visible in this CUDA environment"),
        "{}",
        outcome.stderr
    );
}

#[tokio::test]
async fn an_invalid_inherited_list_never_becomes_unrestricted() {
    let service = start(2);
    let mut inherited: Vec<String> = (0..1099).map(|i| format!("GPU-other-{i}")).collect();
    inherited.push(support::uuid(1));
    let mut command = run_on(
        &service,
        &["-g", "1"],
        &["sh", "-c", "echo $CUDA_VISIBLE_DEVICES"],
    );
    command.env("CUDA_VISIBLE_DEVICES", inherited.join(","));
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");

    // CUDA stops at the first invalid entry. No fallback to all GPUs.
    let mut command = run_on(&service, &[], &["true"]);
    command.env("CUDA_VISIBLE_DEVICES", inherited.join(","));
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(1));
    assert!(
        outcome
            .stderr
            .contains("no GPUs are visible in this CUDA environment"),
        "{}",
        outcome.stderr
    );
}

#[tokio::test]
async fn numeric_visibility_reorders_local_indices_and_narrows_count() {
    let service = start(4);
    for options in [vec!["-g", "0"], vec!["-n", "1"]] {
        let mut command = run_on(
            &service,
            &options,
            &["sh", "-c", "echo $CUDA_VISIBLE_DEVICES"],
        );
        command.env("CUDA_VISIBLE_DEVICES", "2,3");
        let outcome = output(command).await;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, format!("{}\n", support::uuid(2)));
    }
}

#[tokio::test]
async fn empty_negative_and_out_of_range_visibility_never_runs_a_child() {
    let service = start(4);
    for inherited in ["", "-1", "99", "99,0"] {
        let mut command = run_on(&service, &[], &["echo", "must-not-run"]);
        command.env("CUDA_VISIBLE_DEVICES", inherited);
        let outcome = output(command).await;
        assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
        assert!(outcome.stdout.is_empty());
        assert!(outcome
            .stderr
            .contains("no GPUs are visible in this CUDA environment"));
        assert_eq!(service.with_scheduler(Scheduler::free_device_count), 4);
    }
}

#[tokio::test]
async fn a_uuid_outside_numeric_visibility_is_rejected_before_acquisition() {
    let service = start(4);
    let mut command = run_on(
        &service,
        &["-g", &support::uuid(0)],
        &["echo", "must-not-run"],
    );
    command.env("CUDA_VISIBLE_DEVICES", "1");
    let outcome = output(command).await;
    assert_eq!(outcome.code, Some(1));
    assert!(outcome.stdout.is_empty());
    assert!(outcome
        .stderr
        .contains("not visible in this CUDA environment"));
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 4);
}

#[tokio::test]
async fn mig_detection_and_failed_mig_observation_fail_closed() {
    let service = start(4);
    for flag in [
        "MOCK_MIG_ENABLED",
        "MOCK_MIG_FAILURE",
        "MOCK_MIG_HANDLE",
        "MOCK_MIG_HANDLE_ERROR",
    ] {
        let mut command = run_on(&service, &[], &["echo", "must-not-run"]);
        command.env(flag, "1");
        let outcome = output(command).await;
        assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.contains("MIG"));
        assert_eq!(service.with_scheduler(Scheduler::free_device_count), 4);
    }
    let mut command = run_on(&service, &[], &["true"]);
    command.env("CUDA_VISIBLE_DEVICES", "MIG-abc");
    assert!(output(command).await.stderr.contains("unsupported MIG"));
}

#[tokio::test]
async fn cuda_failures_never_fall_back_to_nvml_devices() {
    let service = start(4);
    for flag in [
        "MOCK_CUDA_INIT_ERROR",
        "MOCK_CUDA_COUNT_ERROR",
        "MOCK_CUDA_NO_DEVICE",
    ] {
        let mut command = run_on(&service, &[], &["echo", "must-not-run"]);
        command.env(flag, "1");
        let result = output(command).await;
        assert_eq!(result.code, Some(1), "{}", result.stderr);
        assert!(result.stdout.is_empty());
        assert_eq!(service.with_scheduler(Scheduler::free_device_count), 4);
    }
}

#[tokio::test]
async fn explicitly_unsupported_mig_queries_do_not_hide_non_mig_devices() {
    let service = start(4);
    let mut command = run_on(&service, &[], &["echo", "ran"]);
    command.env("MOCK_MIG_UNSUPPORTED", "1");
    let result = output(command).await;
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(result.stdout, "ran\n");
}

#[tokio::test]
async fn missing_cuda_symbols_do_not_panic_out_of_visibility_discovery() {
    let service = start(4);
    let mut command = run_on(&service, &[], &["echo", "must-not-run"]);
    command.env("LD_LIBRARY_PATH", support::missing_uuid_directory());
    let result = output(command).await;
    assert_eq!(result.code, Some(1), "{}", result.stderr);
    assert!(result.stdout.is_empty());
    assert!(result
        .stderr
        .contains("CUDA driver unavailable or missing required symbols"));
    assert!(!result.stderr.contains("panicked"), "{}", result.stderr);
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 4);
}

#[tokio::test]
async fn inconsistent_nvml_identity_is_not_used_as_a_fallback() {
    let service = start(4);
    let mut command = run_on(&service, &[], &["echo", "must-not-run"]);
    command.env("MOCK_UUID_MISMATCH", "1");
    let result = output(command).await;
    assert_eq!(result.code, Some(1), "{}", result.stderr);
    assert!(result.stdout.is_empty());
    assert!(result
        .stderr
        .contains("NVML UUID disagrees with CUDA device"));
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 4);
}

/// A one-GPU daemon whose grants carry a hold limit of `max_hold_ms`.
fn start_holding_for(max_hold_ms: u64) -> Service {
    let config = ServiceConfig {
        socket_path: scratch_socket(),
        require_secure_directory: false,
        max_hold_ms,
        ..ServiceConfig::default()
    };
    Service::start(
        Arc::new(StaticInventory::new(vec![
            gpudog_daemon::gpu_inventory::GpuDevice {
                index: 0,
                nvml_index: 0,
                uuid: support::uuid(0),
                name: "mock device".into(),
            },
        ])),
        None,
        config,
        discard_log(),
    )
    .unwrap()
}

fn is_running(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| !rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

#[tokio::test]
async fn the_hold_limit_stops_the_command_and_frees_its_gpus() {
    if runner_ignores(Signal::SIGTERM) {
        return;
    }
    let service = start_holding_for(300);
    let started = std::time::Instant::now();
    let result = output(run_on(&service, &[], &["sleep", "30"])).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(result.code, Some(143), "{}", result.stderr);
    assert!(
        result.stderr.starts_with(&format!(
            "gpudog: holding gpu(s) 0 ({}); hold limit 300ms\n",
            support::uuid(0)
        )),
        "{}",
        result.stderr
    );
    assert!(result.stderr.contains(
        "gpudog: the hold limit of 300ms has ended; stopping the command (SIGKILL in 30s)\n"
    ));
    assert!(!result.stderr.contains("sent SIGKILL"), "{}", result.stderr);
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
}

#[tokio::test]
async fn the_hold_limit_also_stops_what_a_wrapper_started() {
    if runner_ignores(Signal::SIGTERM) {
        return;
    }
    // The shell dies of SIGTERM at once; without stopping its child too,
    // `sleep` would keep the reservation descriptor, and the GPU, for 30 s.
    let service = start_holding_for(300);
    let pid_file = scratch_socket().with_extension("pid");
    let script = format!("sleep 30 & echo $! > {}; wait", sh_quote(&pid_file));
    let result = output(run_on(&service, &[], &["sh", "-c", &script])).await;
    assert_eq!(result.code, Some(143), "{}", result.stderr);
    let sleeper: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    eventually(|| !is_running(sleeper)).await;
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
    std::fs::remove_file(&pid_file).unwrap();
}

#[tokio::test]
async fn a_command_that_ignores_sigterm_is_killed_after_the_grace_period() {
    let service = start_holding_for(200);
    let started = scratch_socket().with_extension("ignoring");
    // Both the shell and `sleep` ignore SIGTERM; `sleep` inherits SIG_IGN.
    let script = format!("trap '' TERM; touch {}; sleep 30", sh_quote(&started));
    let mut command = run_on(&service, &[], &["sh", "-c", &script]);
    command.env("GPUDOG_TEST_HOLD_GRACE_MS", "300");
    let result = output(command).await;
    assert_eq!(result.code, Some(137), "{}", result.stderr);
    assert!(
        result.stderr.contains("(SIGKILL in 300ms)"),
        "{}",
        result.stderr
    );
    assert!(
        result
            .stderr
            .contains("gpudog: the command did not stop after SIGTERM; sent SIGKILL\n"),
        "{}",
        result.stderr
    );
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
    std::fs::remove_file(&started).unwrap();
}

#[tokio::test]
async fn a_longer_grace_than_the_default_is_not_honoured() {
    let service = start_holding_for(100);
    let mut command = run_on(&service, &[], &["sleep", "30"]);
    command.env("GPUDOG_TEST_HOLD_GRACE_MS", "3600000");
    let result = output(command).await;
    assert!(
        result.stderr.contains("(SIGKILL in 30s)"),
        "{}",
        result.stderr
    );
}

#[tokio::test]
async fn a_command_that_ends_before_its_hold_limit_is_left_alone() {
    let service = start_holding_for(60_000);
    let result = output(run_on(&service, &[], &["true"])).await;
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(
        result.stderr,
        format!(
            "gpudog: holding gpu(s) 0 ({}); hold limit 1m\n",
            support::uuid(0)
        )
    );
}
