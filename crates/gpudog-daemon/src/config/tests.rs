use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::scheduler::DEFAULT_MAX_QUEUED_PER_UID;

/// Applies `text` on top of the defaults, requiring success with no warnings.
fn loaded(text: &str) -> DaemonConfig {
    let mut config = DaemonConfig::default();
    let warnings = config.apply_text(text, "<config>").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    config
}

#[test]
fn unmanaged_watch_interval_is_positive_bounded_and_defaults_to_one_second() {
    assert_eq!(loaded("").service.watch_unmanaged_ms, 1000);
    assert_eq!(
        loaded("watch_unmanaged_ms = 25").service.watch_unmanaged_ms,
        25
    );
    assert!(loaded("watch_unmanaged_ms = 0").validate().is_err());
    assert!(!warnings("watch_unmanaged_ms = 3600001").is_empty());
    assert!(!warnings("watch_unmanaged_ms = false").is_empty());
}

/// Applies `text`, requiring success with at least one warning.
#[test]
fn the_hold_limit_defaults_to_none_and_is_bounded_by_the_wire() {
    assert_eq!(loaded("").service.max_hold_ms, 0);
    assert_eq!(
        loaded("max_hold_ms = 7200000").service.max_hold_ms,
        7_200_000
    );
    // 0 is no limit, not a refused value.
    let none = loaded("max_hold_ms = 0");
    assert_eq!(none.service.max_hold_ms, 0);
    assert!(none.validate().is_ok());
    assert_eq!(
        loaded("max_hold_ms = 4294967295").service.max_hold_ms,
        4_294_967_295
    );
    assert_eq!(
        warnings("max_hold_ms = 4294967296"),
        ["ignoring 'max_hold_ms': must be between 0 and 4294967295"]
    );
    assert_eq!(
        warnings("max_hold_ms = -1"),
        ["ignoring 'max_hold_ms': must be between 0 and 4294967295"]
    );
    assert_eq!(
        warnings("max_hold_ms = \"2h\""),
        ["ignoring 'max_hold_ms': expected an integer"]
    );
}

fn warnings(text: &str) -> Vec<String> {
    let warnings = DaemonConfig::default()
        .apply_text(text, "<config>")
        .unwrap();
    assert!(!warnings.is_empty());
    warnings
}

/// A unique scratch path under the workspace `tmp/`, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(text: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/test-config");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!(
            "config-{}-{}.toml",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self(path)
    }

    fn chmod(&self, mode: u32) {
        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn an_empty_config_leaves_every_default_in_place() {
    assert_eq!(loaded(""), DaemonConfig::default());
}

#[test]
fn log_directory_is_obsolete_but_still_accepted() {
    // An older config must still start the daemon; the key just warns.
    for value in [r#""/tmp/gpudog-logs""#, "5"] {
        let mut config = DaemonConfig::default();
        let warnings = config
            .apply_text(&format!("log_directory = {value}"), "<config>")
            .unwrap();
        assert_eq!(
            warnings,
            [format!(
                "ignoring 'log_directory': {LOG_DIRECTORY_OBSOLETE}"
            )]
        );
        assert_eq!(config, DaemonConfig::default());
    }
}

#[test]
fn a_config_sets_only_the_keys_it_mentions() {
    let mut expected = DaemonConfig::default();
    expected.service.scheduler_limits.max_queued_per_uid = 3;
    // Everything else untouched, or a one-line config would silently reset
    // the rest of the daemon's policy.
    assert_eq!(loaded("max_queued_per_uid = 3"), expected);
}

#[test]
fn the_connection_and_scheduler_caps_are_configurable() {
    let config = loaded(
        r#"
        max_connections = 32
        max_connections_per_uid = 4
        handshake_timeout_ms = 2500
        max_held_per_uid = 2
        max_queued_per_uid = 3
    "#,
    );
    assert_eq!(config.service.max_connections, 32);
    assert_eq!(config.service.max_connections_per_uid, 4);
    assert_eq!(
        config.service.handshake_timeout,
        Duration::from_millis(2500)
    );
    assert_eq!(config.service.scheduler_limits.max_held_per_uid, 2);
    assert_eq!(config.service.scheduler_limits.max_queued_per_uid, 3);
}

#[test]
fn the_socket_path_mode_and_directory_check_are_configurable() {
    let config = loaded(
        r#"
        socket_path = "/tmp/gpudog.sock"
        socket_mode = "0600"
        require_secure_directory = false
    "#,
    );
    assert_eq!(config.service.socket_path, Path::new("/tmp/gpudog.sock"));
    assert_eq!(config.service.socket_mode, 0o600);
    assert!(!config.service.require_secure_directory);
}

#[test]
fn socket_mode_is_octal_and_a_decimal_looking_value_is_still_octal() {
    assert_eq!(loaded(r#"socket_mode = "666""#).service.socket_mode, 0o666);
    assert_eq!(loaded(r#"socket_mode = "0666""#).service.socket_mode, 0o666);
    assert_eq!(loaded(r#"socket_mode = "0""#).service.socket_mode, 0);
}

#[test]
fn a_malformed_socket_mode_is_ignored_with_a_warning_not_applied() {
    let mut config = DaemonConfig::default();
    let warnings = config
        .apply_text(r#"socket_mode = "rw-rw-rw-""#, "<config>")
        .unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("socket_mode"));
    assert_eq!(
        config.service.socket_mode,
        ServiceConfig::default().socket_mode
    );
}

#[test]
fn a_socket_mode_beyond_0777_is_refused() {
    assert!(warnings(r#"socket_mode = "7777""#)[0].contains("socket_mode"));
}

#[test]
fn socket_mode_rejects_what_strtoul_would_have_tolerated() {
    // Deliberately stricter than `strtoul`: see `parse_socket_mode`.
    for text in [" 0666", "+0666", "-0", "0666 ", "", "8", "0o666"] {
        let toml = format!("socket_mode = {text:?}");
        assert_eq!(warnings(&toml).len(), 1, "{text:?} was accepted");
    }
}

#[test]
fn an_unknown_key_warns_and_is_otherwise_ignored() {
    let warnings = warnings("max_queued_per_uid = 4\nnot_a_setting = 1");
    assert_eq!(warnings, ["ignoring unknown setting 'not_a_setting'"]);
}

#[test]
fn a_misspelled_key_does_not_silently_take_effect() {
    let mut config = DaemonConfig::default();
    let warnings = config
        .apply_text("max_queued_per_uidd = 4", "<config>")
        .unwrap();
    assert_eq!(
        config.service.scheduler_limits.max_queued_per_uid,
        DEFAULT_MAX_QUEUED_PER_UID
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("max_queued_per_uidd"));
}

#[test]
fn a_key_of_the_wrong_type_warns_and_keeps_the_default() {
    let mut config = DaemonConfig::default();
    let warnings = config
        .apply_text(
            r#"
            max_queued_per_uid = "nineteen"
            require_secure_directory = 1
            socket_path = 5
        "#,
            "<config>",
        )
        .unwrap();
    // Typed keys warn in KNOWN_KEYS order.
    assert_eq!(
        warnings,
        [
            "ignoring 'socket_path': expected a string",
            "ignoring 'require_secure_directory': expected true or false",
            "ignoring 'max_queued_per_uid': expected an integer",
        ]
    );
    assert_eq!(config, DaemonConfig::default());
}

#[test]
fn a_table_is_an_unknown_setting_not_a_namespace() {
    // Only top-level keys are read.
    let warnings = warnings("[service]\nmax_queued_per_uid = 4");
    assert_eq!(warnings, ["ignoring unknown setting 'service'"]);
}

#[test]
fn unknown_keys_are_reported_alphabetically_after_typed_warnings() {
    let warnings = warnings("zeta = 1\nalpha = 2\nmax_queued_per_uid = \"x\"");
    assert_eq!(
        warnings,
        [
            "ignoring 'max_queued_per_uid': expected an integer",
            "ignoring unknown setting 'alpha'",
            "ignoring unknown setting 'zeta'",
        ]
    );
}

#[test]
fn an_unparsable_file_is_fatal_unlike_a_single_bad_key() {
    let mut config = DaemonConfig::default();
    let error = config
        .apply_text("max_queued_per_uid = = 4", "<config>")
        .unwrap_err();
    // The message must locate the problem, or the operator has to bisect the
    // file by hand.
    assert!(error.0.starts_with("<config>: "), "{error}");
    assert!(error.0.contains("(line 1, column"), "{error}");
    assert_eq!(config, DaemonConfig::default());
}

#[test]
fn a_parse_error_reports_the_right_line() {
    let error = DaemonConfig::default()
        .apply_text("max_queued_per_uid = 4\n\nbroken =\n", "<config>")
        .unwrap_err();
    assert!(error.0.contains("(line 3, column"), "{error}");
}

#[test]
fn a_parse_error_is_one_line() {
    for text in [
        "max_queued_per_uid = = 4",
        "broken =\n",
        "[a\nb = 1",
        "a = \"open\n",
        "a = 1\na = 2\n",
        "a = [1,\n",
    ] {
        let error = DaemonConfig::default()
            .apply_text(text, "<config>")
            .unwrap_err();
        assert!(!error.0.contains('\n'), "{text:?}: {error:?}");
    }
}

#[test]
fn a_duplicate_key_is_fatal() {
    assert!(DaemonConfig::default()
        .apply_text("max_queued_per_uid = 4\nmax_queued_per_uid = 5", "<config>")
        .is_err());
}

#[test]
fn zero_valued_caps_are_refused_rather_than_starting_a_useless_daemon() {
    for key in [
        "max_connections",
        "max_connections_per_uid",
        "handshake_timeout_ms",
        "max_held_per_uid",
        "max_queued_per_uid",
    ] {
        let config = loaded(&format!("{key} = 0"));
        let complaint = config.validate().unwrap_err();
        assert!(complaint.contains(key), "zero {key} gave {complaint:?}");
    }
}

#[test]
fn empty_paths_are_refused() {
    let mut config = DaemonConfig::default();
    config.service.socket_path = PathBuf::new();
    assert!(config.validate().unwrap_err().contains("socket_path"));
}

#[test]
fn paths_containing_nul_are_refused() {
    // Truncating at the NUL would bind a different path.
    let config = loaded(r#"socket_path = "/tmp/a\u0000b.sock""#);
    assert!(config.validate().unwrap_err().contains("socket_path"));
}

#[test]
fn the_defaults_satisfy_their_own_invariants() {
    // A daemon with no config file at all must start.
    assert_eq!(DaemonConfig::default().validate(), Ok(()));
}

#[test]
fn a_missing_config_file_is_not_an_error_and_changes_nothing() {
    let mut config = DaemonConfig::default();
    let load = config
        .apply_file(Path::new("tmp/there-is-no-config-here.toml"))
        .unwrap();
    assert_eq!(load, ConfigLoad::default());
    assert_eq!(config, DaemonConfig::default());
}

#[test]
fn a_config_file_on_disk_is_read() {
    let file = Scratch::new("max_queued_per_uid = 7\nmax_held_per_uid = 3\n");
    let mut config = DaemonConfig::default();
    let load = config.apply_file(&file.0).unwrap();
    assert!(load.found);
    assert!(load.warnings.is_empty());
    assert_eq!(config.service.scheduler_limits.max_queued_per_uid, 7);
    assert_eq!(config.service.scheduler_limits.max_held_per_uid, 3);
}

#[test]
fn only_ignored_values_count_as_invalid() {
    let file = Scratch::new(
        "max_queued_per_uid = \"high\"\nsocket_mode = \"rw\"\nlog_directory = \"/var/log\"\nmax_queued_per_uidd = 4\n",
    );
    let load = DaemonConfig::default().apply_file(&file.0).unwrap();
    assert_eq!(load.warnings.len(), 4, "{:?}", load.warnings);
    // The obsolete and the unknown key change nothing, so they do not count.
    assert_eq!(load.invalid_values, 2);

    let file = Scratch::new("log_directory = \"/var/log\"\nmax_queued_per_uidd = 4\n");
    assert_eq!(
        DaemonConfig::default()
            .apply_file(&file.0)
            .unwrap()
            .invalid_values,
        0
    );
}

#[test]
fn the_origin_of_a_parse_error_names_the_file() {
    let file = Scratch::new("this is not toml at all\n");
    let error = DaemonConfig::default().apply_file(&file.0).unwrap_err();
    assert!(error.0.contains(&file.0.display().to_string()), "{error}");
}

#[test]
fn a_config_file_that_is_not_utf8_is_fatal() {
    let file = Scratch::new("");
    std::fs::write(&file.0, b"max_queued_per_uid = 4 # \xff\n").unwrap();
    assert!(DaemonConfig::default().apply_file(&file.0).is_err());
}

#[test]
fn a_config_file_writable_by_others_is_refused() {
    let file = Scratch::new("max_queued_per_uid = 7\n");
    assert!(DaemonConfig::default().apply_file(&file.0).is_ok());

    // Group-writable is enough: this file sets socket_path and
    // require_secure_directory, so writing to it defeats both.
    file.chmod(0o620);
    let mut config = DaemonConfig::default();
    let error = config.apply_file(&file.0).unwrap_err();
    assert!(error.0.contains("writable by group"), "{error}");
    // Refused before anything was applied.
    assert_eq!(config, DaemonConfig::default());

    file.chmod(0o602);
    assert!(DaemonConfig::default().apply_file(&file.0).is_err());
}

#[test]
fn a_config_path_that_is_not_a_regular_file_is_refused() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
    let error = DaemonConfig::default().apply_file(directory).unwrap_err();
    assert!(error.0.contains("not a regular file"), "{error}");
}

#[test]
fn a_fifo_is_refused_without_blocking() {
    let file = Scratch::new("");
    std::fs::remove_file(&file.0).unwrap();
    nix::unistd::mkfifo(&file.0, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    let error = DaemonConfig::default().apply_file(&file.0).unwrap_err();
    assert!(error.0.contains("not a regular file"), "{error}");
}

#[test]
fn a_symlinked_config_is_followed_and_its_target_checked() {
    let target = Scratch::new("max_queued_per_uid = 7\n");
    let link = Scratch::new("");
    std::fs::remove_file(&link.0).unwrap();
    std::os::unix::fs::symlink(&target.0, &link.0).unwrap();

    let mut config = DaemonConfig::default();
    config.apply_file(&link.0).unwrap();
    assert_eq!(config.service.scheduler_limits.max_queued_per_uid, 7);

    target.chmod(0o622);
    assert!(DaemonConfig::default().apply_file(&link.0).is_err());
}

#[test]
fn the_file_accepts_no_value_the_command_line_would_refuse() {
    let mut config = DaemonConfig::default();
    let warnings = config
        .apply_text(
            "max_connections = 2000000000\nhandshake_timeout_ms = 999999999",
            "<config>",
        )
        .unwrap();
    assert_eq!(warnings.len(), 2);
    assert_eq!(config, DaemonConfig::default());
}

#[test]
fn an_oversized_config_file_is_refused_rather_than_read_into_memory() {
    let file = Scratch::new(&format!("# {}\n", "x".repeat(1024 * 1024)));
    let error = DaemonConfig::default().apply_file(&file.0).unwrap_err();
    assert!(error.0.contains("1 MiB"), "{error}");
}

#[test]
fn a_config_file_at_exactly_the_size_limit_is_still_read() {
    let key = "max_queued_per_uid = 7\n# ";
    let file = Scratch::new(&format!("{key}{}", "x".repeat(1024 * 1024 - key.len())));
    let mut config = DaemonConfig::default();
    config.apply_file(&file.0).unwrap();
    assert_eq!(config.service.scheduler_limits.max_queued_per_uid, 7);
}

#[test]
fn devices_is_unset_unless_the_file_names_it() {
    assert_eq!(loaded("").devices, None);
    assert_eq!(
        loaded("devices = [\"GPU-aaa\", \"GPU-bbb\"]\n").devices,
        Some(vec!["GPU-aaa".to_owned(), "GPU-bbb".to_owned()])
    );
}

#[test]
fn a_malformed_devices_list_is_fatal_never_a_fallback_to_every_gpu() {
    for (text, expected) in [
        (
            "devices = \"GPU-aaa\"\n",
            "devices must be an array of \"GPU-<uuid>\" strings",
        ),
        (
            "devices = []\n",
            "devices must list at least one GPU; omit it to manage every GPU",
        ),
        (
            "devices = [1]\n",
            "devices must be an array of \"GPU-<uuid>\" strings",
        ),
        (
            "devices = [\"0\"]\n",
            "devices entry '0' is not a GPU UUID such as \"GPU-8f6a...\" (see nvidia-smi -L)",
        ),
        (
            "devices = [\"MIG-aaa\"]\n",
            "devices entry 'MIG-aaa' is not a GPU UUID such as \"GPU-8f6a...\" (see nvidia-smi -L)",
        ),
        (
            "devices = [\"GPU-aaa\", \"gpu-AAA\"]\n",
            "devices lists gpu-AAA more than once",
        ),
    ] {
        let mut config = DaemonConfig::default();
        let error = config.apply_text(text, "<config>").unwrap_err();
        assert_eq!(error.0, format!("<config>: {expected}"), "{text}");
        assert_eq!(config, DaemonConfig::default(), "{text}");
    }
}

#[test]
fn a_reload_reports_changed_limits_and_keys_that_need_a_restart() {
    let old = DaemonConfig::default();
    assert!(old.limit_changes(&old).is_empty());
    assert!(old.restart_required_changes(&old).is_empty());

    let new = loaded(
        "max_held_per_uid = 2\nmax_hold_ms = 60000\nhandshake_timeout_ms = 500\nsocket_mode = \"0600\"\nsocket_path = \"/run/other.sock\"\n",
    );
    assert_eq!(
        old.limit_changes(&new),
        [
            "handshake_timeout_ms 10000 -> 500",
            "max_held_per_uid 8 -> 2",
            "max_hold_ms 0 -> 60000"
        ]
    );
    assert_eq!(
        old.restart_required_changes(&new),
        ["socket_path", "socket_mode"]
    );
}

/// The commented-out `config.toml` the Debian package installs.
const PACKAGED_TEMPLATE: &str = include_str!("../../../../packaging/config.toml");

#[test]
fn the_packaged_template_changes_nothing_and_its_commented_values_are_the_defaults() {
    assert_eq!(loaded(PACKAGED_TEMPLATE), DaemonConfig::default());

    // Uncomment every `# key = value` line; prose comments stay comments.
    let uncommented: String = PACKAGED_TEMPLATE
        .lines()
        .map(|line| match line.strip_prefix("# ") {
            Some(setting)
                if setting
                    .split_once(" = ")
                    .is_some_and(|(key, _)| KNOWN_KEYS.contains(&key)) =>
            {
                setting
            }
            _ => line,
        })
        .map(|line| format!("{line}\n"))
        .collect();
    let mut config = loaded(&uncommented);
    for key in KNOWN_KEYS {
        let shown = format!("\n{key} = ");
        assert_eq!(
            uncommented.contains(&shown),
            key != "log_directory",
            "{key} in the template"
        );
    }
    // `devices` has no default to show; its example must still be valid.
    assert_eq!(config.devices.take().map(|devices| devices.len()), Some(1));
    assert_eq!(config, DaemonConfig::default());
}

#[test]
fn the_removed_priority_settings_are_unknown_keys() {
    // The daemon orders its queue itself now; an old file still loads, and
    // each key just warns like any other unknown setting.
    let warnings = warnings("min_priority = -10\nmax_priority = 5");
    assert_eq!(
        warnings,
        [
            "ignoring unknown setting 'max_priority'",
            "ignoring unknown setting 'min_priority'",
        ]
    );
}
