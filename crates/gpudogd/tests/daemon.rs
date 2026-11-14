//! Runs the real `gpudogd` binary: start, serve, log, and shut down on a
//! signal.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use gpudog_lib::protocol::codec::FrameCodec;
use gpudog_lib::protocol::{Acquire, Granted, Hello, HelloAck, Message};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio_util::codec::Framed;

fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/daemon");
    std::fs::create_dir_all(&directory).unwrap();
    // Canonical, so `crates/x/../../` does not eat into the 108-byte
    // `sun_path` limit, which a worktree path already strains.
    let directory = directory.canonicalize().unwrap();
    directory.join(format!(
        "{}-{}-{name}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A private config file, so a real `/etc/gpudog/config.toml` on the test
/// host cannot leak in.
fn config_file(text: &str) -> PathBuf {
    let path = scratch("config.toml");
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// A running `gpudogd` that is killed if the test fails before it exits, so
/// a failed assertion does not leave a daemon behind.
struct Daemon(Child);

impl std::ops::Deref for Daemon {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Daemon {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn(args: &[&str], journal: bool) -> Daemon {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gpudogd"));
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if journal {
        command.env("JOURNAL_STREAM", "8:12345");
    } else {
        command.env_remove("JOURNAL_STREAM");
    }
    Daemon(command.spawn().unwrap())
}

fn wait_for(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon never bound {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for exit and returns (success, stderr).
fn finish(mut child: Daemon) -> (bool, String) {
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status.success(), stderr)
}

#[tokio::test]
async fn serves_a_reservation_and_shuts_down_cleanly_on_sigterm() {
    let socket = scratch("d.sock");
    let config = config_file("max_held_per_uid = 5\n");
    let daemon = spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-s",
            socket.to_str().unwrap(),
            "-n",
            "2",
            "--allow-insecure-socket-directory",
        ],
        true,
    );
    wait_for(&socket);

    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let mut line = Framed::new(stream, FrameCodec);
    line.send(Message::from(Hello::default())).await.unwrap();
    assert_eq!(
        line.next().await.unwrap().unwrap(),
        Message::HelloAck(HelloAck::default())
    );
    let request = Acquire {
        num_gpus: 2,
        command: b"train\n<3>forged".to_vec(),
        candidates: vec!["GPU-static-0".into(), "GPU-static-1".into()],
        ..Acquire::default()
    };
    line.send(Message::from(request)).await.unwrap();
    assert_eq!(
        line.next().await.unwrap().unwrap(),
        Message::Granted(Granted {
            gpus: vec![0, 1],
            uuids: vec!["GPU-static-0".into(), "GPU-static-1".into()]
        })
    );

    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGTERM).unwrap();
    let (success, stderr) = finish(daemon);
    assert!(success, "{stderr}");
    assert!(!socket.exists(), "socket left behind");
    // Journal format: one priority-prefixed line per event.
    for expected in [
        format!(
            "<6>gpudogd {} (protocol v6) starting",
            env!("CARGO_PKG_VERSION")
        ),
        format!("<6>loaded configuration from {}", config.display()),
        "<4>NVML bypassed; assuming 2 GPU(s)".to_owned(),
        "<6>gpu 0 static device 0 (GPU-static-0, nvml index 0)".to_owned(),
        format!("<6>listening on {} with 2 GPU(s)", socket.display()),
        "acquire num_gpus=2 candidates=[GPU-static-0,GPU-static-1] command=train?<3>forged"
            .to_owned(),
        "granted gpus=[0:GPU-static-0,1:GPU-static-1]".to_owned(),
        "<6>SIGTERM received; shutting down".to_owned(),
        "<6>gpudogd stopped".to_owned(),
    ] {
        assert!(
            stderr.contains(&expected),
            "missing {expected:?} in:\n{stderr}"
        );
    }
    assert!(stderr.lines().all(|l| l.starts_with('<')), "{stderr}");
    let starting = format!(
        "<6>gpudogd {} (protocol v6) starting",
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        stderr.lines().next(),
        Some(starting.as_str()),
        "the version is not the first line:\n{stderr}"
    );
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn a_bad_setting_is_fatal_and_says_why() {
    let config = config_file("max_connections_per_uid = 0\n");
    let daemon = spawn(&["-c", config.to_str().unwrap(), "-n", "1"], false);
    let (success, stderr) = finish(daemon);
    assert!(!success);
    assert_eq!(
        stderr.lines().last(),
        Some("gpudogd: error: max_connections_per_uid must be at least 1")
    );
    std::fs::remove_file(&config).unwrap();
}

#[tokio::test]
async fn devices_restricts_and_renumbers_the_static_inventory() {
    let socket = scratch("dev.sock");
    let config = config_file("devices = [\"GPU-static-3\", \"gpu-STATIC-1\"]\n");
    let daemon = spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-s",
            socket.to_str().unwrap(),
            "-n",
            "4",
            "--allow-insecure-socket-directory",
        ],
        true,
    );
    wait_for(&socket);
    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGTERM).unwrap();
    let (success, stderr) = finish(daemon);
    assert!(success, "{stderr}");
    for expected in [
        "<6>gpu 0 static device 1 (GPU-static-1, nvml index 1)",
        "<6>gpu 1 static device 3 (GPU-static-3, nvml index 3)",
        "with 2 GPU(s)",
    ] {
        assert!(
            stderr.contains(expected),
            "missing {expected:?} in:\n{stderr}"
        );
    }
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn a_configured_device_that_is_missing_is_fatal() {
    let config = config_file("devices = [\"GPU-static-0\", \"GPU-static-9\"]\n");
    let (success, stderr) = finish(spawn(&["-c", config.to_str().unwrap(), "-n", "2"], false));
    assert!(!success);
    assert_eq!(
        stderr.lines().last(),
        Some("gpudogd: error: cannot select GPUs: configured device GPU-static-9 was not found")
    );
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn a_malformed_devices_list_is_fatal() {
    let config = config_file("devices = []\n");
    let (success, stderr) = finish(spawn(&["-c", config.to_str().unwrap(), "-n", "2"], false));
    assert!(!success);
    assert!(
        stderr.contains("devices must list at least one GPU; omit it to manage every GPU"),
        "{stderr}"
    );
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn a_missing_explicit_config_file_is_fatal() {
    let missing = scratch("absent.toml");
    let (success, stderr) = finish(spawn(&["-c", missing.to_str().unwrap(), "-n", "1"], false));
    assert!(!success);
    assert!(stderr.contains("no such configuration file"), "{stderr}");
}

#[test]
fn an_unsafe_config_file_is_refused() {
    let config = config_file("");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o666)).unwrap();
    let (success, stderr) = finish(spawn(&["-c", config.to_str().unwrap(), "-n", "1"], false));
    assert!(!success);
    assert!(stderr.contains("writable by group or others"), "{stderr}");
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn obsolete_log_directory_settings_warn_but_start() {
    let socket = scratch("o.sock");
    let config = config_file("log_directory = \"/var/log/gpudog\"\n");
    let daemon = spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-l",
            "/var/log/gpudog",
            "-s",
            socket.to_str().unwrap(),
            "-n",
            "1",
            "--allow-insecure-socket-directory",
        ],
        false,
    );
    wait_for(&socket);
    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGINT).unwrap();
    let (success, stderr) = finish(daemon);
    assert!(success, "{stderr}");
    assert!(
        stderr.contains("gpudogd: warning: ignoring --log-directory: gpudogd now logs to stderr")
    );
    assert!(stderr.contains("ignoring 'log_directory': gpudogd now logs to stderr"));
    assert!(stderr.contains("gpudogd: access: SIGINT received; shutting down"));
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn a_missing_default_config_file_warns_but_starts() {
    // Only meaningful where nothing is installed at the default path.
    if Path::new(gpudog_lib::config::DEFAULT_CONFIG_PATH).exists() {
        eprintln!(
            "skipped: {} exists",
            gpudog_lib::config::DEFAULT_CONFIG_PATH
        );
        return;
    }
    let socket = scratch("m.sock");
    let daemon = spawn(
        &[
            "-s",
            socket.to_str().unwrap(),
            "-n",
            "1",
            "--allow-insecure-socket-directory",
        ],
        false,
    );
    wait_for(&socket);
    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGINT).unwrap();
    let (success, stderr) = finish(daemon);
    assert!(success, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "gpudogd: warning: no configuration file at {}; using the defaults",
            gpudog_lib::config::DEFAULT_CONFIG_PATH
        )),
        "{stderr}"
    );
}

#[test]
fn the_version_names_the_protocol() {
    let output = Command::new(env!("CARGO_BIN_EXE_gpudogd"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("gpudogd {} (protocol v6)\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn command_line_flags_override_the_config_file() {
    // The file names one socket and a connection cap of 1; the flags must
    // win on the socket, and the file must still apply what flags omit.
    let file_socket = scratch("file.sock");
    let flag_socket = scratch("flag.sock");
    let config = config_file(&format!(
        "socket_path = {:?}\nmax_connections = 1\nrequire_secure_directory = false\n",
        file_socket.to_str().unwrap()
    ));
    let daemon = spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-s",
            flag_socket.to_str().unwrap(),
            "-n",
            "1",
        ],
        false,
    );
    wait_for(&flag_socket);
    assert!(!file_socket.exists());

    // max_connections = 1 from the file: a second connection is refused.
    let _first = std::os::unix::net::UnixStream::connect(&flag_socket).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut second = std::os::unix::net::UnixStream::connect(&flag_socket).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reply = Vec::new();
    second.read_to_end(&mut reply).unwrap();
    let Message::Denied(denied) = gpudog_lib::protocol::decode_frame(&reply).unwrap() else {
        panic!("expected a denial");
    };
    assert_eq!(denied.detail, b"connection limit reached");

    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGTERM).unwrap();
    let (success, stderr) = finish(daemon);
    assert!(success, "{stderr}");
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn a_flag_cannot_make_an_invalid_combination_valid_or_vice_versa() {
    // Validation runs after the merge: the file's handshake_timeout_ms = 0
    // is fatal no matter which flags accompany it.
    let config = config_file("handshake_timeout_ms = 0\n");
    let (success, stderr) = finish(spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-n",
            "1",
            "--max-connections",
            "5",
        ],
        false,
    ));
    assert!(!success);
    assert!(
        stderr.contains("handshake_timeout_ms must be at least 1"),
        "{stderr}"
    );
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn a_startup_failure_is_reported_with_its_cause() {
    // Make the rejection independent of the runner's UID and umask. A safe
    // root-owned tmp/ would start the daemon and make finish wait forever.
    let directory = scratch("insecure");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
    let socket = directory.join("d.sock");
    let config = config_file("");
    let (success, stderr) = finish(spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-s",
            socket.to_str().unwrap(),
            "-n",
            "1",
        ],
        false,
    ));
    assert!(!success);
    assert!(
        stderr.contains("gpudogd: error: startup failed: refusing to bind: "),
        "{stderr}"
    );
    std::fs::remove_file(&config).unwrap();
    std::fs::remove_dir(&directory).unwrap();
}

/// Collects a running daemon's stderr lines on a thread, so a test can wait
/// for a line while the daemon keeps running.
struct LogLines {
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl LogLines {
    fn of(child: &mut Child) -> Self {
        let stderr = child.stderr.take().unwrap();
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            lines,
            seen: Vec::new(),
        }
    }

    /// Waits for a line containing `text` that has not been matched before,
    /// and returns it.
    fn wait_for(&mut self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(text) {
                        return line;
                    }
                }
                Err(_) => panic!("no line containing {text:?} in:\n{}", self.seen.join("\n")),
            }
        }
    }
}

/// Replaces the config file's contents, keeping its safe mode.
fn rewrite(config: &Path, text: &str) {
    std::fs::write(config, text).unwrap();
}

fn hang_up(daemon: &Child) {
    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGHUP).unwrap();
}

fn denial(message: &Message) -> String {
    match message {
        Message::Denied(denied) => String::from_utf8_lossy(&denied.detail).into_owned(),
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[tokio::test]
async fn sighup_reloads_the_limits_and_keeps_serving() {
    let socket = scratch("hup.sock");
    let config = config_file("max_held_per_uid = 5\n");
    let mut daemon = spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-s",
            socket.to_str().unwrap(),
            "-n",
            "2",
            "--max-connections",
            "4",
            "--allow-insecure-socket-directory",
        ],
        false,
    );
    let mut log = LogLines::of(&mut daemon);
    log.wait_for("listening on");

    // The flag keeps winning over the file, and the socket mode needs a
    // restart; only max_held_per_uid changes.
    rewrite(
        &config,
        "max_held_per_uid = 1\nmax_connections = 1\nsocket_mode = \"0600\"\n",
    );
    hang_up(&daemon);
    log.wait_for("SIGHUP received; reloading the configuration");
    log.wait_for("socket_mode changed; restart gpudogd to apply it");
    assert_eq!(
        log.wait_for("configuration reloaded"),
        "gpudogd: access: configuration reloaded: max_held_per_uid 5 -> 1"
    );
    assert_eq!(
        denial(&acquire_count(&socket, 2).await.0),
        "requested 2 GPUs but the per-user cap is 1 GPU"
    );
    assert!(matches!(
        acquire_count(&socket, 1).await.0,
        Message::Granted(_)
    ));
    // Still 0666: the reload did not touch the socket.
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o666
    );

    // Each unusable file is reported and changes nothing.
    for (text, reason) in [
        (
            "max_held_per_uid = \"high\"\n",
            "a setting has an invalid value",
        ),
        ("max_held_per_uid = = 4\n", "line 1"),
        (
            "max_connections_per_uid = 0\n",
            "max_connections_per_uid must be at least 1",
        ),
        ("devices = []\n", "devices must list at least one GPU"),
    ] {
        rewrite(&config, text);
        hang_up(&daemon);
        let line = log.wait_for("configuration reload failed");
        assert!(
            line.contains("keeping the previous configuration") && line.contains(reason),
            "{line}"
        );
    }
    std::fs::remove_file(&config).unwrap();
    hang_up(&daemon);
    assert!(log
        .wait_for("configuration reload failed")
        .contains("no such configuration file"));
    assert_eq!(
        denial(&acquire_count(&socket, 2).await.0),
        "requested 2 GPUs but the per-user cap is 1 GPU"
    );

    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGTERM).unwrap();
    log.wait_for("gpudogd stopped");
    assert!(daemon.wait().unwrap().success());
    assert!(!socket.exists(), "socket left behind");
}

#[test]
fn a_sighup_while_starting_does_not_end_the_daemon() {
    let socket = scratch("early.sock");
    let config = config_file("max_held_per_uid = 5\n");
    let mut daemon = spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-s",
            socket.to_str().unwrap(),
            "-n",
            "1",
            "--allow-insecure-socket-directory",
        ],
        false,
    );
    let mut log = LogLines::of(&mut daemon);
    // The first log line comes after the handlers are installed; before
    // that, SIGHUP still has its default action.
    log.wait_for("loaded configuration");
    while !socket.exists() {
        hang_up(&daemon);
        std::thread::sleep(Duration::from_millis(1));
        assert!(daemon.try_wait().unwrap().is_none(), "daemon ended");
    }
    hang_up(&daemon);
    log.wait_for("configuration reloaded");
    assert!(daemon.try_wait().unwrap().is_none(), "daemon ended");
    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGTERM).unwrap();
    log.wait_for("gpudogd stopped");
    assert!(daemon.wait().unwrap().success());
    std::fs::remove_file(&config).unwrap();
}

/// Asks for `count` GPUs among the static ones, and returns the daemon's
/// answer with the connection, which holds any grant until dropped.
async fn acquire_count(
    socket: &Path,
    count: u32,
) -> (Message, Framed<tokio::net::UnixStream, FrameCodec>) {
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let mut line = Framed::new(stream, FrameCodec);
    line.send(Message::from(Hello::default())).await.unwrap();
    line.next().await.unwrap().unwrap();
    line.send(Message::from(Acquire {
        num_gpus: count,
        candidates: (0..4).map(|i| format!("GPU-static-{i}")).collect(),
        ..Acquire::default()
    }))
    .await
    .unwrap();
    (line.next().await.unwrap().unwrap(), line)
}

#[tokio::test]
async fn sighup_applies_a_changed_devices_list() {
    let socket = scratch("devices.sock");
    let config = config_file("devices = [\"GPU-static-0\", \"GPU-static-1\"]\n");
    let mut daemon = spawn(
        &[
            "-c",
            config.to_str().unwrap(),
            "-s",
            socket.to_str().unwrap(),
            "-n",
            "3",
            "--allow-insecure-socket-directory",
        ],
        false,
    );
    let mut log = LogLines::of(&mut daemon);
    log.wait_for("listening on");
    let (reply, holder) = acquire_count(&socket, 1).await;
    assert_eq!(
        reply,
        Message::Granted(Granted {
            gpus: vec![0],
            uuids: vec!["GPU-static-0".into()]
        })
    );

    rewrite(&config, "devices = [\"GPU-static-1\", \"GPU-static-2\"]\n");
    hang_up(&daemon);
    log.wait_for("now managing GPU-static-2");
    log.wait_for(
        "no longer managing GPU-static-0 for new requests; its current reservation continues",
    );
    for expected in [
        "gpu 0 static device 0 (GPU-static-0, nvml index 0; removed, reserved until its job ends)",
        "gpu 1 static device 1 (GPU-static-1, nvml index 1)",
        "gpu 2 static device 2 (GPU-static-2, nvml index 2)",
    ] {
        log.wait_for(expected);
    }
    assert_eq!(
        log.wait_for("configuration reloaded"),
        "gpudogd: access: configuration reloaded: devices"
    );
    let (reply, _second) = acquire_count(&socket, 2).await;
    assert_eq!(
        reply,
        Message::Granted(Granted {
            gpus: vec![1, 2],
            uuids: vec!["GPU-static-1".into(), "GPU-static-2".into()]
        })
    );

    // A GPU the daemon did not see at startup cannot be added.
    rewrite(&config, "devices = [\"GPU-static-1\", \"GPU-static-7\"]\n");
    hang_up(&daemon);
    assert_eq!(
        log.wait_for("configuration reload failed"),
        "gpudogd: error: configuration reload failed; keeping the previous configuration: \
         configured device GPU-static-7 was not found when gpudogd started; \
         restart gpudogd to use a GPU added since it started"
    );

    // Released, the removed GPU leaves the numbering at the next reload.
    drop(holder);
    log.wait_for("released its reservation");
    rewrite(&config, "devices = [\"GPU-static-1\", \"GPU-static-2\"]\n");
    hang_up(&daemon);
    log.wait_for("gpu 0 static device 1 (GPU-static-1, nvml index 1)");
    log.wait_for("gpu 1 static device 2 (GPU-static-2, nvml index 2)");
    assert_eq!(
        log.wait_for("configuration reloaded"),
        "gpudogd: access: configuration reloaded: daemon GPU numbers"
    );

    kill(Pid::from_raw(daemon.id() as i32), Signal::SIGTERM).unwrap();
    log.wait_for("gpudogd stopped");
    assert!(daemon.wait().unwrap().success());
    std::fs::remove_file(&config).unwrap();
}
