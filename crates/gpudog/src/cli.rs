//! Command-line definitions for `gpudog`.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use gpudog_core::protocol::PROTOCOL_VERSION;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (protocol v8)");
const _: () = assert!(PROTOCOL_VERSION == 8, "update VERSION");

#[derive(Debug, Parser)]
#[command(
    name = "gpudog",
    version = VERSION,
    about = "GPUDog: run a process while holding an exclusive GPU reservation"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Args)]
pub(crate) struct Socket {
    /// Daemon socket to connect to
    #[arg(short, long, default_value = "/run/gpudog/gpudogd.sock")]
    pub(crate) socket: PathBuf,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a command under GPUDog's management
    Run {
        #[command(flatten)]
        socket: Socket,
        /// GPUs to take, as CUDA numbers (0,1) or UUIDs (GPU-...,GPU-...), comma separated (not with -n)
        #[arg(short, long, conflicts_with = "num_gpus")]
        gpus: Option<String>,
        /// Number of GPUs to take [default: 1] (not with -g)
        #[arg(short, long = "num_gpus", visible_alias = "num-gpus")]
        num_gpus: Option<u32>,
        /// How long to wait for GPUs, e.g. 30s, 5m, 2h [default: wait indefinitely]
        #[arg(short, long)]
        timeout: Option<String>,
        /// Command to run (optionally after --)
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_parser = clap::value_parser!(OsString)
        )]
        command: Vec<OsString>,
    },
    /// Check that the daemon is responding
    Health {
        #[command(flatten)]
        socket: Socket,
    },
    /// Show a live view of running and queued processes
    Dash {
        #[command(flatten)]
        socket: Socket,
        /// Seconds between refreshes
        #[arg(short, long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=3600))]
        refresh: u64,
        /// Hide daemon-observed GPU process rows; scheduling remains observation-aware
        #[arg(long)]
        no_gpu_processes: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("gpudog").chain(args.iter().copied()))
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_takes_the_command_with_or_without_a_double_dash() {
        for args in [
            &["run", "--", "python", "-u", "x.py"][..],
            &["run", "python", "-u", "x.py"][..],
        ] {
            let Command::Run { command, .. } = parse(args).unwrap().command else {
                unreachable!()
            };
            assert_eq!(command, ["python", "-u", "x.py"]);
        }
    }

    #[test]
    fn options_before_the_command_are_ours_and_after_it_the_commands() {
        let Command::Run {
            num_gpus, command, ..
        } = parse(&["run", "-n", "2", "train", "-n", "5"])
            .unwrap()
            .command
        else {
            unreachable!()
        };
        assert_eq!(num_gpus, Some(2));
        assert_eq!(command, ["train", "-n", "5"]);
    }

    #[test]
    fn there_is_no_priority_option() {
        // The daemon orders the queue itself. `-p` is no gpudog option, so,
        // like any other word that is not one, it starts the command.
        let Command::Run { command, .. } = parse(&["run", "-p", "5", "x"]).unwrap().command else {
            unreachable!()
        };
        assert_eq!(command, ["-p", "5", "x"]);
        let help = Cli::try_parse_from(["gpudog", "run", "--help"])
            .unwrap_err()
            .to_string();
        assert!(!help.contains("priority"), "{help}");
    }

    #[test]
    fn num_gpus_and_gpus_conflict_and_a_command_is_required() {
        assert!(parse(&["run", "-n", "1", "-g", "0", "x"]).is_err());
        assert!(parse(&["run", "-n", "1"]).is_err());
        assert!(parse(&["run", "--num_gpus", "2", "x"]).is_ok());
        assert!(parse(&["run", "--num-gpus", "2", "x"]).is_ok());
    }

    #[test]
    fn the_refresh_interval_is_bounded() {
        assert!(parse(&["dash", "-r", "0"]).is_err());
        assert!(parse(&["dash", "-r", "3600"]).is_ok());
        assert!(parse(&["dash", "-r", "3601"]).is_err());
    }

    #[test]
    fn reservation_only_dashboard_is_explicit_and_default_stays_full() {
        for (args, expected) in [
            (&["dash"][..], false),
            (&["dash", "--no-gpu-processes"][..], true),
        ] {
            let Command::Dash {
                no_gpu_processes, ..
            } = parse(args).unwrap().command
            else {
                unreachable!()
            };
            assert_eq!(no_gpu_processes, expected);
        }
    }
}
