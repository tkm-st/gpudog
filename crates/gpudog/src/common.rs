//! Connection errors and stderr output shared by CLI commands.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use gpudog_lib::client::{Client, ClientError};

/// Writes one `gpudog:` line to stderr. Unlike `eprintln!`, a failed write
/// is ignored rather than a panic: after a terminal hangup, stderr may be
/// gone while `gpudog` still waits (in the queue under `nohup`, or for the
/// command), and a panic would abandon the request or replace the command's
/// exit status with 101.
pub(crate) fn note(message: impl std::fmt::Display) {
    let _ = writeln!(std::io::stderr(), "gpudog: {message}");
}

pub(crate) fn complain(message: impl std::fmt::Display) -> ExitCode {
    note(message);
    ExitCode::FAILURE
}

pub(crate) async fn connect(path: &Path) -> Result<Client, ExitCode> {
    Client::connect(path).await.map_err(|error| match error {
        ClientError::Denied { .. } => complain(&error),
        _ => {
            note(format!(
                "cannot reach gpudogd at {}: {error}",
                path.display()
            ));
            if error.is_daemon_absent() {
                note("is gpudogd running?");
            }
            ExitCode::FAILURE
        }
    })
}
