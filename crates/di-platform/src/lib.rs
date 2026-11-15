//! Platform device layer: find drives, describe them, mount their
//! filesystems read-only, unmount, eject, and report hot-plug events.
//!
//! * macOS: DiskArbitration for events; `diskutil … -plist` and the IORegistry for
//!   metadata; `diskutil` for mount / unmount / eject (it drives DiskArbitration).
//! * Linux: udisks2 over D-Bus (`GetManagedObjects`, `Filesystem.Mount`, …), sysfs
//!   for USB details.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

pub mod identity;
pub mod smart;
pub mod util;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
use macos as sys;

/// Transport of a drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Bus {
    Usb,
    Thunderbolt,
    Sata,
    Nvme,
    Sd,
    /// Disk image (macOS) or loop device (Linux).
    Image,
    Other(String),
}

impl Bus {
    pub fn as_str(&self) -> &str {
        match self {
            Bus::Usb => "usb",
            Bus::Thunderbolt => "thunderbolt",
            Bus::Sata => "sata",
            Bus::Nvme => "nvme",
            Bus::Sd => "sd",
            Bus::Image => "image",
            Bus::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Medium {
    Hdd,
    Ssd,
    Unknown,
}

impl Medium {
    pub fn as_str(self) -> &'static str {
        match self {
            Medium::Hdd => "hdd",
            Medium::Ssd => "ssd",
            Medium::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UsbInfo {
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub speed_mbps: Option<u32>,
    /// `uas` or `usb-storage` (Linux).
    pub driver: Option<String>,
}

/// One partition-table entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartitionProbe {
    pub number: u32,
    pub dev: String,
    pub start_bytes: u64,
    pub size_bytes: u64,
    /// GPT type GUID or MBR type (`0x07`), when the platform reports it.
    pub type_id: Option<String>,
    /// Human name: `Apple_APFS`, `Microsoft Basic Data`, …
    pub type_name: Option<String>,
    /// GPT unique partition GUID.
    pub part_uuid: Option<String>,
    pub part_label: Option<String>,
}

/// One filesystem (APFS volumes each count separately).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeProbe {
    /// Device identifier to mount: `disk7s2`, `disk8s1`, `/dev/sdb1`.
    pub dev: String,
    /// Partition holding it (for APFS: the container's physical store partition).
    pub partition_number: Option<u32>,
    pub container_uuid: Option<String>,
    /// Normalized: apfs, hfsplus, exfat, vfat, ntfs, ext4, btrfs, xfs, crypto_luks, bitlocker, …
    pub fs_type: String,
    pub fs_uuid: Option<String>,
    pub label: Option<String>,
    pub size_bytes: Option<u64>,
    pub mount_point: Option<PathBuf>,
    /// Some(true) when currently mounted read-write.
    pub mounted_writable: Option<bool>,
    pub encrypted: Option<bool>,
    pub locked: Option<bool>,
    pub apfs_roles: Vec<String>,
    /// Can this host mount and scan it? If not, why.
    pub unscannable_reason: Option<String>,
}

impl VolumeProbe {
    pub fn scannable(&self) -> bool {
        self.unscannable_reason.is_none()
    }
}

/// Everything known about one whole drive.
#[derive(Debug, Clone, Serialize)]
pub struct DriveProbe {
    /// Platform id of the whole disk: `disk7` (macOS), `sdb` (Linux).
    pub id: String,
    pub dev_path: String,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub revision: Option<String>,
    pub serial: Option<String>,
    pub wwn: Option<String>,
    pub bus: Bus,
    pub internal: bool,
    pub removable: bool,
    pub ejectable: bool,
    /// The disk holds the running system (never touched automatically).
    pub system_disk: bool,
    pub capacity_bytes: u64,
    pub logical_block_size: Option<u32>,
    pub physical_block_size: Option<u32>,
    pub medium: Medium,
    pub rotation_rpm: Option<u32>,
    pub usb: Option<UsbInfo>,
    /// gpt | mbr | apm | none
    pub ptable_type: Option<String>,
    pub ptable_uuid: Option<String>,
    pub partitions: Vec<PartitionProbe>,
    pub volumes: Vec<VolumeProbe>,
    /// Verbatim platform metadata, stored as `raw_info`.
    pub raw: serde_json::Value,
}

impl DriveProbe {
    /// Candidate for automatic handling: external or removable, not the system disk.
    pub fn is_external(&self) -> bool {
        !self.system_disk && (!self.internal || self.removable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A whole disk appeared (also emitted for every disk present at startup).
    Appeared(String),
    Disappeared(String),
}

/// Whole-disk ids of all drives the platform knows about.
pub fn list_drives() -> anyhow::Result<Vec<String>> {
    sys::list_drives()
}

/// Cheaply map any disk/partition/volume identifier to its physical whole disk id.
pub fn resolve_whole(dev: &str) -> anyhow::Result<String> {
    sys::resolve_whole(dev)
}

/// Describe one whole disk (accepts `disk7`, `/dev/disk7`, `sdb`, `/dev/sdb`, or any
/// partition/volume of it).
pub fn probe(dev: &str) -> anyhow::Result<DriveProbe> {
    let mut p = sys::probe(dev)?;
    mark_efi(&mut p);
    Ok(p)
}

/// EFI system partitions hold boot files, not user data, and the empty ones Disk
/// Utility creates often refuse to mount: record them, but don't scan them.
fn mark_efi(p: &mut DriveProbe) {
    let efi: Vec<u32> = p
        .partitions
        .iter()
        .filter(|x| {
            let name = x.type_name.as_deref().unwrap_or("");
            name.eq_ignore_ascii_case("EFI") || name == "EFI System"
        })
        .map(|x| x.number)
        .collect();
    for v in &mut p.volumes {
        if v.unscannable_reason.is_none() && v.partition_number.is_some_and(|n| efi.contains(&n)) {
            v.unscannable_reason = Some("EFI system partition (boot files, not user data)".into());
        }
    }
}

/// Mount a volume read-only (and hidden from Finder on macOS). Returns the mount point.
/// If it is already mounted, returns the existing mount point unchanged.
pub fn mount_ro(vol: &VolumeProbe) -> anyhow::Result<PathBuf> {
    sys::mount_ro(vol)
}

/// Unmount every volume of a drive.
pub fn unmount_drive(drive: &DriveProbe, force: bool) -> anyhow::Result<()> {
    sys::unmount_drive(drive, force)
}

/// Eject / power off a drive (all its volumes must be unmounted).
pub fn eject(drive: &DriveProbe) -> anyhow::Result<()> {
    sys::eject(drive)
}

/// Start watching for drives. Events arrive on the returned channel until it is dropped.
pub fn watch() -> anyhow::Result<std::sync::mpsc::Receiver<DeviceEvent>> {
    sys::watch()
}

/// Default timeout for external commands (diskutil, …).
pub const CMD_TIMEOUT: Duration = Duration::from_secs(60);

/// Answer from the platform's authorization service for an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AuthAnswer {
    Yes,
    /// Allowed after the user authenticates (works in a desktop session, not over SSH).
    NeedsAuthentication,
    No,
}

/// udisks2 daemon version (Linux).
#[cfg(target_os = "linux")]
pub fn udisks_version() -> anyhow::Result<String> {
    linux::udisks_version()
}

/// Would polkit allow this process to perform `action` (e.g.
/// `org.freedesktop.udisks2.filesystem-mount`) without asking? (Linux)
#[cfg(target_os = "linux")]
pub fn polkit_check(action: &str) -> anyhow::Result<AuthAnswer> {
    linux::polkit_check(action)
}
