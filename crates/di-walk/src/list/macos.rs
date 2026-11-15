//! macOS fast path: `getattrlistbulk(2)`.
//!
//! One call fills a buffer with many directory entries, each carrying the requested
//! attributes, so no per-file `lstat` is needed. Layout of each entry:
//!
//! ```text
//! u32 length | attribute_set_t returned | [u32 error] | common attrs (bit order)
//!            | dir attrs | file attrs
//! ```
//!
//! Only attributes present in `returned` are packed (we do not pass
//! `FSOPT_PACK_INVAL_ATTRS`), each 4-byte aligned. `ATTR_CMN_ERROR`, when present,
//! immediately follows the returned-attributes set.

use std::ffi::{CString, c_int, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use di_model::{FileKind, flags};

use super::{Entry, EntryKind, GenericLister, Lister, Meta, map_bsd_flags, ts_ns};

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: c_int,
        attr_list: *mut AttrList,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: u64,
    ) -> c_int;
}

const ATTR_BIT_MAP_COUNT: u16 = 5;

const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_CRTIME: u32 = 0x0000_0200;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_CHGTIME: u32 = 0x0000_0800;
const ATTR_CMN_ACCTIME: u32 = 0x0000_1000;
const ATTR_CMN_OWNERID: u32 = 0x0000_8000;
const ATTR_CMN_GRPID: u32 = 0x0001_0000;
const ATTR_CMN_ACCESSMASK: u32 = 0x0002_0000;
const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

const ATTR_DIR_ENTRYCOUNT: u32 = 0x0000_0002;
const ATTR_DIR_MOUNTSTATUS: u32 = 0x0000_0004;
const DIR_MNTSTATUS_MNTPOINT: u32 = 0x0000_0001;

const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;
const ATTR_FILE_RSRCLENGTH: u32 = 0x0000_1000;

// fsobj_type_t (enum vtype)
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VBLK: u32 = 3;
const VCHR: u32 = 4;
const VLNK: u32 = 5;
const VSOCK: u32 = 6;
const VFIFO: u32 = 7;

const BUF_SIZE: usize = 256 * 1024;

pub struct BulkLister {
    fallback: GenericLister,
}

impl BulkLister {
    pub fn new() -> Self {
        BulkLister { fallback: GenericLister }
    }
}

impl Lister for BulkLister {
    fn method(&self) -> &'static str {
        "getattrlistbulk"
    }

    fn list(&self, dir: &Path, out: &mut Vec<Entry>, buf: &mut Vec<u8>) -> io::Result<()> {
        match list_bulk(dir, out, buf) {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(e.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::EINVAL))
                    || e.kind() == io::ErrorKind::InvalidData =>
            {
                // Filesystem without bulk support, or a buffer we could not parse.
                self.fallback.list(dir, out, buf)
            }
            Err(e) => Err(e),
        }
    }
}

struct Fd(c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

fn list_bulk(dir: &Path, out: &mut Vec<Entry>, buf: &mut Vec<u8>) -> io::Result<()> {
    out.clear();
    let cpath = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = Fd(fd);

    let mut al = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_ERROR
            | ATTR_CMN_DEVID
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_CRTIME
            | ATTR_CMN_MODTIME
            | ATTR_CMN_CHGTIME
            | ATTR_CMN_ACCTIME
            | ATTR_CMN_OWNERID
            | ATTR_CMN_GRPID
            | ATTR_CMN_ACCESSMASK
            | ATTR_CMN_FLAGS
            | ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: ATTR_DIR_ENTRYCOUNT | ATTR_DIR_MOUNTSTATUS,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE | ATTR_FILE_DATALENGTH | ATTR_FILE_RSRCLENGTH,
        forkattr: 0,
    };

    if buf.len() < BUF_SIZE {
        buf.resize(BUF_SIZE, 0);
    }

    loop {
        let n = unsafe { getattrlistbulk(fd.0, &mut al, buf.as_mut_ptr() as *mut c_void, buf.len(), 0) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            break;
        }
        let mut pos = 0usize;
        for _ in 0..n {
            let len = read_u32(buf, pos)? as usize;
            if len < 4 || pos + len > buf.len() {
                return Err(bad("entry length out of bounds"));
            }
            let entry_buf = &buf[pos..pos + len];
            let mut entry = parse_entry(entry_buf)?;
            if entry.kind == EntryKind::NonDir(FileKind::Symlink) {
                entry.link_target = readlinkat(fd.0, &entry.name);
            }
            out.push(entry);
            pos += len;
        }
    }
    Ok(())
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_u32(buf: &[u8], pos: usize) -> io::Result<u32> {
    buf.get(pos..pos + 4)
        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
        .ok_or_else(|| bad("truncated attribute buffer"))
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos + n).ok_or_else(|| bad("truncated entry"))?;
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_ne_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_ne_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_ne_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_ne_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn timespec(&mut self) -> io::Result<Option<i64>> {
        let sec = self.i64()?;
        let nsec = self.i64()?;
        Ok(ts_ns(sec, nsec))
    }
}

fn parse_entry(e: &[u8]) -> io::Result<Entry> {
    let mut c = Cursor { buf: e, pos: 4 };
    let r_common = c.u32()?;
    let _r_vol = c.u32()?;
    let r_dir = c.u32()?;
    let r_file = c.u32()?;
    let _r_fork = c.u32()?;

    let mut error = None;
    if r_common & ATTR_CMN_ERROR != 0 {
        let errno = c.u32()? as i32;
        if errno != 0 {
            error = Some(("stat", Some(errno), io::Error::from_raw_os_error(errno).to_string()));
        }
    }

    let mut name = Vec::new();
    if r_common & ATTR_CMN_NAME != 0 {
        let ref_pos = c.pos;
        let off = c.i32()?;
        let len = c.u32()? as usize;
        let start = (ref_pos as i64 + off as i64) as usize;
        let bytes = e.get(start..start + len).ok_or_else(|| bad("name out of bounds"))?;
        // Length includes the terminating NUL.
        name = bytes.split(|&b| b == 0).next().unwrap_or(&[]).to_vec();
    }
    if name.is_empty() {
        return Err(bad("entry without name"));
    }

    let mut meta = Meta::default();
    let mut objtype = 0;
    if r_common & ATTR_CMN_DEVID != 0 {
        // dev_t is i32; sign-extend exactly like std's MetadataExt::dev().
        meta.dev = Some(c.i32()? as u64);
    }
    if r_common & ATTR_CMN_OBJTYPE != 0 {
        objtype = c.u32()?;
    }
    if r_common & ATTR_CMN_CRTIME != 0 {
        meta.btime_ns = c.timespec()?;
    }
    if r_common & ATTR_CMN_MODTIME != 0 {
        meta.mtime_ns = c.timespec()?;
    }
    if r_common & ATTR_CMN_CHGTIME != 0 {
        meta.ctime_ns = c.timespec()?;
    }
    if r_common & ATTR_CMN_ACCTIME != 0 {
        meta.atime_ns = c.timespec()?;
    }
    if r_common & ATTR_CMN_OWNERID != 0 {
        meta.uid = Some(c.u32()?);
    }
    if r_common & ATTR_CMN_GRPID != 0 {
        meta.gid = Some(c.u32()?);
    }
    let mut perms = None;
    if r_common & ATTR_CMN_ACCESSMASK != 0 {
        perms = Some(c.u32()? & 0o7777);
    }
    if r_common & ATTR_CMN_FLAGS != 0 {
        meta.flags |= map_bsd_flags(c.u32()?);
    }
    if r_common & ATTR_CMN_FILEID != 0 {
        meta.inode = Some(c.u64()?);
    }

    // Directory attributes
    if r_dir & ATTR_DIR_ENTRYCOUNT != 0 {
        meta.entry_count = Some(c.u32()?);
    }
    if r_dir & ATTR_DIR_MOUNTSTATUS != 0 {
        meta.mount_point = c.u32()? & DIR_MNTSTATUS_MNTPOINT != 0;
    }

    // File attributes
    if r_file & ATTR_FILE_LINKCOUNT != 0 {
        meta.nlink = Some(c.u32()? as u64);
    }
    if r_file & ATTR_FILE_ALLOCSIZE != 0 {
        meta.alloc = Some(c.i64()?.max(0) as u64);
    }
    if r_file & ATTR_FILE_DATALENGTH != 0 {
        meta.size = c.i64()?.max(0) as u64;
    }
    if r_file & ATTR_FILE_RSRCLENGTH != 0 && c.i64()? > 0 {
        meta.flags |= flags::HAS_RSRC_FORK;
    }

    let (kind, type_bits) = match objtype {
        VDIR => (EntryKind::Dir, 0o040000),
        VREG => (EntryKind::NonDir(FileKind::File), 0o100000),
        VLNK => (EntryKind::NonDir(FileKind::Symlink), 0o120000),
        VFIFO => (EntryKind::NonDir(FileKind::Fifo), 0o010000),
        VSOCK => (EntryKind::NonDir(FileKind::Socket), 0o140000),
        VCHR => (EntryKind::NonDir(FileKind::CharDev), 0o020000),
        VBLK => (EntryKind::NonDir(FileKind::BlockDev), 0o060000),
        _ => (EntryKind::NonDir(FileKind::Other), 0),
    };
    meta.mode = perms.map(|p| p | type_bits);

    Ok(Entry { name, kind, meta, link_target: None, error })
}

fn readlinkat(dirfd: c_int, name: &[u8]) -> Option<Vec<u8>> {
    let cname = CString::new(name).ok()?;
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::readlinkat(dirfd, cname.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if n < 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    /// The bulk path must agree with lstat, without silently falling back.
    #[test]
    fn bulk_matches_lstat() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello world").unwrap();
        fs::write(root.join("caf\u{0065}\u{0301}.jpg"), vec![7u8; 70_000]).unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("ln")).unwrap();
        for i in 0..2000 {
            // Enough entries to need several getattrlistbulk calls.
            fs::write(root.join(format!("f{i:05}.dat")), vec![0u8; i % 97]).unwrap();
        }

        let mut out = Vec::new();
        let mut buf = Vec::new();
        list_bulk(root, &mut out, &mut buf).expect("bulk listing works on this filesystem");
        assert_eq!(out.len(), 2004);

        let by_name: HashMap<Vec<u8>, &Entry> = out.iter().map(|e| (e.name.clone(), e)).collect();
        for de in fs::read_dir(root).unwrap() {
            let de = de.unwrap();
            let name = de.file_name().as_bytes().to_vec();
            let e = by_name.get(&name).unwrap_or_else(|| panic!("missing {:?}", de.file_name()));
            let (kind, m) = crate::stat_path(&de.path()).unwrap();
            assert_eq!(e.kind, kind, "{:?}", de.file_name());
            assert_eq!(e.meta.inode, m.inode);
            assert_eq!(e.meta.mtime_ns, m.mtime_ns);
            assert_eq!(e.meta.btime_ns, m.btime_ns);
            assert_eq!(e.meta.mode, m.mode);
            assert_eq!(e.meta.uid, m.uid);
            if kind != EntryKind::Dir {
                assert_eq!(e.meta.size, m.size, "{:?}", de.file_name());
                assert_eq!(e.meta.alloc, m.alloc, "{:?}", de.file_name());
                assert_eq!(e.meta.nlink, m.nlink);
            }
            assert!(e.error.is_none());
        }
        let sub = by_name[&b"sub".to_vec()];
        assert_eq!(sub.meta.entry_count, Some(0));
        let ln = by_name[&b"ln".to_vec()];
        assert_eq!(ln.link_target.as_deref(), Some(&b"a.txt"[..]));
    }
}
