//! Device test on Linux with loop devices attached through udisks2 (no root needed in a
//! desktop session, or with contrib/polkit/50-drive-index.rules). Opt-in, it attaches
//! and detaches devices; needs sfdisk and udisksctl:
//!
//!     cargo test -p di-platform --test linux_loop -- --ignored --nocapture

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use di_platform::DriveProbe;

/// A 64 MiB disk image with one MBR partition of type 0x07 (NTFS / exFAT) and no
/// filesystem in it.
fn unformatted_ntfs_image(path: &Path) {
    std::fs::File::create(path).unwrap().set_len(64 << 20).unwrap();
    let mut sfdisk = Command::new("sfdisk")
        .args(["--quiet", path.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("sfdisk");
    use std::io::Write;
    sfdisk.stdin.take().unwrap().write_all(b"label: dos\ntype=7\n").unwrap();
    assert!(sfdisk.wait().unwrap().success(), "sfdisk failed");
}

/// Attach an image as a loop device; returns its name (`loop24`).
fn attach(image: &Path) -> String {
    let out = Command::new("udisksctl")
        .args(["loop-setup", "--no-user-interaction", "-f", image.to_str().unwrap()])
        .output()
        .expect("udisksctl");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "udisksctl loop-setup: {text}{}", String::from_utf8_lossy(&out.stderr));
    // "Mapped file /tmp/x.img as /dev/loop24."
    text.trim().trim_end_matches('.').rsplit("/dev/").next().unwrap().to_string()
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Probe once udisks shows the partition.
fn probe_with_partition(name: &str) -> DriveProbe {
    wait_for("the partition", || di_platform::probe(name).ok().filter(|p| !p.partitions.is_empty()))
}

#[test]
#[ignore]
fn unrecognised_partition_and_a_reused_name() {
    let tmp = tempfile::tempdir().unwrap();
    let image = tmp.path().join("unformatted.img");
    unformatted_ntfs_image(&image);

    let first = probe_with_partition(&attach(&image));
    println!("first: {:?}", first.disk);
    assert!(first.disk.seq.is_some(), "no diskseq (kernel before 5.15?)");
    // The partition is reported, with the reason nothing can be scanned on it.
    let [vol] = first.volumes.as_slice() else { panic!("volumes: {:?}", first.volumes) };
    assert_eq!(vol.fs_type, "unknown");
    let reason = vol.unscannable_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("no filesystem signature found (partition type 0x07, NTFS / exFAT)"), "{reason}");

    // Detach through our own eject (udisks Loop.Delete), then attach again. The kernel
    // hands out the lowest free loop device, usually the name just freed.
    di_platform::eject(&first).unwrap();
    wait_for("the detach", || (!di_platform::is_present(&first)).then_some(()));
    let second = probe_with_partition(&attach(&image));
    println!("second: {:?} (name reused: {})", second.disk, second.disk.name == first.disk.name);
    assert_ne!(second.disk, first.disk);

    // Anything decided for the first attachment is refused, even under its old name.
    assert!(di_platform::probe_ref(&first.disk).is_err());
    let err = di_platform::eject(&first).unwrap_err();
    println!("stale eject refused: {err:#}");
    assert!(!di_platform::is_present(&first));
    assert!(di_platform::is_present(&second), "the second attachment was left alone");
    assert_eq!(di_platform::probe_ref(&second.disk).unwrap().disk, second.disk);

    di_platform::eject(&second).unwrap();
}
