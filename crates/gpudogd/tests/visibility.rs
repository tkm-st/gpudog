//! Real daemon startup with isolated CUDA/NVML libraries. No GPU required.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use gpudog_client::client::Client;
use tokio::process::Command;

#[path = "../../gpudog/tests/support/mod.rs"]
mod support;

fn scratch() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../tmp/visibility-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.canonicalize().unwrap()
}

/// Runs a daemon that is expected to refuse to start, with a time limit so a
/// daemon that starts after all cannot hang the test.
async fn refused(mut command: Command) -> String {
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("gpudogd neither started nor exited")
        .unwrap();
    assert!(!output.status.success());
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn daemon(root: &Path, config: &str) -> Command {
    let file = root.join("config.toml");
    std::fs::write(&file, config).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gpudogd"));
    command
        .arg("--config")
        .arg(file)
        .arg("--socket")
        .arg(root.join("d.sock"))
        .env("LD_LIBRARY_PATH", support::driver_directory())
        .env("CUDA_DEVICE_ORDER", "PCI_BUS_ID")
        .env_remove("CUDA_VISIBLE_DEVICES")
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn daemon_manages_only_cuda_visible_uuids_in_nvml_order() {
    let root = scratch();
    let mut command = daemon(&root, "");
    command.env("CUDA_VISIBLE_DEVICES", "2,0");
    let mut child = command.spawn().unwrap();
    let mut connected = None;
    // As long as daemon.rs allows: a loaded runner can be slow to start it.
    for _ in 0..2000 {
        if let Ok(client) = Client::connect(&root.join("d.sock")).await {
            connected = Some(client);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut client = connected.expect("daemon did not start");
    let status = client.status().await.unwrap();
    assert_eq!(status.device_count, 2);
    assert_eq!(status.devices[0].uuid, support::uuid(0).as_bytes());
    assert_eq!(status.devices[1].uuid, support::uuid(2).as_bytes());
    drop(client);
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn configured_uuid_outside_cuda_view_prevents_startup() {
    let root = scratch();
    let mut command = daemon(&root, &format!("devices = [\"{}\"]", support::uuid(0)));
    command.env("CUDA_VISIBLE_DEVICES", "1");
    assert!(refused(command).await.contains("was not found"));
    assert!(!root.join("d.sock").exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn mig_enabled_or_unobservable_devices_prevent_startup() {
    for flag in ["MOCK_MIG_ENABLED", "MOCK_MIG_FAILURE"] {
        let root = scratch();
        let mut command = daemon(&root, "");
        command.env(flag, "1");
        assert!(refused(command).await.contains("MIG"));
        assert!(!root.join("d.sock").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}

#[tokio::test]
async fn missing_cuda_symbols_prevent_startup_without_fallback() {
    let root = scratch();
    let mut command = daemon(&root, "");
    command.env("LD_LIBRARY_PATH", support::missing_uuid_directory());
    let stderr = refused(command).await;
    assert!(
        stderr.contains("CUDA driver unavailable or missing required symbols"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!root.join("d.sock").exists());
    std::fs::remove_dir_all(&root).unwrap();
}
