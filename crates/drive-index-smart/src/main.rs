//! `drive-index-smart`: runs `smartctl -j -i -H -A [-d TYPE] /dev/DISK` as root,
//! and nothing else, so drive-index can read SMART health as a normal user on Linux.
//!
//! It is meant to be installed setuid root, executable only by one group (see the
//! README, "SMART on Linux without root"). Everything it accepts is fixed or
//! checked before it gains anything:
//!
//! - the options are exactly `-j -i -H -A`: identify, health and attributes, all
//!   read-only queries;
//! - the device type comes from an allow-list;
//! - the device is a whole disk (`sdX`, `nvmeNnM`, `vdX`, `hdX`): a block device
//!   in /dev owned by root, not a symlink;
//! - smartctl is run from a fixed location, owned by root and not writable by others,
//!   with an empty environment.

use std::process::ExitCode;

/// The options drive-index passes, in this order.
const FIXED: &[&str] = &["-j", "-i", "-H", "-A"];

/// smartctl `-d` types drive-index tries.
const TYPES: &[&str] = &[
    "auto", "ata", "scsi", "nvme", "sat", "sntjmicron", "sntasmedia", "sntrealtek", "usbjmicron", "usbcypress", "usbprolific",
    "usbsunplus",
];

/// Where smartctl may live.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SMARTCTL: &[&str] = &["/usr/sbin/smartctl", "/usr/bin/smartctl", "/sbin/smartctl", "/usr/local/sbin/smartctl"];

/// Whole-disk device names: `sda`, `sdab`, `nvme0n1`, `vda`, `hda`.
fn whole_disk_name(name: &str) -> bool {
    let letters = |s: &str| !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_lowercase());
    let digits = |s: &str| !s.is_empty() && s.len() <= 4 && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = name.strip_prefix("nvme") {
        return rest.split_once('n').is_some_and(|(c, n)| digits(c) && digits(n));
    }
    ["sd", "vd", "hd"].iter().any(|p| name.strip_prefix(p).is_some_and(letters))
}

/// Parse `-j -i -H -A [-d TYPE] DEVICE` into (type, device name).
fn parse_args(args: &[String]) -> Result<(Option<&str>, &str), String> {
    let n = FIXED.len();
    if args.len() < n + 1 || args[..n].iter().zip(FIXED).any(|(a, f)| a != f) {
        return Err("expected -j -i -H -A first".into());
    }
    let (dtype, dev) = match &args[n..] {
        [dev] => (None, dev.as_str()),
        [d, t, dev] if d == "-d" => {
            if !TYPES.contains(&t.as_str()) {
                return Err(format!("device type '{t}' is not allowed"));
            }
            (Some(t.as_str()), dev.as_str())
        }
        _ => return Err("unexpected arguments".into()),
    };
    let name = dev.strip_prefix("/dev/").ok_or_else(|| format!("{dev}: the device must be /dev/DISK"))?;
    if !whole_disk_name(name) {
        return Err(format!("{dev} is not a whole disk (sdX, nvmeNnM, vdX or hdX)"));
    }
    Ok((dtype, dev))
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("drive-index-smart: {msg}");
    eprintln!("usage: drive-index-smart -j -i -H -A [-d TYPE] /dev/DISK");
    ExitCode::from(64)
}

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let args: Vec<String> = match std::env::args_os().skip(1).map(|a| a.into_string()).collect() {
        Ok(a) => a,
        Err(_) => return usage("arguments must be UTF-8"),
    };
    let (dtype, dev) = match parse_args(&args) {
        Ok(x) => x,
        Err(e) => return usage(&e),
    };
    match std::fs::symlink_metadata(dev) {
        Ok(m) if m.file_type().is_block_device() && m.uid() == 0 => {}
        Ok(_) => return usage(&format!("{dev} is not a root-owned block device")),
        Err(e) => return usage(&format!("{dev}: {e}")),
    }
    let trusted = |p: &str| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0 && m.mode() & 0o111 != 0)
    };
    let Some(smartctl) = SMARTCTL.iter().copied().find(|p| trusted(p)) else {
        eprintln!("drive-index-smart: no root-owned smartctl in {} (install smartmontools)", SMARTCTL.join(", "));
        return ExitCode::from(69);
    };

    // Setuid sets only the effective user; become root fully and drop the caller's
    // supplementary groups so smartctl runs as an ordinary root process.
    // SAFETY: plain libc calls with valid arguments.
    unsafe {
        if libc::geteuid() == 0 && (libc::setgroups(0, std::ptr::null()) != 0 || libc::setresgid(0, 0, 0) != 0 || libc::setresuid(0, 0, 0) != 0)
        {
            eprintln!("drive-index-smart: could not switch to root");
            return ExitCode::from(71);
        }
    }
    let mut cmd = Command::new(smartctl);
    cmd.args(FIXED);
    if let Some(t) = dtype {
        cmd.args(["-d", t]);
    }
    cmd.arg(dev).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LC_ALL", "C").stdin(Stdio::null());
    let err = cmd.exec();
    eprintln!("drive-index-smart: running {smartctl}: {err}");
    ExitCode::from(71)
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    let _ = (parse_args, usage);
    eprintln!("drive-index-smart is only needed on Linux; on macOS drive-index runs smartctl directly.");
    ExitCode::from(69)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn accepts_only_the_fixed_query() {
        assert_eq!(parse_args(&args("-j -i -H -A /dev/sdb")), Ok((None, "/dev/sdb")));
        assert_eq!(parse_args(&args("-j -i -H -A -d sat /dev/sdb")), Ok((Some("sat"), "/dev/sdb")));
        assert_eq!(parse_args(&args("-j -i -H -A /dev/nvme0n1")), Ok((None, "/dev/nvme0n1")));
        for bad in [
            "-j -i -H -A",
            "-i -j -H -A /dev/sdb",
            "-j -i -H -A -t long /dev/sdb",
            "-j -i -H -A -s on /dev/sdb",
            "-j -i -H -A -d sat,auto /dev/sdb",
            "-j -i -H -A -d sat /dev/sdb /dev/sdc",
            "-j -i -H -A /dev/sdb1",
            "-j -i -H -A /dev/nvme0n1p1",
            "-j -i -H -A /dev/nvme0",
            "-j -i -H -A /dev/../etc/shadow",
            "-j -i -H -A /tmp/sdb",
            "-j -i -H -A sdb",
            "-j -i -H -A /dev/loop0",
        ] {
            assert!(parse_args(&args(bad)).is_err(), "accepted: {bad}");
        }
    }
}
