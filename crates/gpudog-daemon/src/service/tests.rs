//! Service tests. Every test runs a real daemon on a
//! scratch socket and talks to it over the wire.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::AsyncWriteExt;

use super::*;
use crate::gpu_inventory::StaticInventory;

/// Unique short path under the workspace `tmp/`. `sun_path` is only 108
/// bytes, so the name is kept small.
fn scratch_socket() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/svc");
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

fn test_config(path: PathBuf) -> ServiceConfig {
    ServiceConfig {
        socket_path: path,
        // tmp/ is not root-owned; the check itself is tested separately.
        require_secure_directory: false,
        handshake_timeout: Duration::from_millis(2000),
        ..ServiceConfig::default()
    }
}

fn start_with(gpus: u32, config: ServiceConfig) -> Service {
    Service::start(
        Arc::new(StaticInventory::with_count(gpus)),
        None,
        config,
        discard_log(),
    )
    .unwrap()
}

fn start(gpus: u32) -> Service {
    start_with(gpus, test_config(scratch_socket()))
}

type ObservedState =
    Arc<Mutex<Result<Vec<crate::gpu_processes::GpuProcess>, gpudog_core::nvml::NvmlError>>>;

struct ControlledSource(ObservedState);
impl GpuProcessSource for ControlledSource {
    fn processes(
        &self,
        _uuids: &[String],
    ) -> Result<Vec<crate::gpu_processes::GpuProcess>, gpudog_core::nvml::NvmlError> {
        self.0.lock().unwrap().clone()
    }
}

fn observed_service(
    initial: Result<Vec<crate::gpu_processes::GpuProcess>, gpudog_core::nvml::NvmlError>,
) -> (Service, ObservedState) {
    observed_service_logging(initial, discard_log())
}

fn observed_service_logging(
    initial: Result<Vec<crate::gpu_processes::GpuProcess>, gpudog_core::nvml::NvmlError>,
    log: LogSink,
) -> (Service, ObservedState) {
    let observed = Arc::new(Mutex::new(initial));
    let path = scratch_socket();
    let procfs = path.with_extension("proc");
    std::fs::create_dir(&procfs).unwrap();
    let mut config = test_config(path);
    config.watch_unmanaged_ms = 20;
    let service = Service::start_with_observer(
        Arc::new(StaticInventory::with_count(2)),
        config,
        log,
        Arc::new(ControlledSource(observed.clone())),
        ProcFs::new(procfs),
    )
    .unwrap();
    (service, observed)
}

#[tokio::test]
async fn gpu_processes_without_readable_owners_are_reported_once() {
    // The daemon's procfs knows nothing of pid 99, as in a container without
    // --pid=host: busy GPUs cannot count as anyone's, so it says so once.
    let process = crate::gpu_processes::GpuProcess {
        nvml_index: 0,
        gpu_uuid: "GPU-static-0".into(),
        pid: 99,
        used_memory: Some(42),
    };
    let (sink, events) = recording_log();
    let (service, source) = observed_service_logging(Ok(vec![process.clone()]), sink);
    eventually(|| service.snapshot().observation_age_ms.is_some()).await;
    // Several observation periods.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The GPU goes idle and busy again, as with every job: still once.
    *source.lock().unwrap() = Ok(vec![]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    *source.lock().unwrap() = Ok(vec![process]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let warnings = events
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, message)| {
            *level == LogLevel::Warning
                && message.contains("no observed GPU process has a readable owner")
        })
        .count();
    assert_eq!(warnings, 1);
}

#[tokio::test]
async fn external_process_is_shown_and_waited_for_even_without_proc_metadata() {
    let process = crate::gpu_processes::GpuProcess {
        nvml_index: 0,
        gpu_uuid: "GPU-static-0".into(),
        pid: 99,
        used_memory: Some(42),
    };
    let (service, source) = observed_service(Ok(vec![process]));
    eventually(|| service.snapshot().observation_age_ms.is_some()).await;
    let status = service.snapshot();
    assert_eq!(status.busy_gpus, [0]);
    assert_eq!(
        status.processes[0].ownership,
        protocol::ProcessOwnership::Unknown
    );
    let mut client = Client::ready(&service).await;
    assert!(is_queued(&client.acquire(by_uuid(&["GPU-static-0"])).await));
    assert!(!service.with_scheduler(|s| s.is_held(Slot(0))));
    assert_eq!(service.snapshot().busy_gpus, [0]);
    *source.lock().unwrap() = Ok(vec![]);
    assert!(is_granted(
        &tokio::time::timeout(Duration::from_secs(1), client.recv())
            .await
            .unwrap()
    ));
}

#[tokio::test]
async fn failed_observation_blocks_new_grants_and_still_expires_queue_requests() {
    let (service, source) = observed_service(Ok(vec![]));
    eventually(|| !service.with_scheduler(Scheduler::allocation_paused)).await;
    let mut holder = Client::ready(&service).await;
    let first = holder.acquire(by_uuid(&["GPU-static-0"])).await;
    // A query may start between the readiness test and submission.
    if is_queued(&first) {
        assert!(is_granted(&holder.recv().await));
    } else {
        assert!(is_granted(&first));
    }
    *source.lock().unwrap() = Err(gpudog_core::nvml::NvmlError("mock observer failure".into()));
    eventually(|| !service.snapshot().observation_error.is_empty()).await;
    assert!(service.with_scheduler(|s| s.holds(1)));
    let mut waiter = Client::ready(&service).await;
    assert!(is_queued(
        &waiter
            .acquire(Acquire {
                timeout_ms: 50,
                ..by_uuid(&["GPU-static-1"])
            })
            .await
    ));
    assert_eq!(
        deny_reason(
            &tokio::time::timeout(Duration::from_secs(1), waiter.recv())
                .await
                .unwrap()
        ),
        Some(DenyReason::TIMED_OUT)
    );
    assert!(service.snapshot().allocation_paused);
    *source.lock().unwrap() = Ok(vec![]);
    eventually(|| !service.with_scheduler(Scheduler::allocation_paused)).await;
    assert!(service.snapshot().observation_error.is_empty());
}

#[tokio::test]
async fn failed_initial_observation_never_grants_from_an_unchecked_inventory() {
    let (service, _) = observed_service(Err(gpudog_core::nvml::NvmlError("unavailable".into())));
    let mut client = Client::ready(&service).await;
    assert!(is_queued(
        &client
            .acquire(Acquire {
                timeout_ms: 40,
                ..by_uuid(&["GPU-static-0"])
            })
            .await
    ));
    assert_eq!(
        deny_reason(
            &tokio::time::timeout(Duration::from_secs(1), client.recv())
                .await
                .unwrap()
        ),
        Some(DenyReason::TIMED_OUT)
    );
    assert!(service.snapshot().holders.is_empty());
}

#[tokio::test]
async fn stop_wins_against_late_success_and_holder_release() {
    let mut service = start(1);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(&holder.acquire(want(1)).await));
    let mut waiter = Client::ready(&service).await;
    assert!(is_queued(&waiter.acquire(want(1)).await));
    let shared = service.shared.clone();
    let ready = Arc::new(std::sync::Barrier::new(2));
    let late_ready = ready.clone();
    let late_success = std::thread::spawn(move || {
        late_ready.wait();
        let mut state = shared.lock();
        state.resume_observed_usage(&[]); // same gate used by successful samples
        state.pump(Instant::now());
    });
    service.stop();
    ready.wait();
    late_success.join().unwrap();
    assert!(service.with_scheduler(Scheduler::allocation_paused));
    drop(holder);
    eventually(|| !service.with_scheduler(|s| s.is_held(Slot(0)))).await;
    assert!(service.with_scheduler(Scheduler::allocation_paused));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), waiter.recv())
            .await
            .is_err()
    );
    assert!(service.with_scheduler(|s| s.is_queued(2)));
}

#[tokio::test]
async fn stale_sample_cannot_grant_even_if_last_sample_reported_no_usage() {
    let service = start(1);
    {
        let mut state = service.shared.lock();
        state.observation.enabled = true;
        state.observation.period = Duration::from_millis(20);
        state.observation.completed = Some(Instant::now() - Duration::from_millis(100));
        state.scheduler.set_observed_usage(false, &[]);
    }
    let mut client = Client::ready(&service).await;
    assert!(is_queued(
        &client
            .acquire(Acquire {
                timeout_ms: 40,
                ..want(1)
            })
            .await
    ));
    assert!(service.snapshot().allocation_paused);
    assert_eq!(
        deny_reason(
            &tokio::time::timeout(Duration::from_secs(1), client.recv())
                .await
                .unwrap()
        ),
        Some(DenyReason::TIMED_OUT)
    );
}

struct SlowSource {
    started: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<std::sync::atomic::AtomicBool>,
}
impl GpuProcessSource for SlowSource {
    fn processes(
        &self,
        _uuids: &[String],
    ) -> Result<Vec<crate::gpu_processes::GpuProcess>, gpudog_core::nvml::NvmlError> {
        self.started.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.release.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(vec![])
    }
}

#[tokio::test]
async fn stalled_observation_does_not_block_socket_handshakes_or_queue_expiry() {
    use std::sync::atomic::AtomicBool;
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let service = Service::start_with_observer(
        Arc::new(StaticInventory::with_count(1)),
        test_config(scratch_socket()),
        discard_log(),
        Arc::new(SlowSource {
            started: started.clone(),
            release: release.clone(),
        }),
        ProcFs::default(),
    )
    .unwrap();
    eventually(|| started.load(Ordering::SeqCst)).await;
    let result = tokio::time::timeout(Duration::from_millis(500), async {
        let mut client = Client::ready(&service).await;
        assert!(is_queued(
            &client
                .acquire(Acquire {
                    timeout_ms: 40,
                    ..want(1)
                })
                .await
        ));
        client.recv().await
    })
    .await;
    release.store(true, Ordering::SeqCst);
    assert_eq!(deny_reason(&result.unwrap()), Some(DenyReason::TIMED_OUT));
    assert!(service.snapshot().holders.is_empty());
}

struct Client(Line);

impl Client {
    async fn connect(service: &Service) -> Self {
        Self(Framed::new(
            UnixStream::connect(service.socket_path()).await.unwrap(),
            FrameCodec,
        ))
    }

    async fn send(&mut self, message: impl Into<Message>) {
        self.0.send(message.into()).await.unwrap();
    }

    /// The next message, or `None` once the daemon has closed the
    /// connection (or sent something unparsable).
    async fn recv(&mut self) -> Option<Message> {
        self.0.next().await.and_then(Result::ok)
    }

    async fn handshake(&mut self) {
        self.send(protocol::Hello::default()).await;
        assert_eq!(
            self.recv().await,
            Some(Message::HelloAck(HelloAck::default()))
        );
    }

    async fn acquire(&mut self, request: Acquire) -> Option<Message> {
        self.send(request).await;
        self.recv().await
    }

    /// A client already handshaken.
    async fn ready(service: &Service) -> Self {
        let mut client = Self::connect(service).await;
        client.handshake().await;
        client
    }
}

fn want(count: u32) -> Acquire {
    Acquire {
        num_gpus: count,
        candidates: (0..32).map(|i| format!("GPU-static-{i}")).collect(),
        ..Acquire::default()
    }
}

fn is_granted(message: &Option<Message>) -> bool {
    matches!(message, Some(Message::Granted(_)))
}

fn is_queued(message: &Option<Message>) -> bool {
    matches!(message, Some(Message::Queued(_)))
}

fn deny_reason(message: &Option<Message>) -> Option<DenyReason> {
    match message {
        Some(Message::Denied(denied)) => Some(denied.reason),
        _ => None,
    }
}

/// Lets other sessions progress.
async fn pause(milliseconds: u64) {
    tokio::time::sleep(Duration::from_millis(milliseconds)).await;
}

/// Waits (bounded) until `condition` holds, for daemon-side effects of a
/// client action that the client cannot observe directly.
async fn eventually(mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        pause(5).await;
    }
    panic!("condition never became true");
}

#[tokio::test]
async fn completes_a_handshake_and_answers_ping() {
    let service = start(2);
    let mut client = Client::ready(&service).await;
    client.send(Message::Ping).await;
    assert_eq!(client.recv().await, Some(Message::Pong));
}

#[tokio::test]
async fn creates_the_socket_with_the_configured_mode() {
    // bind(2) applies the umask, so without an explicit chmod every non-root
    // client would get EACCES.
    let service = start(1);
    let mode = std::fs::metadata(service.socket_path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o666);
}

#[tokio::test]
async fn grants_a_reservation_and_reports_the_devices() {
    let service = start(2);
    let mut client = Client::ready(&service).await;
    assert_eq!(
        client.acquire(want(2)).await,
        Some(
            Granted {
                gpus: vec![0, 1],
                uuids: vec!["GPU-static-0".into(), "GPU-static-1".into()],
                hold_limit_ms: 0,
            }
            .into()
        )
    );
}

#[tokio::test]
async fn releases_a_reservation_when_the_client_disconnects() {
    // The reservation is scoped to the connection: this is the crash-safety
    // story, so it is tested with an abrupt close.
    let service = start(1);
    let mut first = Client::ready(&service).await;
    assert!(is_granted(&first.acquire(want(1)).await));
    drop(first);
    eventually(|| service.with_scheduler(|s| s.free_device_count() == 1)).await;

    let mut second = Client::ready(&service).await;
    assert!(is_granted(&second.acquire(want(1)).await));
    drop(second);
    eventually(|| service.with_scheduler(|s| s.free_device_count() == 1)).await;
}

#[tokio::test]
async fn queues_a_second_client_and_grants_it_on_release() {
    let service = start(1);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(&holder.acquire(want(1)).await));

    let mut waiter = Client::ready(&service).await;
    assert!(is_queued(&waiter.acquire(want(1)).await));

    // Holder goes away; the queued client must be woken and granted.
    drop(holder);
    assert!(is_granted(&waiter.recv().await));
}

#[tokio::test]
async fn denies_a_client_that_times_out_while_queued() {
    let service = start(1);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(&holder.acquire(want(1)).await));

    let mut waiter = Client::ready(&service).await;
    let request = Acquire {
        timeout_ms: 60,
        ..want(1)
    };
    assert!(is_queued(&waiter.acquire(request).await));
    assert_eq!(
        deny_reason(&waiter.recv().await),
        Some(DenyReason::TIMED_OUT)
    );
}

#[tokio::test]
async fn denies_a_request_larger_than_the_machine() {
    let service = start(2);
    let mut client = Client::ready(&service).await;
    assert_eq!(
        deny_reason(&client.acquire(want(4)).await),
        Some(DenyReason::UNSATISFIABLE)
    );
}

#[tokio::test]
async fn rejects_a_client_speaking_another_protocol_version() {
    let service = start(1);
    let mut client = Client::connect(&service).await;
    client
        .send(protocol::Hello {
            version: PROTOCOL_VERSION + 99,
        })
        .await;
    let reply = client.recv().await;
    assert_eq!(deny_reason(&reply), Some(DenyReason::VERSION_MISMATCH));
    let Some(Message::Denied(denied)) = reply else {
        unreachable!()
    };
    assert_eq!(
        String::from_utf8(denied.detail).unwrap(),
        format!(
            "daemon speaks protocol version {PROTOCOL_VERSION}, client speaks {}",
            PROTOCOL_VERSION + 99
        )
    );
}

#[tokio::test]
async fn logs_a_version_mismatch_even_if_the_client_hangs_up_at_once() {
    let (sink, events) = recording_log();
    let service = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        test_config(scratch_socket()),
        sink,
    )
    .unwrap();
    let mut client = Client::connect(&service).await;
    client
        .send(protocol::Hello {
            version: PROTOCOL_VERSION + 99,
        })
        .await;
    drop(client);
    let logged = || {
        events.lock().unwrap().iter().any(|(level, line)| {
            *level == LogLevel::Warning && line.contains("version mismatch (client")
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !logged() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(logged());
}

#[test]
fn a_peer_that_hangs_up_is_not_a_transport_error() {
    for kind in [
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::BrokenPipe,
    ] {
        assert!(is_hang_up(&io::Error::from(kind)), "{kind:?}");
    }
    assert!(!is_hang_up(&io::Error::from(
        io::ErrorKind::PermissionDenied
    )));
}

#[tokio::test]
async fn drops_a_client_that_skips_the_handshake() {
    let service = start(1);
    let mut client = Client::connect(&service).await;
    // Straight to acquire, with no hello first.
    assert_eq!(client.acquire(want(1)).await, None);
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 1);
}

#[tokio::test]
async fn refuses_connections_past_its_limit() {
    let service = start_with(
        4,
        ServiceConfig {
            max_connections: 1,
            ..test_config(scratch_socket())
        },
    );
    let mut first = Client::ready(&service).await;
    assert!(is_granted(&first.acquire(want(1)).await));

    let mut second = Client::connect(&service).await;
    assert_eq!(
        deny_reason(&second.recv().await),
        Some(DenyReason::INTERNAL_ERROR)
    );
}

#[tokio::test]
async fn releases_a_queued_client_that_gives_up() {
    // A client that disconnects while waiting must vanish from the queue, not
    // block the clients behind it.
    let service = start(1);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(&holder.acquire(want(1)).await));
    let mut leaver = Client::ready(&service).await;
    assert!(is_queued(&leaver.acquire(want(1)).await));
    let mut patient = Client::ready(&service).await;
    assert!(is_queued(&patient.acquire(want(1)).await));

    // The impatient client hangs up while still queued.
    drop(leaver);
    eventually(|| service.with_scheduler(|s| s.queue_size() == 1)).await;
    drop(holder);

    assert!(is_granted(&patient.recv().await));
}

#[tokio::test]
async fn drops_the_queue_entry_of_a_client_that_dies_unsatisfied() {
    // The head wants more GPUs than the holder will ever free, so nothing can
    // unjam it by granting. Its connection dies abruptly. The session must
    // unwind anyway, or a dead client keeps its GPUs reserved as a waiter forever.
    let service = start(2);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(&holder.acquire(want(1)).await));
    let mut doomed = Client::ready(&service).await;
    assert!(is_queued(&doomed.acquire(want(2)).await));
    let mut blocked = Client::ready(&service).await;
    assert!(is_queued(&blocked.acquire(want(1)).await));

    // SIGKILL-equivalent: the doomed client vanishes without a word.
    drop(doomed);
    // The head is gone, so the blocked client is granted the free GPU
    // without waiting for the holder.
    assert!(is_granted(&blocked.recv().await));
    assert_eq!(service.with_scheduler(Scheduler::queue_size), 0);
}

#[tokio::test]
async fn enforces_a_per_user_connection_limit() {
    // All test connections share one uid, so a low limit is testable.
    let service = start_with(
        8,
        ServiceConfig {
            max_connections_per_uid: 2,
            ..test_config(scratch_socket())
        },
    );
    let first = Client::ready(&service).await;
    let second = Client::ready(&service).await;
    let mut third = Client::connect(&service).await;
    assert_eq!(
        deny_reason(&third.recv().await),
        Some(DenyReason::INVALID_REQUEST)
    );
    drop((first, second, third));

    // Both admitted and rejected connections must leave no phantom counts.
    let uid = nix::unistd::getuid().as_raw();
    eventually(|| service.active_connections() == 0 && service.connections_per_uid(uid) == 0).await;
}

#[tokio::test]
async fn per_user_counts_unwind_after_over_limit_rejections() {
    // An over-limit rejection must not leak its count, or a user cycling
    // connects is locked out by their own phantom connections.
    let service = start_with(
        8,
        ServiceConfig {
            max_connections_per_uid: 2,
            ..test_config(scratch_socket())
        },
    );
    let uid = nix::unistd::getuid().as_raw();
    for _ in 0..3 {
        let first = Client::ready(&service).await;
        let second = Client::ready(&service).await;
        let mut third = Client::connect(&service).await;
        assert!(deny_reason(&third.recv().await).is_some());
        drop((first, second, third));
        eventually(|| service.connections_per_uid(uid) == 0).await;
    }
    assert_eq!(service.active_connections(), 0);
}

#[tokio::test]
async fn ends_a_queued_client_that_sends_garbage() {
    // A protocol violation while parked must unwind the session and free the
    // queue slot, exactly like a disconnect.
    let service = start(1);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(&holder.acquire(want(1)).await));
    let mut noisy = Client::ready(&service).await;
    assert!(is_queued(&noisy.acquire(want(1)).await));
    let mut patient = Client::ready(&service).await;
    assert!(is_queued(&patient.acquire(want(1)).await));

    // The queued client sends an unknown message type.
    noisy
        .0
        .get_mut()
        .write_all(&[0x02, 0x00, 0x00, 0x00, 0xEE, 0x00])
        .await
        .unwrap();
    assert_eq!(noisy.recv().await, None);

    // Only the patient remains, still waiting for the holder's GPU.
    eventually(|| service.with_scheduler(|s| s.queue_size() == 1)).await;
    drop(holder);
    assert!(is_granted(&patient.recv().await));
}

#[tokio::test]
async fn a_queued_client_may_ping_while_waiting() {
    let service = start(1);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(&holder.acquire(want(1)).await));
    let mut waiter = Client::ready(&service).await;
    assert!(is_queued(&waiter.acquire(want(1)).await));

    waiter.send(Message::Ping).await;
    assert_eq!(waiter.recv().await, Some(Message::Pong));
    drop(holder);
    assert!(is_granted(&waiter.recv().await));
}

#[tokio::test]
async fn drops_a_client_that_never_finishes_the_handshake() {
    let service = start_with(
        2,
        ServiceConfig {
            handshake_timeout: Duration::from_millis(80),
            ..test_config(scratch_socket())
        },
    );
    let mut silent = Client::connect(&service).await;
    // Say nothing at all. The daemon's handshake deadline must close us.
    assert_eq!(silent.recv().await, None);
    eventually(|| service.active_connections() == 0).await;
}

#[tokio::test]
async fn answers_status_with_holders_waiters_and_devices() {
    let service = start(1);
    let mut holder = Client::ready(&service).await;
    let request = Acquire {
        command: b"python train.py\x1b[31m".to_vec(),
        ..want(1)
    };
    assert!(is_granted(&holder.acquire(request).await));
    let mut waiter = Client::ready(&service).await;
    assert!(is_queued(&waiter.acquire(want(1)).await));

    let mut dash = Client::ready(&service).await;
    dash.send(Message::Status).await;
    let Some(Message::StatusReply(reply)) = dash.recv().await else {
        panic!("expected a status reply");
    };
    assert_eq!(reply.device_count, 1);
    assert_eq!(reply.devices.len(), 1);
    assert_eq!(reply.holders.len(), 1);
    assert_eq!(reply.waiting.len(), 1);
    assert!(!reply.truncated);
    assert_eq!(reply.holders[0].pid as u32, std::process::id());
    // Sanitised at the daemon boundary.
    assert_eq!(reply.holders[0].command, b"python train.py?[31m");
    assert_eq!(reply.waiting[0].position, 0);
}

#[tokio::test]
async fn the_directory_check_rejects_a_world_writable_directory() {
    // A writable directory lets a local user unlink the socket and bind their
    // own, becoming a man-in-the-middle for every client.
    let directory = scratch_socket().with_extension("dir");
    std::fs::create_dir_all(&directory).unwrap();
    let chmod =
        |mode| std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(mode)).unwrap();

    chmod(0o777);
    assert!(directory_complaint(&directory)
        .unwrap()
        .contains("writable"));
    chmod(0o755);
    assert_eq!(directory_complaint(&directory), None);
    // The sticky bit does not make a shared directory safe.
    chmod(0o1777);
    assert!(directory_complaint(&directory).is_some());

    std::fs::remove_dir(&directory).unwrap();
}

#[tokio::test]
async fn the_directory_check_rejects_a_missing_directory() {
    let complaint = directory_complaint(Path::new("tmp/definitely-not-here-12345")).unwrap();
    assert!(complaint.contains("cannot stat"), "{complaint}");
    assert!(
        complaint.ends_with("No such file or directory"),
        "{complaint}"
    );
}

#[test]
fn a_bare_socket_name_is_checked_against_the_working_directory() {
    assert_eq!(socket_directory(Path::new("gpudogd.sock")), Path::new("."));
    assert_eq!(
        socket_directory(Path::new("/run/x.sock")),
        Path::new("/run")
    );
    assert_eq!(socket_directory(Path::new("a/x.sock")), Path::new("a"));
}

#[tokio::test]
async fn start_creates_a_missing_socket_directory() {
    let directory = scratch_socket().with_extension("new");
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let lines = lines.clone();
        Arc::new(move |_: LogLevel, line: &str| lines.lock().unwrap().push(line.to_owned()))
    };
    let config = ServiceConfig {
        require_secure_directory: true,
        ..test_config(directory.join("s.sock"))
    };
    let service =
        Service::start(Arc::new(StaticInventory::with_count(1)), None, config, sink).unwrap();
    let info = std::fs::symlink_metadata(&directory).unwrap();
    assert!(info.is_dir());
    assert_eq!(info.mode() & 0o7777, 0o755);
    let created = format!("created socket directory {}", directory.display());
    assert!(
        lines.lock().unwrap().contains(&created),
        "{:?}",
        lines.lock().unwrap()
    );
    drop(service);
    std::fs::remove_dir(&directory).unwrap();
}

#[tokio::test]
async fn a_directory_under_an_open_parent_keeps_the_umask_mode() {
    let parent = scratch_socket().with_extension("open-parent");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
    let directory = parent.join("run");
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let lines = lines.clone();
        Arc::new(move |level: LogLevel, line: &str| {
            lines.lock().unwrap().push((level, line.to_owned()));
        })
    };
    let service = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        test_config(directory.join("s.sock")),
        sink,
    )
    .unwrap();
    let mode = std::fs::metadata(&directory).unwrap().mode() & 0o7777;
    let warning = format!(
        "left mode {mode:04o} from the umask on {}: ",
        directory.display()
    );
    let lines = lines.lock().unwrap().clone();
    assert!(
        lines.iter().any(|(level, line)| *level == LogLevel::Warning
            && line.starts_with(&warning)
            && line.contains("writable by group or others")),
        "{lines:?}"
    );
    drop(service);
    assert_eq!(create_socket_directory(&directory).unwrap(), Created::No);
    std::fs::remove_dir(&directory).unwrap();
    std::fs::remove_dir(&parent).unwrap();
}

#[tokio::test]
async fn start_creates_only_the_last_directory() {
    let parent = scratch_socket().with_extension("absent");
    let error = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        test_config(parent.join("dir/s.sock")),
        discard_log(),
    )
    .err()
    .unwrap();
    assert!(error.0.starts_with("cannot create "), "{error}");
    assert!(error.0.ends_with("No such file or directory"), "{error}");
    assert!(!parent.exists());
}

#[tokio::test]
async fn start_leaves_an_existing_socket_directory_symlink_to_the_check() {
    // mkdir sees the link as existing, so nothing is created or chmodded.
    // The O_NOFOLLOW open guards a race that cannot be staged here.
    let target = scratch_socket().with_extension("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777)).unwrap();
    let link = scratch_socket().with_extension("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let config = ServiceConfig {
        require_secure_directory: true,
        ..test_config(link.join("s.sock"))
    };
    let error = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        config,
        discard_log(),
    )
    .err()
    .unwrap();
    assert!(error.0.contains("writable by group or others"), "{error}");
    // Not chmodded through the link.
    let mode = std::fs::metadata(&target).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o777);
    std::fs::remove_file(&link).unwrap();
    std::fs::remove_dir(&target).unwrap();
}

#[tokio::test]
async fn start_refuses_an_insecure_socket_directory() {
    let directory = scratch_socket().with_extension("open");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
    let config = ServiceConfig {
        require_secure_directory: true,
        ..test_config(directory.join("s.sock"))
    };
    let error = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        config,
        discard_log(),
    )
    .err()
    .unwrap();
    assert!(error.0.starts_with("refusing to bind: "), "{error}");
    std::fs::remove_dir(&directory).unwrap();
}

#[tokio::test]
async fn refuses_to_bind_where_another_daemon_is_listening() {
    let service = start(1);
    let duplicate = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        test_config(service.socket_path().to_owned()),
        discard_log(),
    );
    assert!(duplicate.err().unwrap().0.contains("already listening"));
    // The refused duplicate must not have removed the live socket.
    Client::ready(&service).await;
}

#[tokio::test]
async fn replaces_a_stale_socket_file() {
    // A crashed daemon leaves its socket behind; the next start reclaims it.
    let path = scratch_socket();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());

    let service = start_with(1, test_config(path.clone()));
    Client::ready(&service).await;
    drop(service);
    assert!(!path.exists());
}

#[tokio::test]
async fn stop_removes_the_socket_and_is_idempotent() {
    let mut service = start(1);
    let path = service.socket_path().to_owned();
    service.stop();
    service.stop();
    assert!(!path.exists());
}

#[test]
fn shutdown_with_a_live_session_still_releases_it() {
    // A session destroyed at runtime teardown must still clean up: the
    // cleanup is a Drop, so it must run.
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink: LogSink = {
        let events = events.clone();
        Arc::new(move |_, message| events.lock().unwrap().push(message.to_owned()))
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (service, client) = runtime.block_on(async {
        let service = Service::start(
            Arc::new(StaticInventory::with_count(1)),
            None,
            test_config(scratch_socket()),
            sink,
        )
        .unwrap();
        let mut client = Client::ready(&service).await;
        assert!(is_granted(&client.acquire(want(1)).await));
        (service, client)
    });
    drop(runtime);
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.ends_with("released its reservation")),
        "{:?}",
        events.lock().unwrap()
    );
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 1);
    assert_eq!(service.active_connections(), 0);
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_client_is_eventually_granted_under_concurrent_churn() {
    // Many clients race for one GPU on a multi-threaded runtime. A lost
    // wakeup would leave one parked forever, and the timeout below would
    // fire.
    let service = Arc::new(start_with(
        1,
        ServiceConfig {
            max_connections_per_uid: 64,
            // Every test client shares one uid; lift the per-user queue cap
            // so the race, not the cap, is what is exercised.
            scheduler_limits: SchedulerLimits {
                max_queued_per_uid: 64,
                ..SchedulerLimits::default()
            },
            ..test_config(scratch_socket())
        },
    ));
    let clients: Vec<_> = (0..24)
        .map(|_| {
            let service = service.clone();
            tokio::spawn(async move {
                let mut client = Client::ready(&service).await;
                let mut reply = client.acquire(want(1)).await;
                if is_queued(&reply) {
                    reply = client.recv().await;
                }
                assert!(is_granted(&reply), "{reply:?}");
                tokio::task::yield_now().await;
            })
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(10), async {
        for client in clients {
            client.await.unwrap();
        }
    })
    .await
    .expect("a client was never granted");
    eventually(|| service.with_scheduler(|s| s.free_device_count() == 1 && s.queue_size() == 0))
        .await;
}

type Events = Arc<Mutex<Vec<(LogLevel, String)>>>;

/// A log sink that records every event, for asserting what was (not) logged.
fn recording_log() -> (LogSink, Events) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink: LogSink = {
        let events = events.clone();
        Arc::new(move |level, message: &str| {
            events.lock().unwrap().push((level, message.to_owned()))
        })
    };
    (sink, events)
}

#[tokio::test]
async fn a_client_dying_mid_frame_is_not_logged_as_an_error() {
    // Any EOF is a quiet hang-up.
    let (sink, events) = recording_log();
    let service = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        test_config(scratch_socket()),
        sink,
    )
    .unwrap();
    let mut client = Client::ready(&service).await;
    // Half of a ping frame, then gone.
    client
        .0
        .get_mut()
        .write_all(&[0x02, 0x00, 0x00, 0x00, 0x07])
        .await
        .unwrap();
    drop(client);
    eventually(|| service.active_connections() == 0).await;
    let errors: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, _)| *level == LogLevel::Error)
        .cloned()
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

#[tokio::test]
async fn a_client_that_never_reads_its_replies_is_dropped() {
    // Pre-acquire writes are bounded too: a client flooding Status requests
    // without reading must not pin a connection slot forever.
    let service = start_with(
        1,
        ServiceConfig {
            handshake_timeout: Duration::from_millis(100),
            ..test_config(scratch_socket())
        },
    );
    let client = Client::ready(&service).await;
    assert_eq!(service.active_connections(), 1);
    let status = protocol::encode(&Message::Status).unwrap();
    let flood: Vec<u8> = status
        .iter()
        .copied()
        .cycle()
        .take(status.len() * 20_000)
        .collect();
    let mut stream = client.0.into_inner();
    let writer = tokio::spawn(async move {
        // May fail once the daemon gives up and closes; that is the point.
        let _ = stream.write_all(&flood).await;
        stream
    });
    for _ in 0..600 {
        if service.active_connections() == 0 {
            break;
        }
        pause(5).await;
    }
    assert_eq!(service.active_connections(), 0);
    drop(writer);
}

#[tokio::test]
async fn refuses_to_replace_a_file_that_is_not_a_socket() {
    let path = scratch_socket();
    std::fs::write(&path, "precious").unwrap();
    let error = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        test_config(path.clone()),
        discard_log(),
    )
    .err()
    .unwrap();
    assert!(error.0.contains("not a socket"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn pump_hands_a_reclaimed_grant_to_the_next_waiter_at_once() {
    // Id 2 is granted but its session is already gone; id 3 is behind it.
    // Without the re-tick, id 3 would wait for some later event.
    let request = |id| ReservationRequest {
        id,
        uid: 1000,
        selection: GpuSelection::Count,
        num_gpus: 1,
        gpus: Vec::new(),
        candidates: None,
        deadline: None,
    };
    let now = Instant::now();
    let mut state = State {
        scheduler: Scheduler::new(&StaticInventory::with_count(1), SchedulerLimits::default()),
        inventory: Arc::new(StaticInventory::with_count(1)),
        waiters: HashMap::new(),
        sessions: HashMap::new(),
        active: 0,
        per_uid: HashMap::new(),
        next_id: 4,
        observation: Observation::default(),
        stopped: false,
        config: ServiceConfig::default(),
    };
    for id in 1..=3 {
        let _ = state.scheduler.submit(&request(id), now);
    }
    assert!(state.scheduler.holds(1));
    // Dropping the receiver at once is what makes the send to id 2 fail.
    let (gone, _) = oneshot::channel();
    let (waiting, mut woken) = oneshot::channel();
    state.waiters.insert(2, gone);
    state.waiters.insert(3, waiting);

    state.scheduler.release(1);
    state.pump(now);
    assert!(matches!(woken.try_recv(), Ok(Reply::Granted { .. })));
    assert!(state.scheduler.holds(3));
    assert!(!state.scheduler.is_queued(3));
    assert!(!state.scheduler.holds(2));
}

#[test]
fn outcomes_another_submit_decides_are_delivered_by_its_pump() {
    // 2 GPUs. uid 3000 holds one. uid 1000's request 2 waits for both until
    // a deadline, and keeps uid 2000's request 3 waiting. A later submit of
    // uid 4000 expires request 2 and, in the same walk, grants request 3;
    // only the pump in `State::submit` delivers both outcomes, since no
    // release or tick follows.
    let request = |id, uid, num_gpus, deadline| ReservationRequest {
        id,
        uid,
        selection: GpuSelection::Count,
        num_gpus,
        gpus: Vec::new(),
        candidates: None,
        deadline,
    };
    let now = Instant::now();
    let later = now + Duration::from_secs(10);
    let mut state = State {
        scheduler: Scheduler::new(&StaticInventory::with_count(2), SchedulerLimits::default()),
        inventory: Arc::new(StaticInventory::with_count(2)),
        waiters: HashMap::new(),
        sessions: HashMap::new(),
        active: 0,
        per_uid: HashMap::new(),
        next_id: 5,
        observation: Observation::default(),
        stopped: false,
        config: ServiceConfig::default(),
    };
    assert!(matches!(
        state.submit(&request(1, 3000, 1, None), now).0,
        Reply::Granted { .. }
    ));
    let (queued, two) = state.submit(&request(2, 1000, 2, Some(later)), now);
    assert!(matches!(queued, Reply::Queued { .. }));
    let mut two = two.expect("a queued request has a waiter");
    let (queued, three) = state.submit(&request(3, 2000, 1, None), now);
    assert!(matches!(queued, Reply::Queued { .. }));
    let mut three = three.expect("a queued request has a waiter");

    let (queued, _four) = state.submit(&request(4, 4000, 1, None), later);
    assert!(matches!(queued, Reply::Queued { .. }));
    assert!(matches!(two.try_recv(), Ok(Reply::Denied { .. })));
    assert!(matches!(three.try_recv(), Ok(Reply::Granted { .. })));
    assert!(state.scheduler.holds(3));
    assert!(state.scheduler.is_queued(4));
}

#[tokio::test]
async fn a_snapshot_caps_an_oversized_device_name_instead_of_failing() {
    let service = Service::start(
        Arc::new(StaticInventory::new(vec![
            crate::gpu_inventory::GpuDevice {
                index: 0,
                uuid: "GPU-0".into(),
                name: "n".repeat(protocol::MAX_STRING_SIZE as usize + 100),
                nvml_index: 0,
            },
        ])),
        None,
        test_config(scratch_socket()),
        discard_log(),
    )
    .unwrap();
    let mut dash = Client::ready(&service).await;
    dash.send(Message::Status).await;
    let Some(Message::StatusReply(reply)) = dash.recv().await else {
        panic!("expected a status reply");
    };
    assert_eq!(
        reply.devices[0].name.len(),
        protocol::MAX_STRING_SIZE as usize
    );
    assert_eq!(reply.devices[0].uuid, b"GPU-0");
    assert!(!reply.truncated);
}

#[tokio::test]
async fn a_snapshot_keeps_reservations_when_the_device_table_cannot_fit() {
    // Devices are spent last from the frame budget: on a machine where
    // truncation bites, the queue is what someone opened the dashboard for.
    let devices: Vec<_> = (0..256)
        .map(|index| crate::gpu_inventory::GpuDevice {
            index,
            uuid: format!("GPU-{index}"),
            name: "n".repeat(400),
            nvml_index: index,
        })
        .collect();
    let service = Service::start(
        Arc::new(StaticInventory::new(devices)),
        None,
        test_config(scratch_socket()),
        discard_log(),
    )
    .unwrap();
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(
        &holder
            .acquire(Acquire {
                candidates: vec!["GPU-0".into()],
                ..want(1)
            })
            .await
    ));

    let reply = service.snapshot();
    assert!(reply.truncated);
    assert_eq!(reply.device_count, 256);
    assert_eq!(reply.holders.len(), 1);
    assert!(reply.devices.len() < 256);
    // Truncated to fit, so it still encodes as one frame.
    assert!(protocol::encode(&Message::StatusReply(reply)).is_ok());
}

fn by_uuid(uuids: &[&str]) -> Acquire {
    Acquire {
        selection: GpuSelection::ExplicitUuids,
        num_gpus: uuids.len() as u32,
        uuids: uuids.iter().map(|&u| u.to_owned()).collect(),
        ..Acquire::default()
    }
}

#[tokio::test]
async fn resolves_an_acquire_by_uuid_onto_its_logical_index() {
    let (sink, events) = recording_log();
    let service = Service::start(
        Arc::new(StaticInventory::with_count(4)),
        None,
        test_config(scratch_socket()),
        sink,
    )
    .unwrap();
    let mut client = Client::ready(&service).await;
    // The daemon's spelling comes back, not the client's: it is what CUDA
    // will be given.
    assert_eq!(
        client
            .acquire(by_uuid(&["gpu-STATIC-3", "GPU-static-1"]))
            .await,
        Some(
            Granted {
                gpus: vec![3, 1],
                uuids: vec!["GPU-static-3".into(), "GPU-static-1".into()],
                hold_limit_ms: 0,
            }
            .into()
        )
    );
    eventually(|| {
        events.lock().unwrap().iter().any(|(level, line)| {
            *level == LogLevel::Access
                && line.ends_with(" granted gpus=[3:GPU-static-3,1:GPU-static-1]")
        })
    })
    .await;
}

#[tokio::test]
async fn denies_an_acquire_naming_a_device_it_does_not_manage() {
    let (sink, events) = recording_log();
    let service = Service::start(
        Arc::new(StaticInventory::with_count(2)),
        None,
        test_config(scratch_socket()),
        sink,
    )
    .unwrap();
    let mut client = Client::ready(&service).await;
    assert_eq!(
        client.acquire(by_uuid(&["GPU-elsewhere"])).await,
        Some(
            Denied {
                reason: DenyReason::INVALID_REQUEST,
                detail: b"GPU-elsewhere is not managed by this daemon".to_vec(),
            }
            .into()
        )
    );
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 2);
    assert!(events.lock().unwrap().iter().any(|(level, line)| {
        *level == LogLevel::Warning && line.ends_with(" requested unmanaged device GPU-elsewhere")
    }));
}

#[tokio::test]
async fn requests_for_an_unmanaged_device_are_throttled_in_the_log() {
    // Each request is a fresh connection, as `gpudog run` makes them; only
    // the first warning in the window is logged.
    let (sink, events) = recording_log();
    let service = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        test_config(scratch_socket()),
        sink,
    )
    .unwrap();
    for _ in 0..3 {
        let mut client = Client::ready(&service).await;
        assert_eq!(
            deny_reason(&client.acquire(by_uuid(&["GPU-elsewhere"])).await),
            Some(DenyReason::INVALID_REQUEST)
        );
    }
    let events = events.lock().unwrap();
    let count = |text: &str| {
        events
            .iter()
            .filter(|(_, line)| line.contains(text))
            .count()
    };
    assert_eq!(count("requested unmanaged device"), 1);
    // Every denial is still in the audit trail.
    assert_eq!(
        count("denied: GPU-elsewhere is not managed by this daemon"),
        3
    );
}

#[tokio::test]
async fn a_grant_to_a_client_that_hangs_up_at_once_is_logged_and_released() {
    let (sink, events) = recording_log();
    let service = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        test_config(scratch_socket()),
        sink,
    )
    .unwrap();
    let mut client = Client::ready(&service).await;
    client.send(want(1)).await;
    drop(client);
    let has = |text: &str| {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, line)| line.contains(text))
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !has("released its reservation") && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(has(" granted gpus=[0:"));
    assert!(has("released its reservation"));
    assert!(!has("transport error"));
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 1);
}

#[tokio::test]
async fn narrows_a_count_request_to_its_managed_candidates() {
    let service = start(4);
    let mut client = Client::ready(&service).await;
    // An unmanaged candidate is ignored, not refused.
    let request = Acquire {
        candidates: vec!["GPU-elsewhere".into(), "gpu-STATIC-2".into()],
        ..want(1)
    };
    let Some(Message::Granted(granted)) = client.acquire(request).await else {
        panic!("expected a grant");
    };
    assert_eq!(granted.uuids, ["GPU-static-2"]);
}

#[tokio::test]
async fn candidates_that_are_all_unmanaged_are_unsatisfiable() {
    let service = start(2);
    let mut client = Client::ready(&service).await;
    let request = Acquire {
        candidates: vec!["GPU-elsewhere".into()],
        ..want(1)
    };
    let reply = client.acquire(request).await;
    assert_eq!(deny_reason(&reply), Some(DenyReason::UNSATISFIABLE));
}

#[tokio::test]
async fn a_reload_applies_new_limits_to_later_requests_only() {
    let service = start(4);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(&holder.acquire(want(3)).await));
    // Fits the old held cap of 8, but not the new one of 2 on its own.
    let mut large = Client::ready(&service).await;
    assert!(is_queued(&large.acquire(want(3)).await));
    let mut small = Client::ready(&service).await;
    assert!(is_queued(&small.acquire(want(2)).await));

    let mut config = service.config();
    config.scheduler_limits.max_held_per_uid = 2;
    service.reconfigure(&config, None).unwrap();

    // Nothing held is taken back; the impossible waiter is told at once,
    // with the wording of a request refused at submit.
    assert_eq!(service.with_scheduler(Scheduler::free_device_count), 1);
    match large.recv().await {
        Some(Message::Denied(denied)) => {
            assert_eq!(denied.reason, DenyReason::INVALID_REQUEST);
            assert_eq!(
                String::from_utf8_lossy(&denied.detail),
                "requested 3 GPUs but the per-user cap is 2 GPUs"
            );
        }
        other => panic!("expected a denial, got {other:?}"),
    }
    assert_eq!(service.with_scheduler(Scheduler::queue_size), 1);

    // New requests meet the new cap.
    let mut late = Client::ready(&service).await;
    match late.acquire(want(3)).await {
        Some(Message::Denied(denied)) => assert_eq!(
            String::from_utf8_lossy(&denied.detail),
            "requested 3 GPUs but the per-user cap is 2 GPUs"
        ),
        other => panic!("expected a denial, got {other:?}"),
    }
    assert_eq!(service.config().scheduler_limits.max_held_per_uid, 2);
    assert_eq!(service.snapshot().holders.len(), 1);
    drop(small);
}

#[tokio::test]
async fn shortening_the_observation_period_wakes_the_observer() {
    let path = scratch_socket();
    let procfs = path.with_extension("proc");
    std::fs::create_dir(&procfs).unwrap();
    let mut config = test_config(path);
    config.watch_unmanaged_ms = MAX_WATCH_UNMANAGED_MS;
    let service = Service::start_with_observer(
        Arc::new(StaticInventory::with_count(1)),
        config,
        discard_log(),
        Arc::new(ControlledSource(Arc::new(Mutex::new(Ok(Vec::new()))))),
        ProcFs::new(procfs),
    )
    .unwrap();
    eventually(|| !service.snapshot().allocation_paused).await;
    pause(100).await;

    // The last observation is now older than the new period. Without an
    // early wake-up the observer would sleep for the rest of the hour and
    // every grant would stay paused.
    let mut config = service.config();
    config.watch_unmanaged_ms = 50;
    service.reconfigure(&config, None).unwrap();
    assert_eq!(service.snapshot().watch_unmanaged_ms, 50);
    eventually(|| !service.snapshot().allocation_paused).await;
    pause(200).await;
    eventually(|| !service.snapshot().allocation_paused).await;
    let mut client = Client::ready(&service).await;
    // A 50 ms period can lapse while a loaded runner connects, pausing
    // grants until the next observation; that one follows within 50 ms, not
    // an hour, so a grant after a short wait still shows the wake-up.
    let mut reply = client.acquire(want(1)).await;
    if is_queued(&reply) {
        reply = tokio::time::timeout(Duration::from_secs(2), client.recv())
            .await
            .expect("granted after the next observation");
    }
    assert!(is_granted(&reply), "{reply:?}");
}

fn uuids(list: &[&str]) -> Vec<String> {
    list.iter().map(|&uuid| uuid.to_owned()).collect()
}

/// A snapshot through the wire, so the decoder's checks (every number below
/// `device_count`) apply to it.
async fn status_over_the_wire(service: &Service) -> StatusReply {
    let mut dash = Client::ready(service).await;
    dash.send(Message::Status).await;
    match dash.recv().await {
        Some(Message::StatusReply(reply)) => reply,
        other => panic!("expected a status reply, got {other:?}"),
    }
}

fn detail(message: &Option<Message>) -> String {
    match message {
        Some(Message::Denied(denied)) => String::from_utf8_lossy(&denied.detail).into_owned(),
        other => panic!("expected a denial, got {other:?}"),
    }
}

fn granted(message: &Option<Message>) -> (Vec<u32>, Vec<String>) {
    match message {
        Some(Message::Granted(granted)) => (granted.gpus.clone(), granted.uuids.clone()),
        other => panic!("expected a grant, got {other:?}"),
    }
}

fn numbered(reply: &StatusReply) -> Vec<(u32, String)> {
    reply
        .devices
        .iter()
        .map(|device| {
            (
                device.index,
                String::from_utf8_lossy(&device.uuid).into_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_reload_that_removes_a_free_gpu_renumbers_the_rest() {
    let service = start(3);
    let mut holder = Client::ready(&service).await;
    assert_eq!(
        granted(&holder.acquire(by_uuid(&["GPU-static-2"])).await),
        (vec![2], uuids(&["GPU-static-2"]))
    );

    let changes = service
        .reconfigure(
            &service.config(),
            Some(&uuids(&["GPU-static-0", "GPU-static-2"])),
        )
        .unwrap();
    assert_eq!(changes.removed, ["GPU-static-1"]);
    assert!(changes.added.is_empty() && changes.retiring.is_empty());

    let status = status_over_the_wire(&service).await;
    assert_eq!(status.device_count, 2);
    assert_eq!(
        numbered(&status),
        [(0, "GPU-static-0".into()), (1, "GPU-static-2".into())]
    );
    assert_eq!(
        status.holders[0].gpus,
        [1],
        "the holder's GPU is renumbered"
    );

    let mut client = Client::ready(&service).await;
    assert_eq!(
        detail(&client.acquire(by_uuid(&["GPU-static-1"])).await),
        "GPU-static-1 is not managed by this daemon"
    );
    let mut other = Client::ready(&service).await;
    assert_eq!(
        granted(&other.acquire(want(1)).await),
        (vec![0], uuids(&["GPU-static-0"]))
    );
}

#[tokio::test]
async fn a_removed_gpu_drains_and_keeps_its_number_until_the_next_reload() {
    let service = start(2);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(
        &holder.acquire(by_uuid(&["GPU-static-0"])).await
    ));

    let changes = service
        .reconfigure(&service.config(), Some(&uuids(&["GPU-static-1"])))
        .unwrap();
    assert_eq!(changes.retiring, ["GPU-static-0"]);
    let status = status_over_the_wire(&service).await;
    assert_eq!(status.device_count, 2);
    assert_eq!(status.holders[0].gpus, [0]);
    assert_eq!(status.busy_gpus, [0]);
    let mut client = Client::ready(&service).await;
    assert_eq!(
        detail(&client.acquire(want(2)).await),
        "requested 2 GPUs but this host has only 1 GPU"
    );

    // Released, it stays out of use and keeps its number.
    drop(holder);
    eventually(|| !service.with_scheduler(|s| s.is_held(Slot(0)))).await;
    let status = status_over_the_wire(&service).await;
    assert_eq!(status.device_count, 2);
    assert_eq!(status.busy_gpus, [0]);
    let mut next = Client::ready(&service).await;
    assert_eq!(
        granted(&next.acquire(want(1)).await),
        (vec![1], uuids(&["GPU-static-1"]))
    );
    let mut waiter = Client::ready(&service).await;
    assert!(is_queued(&waiter.acquire(want(1)).await));

    // The next reload drops it, even with the same `devices`.
    let changes = service
        .reconfigure(&service.config(), Some(&uuids(&["GPU-static-1"])))
        .unwrap();
    assert!(!changes.changed());
    let status = status_over_the_wire(&service).await;
    assert_eq!(numbered(&status), [(0, "GPU-static-1".into())]);
    assert_eq!(status.holders[0].gpus, [0]);
    assert!(status.busy_gpus.is_empty());
    assert_eq!(status.waiting.len(), 1);
}

#[tokio::test]
async fn a_reload_denies_a_waiter_for_a_removed_gpu_at_once() {
    let service = start(2);
    let mut holder = Client::ready(&service).await;
    assert!(is_granted(
        &holder.acquire(by_uuid(&["GPU-static-1"])).await
    ));
    let mut waiter = Client::ready(&service).await;
    assert!(is_queued(&waiter.acquire(by_uuid(&["GPU-static-1"])).await));

    service
        .reconfigure(&service.config(), Some(&uuids(&["GPU-static-0"])))
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(1), waiter.recv())
        .await
        .unwrap();
    assert_eq!(deny_reason(&reply), Some(DenyReason::UNSATISFIABLE));
    assert_eq!(detail(&reply), "GPU-static-1 is not managed by this daemon");
    assert!(service.with_scheduler(|s| s.is_held(Slot(1))));
}

#[tokio::test]
async fn a_reload_naming_an_unknown_gpu_changes_nothing() {
    let service = start(2);
    let mut config = service.config();
    config.max_connections = 7;
    let error = service
        .reconfigure(&config, Some(&uuids(&["GPU-static-0", "GPU-static-9"])))
        .unwrap_err();
    assert_eq!(
        error.0,
        "configured device GPU-static-9 was not found when gpudogd started"
    );
    assert_eq!(service.config().max_connections, DEFAULT_MAX_CONNECTIONS);
    assert_eq!(service.snapshot().device_count, 2);
}

/// Answers "no GPU process" once per permit, recording the UUIDs of every
/// query, so a test can hold a query in flight.
#[derive(Default)]
struct GatedSource {
    permits: Mutex<u32>,
    calls: Mutex<Vec<Vec<String>>>,
    /// Queries waiting at the closed gate.
    blocked: Mutex<usize>,
}

impl GatedSource {
    fn allow(&self, permits: u32) {
        *self.permits.lock().unwrap() = permits;
    }

    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn blocked(&self) -> usize {
        *self.blocked.lock().unwrap()
    }

    fn last(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

impl GpuProcessSource for GatedSource {
    fn processes(
        &self,
        uuids: &[String],
    ) -> Result<Vec<crate::gpu_processes::GpuProcess>, gpudog_core::nvml::NvmlError> {
        self.calls.lock().unwrap().push(uuids.to_vec());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut waiting = false;
        let result = loop {
            {
                let mut permits = self.permits.lock().unwrap();
                if *permits > 0 {
                    *permits -= 1;
                    break Ok(Vec::new());
                }
            }
            if !waiting {
                waiting = true;
                *self.blocked.lock().unwrap() += 1;
            }
            if Instant::now() > deadline {
                break Err(gpudog_core::nvml::NvmlError("gate never opened".into()));
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        if waiting {
            *self.blocked.lock().unwrap() -= 1;
        }
        result
    }
}

#[tokio::test]
async fn an_added_gpu_is_not_granted_until_an_observation_of_it_succeeds() {
    let path = scratch_socket();
    let procfs = path.with_extension("proc");
    std::fs::create_dir(&procfs).unwrap();
    let mut config = test_config(path);
    config.watch_unmanaged_ms = 20;
    let gate = Arc::new(GatedSource::default());
    gate.allow(u32::MAX);
    let (log, events) = recording_log();
    let service = Service::start_with_observer(
        Arc::new(StaticInventory::with_count(2)),
        config,
        log,
        gate.clone(),
        ProcFs::new(procfs),
    )
    .unwrap();
    let config = service.config();
    service
        .reconfigure(&config, Some(&uuids(&["GPU-static-0"])))
        .unwrap();
    eventually(|| gate.last() == ["GPU-static-0"] && !service.snapshot().allocation_paused).await;

    // Hold a query of the old devices in flight, then add GPU 1.
    // Waiting for a blocked query, not a new call: a query that started
    // just before the gate closed is already counted in `calls`.
    gate.allow(0);
    eventually(|| gate.blocked() == 1).await;
    assert_eq!(gate.last(), ["GPU-static-0"]);
    let changes = service.reconfigure(&config, None).unwrap();
    assert_eq!(changes.added, ["GPU-static-1"]);
    assert_eq!(service.snapshot().busy_gpus, [1]);
    let mut client = Client::ready(&service).await;
    assert!(is_queued(&client.acquire(by_uuid(&["GPU-static-1"])).await));

    // The old query reports no usage, but did not look at GPU 1: dropped.
    let before = gate.calls();
    gate.allow(1);
    eventually(|| gate.calls() > before).await;
    assert_eq!(gate.last(), ["GPU-static-0", "GPU-static-1"]);
    assert_eq!(service.snapshot().busy_gpus, [1]);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), client.recv())
            .await
            .is_err()
    );
    assert!(events.lock().unwrap().iter().any(|(_, message)| message
        == "the managed GPUs changed during a GPU observation; observing again"));

    gate.allow(u32::MAX);
    assert!(is_granted(
        &tokio::time::timeout(Duration::from_secs(1), client.recv())
            .await
            .unwrap()
    ));
}

#[tokio::test]
async fn a_removed_gpu_that_is_still_held_is_no_longer_observed() {
    let path = scratch_socket();
    let procfs = path.with_extension("proc");
    std::fs::create_dir(&procfs).unwrap();
    let mut config = test_config(path);
    config.watch_unmanaged_ms = 20;
    let gate = Arc::new(GatedSource::default());
    gate.allow(u32::MAX);
    let service = Service::start_with_observer(
        Arc::new(StaticInventory::with_count(2)),
        config,
        discard_log(),
        gate.clone(),
        ProcFs::new(procfs),
    )
    .unwrap();
    eventually(|| !service.snapshot().allocation_paused).await;
    let mut holder = Client::ready(&service).await;
    let first = holder.acquire(by_uuid(&["GPU-static-0"])).await;
    if is_queued(&first) {
        assert!(is_granted(&holder.recv().await));
    } else {
        assert!(is_granted(&first));
    }

    let changes = service
        .reconfigure(&service.config(), Some(&uuids(&["GPU-static-1"])))
        .unwrap();
    assert_eq!(changes.retiring, ["GPU-static-0"]);
    // A broken GPU that a hung job still holds must not keep failing every
    // observation.
    let before = gate.calls();
    eventually(|| gate.calls() > before + 1).await;
    assert_eq!(gate.last(), ["GPU-static-1"]);
    assert!(service.with_scheduler(|s| s.is_held(Slot(0))));
}

#[test]
fn log_throttle_admits_one_line_per_uid_and_kind_per_window() {
    let mut throttle = LogThrottle::default();
    let start = Instant::now();
    let later = start + NOISE_WINDOW;
    assert_eq!(
        throttle.admit(Some(1000), Noise::HandshakeTimeout, start),
        Some(0)
    );
    assert_eq!(
        throttle.admit(Some(1000), Noise::HandshakeTimeout, start),
        None
    );
    assert_eq!(
        throttle.admit(Some(1000), Noise::HandshakeTimeout, start),
        None
    );
    // Other uids and other kinds have their own windows.
    assert_eq!(
        throttle.admit(Some(1001), Noise::HandshakeTimeout, start),
        Some(0)
    );
    assert_eq!(
        throttle.admit(Some(1000), Noise::ProtocolViolation, start),
        Some(0)
    );
    // The first line after the window reports what was dropped.
    assert_eq!(
        throttle.admit(Some(1000), Noise::HandshakeTimeout, later),
        Some(2)
    );
    assert_eq!(
        throttle.admit(Some(1000), Noise::HandshakeTimeout, later),
        None
    );
}

#[test]
fn a_throttled_line_says_how_many_were_left_out() {
    assert_eq!(with_suppressed("x", 0), "x");
    assert_eq!(with_suppressed("x", 1), "x (1 similar line not logged)");
    assert_eq!(with_suppressed("x", 12), "x (12 similar lines not logged)");
}

#[test]
fn log_throttle_shares_one_window_once_too_many_uids_are_tracked() {
    // A client cycling through uids must not grow the map without bound.
    let mut throttle = LogThrottle::default();
    let now = Instant::now();
    for uid in 0..NOISE_MAX_KEYS as u32 {
        assert_eq!(
            throttle.admit(Some(uid), Noise::ProtocolViolation, now),
            Some(0)
        );
    }
    assert_eq!(
        throttle.admit(Some(u32::MAX - 1), Noise::ProtocolViolation, now),
        Some(0)
    );
    assert_eq!(
        throttle.admit(Some(u32::MAX - 2), Noise::ProtocolViolation, now),
        None
    );
    assert!(throttle.windows.len() <= NOISE_MAX_KEYS + 1);
    // Expired windows are pruned before falling back.
    let later = now + NOISE_WINDOW;
    assert_eq!(
        throttle.admit(Some(u32::MAX - 3), Noise::ProtocolViolation, later),
        Some(0)
    );
    assert!(throttle.windows.len() <= 2);
}

#[tokio::test]
async fn over_limit_warnings_from_one_user_are_throttled() {
    // A connect loop over the per-user limit must not flood the log and
    // push out other users' lines.
    let (sink, events) = recording_log();
    let service = Service::start(
        Arc::new(StaticInventory::with_count(1)),
        None,
        ServiceConfig {
            max_connections_per_uid: 1,
            ..test_config(scratch_socket())
        },
        sink,
    )
    .unwrap();
    let holder = Client::ready(&service).await;
    for _ in 0..5 {
        let mut extra = Client::connect(&service).await;
        assert_eq!(
            deny_reason(&extra.recv().await),
            Some(DenyReason::INVALID_REQUEST)
        );
    }
    let warnings = events
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, message)| message.contains("over its per-user connection limit"))
        .count();
    assert_eq!(warnings, 1);
    drop(holder);
}

#[tokio::test]
async fn a_kernel_without_child_lists_is_reported_at_startup() {
    // The fake procfs has no thread-self/children, as on such a kernel.
    let (sink, events) = recording_log();
    let (_service, _source) = observed_service_logging(Ok(vec![]), sink);
    assert!(events.lock().unwrap().iter().any(|(level, message)| {
        *level == LogLevel::Access
            && message == "this kernel does not list child processes; processes a job leaves behind may show as not managed"
    }));
}

#[tokio::test]
async fn an_unreadable_procfs_is_not_reported_as_a_kernel_without_child_lists() {
    // The observation reports `cannot read procfs`; the startup line about
    // child lists would only mislead.
    let (sink, events) = recording_log();
    let mut config = test_config(scratch_socket());
    config.watch_unmanaged_ms = 20;
    let service = Service::start_with_observer(
        Arc::new(StaticInventory::with_count(1)),
        config,
        sink,
        Arc::new(ControlledSource(Arc::new(Mutex::new(Ok(vec![]))))),
        ProcFs::new("/nonexistent-procfs-root"),
    )
    .unwrap();
    eventually(|| !service.snapshot().observation_error.is_empty()).await;
    assert!(!events
        .lock()
        .unwrap()
        .iter()
        .any(|(_, message)| message.contains("does not list child processes")));
}
