//! Linux fast path: `getdents64` (via rustix `RawDir`) + `statx` relative to the
//! directory fd. Entries are stat'ed in inode order, which reduces seeks on
//! ext2/3/4, xfs and hfsplus on rotational media (PLAN.md §4.3).

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use di_model::{FileKind, flags};
use rustix::fs::{AtFlags, Mode, OFlags, RawDir, StatxFlags};

use super::{Entry, EntryKind, GenericLister, Lister, Meta, kind_from_mode, ts_ns};

const STATX_ATTR_COMPRESSED: u64 = 0x0000_0004;
const STATX_ATTR_IMMUTABLE: u64 = 0x0000_0010;
const STATX_ATTR_APPEND: u64 = 0x0000_0020;
const STATX_ATTR_NODUMP: u64 = 0x0000_0040;
const STATX_ATTR_ENCRYPTED: u64 = 0x0000_0800;
const STATX_ATTR_MOUNT_ROOT: u64 = 0x0000_2000;

pub struct StatxLister {
    fallback: GenericLister,
}

impl StatxLister {
    pub fn new() -> Self {
        StatxLister { fallback: GenericLister }
    }
}

impl Lister for StatxLister {
    fn method(&self) -> &'static str {
        "getdents-statx"
    }

    fn list(&self, dir: &Path, out: &mut Vec<Entry>, buf: &mut Vec<u8>) -> io::Result<()> {
        match list_statx(dir, out, buf) {
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => self.fallback.list(dir, out, buf),
            other => other,
        }
    }
}

fn list_statx(dir: &Path, out: &mut Vec<Entry>, buf: &mut Vec<u8>) -> io::Result<()> {
    out.clear();
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;

    let mut names: Vec<(Vec<u8>, u64, rustix::fs::FileType)> = Vec::new();
    buf.clear();
    buf.reserve(64 * 1024);
    {
        let mut iter = RawDir::new(&fd, buf.spare_capacity_mut());
        while let Some(entry) = iter.next() {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            names.push((name.to_vec(), entry.ino(), entry.file_type()));
        }
    }
    names.sort_unstable_by_key(|(_, ino, _)| *ino);

    for (name, _ino, dtype) in names {
        let os = OsStr::from_bytes(&name);
        match rustix::fs::statx(
            &fd,
            os,
            AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC,
            StatxFlags::BASIC_STATS | StatxFlags::BTIME,
        ) {
            Ok(st) => {
                let mode = st.stx_mode as u32;
                let kind = kind_from_mode(mode);
                let attrs = st.stx_attributes.bits();
                let attr_mask = st.stx_attributes_mask.bits();
                let mut f = 0;
                if attrs & STATX_ATTR_COMPRESSED != 0 {
                    f |= flags::COMPRESSED;
                }
                if attrs & STATX_ATTR_IMMUTABLE != 0 {
                    f |= flags::IMMUTABLE;
                }
                if attrs & STATX_ATTR_APPEND != 0 {
                    f |= flags::APPEND_ONLY;
                }
                if attrs & STATX_ATTR_NODUMP != 0 {
                    f |= flags::NODUMP;
                }
                if attrs & STATX_ATTR_ENCRYPTED != 0 {
                    f |= flags::ENCRYPTED;
                }
                let has_btime = StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::BTIME);
                let meta = Meta {
                    size: st.stx_size,
                    alloc: Some(st.stx_blocks.saturating_mul(512)),
                    mtime_ns: ts_ns(st.stx_mtime.tv_sec, st.stx_mtime.tv_nsec as i64),
                    ctime_ns: ts_ns(st.stx_ctime.tv_sec, st.stx_ctime.tv_nsec as i64),
                    atime_ns: ts_ns(st.stx_atime.tv_sec, st.stx_atime.tv_nsec as i64),
                    btime_ns: if has_btime {
                        ts_ns(st.stx_btime.tv_sec, st.stx_btime.tv_nsec as i64)
                    } else {
                        None
                    },
                    mode: Some(mode),
                    uid: Some(st.stx_uid),
                    gid: Some(st.stx_gid),
                    inode: Some(st.stx_ino),
                    nlink: Some(st.stx_nlink as u64),
                    dev: Some(libc::makedev(st.stx_dev_major, st.stx_dev_minor) as u64),
                    flags: f,
                    mount_point: attr_mask & STATX_ATTR_MOUNT_ROOT != 0 && attrs & STATX_ATTR_MOUNT_ROOT != 0,
                    entry_count: None,
                };
                let link_target = if kind == EntryKind::NonDir(FileKind::Symlink) {
                    rustix::fs::readlinkat(&fd, os, Vec::new()).ok().map(|c| c.into_bytes())
                } else {
                    None
                };
                out.push(Entry { name, kind, meta, link_target, error: None });
            }
            Err(e) if e.raw_os_error() == libc::ENOSYS => {
                return Err(io::Error::from_raw_os_error(libc::ENOSYS));
            }
            Err(e) => {
                let err = io::Error::from(e);
                // Keep walking directories even when their metadata is unreadable.
                let kind = match dtype {
                    rustix::fs::FileType::Directory => EntryKind::Dir,
                    rustix::fs::FileType::Symlink => EntryKind::NonDir(FileKind::Symlink),
                    rustix::fs::FileType::RegularFile => EntryKind::NonDir(FileKind::File),
                    _ => EntryKind::NonDir(FileKind::Other),
                };
                out.push(Entry {
                    name,
                    kind,
                    meta: Meta::default(),
                    link_target: None,
                    error: Some(("stat", err.raw_os_error(), err.to_string())),
                });
            }
        }
    }
    Ok(())
}
