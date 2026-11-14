//! Snapshot fetching and terminal redraws for `gpudog dash`.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use gpudog_lib::client::{Client, ClientError};
use gpudog_lib::dashboard;
use gpudog_lib::dashboard::{daemon_view, ProcessView};
use gpudog_lib::protocol::StatusReply;

/// Fetches one snapshot on a fresh connection. Not one held open: the
/// daemon caps connections per uid, and a dashboard parked for days would
/// spend one of the operator's own slots for nothing.
async fn fetch(socket: &Path) -> Result<StatusReply, ClientError> {
    Client::connect(socket).await?.status().await
}

/// Lists GPU processes for one frame; any failure becomes a note in the view.
fn gpu_processes(status: &StatusReply, hidden: bool) -> ProcessView {
    if hidden {
        ProcessView::Unavailable(
            "GPU process rows hidden (--no-gpu-processes); daemon observation remains enabled"
                .into(),
        )
    } else {
        daemon_view(status)
    }
}

/// Redraws until interrupted (Ctrl-C ends it with the default action).
pub(crate) async fn dash(socket: &Path, refresh: u64, no_gpu_processes: bool) -> ExitCode {
    let clear = std::io::stdout().is_terminal();
    loop {
        let view = match fetch(socket).await {
            Ok(status) => {
                dashboard::render(&status, &gpu_processes(&status, no_gpu_processes), refresh)
            }
            Err(error) => dashboard::render_error(&error.to_string(), refresh),
        };
        let mut out = std::io::stdout().lock();
        // Home and clear, then draw: one write, so the screen never shows a
        // half-drawn frame for long.
        let frame = if clear {
            format!("\x1b[H\x1b[2J{view}")
        } else {
            format!("{view}\n")
        };
        if out
            .write_all(frame.as_bytes())
            .and_then(|()| out.flush())
            .is_err()
        {
            // stdout went away (a closed pipe): nothing left to show.
            return ExitCode::SUCCESS;
        }
        drop(out);
        tokio::time::sleep(Duration::from_secs(refresh)).await;
    }
}
