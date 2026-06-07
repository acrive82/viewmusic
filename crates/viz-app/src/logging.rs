//! Application logging setup.
//!
//! Writes structured logs to a single stable file under the macOS Logs directory
//! (`~/Library/Logs/io.github.acrive82.viewmusic/viewmusic.log`),
//! through a `tracing_appender` non-blocking writer so file I/O never touches the
//! real-time audio or render threads.
//!
//! The file uses `Rotation::NEVER` (one stable filename for the documented
//! troubleshooting path). To stop unbounded growth, [`init`] checks the file size
//! at startup and, if it exceeds [`MAX_LOG_BYTES`], rotates it to `viewmusic.log.1`
//! (replacing any previous backup) BEFORE the appender opens the file.

use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Reverse-DNS bundle id; also the Logs subdirectory name.
pub const BUNDLE_ID: &str = "io.github.acrive82.viewmusic";
/// Stable log file name (the documented troubleshooting path).
pub const LOG_FILE_NAME: &str = "viewmusic.log";
/// Single backup file name produced by startup rotation.
pub const LOG_BACKUP_NAME: &str = "viewmusic.log.1";
/// Startup rotation threshold: rotate when the live log exceeds 10 MB.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
/// Default log level when `RUST_LOG` is unset.
pub const DEFAULT_FILTER: &str = "info";

/// Computes the log directory `~/Library/Logs/<BUNDLE_ID>` from a home directory.
///
/// `directories` does not expose a macOS Logs location, so the path is built from
/// the home directory directly. Pure function — unit-tested.
pub fn log_dir_from_home(home: &Path) -> PathBuf {
    home.join("Library").join("Logs").join(BUNDLE_ID)
}

/// Resolves the active log directory from the current user's home directory.
///
/// Returns `None` only when no home directory can be determined.
pub fn log_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(log_dir_from_home(base.home_dir()))
}

/// Decides whether the live log file should be rotated to its backup at startup.
///
/// Pure decision function (the size check), separated so it can be unit-tested
/// without touching the filesystem. Returns `true` when `size_bytes` exceeds the
/// threshold.
pub fn should_rotate(size_bytes: u64, max_bytes: u64) -> bool {
    size_bytes > max_bytes
}

/// Performs the startup size-based rotation in `dir`, if needed.
///
/// If `dir/viewmusic.log` exceeds [`MAX_LOG_BYTES`], it is moved to
/// `dir/viewmusic.log.1` (replacing any previous backup). Errors are best-effort:
/// a failure here must not prevent the app from starting, so it is swallowed (no
/// logger exists yet to report it).
fn rotate_if_oversized(dir: &Path) {
    let live = dir.join(LOG_FILE_NAME);
    let Ok(meta) = std::fs::metadata(&live) else {
        // No existing log (or unreadable) — nothing to rotate.
        return;
    };
    if !should_rotate(meta.len(), MAX_LOG_BYTES) {
        return;
    }
    let backup = dir.join(LOG_BACKUP_NAME);
    // Replace any previous backup; ignore failure when it does not exist.
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(&live, &backup);
}

/// Builds the env filter from `RUST_LOG`, falling back to [`DEFAULT_FILTER`].
fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Initializes file logging and returns the worker guard.
///
/// The caller (main) MUST keep the returned [`WorkerGuard`] alive for the lifetime
/// of the process; dropping it flushes and stops the background writer.
///
/// On success the global tracing subscriber is installed and a startup line is
/// emitted. If the log directory cannot be resolved or created, a guard is still
/// returned (writing to a temp dir) so the rest of the app can run; this is logged
/// to stderr as a last resort.
pub fn init() -> WorkerGuard {
    let dir = log_dir().unwrap_or_else(std::env::temp_dir);

    // Create the directory tree before any size check / appender open.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("viewmusic: could not create log directory {dir:?}: {e}");
    }

    // Startup rotation must happen BEFORE the appender opens the file for append.
    rotate_if_oversized(&dir);

    let file_appender = tracing_appender::rolling::never(&dir, LOG_FILE_NAME);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true);

    // `try_init` keeps tests from panicking if a subscriber is already set.
    if let Err(e) = subscriber.try_init() {
        eprintln!("viewmusic: tracing subscriber already initialized: {e}");
    }

    tracing::info!(target: "viewmusic", log_dir = %dir.display(), "logging initialized");
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_layout_matches_macos_convention() {
        let home = Path::new("/Users/alice");
        let dir = log_dir_from_home(home);
        assert_eq!(
            dir,
            PathBuf::from("/Users/alice/Library/Logs/io.github.acrive82.viewmusic")
        );
    }

    #[test]
    fn rotation_decision_is_threshold_strict() {
        // At or below the threshold: keep.
        assert!(!should_rotate(0, MAX_LOG_BYTES));
        assert!(!should_rotate(MAX_LOG_BYTES, MAX_LOG_BYTES));
        // Strictly above: rotate.
        assert!(should_rotate(MAX_LOG_BYTES + 1, MAX_LOG_BYTES));
        assert!(should_rotate(u64::MAX, MAX_LOG_BYTES));
    }

    #[test]
    fn rotate_moves_oversized_live_to_backup() {
        let tmp = std::env::temp_dir().join(format!("viewmusic-logtest-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let live = tmp.join(LOG_FILE_NAME);
        let backup = tmp.join(LOG_BACKUP_NAME);

        // Pre-existing stale backup must be replaced.
        std::fs::write(&backup, b"old-backup").unwrap();
        // Live file just over the threshold.
        std::fs::write(&live, vec![0u8; (MAX_LOG_BYTES + 16) as usize]).unwrap();

        rotate_if_oversized(&tmp);

        assert!(!live.exists(), "oversized live log should have been moved");
        assert!(backup.exists(), "backup should exist after rotation");
        let backup_len = std::fs::metadata(&backup).unwrap().len();
        assert!(
            backup_len > MAX_LOG_BYTES,
            "backup should be the moved live file, not the stale one"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn rotate_leaves_small_log_in_place() {
        let tmp = std::env::temp_dir().join(format!("viewmusic-logtest2-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let live = tmp.join(LOG_FILE_NAME);
        std::fs::write(&live, b"small").unwrap();

        rotate_if_oversized(&tmp);

        assert!(live.exists(), "small live log should be untouched");
        assert!(!tmp.join(LOG_BACKUP_NAME).exists());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
