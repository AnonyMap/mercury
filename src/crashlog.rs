//! Crash reports. A panic anywhere writes `logs/last-crash.txt` (message, thread, backtrace). The
//! desktop app catches panics in its background tasks and keeps running; those reports are filed
//! as `error-<time>.txt`. A report still called `last-crash.txt` at the next start means Mercury
//! really went down, and the app says so once.

use std::path::{Path, PathBuf};

const LAST: &str = "last-crash.txt";
const KEEP: usize = 10;

pub fn install(logs: PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        let text = format!(
            "Mercury {} on {} ({})\ntime: {}\nthread: {thread}\n{info}\n\nbacktrace:\n{bt}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            chrono::Local::now().to_rfc3339(),
        );
        let _ = std::fs::create_dir_all(&logs);
        let _ = std::fs::write(logs.join(LAST), text);
        default(info);
    }));
}

/// After a panic was caught and survived: file the report under `error-<time>.txt`.
pub fn file_recovered(logs: &Path) -> Option<PathBuf> {
    rename_last(logs, "error")
}

/// At start-up: if the previous run crashed, file its report and return where it went.
pub fn take_previous_crash(logs: &Path) -> Option<PathBuf> {
    rename_last(logs, "crash")
}

fn rename_last(logs: &Path, prefix: &str) -> Option<PathBuf> {
    let last = logs.join(LAST);
    if !last.is_file() {
        return None;
    }
    let to = logs.join(format!("{prefix}-{}.txt", chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")));
    std::fs::rename(&last, &to).ok()?;
    prune(logs);
    Some(to)
}

fn prune(logs: &Path) {
    let Ok(rd) = std::fs::read_dir(logs) else { return };
    let mut reports: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| (n.starts_with("crash-") || n.starts_with("error-")) && n.ends_with(".txt"))
        })
        .collect();
    reports.sort();
    let excess = reports.len().saturating_sub(KEEP);
    for p in reports.into_iter().take(excess) {
        let _ = std::fs::remove_file(p);
    }
}

/// The text of a caught panic payload.
pub fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_filed_and_pruned() {
        let d = tempfile::tempdir().unwrap();
        assert!(take_previous_crash(d.path()).is_none());
        std::fs::write(d.path().join(LAST), "boom").unwrap();
        let p = take_previous_crash(d.path()).unwrap();
        assert!(p.file_name().unwrap().to_str().unwrap().starts_with("crash-"));
        assert!(!d.path().join(LAST).exists());
        for i in 0..15 {
            std::fs::write(d.path().join(format!("error-2020010{}-0000{i:02}.txt", i % 9)), "x").unwrap();
        }
        std::fs::write(d.path().join(LAST), "boom").unwrap();
        file_recovered(d.path()).unwrap();
        let n = std::fs::read_dir(d.path()).unwrap().count();
        assert_eq!(n, KEEP);
        assert_eq!(panic_message(&"x"), "x");
        assert_eq!(panic_message(&String::from("y")), "y");
    }
}
