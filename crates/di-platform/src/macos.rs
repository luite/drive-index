//! macOS: diskutil + IORegistry for metadata, diskutil for mount / unmount / eject,
//! DiskArbitration callbacks for hot-plug events.

use std::collections::HashSet;
use std::ffi::{CStr, c_void};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};

use crate::util::run;
use crate::{
    Bus, CMD_TIMEOUT, DeviceEvent, DriveProbe, Medium, PartitionProbe, UsbInfo, VolumeProbe,
};

// -------------------------------------------------------------------------------------
// plist helpers
// -------------------------------------------------------------------------------------

fn diskutil_plist(args: &[&str]) -> Result<Value> {
    let out = run("/usr/sbin/diskutil", args, CMD_TIMEOUT)?;
    Value::from_reader(std::io::Cursor::new(out.stdout)).with_context(|| format!("parsing diskutil {}", args.join(" ")))
}

fn info(dev: &str) -> Result<Dictionary> {
    diskutil_plist(&["info", "-plist", dev])?
        .into_dictionary()
        .context("diskutil info: not a dictionary")
}

fn s(d: &Dictionary, k: &str) -> Option<String> {
    d.get(k).and_then(Value::as_string).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

fn u(d: &Dictionary, k: &str) -> Option<u64> {
    d.get(k).and_then(|v| v.as_unsigned_integer().or_else(|| v.as_signed_integer().map(|i| i as u64)))
}

fn b(d: &Dictionary, k: &str) -> Option<bool> {
    d.get(k).and_then(Value::as_boolean)
}

fn arr<'a>(d: &'a Dictionary, k: &str) -> &'a [Value] {
    d.get(k).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub(crate) fn to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Array(a) => J::Array(a.iter().map(to_json).collect()),
        Value::Dictionary(d) => J::Object(d.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
        Value::Boolean(b) => J::Bool(*b),
        Value::Data(d) => J::String(d.iter().map(|b| format!("{b:02x}")).collect()),
        Value::Date(d) => J::String(format!("{d:?}")),
        Value::Real(r) => serde_json::Number::from_f64(*r).map(J::Number).unwrap_or(J::Null),
        Value::Integer(i) => i
            .as_signed()
            .map(J::from)
            .or_else(|| i.as_unsigned().map(J::from))
            .unwrap_or(J::Null),
        Value::String(s) => J::String(s.clone()),
        Value::Uid(u) => J::from(u.get()),
        _ => J::Null,
    }
}

fn strip_dev(dev: &str) -> String {
    let d = dev.strip_prefix("/dev/").unwrap_or(dev);
    d.strip_prefix('r').filter(|r| r.starts_with("disk")).unwrap_or(d).to_string()
}

/// Partition number from an identifier like `disk7s2`.
fn slice_number(id: &str) -> Option<u32> {
    let rest = id.strip_prefix("disk")?;
    let (_, n) = rest.split_once('s')?;
    n.parse().ok()
}

// -------------------------------------------------------------------------------------
// Listing and probing
// -------------------------------------------------------------------------------------

/// Whole disks that are synthesized APFS containers (not physical drives).
fn synthesized(list: &Dictionary) -> HashSet<String> {
    arr(list, "AllDisksAndPartitions")
        .iter()
        .filter_map(Value::as_dictionary)
        .filter(|e| e.contains_key("APFSPhysicalStores"))
        .filter_map(|e| s(e, "DeviceIdentifier"))
        .collect()
}

pub fn list_drives() -> Result<Vec<String>> {
    let list = diskutil_plist(&["list", "-plist"])?.into_dictionary().context("diskutil list")?;
    let synth = synthesized(&list);
    Ok(arr(&list, "WholeDisks")
        .iter()
        .filter_map(Value::as_string)
        .filter(|d| !synth.contains(*d))
        .map(str::to_string)
        .collect())
}

/// Map every identifier in `diskutil list` (whole disks, partitions, APFS volumes) to
/// its whole disk, and every synthesized APFS container to its physical store.
struct Topology {
    whole_of: std::collections::HashMap<String, String>,
    store_of_container: std::collections::HashMap<String, String>,
}

impl Topology {
    fn from_list(list: &Dictionary) -> Topology {
        let mut whole_of = std::collections::HashMap::new();
        let mut store_of_container = std::collections::HashMap::new();
        for e in arr(list, "AllDisksAndPartitions").iter().filter_map(Value::as_dictionary) {
            let Some(whole) = s(e, "DeviceIdentifier") else { continue };
            whole_of.insert(whole.clone(), whole.clone());
            for key in ["Partitions", "APFSVolumes"] {
                for p in arr(e, key).iter().filter_map(Value::as_dictionary) {
                    if let Some(id) = s(p, "DeviceIdentifier") {
                        whole_of.insert(id, whole.clone());
                    }
                }
            }
            if let Some(store) = arr(e, "APFSPhysicalStores")
                .iter()
                .filter_map(Value::as_dictionary)
                .find_map(|p| s(p, "DeviceIdentifier"))
            {
                store_of_container.insert(whole.clone(), store);
            }
        }
        Topology { whole_of, store_of_container }
    }

    /// Physical whole disk of any identifier, following APFS containers to their store.
    fn physical(&self, id: &str) -> Option<String> {
        let mut whole = self.whole_of.get(id)?.clone();
        for _ in 0..4 {
            match self.store_of_container.get(&whole) {
                Some(store) => whole = self.whole_of.get(store)?.clone(),
                None => break,
            }
        }
        Some(whole)
    }
}

/// Physical whole disks holding the running system (computed once per process).
fn system_disks(topo: &Topology) -> &'static HashSet<String> {
    static CACHE: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let mut out = HashSet::new();
        for path in ["/", "/System/Volumes/Data"] {
            if let Some(w) = info(path).ok().and_then(|i| s(&i, "DeviceIdentifier")).and_then(|id| topo.physical(&id)) {
                out.insert(w);
            }
        }
        out
    })
}

/// `diskutil info` for several identifiers in parallel (each call takes ~100 ms).
fn info_many(ids: &[String]) -> std::collections::HashMap<String, Dictionary> {
    std::thread::scope(|sc| {
        let handles: Vec<_> = ids.iter().map(|id| (id.clone(), sc.spawn(move || info(id).ok()))).collect();
        handles
            .into_iter()
            .filter_map(|(id, h)| h.join().ok().flatten().map(|d| (id, d)))
            .collect()
    })
}

fn normalize_fs(t: &str) -> String {
    match t {
        "hfs" => "hfsplus".into(),
        "msdos" => "vfat".into(),
        other => other.to_ascii_lowercase(),
    }
}

/// Filesystems macOS can mount (NTFS read-only).
fn host_can_mount(fs: &str) -> bool {
    matches!(fs, "apfs" | "hfsplus" | "exfat" | "vfat" | "ntfs" | "udf" | "cd9660")
}

/// Partition contents that never hold a scannable filesystem.
fn non_fs_partition(content: &str) -> bool {
    matches!(
        content,
        "Microsoft Reserved" | "Apple_Boot" | "Apple_Free" | "Linux Swap" | "Linux_Swap" | "BIOS Boot" | "Apple_KernelCoreDump" | "Apple_APFS_ISC" | "Apple_APFS_Recovery"
    )
}

pub fn resolve_whole(dev: &str) -> Result<String> {
    let dev = strip_dev(dev);
    let list = diskutil_plist(&["list", "-plist"])?.into_dictionary().context("diskutil list")?;
    Topology::from_list(&list).physical(&dev).with_context(|| format!("no such disk {dev}"))
}

pub fn probe(dev: &str) -> Result<DriveProbe> {
    let dev = strip_dev(dev);
    let list = diskutil_plist(&["list", "-plist"])?.into_dictionary().context("diskutil list")?;
    let topo = Topology::from_list(&list);
    let whole = topo.physical(&dev).with_context(|| format!("no such disk {dev}"))?;
    let entry = arr(&list, "AllDisksAndPartitions")
        .iter()
        .filter_map(Value::as_dictionary)
        .find(|e| s(e, "DeviceIdentifier").as_deref() == Some(whole.as_str()))
        .cloned()
        .unwrap_or_default();
    let part_ids: Vec<String> = arr(&entry, "Partitions")
        .iter()
        .filter_map(Value::as_dictionary)
        .filter_map(|p| s(p, "DeviceIdentifier"))
        .collect();
    let has_apfs = arr(&entry, "Partitions")
        .iter()
        .filter_map(Value::as_dictionary)
        .any(|p| s(p, "Content").as_deref() == Some("Apple_APFS"));

    // Everything below is independent: run it concurrently.
    let (wi, mut infos, apfs, ioreg, system) = std::thread::scope(|sc| {
        let wi = sc.spawn(|| info(&whole));
        let infos = sc.spawn(|| info_many(&part_ids));
        let apfs = sc.spawn(|| {
            if has_apfs {
                diskutil_plist(&["apfs", "list", "-plist"]).ok().and_then(Value::into_dictionary).unwrap_or_default()
            } else {
                Dictionary::new()
            }
        });
        let ioreg = sc.spawn(|| IoregInfo::for_disk(&whole));
        let system = sc.spawn(|| system_disks(&topo).contains(&whole));
        (wi.join(), infos.join(), apfs.join(), ioreg.join(), system.join())
    });
    let wi = wi.map_err(|_| anyhow::anyhow!("probe thread panicked"))??;
    let infos = infos.as_mut().map_err(|_| anyhow::anyhow!("probe thread panicked"))?;
    let apfs = apfs.unwrap_or_default();
    let ioreg = ioreg.map_err(|_| anyhow::anyhow!("probe thread panicked"))?;
    let system_disk = system.unwrap_or(false);

    let chars = ioreg.characteristics.clone().unwrap_or_default();
    let proto = ioreg.protocol.clone().unwrap_or_default();

    let bus_protocol = s(&wi, "BusProtocol").unwrap_or_default();
    let interconnect = s(&proto, "Physical Interconnect").unwrap_or_default();
    let bus = match (bus_protocol.as_str(), interconnect.as_str()) {
        ("Disk Image", _) => Bus::Image,
        ("USB", _) | (_, "USB") => Bus::Usb,
        ("Thunderbolt", _) | (_, "Thunderbolt") => Bus::Thunderbolt,
        ("Secure Digital", _) | (_, "Secure Digital") => Bus::Sd,
        ("SATA", _) | (_, "SATA") => Bus::Sata,
        ("PCI-Express", _) | ("PCI", _) | ("Apple Fabric", _) | ("NVMe", _) => Bus::Nvme,
        (other, _) => Bus::Other(other.to_string()),
    };
    let medium = match (s(&chars, "Medium Type").as_deref(), b(&wi, "SolidState")) {
        (Some("Rotational"), _) => Medium::Hdd,
        (Some("Solid State"), _) | (_, Some(true)) => Medium::Ssd,
        _ => Medium::Unknown,
    };
    let ptable_type = match s(&wi, "Content").as_deref() {
        Some("GUID_partition_scheme") => Some("gpt".to_string()),
        Some("FDisk_partition_scheme") => Some("mbr".to_string()),
        Some("Apple_partition_scheme") => Some("apm".to_string()),
        _ => Some("none".to_string()),
    };

    // Partitions and their filesystems.
    let mut partitions = Vec::new();
    let mut volumes = Vec::new();
    let mut raw_parts = Vec::new();
    for p in arr(&entry, "Partitions").iter().filter_map(Value::as_dictionary) {
        let Some(pid) = s(p, "DeviceIdentifier") else { continue };
        let pi = infos.remove(&pid).unwrap_or_default();
        let number = slice_number(&pid).unwrap_or(0);
        let content = s(&pi, "Content").or_else(|| s(p, "Content")).unwrap_or_default();
        partitions.push(PartitionProbe {
            number,
            dev: pid.clone(),
            start_bytes: u(&pi, "PartitionMapPartitionOffset").unwrap_or(0),
            size_bytes: u(&pi, "Size").or_else(|| u(p, "Size")).unwrap_or(0),
            type_id: None,
            type_name: Some(content.clone()).filter(|c| !c.is_empty()),
            part_uuid: if ptable_type.as_deref() == Some("gpt") { s(&pi, "DiskUUID") } else { None },
            part_label: None,
        });

        if let Some(container) = s(&pi, "APFSContainerReference") {
            volumes.extend(apfs_volumes(&apfs, &container, number));
        } else if let Some(fs) = s(&pi, "FilesystemType") {
            volumes.push(volume_from_info(&pid, Some(number), &normalize_fs(&fs), &pi));
        } else if !content.is_empty() && !non_fs_partition(&content) {
            volumes.push(VolumeProbe {
                dev: pid.clone(),
                partition_number: Some(number),
                container_uuid: None,
                fs_type: content.to_ascii_lowercase().replace(' ', "_"),
                fs_uuid: None,
                label: None,
                size_bytes: u(&pi, "Size"),
                mount_point: None,
                mounted_writable: None,
                encrypted: None,
                locked: None,
                apfs_roles: Vec::new(),
                unscannable_reason: Some(format!(
                    "no macOS driver for this filesystem ({content}); scan it on a Linux host"
                )),
            });
        }
        raw_parts.push(to_json(&Value::Dictionary(pi)));
    }
    // A filesystem directly on the whole disk (no partition table).
    if partitions.is_empty() {
        if let Some(fs) = s(&wi, "FilesystemType") {
            volumes.push(volume_from_info(&whole, None, &normalize_fs(&fs), &wi));
        }
    }

    let model = s(&chars, "Product Name").or_else(|| s(&wi, "MediaName"));
    let raw = serde_json::json!({
        "diskutil_info": to_json(&Value::Dictionary(wi.clone())),
        "diskutil_list": to_json(&Value::Dictionary(entry.clone())),
        "partitions": raw_parts,
        "device_characteristics": to_json(&Value::Dictionary(chars.clone())),
        "protocol_characteristics": to_json(&Value::Dictionary(proto.clone())),
        "usb": ioreg.usb_raw,
    });

    Ok(DriveProbe {
        id: whole.clone(),
        dev_path: format!("/dev/{whole}"),
        vendor: s(&chars, "Vendor Name"),
        model,
        revision: s(&chars, "Product Revision Level"),
        serial: s(&chars, "Serial Number"),
        wwn: None,
        internal: b(&wi, "Internal").unwrap_or(false),
        removable: b(&wi, "RemovableMedia").unwrap_or(false) || b(&wi, "Removable").unwrap_or(false),
        ejectable: b(&wi, "Ejectable").unwrap_or(false),
        system_disk,
        capacity_bytes: u(&wi, "Size").or_else(|| u(&wi, "TotalSize")).unwrap_or(0),
        logical_block_size: u(&wi, "DeviceBlockSize").map(|v| v as u32),
        physical_block_size: None,
        medium,
        rotation_rpm: None,
        usb: ioreg.usb,
        ptable_type,
        ptable_uuid: None,
        partitions,
        volumes,
        bus,
        raw,
    })
}

fn volume_from_info(dev: &str, partition: Option<u32>, fs: &str, i: &Dictionary) -> VolumeProbe {
    let mount_point = s(i, "MountPoint").map(PathBuf::from);
    let unscannable_reason =
        (!host_can_mount(fs)).then(|| format!("no macOS driver for {fs}; scan it on a Linux host"));
    VolumeProbe {
        dev: dev.to_string(),
        partition_number: partition,
        container_uuid: None,
        fs_type: fs.to_string(),
        fs_uuid: s(i, "VolumeUUID"),
        label: s(i, "VolumeName"),
        size_bytes: u(i, "Size"),
        mounted_writable: mount_point.as_ref().map(|_| b(i, "WritableVolume").unwrap_or(false)),
        mount_point,
        encrypted: None,
        locked: None,
        apfs_roles: Vec::new(),
        unscannable_reason,
    }
}

fn apfs_volumes(apfs: &Dictionary, container_ref: &str, partition: u32) -> Vec<VolumeProbe> {
    let Some(c) = arr(apfs, "Containers")
        .iter()
        .filter_map(Value::as_dictionary)
        .find(|c| s(c, "ContainerReference").as_deref() == Some(container_ref))
    else {
        return Vec::new();
    };
    let container_uuid = s(c, "APFSContainerUUID");
    let vol_ids: Vec<String> =
        arr(c, "Volumes").iter().filter_map(Value::as_dictionary).filter_map(|v| s(v, "DeviceIdentifier")).collect();
    let mut infos = info_many(&vol_ids);
    arr(c, "Volumes")
        .iter()
        .filter_map(Value::as_dictionary)
        .filter_map(|v| {
            let dev = s(v, "DeviceIdentifier")?;
            let vi = infos.remove(&dev).unwrap_or_default();
            let roles: Vec<String> =
                arr(v, "Roles").iter().filter_map(Value::as_string).map(str::to_string).collect();
            let locked = b(v, "Locked");
            let encrypted = Some(b(v, "FileVault").unwrap_or(false) || b(v, "Encryption").unwrap_or(false));
            let mut vol = volume_from_info(&dev, Some(partition), "apfs", &vi);
            vol.container_uuid = container_uuid.clone();
            vol.fs_uuid = s(v, "APFSVolumeUUID").or(vol.fs_uuid);
            vol.label = s(v, "Name").or(vol.label);
            vol.size_bytes = u(v, "CapacityInUse").or(vol.size_bytes);
            vol.encrypted = encrypted;
            vol.locked = locked;
            if locked == Some(true) {
                vol.unscannable_reason = Some("encrypted volume is locked".into());
            } else if roles.iter().any(|r| r == "Preboot" || r == "Recovery" || r == "VM" || r == "Update") {
                vol.unscannable_reason = Some(format!("APFS {} volume (system support, not user data)", roles.join("/")));
            }
            vol.apfs_roles = roles;
            Some(vol)
        })
        .collect()
}

/// IORegistry facts for a whole disk: device/protocol characteristics and USB details.
struct IoregInfo {
    characteristics: Option<Dictionary>,
    protocol: Option<Dictionary>,
    usb: Option<UsbInfo>,
    usb_raw: serde_json::Value,
}

impl IoregInfo {
    fn for_disk(bsd: &str) -> IoregInfo {
        let mut out = IoregInfo { characteristics: None, protocol: None, usb: None, usb_raw: serde_json::Value::Null };
        if let Some(path) = ioreg_path("IOBlockStorageDevice", bsd) {
            for node in path.iter().rev() {
                if out.characteristics.is_none() {
                    out.characteristics = node.get("Device Characteristics").and_then(Value::as_dictionary).cloned();
                }
                if out.protocol.is_none() {
                    out.protocol = node.get("Protocol Characteristics").and_then(Value::as_dictionary).cloned();
                }
            }
        }
        if let Some(path) = ioreg_path("IOUSBHostDevice", bsd) {
            if let Some(dev) = path.first() {
                let num = |k: &str| u(dev, k).map(|v| v as u16);
                let speed = u(dev, "Device Speed").or_else(|| u(dev, "UsbDeviceSpeed")).map(|code| match code {
                    0 => 1,
                    1 => 12,
                    2 => 480,
                    3 => 5000,
                    4 => 10000,
                    5 => 20000,
                    n => n as u32,
                });
                out.usb = Some(UsbInfo {
                    vendor_id: num("idVendor"),
                    product_id: num("idProduct"),
                    serial: s(dev, "USB Serial Number").or_else(|| s(dev, "kUSBSerialNumberString")),
                    manufacturer: s(dev, "USB Vendor Name").or_else(|| s(dev, "kUSBVendorString")),
                    product: s(dev, "USB Product Name").or_else(|| s(dev, "kUSBProductString")),
                    speed_mbps: speed,
                    driver: None,
                });
                let mut shallow = dev.clone();
                shallow.remove("IORegistryEntryChildren");
                out.usb_raw = to_json(&Value::Dictionary(shallow));
            }
        }
        out
    }
}

/// The chain of IORegistry nodes from a `class` root down to the IOMedia whose
/// `BSD Name` is `bsd`.
fn ioreg_path(class: &str, bsd: &str) -> Option<Vec<Dictionary>> {
    let out = run("/usr/sbin/ioreg", &["-a", "-r", "-c", class, "-l", "-w0"], CMD_TIMEOUT).ok()?;
    if out.stdout.is_empty() {
        return None;
    }
    let roots = Value::from_reader(std::io::Cursor::new(out.stdout)).ok()?.into_array()?;
    fn find(node: &Dictionary, bsd: &str, path: &mut Vec<Dictionary>) -> bool {
        path.push(node.clone());
        if s(node, "BSD Name").as_deref() == Some(bsd) {
            return true;
        }
        for child in arr(node, "IORegistryEntryChildren").iter().filter_map(Value::as_dictionary) {
            if find(child, bsd, path) {
                return true;
            }
        }
        path.pop();
        false
    }
    for root in roots.iter().filter_map(Value::as_dictionary) {
        let mut path = Vec::new();
        if find(root, bsd, &mut path) {
            for p in &mut path {
                p.remove("IORegistryEntryChildren");
            }
            return Some(path);
        }
    }
    None
}

// -------------------------------------------------------------------------------------
// Mount / unmount / eject
// -------------------------------------------------------------------------------------

pub fn mount_ro(vol: &VolumeProbe) -> Result<PathBuf> {
    if let Some(mp) = &vol.mount_point {
        return Ok(mp.clone());
    }
    if let Some(reason) = &vol.unscannable_reason {
        bail!("{} cannot be mounted: {reason}", vol.dev);
    }
    run("/usr/sbin/diskutil", &["mount", "readOnly", "nobrowse", &vol.dev], CMD_TIMEOUT)
        .with_context(|| format!("mounting {} read-only", vol.dev))?;
    let i = info(&vol.dev)?;
    let mp = s(&i, "MountPoint").with_context(|| format!("{} did not mount", vol.dev))?;
    if b(&i, "WritableVolume") == Some(true) {
        bail!("{} mounted read-write despite the read-only request; unmount it manually", vol.dev);
    }
    Ok(PathBuf::from(mp))
}

pub fn unmount_drive(drive: &DriveProbe, force: bool) -> Result<()> {
    // Unmounting the physical disk also unmounts APFS volumes of containers on it.
    let mut args = vec!["unmountDisk"];
    if force {
        args.push("force");
    }
    args.push(&drive.id);
    run("/usr/sbin/diskutil", &args, CMD_TIMEOUT).with_context(|| format!("unmounting {}", drive.id))?;
    Ok(())
}

pub fn eject(drive: &DriveProbe) -> Result<()> {
    run("/usr/sbin/diskutil", &["eject", &drive.id], CMD_TIMEOUT).with_context(|| format!("ejecting {}", drive.id))?;
    Ok(())
}

// -------------------------------------------------------------------------------------
// Hot-plug events (DiskArbitration)
// -------------------------------------------------------------------------------------

mod da {
    use super::*;
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use objc2_disk_arbitration::{
        DADisk, DARegisterDiskAppearedCallback, DARegisterDiskDisappearedCallback, DASession,
    };

    fn whole_name(disk: &DADisk) -> Option<String> {
        unsafe {
            let whole = disk.whole_disk();
            let d: &DADisk = whole.as_deref().unwrap_or(disk);
            let p = d.bsd_name();
            (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    }

    unsafe extern "C-unwind" fn appeared(disk: NonNull<DADisk>, ctx: *mut c_void) {
        let tx = unsafe { &*(ctx as *const Sender<DeviceEvent>) };
        if let Some(name) = whole_name(unsafe { disk.as_ref() }) {
            if tx.send(DeviceEvent::Appeared(name)).is_err() {
                if let Some(rl) = CFRunLoop::current() {
                    rl.stop();
                }
            }
        }
    }

    unsafe extern "C-unwind" fn disappeared(disk: NonNull<DADisk>, ctx: *mut c_void) {
        let tx = unsafe { &*(ctx as *const Sender<DeviceEvent>) };
        if let Some(name) = whole_name(unsafe { disk.as_ref() }) {
            if tx.send(DeviceEvent::Disappeared(name)).is_err() {
                if let Some(rl) = CFRunLoop::current() {
                    rl.stop();
                }
            }
        }
    }

    pub fn run_session(tx: Sender<DeviceEvent>, ready: Sender<Result<()>>) {
        unsafe {
            let Some(session) = DASession::new(None) else {
                let _ = ready.send(Err(anyhow::anyhow!("DASessionCreate failed")));
                return;
            };
            let ctx = Box::into_raw(Box::new(tx)) as *mut c_void;
            DARegisterDiskAppearedCallback(&session, None, Some(appeared), ctx);
            DARegisterDiskDisappearedCallback(&session, None, Some(disappeared), ctx);
            let (Some(rl), Some(mode)) = (CFRunLoop::current(), kCFRunLoopDefaultMode) else {
                let _ = ready.send(Err(anyhow::anyhow!("no run loop")));
                return;
            };
            session.schedule_with_run_loop(&rl, mode);
            let _ = ready.send(Ok(()));
            CFRunLoop::run();
            session.unschedule_from_run_loop(&rl, mode);
            drop(Box::from_raw(ctx as *mut Sender<DeviceEvent>));
        }
    }
}

pub fn watch() -> Result<Receiver<DeviceEvent>> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("diskarbitration".into())
        .spawn(move || da::run_session(tx, ready_tx))?;
    ready_rx.recv().context("DiskArbitration thread exited")??;
    Ok(rx)
}
