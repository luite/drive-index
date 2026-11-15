//! Fast, parallel, read-only directory walking (PLAN.md §4.2–4.4).
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
