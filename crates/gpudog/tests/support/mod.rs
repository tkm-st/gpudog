//! Isolated driver doubles for real CLI subprocesses, never a production
//! fallback or environment-variable bypass in gpudog itself.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn uuid(index: u32) -> String {
    format!("GPU-00000000-0000-0000-0000-{index:012x}")
}

pub fn driver_directory() -> &'static Path {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY.get_or_init(|| build_driver(false))
}

pub fn missing_uuid_directory() -> &'static Path {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY.get_or_init(|| build_driver(true))
}

fn build_driver(missing_uuid: bool) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("gpudog");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    remove_stale_drivers(&root.join("../../tmp"));
    let directory = root.join(format!(
        "../../tmp/mock-driver-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let directory = directory.canonicalize().unwrap();
    let library = directory.join("libmock-driver.so");
    let mut compiler = std::process::Command::new("rustc");
    compiler.args(["--edition=2021", "--crate-type=cdylib", "-O"]);
    if missing_uuid {
        compiler.args(["--cfg", "mock_missing_uuid"]);
    }
    let output = compiler
        .arg(root.join("tests/support/mock_driver.rs"))
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in ["libcuda.so", "libcuda.so.1", "libnvidia-ml.so.1"] {
        std::os::unix::fs::symlink(&library, directory.join(name)).unwrap();
    }
    directory
}

/// Removes the drivers of test processes that have exited. A test process
/// cannot remove its own driver: it is in use until the process ends.
/// Assumes every run sharing this `tmp/` is in one PID namespace; a run in
/// another namespace (a container with the checkout mounted) would look
/// exited and lose its driver.
fn remove_stale_drivers(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("mock-driver-")) else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if !Path::new(&format!("/proc/{pid}")).exists() {
            // Another test binary may be removing it too.
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
