//! Host and filesystem facts: hostname, time zone, statfs.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use di_model::FsUsage;

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "unknown-host".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    // Strip a trailing ".local" (macOS) so the same machine has one stable name.
    name.strip_suffix(".local").map(str::to_owned).unwrap_or(name)
}

/// IANA time zone name of this host, best effort (`TZ`, then `/etc/localtime`).
pub fn time_zone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return Some(tz.trim_start_matches(':').to_string());
        }
    }
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let s = target.to_string_lossy();
    s.find("zoneinfo/").map(|i| s[i + "zoneinfo/".len()..].to_string())
}

/// Filesystem type, mount point and usage of the filesystem holding `path`.
pub fn fs_usage(path: &Path) -> FsUsage {
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        return FsUsage { fs_type: "unknown".into(), ..Default::default() };
    };
    imp::fs_usage(&cpath, path)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    fn cstr_field(field: &[libc::c_char]) -> String {
        let bytes: Vec<u8> = field.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn fs_usage(cpath: &CString, _path: &Path) -> FsUsage {
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(cpath.as_ptr(), &mut st) } != 0 {
            return FsUsage { fs_type: "unknown".into(), ..Default::default() };
        }
        let bsize = st.f_bsize as u64;
        FsUsage {
            fs_type: cstr_field(&st.f_fstypename),
            mount_point: Some(cstr_field(&st.f_mntonname)),
            size_bytes: Some(st.f_blocks * bsize),
            used_bytes: Some(st.f_blocks.saturating_sub(st.f_bfree) * bsize),
            free_bytes: Some(st.f_bavail * bsize),
            block_size: Some(bsize),
            inodes_total: Some(st.f_files),
            inodes_used: Some(st.f_files.saturating_sub(st.f_ffree)),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn fs_usage(cpath: &CString, path: &Path) -> FsUsage {
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let (fs_type, mount_point) = mount_for(path).unwrap_or_else(|| ("unknown".into(), None));
        if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } != 0 {
            return FsUsage { fs_type, mount_point, ..Default::default() };
        }
        let frsize = st.f_frsize as u64;
        let blocks = st.f_blocks as u64;
        FsUsage {
            fs_type,
            mount_point,
            size_bytes: Some(blocks * frsize),
            used_bytes: Some(blocks.saturating_sub(st.f_bfree as u64) * frsize),
            free_bytes: Some(st.f_bavail as u64 * frsize),
            block_size: Some(frsize),
            inodes_total: Some(st.f_files as u64),
            inodes_used: Some((st.f_files as u64).saturating_sub(st.f_ffree as u64)),
        }
    }

    /// Longest mount point in /proc/self/mountinfo that contains `path`.
    fn mount_for(path: &Path) -> Option<(String, Option<String>)> {
        let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let mut best: Option<(usize, String, String)> = None;
        for line in info.lines() {
            // id parent maj:min root mount_point options [optional...] - fstype source superopts
            let fields: Vec<&str> = line.split(' ').collect();
            let Some(sep) = fields.iter().position(|f| *f == "-") else { continue };
            let (Some(mp), Some(fstype)) = (fields.get(4), fields.get(sep + 1)) else { continue };
            let mp = unescape_mount(mp);
            if path.starts_with(&mp) && best.as_ref().is_none_or(|(len, _, _)| mp.len() > *len) {
                best = Some((mp.len(), fstype.to_string(), mp));
            }
        }
        best.map(|(_, t, mp)| (t, Some(mp)))
    }

    /// mountinfo escapes space, tab, newline and backslash as \ooo.
    fn unescape_mount(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
                out.push(v);
                i += 4;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}
