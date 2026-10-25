//! Crash reports. A panic on the UI thread closes the app; the hook writes
//! what happened to `crash.log` in the config folder (only on this computer)
//! and the next start says so, so the cause can be found and reported.

use std::path::{Path, PathBuf};

const LOG: &str = "crash.log";
const PENDING: &str = "crash.pending";
/// Panic messages can quote the text being shown; keep only their start.
const MAX_MESSAGE: usize = 500;

/// Installs the panic hook (after the default one, which still prints).
pub fn install(dir: &Path) {
    let dir = dir.to_path_buf();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Panics on background threads end that thread or task, not the app.
        if std::thread::current().name() == Some("main") {
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            let message: String = message.chars().take(MAX_MESSAGE).collect();
            let location = info.location().map(|l| l.to_string()).unwrap_or_default();
            let report = format!(
                "Merpati {} closed unexpectedly on {}\n{message}\nat {location}\n\n{}\n",
                env!("CARGO_PKG_VERSION"),
                chrono::Local::now().to_rfc3339(),
                std::backtrace::Backtrace::force_capture(),
            );
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(LOG), report);
            let _ = std::fs::write(dir.join(PENDING), "");
        }
        default(info);
    }));
}

/// The report of the previous run when it ended in a crash (once).
pub fn take_pending(dir: &Path) -> Option<PathBuf> {
    std::fs::remove_file(dir.join(PENDING)).ok().map(|_| dir.join(LOG))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_report_is_announced_once() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(take_pending(dir.path()), None);
        std::fs::write(dir.path().join(PENDING), "").unwrap();
        assert_eq!(take_pending(dir.path()), Some(dir.path().join(LOG)));
        assert_eq!(take_pending(dir.path()), None);
    }
}
