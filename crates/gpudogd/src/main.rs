//! `gpudogd`: arbitrates exclusive GPU reservations for local users.
//!
//! Configuration has three layers, lowest precedence first: compiled-in
//! defaults, `config.toml`, then options actually given on the command line.
//! SIGHUP reloads `config.toml` (see [`reload`]). Logs go to stderr, for the
//! systemd journal.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use gpudog_core::protocol::{MAX_GPU_COUNT, PROTOCOL_VERSION};
use gpudog_daemon::config::{
    DaemonConfig, DEFAULT_CONFIG_PATH, LOG_DIRECTORY_OBSOLETE, MAX_CONNECTIONS_LIMIT,
    MAX_HANDSHAKE_TIMEOUT_MS,
};
use gpudog_daemon::daemon_log::DaemonLog;
use gpudog_daemon::gpu_inventory::{
    select_devices, GpuDevice, GpuInventory, NvmlInventory, StaticInventory,
};
use gpudog_daemon::service::{LogLevel, LogSink, Service, ShownDevice};
use tokio::signal::unix::{signal, SignalKind};

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (protocol v8)");
const _: () = assert!(PROTOCOL_VERSION == 8, "update VERSION");

/// Upper bound on each exit step: runtime shutdown, then draining the log.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Options that override `config.toml`. Every one is an `Option`, so "not
/// given" is distinguishable from "given the default value".
#[derive(Debug, Parser)]
#[command(name = "gpudogd", version = VERSION, about = "GPUDog daemon: arbitrates exclusive GPU reservations",
    after_help = "SIGHUP reloads the configuration file; see \"Changing the configuration\" in the administration guide.")]
struct Cli {
    /// TOML configuration file [default: /etc/gpudog/config.toml]
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Unix domain socket to listen on [default: /run/gpudog/gpudogd.sock]
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// Obsolete: logs now go to stderr (the systemd journal). Accepted and
    /// ignored so existing unit files keep working.
    #[arg(short = 'l', long, hide = true)]
    log_directory: Option<PathBuf>,

    /// Maximum concurrent client connections [default: 256]
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_CONNECTIONS_LIMIT as u64))]
    max_connections: Option<u64>,

    /// Milliseconds a client may take for each message before acquiring [default: 10000]
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_HANDSHAKE_TIMEOUT_MS as u64))]
    handshake_timeout_ms: Option<u64>,

    /// Bypass NVML and assume this many GPUs (testing)
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(0..=i64::from(MAX_GPU_COUNT)))]
    num_gpus: Option<u32>,

    /// Bind even if the socket directory is writable by others (unsafe: permits socket hijacking)
    #[arg(long)]
    allow_insecure_socket_directory: bool,
}

/// A fatal startup problem, already worded for the operator.
struct Fatal(String);

impl<T: Into<String>> From<T> for Fatal {
    fn from(message: T) -> Self {
        Self(message.into())
    }
}

/// Why the configuration is being loaded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Load {
    Startup,
    /// Stricter than startup: a running daemon already has a good
    /// configuration, so anything that would silently replace part of it
    /// with a default is refused instead.
    Reload,
}

/// Loads `config.toml` and merges the command line over it, then validates
/// the result, so a flag cannot reintroduce a combination the file alone
/// would have been rejected for.
fn settings(cli: &Cli, log: &LogSink, load: Load) -> Result<DaemonConfig, Fatal> {
    let path = cli
        .config
        .clone()
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.into());
    let mut settings = DaemonConfig::default();
    let loaded = settings.apply_file(&path).map_err(|error| error.0)?;
    for warning in &loaded.warnings {
        log(LogLevel::Warning, &format!("{}: {warning}", path.display()));
    }
    if load == Load::Reload {
        // Without the file every setting, `devices` included, would fall
        // back to its default: on a reload that means managing every GPU.
        if !loaded.found {
            return Err(format!("no such configuration file: {}", path.display()).into());
        }
        if loaded.invalid_values > 0 {
            return Err(format!(
                "{}: a setting has an invalid value (see the warning above)",
                path.display()
            )
            .into());
        }
    }
    // On a reload only "configuration reloaded" means the file was applied;
    // a "loaded" line before a later refusal would read as success.
    if loaded.found {
        if load == Load::Startup {
            log(
                LogLevel::Access,
                &format!("loaded configuration from {}", path.display()),
            );
        }
    } else if cli.config.is_some() {
        // A default path that does not exist is allowed; a path the operator
        // typed and that does not exist is a mistake worth stopping for.
        return Err(format!("no such configuration file: {}", path.display()).into());
    } else if load == Load::Startup {
        // Allowed, but said once: a file deleted or renamed by mistake would
        // otherwise silently reset `devices` and every per-user cap.
        log(
            LogLevel::Warning,
            &format!(
                "no configuration file at {}; using the defaults",
                path.display()
            ),
        );
    }

    let service = &mut settings.service;
    if let Some(socket) = &cli.socket {
        service.socket_path.clone_from(socket);
    }
    if cli.log_directory.is_some() {
        log(
            LogLevel::Warning,
            &format!("ignoring --log-directory: {LOG_DIRECTORY_OBSOLETE}"),
        );
    }
    if let Some(max) = cli.max_connections {
        // Bounded by clap to MAX_CONNECTIONS_LIMIT.
        service.max_connections = max as usize;
    }
    if let Some(ms) = cli.handshake_timeout_ms {
        service.handshake_timeout = Duration::from_millis(ms);
    }
    if cli.allow_insecure_socket_directory {
        service.require_secure_directory = false;
    }
    settings.validate()?;
    Ok(settings)
}

/// Every GPU the daemon can see: NVML unless explicitly overridden. A
/// daemon that cannot see its GPUs is useless, so failing to enumerate is
/// fatal rather than silently serving an empty machine.
///
/// The whole list is kept, so a reload can manage any of these GPUs; the
/// `devices` allow-list, when `config.toml` sets one, is checked against it
/// here and applied by the service. Enumeration happens only here: CUDA's
/// device list is fixed for the process's lifetime, so a GPU added later
/// needs a restart anyway.
fn inventory(
    cli: &Cli,
    allowed: Option<&[String]>,
    log: &LogSink,
) -> Result<Arc<dyn GpuInventory>, Fatal> {
    let (inventory, complaint): (Arc<dyn GpuInventory>, _) = match cli.num_gpus {
        Some(count) => {
            log(
                LogLevel::Warning,
                &format!("NVML bypassed; assuming {count} GPU(s)"),
            );
            (
                Arc::new(StaticInventory::with_count(count)),
                "cannot select GPUs",
            )
        }
        None => (
            Arc::new(
                NvmlInventory::probe()
                    .map_err(|error| format!("cannot enumerate GPUs: {error}"))?,
            ),
            "cannot enumerate GPUs",
        ),
    };
    let selected = select_devices(inventory.devices().to_vec(), allowed)
        .map_err(|error| format!("{complaint}: {error}"))?;
    for device in &selected {
        log(LogLevel::Access, &describe(device, None));
    }
    Ok(inventory)
}

/// One `gpu N ...` log line. `shown` adds the state a reload left it in.
fn describe(device: &GpuDevice, shown: Option<&ShownDevice>) -> String {
    let note = match shown {
        Some(shown) if shown.retiring && shown.held => "; removed, reserved until its job ends",
        // Released between the reload and this line: gone at the next one.
        Some(shown) if shown.retiring => "; removed",
        _ => "",
    };
    format!(
        "gpu {} {} ({}, nvml index {}{note})",
        device.index, device.name, device.uuid, device.nvml_index
    )
}

/// Applies `config.toml` again, on SIGHUP.
///
/// The limits and `devices` take effect from the next connection, request
/// or observation (see [`Service::reconfigure`]). Keys that need a restart
/// are reported and ignored. A file that cannot be used is reported and
/// changes nothing: the daemon keeps serving with the configuration it has.
fn reload(cli: &Cli, log: &LogSink, service: &Service, current: &mut DaemonConfig) {
    log(
        LogLevel::Access,
        "SIGHUP received; reloading the configuration",
    );
    let refuse = |message: &str| {
        log(
            LogLevel::Error,
            &format!("configuration reload failed; keeping the previous configuration: {message}"),
        );
    };
    let new = match settings(cli, log, Load::Reload) {
        Ok(new) => new,
        Err(Fatal(message)) => return refuse(&message),
    };
    let mut changes = current.limit_changes(&new);
    let before = service.devices();
    let devices = match service.reconfigure(&new.service, new.devices.as_deref()) {
        Ok(devices) => devices,
        Err(error) => {
            return refuse(&format!(
                "{error}; restart gpudogd to use a GPU added since it started"
            ))
        }
    };
    for key in current.restart_required_changes(&new) {
        log(
            LogLevel::Warning,
            &format!("{key} changed; restart gpudogd to apply it"),
        );
    }
    for uuid in &devices.added {
        log(LogLevel::Access, &format!("now managing {uuid}"));
    }
    for uuid in &devices.removed {
        log(LogLevel::Access, &format!("no longer managing {uuid}"));
    }
    for uuid in &devices.retiring {
        log(
            LogLevel::Access,
            &format!(
                "no longer managing {uuid} for new requests; its current reservation continues"
            ),
        );
    }
    // Daemon GPU numbers change only here, so list them whenever they did.
    let after = service.devices();
    let numbering = |list: &[ShownDevice]| -> Vec<(u32, String, bool)> {
        list.iter()
            .map(|shown| {
                (
                    shown.device.index,
                    shown.device.uuid.clone(),
                    shown.retiring,
                )
            })
            .collect()
    };
    let renumbered = numbering(&after) != numbering(&before);
    if renumbered {
        for shown in &after {
            log(LogLevel::Access, &describe(&shown.device, Some(shown)));
        }
    }
    if devices.changed() {
        changes.insert(0, "devices".to_owned());
    } else if renumbered {
        // A removed GPU's reservation ended since the last reload.
        changes.insert(0, "daemon GPU numbers".to_owned());
    }
    // The socket settings stay as they were at startup.
    current.service = service.config();
    current.devices = new.devices;
    log(
        LogLevel::Access,
        &if changes.is_empty() {
            "configuration reloaded; nothing changed".to_owned()
        } else {
            format!("configuration reloaded: {}", changes.join(", "))
        },
    );
}

async fn run(cli: Cli, log: LogSink) -> Result<(), Fatal> {
    // Installed first, so a signal during startup (reading the
    // configuration and probing NVML can take seconds) is not the default
    // action, which ends the daemon and, once bound, leaves the socket
    // behind. A SIGHUP that arrives meanwhile is handled once the daemon
    // serves.
    let mut terminate =
        signal(SignalKind::terminate()).map_err(|e| format!("cannot handle SIGTERM: {e}"))?;
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|e| format!("cannot handle SIGINT: {e}"))?;
    let mut hangup =
        signal(SignalKind::hangup()).map_err(|e| format!("cannot handle SIGHUP: {e}"))?;
    // First, so the log of a daemon an upgrade left running names its version.
    log(LogLevel::Access, &format!("gpudogd {VERSION} starting"));

    let mut current = settings(&cli, &log, Load::Startup)?;
    let inventory = inventory(&cli, current.devices.as_deref(), &log)?;

    let mut service = Service::start(
        inventory,
        current.devices.as_deref(),
        current.service.clone(),
        log.clone(),
    )
    .map_err(|error| format!("startup failed: {}", error.0))?;

    let name = loop {
        tokio::select! {
            _ = terminate.recv() => break "SIGTERM",
            _ = interrupt.recv() => break "SIGINT",
            _ = hangup.recv() => reload(&cli, &log, &service, &mut current),
        }
    };
    // Stopping removes the socket so clients fail fast instead of pointing
    // at a dead daemon. Live sessions end when the runtime is dropped, and
    // their guards release every reservation on the way out.
    log(LogLevel::Access, &format!("{name} received; shutting down"));
    service.stop();
    log(LogLevel::Access, "gpudogd stopped");
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let logger = match DaemonLog::stderr() {
        Ok(logger) => logger,
        Err(error) => {
            // No logger yet, so straight to stderr.
            eprintln!("gpudogd: error: cannot start the log writer thread: {error}");
            return ExitCode::FAILURE;
        }
    };
    let log = logger.sink();
    let code = match tokio::runtime::Runtime::new() {
        Err(error) => {
            log(
                LogLevel::Error,
                &format!("cannot start the async runtime: {error}"),
            );
            ExitCode::FAILURE
        }
        Ok(runtime) => {
            let result = runtime.block_on(run(cli, log.clone()));
            // Drops every live session task; their guards release
            // reservations. Bounded, so a stuck task cannot hold up exit.
            runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(Fatal(message)) => {
                    log(LogLevel::Error, &message);
                    ExitCode::FAILURE
                }
            }
        }
    };
    // The last lines (often the reason for a failure) must reach the journal.
    logger.flush(SHUTDOWN_TIMEOUT);
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("gpudogd").chain(args.iter().copied()))
    }

    #[test]
    fn option_bounds_match_the_config_file_bounds() {
        assert!(parse(&["--max-connections", "0"]).is_err());
        assert!(parse(&["--max-connections", "1000000"]).is_ok());
        assert!(parse(&["--max-connections", "1000001"]).is_err());
        assert!(parse(&["--handshake-timeout-ms", "0"]).is_err());
        assert!(parse(&["--handshake-timeout-ms", "3600001"]).is_err());
        assert!(parse(&["-n", "1024"]).is_ok());
        assert!(parse(&["-n", "1025"]).is_err());
        assert!(parse(&["-n", "-1"]).is_err());
    }

    #[test]
    fn the_obsolete_log_directory_flag_is_still_accepted() {
        assert!(parse(&["-l", "/var/log/gpudog"]).is_ok());
        assert!(parse(&["--log-directory", "/var/log/gpudog"]).is_ok());
    }
}
