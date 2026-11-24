//! `gpudog`: run a process while holding an exclusive GPU reservation.

mod cli;
mod common;
mod dash;
mod health;
mod hold_limit;
mod idle_stop;
mod run;

use std::process::ExitCode;

use clap::Parser;
use gpudog_client::cli_options::RunOptions;

use cli::{Cli, Command};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run {
            socket,
            gpus,
            num_gpus,
            timeout,
            command,
        } => {
            let options = RunOptions {
                num_gpus,
                gpus,
                timeout,
                // Display text only; the child gets the exact bytes.
                command: command
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
            };
            run::run(&socket.socket, &command, options).await
        }
        Command::Health { socket } => health::health(&socket.socket).await,
        Command::Dash {
            socket,
            refresh,
            no_gpu_processes,
        } => dash::dash(&socket.socket, refresh, no_gpu_processes).await,
    }
}
