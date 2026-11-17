//! Scanning a whole drive: probe → identify → record → mount each filesystem
//! read-only and walk it → read file contents → unmount → eject.

use std::path::PathBuf;

use anyhow::{Result, bail};

use di_core::Catalog;
use di_core::drives::{DriveMatch, DriveRecord, FsRecord, PartitionRecord};
use di_model::smart::{SmartReport, SmartUnavailable};
use di_platform::{DriveProbe, VolumeProbe, identity};
use di_walk::{Control, Progress, WalkConfig, host};

use crate::content::ContentOutcome;
use crate::run::{FsTarget, Phase, PhaseCell, ResumeMode, ScanOutcome, ScanStatus, run_fs_scan};

// Note [Walk first, read later]
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// A device scan has two stages, each over all of the drive's filesystems:
//
//  1. walk: mount each filesystem and record its directory tree and metadata;
//  2. read: run the content phase (media metadata, hashes) on each of them.
//
// The walk reads little (directories and inodes) and produces most of the value: a
// complete list of names, sizes and dates. The content phase reads file data. It
// takes longer, and it is where a failing drive or a flaky connection shows itself.
// Walking everything first means such a drive still gets all its filesystems
// catalogued before the risky reads start.
//
// A filesystem is not walked again if its last complete scan is younger than
// `rescan_after_secs` (`WalkPlan::Fresh`). It is still mounted and read if it has
// pending content work, for example because an earlier content phase was cut
// short. Scans that start on their own when a known drive is connected use the
// policy's --rescan-after (30 days by default); explicit scans pass 0 and walk
// everything.

/// Does a filesystem need walking? See Note [Walk first, read later].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkPlan {
    /// Never scanned, scanned too long ago, or an interrupted scan to resume.
    Walk,
    /// Completely scanned at `last_scan`, recently enough to keep.
    Fresh { last_scan: i64 },
}

/// The walk plan for filesystem `fs` at time `now`; `rescan_after_secs` 0 always walks.
pub fn walk_plan(catalog: &Catalog, fs: i64, rescan_after_secs: i64, now: i64) -> Result<WalkPlan> {
    if catalog.resumable_scan(fs)?.is_some() {
        return Ok(WalkPlan::Walk);
    }
    Ok(match catalog.last_scanned(fs)? {
        Some(t) if now - t < rescan_after_secs => WalkPlan::Fresh { last_scan: t },
        _ => WalkPlan::Walk,
    })
}

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
    /// Content phase per filesystem while mounted; None skips it.
    pub content: Option<crate::content::ContentConfig>,
    /// Read SMART health (and the real disk serial) with smartctl.
    pub smart: bool,
    /// Don't walk filesystems completely scanned less than this long ago; 0 walks
    /// them all. See Note [Walk first, read later].
    pub rescan_after_secs: i64,
}

#[derive(Debug)]
pub enum VolumeOutcome {
    /// Walked in this run (complete or interrupted: see the status).
    Scanned(Box<ScanOutcome>),
    /// Not walked: scanned completely at `last_scan`, recently enough.
    UpToDate { last_scan: i64 },
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
    /// The content phase, if it ran for this filesystem.
    pub content: Option<ContentOutcome>,
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
    pub smart: Option<Result<SmartReport, SmartUnavailable>>,
}

/// Progress notifications for the caller (CLI line, TUI).
#[derive(Debug)]
pub enum Step<'a> {
    Identified { drive_id: i64, new: bool, probe: &'a DriveProbe },
    Smart(&'a Result<SmartReport, SmartUnavailable>),
    /// Not walked again: completely scanned at `last_scan`. If `read_pending`, it is
    /// mounted for the file contents still to be read.
    UpToDate { vol: &'a VolumeProbe, last_scan: i64, read_pending: bool },
    Mounting(&'a VolumeProbe),
    Scanning { vol: &'a VolumeProbe, mount_point: &'a std::path::Path },
    Scanned(&'a VolumeProbe, &'a ScanOutcome),
    /// Content phase starting for a volume, with this many files to read.
    Content { vol: &'a VolumeProbe, files: u64 },
    ContentDone(&'a VolumeProbe, &'a ContentOutcome),
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

/// Fill what the OS did not report from SMART, and add the disk's own identifiers.
/// OS-reported values win, so a host without SMART access records the same drive.
fn enrich(rec: &mut DriveRecord, s: &SmartReport) {
    if rec.serial.is_none() {
        rec.serial = s.serial.clone();
    }
    if rec.wwn.is_none() {
        rec.wwn = s.wwn.clone();
    }
    if rec.firmware.is_none() {
        rec.firmware = s.firmware.clone();
    }
    if rec.rotation_rpm.is_none() {
        rec.rotation_rpm = s.rotation_rpm;
    }
    for i in identity::smart_identifiers(s) {
        if !rec.identifiers.iter().any(|x| x.kind == i.kind && x.value == i.value) {
            rec.identifiers.push(i);
        }
    }
}

/// Identify a probed drive and record it (and its filesystems) in the catalog.
/// Returns `(drive_id, new, weak, filesystem ids in probe.volumes order)`.
pub fn record_drive(
    catalog: &mut Catalog,
    probe: &DriveProbe,
    smart: Option<&SmartReport>,
    choice: IdentityChoice,
    label: Option<&str>,
) -> Result<(i64, bool, bool, Vec<i64>)> {
    let mut rec = to_record(probe);
    if let Some(s) = smart {
        enrich(&mut rec, s);
    }
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
    let smart = req.smart.then(|| {
        // The device type that worked for this drive last time saves the fallbacks.
        let cached = match catalog.match_drive(&to_record(&probe)) {
            Ok(DriveMatch::Known { drive_id, .. }) => di_core::smart::access(catalog.conn(), drive_id).ok().and_then(|a| a.device_type),
            _ => None,
        };
        di_platform::smart::read(&probe, cached.as_deref())
    });
    let (drive_id, new_drive, weak_match, fs_ids) =
        record_drive(&mut catalog, &probe, smart.as_ref().and_then(|r| r.as_ref().ok()), req.identity, req.label.as_deref())?;
    on_step(Step::Identified { drive_id, new: new_drive, probe: &probe });
    if let Some(r) = &smart {
        catalog.save_smart(drive_id, r)?;
        on_step(Step::Smart(r));
    }

    let mut report = DeviceReport {
        drive_id,
        dev_path: probe.dev_path.clone(),
        new_drive,
        weak_match,
        volumes: Vec::new(),
        unmounted: false,
        ejected: false,
        interrupted: false,
        smart,
    };
    let now = di_core::now_secs();

    // Stage 1: walk every filesystem. See Note [Walk first, read later].
    // Mounted filesystems for stage 2, by index into `probe.volumes` / `report.volumes`.
    let mut to_read: Vec<(usize, FsTarget)> = Vec::new();
    for (i, (vol, &fs_id)) in probe.volumes.iter().zip(&fs_ids).enumerate() {
        let outcome = 'outcome: {
            if let Some(reason) = &vol.unscannable_reason {
                break 'outcome VolumeOutcome::Skipped(reason.clone());
            }
            if control.is_cancelled() {
                break 'outcome VolumeOutcome::Skipped("cancelled".into());
            }
            let plan = walk_plan(&catalog, fs_id, req.rescan_after_secs, now)?;
            if let WalkPlan::Fresh { last_scan } = plan {
                let read_pending = match &req.content {
                    Some(cfg) => crate::content::has_pending(catalog.conn(), fs_id, cfg)?,
                    None => false,
                };
                on_step(Step::UpToDate { vol, last_scan, read_pending });
                if !read_pending {
                    break 'outcome VolumeOutcome::UpToDate { last_scan };
                }
            }
            on_step(Step::Mounting(vol));
            let mp = match di_platform::mount_ro(vol) {
                Ok(mp) => mp,
                Err(e) => break 'outcome VolumeOutcome::Failed(format!("{e:#}")),
            };
            let root = match mp.canonicalize() {
                Ok(r) => r,
                Err(e) => break 'outcome VolumeOutcome::Failed(format!("mount point {}: {e}", mp.display())),
            };
            catalog.update_fs_usage(fs_id, &host::fs_usage(&root), &root.display().to_string())?;
            let target = FsTarget { drive_id, filesystem_id: fs_id, root };
            match plan {
                WalkPlan::Fresh { last_scan } => {
                    to_read.push((i, target));
                    VolumeOutcome::UpToDate { last_scan }
                }
                WalkPlan::Walk => {
                    on_step(Step::Scanning { vol, mount_point: &target.root });
                    reset_progress(progress);
                    match run_fs_scan(&mut catalog, &target, &req.walk, req.resume, control, progress, phase) {
                        Ok(out) => {
                            on_step(Step::Scanned(vol, &out));
                            match out.status {
                                ScanStatus::Complete => to_read.push((i, target)),
                                ScanStatus::Interrupted => report.interrupted = true,
                            }
                            VolumeOutcome::Scanned(Box::new(out))
                        }
                        Err(e) => VolumeOutcome::Failed(format!("{e:#}")),
                    }
                }
            }
        };
        report.volumes.push(VolumeReport {
            dev: vol.dev.clone(),
            fs_type: vol.fs_type.clone(),
            label: vol.label.clone(),
            filesystem_id: fs_id,
            was_mounted_rw: vol.mounted_writable == Some(true),
            outcome,
            content: None,
        });
    }

    // Stage 2: read file contents of the filesystems mounted above.
    if let Some(cfg) = &req.content {
        for (i, target) in &to_read {
            if control.is_cancelled() {
                break;
            }
            let vol = &probe.volumes[*i];
            phase.set(Phase::Content);
            let mut started = |files| on_step(Step::Content { vol, files });
            match crate::content::run_content(&mut catalog, target.filesystem_id, &target.root, cfg, control, progress, &mut started) {
                Ok(c) => {
                    on_step(Step::ContentDone(vol, &c));
                    report.interrupted |= c.cancelled;
                    report.volumes[*i].content = Some(c);
                }
                Err(e) => report.volumes[*i].outcome = VolumeOutcome::Failed(format!("content phase: {e:#}")),
            }
            phase.set(Phase::Done);
        }
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
    p.reset();
}

/// One-line summary of a finished filesystem scan.
pub fn describe_outcome(out: &ScanOutcome, threads: usize) -> String {
    use di_model::units::{format_count, format_duration, format_size};
    let t = &out.totals;
    let secs = out.walk_ms as f64 / 1000.0;
    let rate = if secs > 0.0 { (t.files as f64 / secs) as u64 } else { 0 };
    let mut s = format!(
        "{} files, {} dirs, {} in {} ({}/s, {}, {} threads); merged in {}",
        format_count(t.files),
        format_count(t.dirs),
        format_size(t.bytes),
        format_duration(std::time::Duration::from_millis(out.walk_ms)),
        format_count(rate),
        out.method,
        threads,
        format_duration(std::time::Duration::from_millis(out.merge_ms)),
    );
    if let Some(done) = out.resumed_dirs {
        s.push_str(&format!("; resumed scan #{} ({} dirs were already done)", out.scan_id, format_count(done)));
    }
    if t.errors > 0 {
        s.push_str(&format!("; {} entries unreadable", t.errors));
    }
    s
}

/// Human text for a device-scan step (None for steps not worth a line).
pub fn step_text(step: &Step, threads: usize) -> Option<String> {
    use di_model::units::format_size;
    Some(match step {
        Step::Identified { drive_id, new, probe } => format!(
            "{} {} — {} {} ({}){}",
            probe.dev_path,
            if *new { format!("new drive D{drive_id}") } else { format!("is D{drive_id}") },
            probe.model.as_deref().unwrap_or("drive"),
            format_size(probe.capacity_bytes),
            probe.bus.as_str(),
            if probe.volumes.is_empty() { ", no filesystems" } else { "" }
        ),
        Step::Smart(Ok(s)) => {
            let mut t = format!("  SMART: {}", s.summary());
            for w in s.warnings() {
                t.push_str(&format!("\n  ⚠ {w}"));
            }
            t
        }
        Step::Smart(Err(SmartUnavailable::NotInstalled)) => return None,
        Step::Smart(Err(SmartUnavailable::Unsupported(m))) if m.starts_with("disk images") => return None,
        Step::Smart(Err(e)) => format!("  SMART unavailable: {}", e.reason()),
        Step::UpToDate { vol: v, last_scan, read_pending } => format!(
            "  {} {} \"{}\": up to date (scanned {} ago){}",
            v.dev,
            v.fs_type,
            v.label.as_deref().unwrap_or(""),
            di_model::units::format_age(di_core::now_secs() - last_scan),
            if *read_pending { "; reading the file contents still pending" } else { "" }
        ),
        Step::Mounting(v) if v.mount_point.is_none() => {
            format!("  {} {} \"{}\": mounting read-only", v.dev, v.fs_type, v.label.as_deref().unwrap_or(""))
        }
        Step::Mounting(v) => format!(
            "  {} {} \"{}\": already mounted{}",
            v.dev,
            v.fs_type,
            v.label.as_deref().unwrap_or(""),
            if v.mounted_writable == Some(true) { " READ-WRITE by the system (scanned as-is)" } else { " read-only" }
        ),
        Step::Scanned(v, out) => match out.status {
            ScanStatus::Complete => format!("  {}: {}", v.dev, describe_outcome(out, threads)),
            ScanStatus::Interrupted => format!("  {}: interrupted (resumes next time)", v.dev),
        },
        Step::Content { vol, files } => format!(
            "  {}: reading {} {} for metadata and hashes…",
            vol.dev,
            di_model::units::format_count(*files),
            if *files == 1 { "file" } else { "files" }
        ),
        Step::ContentDone(v, c) => {
            let mut t = format!(
                "  {}: {} files read ({} with metadata, {} hashed) in {}",
                v.dev,
                di_model::units::format_count(c.processed),
                di_model::units::format_count(c.media),
                di_model::units::format_count(c.hashed),
                di_model::units::format_duration(std::time::Duration::from_millis(c.ms))
            );
            if c.verified > 0 {
                t.push_str(&format!("; {} likely duplicates verified by full hash", di_model::units::format_count(c.verified)));
            }
            if c.errors > 0 {
                t.push_str(&format!("; {} unreadable", c.errors));
            }
            if c.cancelled {
                t.push_str("; stopped (continues next time)");
            }
            if c.processed == 0 {
                return None;
            }
            t
        }
        Step::Scanning { .. } | Step::Unmounting | Step::Ejecting => return None,
    })
}

/// Closing lines for a device report: skipped/failed volumes and the final state.
pub fn report_lines(r: &DeviceReport) -> Vec<String> {
    let mut out = Vec::new();
    for v in &r.volumes {
        match &v.outcome {
            VolumeOutcome::Skipped(reason) if !reason.is_empty() => {
                out.push(format!("  {} {}: skipped — {reason}", v.dev, v.fs_type))
            }
            VolumeOutcome::Failed(e) => out.push(format!("  {} {}: FAILED — {e}", v.dev, v.fs_type)),
            _ => {}
        }
    }
    if r.weak_match {
        out.push(format!(
            "  note: recognised as D{} by weak identifiers only; if this is a different drive, rescan with --new-drive",
            r.drive_id
        ));
    }
    let dev = &r.dev_path;
    out.push(if r.ejected {
        format!("Ejected {dev} — safe to remove.")
    } else if r.unmounted {
        format!("Unmounted {dev}.")
    } else if r.interrupted {
        format!("{dev} left mounted read-only; the interrupted scan resumes when you scan it again.")
    } else if r.volumes.iter().any(|v| matches!(v.outcome, VolumeOutcome::Failed(_))) {
        format!("{dev} left mounted because a filesystem failed to scan.")
    } else {
        format!("{dev} left mounted read-only.")
    });
    out
}
