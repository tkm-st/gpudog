//! Parsing of `gpudog run` options before CUDA view resolution.
//!
//! Pure: no I/O, so every rejection (each is a message a user will read) is
//! unit-testable without a daemon. Errors are CLI-ready sentences without
//! the `gpudog: ` prefix.

use crate::protocol::{
    is_valid_gpu_uuid, same_uuid, sanitize_display_text, Acquire, GpuSelection, MAX_COMMAND_SIZE,
    MAX_GPU_COUNT, MAX_TIMEOUT_MS,
};

const SECOND: u64 = 1000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// The `gpudog run` options as the command line gave them. `num_gpus` and
/// `gpus` are `Option` because "absent" and "explicitly the default" differ:
/// they are mutually exclusive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// `-n,--num_gpus`. Absent means one GPU unless `gpus` is set.
    pub num_gpus: Option<u32>,
    /// `-g,--gpus`, unparsed, e.g. `"0,2,3"` or `"GPU-8f6a...,GPU-1c2d..."`.
    pub gpus: Option<String>,
    /// `-t,--timeout`, unparsed, e.g. `"30s"`. Absent means wait forever.
    pub timeout: Option<String>,
    /// The command about to run, for the dashboard and the access log.
    pub command: Vec<String>,
}

/// Trims ASCII whitespace: space, tab, newline, carriage return, form
/// feed and vertical tab.
fn trim(text: &str) -> &str {
    text.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b'))
}

/// Parses unsigned decimal digits, refusing anything above `limit` without
/// overflowing on the way there.
fn parse_unsigned(text: &str, limit: u64) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.bytes().try_fold(0u64, |value, digit| {
        value
            .checked_mul(10)?
            .checked_add(u64::from(digit - b'0'))
            .filter(|&v| v <= limit)
    })
}

/// True if `entry` starts with `prefix` (four characters ending in `-`),
/// compared case-insensitively because CUDA does.
fn has_prefix(entry: &str, prefix: &str) -> bool {
    entry
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn is_gpu_uuid_form(entry: &str) -> bool {
    has_prefix(entry, "GPU-")
}

fn is_mig_form(entry: &str) -> bool {
    has_prefix(entry, "MIG-")
}

const EMPTY_LIST: &str = "--gpus needs at least one CUDA number or UUID, for example --gpus 0,1";
const MIX_ERROR: &str = "--gpus cannot mix CUDA numbers and UUIDs; use one form for the whole list";

fn mig_error(entry: &str) -> String {
    format!("--gpus does not support MIG devices yet ('{entry}')")
}

/// Splits a comma-separated list into trimmed entries, or reports an empty
/// entry (including the one a trailing comma leaves) before looking at any
/// entry's content.
fn split_list(text: &str) -> Result<Vec<&str>, String> {
    let list = trim(text);
    if list.is_empty() {
        return Err(EMPTY_LIST.into());
    }
    let entries: Vec<&str> = list.split(',').map(trim).collect();
    if entries.iter().any(|entry| entry.is_empty()) {
        return Err(
            "--gpus has an empty entry; the expected form is a comma-separated list like 0,1,2"
                .into(),
        );
    }
    Ok(entries)
}

/// Parses a `CUDA_VISIBLE_DEVICES`-style list of indices, e.g. `"0,2,3"`,
/// ignoring whitespace around the list and each entry. Indices are the
/// caller's CUDA numbering, resolved by `VisibleDevices` before I/O. Duplicates are
/// rejected here, as well as by the codec, so the user is told which one.
///
/// A `GPU-<uuid>` entry is refused as mixing forms (a pure UUID list goes
/// to [`parse_gpu_uuid_list`]), and `MIG-<uuid>` as not supported yet.
pub fn parse_gpu_list(text: &str) -> Result<Vec<u32>, String> {
    let mut seen = [false; MAX_GPU_COUNT as usize];
    let mut indices = Vec::new();
    for entry in split_list(text)? {
        if is_mig_form(entry) {
            return Err(mig_error(entry));
        }
        if is_gpu_uuid_form(entry) {
            return Err(MIX_ERROR.into());
        }
        let index = parse_unsigned(entry, u64::from(MAX_GPU_COUNT - 1)).ok_or_else(|| {
            format!(
                "--gpus entry '{entry}' is not a CUDA number from 0 to {} or a GPU-<uuid>",
                MAX_GPU_COUNT - 1
            )
        })? as u32;
        if std::mem::replace(&mut seen[index as usize], true) {
            return Err(format!("--gpus lists GPU {index} more than once"));
        }
        indices.push(index);
    }
    Ok(indices)
}

/// Parses a list of `GPU-<uuid>`s, e.g. `"GPU-8f6a...,GPU-1c2d..."`. The
/// daemon resolves them; this only checks the form and duplicates
/// (case-insensitively, as CUDA compares them).
pub fn parse_gpu_uuid_list(text: &str) -> Result<Vec<String>, String> {
    let mut uuids: Vec<String> = Vec::new();
    for entry in split_list(text)? {
        if is_mig_form(entry) {
            return Err(mig_error(entry));
        }
        if !is_gpu_uuid_form(entry) {
            return Err(MIX_ERROR.into());
        }
        if !is_valid_gpu_uuid(entry) {
            return Err(format!(
                "--gpus entry '{}' is not a GPU UUID such as GPU-8f6a... (see nvidia-smi -L)",
                sanitize_display_text(entry.as_bytes())
            ));
        }
        if uuids.iter().any(|seen| same_uuid(seen, entry)) {
            return Err(format!("--gpus lists {entry} more than once"));
        }
        uuids.push(entry.to_owned());
    }
    Ok(uuids)
}

/// Parses a timeout into milliseconds: `ms`, `s`, `m`, `h` or `d`,
/// case-insensitively, and a bare number is seconds. Zero is rejected,
/// because on the wire it means "wait forever".
pub fn parse_timeout_ms(text: &str) -> Result<u64, String> {
    let value = trim(text);
    if value.is_empty() {
        return Err("--timeout needs a duration, for example 30s, 5m or 2h".into());
    }
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let (number, suffix) = value.split_at(digits);
    let suffix = trim(suffix);
    if number.is_empty() {
        return Err(format!(
            "--timeout value '{value}' is not a duration, for example 30s, 5m or 2h"
        ));
    }
    let unit = match suffix.to_ascii_lowercase().as_str() {
        "ms" => 1,
        "" | "s" => SECOND,
        "m" => MINUTE,
        "h" => HOUR,
        "d" => DAY,
        _ => {
            return Err(format!(
                "--timeout unit '{suffix}' is not one of ms, s, m, h, d"
            ))
        }
    };
    // Bounding the number by `MAX / unit` keeps the multiplication in range,
    // so an absurd value is a message, not a wrapped-around short timeout.
    let count = parse_unsigned(number, MAX_TIMEOUT_MS / unit).ok_or_else(|| {
        format!(
            "--timeout is too long; the maximum is {} days",
            MAX_TIMEOUT_MS / DAY
        )
    })?;
    match count * unit {
        0 => Err("--timeout must be greater than zero; omit --timeout to wait indefinitely".into()),
        milliseconds => Ok(milliseconds),
    }
}

/// Joins `command` into one line, truncated with `...` to
/// [`MAX_COMMAND_SIZE`] bytes so the daemon never rejects a request over a
/// purely cosmetic field. Truncation respects UTF-8 boundaries.
pub fn join_command(command: &[String]) -> Vec<u8> {
    const MARKER: &str = "...";
    // No separator until something has been written, so empty
    // leading arguments leave no stray space.
    let mut joined = String::new();
    for argument in command {
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(argument);
    }
    let limit = MAX_COMMAND_SIZE as usize;
    if joined.len() <= limit {
        return joined.into_bytes();
    }
    let mut end = limit - MARKER.len();
    while !joined.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = joined.as_bytes()[..end].to_vec();
    out.extend_from_slice(MARKER.as_bytes());
    out
}

/// Builds the request to send, or explains why the options cannot form one.
/// The result must be prepared with `device_visibility::VisibleDevices`
/// before encoding: local indices and empty count candidates are not wire requests.
pub fn build_acquire(options: &RunOptions) -> Result<Acquire, String> {
    if options.num_gpus.is_some() && options.gpus.is_some() {
        return Err(
            "--num_gpus and --gpus cannot be used together; --gpus already says how many GPUs to take".into(),
        );
    }
    let mut request = Acquire {
        command: join_command(&options.command),
        ..Acquire::default()
    };
    if let Some(gpus) = options
        .gpus
        .as_deref()
        .filter(|g| is_gpu_uuid_form(trim(g)))
    {
        request.uuids = parse_gpu_uuid_list(gpus)?;
        request.selection = GpuSelection::ExplicitUuids;
        request.num_gpus = request.uuids.len() as u32;
    } else if let Some(gpus) = &options.gpus {
        request.gpus = parse_gpu_list(gpus)?;
        request.selection = GpuSelection::ExplicitList;
        // The codec rejects a list whose size disagrees with `num_gpus`.
        request.num_gpus = request.gpus.len() as u32;
    } else {
        let count = options.num_gpus.unwrap_or(1);
        if count == 0 {
            return Err("--num_gpus must be at least 1".into());
        }
        if count > MAX_GPU_COUNT {
            return Err(format!("--num_gpus must be at most {MAX_GPU_COUNT}"));
        }
        request.num_gpus = count;
    }
    if let Some(timeout) = &options.timeout {
        request.timeout_ms = parse_timeout_ms(timeout)?;
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{encode, Message};

    /// Success means "a request the codec accepts", so every success is
    /// checked against `encode`.
    fn build(options: RunOptions) -> Acquire {
        let request = build_acquire(&options).unwrap();
        let uuids = if request.selection == GpuSelection::ExplicitUuids {
            request.uuids.clone()
        } else {
            (0..MAX_GPU_COUNT)
                .map(|i| format!("GPU-static-{i}"))
                .collect::<Vec<_>>()
        };
        let view = crate::device_visibility::VisibleDevices::from_uuids(uuids).unwrap();
        assert!(encode(&Message::from(view.prepare_acquire(&request).unwrap())).is_ok());
        request
    }

    fn reject(options: RunOptions) -> String {
        build_acquire(&options).unwrap_err()
    }

    fn gpus(text: &str) -> Vec<u32> {
        parse_gpu_list(text).unwrap()
    }

    fn reject_gpus(text: &str) -> String {
        parse_gpu_list(text).unwrap_err()
    }

    fn timeout(text: &str) -> u64 {
        parse_timeout_ms(text).unwrap()
    }

    fn reject_timeout(text: &str) -> String {
        parse_timeout_ms(text).unwrap_err()
    }

    fn with_gpus(text: &str) -> RunOptions {
        RunOptions {
            gpus: Some(text.into()),
            ..RunOptions::default()
        }
    }

    fn with_count(count: u32) -> RunOptions {
        RunOptions {
            num_gpus: Some(count),
            ..RunOptions::default()
        }
    }

    #[test]
    fn no_options_asks_for_one_gpu_and_no_timeout() {
        let request = build(RunOptions::default());
        assert_eq!(request.selection, GpuSelection::Count);
        assert_eq!(request.num_gpus, 1);
        assert!(request.gpus.is_empty());
        assert_eq!(request.timeout_ms, 0);
    }

    #[test]
    fn num_gpus_is_forwarded_as_a_count_request() {
        let request = build(with_count(4));
        assert_eq!(request.selection, GpuSelection::Count);
        assert_eq!(request.num_gpus, 4);
        assert!(request.gpus.is_empty());
    }

    #[test]
    fn num_gpus_of_zero_is_rejected_before_it_reaches_the_codec() {
        assert!(reject(with_count(0)).contains("at least 1"));
    }

    #[test]
    fn num_gpus_is_bounded_by_the_protocol_limit() {
        assert_eq!(build(with_count(MAX_GPU_COUNT)).num_gpus, MAX_GPU_COUNT);
        assert!(reject(with_count(MAX_GPU_COUNT + 1)).contains("at most"));
    }

    #[test]
    fn gpus_selects_an_explicit_list_and_sets_the_matching_count() {
        let request = build(with_gpus("0,2,3"));
        assert_eq!(request.selection, GpuSelection::ExplicitList);
        assert_eq!(request.gpus, [0, 2, 3]);
        assert_eq!(request.num_gpus, 3);
    }

    #[test]
    fn gpus_preserves_the_order_the_user_gave() {
        assert_eq!(gpus("3,1,2"), [3, 1, 2]);
    }

    #[test]
    fn gpus_tolerates_whitespace_around_the_list_and_its_entries() {
        assert_eq!(gpus("  0 , 1  "), [0, 1]);
    }

    #[test]
    fn a_single_gpu_index_is_a_valid_list() {
        assert_eq!(gpus("7"), [7]);
    }

    #[test]
    fn leading_zeros_in_a_gpu_index_are_accepted() {
        assert_eq!(gpus("007"), [7]);
    }

    #[test]
    fn an_empty_gpus_value_is_rejected() {
        assert!(reject_gpus("").contains("at least one"));
        assert!(reject_gpus("   ").contains("at least one"));
    }

    #[test]
    fn a_trailing_or_doubled_comma_in_gpus_is_rejected() {
        for text in ["0,", "0,,1", ",0"] {
            assert!(reject_gpus(text).contains("empty entry"), "{text}");
        }
    }

    #[test]
    fn a_non_numeric_gpu_entry_is_rejected_and_named() {
        assert!(reject_gpus("0,abc").contains("abc"));
    }

    #[test]
    fn a_negative_gpu_index_is_rejected_rather_than_wrapped() {
        assert!(reject_gpus("-1").contains("-1"));
    }

    #[test]
    fn a_gpu_index_at_or_past_the_protocol_bound_is_rejected() {
        assert_eq!(gpus(&(MAX_GPU_COUNT - 1).to_string()), [MAX_GPU_COUNT - 1]);
        assert!(reject_gpus(&MAX_GPU_COUNT.to_string()).contains("is not a CUDA number"));
        // Large enough to overflow a naive accumulator.
        assert!(reject_gpus("99999999999999999999").contains("is not a CUDA number"));
    }

    #[test]
    fn a_duplicated_gpu_index_is_rejected_and_named() {
        assert!(reject_gpus("1,2,1").contains("GPU 1 more than once"));
        assert!(reject_gpus("0,2,2").contains("GPU 2 more than once"));
    }

    #[test]
    fn gpus_by_uuid_selects_an_explicit_uuid_list() {
        let request = build(with_gpus(" GPU-1a2b3c4d , gpu-5E6F "));
        assert_eq!(request.selection, GpuSelection::ExplicitUuids);
        assert_eq!(request.num_gpus, 2);
        assert!(request.gpus.is_empty());
        assert_eq!(request.uuids, ["GPU-1a2b3c4d", "gpu-5E6F"]);
    }

    #[test]
    fn gpus_refuses_to_mix_indices_and_uuids_in_either_order() {
        assert!(reject_gpus("0,GPU-1a2b3c4d").contains("cannot mix"));
        assert!(parse_gpu_uuid_list("GPU-1a2b3c4d,0")
            .unwrap_err()
            .contains("cannot mix"));
        assert!(reject(with_gpus("GPU-1a2b3c4d,0")).contains("cannot mix"));
    }

    #[test]
    fn gpus_refuses_mig_devices_by_name() {
        assert_eq!(
            reject_gpus("MIG-1a2b3c4d"),
            "--gpus does not support MIG devices yet ('MIG-1a2b3c4d')"
        );
        assert!(parse_gpu_uuid_list("GPU-1a2b,MIG-1a2b3c4d")
            .unwrap_err()
            .contains("MIG"));
    }

    #[test]
    fn gpus_refuses_a_malformed_or_repeated_uuid() {
        assert_eq!(
            parse_gpu_uuid_list("GPU-a;b").unwrap_err(),
            "--gpus entry 'GPU-a;b' is not a GPU UUID such as GPU-8f6a... (see nvidia-smi -L)"
        );
        assert_eq!(
            parse_gpu_uuid_list("GPU-abc,gpu-ABC").unwrap_err(),
            "--gpus lists gpu-ABC more than once"
        );
    }

    #[test]
    fn num_gpus_and_gpus_together_are_rejected() {
        let options = RunOptions {
            num_gpus: Some(2),
            ..with_gpus("0,1")
        };
        assert!(reject(options).contains("cannot be used together"));
    }

    #[test]
    fn a_bare_timeout_is_seconds() {
        assert_eq!(timeout("30"), 30_000);
    }

    #[test]
    fn every_timeout_suffix_is_honoured() {
        assert_eq!(timeout("500ms"), 500);
        assert_eq!(timeout("30s"), 30_000);
        assert_eq!(timeout("5m"), 300_000);
        assert_eq!(timeout("2h"), 7_200_000);
        assert_eq!(timeout("1d"), 86_400_000);
    }

    #[test]
    fn timeout_units_are_case_insensitive() {
        assert_eq!(timeout("500MS"), 500);
        assert_eq!(timeout("30S"), 30_000);
        assert_eq!(timeout("2H"), 7_200_000);
        assert_eq!(timeout("1D"), 86_400_000);
    }

    #[test]
    fn whitespace_around_a_timeout_and_its_unit_is_tolerated() {
        assert_eq!(timeout(" 30 s "), 30_000);
    }

    #[test]
    fn an_unknown_timeout_unit_is_rejected_and_named() {
        let error = reject_timeout("30y");
        assert!(error.contains("'y'"));
        assert!(error.contains("ms, s, m, h, d"));
    }

    #[test]
    fn a_timeout_without_digits_is_rejected() {
        assert!(reject_timeout("abc").contains("not a duration"));
        assert!(reject_timeout("").contains("needs a duration"));
        assert!(reject_timeout("-5s").contains("not a duration"));
    }

    #[test]
    fn a_zero_timeout_is_rejected_because_zero_means_wait_forever() {
        assert!(reject_timeout("0").contains("greater than zero"));
        assert!(reject_timeout("0ms").contains("greater than zero"));
    }

    #[test]
    fn a_timeout_past_the_protocol_bound_is_rejected_not_wrapped() {
        assert_eq!(timeout(&format!("{MAX_TIMEOUT_MS}ms")), MAX_TIMEOUT_MS);
        assert!(reject_timeout(&format!("{}ms", MAX_TIMEOUT_MS + 1)).contains("too long"));
        let error = reject_timeout("50d");
        assert!(
            error.contains(&format!("{} days", MAX_TIMEOUT_MS / DAY)),
            "{error}"
        );
        // The multiplication by the unit must not overflow into a short
        // timeout.
        assert!(reject_timeout("999999999999999999999d").contains("too long"));
    }

    #[test]
    fn the_largest_accepted_timeout_still_encodes() {
        let options = RunOptions {
            timeout: Some("49d".into()),
            ..RunOptions::default()
        };
        assert!(build(options).timeout_ms <= MAX_TIMEOUT_MS);
    }

    #[test]
    fn options_combine_into_one_request() {
        let options = RunOptions {
            timeout: Some("5m".into()),
            ..with_gpus("1,0")
        };
        let request = build(options);
        assert_eq!(request.selection, GpuSelection::ExplicitList);
        assert_eq!(request.gpus, [1, 0]);
        assert_eq!(request.num_gpus, 2);
        assert_eq!(request.timeout_ms, 300_000);
    }

    #[test]
    fn the_command_is_joined_and_carried() {
        let options = RunOptions {
            command: vec!["python".into(), "train.py".into()],
            ..RunOptions::default()
        };
        assert_eq!(build(options).command, b"python train.py");
    }

    #[test]
    fn empty_leading_arguments_leave_no_stray_space() {
        assert_eq!(join_command(&["".into(), "x".into()]), b"x");
    }

    #[test]
    fn a_long_command_is_truncated_to_fit_the_wire() {
        let command = vec!["x".repeat(1000)];
        let joined = join_command(&command);
        assert_eq!(joined.len(), MAX_COMMAND_SIZE as usize);
        assert!(joined.ends_with(b"..."));
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // 'é' is two bytes; an odd offset would split one.
        let command = vec![format!("a{}", "é".repeat(300))];
        let joined = join_command(&command);
        assert!(joined.len() <= MAX_COMMAND_SIZE as usize);
        assert!(String::from_utf8(joined).is_ok());
    }
}
