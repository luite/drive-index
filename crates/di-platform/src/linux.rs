//! Linux: udisks2 over D-Bus for drives, partitions, filesystems, mounting and
//! power-off; sysfs and the udev database for USB details and block sizes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use anyhow::{Context, Result, bail};
use zbus::blocking::Connection;
use zbus::blocking::fdo::ObjectManagerProxy;
use zbus::zvariant::{OwnedValue, Value};

use crate::{
    AuthAnswer, Bus, DeviceEvent, DiskRef, DriveProbe, IdentitySource, Medium, PartitionProbe, UsbInfo, VolumeProbe,
};

const DEST: &str = "org.freedesktop.UDisks2";
const ROOT: &str = "/org/freedesktop/UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const PARTITION: &str = "org.freedesktop.UDisks2.Partition";
const PTABLE: &str = "org.freedesktop.UDisks2.PartitionTable";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const ENCRYPTED: &str = "org.freedesktop.UDisks2.Encrypted";
const LOOP: &str = "org.freedesktop.UDisks2.Loop";

type Props = HashMap<String, OwnedValue>;
type Ifaces = HashMap<String, Props>;

/// All udisks2 objects: path → interface → properties.
struct Objects(HashMap<String, Ifaces>);

fn connect() -> Result<Connection> {
    Connection::system().context("connecting to the system D-Bus (is udisks2 running?)")
}

impl Objects {
    fn load(conn: &Connection) -> Result<Objects> {
        let om = ObjectManagerProxy::builder(conn).destination(DEST)?.path(ROOT)?.build()?;
        let managed = om.get_managed_objects().context("udisks2 GetManagedObjects")?;
        Ok(Objects(
            managed
                .into_iter()
                .map(|(path, ifaces)| {
                    (path.as_str().to_string(), ifaces.into_iter().map(|(i, p)| (i.as_str().to_string(), p)).collect())
                })
                .collect(),
        ))
    }

    fn get(&self, path: &str, iface: &str, key: &str) -> Option<&Value<'static>> {
        self.0.get(path)?.get(iface)?.get(key).map(|v| &**v)
    }
    fn has(&self, path: &str, iface: &str) -> bool {
        self.0.get(path).is_some_and(|i| i.contains_key(iface))
    }
    fn str(&self, path: &str, iface: &str, key: &str) -> Option<String> {
        match self.get(path, iface, key)? {
            Value::Str(s) if !s.as_str().trim().is_empty() => Some(s.as_str().trim().to_string()),
            _ => None,
        }
    }
    fn u64(&self, path: &str, iface: &str, key: &str) -> Option<u64> {
        match self.get(path, iface, key)? {
            Value::U64(n) => Some(*n),
            Value::U32(n) => Some(*n as u64),
            Value::I64(n) if *n >= 0 => Some(*n as u64),
            Value::I32(n) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }
    fn i64(&self, path: &str, iface: &str, key: &str) -> Option<i64> {
        match self.get(path, iface, key)? {
            Value::I32(n) => Some(*n as i64),
            Value::I64(n) => Some(*n),
            Value::U32(n) => Some(*n as i64),
            _ => None,
        }
    }
    fn bool(&self, path: &str, iface: &str, key: &str) -> Option<bool> {
        match self.get(path, iface, key)? {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// Object-path property; `/` means none.
    fn path(&self, path: &str, iface: &str, key: &str) -> Option<String> {
        match self.get(path, iface, key)? {
            Value::ObjectPath(p) if p.as_str() != "/" => Some(p.as_str().to_string()),
            _ => None,
        }
    }
    /// `ay` property holding a NUL-terminated string.
    fn bytes(&self, path: &str, iface: &str, key: &str) -> Option<String> {
        bytes_of(self.get(path, iface, key)?)
    }
    /// `aay` property (e.g. MountPoints).
    fn bytes_list(&self, path: &str, iface: &str, key: &str) -> Vec<String> {
        match self.get(path, iface, key) {
            Some(Value::Array(a)) => a.iter().filter_map(bytes_of).collect(),
            _ => Vec::new(),
        }
    }

    fn blocks(&self) -> impl Iterator<Item = &String> {
        self.0.iter().filter(|(_, i)| i.contains_key(BLOCK)).map(|(p, _)| p)
    }

    /// Block object whose device is `/dev/<name>`.
    fn block_by_name(&self, name: &str) -> Option<String> {
        let dev = format!("/dev/{name}");
        self.blocks().find(|p| self.bytes(p, BLOCK, "Device").as_deref() == Some(dev.as_str())).cloned()
    }

    fn dev_name(&self, block: &str) -> Option<String> {
        self.bytes(block, BLOCK, "Device").map(|d| d.trim_start_matches("/dev/").to_string())
    }

    /// The whole-disk block of any block (partition, LUKS cleartext, whole disk).
    fn whole_of(&self, block: &str) -> String {
        let mut b = block.to_string();
        for _ in 0..4 {
            if let Some(backing) = self.path(&b, BLOCK, "CryptoBackingDevice") {
                b = backing;
            } else if let Some(table) = self.path(&b, PARTITION, "Table") {
                b = table;
            } else {
                break;
            }
        }
        b
    }

    fn is_whole(&self, block: &str) -> bool {
        !self.has(block, PARTITION)
            && self.path(block, BLOCK, "CryptoBackingDevice").is_none()
            && (self.path(block, BLOCK, "Drive").is_some() || self.has(block, LOOP))
            && self.u64(block, BLOCK, "Size").unwrap_or(0) > 0
    }
}

fn bytes_of(v: &Value) -> Option<String> {
    match v {
        Value::Array(a) => {
            let bytes: Vec<u8> = a
                .iter()
                .filter_map(|x| match x {
                    Value::U8(b) => Some(*b),
                    _ => None,
                })
                .take_while(|&b| b != 0)
                .collect();
            (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => None,
    }
}

fn value_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    if let Some(s) = bytes_of(v) {
        return J::String(s);
    }
    match v {
        Value::Bool(b) => J::Bool(*b),
        Value::U8(n) => J::from(*n),
        Value::I16(n) => J::from(*n),
        Value::U16(n) => J::from(*n),
        Value::I32(n) => J::from(*n),
        Value::U32(n) => J::from(*n),
        Value::I64(n) => J::from(*n),
        Value::U64(n) => J::from(*n),
        Value::F64(n) => J::from(*n),
        Value::Str(s) => J::String(s.as_str().to_string()),
        Value::ObjectPath(p) => J::String(p.as_str().to_string()),
        Value::Array(a) => J::Array(a.iter().map(value_json).collect()),
        other => J::String(format!("{other:?}")),
    }
}

fn props_json(o: &Objects, path: &str) -> serde_json::Value {
    let Some(ifaces) = o.0.get(path) else { return serde_json::Value::Null };
    serde_json::Value::Object(
        ifaces
            .iter()
            .map(|(i, props)| {
                let short = i.rsplit('.').next().unwrap_or(i).to_string();
                (short, serde_json::Value::Object(props.iter().map(|(k, v)| (k.clone(), value_json(v))).collect()))
            })
            .collect(),
    )
}

// -------------------------------------------------------------------------------------
// sysfs / udev helpers
// -------------------------------------------------------------------------------------

fn read_trim(p: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn sysfs_block(name: &str) -> PathBuf {
    PathBuf::from("/sys/class/block").join(name)
}

/// The kernel's attachment number of a whole disk (5.15 and later). See Note [Disks are
/// not their names] in di-platform.
fn diskseq(name: &str) -> Option<u64> {
    read_trim(sysfs_block(name).join("diskseq")).and_then(|s| s.parse().ok())
}

fn disk_ref(name: &str) -> DiskRef {
    DiskRef { name: name.to_string(), seq: diskseq(name) }
}

// Note [Rotational or not]
// ~~~~~~~~~~~~~~~~~~~~~~~~
// udisks' RotationRate comes from the drive's ATA IDENTIFY data (via udev), when that
// got through: 0 for solid state, the rpm for a hard disk, -1 when unknown.
//
// Otherwise there is the kernel's queue/rotational flag. For NVMe, MMC and virtio
// disks the driver sets it, and it is right. For SCSI disks (sd*: SATA through
// libata, USB, SAS) it defaults to 1 and is only cleared when the device's Block
// Device Characteristics VPD page (B1h) says "non-rotating". USB bridges often don't
// provide that page (usb-storage does not even ask for it), so a USB stick reads as
// rotational. For sd* devices we read the page ourselves (sysfs `vpd_pgb1`, bytes
// 4-5: 1 = non-rotating, 0x0401-0xfffe = rpm, 0 = not reported) and say "unknown"
// when it does not tell.
//
// A SMART reading can still fill in the medium later (`enrich` in di-scan:device).

/// Medium and rpm of a whole disk; `rotation_rate` is udisks' RotationRate.
/// See Note [Rotational or not].
fn medium(whole_name: &str, rotation_rate: Option<i64>) -> (Medium, Option<u32>) {
    match rotation_rate {
        Some(0) => return (Medium::Ssd, None),
        Some(n) if n > 0 => return (Medium::Hdd, u32::try_from(n).ok()),
        _ => {}
    }
    if whole_name.starts_with("sd") {
        let page = std::fs::read(sysfs_block(whole_name).join("device/vpd_pgb1")).unwrap_or_default();
        return medium_from_vpd_b1(&page);
    }
    match read_trim(sysfs_block(whole_name).join("queue/rotational")).as_deref() {
        Some("0") => (Medium::Ssd, None),
        Some("1") => (Medium::Hdd, None),
        _ => (Medium::Unknown, None),
    }
}

/// Medium rotation rate from a Block Device Characteristics VPD page.
fn medium_from_vpd_b1(page: &[u8]) -> (Medium, Option<u32>) {
    match page.get(4..6).map(|b| u16::from_be_bytes([b[0], b[1]])) {
        Some(1) => (Medium::Ssd, None),
        Some(rpm @ 0x0401..=0xfffe) => (Medium::Hdd, Some(rpm as u32)),
        _ => (Medium::Unknown, None),
    }
}

/// Properties from the udev database (`/run/udev/data/b<maj>:<min>`, world-readable).
fn udev_props(name: &str) -> HashMap<String, String> {
    let Some(devno) = read_trim(sysfs_block(name).join("dev")) else { return HashMap::new() };
    let Ok(data) = std::fs::read_to_string(format!("/run/udev/data/b{devno}")) else { return HashMap::new() };
    data.lines()
        .filter_map(|l| l.strip_prefix("E:"))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// USB device details: walk up from the block device to the USB device directory.
fn usb_info(name: &str) -> Option<UsbInfo> {
    let dev = std::fs::canonicalize(sysfs_block(name).join("device")).ok()?;
    let mut child: Option<PathBuf> = None;
    let mut dir = dev.as_path();
    loop {
        if dir.join("idVendor").exists() {
            let hex = |f: &str| read_trim(dir.join(f)).and_then(|s| u16::from_str_radix(&s, 16).ok());
            // The USB interface directory (just below the device) names the driver.
            let driver = child
                .as_ref()
                .and_then(|c| std::fs::read_link(c.join("driver")).ok())
                .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()));
            return Some(UsbInfo {
                vendor_id: hex("idVendor"),
                product_id: hex("idProduct"),
                serial: read_trim(dir.join("serial")),
                manufacturer: read_trim(dir.join("manufacturer")),
                product: read_trim(dir.join("product")),
                speed_mbps: read_trim(dir.join("speed")).and_then(|s| s.parse::<f64>().ok()).map(|s| s as u32),
                driver,
            });
        }
        child = Some(dir.to_path_buf());
        dir = dir.parent()?;
        if dir == Path::new("/sys/devices") || dir == Path::new("/") {
            return None;
        }
    }
}

fn normalize_fs(t: &str) -> String {
    match t {
        "crypto_LUKS" => "crypto_luks".into(),
        "BitLocker" => "bitlocker".into(),
        other => other.to_ascii_lowercase(),
    }
}

fn kernel_filesystems() -> HashSet<String> {
    std::fs::read_to_string("/proc/filesystems")
        .map(|s| s.lines().filter_map(|l| l.split_whitespace().last()).map(str::to_string).collect())
        .unwrap_or_default()
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(program).is_file()))
}

/// Why this host cannot mount `fs`, if it cannot.
fn unmountable_reason(fs: &str) -> Option<String> {
    match fs {
        "vfat" | "exfat" | "ntfs" | "ntfs3" | "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "hfsplus" | "hfs"
        | "iso9660" | "udf" | "f2fs" | "jfs" | "nilfs2" | "zfs_member_never" => None,
        "apfs" => {
            if kernel_filesystems().contains("apfs") || on_path("apfs-fuse") {
                None
            } else {
                Some("no APFS driver on this host (install apfs-fuse or linux-apfs-rw); scan it on a Mac".into())
            }
        }
        "crypto_luks" | "bitlocker" => Some("encrypted volume is locked".into()),
        other => Some(format!("{other} is not a supported filesystem")),
    }
}

/// Why a block without a recognised filesystem is worth reporting, if it is: a
/// partition, a disk without a partition table, or an unlocked encrypted volume that
/// should hold a filesystem but has none blkid knows. `ptype` is the partition's type.
fn unrecognised_reason(ptype: Option<&str>, whole_disk: bool, cleartext: bool) -> Option<String> {
    if whole_disk {
        return Some("no partition table or filesystem found: blank, or encrypted as a whole (e.g. VeraCrypt)".into());
    }
    if cleartext {
        return Some("no filesystem found inside the unlocked encrypted volume".into());
    }
    let ptype = ptype?;
    if di_model::parttypes::holds_no_filesystem(ptype) {
        return None;
    }
    let kind = match di_model::parttypes::name(ptype) {
        Some(name) => format!("{ptype}, {name}"),
        None => ptype.to_string(),
    };
    Some(format!("no filesystem signature found (partition type {kind}): unformatted, damaged or encrypted (e.g. VeraCrypt)"))
}

/// Mount options for a truly read-only mount.
fn ro_options(fs: &str) -> &'static str {
    match fs {
        "ext3" | "ext4" => "ro,noload",
        "xfs" => "ro,norecovery",
        _ => "ro",
    }
}

/// Mount point → is it mounted read-write? (from /proc/self/mountinfo)
fn mounted_rw(mount_point: &str) -> Option<bool> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    info.lines().find_map(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        (f.get(4) == Some(&mount_point)).then(|| f.get(5).is_some_and(|o| o.split(',').any(|x| x == "rw")))
    })
}

// -------------------------------------------------------------------------------------
// Public API
// -------------------------------------------------------------------------------------

pub fn list_drives() -> Result<Vec<String>> {
    let conn = connect()?;
    let o = Objects::load(&conn)?;
    let mut names: Vec<String> = o.blocks().filter(|b| o.is_whole(b)).filter_map(|b| o.dev_name(b)).collect();
    names.sort();
    Ok(names)
}

pub fn resolve_whole(dev: &str) -> Result<DiskRef> {
    let conn = connect()?;
    let o = Objects::load(&conn)?;
    let name = dev.trim_start_matches("/dev/");
    let block = o.block_by_name(name).with_context(|| format!("no such block device {name}"))?;
    let whole = o.dev_name(&o.whole_of(&block)).context("whole disk has no device name")?;
    Ok(disk_ref(&whole))
}

pub fn probe(dev: &str) -> Result<DriveProbe> {
    let conn = connect()?;
    let o = Objects::load(&conn)?;
    let name = dev.trim_start_matches("/dev/");
    let block = o.block_by_name(name).with_context(|| format!("no such block device {name}"))?;
    let whole = o.whole_of(&block);
    let whole_name = o.dev_name(&whole).context("whole disk has no device name")?;
    let drive = o.path(&whole, BLOCK, "Drive");
    let d = drive.as_deref().unwrap_or("");
    let udev = udev_props(&whole_name);

    // Blocks belonging to this disk: the disk, its partitions, and their cleartext devices.
    let mut members: Vec<String> = o.blocks().filter(|b| o.whole_of(b) == whole).cloned().collect();
    members.sort();
    let mount_points: Vec<String> = members.iter().flat_map(|b| o.bytes_list(b, FILESYSTEM, "MountPoints")).collect();
    // The running system's disk, and system-managed loop devices (snaps, etc.), are
    // never touched automatically.
    let backing_file = o.bytes(&whole, LOOP, "BackingFile").unwrap_or_default();
    let system_disk = mount_points.iter().any(|m| {
        matches!(m.as_str(), "/" | "/usr" | "/var" | "/home" | "/boot") || m.starts_with("/boot/") || m.starts_with("/snap/")
    }) || o.bool(&whole, BLOCK, "HintIgnore").unwrap_or(false)
        || backing_file.starts_with("/var/lib/snapd/");

    let conn_bus = o.str(d, DRIVE, "ConnectionBus").unwrap_or_default();
    let bus = match conn_bus.as_str() {
        "usb" => Bus::Usb,
        "sdio" | "mmc" => Bus::Sd,
        _ if o.has(&whole, LOOP) => Bus::Image,
        _ if whole_name.starts_with("nvme") => Bus::Nvme,
        _ if whole_name.starts_with("mmcblk") => Bus::Sd,
        _ if whole_name.starts_with("sd") => Bus::Sata,
        "" => Bus::Other("unknown".into()),
        other => Bus::Other(other.to_string()),
    };
    let (medium, rotation_rpm) = medium(&whole_name, o.i64(d, DRIVE, "RotationRate"));
    let removable = o.bool(d, DRIVE, "Removable").unwrap_or(false) || o.bool(d, DRIVE, "MediaRemovable").unwrap_or(false);
    let ptable_type = match o.str(&whole, PTABLE, "Type").as_deref() {
        Some("gpt") => Some("gpt".to_string()),
        Some("dos") => Some("mbr".to_string()),
        Some(other) => Some(other.to_string()),
        None => Some("none".to_string()),
    };

    let mut partitions = Vec::new();
    let mut volumes = Vec::new();
    for b in &members {
        let bname = o.dev_name(b).unwrap_or_default();
        let part_number = o.u64(b, PARTITION, "Number").map(|n| n as u32);
        if o.has(b, PARTITION) {
            partitions.push(PartitionProbe {
                number: part_number.unwrap_or(0),
                dev: bname.clone(),
                start_bytes: o.u64(b, PARTITION, "Offset").unwrap_or(0),
                size_bytes: o.u64(b, PARTITION, "Size").unwrap_or(0),
                type_name: o.str(b, PARTITION, "Type").as_deref().and_then(di_model::parttypes::name).map(str::to_string),
                type_id: o.str(b, PARTITION, "Type"),
                part_uuid: o.str(b, PARTITION, "UUID").filter(|_| ptable_type.as_deref() == Some("gpt")),
                part_label: o.str(b, PARTITION, "Name"),
            });
        }
        let usage = o.str(b, BLOCK, "IdUsage").unwrap_or_default();
        let fs_type = normalize_fs(&o.str(b, BLOCK, "IdType").unwrap_or_default());
        let cleartext_of = o.path(b, BLOCK, "CryptoBackingDevice");
        // The partition number of a cleartext device is that of its backing partition.
        let part_number = match &cleartext_of {
            Some(backing) => o.u64(backing, PARTITION, "Number").map(|n| n as u32),
            None => part_number,
        };
        let mount_point = o.bytes_list(b, FILESYSTEM, "MountPoints").into_iter().next();
        let mk = |reason: Option<String>| VolumeProbe {
            dev: bname.clone(),
            partition_number: part_number,
            container_uuid: None,
            fs_type: fs_type.clone(),
            fs_uuid: o.str(b, BLOCK, "IdUUID"),
            label: o.str(b, BLOCK, "IdLabel"),
            size_bytes: o.u64(b, BLOCK, "Size"),
            mounted_writable: mount_point.as_deref().and_then(mounted_rw),
            mount_point: mount_point.as_ref().map(PathBuf::from),
            encrypted: Some(cleartext_of.is_some()),
            locked: None,
            apfs_roles: Vec::new(),
            unscannable_reason: reason,
        };
        match usage.as_str() {
            "filesystem" => volumes.push(mk(unmountable_reason(&fs_type))),
            "crypto" => {
                // Unlocked volumes show up as their own cleartext block (handled above).
                if o.path(b, ENCRYPTED, "CleartextDevice").is_none() {
                    let mut v = mk(Some("encrypted volume is locked".into()));
                    v.encrypted = Some(true);
                    v.locked = Some(true);
                    volumes.push(v);
                }
            }
            // The UUID of a RAID, LVM or ZFS member names the array or pool, which all its
            // members share: it identifies no single disk.
            "raid" => {
                let mut v = mk(Some(format!("{fs_type} member: RAID, LVM and ZFS pools are not supported")));
                v.fs_uuid = None;
                volumes.push(v);
            }
            // Nothing blkid recognises. Reported, so a drive whose filesystem cannot be
            // read says so instead of looking empty.
            "" if fs_type.is_empty() && o.u64(b, BLOCK, "Size").unwrap_or(0) > 0 => {
                let container = o.bool(b, PARTITION, "IsContainer").unwrap_or(false);
                let whole_disk = *b == whole && !o.has(b, PTABLE) && members.len() == 1;
                let reason = if container {
                    None
                } else {
                    unrecognised_reason(o.str(b, PARTITION, "Type").as_deref(), whole_disk, cleartext_of.is_some())
                };
                if let Some(reason) = reason {
                    let mut v = mk(Some(reason));
                    v.fs_type = "unknown".into();
                    volumes.push(v);
                }
            }
            _ => {}
        }
    }
    partitions.sort_by_key(|p| p.number);
    volumes.sort_by_key(|v| (v.partition_number.unwrap_or(0), v.dev.clone()));

    let mut raw_blocks = serde_json::Map::new();
    for b in &members {
        raw_blocks.insert(o.dev_name(b).unwrap_or_default(), props_json(&o, b));
    }
    // udev reads a disk's ATA identify data, and then reports ID_BUS=ata, also through
    // USB bridges that pass it on. Otherwise (ID_BUS=usb) a USB disk's model and serial
    // are the bridge's. See Note [Disk facts] in di-model:identity.
    let identity_from = if bus == Bus::Usb && udev.get("ID_BUS").map(String::as_str) != Some("ata") {
        IdentitySource::Enclosure
    } else {
        IdentitySource::Disk
    };
    let raw = serde_json::json!({
        "drive": drive.as_deref().map(|p| props_json(&o, p)),
        "blocks": raw_blocks,
        "udev": udev,
    });

    Ok(DriveProbe {
        disk: disk_ref(&whole_name),
        dev_path: format!("/dev/{whole_name}"),
        vendor: o.str(d, DRIVE, "Vendor").or_else(|| udev.get("ID_VENDOR").cloned()),
        model: o.str(d, DRIVE, "Model").or_else(|| udev.get("ID_MODEL").cloned()),
        revision: o.str(d, DRIVE, "Revision"),
        serial: o.str(d, DRIVE, "Serial").or_else(|| udev.get("ID_SERIAL_SHORT").cloned()),
        wwn: o.str(d, DRIVE, "WWN").or_else(|| udev.get("ID_WWN").cloned()),
        identity_from,
        internal: conn_bus.is_empty() && !removable && bus != Bus::Image,
        removable,
        ejectable: o.bool(d, DRIVE, "Ejectable").unwrap_or(false)
            || o.bool(d, DRIVE, "CanPowerOff").unwrap_or(false)
            || bus == Bus::Image,
        system_disk,
        capacity_bytes: o.u64(d, DRIVE, "Size").filter(|&n| n > 0).or_else(|| o.u64(&whole, BLOCK, "Size")).unwrap_or(0),
        logical_block_size: read_trim(sysfs_block(&whole_name).join("queue/logical_block_size")).and_then(|s| s.parse().ok()),
        physical_block_size: read_trim(sysfs_block(&whole_name).join("queue/physical_block_size")).and_then(|s| s.parse().ok()),
        medium,
        rotation_rpm,
        usb: if bus == Bus::Usb { usb_info(&whole_name) } else { None },
        ptable_uuid: udev.get("ID_PART_TABLE_UUID").cloned(),
        ptable_type,
        partitions,
        volumes,
        bus,
        raw,
    })
}

fn call(conn: &Connection, path: &str, iface: &str, method: &str, opts: HashMap<&str, Value>) -> Result<zbus::message::Message> {
    Ok(conn.call_method(Some(DEST), path, Some(iface), method, &(opts,))?)
}

fn no_interaction() -> HashMap<&'static str, Value<'static>> {
    HashMap::from([("auth.no_user_interaction", Value::from(true))])
}

/// The kernel removes a disk's sysfs entry when it goes away. A detached loop device
/// keeps its entry (and /dev node) but has size 0. Another disk under the same name has
/// another `diskseq`.
pub fn is_present(drive: &DriveProbe) -> bool {
    let name = &drive.disk.name;
    diskseq(name) == drive.disk.seq
        && read_trim(sysfs_block(name).join("size")).and_then(|s| s.parse::<u64>().ok()).is_some_and(|n| n > 0)
}

pub fn mount_ro(vol: &VolumeProbe) -> Result<PathBuf> {
    if let Some(mp) = &vol.mount_point {
        return Ok(mp.clone());
    }
    if let Some(reason) = &vol.unscannable_reason {
        bail!("{} cannot be mounted: {reason}", vol.dev);
    }
    let conn = connect()?;
    let o = Objects::load(&conn)?;
    let block = o.block_by_name(&vol.dev).with_context(|| format!("no such block device {}", vol.dev))?;
    let mut tried = Vec::new();
    for opts in [ro_options(&vol.fs_type), "ro"] {
        if tried.contains(&opts) {
            continue;
        }
        tried.push(opts);
        let mut args = no_interaction();
        args.insert("options", Value::from(opts));
        match call(&conn, &block, FILESYSTEM, "Mount", args) {
            Ok(reply) => {
                let mp: String = reply.body().deserialize().context("Mount reply")?;
                if mounted_rw(&mp) == Some(true) {
                    bail!("{} mounted read-write despite the read-only request; unmount it manually", vol.dev);
                }
                return Ok(PathBuf::from(mp));
            }
            // udisks2 only allows whitelisted options; fall back to plain "ro".
            Err(e) if opts != "ro" && format!("{e:#}").contains("option") => continue,
            Err(e) => return Err(e).with_context(|| format!("mounting {} read-only", vol.dev)),
        }
    }
    bail!("could not mount {} read-only", vol.dev)
}

pub fn unmount_drive(drive: &DriveProbe, force: bool) -> Result<()> {
    let conn = connect()?;
    let o = Objects::load(&conn)?;
    let name = &drive.disk.name;
    let whole = o.block_by_name(name).with_context(|| format!("no such block device {name}"))?;
    for b in o.blocks().filter(|b| o.whole_of(b) == whole) {
        if !o.bytes_list(b, FILESYSTEM, "MountPoints").is_empty() {
            let mut args = no_interaction();
            args.insert("force", Value::from(force));
            call(&conn, b, FILESYSTEM, "Unmount", args)
                .with_context(|| format!("unmounting {}", o.dev_name(b).unwrap_or_default()))?;
        }
    }
    Ok(())
}

pub fn eject(drive: &DriveProbe) -> Result<()> {
    let conn = connect()?;
    let o = Objects::load(&conn)?;
    let name = &drive.disk.name;
    let whole = o.block_by_name(name).with_context(|| format!("no such block device {name}"))?;
    if o.has(&whole, LOOP) {
        call(&conn, &whole, LOOP, "Delete", no_interaction()).context("detaching loop device")?;
        return Ok(());
    }
    let d = o.path(&whole, BLOCK, "Drive").context("block device has no drive")?;
    if o.bool(&d, DRIVE, "CanPowerOff").unwrap_or(false) {
        call(&conn, &d, DRIVE, "PowerOff", no_interaction()).context("powering off drive")?;
    } else {
        call(&conn, &d, DRIVE, "Eject", no_interaction()).context("ejecting drive")?;
    }
    Ok(())
}

pub fn watch() -> Result<Receiver<DeviceEvent>> {
    let conn = connect()?;
    let o = Objects::load(&conn)?;
    let (tx, rx) = mpsc::channel();
    // Block object path → whole-disk name, so removals can be attributed after the fact.
    let names = std::sync::Arc::new(std::sync::Mutex::new(HashMap::<String, String>::new()));
    {
        let mut map = names.lock().unwrap();
        for b in o.blocks() {
            if let Some(n) = o.dev_name(&o.whole_of(b)) {
                map.insert(b.clone(), n);
            }
        }
        let wholes: HashSet<String> = o.blocks().filter(|b| o.is_whole(b)).filter_map(|b| o.dev_name(b)).collect();
        for w in wholes {
            let _ = tx.send(DeviceEvent::Appeared(w));
        }
    }

    let om = ObjectManagerProxy::builder(&conn).destination(DEST)?.path(ROOT)?.build()?;
    let added = om.receive_interfaces_added()?;
    let removed = om.receive_interfaces_removed()?;
    {
        let (tx, names, conn) = (tx.clone(), names.clone(), conn.clone());
        std::thread::Builder::new().name("udisks-added".into()).spawn(move || {
            for sig in added {
                let Ok(args) = sig.args() else { continue };
                let path = args.object_path().as_str().to_string();
                if !path.starts_with("/org/freedesktop/UDisks2/block_devices/") {
                    continue;
                }
                let Ok(o) = Objects::load(&conn) else { continue };
                let Some(whole) = o.0.contains_key(&path).then(|| o.dev_name(&o.whole_of(&path))).flatten() else {
                    continue;
                };
                names.lock().unwrap().insert(path, whole.clone());
                if tx.send(DeviceEvent::Appeared(whole)).is_err() {
                    break;
                }
            }
        })?;
    }
    std::thread::Builder::new().name("udisks-removed".into()).spawn(move || {
        for sig in removed {
            let Ok(args) = sig.args() else { continue };
            let path = args.object_path().as_str().to_string();
            let removes_block = args.interfaces().iter().any(|i| i.as_str() == BLOCK);
            if !removes_block {
                continue;
            }
            let whole = names.lock().unwrap().remove(&path);
            if let Some(whole) = whole {
                if tx.send(DeviceEvent::Disappeared(whole)).is_err() {
                    break;
                }
            }
        }
    })?;
    Ok(rx)
}

pub fn udisks_version() -> Result<String> {
    let conn = connect()?;
    let reply = conn.call_method(
        Some(DEST),
        "/org/freedesktop/UDisks2/Manager",
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &("org.freedesktop.UDisks2.Manager", "Version"),
    )?;
    let v: OwnedValue = reply.body().deserialize()?;
    match &*v {
        Value::Str(s) => Ok(s.as_str().to_string()),
        other => Ok(format!("{other:?}")),
    }
}

/// Process start time in clock ticks since boot (field 22 of /proc/self/stat), as
/// polkit's unix-process subject requires.
fn proc_start_time() -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let after_comm = stat.rsplit_once(')').map(|(_, r)| r).context("bad /proc/self/stat")?;
    after_comm
        .split_whitespace()
        .nth(19)
        .and_then(|f| f.parse().ok())
        .context("no start time in /proc/self/stat")
}

pub fn polkit_check(action: &str) -> Result<AuthAnswer> {
    let conn = connect()?;
    let subject = (
        "unix-process",
        HashMap::from([("pid", Value::from(std::process::id())), ("start-time", Value::from(proc_start_time()?))]),
    );
    let details: HashMap<&str, &str> = HashMap::new();
    let reply = conn.call_method(
        Some("org.freedesktop.PolicyKit1"),
        "/org/freedesktop/PolicyKit1/Authority",
        Some("org.freedesktop.PolicyKit1.Authority"),
        "CheckAuthorization",
        &(subject, action, details, 0u32, ""),
    )?;
    let (authorized, challenge, _details): (bool, bool, HashMap<String, String>) = reply.body().deserialize()?;
    Ok(if authorized {
        AuthAnswer::Yes
    } else if challenge {
        AuthAnswer::NeedsAuthentication
    } else {
        AuthAnswer::No
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpd_b1_rotation_rate() {
        let page = |rate: u16| {
            let [hi, lo] = rate.to_be_bytes();
            vec![0x00, 0xb1, 0x00, 0x3c, hi, lo, 0, 0]
        };
        assert_eq!(medium_from_vpd_b1(&page(1)), (Medium::Ssd, None));
        assert_eq!(medium_from_vpd_b1(&page(7200)), (Medium::Hdd, Some(7200)));
        // Not reported, reserved values, and no page at all: a USB stick, say.
        for p in [page(0), page(2), page(0xffff), vec![], vec![0, 0xb1, 0]] {
            assert_eq!(medium_from_vpd_b1(&p), (Medium::Unknown, None), "{p:?}");
        }
    }
}
