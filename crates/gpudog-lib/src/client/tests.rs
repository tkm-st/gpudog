//! Client tests, run against a real in-process daemon.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::cli_options::{build_acquire, RunOptions};
use crate::gpu_inventory::StaticInventory;
use crate::protocol::{codec::FrameCodec, Denied, MAX_COMMAND_SIZE};
use crate::scheduler::Scheduler;
use crate::service::{discard_log, LogSink, Service, ServiceConfig};

fn scratch_socket() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/cli");
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
    start_logging(gpus, discard_log())
}

fn start_logging(gpus: u32, sink: LogSink) -> Service {
    let config = ServiceConfig {
        socket_path: scratch_socket(),
        require_secure_directory: false,
        ..ServiceConfig::default()
    };
    Service::start(
        Arc::new(StaticInventory::with_count(gpus)),
        None,
        config,
        sink,
    )
    .unwrap()
}

async fn connect(service: &Service) -> Client {
    Client::connect(service.socket_path()).await.unwrap()
}

fn want(count: u32) -> Acquire {
    Acquire {
        num_gpus: count,
        ..Acquire::default()
    }
}

fn test_view() -> VisibleDevices {
    VisibleDevices::from_uuids((0..32).map(|i| format!("GPU-static-{i}")).collect()).unwrap()
}

fn from_cli(options: RunOptions) -> Acquire {
    build_acquire(&options).unwrap()
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    for _ in 0..400 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition never became true");
}

fn denial(result: Result<impl std::fmt::Debug, ClientError>) -> (DenyReason, String) {
    match result {
        Err(ClientError::Denied { reason, detail }) => (reason, detail),
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[tokio::test]
async fn connects_handshakes_and_acquires() {
    let service = start(2);
    let mut client = connect(&service).await;
    let granted = client
        .acquire_with_visibility(&want(2), &test_view(), |_| {})
        .await
        .unwrap();
    assert_eq!(granted.gpus, [0, 1]);
    assert_eq!(granted.uuids, ["GPU-static-0", "GPU-static-1"]);
}

#[tokio::test]
async fn ping_round_trips() {
    let service = start(1);
    connect(&service).await.ping().await.unwrap();
}

#[tokio::test]
async fn a_refusal_is_an_error_that_reads_well() {
    let service = start(2);
    let result = connect(&service)
        .await
        .acquire_with_visibility(&want(4), &test_view(), |_| {})
        .await;
    let error = result.unwrap_err();
    assert_eq!(
        error.to_string(),
        "request can never be satisfied: requested 4 GPUs but this host has only 2 GPUs"
    );
    assert!(matches!(
        error,
        ClientError::Denied {
            reason: DenyReason::UNSATISFIABLE,
            ..
        }
    ));
}

#[tokio::test]
async fn dropping_the_client_frees_the_gpus() {
    let service = start(1);
    let mut client = connect(&service).await;
    client
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 0);
    drop(client);
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
}

#[tokio::test]
async fn waits_in_the_queue_and_is_told_its_position() {
    let service = start(1);
    let mut holder = connect(&service).await;
    holder
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();

    let mut waiter = connect(&service).await;
    let waiting = tokio::spawn(async move {
        let mut positions = Vec::new();
        let gpus = waiter
            .acquire_with_visibility(&want(1), &test_view(), |p| positions.push(p))
            .await
            .unwrap();
        (gpus, positions)
    });
    eventually(|| service.with_scheduler(Scheduler::queue_size) == 1).await;
    drop(holder);
    let (gpus, positions) = waiting.await.unwrap();
    assert_eq!(gpus.gpus, [0]);
    assert_eq!(positions, [0]);
}

#[tokio::test]
async fn a_queued_request_that_times_out_is_reported() {
    let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink: LogSink = {
        let events = events.clone();
        Arc::new(move |_, message| events.lock().unwrap().push(message.to_owned()))
    };
    let service = start_logging(1, sink);
    let mut holder = connect(&service).await;
    holder
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();
    let request = from_cli(RunOptions {
        timeout: Some("80ms".into()),
        ..RunOptions::default()
    });
    let (reason, detail) = denial(
        connect(&service)
            .await
            .acquire_with_visibility(&request, &test_view(), |_| {})
            .await,
    );
    // The daemon's own denial, not a client-side timer: the daemon log
    // records it as a denial, not as a request the client abandoned.
    assert_eq!(reason, DenyReason::TIMED_OUT);
    assert_eq!(detail, "no GPU became available before the deadline");
    // Logged just after the reply is written, so wait for it.
    let denial_logged = || {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.ends_with("denied: no GPU became available before the deadline"))
    };
    eventually(denial_logged).await;
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.ends_with("released its reservation")),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_missing_daemon_is_a_connect_error() {
    let error = Client::connect(&scratch_socket()).await.err().unwrap();
    assert!(error.is_daemon_absent(), "{error:?}");
    assert_eq!(error.to_string(), "No such file or directory");
}

#[tokio::test]
async fn the_released_descriptor_keeps_the_reservation_alive() {
    let service = start(1);
    let mut client = connect(&service).await;
    client
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();
    let fd = client.into_reservation().unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 0);
    drop(fd);
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
}

#[tokio::test]
async fn the_released_descriptor_survives_exec() {
    // The child inherits the descriptor; once this process closes its copy
    // the reservation must live on until the child exits.
    let service = start(1);
    let mut client = connect(&service).await;
    client
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();
    let fd = client.into_reservation().unwrap();
    let flags = fcntl(fd.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
    assert_eq!(flags & FdFlag::FD_CLOEXEC.bits(), 0);

    let mut child = std::process::Command::new("sh")
        .args(["-c", "sleep 0.3"])
        .spawn()
        .unwrap();
    drop(fd);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 0);
    child.wait().unwrap();
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
}

#[tokio::test]
async fn command_line_options_reach_the_daemon_as_a_multi_gpu_request() {
    let service = start(4);
    let request = from_cli(RunOptions {
        num_gpus: Some(3),
        ..RunOptions::default()
    });
    assert_eq!(
        connect(&service)
            .await
            .acquire_with_visibility(&request, &test_view(), |_| {})
            .await
            .unwrap()
            .gpus,
        [0, 1, 2]
    );
}

#[tokio::test]
async fn an_explicit_gpu_list_is_honoured_exactly() {
    let service = start(4);
    let request = from_cli(RunOptions {
        gpus: Some("3,1".into()),
        ..RunOptions::default()
    });
    assert_eq!(
        connect(&service)
            .await
            .acquire_with_visibility(&request, &test_view(), |_| {})
            .await
            .unwrap()
            .gpus,
        [3, 1]
    );
}

#[tokio::test]
async fn an_explicit_gpu_list_waits_for_the_named_device_rather_than_taking_a_free_one() {
    let service = start(2);
    let mut holder = connect(&service).await;
    let zero = from_cli(RunOptions {
        gpus: Some("0".into()),
        ..RunOptions::default()
    });
    holder
        .acquire_with_visibility(&zero, &test_view(), |_| {})
        .await
        .unwrap();

    let mut waiter = connect(&service).await;
    let waiting = tokio::spawn(async move {
        waiter
            .acquire_with_visibility(&zero, &test_view(), |_| {})
            .await
            .unwrap()
    });
    eventually(|| service.with_scheduler(Scheduler::queue_size) == 1).await;
    // GPU 1 is free the whole time, but was not asked for.
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 1);
    drop(holder);
    assert_eq!(waiting.await.unwrap().gpus, [0]);
}

#[tokio::test]
async fn an_explicit_gpu_list_naming_a_device_that_does_not_exist_is_refused() {
    let service = start(2);
    let request = from_cli(RunOptions {
        gpus: Some("5".into()),
        ..RunOptions::default()
    });
    let (reason, detail) = denial(
        connect(&service)
            .await
            .acquire_with_visibility(&request, &test_view(), |_| {})
            .await,
    );
    assert_eq!(reason, DenyReason::INVALID_REQUEST);
    assert!(detail.contains("GPU-static-5 is not managed"), "{detail}");
}

#[tokio::test]
async fn a_status_request_reports_the_machine_with_nothing_running() {
    let service = start(2);
    let reply = connect(&service).await.status().await.unwrap();
    assert_eq!(reply.device_count, 2);
    assert_eq!(reply.devices.len(), 2);
    assert!(reply.holders.is_empty() && reply.waiting.is_empty() && !reply.truncated);
}

#[tokio::test]
async fn a_status_request_reports_a_holder_with_its_command_and_pid() {
    let service = start(1);
    let mut holder = connect(&service).await;
    let request = from_cli(RunOptions {
        command: vec!["python".into(), "train.py".into()],
        ..RunOptions::default()
    });
    holder
        .acquire_with_visibility(&request, &test_view(), |_| {})
        .await
        .unwrap();
    let reply = connect(&service).await.status().await.unwrap();
    assert_eq!(reply.holders.len(), 1);
    assert_eq!(reply.holders[0].command, b"python train.py");
    assert_eq!(reply.holders[0].pid as u32, std::process::id());
    assert_eq!(reply.holders[0].gpus, [0]);
}

#[tokio::test]
async fn a_snapshot_reports_a_queued_request_what_it_wants_and_where_it_sits() {
    let service = start(1);
    let mut holder = connect(&service).await;
    holder
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();
    let mut waiter = connect(&service).await;
    let _waiting = tokio::spawn(async move {
        waiter
            .acquire_with_visibility(&want(1), &test_view(), |_| {})
            .await
    });
    eventually(|| service.with_scheduler(Scheduler::queue_size) == 1).await;

    let reply = connect(&service).await.status().await.unwrap();
    assert_eq!(reply.waiting.len(), 1);
    assert_eq!(reply.waiting[0].num_gpus, 1);
    assert_eq!(reply.waiting[0].position, 0);
    assert!(reply.waiting[0].gpus.is_empty());
}

#[tokio::test]
async fn an_oversized_command_is_refused_by_the_encoder_not_silently_truncated() {
    // `join_command` truncates; a request built by hand must not bypass the
    // wire bound.
    let service = start(1);
    let request = Acquire {
        command: vec![b'x'; MAX_COMMAND_SIZE as usize + 1],
        ..want(1)
    };
    let error = connect(&service)
        .await
        .acquire_with_visibility(&request, &test_view(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Visibility(_)));
}

#[tokio::test]
async fn status_may_be_polled_repeatedly_and_an_observer_is_not_a_holder() {
    let service = start(1);
    let mut observer = connect(&service).await;
    for _ in 0..3 {
        assert!(observer.status().await.unwrap().holders.is_empty());
    }
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 1);
}

#[tokio::test]
async fn a_released_reservation_disappears_from_the_next_snapshot() {
    let service = start(1);
    let mut holder = connect(&service).await;
    holder
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();
    assert_eq!(
        connect(&service)
            .await
            .status()
            .await
            .unwrap()
            .holders
            .len(),
        1
    );
    drop(holder);
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
    assert!(connect(&service)
        .await
        .status()
        .await
        .unwrap()
        .holders
        .is_empty());
}

#[tokio::test]
async fn a_hostile_daemons_denial_detail_is_sanitised() {
    // The detail reaches the user's terminal; escape sequences must not.
    let path = scratch_socket();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut line = Framed::new(stream, FrameCodec);
        let _hello = line.next().await;
        let denied = Denied {
            reason: DenyReason(0xEE),
            detail: b"\x1b]0;pwned\x07\x1b[2J".to_vec(),
        };
        line.send(Message::from(denied)).await.unwrap();
    });
    let error = Client::connect(&path).await.err().unwrap();
    assert_eq!(error.to_string(), "request refused: ?]0;pwned??[2J");
    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn an_out_of_view_grant_closes_the_connection_even_if_client_is_retained() {
    let path = scratch_socket();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut line = Framed::new(stream, FrameCodec);
        assert!(matches!(line.next().await, Some(Ok(Message::Hello(_)))));
        line.send(Message::HelloAck(crate::protocol::HelloAck::default()))
            .await
            .unwrap();
        assert!(matches!(line.next().await, Some(Ok(Message::Acquire(_)))));
        line.send(Message::Granted(Granted {
            gpus: vec![0],
            uuids: vec!["GPU-other".into()],
        }))
        .await
        .unwrap();
        let eof = tokio::time::timeout(Duration::from_secs(1), line.next())
            .await
            .unwrap();
        assert!(eof.is_none(), "bad grant did not release the connection");
    });
    let mut client = Client::connect(&path).await.unwrap();
    let error = client
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Visibility(_)));
    peer.await.unwrap();
    drop(client);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn a_mismatched_hello_ack_is_not_accepted() {
    let path = scratch_socket();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut line = Framed::new(stream, FrameCodec);
        let _ = line.next().await;
        line.send(Message::HelloAck(crate::protocol::HelloAck { version: 3 }))
            .await
            .unwrap();
    });
    assert!(matches!(
        Client::connect(&path).await,
        Err(ClientError::Unexpected(_))
    ));
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn malformed_grant_frames_release_a_retained_client_connection() {
    use tokio::io::AsyncWriteExt;

    fn raw_grant(gpus: &[u32], uuids: &[&str]) -> Vec<u8> {
        let mut body = 5u16.to_le_bytes().to_vec();
        body.extend_from_slice(&(gpus.len() as u32).to_le_bytes());
        for index in gpus {
            body.extend_from_slice(&index.to_le_bytes());
        }
        body.extend_from_slice(&(uuids.len() as u32).to_le_bytes());
        for uuid in uuids {
            body.extend_from_slice(&(uuid.len() as u32).to_le_bytes());
            body.extend_from_slice(uuid.as_bytes());
        }
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend(body);
        frame
    }

    let cases = [
        raw_grant(&[], &[]),
        raw_grant(&[0, 1], &["GPU-static-0", "GPU-static-0"]),
        raw_grant(&[0, 1], &["GPU-static-0"]),
        crate::protocol::encode(&Message::Pong).unwrap(),
    ];
    for raw in cases {
        let path = scratch_socket();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut line = Framed::new(stream, FrameCodec);
            let _ = line.next().await;
            line.send(Message::HelloAck(crate::protocol::HelloAck::default()))
                .await
                .unwrap();
            assert!(matches!(line.next().await, Some(Ok(Message::Acquire(_)))));
            line.get_mut().write_all(&raw).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(1), line.next())
                .await
                .unwrap()
                .is_none());
        });
        let mut client = Client::connect(&path).await.unwrap();
        let result = client
            .acquire_with_visibility(&want(1), &test_view(), |_| {})
            .await;
        assert!(result.is_err());
        peer.await.unwrap(); // Client is intentionally still alive here.
        drop(client);
        std::fs::remove_file(path).unwrap();
    }
}

/// A peer that accepts, then runs `script` on the connection.
fn silent_daemon<F, Fut>(path: &Path, script: F) -> tokio::task::JoinHandle<()>
where
    F: FnOnce(Framed<UnixStream, FrameCodec>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        script(Framed::new(stream, FrameCodec)).await;
    })
}

async fn hello_then<F, Fut>(mut line: Framed<UnixStream, FrameCodec>, rest: F)
where
    F: FnOnce(Framed<UnixStream, FrameCodec>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    assert!(matches!(line.next().await, Some(Ok(Message::Hello(_)))));
    line.send(Message::HelloAck(crate::protocol::HelloAck::default()))
        .await
        .unwrap();
    rest(line).await;
}

/// Long enough for a local connect and handshake to finish well inside it on
/// a loaded runner, since the same bound must also not expire on them.
const SHORT: Duration = Duration::from_millis(500);

#[tokio::test]
async fn acquisition_safety_status_and_second_acquire_preserve_a_holder() {
    let service = start(1);
    let mut client = connect(&service).await;
    client
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();
    let error = client.status().await.unwrap_err();
    assert!(matches!(error, ClientError::Unexpected(_)));
    assert_eq!(
        error.to_string(),
        "client already holds GPUs; use a separate connection"
    );
    client.ping().await.unwrap();
    assert!(matches!(
        client
            .acquire_with_visibility(&want(1), &test_view(), |_| {})
            .await,
        Err(ClientError::Unexpected(_))
    ));
    client.ping().await.unwrap();
    assert_eq!(
        connect(&service)
            .await
            .status()
            .await
            .unwrap()
            .holders
            .len(),
        1
    );
    let fd = client.into_reservation().unwrap();
    assert_eq!(
        connect(&service)
            .await
            .status()
            .await
            .unwrap()
            .holders
            .len(),
        1
    );
    drop(fd);
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
}

#[tokio::test]
async fn acquisition_safety_cancelled_wait_releases_a_retained_client() {
    let service = start(1);
    let mut holder = connect(&service).await;
    holder
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap();
    let mut waiter = connect(&service).await;
    let request = want(1);
    let view = test_view();
    let (queued, received) = tokio::sync::oneshot::channel();
    let mut queued = Some(queued);
    let mut acquiring = Box::pin(waiter.acquire_with_visibility(&request, &view, |_| {
        let _ = queued.take().unwrap().send(());
    }));
    tokio::select! {
        result = &mut acquiring => panic!("acquire ended before cancellation: {result:?}"),
        result = received => result.unwrap(),
    }
    drop(acquiring);
    eventually(|| service.with_scheduler(Scheduler::queue_size) == 0).await;
    drop(holder);
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
    assert_eq!(
        waiter.ping().await.unwrap_err().to_string(),
        "client connection is closed"
    );
    assert_eq!(
        waiter.into_reservation().unwrap_err().to_string(),
        "client has no GPU reservation"
    );
}

#[tokio::test]
async fn acquisition_safety_cancellation_before_first_reply_closes_the_peer() {
    let path = scratch_socket();
    let (sent, received) = tokio::sync::oneshot::channel();
    let peer = silent_daemon(&path, |line| {
        hello_then(line, |mut line| async move {
            assert!(matches!(line.next().await, Some(Ok(Message::Acquire(_)))));
            sent.send(()).unwrap();
            assert!(tokio::time::timeout(SHORT * 4, line.next())
                .await
                .unwrap()
                .is_none());
        })
    });
    let mut client = Client::connect(&path).await.unwrap();
    let request = want(1);
    let view = test_view();
    let mut acquiring = Box::pin(client.acquire_with_visibility(&request, &view, |_| {}));
    tokio::select! {
        result = &mut acquiring => panic!("acquire ended before cancellation: {result:?}"),
        result = received => result.unwrap(),
    }
    drop(acquiring);
    peer.await.unwrap();
    assert!(client.ping().await.is_err());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn acquisition_safety_local_errors_leave_a_ready_connection_usable() {
    let service = start(1);
    let mut client = connect(&service).await;
    assert!(client
        .acquire_with_visibility(&want(0), &test_view(), |_| {})
        .await
        .is_err());
    client.ping().await.unwrap();
    client.status().await.unwrap();
    let request = Acquire {
        timeout_ms: SHORT.as_millis() as u64,
        ..want(1)
    };
    client
        .acquire_with_visibility(&request, &test_view(), |_| {})
        .await
        .unwrap();
    drop(client);
    eventually(|| service.with_scheduler(Scheduler::free_device_count) == 1).await;
    assert!(connect(&service).await.into_reservation().is_err());
}

#[tokio::test]
async fn acquisition_safety_an_unanswered_request_closes_the_retained_client() {
    let path = scratch_socket();
    let peer = silent_daemon(&path, |line| {
        hello_then(line, |mut line| async move {
            assert!(matches!(line.next().await, Some(Ok(Message::Acquire(_)))));
            assert!(tokio::time::timeout(SHORT * 4, line.next())
                .await
                .unwrap()
                .is_none());
        })
    });
    let mut client = Client::connect_with_timeout(&path, SHORT).await.unwrap();
    let result = client
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await;
    assert!(
        matches!(result, Err(ClientError::Unresponsive(_))),
        "{:?}",
        result.err()
    );
    peer.await.unwrap();
    assert!(client.ping().await.is_err());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn a_daemon_that_never_completes_the_handshake_is_reported_as_unresponsive() {
    let path = scratch_socket();
    let peer = silent_daemon(&path, |mut line| async move {
        let _hello = line.next().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let error = Client::connect_with_timeout(&path, SHORT)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, ClientError::Unresponsive(t) if t == SHORT));
    assert_eq!(
        error.to_string(),
        "gpudogd did not answer within 0.5 seconds"
    );
    assert!(!error.is_daemon_absent());
    peer.abort();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn unanswered_ping_and_status_are_reported_as_unresponsive() {
    for status in [false, true] {
        let path = scratch_socket();
        let peer = silent_daemon(&path, |line| {
            hello_then(line, |mut line| async move {
                let _request = line.next().await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            })
        });
        let mut client = Client::connect_with_timeout(&path, SHORT).await.unwrap();
        let error = if status {
            client.status().await.err().unwrap()
        } else {
            client.ping().await.err().unwrap()
        };
        assert!(matches!(error, ClientError::Unresponsive(_)), "{error}");
        peer.abort();
        std::fs::remove_file(path).unwrap();
    }
}

#[tokio::test]
async fn an_unanswered_acquire_is_unresponsive_but_waiting_in_the_queue_is_not() {
    // No first reply: bounded.
    let path = scratch_socket();
    let peer = silent_daemon(&path, |line| {
        hello_then(line, |mut line| async move {
            let _acquire = line.next().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    });
    let mut client = Client::connect_with_timeout(&path, SHORT).await.unwrap();
    let error = client
        .acquire_with_visibility(&want(1), &test_view(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Unresponsive(_)), "{error}");
    peer.abort();
    std::fs::remove_file(path).unwrap();

    // Queued at once, granted well after the bound: the wait is not bounded.
    let path = scratch_socket();
    let peer = silent_daemon(&path, |line| {
        hello_then(line, |mut line| async move {
            assert!(matches!(line.next().await, Some(Ok(Message::Acquire(_)))));
            line.send(Message::Queued(crate::protocol::Queued { position: 0 }))
                .await
                .unwrap();
            tokio::time::sleep(SHORT * 4).await;
            line.send(Message::Granted(Granted {
                gpus: vec![0],
                uuids: vec!["GPU-static-0".into()],
            }))
            .await
            .unwrap();
            let _ = line.next().await;
        })
    });
    let mut client = Client::connect_with_timeout(&path, SHORT).await.unwrap();
    let mut positions = Vec::new();
    let granted = client
        .acquire_with_visibility(&want(1), &test_view(), |p| positions.push(p))
        .await
        .unwrap();
    assert_eq!(positions, [0]);
    assert_eq!(granted.uuids, ["GPU-static-0"]);
    drop(client);
    peer.await.unwrap();
    std::fs::remove_file(path).unwrap();
}
