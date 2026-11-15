//! `config.toml` for `gpudogd`, as described in `README.md`.
//!
//! The daemon is configured from three layers, lowest precedence first:
//! compiled-in defaults, then the config file, then explicit command-line
//! options. A missing config file is not an error (it means "keep the
//! defaults") because the daemon must start on a host where nobody has
//! written one.
//!
//! Existing files keep loading: the keys, types, bounds and the split
//! between fatal errors and warnings are a compatibility contract
//! (`tests/config_compat.rs`).

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use toml::{Table, Value};

use crate::service::ServiceConfig;
use gpudog_core::protocol::{
    is_valid_gpu_uuid, same_uuid, sanitize_display_text, MAX_HOLD_LIMIT_MS,
};

/// Everything `config.toml` can set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonConfig {
    pub service: ServiceConfig,
    /// `devices`: the `GPU-<uuid>`s this daemon manages. `None` means every
    /// device NVML reports. Never empty when set.
    pub devices: Option<Vec<String>>,
}

/// Warning for the obsolete `log_directory` key and `--log-directory` flag.
/// Still accepted, so an older config or unit file keeps starting the
/// daemon, but the daemon now logs to stderr for the systemd journal.
pub const LOG_DIRECTORY_OBSOLETE: &str =
    "gpudogd now logs to stderr (the systemd journal; see `journalctl -u gpudogd`)";

/// Where the daemon looks when `--config` is not given.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/gpudog/config.toml";

/// A config file that was applied, possibly with complaints.
///
/// `warnings` are non-fatal (an unknown key, a key of the wrong type) so a
/// config written for a newer build does not stop an older daemon starting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLoad {
    /// False when the file did not exist, in which case the config is
    /// untouched.
    pub found: bool,
    /// Ignored settings, one sentence each: typed keys in a fixed order, then
    /// unknown keys alphabetically.
    pub warnings: Vec<String>,
    /// How many of `warnings` are about a known key whose value was ignored
    /// (wrong type or out of range), so the key kept its previous value.
    /// Unknown and obsolete keys are not counted. A reload refuses a file
    /// with any, because the key would silently fall back to its default.
    pub invalid_values: usize,
}

/// The file exists but could not be used at all. Fatal: applying half a
/// config to a privileged daemon is worse than refusing to start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// Every key this build understands. Anything else earns a warning: a newer
/// config must not stop an older daemon, but a typo must not be silent.
const KNOWN_KEYS: [&str; 12] = [
    "log_directory",
    "socket_path",
    "socket_mode",
    "require_secure_directory",
    "max_connections",
    "max_connections_per_uid",
    "handshake_timeout_ms",
    "max_held_per_uid",
    "max_queued_per_uid",
    "devices",
    "watch_unmanaged_ms",
    "max_hold_ms",
];

const MAX_SOCKET_MODE: u32 = 0o777;

/// Upper bounds shared with the equivalent command-line options, so a value
/// refused as a flag is not quietly accepted from the file.
pub const MAX_CONNECTIONS_LIMIT: i64 = 1_000_000;
pub const MAX_HANDSHAKE_TIMEOUT_MS: i64 = 3_600_000;
/// One reservation per connection, so this can never usefully exceed the
/// connection cap.
pub const MAX_PER_UID_LIMIT: i64 = MAX_CONNECTIONS_LIMIT;

/// Largest config file worth reading. Without it `--config /dev/zero` reads
/// until the machine runs out of memory.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Applies one typed key at a time, recording a warning when the value has
/// the wrong type or range. Never fatal: the daemon keeps that key's default.
struct Reader<'a> {
    table: &'a Table,
    warnings: Vec<String>,
    /// Warnings about values, as opposed to obsolete keys.
    invalid_values: usize,
}

impl Reader<'_> {
    /// A known key whose value cannot be used.
    fn warn(&mut self, key: &str, why: &str) {
        self.note(key, why);
        self.invalid_values += 1;
    }

    /// A key that is accepted but has no effect.
    fn note(&mut self, key: &str, why: &str) {
        self.warnings.push(format!("ignoring '{key}': {why}"));
    }

    fn string(&mut self, key: &str) -> Option<&str> {
        match self.table.get(key)? {
            Value::String(value) => Some(value),
            _ => {
                self.warn(key, "expected a string");
                None
            }
        }
    }

    fn boolean(&mut self, key: &str) -> Option<bool> {
        match self.table.get(key)? {
            Value::Boolean(value) => Some(*value),
            _ => {
                self.warn(key, "expected true or false");
                None
            }
        }
    }

    /// `low..=high` bounds what the destination can hold, not what is
    /// sensible: that is [`DaemonConfig::validate`]'s job, and it must run
    /// after the command line has been merged in. `documented_low` is the
    /// minimum the documentation gives for the key, which the warning for a
    /// value outside `low..=high` names; it can be above `low` because
    /// `validate` refuses the values in between with its own error.
    fn integer(&mut self, key: &str, low: i64, documented_low: i64, high: i64) -> Option<i64> {
        let value = match self.table.get(key)? {
            Value::Integer(value) => *value,
            _ => {
                self.warn(key, "expected an integer");
                return None;
            }
        };
        if !(low..=high).contains(&value) {
            self.warn(key, &format!("must be between {documented_low} and {high}"));
            return None;
        }
        Some(value)
    }
}

/// Parses an octal permission string. A string rather than a TOML integer
/// because `socket_mode = 0666` is either a syntax error or decimal 666,
/// depending on the reader, and neither is what the author meant.
///
/// Stricter than `strtoul`, which also accepts leading whitespace, a sign,
/// and therefore `"-0"`.
fn parse_socket_mode(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    u32::from_str_radix(text, 8)
        .ok()
        .filter(|&mode| mode <= MAX_SOCKET_MODE)
}

/// 1-based line and column of byte `offset` in `text`.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |tail| tail.chars().count())
        + 1;
    (line, column)
}

/// Reads `devices`. Unlike every other key, a malformed value is fatal:
/// falling back to the default would make the daemon hand out every GPU on
/// the machine, which is the opposite of what was asked.
fn read_devices(table: &Table) -> Result<Option<Vec<String>>, String> {
    const NOT_AN_ARRAY: &str = "devices must be an array of \"GPU-<uuid>\" strings";
    let Some(node) = table.get("devices") else {
        return Ok(None);
    };
    let Value::Array(array) = node else {
        return Err(NOT_AN_ARRAY.to_owned());
    };
    if array.is_empty() {
        return Err("devices must list at least one GPU; omit it to manage every GPU".to_owned());
    }
    let mut uuids: Vec<String> = Vec::with_capacity(array.len());
    for element in array {
        let Value::String(value) = element else {
            return Err(NOT_AN_ARRAY.to_owned());
        };
        if !is_valid_gpu_uuid(value) {
            return Err(format!(
                "devices entry '{}' is not a GPU UUID such as \"GPU-8f6a...\" (see nvidia-smi -L)",
                sanitize_display_text(value.as_bytes())
            ));
        }
        if uuids.iter().any(|seen| same_uuid(seen, value)) {
            return Err(format!("devices lists {value} more than once"));
        }
        uuids.push(value.clone());
    }
    Ok(Some(uuids))
}

fn has_nul(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().contains(&0)
}

impl DaemonConfig {
    /// Applies a config file's *contents* on top of `self`, leaving keys the
    /// text does not mention alone. `origin` appears in messages. On error
    /// nothing is applied.
    pub fn apply_text(&mut self, text: &str, origin: &str) -> Result<Vec<String>, ConfigError> {
        self.apply_text_counted(text, origin)
            .map(|(warnings, _)| warnings)
    }

    /// [`Self::apply_text`], also returning [`ConfigLoad::invalid_values`].
    fn apply_text_counted(
        &mut self,
        text: &str,
        origin: &str,
    ) -> Result<(Vec<String>, usize), ConfigError> {
        let table: Table = text.parse().map_err(|error: toml::de::Error| {
            // Fatal, unlike a single bad key: if the file does not parse
            // there is no telling which settings were meant to apply.
            let (line, column) = line_and_column(text, error.span().map_or(0, |span| span.start));
            // One line, so a journal or terminal shows it as one message.
            let message: Vec<&str> = error
                .message()
                .lines()
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .collect();
            ConfigError(format!(
                "{origin}: {} (line {line}, column {column})",
                message.join("; ")
            ))
        })?;

        let devices = read_devices(&table)
            .map_err(|complaint| ConfigError(format!("{origin}: {complaint}")))?;
        if devices.is_some() {
            self.devices = devices;
        }

        let mut r = Reader {
            table: &table,
            warnings: Vec::new(),
            invalid_values: 0,
        };
        let service = &mut self.service;

        if r.table.contains_key("log_directory") {
            r.note("log_directory", LOG_DIRECTORY_OBSOLETE);
        }
        if let Some(value) = r.string("socket_path") {
            service.socket_path = value.into();
        }
        if let Some(value) = r.string("socket_mode") {
            match parse_socket_mode(value) {
                Some(mode) => service.socket_mode = mode,
                None => r.warn(
                    "socket_mode",
                    "expected an octal permission string such as \"0666\"",
                ),
            }
        }
        if let Some(value) = r.boolean("require_secure_directory") {
            service.require_secure_directory = value;
        }
        // The `as` casts below are lossless: `integer` has already bounded
        // each value to its destination's range. Each key is documented as
        // starting at 1; a 0 passes `integer` so that `validate` can refuse
        // it with its own error.
        if let Some(value) = r.integer("max_connections", 0, 1, MAX_CONNECTIONS_LIMIT) {
            service.max_connections = value as usize;
        }
        if let Some(value) = r.integer("max_connections_per_uid", 0, 1, MAX_CONNECTIONS_LIMIT) {
            service.max_connections_per_uid = value as usize;
        }
        if let Some(value) = r.integer("handshake_timeout_ms", 0, 1, MAX_HANDSHAKE_TIMEOUT_MS) {
            service.handshake_timeout = Duration::from_millis(value as u64);
        }
        if let Some(value) = r.integer(
            "watch_unmanaged_ms",
            0,
            1,
            crate::service::MAX_WATCH_UNMANAGED_MS as i64,
        ) {
            service.watch_unmanaged_ms = value as u64;
        }
        if let Some(value) = r.integer("max_held_per_uid", 0, 1, MAX_PER_UID_LIMIT) {
            service.scheduler_limits.max_held_per_uid = value as u32;
        }
        if let Some(value) = r.integer("max_queued_per_uid", 0, 1, MAX_PER_UID_LIMIT) {
            service.scheduler_limits.max_queued_per_uid = value as u32;
        }
        // 0 is the default, no limit; the bound is what `Granted` can carry.
        if let Some(value) = r.integer("max_hold_ms", 0, 0, MAX_HOLD_LIMIT_MS as i64) {
            service.max_hold_ms = value as u64;
        }

        let invalid_values = r.invalid_values;
        let mut warnings = r.warnings;
        // `Table` is ordered by key, so warnings come in key order.
        warnings.extend(
            table
                .keys()
                .filter(|key| !KNOWN_KEYS.contains(&key.as_str()))
                .map(|key| format!("ignoring unknown setting '{key}'")),
        );
        Ok((warnings, invalid_values))
    }

    /// Opens, checks and applies the file at `path` on top of `self`.
    ///
    /// A missing file is not an error: the daemon has to start on a host
    /// where nobody wrote one. The safety check (see [`check_opened`]) runs
    /// on the opened file, not the path, so it applies to exactly the bytes
    /// that are read, with no window between a check and a reopen.
    pub fn apply_file(&mut self, path: &Path) -> Result<ConfigLoad, ConfigError> {
        let unreadable = || {
            ConfigError(format!(
                "{}: cannot be read, or is larger than 1 MiB",
                path.display()
            ))
        };
        // Non-blocking so a FIFO cannot hang the open; it is then refused
        // as not a regular file. Symlinks are followed on purpose: a
        // symlinked config (e.g. a Kubernetes ConfigMap) is legitimate, and
        // the target is what gets checked.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OFlag::O_NONBLOCK.bits())
            .open(path)
        {
            Ok(file) => file,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(Errno::ENOTDIR as i32) =>
            {
                return Ok(ConfigLoad::default());
            }
            Err(_) => return Err(unreadable()),
        };
        check_opened(&file, path).map_err(ConfigError)?;

        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| unreadable())?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(unreadable());
        }
        let origin = path.display().to_string();
        let text = String::from_utf8(bytes)
            .map_err(|_| ConfigError(format!("{origin}: not valid UTF-8")))?;

        let (warnings, invalid_values) = self.apply_text_counted(&text, &origin)?;
        Ok(ConfigLoad {
            found: true,
            warnings,
            invalid_values,
        })
    }

    /// Checks the invariants `README.md` states, plus the ones the code
    /// depends on. Applied after the command line has been merged in, so a
    /// flag cannot reintroduce a combination the file was checked for.
    ///
    /// Unlike an unknown key, a well-typed but impossible value is fatal: it
    /// says exactly what the operator wanted, and it cannot be delivered.
    pub fn validate(&self) -> Result<(), String> {
        let service = &self.service;
        let limits = &service.scheduler_limits;
        let complaint = if service.max_connections == 0 {
            "max_connections must be at least 1"
        } else if service.max_connections_per_uid == 0 {
            "max_connections_per_uid must be at least 1"
        } else if service.handshake_timeout.is_zero() {
            "handshake_timeout_ms must be at least 1"
        } else if service.watch_unmanaged_ms == 0
            || service.watch_unmanaged_ms > crate::service::MAX_WATCH_UNMANAGED_MS
        {
            "watch_unmanaged_ms must be in 1..=3600000"
        } else if limits.max_held_per_uid == 0 {
            "max_held_per_uid must be at least 1"
        } else if limits.max_queued_per_uid == 0 {
            "max_queued_per_uid must be at least 1"
        } else if service.socket_path.as_os_str().is_empty() {
            "socket_path must not be empty"
        } else if has_nul(&service.socket_path) {
            // Truncating at the NUL would use a path that is not what the
            // file says, so it is refused.
            "socket_path must not contain a NUL character"
        } else {
            return Ok(());
        };
        Err(complaint.to_owned())
    }
}

impl DaemonConfig {
    /// Keys that differ from `self` in `new` but only take effect after a
    /// restart, in a fixed order. A reload reports and ignores them.
    pub fn restart_required_changes(&self, new: &Self) -> Vec<&'static str> {
        let (old, new_service) = (&self.service, &new.service);
        let mut changed = Vec::new();
        if old.socket_path != new_service.socket_path {
            changed.push("socket_path");
        }
        if old.socket_mode != new_service.socket_mode {
            changed.push("socket_mode");
        }
        if old.require_secure_directory != new_service.require_secure_directory {
            changed.push("require_secure_directory");
        }
        changed
    }

    /// The limits a reload changes, as `key old -> new`, in a fixed order:
    /// `max_connections`, `max_connections_per_uid`, `handshake_timeout_ms`,
    /// the scheduler caps, `max_hold_ms`, then `watch_unmanaged_ms`.
    pub fn limit_changes(&self, new: &Self) -> Vec<String> {
        let (old, new) = (&self.service, &new.service);
        let mut changes = Vec::new();
        let mut compare = |key: &str, before: String, after: String| {
            if before != after {
                changes.push(format!("{key} {before} -> {after}"));
            }
        };
        compare(
            "max_connections",
            old.max_connections.to_string(),
            new.max_connections.to_string(),
        );
        compare(
            "max_connections_per_uid",
            old.max_connections_per_uid.to_string(),
            new.max_connections_per_uid.to_string(),
        );
        compare(
            "handshake_timeout_ms",
            old.handshake_timeout.as_millis().to_string(),
            new.handshake_timeout.as_millis().to_string(),
        );
        compare(
            "max_held_per_uid",
            old.scheduler_limits.max_held_per_uid.to_string(),
            new.scheduler_limits.max_held_per_uid.to_string(),
        );
        compare(
            "max_queued_per_uid",
            old.scheduler_limits.max_queued_per_uid.to_string(),
            new.scheduler_limits.max_queued_per_uid.to_string(),
        );
        compare(
            "max_hold_ms",
            old.max_hold_ms.to_string(),
            new.max_hold_ms.to_string(),
        );
        compare(
            "watch_unmanaged_ms",
            old.watch_unmanaged_ms.to_string(),
            new.watch_unmanaged_ms.to_string(),
        );
        changes
    }
}

/// Checks that an opened config file is safe for a privileged daemon to use.
///
/// This file sets `socket_path`, `require_secure_directory` and every
/// per-user cap, so write access to it is write access to the daemon's whole
/// policy. Files owned by the running user pass, so the parser stays
/// testable without root.
fn check_opened(file: &std::fs::File, path: &Path) -> Result<(), String> {
    let shown = path.display();
    let info = file
        .metadata()
        .map_err(|_| format!("{shown}: cannot be read, or is larger than 1 MiB"))?;
    if !info.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    let euid = nix::unistd::geteuid().as_raw();
    if info.uid() != 0 && info.uid() != euid {
        return Err(format!(
            "{shown} is owned by uid {}, which is neither root nor this daemon",
            info.uid()
        ));
    }
    if info.mode() & 0o022 != 0 {
        return Err(format!(
            "{shown} is writable by group or others, which would let a local user set socket_path and \
             require_secure_directory"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
