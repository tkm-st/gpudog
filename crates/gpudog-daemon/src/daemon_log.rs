//! Daemon logging to stderr, for the systemd journal.
//!
//! No log files are written. When stderr is connected
//! to the journal, systemd sets `JOURNAL_STREAM`, and each line is prefixed
//! with a `<N>` syslog priority (`SyslogLevelPrefix=`, on by default), so
//! `journalctl -u gpudogd -p warning` shows only warnings and errors. On a
//! terminal the lines are plain.
//!
//! Writing never blocks a session: lines go through a bounded queue to one
//! writer thread, and are dropped (and counted) when the queue is full. A
//! stalled journal or a full stderr pipe must not freeze the daemon, since a
//! frozen daemon stops granting and expiring reservations for everyone.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use crate::service::{LogLevel, LogSink};

/// Lines buffered before new ones are dropped.
const QUEUE_LINES: usize = 4096;

/// Longest message kept, in characters. journald splits longer lines
/// (`LineMax=`, 48 KiB), and the continuation would be parsed as a new line
/// with whatever prefix it happens to start with.
pub const MAX_MESSAGE_CHARS: usize = 4096;

/// syslog priorities, as `sd-daemon.h` defines `SD_INFO` etc.
fn priority(level: LogLevel) -> u8 {
    match level {
        LogLevel::Access => 6,
        LogLevel::Warning => 4,
        LogLevel::Error => 3,
    }
}

fn label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Access => "access",
        LogLevel::Warning => "warning",
        LogLevel::Error => "error",
    }
}

/// One log line, newline included.
///
/// Control characters become `?`, and the message is capped at
/// [`MAX_MESSAGE_CHARS`]: one event must stay one line, or a message could
/// forge a second journal entry with its own priority prefix. (Client text is
/// already sanitised at the daemon boundary; this also covers error strings
/// from the OS and libraries.)
pub fn format_line(level: LogLevel, message: &str, journal: bool) -> String {
    let mut clean: String = message
        .chars()
        .take(MAX_MESSAGE_CHARS)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if message.chars().nth(MAX_MESSAGE_CHARS).is_some() {
        clean.push_str("...");
    }
    if journal {
        format!("<{}>{clean}\n", priority(level))
    } else {
        format!("gpudogd: {}: {clean}\n", label(level))
    }
}

/// True when systemd has connected stderr to the journal.
pub fn stderr_is_journal() -> bool {
    std::env::var_os("JOURNAL_STREAM").is_some()
}

enum Entry {
    Line(String),
    /// Acknowledged once every earlier line has been written.
    Flush(mpsc::Sender<()>),
}

/// The daemon's logger: a non-blocking [`LogSink`] and a flush for exit.
pub struct DaemonLog {
    queue: SyncSender<Entry>,
    journal: bool,
    dropped: Arc<AtomicU64>,
}

impl DaemonLog {
    /// Starts the writer thread for `out` (stderr in the daemon). Fails only
    /// if the thread cannot be spawned.
    pub fn new(journal: bool, out: impl Write + Send + 'static) -> std::io::Result<Self> {
        let (queue, entries) = mpsc::sync_channel::<Entry>(QUEUE_LINES);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        std::thread::Builder::new()
            .name("gpudogd-log".into())
            .spawn(move || {
                let mut out = out;
                for entry in entries {
                    let lost_now = lost.swap(0, Ordering::Relaxed);
                    if lost_now > 0 {
                        let note = format!(
                            "{lost_now} log line(s) dropped: the log output was not keeping up"
                        );
                        let _ =
                            out.write_all(format_line(LogLevel::Error, &note, journal).as_bytes());
                    }
                    match entry {
                        // Nowhere left to report a failure to write.
                        Entry::Line(line) => {
                            let _ = out.write_all(line.as_bytes());
                        }
                        Entry::Flush(done) => {
                            let _ = out.flush();
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self {
            queue,
            journal,
            dropped,
        })
    }

    /// Logs to stderr, in journal format when systemd set `JOURNAL_STREAM`.
    pub fn stderr() -> std::io::Result<Self> {
        Self::new(stderr_is_journal(), std::io::stderr())
    }

    /// A sink that never blocks: a full queue drops the line and counts it.
    pub fn sink(&self) -> LogSink {
        let queue = self.queue.clone();
        let journal = self.journal;
        let dropped = self.dropped.clone();
        Arc::new(move |level, message| {
            let line = format_line(level, message, journal);
            if let Err(TrySendError::Full(_)) = queue.try_send(Entry::Line(line)) {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        })
    }

    /// Waits up to `timeout` for every line logged so far to be written.
    /// Returns false if the output did not drain in time.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let (done, acknowledged) = mpsc::channel();
        let mut entry = Entry::Flush(done);
        // The queue may be full of backlog; keep offering until the deadline.
        loop {
            match self.queue.try_send(entry) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) if std::time::Instant::now() < deadline => {
                    entry = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        }
        acknowledged
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn journal_lines_carry_a_syslog_priority() {
        assert_eq!(
            format_line(LogLevel::Access, "granted", true),
            "<6>granted\n"
        );
        assert_eq!(format_line(LogLevel::Warning, "slow", true), "<4>slow\n");
        assert_eq!(format_line(LogLevel::Error, "broken", true), "<3>broken\n");
    }

    #[test]
    fn terminal_lines_are_labelled() {
        assert_eq!(
            format_line(LogLevel::Warning, "slow", false),
            "gpudogd: warning: slow\n"
        );
    }

    #[test]
    fn a_message_cannot_forge_a_second_journal_entry() {
        assert_eq!(
            format_line(LogLevel::Access, "a\n<3>fake\r\x1b", true),
            "<6>a?<3>fake??\n"
        );
    }

    #[test]
    fn a_long_message_is_capped_below_the_journal_line_limit() {
        let line = format_line(LogLevel::Access, &"x".repeat(100_000), true);
        assert_eq!(line.len(), "<6>".len() + MAX_MESSAGE_CHARS + "...\n".len());
        assert_eq!(
            format_line(LogLevel::Access, &"x".repeat(MAX_MESSAGE_CHARS), true).len(),
            MAX_MESSAGE_CHARS + 4
        );
    }

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn lines_are_written_in_order_and_flush_waits_for_them() {
        let out = Shared::default();
        let log = DaemonLog::new(false, out.clone()).unwrap();
        let sink = log.sink();
        sink(LogLevel::Access, "one");
        sink(LogLevel::Error, "two");
        assert!(log.flush(Duration::from_secs(5)));
        assert_eq!(
            String::from_utf8(out.0.lock().unwrap().clone()).unwrap(),
            "gpudogd: access: one\ngpudogd: error: two\n"
        );
    }

    /// An output that blocks until released, like a full pipe.
    struct Stuck(mpsc::Receiver<()>, Shared);

    impl Write for Stuck {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            self.1.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stuck_output_never_blocks_the_caller_and_drops_are_reported() {
        let (release, gate) = mpsc::channel();
        let out = Shared::default();
        let log = DaemonLog::new(true, Stuck(gate, out.clone())).unwrap();
        let sink = log.sink();
        let start = std::time::Instant::now();
        for i in 0..(QUEUE_LINES * 2) {
            sink(LogLevel::Access, &format!("line {i}"));
        }
        assert!(start.elapsed() < Duration::from_secs(2), "logging blocked");
        assert!(!log.flush(Duration::from_millis(50)));

        // Unstick the output; the backlog drains, then the drop count shows.
        std::thread::spawn(move || while release.send(()).is_ok() {});
        assert!(log.flush(Duration::from_secs(10)));
        sink(LogLevel::Access, "after");
        assert!(log.flush(Duration::from_secs(10)));
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("log line(s) dropped"), "no drop report");
        assert!(text.ends_with("<6>after\n"));
    }
}
