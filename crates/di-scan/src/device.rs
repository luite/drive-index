//! Scanning a whole drive (PLAN.md §2.1): probe → identify → record → mount each
//! filesystem read-only → scan → unmount → eject.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use di_core::Catalog;
use di_core::drives::{DriveMatch, DriveRecord, FsRecord, PartitionRecord};
use di_platform::{DriveProbe, VolumeProbe, identity};
use di_walk::{Control, Progress, WalkConfig, host};

use crate::run::{FsTarget, PhaseCell, ResumeMode, ScanOutcome, ScanStatus, run_fs_scan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    KeepMounted,
    Unmount,
    Eject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityChoice {
    /// Match automatically; ambiguous matches are an error.
    Auto,
    /// Treat as a new drive even if identifiers match an existing one.
    New,
    /// Treat as this existing drive.
    SameAs(i64),
}

#[derive(Debug, Clone)]
pub struct DeviceScanRequest {
    pub catalog_path: PathBuf,
    pub dev: String,
    pub label: Option<String>,
    pub walk: WalkConfig,
    pub resume: ResumeMode,
    pub after: After,
    pub identity: IdentityChoice,
    /// Allow internal (non-removable) disks. The system disk is always refused.
    pub allow_internal: bool,
}

#[derive(Debug)]
pub enum VolumeOutcome {
    Scanned(Box<ScanOutcome>),
    Skipped(String),
    Failed(String),
}

#[derive(Debug)]
pub struct VolumeReport {
    pub dev: String,
    pub fs_type: String,
    pub label: Option<String>,
    pub filesystem_id: i64,
    /// Mounted read-write by the OS before we started (scanned as-is).
    pub was_mounted_rw: bool,
    pub outcome: VolumeOutcome,
}

#[derive(Debug)]
pub struct DeviceReport {
    pub drive_id: i64,
    /// Whole-disk device path (`/dev/disk7`).
    pub dev_path: String,
    pub new_drive: bool,
    pub weak_match: bool,
    pub volumes: Vec<VolumeReport>,
    pub unmounted: bool,
    pub ejected: bool,
    pub interrupted: bool,
}

/// Progress notifications for the caller (CLI line, TUI).
#[derive(Debug)]
pub enum Step<'a> {
    Identified { drive_id: i64, new: bool, probe: &'a DriveProbe },
    Mounting(&'a VolumeProbe),
    Scanning { vol: &'a VolumeProbe, mount_point: &'a std::path::Path },
    Scanned(&'a VolumeProbe, &'a ScanOutcome),
    Unmounting,
    Ejecting,
}

pub fn to_record(p: &DriveProbe) -> DriveRecord {
    let scheme = p.ptable_type.clone().unwrap_or_else(|| "none".into());
    DriveRecord {
        vendor: p.vendor.clone(),
        model: p.model.clone(),
        serial: p.serial.clone(),
        wwn: p.wwn.clone(),
        firmware: p.revision.clone(),
        bus: p.bus.as_str().to_string(),
        transport_driver: p.usb.as_ref().and_then(|u| u.driver.clone()),
        usb_vendor_id: p.usb.as_ref().and_then(|u| u.vendor_id),
        usb_product_id: p.usb.as_ref().and_then(|u| u.product_id),
        usb_serial: p.usb.as_ref().and_then(|u| u.serial.clone()),
        usb_manufacturer: p.usb.as_ref().and_then(|u| u.manufacturer.clone()),
        usb_product: p.usb.as_ref().and_then(|u| u.product.clone()),
        usb_speed_mbps: p.usb.as_ref().and_then(|u| u.speed_mbps),
        capacity_bytes: p.capacity_bytes,
        logical_block_size: p.logical_block_size,
        physical_block_size: p.physical_block_size,
        medium: p.medium.as_str().to_string(),
        rotation_rpm: p.rotation_rpm,
        removable: p.removable,
        ptable_type: p.ptable_type.clone(),
        ptable_uuid: p.ptable_uuid.clone(),
        raw_info: serde_json::to_string(&p.raw).ok(),
        identifiers: identity::identifiers(p),
        partitions: p
            .partitions
            .iter()
            .map(|x| PartitionRecord {
                number: x.number,
                scheme: scheme.clone(),
                start_bytes: x.start_bytes,
                size_bytes: x.size_bytes,
                type_id: x.type_id.clone(),
                type_name: x.type_name.clone(),
                part_uuid: x.part_uuid.clone(),
                part_label: x.part_label.clone(),
            })
            .collect(),
    }
}

pub fn to_fs_record(v: &VolumeProbe) -> FsRecord {
    FsRecord {
        partition_number: v.partition_number,
        container_uuid: v.container_uuid.clone(),
        fs_type: v.fs_type.clone(),
        fs_uuid: v.fs_uuid.clone(),
        volume_label: v.label.clone(),
        size_bytes: v.size_bytes,
        encrypted: v.encrypted,
        locked: v.locked,
        apfs_role: (!v.apfs_roles.is_empty()).then(|| v.apfs_roles.join(",")),
        unscannable_reason: v.unscannable_reason.clone(),
        mount_point: v.mount_point.as_ref().map(|p| p.display().to_string()),
    }
}

/// Identify a probed drive and record it (and its filesystems) in the catalog.
/// Returns `(drive_id, new, weak, filesystem ids in probe.volumes order)`.
pub fn record_drive(
    catalog: &mut Catalog,
    probe: &DriveProbe,
    choice: IdentityChoice,
    label: Option<&str>,
) -> Result<(i64, bool, bool, Vec<i64>)> {
    let rec = to_record(probe);
    let (existing, weak) = match choice {
        IdentityChoice::New => (None, false),
        IdentityChoice::SameAs(id) => {
            if catalog.drive_label(id).is_err() {
                bail!("no drive D{id}");
            }
            (Some(id), false)
        }
        IdentityChoice::Auto => match catalog.match_drive(&rec)? {
            DriveMatch::New => (None, false),
            DriveMatch::Known { drive_id, weak, .. } => (Some(drive_id), weak),
            DriveMatch::Ambiguous(candidates) => {
                let list: Vec<String> =
                    candidates.iter().map(|(d, score)| format!("D{d} (score {score})")).collect();
                bail!(
                    "{} matches several known drives or only weakly: {}. Pass --same-as D<n> or --new-drive.",
                    probe.dev_path,
                    list.join(", ")
                );
            }
        },
    };
    let drive_id = catalog.save_drive(&rec, existing, label)?;
    let mut fs_ids = Vec::new();
    for v in &probe.volumes {
        fs_ids.push(catalog.save_filesystem(drive_id, &to_fs_record(v))?);
    }
    Ok((drive_id, existing.is_none(), weak, fs_ids))
}

pub fn scan_device(
    req: &DeviceScanRequest,
    control: &Control,
    progress: &Progress,
    phase: &PhaseCell,
    on_step: &mut dyn FnMut(Step),
) -> Result<DeviceReport> {
    let probe = di_platform::probe(&req.dev)?;
    if probe.system_disk {
        bail!("{} holds the running system; refusing to touch it", probe.dev_path);
    }
    if probe.internal && !probe.removable && !req.allow_internal {
        bail!("{} is an internal disk; pass --allow-internal to scan it", probe.dev_path);
    }

    let mut catalog = Catalog::open(&req.catalog_path)?;
    let (drive_id, new_drive, weak_match, fs_ids) =
        record_drive(&mut catalog, &probe, req.identity, req.label.as_deref())?;
    on_step(Step::Identified { drive_id, new: new_drive, probe: &probe });

    let mut report = DeviceReport {
        drive_id,
        dev_path: probe.dev_path.clone(),
        new_drive,
        weak_match,
        volumes: Vec::new(),
        unmounted: false,
        ejected: false,
        interrupted: false,
    };

    for (vol, &fs_id) in probe.volumes.iter().zip(&fs_ids) {
        let mut vr = VolumeReport {
            dev: vol.dev.clone(),
            fs_type: vol.fs_type.clone(),
            label: vol.label.clone(),
            filesystem_id: fs_id,
            was_mounted_rw: vol.mounted_writable == Some(true),
            outcome: VolumeOutcome::Skipped(String::new()),
        };
        if let Some(reason) = &vol.unscannable_reason {
            vr.outcome = VolumeOutcome::Skipped(reason.clone());
            report.volumes.push(vr);
            continue;
        }
        if control.is_cancelled() {
            vr.outcome = VolumeOutcome::Skipped("cancelled".into());
            report.volumes.push(vr);
            continue;
        }
        on_step(Step::Mounting(vol));
        let mp = match di_platform::mount_ro(vol) {
            Ok(mp) => mp,
            Err(e) => {
                vr.outcome = VolumeOutcome::Failed(format!("{e:#}"));
                report.volumes.push(vr);
                continue;
            }
        };
        let root = mp.canonicalize().with_context(|| format!("mount point {}", mp.display()))?;
        catalog.update_fs_usage(fs_id, &host::fs_usage(&root), &root.display().to_string())?;
        on_step(Step::Scanning { vol, mount_point: &root });
        reset_progress(progress);
        let target = FsTarget { drive_id, filesystem_id: fs_id, root };
        match run_fs_scan(&mut catalog, &target, &req.walk, req.resume, control, progress, phase) {
            Ok(out) => {
                on_step(Step::Scanned(vol, &out));
                if out.status == ScanStatus::Interrupted {
                    report.interrupted = true;
                }
                vr.outcome = VolumeOutcome::Scanned(Box::new(out));
            }
            Err(e) => vr.outcome = VolumeOutcome::Failed(format!("{e:#}")),
        }
        report.volumes.push(vr);
    }

    // Leave the drive mounted if anything is unfinished, so the user can look or resume.
    let all_done = !report.interrupted
        && report.volumes.iter().all(|v| !matches!(v.outcome, VolumeOutcome::Failed(_)));
    if all_done && req.after != After::KeepMounted {
        on_step(Step::Unmounting);
        di_platform::unmount_drive(&probe, false)?;
        report.unmounted = true;
        if req.after == After::Eject && probe.ejectable {
            on_step(Step::Ejecting);
            di_platform::eject(&probe)?;
            report.ejected = true;
        }
    }
    Ok(report)
}

fn reset_progress(p: &Progress) {
    use std::sync::atomic::Ordering::Relaxed;
    p.files.store(0, Relaxed);
    p.dirs.store(0, Relaxed);
    p.bytes.store(0, Relaxed);
    p.alloc_bytes.store(0, Relaxed);
    p.errors.store(0, Relaxed);
}
