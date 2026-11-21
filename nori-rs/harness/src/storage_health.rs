//! Whether Nori can write its own files: one process-wide state that the UI follows.
//!
//! A full disk (or a failing one) must never cost a conversation: the session keeps running,
//! what could not be written is kept in memory and written in order once writing works again,
//! and the person is told once when saving stops and once when it resumes. Writers report here
//! ([`failed`], [`recovered`]); the UI follows [`subscribe`].

use std::sync::OnceLock;
use std::time::SystemTime;

use tokio::sync::watch;

/// How writing Nori's files is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageHealth {
    Ok,
    /// Writes work, but the disk holding Nori's files is nearly full.
    Low {
        free_bytes: i64,
    },
    /// Writes fail; what they held is kept in memory until they work again.
    Failing {
        /// What could not be written, in a few words.
        what: String,
        /// Why, as the system said it.
        error: String,
        /// Whether the disk is full (rather than failing otherwise).
        full: bool,
        since: SystemTime,
    },
}

fn channel() -> &'static watch::Sender<StorageHealth> {
    static HEALTH: OnceLock<watch::Sender<StorageHealth>> = OnceLock::new();
    HEALTH.get_or_init(|| watch::channel(StorageHealth::Ok).0)
}

/// Follow the state; the current value is first.
pub fn subscribe() -> watch::Receiver<StorageHealth> {
    channel().subscribe()
}

/// Whether `error` means there is no space left (or quota).
pub fn is_full(error: &std::io::Error) -> bool {
    // ENOSPC is 28 and EDQUOT 69 on macOS, 122 on Linux.
    error.kind() == std::io::ErrorKind::StorageFull
        || matches!(error.raw_os_error(), Some(28))
        || (cfg!(target_os = "macos") && error.raw_os_error() == Some(69))
        || (cfg!(target_os = "linux") && error.raw_os_error() == Some(122))
}

/// Below this, the person is told the disk is nearly full; above twice it, that is forgotten.
pub const LOW_SPACE_BYTES: i64 = 1024 * 1024 * 1024;

/// Free bytes on the filesystem holding `path`, when the system says.
pub fn free_bytes(path: &std::path::Path) -> Option<i64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a valid C string and `stats` a valid out-pointer for the call.
        let status = unsafe { libc::statvfs(path.as_ptr(), &raw mut stats) };
        (status == 0).then(|| {
            i64::try_from(stats.f_bavail as u128 * stats.f_frsize as u128).unwrap_or(i64::MAX)
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Look at the free space where Nori writes (`path`, its home): nearly full is said once,
/// before anything fails; plenty again is forgotten quietly.
pub fn check_space(path: &std::path::Path) {
    let Some(free) = free_bytes(path) else {
        return;
    };
    channel().send_if_modified(|health| match health {
        StorageHealth::Ok if free < LOW_SPACE_BYTES => {
            *health = StorageHealth::Low { free_bytes: free };
            true
        }
        StorageHealth::Low { .. } if free > 2 * LOW_SPACE_BYTES => {
            *health = StorageHealth::Ok;
            true
        }
        StorageHealth::Ok | StorageHealth::Low { .. } | StorageHealth::Failing { .. } => false,
    });
}

/// A write of `what` failed. The first failure is what the person hears about.
pub(crate) fn failed(what: &str, error: &std::io::Error) {
    channel().send_if_modified(|health| match health {
        StorageHealth::Ok | StorageHealth::Low { .. } => {
            tracing::warn!("cannot write {what}: {error}");
            *health = StorageHealth::Failing {
                what: what.to_owned(),
                error: error.to_string(),
                full: is_full(error),
                since: SystemTime::now(),
            };
            true
        }
        StorageHealth::Failing { .. } => false,
    });
}

/// Writes work again, and everything held back has been written.
pub(crate) fn recovered() {
    channel().send_if_modified(|health| match health {
        StorageHealth::Ok | StorageHealth::Low { .. } => false,
        StorageHealth::Failing { .. } => {
            tracing::info!("writing works again");
            *health = StorageHealth::Ok;
            true
        }
    });
}
