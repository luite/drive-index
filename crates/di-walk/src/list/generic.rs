//! Portable lister: `read_dir` + `symlink_metadata` per entry.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

use super::{Entry, EntryKind, Lister, Meta, kind_from_mode, ts_ns};

pub struct GenericLister;

impl Lister for GenericLister {
    fn method(&self) -> &'static str {
        "generic"
    }

    fn list(&self, dir: &Path, out: &mut Vec<Entry>, _buf: &mut Vec<u8>) -> io::Result<()> {
        out.clear();
        for de in fs::read_dir(dir)? {
            let de = de?;
            let name = de.file_name().as_bytes().to_vec();
            let path = de.path();
            match fs::symlink_metadata(&path) {
                Ok(md) => {
                    let (kind, meta) = from_std(&md);
                    let link_target = if kind == EntryKind::NonDir(di_model::FileKind::Symlink) {
                        fs::read_link(&path).ok().map(|p| p.as_os_str().as_bytes().to_vec())
                    } else {
                        None
                    };
                    out.push(Entry { name, kind, meta, link_target, error: None });
                }
                Err(e) => {
                    // Fall back to the dirent type so directories are still walked.
                    let kind = match de.file_type() {
                        Ok(t) if t.is_dir() => EntryKind::Dir,
                        Ok(t) if t.is_symlink() => EntryKind::NonDir(di_model::FileKind::Symlink),
                        _ => EntryKind::NonDir(di_model::FileKind::File),
                    };
                    out.push(Entry {
                        name,
                        kind,
                        meta: Meta::default(),
                        link_target: None,
                        error: Some(("stat", e.raw_os_error(), e.to_string())),
                    });
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn from_std(md: &fs::Metadata) -> (EntryKind, Meta) {
    let mode = md.mode();
    let kind = kind_from_mode(mode);
    let btime_ns = md
        .created()
        .ok()
        .and_then(|t| match t.duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).ok(),
            Err(e) => i64::try_from(e.duration().as_nanos()).ok().map(|n| -n),
        });

    #[cfg(target_os = "macos")]
    let flags = {
        use std::os::macos::fs::MetadataExt as _;
        super::map_bsd_flags(md.st_flags())
    };
    #[cfg(not(target_os = "macos"))]
    let flags = 0;

    let meta = Meta {
        size: md.size(),
        alloc: Some(md.blocks().saturating_mul(512)),
        mtime_ns: ts_ns(md.mtime(), md.mtime_nsec()),
        ctime_ns: ts_ns(md.ctime(), md.ctime_nsec()),
        atime_ns: ts_ns(md.atime(), md.atime_nsec()),
        btime_ns,
        mode: Some(mode),
        uid: Some(md.uid()),
        gid: Some(md.gid()),
        inode: Some(md.ino()),
        nlink: Some(md.nlink()),
        dev: Some(md.dev()),
        flags,
        mount_point: false,
        entry_count: None,
    };
    (kind, meta)
}
