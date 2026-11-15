//! Directory listers: list one directory, returning entries with metadata.

use std::io;
use std::path::Path;

use di_model::FileKind;

mod generic;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

pub use generic::GenericLister;

/// Metadata of one directory entry. Fields a platform cannot provide are `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// Logical size (data fork on macOS).
    pub size: u64,
    /// Bytes allocated on disk.
    pub alloc: Option<u64>,
    pub mtime_ns: Option<i64>,
    pub ctime_ns: Option<i64>,
    pub atime_ns: Option<i64>,
    pub btime_ns: Option<i64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub inode: Option<u64>,
    pub nlink: Option<u64>,
    pub dev: Option<u64>,
    /// `di_model::flags` bits derived from platform attributes.
    pub flags: u32,
    /// The entry is the root of another mounted filesystem.
    pub mount_point: bool,
    /// Number of directory entries, when the platform reports it up front (macOS).
    pub entry_count: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    NonDir(FileKind),
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Raw on-disk name bytes.
    pub name: Vec<u8>,
    pub kind: EntryKind,
    pub meta: Meta,
    /// Symlink target bytes, for symlinks.
    pub link_target: Option<Vec<u8>>,
    /// Metadata could not be read: (operation, errno, message).
    pub error: Option<(&'static str, Option<i32>, String)>,
}

/// Lists a single directory. Implementations must be cheap to share between threads.
pub trait Lister: Send + Sync {
    /// Short name recorded as `scans.method`.
    fn method(&self) -> &'static str;

    /// List `dir` (absolute path) into `out` (which is cleared first). `buf` is a
    /// per-thread scratch buffer the implementation may resize. An `Err` means the
    /// directory itself could not be listed; per-entry metadata failures are
    /// reported in [`Entry::error`].
    fn list(&self, dir: &Path, out: &mut Vec<Entry>, buf: &mut Vec<u8>) -> io::Result<()>;
}

/// The fastest lister for this platform, with automatic per-directory fallback to
/// [`GenericLister`] where the fast path is unsupported.
pub fn best_lister() -> Box<dyn Lister> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::BulkLister::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::StatxLister::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Box::new(GenericLister)
    }
}

/// `lstat` a single path (used for the scan root).
pub fn stat_path(path: &Path) -> io::Result<(EntryKind, Meta)> {
    let md = std::fs::symlink_metadata(path)?;
    Ok(generic::from_std(&md))
}

/// Map BSD `st_flags` (macOS) to catalog flag bits.
#[allow(dead_code)]
pub(crate) fn map_bsd_flags(st_flags: u32) -> u32 {
    use di_model::flags::*;
    const UF_NODUMP: u32 = 0x0000_0001;
    const UF_IMMUTABLE: u32 = 0x0000_0002;
    const UF_APPEND: u32 = 0x0000_0004;
    const UF_COMPRESSED: u32 = 0x0000_0020;
    const UF_HIDDEN: u32 = 0x0000_8000;
    const SF_IMMUTABLE: u32 = 0x0002_0000;
    const SF_APPEND: u32 = 0x0004_0000;
    const SF_DATALESS: u32 = 0x4000_0000;

    let mut f = 0;
    if st_flags & UF_NODUMP != 0 {
        f |= NODUMP;
    }
    if st_flags & (UF_IMMUTABLE | SF_IMMUTABLE) != 0 {
        f |= IMMUTABLE;
    }
    if st_flags & (UF_APPEND | SF_APPEND) != 0 {
        f |= APPEND_ONLY;
    }
    if st_flags & UF_COMPRESSED != 0 {
        f |= COMPRESSED;
    }
    if st_flags & UF_HIDDEN != 0 {
        f |= HIDDEN;
    }
    if st_flags & SF_DATALESS != 0 {
        f |= DATALESS;
    }
    f
}

/// File kind from the `S_IFMT` bits of a mode.
pub(crate) fn kind_from_mode(mode: u32) -> EntryKind {
    match mode & 0o170000 {
        0o040000 => EntryKind::Dir,
        0o100000 => EntryKind::NonDir(FileKind::File),
        0o120000 => EntryKind::NonDir(FileKind::Symlink),
        0o010000 => EntryKind::NonDir(FileKind::Fifo),
        0o140000 => EntryKind::NonDir(FileKind::Socket),
        0o020000 => EntryKind::NonDir(FileKind::CharDev),
        0o060000 => EntryKind::NonDir(FileKind::BlockDev),
        _ => EntryKind::NonDir(FileKind::Other),
    }
}

pub(crate) fn ts_ns(sec: i64, nsec: i64) -> Option<i64> {
    sec.checked_mul(1_000_000_000)?.checked_add(nsec)
}
