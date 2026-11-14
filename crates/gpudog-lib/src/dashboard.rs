//! Terminal rendering of a status snapshot, for `gpudog dash`.
//!
//! The dashboard is terminal only (a user decision).
//! Every string from the daemon is sanitised again here, even though the
//! daemon sanitises `command` at its boundary: the snapshot could come from
//! a hostile daemon, and an escape sequence in a terminal can rewrite the
//! screen, set the window title, or worse. Output is plain ASCII.

use std::fmt::Write;

use crate::gpu_processes::{Ownership, ProcessRow, ProcessView};
use crate::protocol::{sanitize_display_text, ReservationInfo, StatusReply};

/// A millisecond age as a human reads it at a glance: `42s`, `3m 7s`,
/// `2h 5m`, `1d 4h`.
pub fn human_age(age_ms: u64) -> String {
    const SECOND: u64 = 1000;
    const MINUTE: u64 = 60 * SECOND;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if age_ms >= DAY {
        format!("{}d {}h", age_ms / DAY, (age_ms % DAY) / HOUR)
    } else if age_ms >= HOUR {
        format!("{}h {}m", age_ms / HOUR, (age_ms % HOUR) / MINUTE)
    } else if age_ms >= MINUTE {
        format!("{}m {}s", age_ms / MINUTE, (age_ms % MINUTE) / SECOND)
    } else {
        format!("{}s", age_ms / SECOND)
    }
}

/// A byte count as `512KiB`, `1.5GiB`; `-` when unknown.
pub fn human_bytes(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "-".to_owned();
    };
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn join(values: &[u32]) -> String {
    values
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Left-aligned columns, two spaces apart, the last column unpadded.
fn table(out: &mut String, header: &[&str], rows: &[Vec<String>], empty: &str) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: Vec<String>| -> String {
        let last = cells.len().saturating_sub(1);
        let text: Vec<String> = cells
            .into_iter()
            .enumerate()
            .map(|(i, cell)| {
                // A cell beyond the header has no width to pad to.
                match widths.get(i) {
                    Some(&width) if i != last => format!("{cell:<width$}"),
                    _ => cell,
                }
            })
            .collect();
        format!("  {}\n", text.join("  ").trim_end())
    };
    out.push_str(&line(header.iter().map(|h| h.to_string()).collect()));
    if rows.is_empty() {
        let _ = writeln!(out, "  ({empty})");
    }
    for row in rows {
        out.push_str(&line(row.clone()));
    }
}

fn reservation_rows(list: &[ReservationInfo], queued: bool) -> Vec<Vec<String>> {
    list.iter()
        .map(|info| {
            let mut row = Vec::new();
            if queued {
                row.push(info.position.to_string());
                row.push(info.num_gpus.to_string());
            } else {
                row.push(join(&info.gpus));
            }
            row.extend([
                info.uid.to_string(),
                info.pid.to_string(),
                human_age(info.age_ms),
                sanitize_display_text(&info.command),
            ]);
            row
        })
        .collect()
}

fn process_rows(rows: &[ProcessRow]) -> Vec<Vec<String>> {
    let unknown = || "?".to_owned();
    rows.iter()
        .map(|row| {
            vec![
                row.gpu.map_or_else(unknown, |gpu| gpu.to_string()),
                row.pid.to_string(),
                row.uid.map_or_else(unknown, |uid| uid.to_string()),
                human_bytes(row.used_memory),
                row.ownership.label().to_owned(),
                row.command
                    .as_deref()
                    .map_or_else(unknown, sanitize_display_text),
            ]
        })
        .collect()
}

/// The whole dashboard for one snapshot, newline-terminated.
pub fn render(status: &StatusReply, processes: &ProcessView, refresh_seconds: u64) -> String {
    let busy: std::collections::HashSet<u32> = status
        .holders
        .iter()
        .flat_map(|h| h.gpus.iter())
        .chain(status.busy_gpus.iter())
        .copied()
        .filter(|&gpu| gpu < status.device_count)
        .collect();
    // A hostile or confused daemon could report more held than exist.
    let stale = status.observing
        && (status
            .observation_age_ms
            .is_none_or(|age| age > status.watch_unmanaged_ms)
            || !status.observation_error.is_empty());
    let free = if stale || status.truncated {
        "unknown".to_owned()
    } else {
        u64::from(status.device_count)
            .saturating_sub(busy.len() as u64)
            .to_string()
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "GPUDog: {} GPU{}, {free} free, {} waiting (refreshes every {refresh_seconds}s)",
        status.device_count,
        if status.device_count == 1 { "" } else { "s" },
        status.waiting.len(),
    );
    if status.truncated {
        out.push_str(
            "\nNote: this machine has more entries than fit in one status message, \
             so the tables below are incomplete.\n",
        );
    }
    if status.observing {
        if !status.observation_error.is_empty() {
            let _ = writeln!(
                out,
                "\nGPU observation failed; new grants paused: {}",
                sanitize_display_text(&status.observation_error)
            );
        } else if status.allocation_paused {
            out.push_str("\nGPU observation pending or stale; new grants paused.\n");
        }
        if let Some(age) = status.observation_age_ms {
            let _ = writeln!(
                out,
                "Observation: {age}ms ago (interval {}ms).",
                status.watch_unmanaged_ms
            );
        }
    }

    out.push_str("\nIn use\n");
    table(
        &mut out,
        &["GPUS", "UID", "PID", "HELD", "COMMAND"],
        &reservation_rows(&status.holders, false),
        "no GPUs in use",
    );

    out.push_str("\nWaiting\n");
    table(
        &mut out,
        &["#", "WANTS", "UID", "PID", "WAITING", "COMMAND"],
        &reservation_rows(&status.waiting, true),
        "nothing waiting",
    );

    out.push_str("\nGPU processes\n");
    let gpu_processes: &[ProcessRow] = match processes {
        ProcessView::Rows(rows) => {
            table(
                &mut out,
                &["GPU", "PID", "UID", "MEMORY", "STATUS", "COMMAND"],
                &process_rows(rows),
                "no processes on any GPU",
            );
            rows
        }
        ProcessView::Unavailable(reason) => {
            let _ = writeln!(
                out,
                "  Process list unavailable: {}",
                sanitize_display_text(reason.as_bytes())
            );
            &[]
        }
    };

    if !status.devices.is_empty() {
        out.push_str("\nDevices\n");
        let rows: Vec<Vec<String>> = status
            .devices
            .iter()
            .map(|device| {
                let mut holder = status
                    .holders
                    .iter()
                    .find(|h| h.gpus.contains(&device.index))
                    .map_or_else(|| "-".to_owned(), |h| format!("uid {}", h.uid));
                let unmanaged = gpu_processes
                    .iter()
                    .filter(|r| r.gpu == Some(device.index) && r.ownership == Ownership::NotManaged)
                    .count();
                if unmanaged > 0 {
                    let _ = write!(holder, " (+{unmanaged} not managed)");
                }
                vec![
                    device.index.to_string(),
                    holder,
                    sanitize_display_text(&device.name),
                    sanitize_display_text(&device.uuid),
                ]
            })
            .collect();
        table(
            &mut out,
            &["INDEX", "HELD BY", "NAME", "UUID"],
            &rows,
            "no devices",
        );
    }
    out
}

/// The dashboard when the daemon cannot be reached; it keeps polling.
pub fn render_error(reason: &str, refresh_seconds: u64) -> String {
    format!(
        "GPUDog (retrying every {refresh_seconds}s)\n\nCannot reach gpudogd: {}\n",
        sanitize_display_text(reason.as_bytes())
    )
}

#[cfg(test)]
mod tests {
    //! The main property: nothing reaches the terminal unsanitised.

    use super::*;
    use crate::protocol::DeviceInfo;

    #[test]
    fn observed_and_reserved_devices_are_not_double_counted() {
        let status = StatusReply {
            device_count: 2,
            holders: vec![holder(&[0])],
            busy_gpus: vec![0],
            ..StatusReply::default()
        };
        assert!(render(&status, &no_processes(), 1).contains("1 free"));
        let busy = StatusReply {
            busy_gpus: vec![0, 1],
            ..status
        };
        assert!(render(&busy, &no_processes(), 1).contains("0 free"));
    }

    #[test]
    fn failed_and_truncated_observation_never_claims_free_gpus() {
        let failed = StatusReply {
            device_count: 2,
            observing: true,
            watch_unmanaged_ms: 1000,
            observation_age_ms: Some(0),
            observation_error: b"bad\x1b".to_vec(),
            allocation_paused: true,
            ..StatusReply::default()
        };
        let text = render(&failed, &no_processes(), 1);
        assert!(text.contains("unknown free"));
        assert!(text.contains("bad?"));
        let truncated = StatusReply {
            device_count: 2,
            truncated: true,
            ..StatusReply::default()
        };
        assert!(render(&truncated, &no_processes(), 1).contains("unknown free"));
    }

    fn no_processes() -> ProcessView {
        ProcessView::Rows(Vec::new())
    }

    fn process(gpu: u32, pid: u32, ownership: Ownership) -> ProcessRow {
        ProcessRow {
            gpu: Some(gpu),
            pid,
            uid: Some(2000),
            used_memory: Some(3 << 29),
            command: Some(b"train".to_vec()),
            ownership,
        }
    }

    fn holder(gpus: &[u32]) -> ReservationInfo {
        ReservationInfo {
            id: 1,
            uid: 1000,
            pid: 4242,
            gpus: gpus.to_vec(),
            num_gpus: gpus.len() as u32,
            age_ms: 125_000,
            position: 0,
            command: b"python train.py".to_vec(),
        }
    }

    fn device(index: u32) -> DeviceInfo {
        DeviceInfo {
            index,
            name: b"Test GPU".to_vec(),
            uuid: format!("GPU-{index}").into_bytes(),
        }
    }

    /// Nothing outside printable ASCII plus newline may be emitted.
    fn assert_terminal_safe(text: &str) {
        assert!(
            text.bytes()
                .all(|b| b == b'\n' || (0x20..=0x7E).contains(&b)),
            "unsafe byte in {text:?}"
        );
    }

    #[test]
    fn a_table_tolerates_rows_that_do_not_match_the_header() {
        let mut out = String::new();
        table(
            &mut out,
            &["A", "B"],
            &[vec![], vec!["1".into(), "2".into(), "3".into()]],
            "none",
        );
        assert!(out.contains("1  2  3"), "{out:?}");
        // Header, the empty row, and the long row: nothing dropped.
        assert_eq!(out.lines().count(), 3, "{out:?}");
    }

    #[test]
    fn ages_are_rendered_in_units_a_human_reads() {
        assert_eq!(human_age(0), "0s");
        assert_eq!(human_age(59_999), "59s");
        assert_eq!(human_age(60_000), "1m 0s");
        assert_eq!(human_age(3_725_000), "1h 2m");
        assert_eq!(human_age(90_000_000), "1d 1h");
    }

    #[test]
    fn a_holder_is_rendered_with_its_uid_pid_devices_and_age() {
        let status = StatusReply {
            device_count: 4,
            holders: vec![holder(&[0, 2])],
            ..StatusReply::default()
        };
        let text = render(&status, &no_processes(), 5);
        let row = text
            .lines()
            .find(|l| l.contains("python train.py"))
            .unwrap();
        for field in ["0,2", "1000", "4242", "2m 5s"] {
            assert!(row.contains(field), "{field} missing from {row:?}");
        }
    }

    #[test]
    fn the_free_device_count_accounts_for_every_held_device() {
        let status = StatusReply {
            device_count: 4,
            holders: vec![holder(&[0, 1]), holder(&[3])],
            ..StatusReply::default()
        };
        assert!(
            render(&status, &no_processes(), 5).starts_with("GPUDog: 4 GPUs, 1 free, 0 waiting")
        );
    }

    #[test]
    fn a_free_count_cannot_go_negative_when_holdings_exceed_the_device_count() {
        let status = StatusReply {
            device_count: 1,
            holders: vec![holder(&[0, 1, 2])],
            ..StatusReply::default()
        };
        assert!(render(&status, &no_processes(), 5).starts_with("GPUDog: 1 GPU, 0 free"));
    }

    #[test]
    fn empty_tables_say_so_rather_than_rendering_nothing() {
        let text = render(&StatusReply::default(), &no_processes(), 5);
        assert!(text.contains("(no GPUs in use)"));
        assert!(text.contains("(nothing waiting)"));
    }

    #[test]
    fn a_truncated_snapshot_is_announced() {
        let status = StatusReply {
            truncated: true,
            ..StatusReply::default()
        };
        assert!(render(&status, &no_processes(), 5).contains("tables below are incomplete"));
        assert!(!render(&StatusReply::default(), &no_processes(), 5).contains("incomplete"));
    }

    #[test]
    fn a_queued_entry_shows_its_position_and_what_it_is_waiting_for() {
        let status = StatusReply {
            device_count: 1,
            waiting: vec![ReservationInfo {
                position: 2,
                num_gpus: 3,
                age_ms: 7_000,
                ..holder(&[])
            }],
            ..StatusReply::default()
        };
        let text = render(&status, &no_processes(), 5);
        let row = text
            .lines()
            .find(|l| l.contains("python train.py"))
            .unwrap();
        assert!(row.trim_start().starts_with("2  3 "), "{row:?}");
        assert!(row.contains("7s"));
    }

    #[test]
    fn the_device_table_names_who_holds_each_device() {
        let status = StatusReply {
            device_count: 2,
            devices: vec![device(0), device(1)],
            holders: vec![holder(&[1])],
            ..StatusReply::default()
        };
        let text = render(&status, &no_processes(), 5);
        let rows: Vec<&str> = text.lines().filter(|l| l.contains("Test GPU")).collect();
        assert!(rows[0].contains(" -  "), "{:?}", rows[0]);
        assert!(rows[1].contains("uid 1000"), "{:?}", rows[1]);
    }

    #[test]
    fn columns_line_up() {
        let status = StatusReply {
            device_count: 8,
            holders: vec![holder(&[0]), holder(&[1, 2, 3, 4, 5, 6])],
            ..StatusReply::default()
        };
        let text = render(&status, &no_processes(), 5);
        let uid_columns: Vec<usize> = text
            .lines()
            .filter(|l| l.contains("1000") || l.contains("UID"))
            .take(3)
            .map(|l| l.find("UID").or_else(|| l.find("1000")).unwrap())
            .collect();
        assert!(uid_columns.windows(2).all(|w| w[0] == w[1]), "{text}");
    }

    #[test]
    fn escape_sequences_from_the_daemon_never_reach_the_terminal() {
        // Every string the daemon controls, carrying every byte value.
        let every_byte: Vec<u8> = (0..=255).collect();
        let status = StatusReply {
            device_count: 1,
            devices: vec![DeviceInfo {
                index: 0,
                name: every_byte.clone(),
                uuid: b"\x1b]0;title\x07".to_vec(),
            }],
            holders: vec![ReservationInfo {
                command: every_byte.clone(),
                ..holder(&[0])
            }],
            waiting: vec![ReservationInfo {
                command: b"\x1b[2J\r\n".to_vec(),
                ..holder(&[])
            }],
            truncated: false,
            ..StatusReply::default()
        };
        let text = render(&status, &no_processes(), 5);
        assert_terminal_safe(&text);
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn bytes_are_rendered_in_binary_units() {
        assert_eq!(human_bytes(None), "-");
        assert_eq!(human_bytes(Some(512)), "512B");
        assert_eq!(human_bytes(Some(1536)), "1.5KiB");
        assert_eq!(human_bytes(Some(3 << 29)), "1.5GiB");
    }

    #[test]
    fn every_gpu_process_is_listed_with_its_status() {
        let status = StatusReply {
            device_count: 2,
            holders: vec![holder(&[0])],
            ..StatusReply::default()
        };
        let processes = ProcessView::Rows(vec![
            process(0, 4243, Ownership::Managed),
            process(1, 9001, Ownership::NotManaged),
            process(1, 9002, Ownership::WrongGpu),
            ProcessRow {
                uid: None,
                command: None,
                used_memory: None,
                ..process(1, 9003, Ownership::Unknown)
            },
        ]);
        let text = render(&status, &processes, 5);
        let row = |pid: &str| text.lines().find(|l| l.contains(pid)).unwrap().to_owned();
        assert!(row("4243").contains("managed"), "{text}");
        assert!(row("9001").contains("not managed"), "{text}");
        assert!(row("9002").contains("managed (wrong GPU)"), "{text}");
        let unknown = row("9003");
        assert!(
            unknown.contains("unknown") && unknown.ends_with('?'),
            "{unknown:?}"
        );
        for field in ["2000", "1.5GiB", "train"] {
            assert!(row("9001").contains(field), "{field} missing:\n{text}");
        }
    }

    #[test]
    fn an_idle_machine_says_no_gpu_processes() {
        let text = render(&StatusReply::default(), &no_processes(), 5);
        assert!(text.contains("(no processes on any GPU)"));
    }

    #[test]
    fn the_device_table_counts_processes_not_managed() {
        let status = StatusReply {
            device_count: 2,
            devices: vec![device(0), device(1)],
            holders: vec![holder(&[0])],
            ..StatusReply::default()
        };
        let processes = ProcessView::Rows(vec![
            process(0, 1, Ownership::NotManaged),
            process(1, 2, Ownership::NotManaged),
            process(1, 3, Ownership::NotManaged),
            process(1, 4, Ownership::Managed),
        ]);
        let text = render(&status, &processes, 5);
        let rows: Vec<&str> = text.lines().filter(|l| l.contains("Test GPU")).collect();
        assert!(
            rows[0].contains("uid 1000 (+1 not managed)"),
            "{:?}",
            rows[0]
        );
        assert!(rows[1].contains("- (+2 not managed)"), "{:?}", rows[1]);
    }

    #[test]
    fn truncation_does_not_reclassify_daemon_process_labels() {
        let status = StatusReply {
            truncated: true,
            ..StatusReply::default()
        };
        let processes = ProcessView::Rows(vec![process(0, 1, Ownership::NotManaged)]);
        let text = render(&status, &processes, 5);
        assert!(text.contains("not managed"));
        assert!(!text.contains("unlisted reservation"));
    }

    #[test]
    fn an_unavailable_process_list_is_explained_safely() {
        let processes = ProcessView::Unavailable("no NVML\x1b[2J".to_owned());
        let text = render(&StatusReply::default(), &processes, 5);
        assert!(text.contains("Process list unavailable: no NVML?[2J"));
        assert!(!text.contains("no processes on any GPU"));
        assert_terminal_safe(&text);
    }

    #[test]
    fn escape_sequences_in_process_names_never_reach_the_terminal() {
        let processes = ProcessView::Rows(vec![ProcessRow {
            command: Some((0..=255).collect()),
            ..process(0, 1, Ownership::NotManaged)
        }]);
        let text = render(&StatusReply::default(), &processes, 5);
        assert_terminal_safe(&text);
    }

    #[test]
    fn the_error_view_reports_the_reason_safely() {
        let text = render_error("Connection refused\x1b[2J", 5);
        assert!(text.contains("Cannot reach gpudogd: Connection refused?[2J"));
        assert_terminal_safe(&text);
    }
}
