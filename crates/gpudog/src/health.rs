//! Daemon liveness check for `gpudog health`.

use std::path::Path;
use std::process::ExitCode;

use crate::common::{complain, connect};

pub(crate) async fn health(socket: &Path) -> ExitCode {
    let mut client = match connect(socket).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client.ping().await {
        Ok(()) => {
            println!("gpudogd is responding on {}", socket.display());
            ExitCode::SUCCESS
        }
        Err(error) => complain(error),
    }
}
