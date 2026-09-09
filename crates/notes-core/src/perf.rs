//! Opt-in, privacy-safe runtime timing for local performance investigations.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub const PERF_LOG_PATH: &str = "/private/tmp/apple-notes-tui-perf.log";

static STARTED: OnceLock<Instant> = OnceLock::new();
static LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn enabled() -> bool {
    enabled_value(std::env::var("APPLE_NOTES_TUI_PERF").ok().as_deref())
}

fn enabled_value(value: Option<&str>) -> bool {
    matches!(value, Some("1"))
}

/// Appends only operation metadata, stable opaque targets, duration, and
/// outcome. Callers must never pass user-provided display text or content.
pub fn event(operation: &str, target: Option<&str>, started: Instant, result: &str) {
    if !enabled() {
        return;
    }
    let origin = STARTED.get_or_init(Instant::now);
    let elapsed = origin.elapsed().as_millis();
    let duration = started.elapsed().as_millis();
    let target = target.map_or(String::new(), |value| format!(" target={value}"));
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed");
    write_line(
        PERF_LOG_PATH,
        &format!(
            "+{elapsed:06}ms thread={thread} operation={operation}{target} duration_ms={duration} result={result}"
        ),
    );
}

fn write_line(path: &str, line: &str) {
    let _guard = LOG_LOCK.get_or_init(|| Mutex::new(())).lock().ok();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::{enabled_value, write_line};
    use std::fs;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn perf_log_concurrent_writes_are_atomic() {
        let path = std::env::temp_dir().join(format!(
            "apple-notes-tui-perf-{}-{}.log",
            std::process::id(),
            thread::current().name().unwrap_or("test")
        ));
        let path = Arc::new(path);
        let _ = fs::remove_file(&*path);
        let mut workers = Vec::new();
        for worker in 0..8 {
            let path = Arc::clone(&path);
            workers.push(thread::spawn(move || {
                for record in 0..50 {
                    write_line(
                        path.to_str().expect("temporary path"),
                        &format!("record-{worker}-{record}"),
                    );
                }
            }));
        }
        for worker in workers {
            worker.join().expect("logger worker");
        }
        let contents = fs::read_to_string(&*path).expect("perf log");
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 400);
        for line in lines {
            let mut parts = line.split('-');
            assert_eq!(parts.next(), Some("record"));
            assert!(parts.next().and_then(|v| v.parse::<usize>().ok()).is_some());
            assert!(parts.next().and_then(|v| v.parse::<usize>().ok()).is_some());
            assert!(parts.next().is_none());
        }
        let _ = fs::remove_file(&*path);
    }

    #[test]
    fn perf_disabled_value_does_not_enable_logging() {
        assert!(!enabled_value(None));
        assert!(!enabled_value(Some("0")));
        assert!(!enabled_value(Some("true")));
        assert!(enabled_value(Some("1")));
    }
}
