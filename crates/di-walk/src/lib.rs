//! Fast, parallel, read-only directory walking.
//!
//! The walker has no knowledge of the catalog: it emits one [`DirBatch`] per listed
//! directory over a channel, and the consumer (the staging writer) persists them.
//! Directory listing is delegated to a platform [`Lister`]:
//!
//! * macOS: `getattrlistbulk(2)`, one syscall returns many entries *with* metadata;
//! * Linux: `getdents64` + `statx` per entry, entries sorted by inode first;
//! * elsewhere / fallback: `std::fs::read_dir` + `symlink_metadata`.

pub mod host;
pub mod list;
pub mod walk;

pub use list::{Entry, EntryKind, Lister, Meta, best_lister, stat_path};
pub use walk::{
    Control, DirBatch, DirJob, DirRow, DirState, ErrorRow, FileRow, Progress, WalkConfig,
    WalkOutcome, walk,
};

// Note [Device errors]
// ~~~~~~~~~~~~~~~~~~~~
// Some errnos say nothing about the file and everything about the drive under it:
// the device failed a read (EIO), went away (ENODEV, ENXIO) or stopped answering
// (ETIMEDOUT). A drive that is unplugged mid-scan, or whose USB bridge resets,
// produces a burst of them. They are treated differently from per-file errors
// (EACCES, ENOENT, ELOOP, ...):
//
//  * the walk retries directories that failed this way when a scan is resumed,
//    since the failure was probably the connection, not the directory
//    (see `Staging::requeue_device_errors` in di-scan);
//  * the content phase stops reading from a drive that keeps failing reads
//    (see Note [Drive trouble] in di-scan:content).
//
// On a disk with a genuinely unreadable sector, EIO is permanent; the directory is
// then retried on every resume until a walk completes, after which the error is
// recorded like any other.

/// Is this errno a failure of the device rather than of one file?
/// See Note [Device errors].
pub fn is_device_errno(errno: i32) -> bool {
    matches!(errno, libc::EIO | libc::ENXIO | libc::ENODEV | libc::ETIMEDOUT)
}
