//! Types and helpers shared by the walker, the catalog and the UI.
//!
//! This crate has no I/O and (almost) no dependencies so that every other crate,
//! including the platform walker that is type-checked for foreign targets, can use it.

pub mod flags;
pub mod names;
pub mod units;

/// Type and usage of a mounted filesystem (from statfs/statvfs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsUsage {
    pub fs_type: String,
    pub mount_point: Option<String>,
    pub size_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub block_size: Option<u64>,
    pub inodes_total: Option<u64>,
    pub inodes_used: Option<u64>,
}

/// Kind of a non-directory entry, as stored in `files.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FileKind {
    File = 0,
    Symlink = 1,
    Fifo = 2,
    Socket = 3,
    CharDev = 4,
    BlockDev = 5,
    Other = 6,
}

impl FileKind {
    pub fn code(self) -> i64 {
        self as u8 as i64
    }

    pub fn from_code(code: i64) -> FileKind {
        match code {
            0 => FileKind::File,
            1 => FileKind::Symlink,
            2 => FileKind::Fifo,
            3 => FileKind::Socket,
            4 => FileKind::CharDev,
            5 => FileKind::BlockDev,
            _ => FileKind::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Symlink => "symlink",
            FileKind::Fifo => "fifo",
            FileKind::Socket => "socket",
            FileKind::CharDev => "char",
            FileKind::BlockDev => "block",
            FileKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<FileKind> {
        Some(match s {
            "file" | "f" => FileKind::File,
            "symlink" | "link" | "l" => FileKind::Symlink,
            "fifo" | "pipe" => FileKind::Fifo,
            "socket" | "sock" => FileKind::Socket,
            "char" | "chardev" => FileKind::CharDev,
            "block" | "blockdev" => FileKind::BlockDev,
            "other" => FileKind::Other,
            _ => return None,
        })
    }
}
