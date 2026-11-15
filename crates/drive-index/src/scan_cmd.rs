//! `drive-index scan <DEVICE|DIR>`

use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;

use di_model::units::{format_count, format_size};
use di_scan::device::{DeviceReport, DeviceScanRequest, Step, VolumeOutcome};
use di_scan::{ScanOutcome, ScanRequest, ScanStatus, run_dir_scan};

use crate::output::format_duration;
use crate::term::{Printer, Term};

/// `/dev/...`, or a macOS disk identifier like `disk7` / `disk7s2`.
pub fn is_device(target: &str) -> bool {
    if target.starts_with("/dev/") {
        return true;
    }
    let Some(rest) = target.strip_prefix("disk") else { return false };
    !rest.is_empty()
        && rest.chars().next().unwrap().is_ascii_digit()
        && rest.chars().all(|c| c.is_ascii_digit() || c == 's')
        && !std::path::Path::new(target).exists()
}

pub fn describe(out: &ScanOutcome, threads: usize) -> String {
    let t = &out.totals;
    let secs = out.walk_ms as f64 / 1000.0;
    let rate = if secs > 0.0 { (t.files as f64 / secs) as u64 } else { 0 };
    let mut s = format!(
        "{} files, {} dirs, {} in {} ({}/s, {}, {} threads); merged in {}",
        format_count(t.files),
        format_count(t.dirs),
        format_size(t.bytes),
        format_duration(Duration::from_millis(out.walk_ms)),
        format_count(rate),
        out.method,
        threads,
        format_duration(Duration::from_millis(out.merge_ms)),
    );
    if let Some(done) = out.resumed_dirs {
        s.push_str(&format!("; resumed scan #{} ({} dirs were already done)", out.scan_id, format_count(done)));
    }
    if t.errors > 0 {
        s.push_str(&format!("; {} entries unreadable", t.errors));
    }
    s
}

pub fn scan_dir(req: &ScanRequest, quiet: bool) -> Result<ExitCode> {
    let term = Term::start(quiet)?;
    let control = term.control();
    let result = run_dir_scan(req, &control, &term.progress, &term.phase);
    let dirs = term.progress.dirs.load(Ordering::Relaxed);
    term.finish();
    let out = result?;
    match out.status {
        ScanStatus::Complete => {
            println!("Scanned D{} — {}.", out.drive_id, describe(&out, req.walk.threads));
            Ok(ExitCode::SUCCESS)
        }
        ScanStatus::Interrupted => {
            println!(
                "Scan #{} interrupted after {} directories. Run the same command again to resume, or add --restart.",
                out.scan_id,
                format_count(dirs)
            );
            Ok(ExitCode::from(130))
        }
    }
}

/// Status lines for each step of a device scan.
pub fn step_printer(printer: Printer, threads: usize) -> impl FnMut(Step) + Send {
    move |step| match step {
        Step::Identified { drive_id, new, probe } => printer.say(format!(
            "{} {} — {} {} ({}){}",
            probe.dev_path,
            if new { format!("new drive D{drive_id}") } else { format!("is D{drive_id}") },
            probe.model.as_deref().unwrap_or("drive"),
            format_size(probe.capacity_bytes),
            probe.bus.as_str(),
            if probe.volumes.is_empty() { ", no filesystems" } else { "" }
        )),
        Step::Mounting(v) if v.mount_point.is_none() => {
            printer.say(format!("  {} {} \"{}\": mounting read-only", v.dev, v.fs_type, v.label.as_deref().unwrap_or("")))
        }
        Step::Mounting(v) => printer.say(format!(
            "  {} {} \"{}\": already mounted{}",
            v.dev,
            v.fs_type,
            v.label.as_deref().unwrap_or(""),
            if v.mounted_writable == Some(true) { " READ-WRITE by the system (scanned as-is)" } else { " read-only" }
        )),
        Step::Scanning { .. } => {}
        Step::Scanned(v, out) => match out.status {
            ScanStatus::Complete => printer.say(format!("  {}: {}", v.dev, describe(out, threads))),
            ScanStatus::Interrupted => printer.say(format!("  {}: interrupted (resumes next time)", v.dev)),
        },
        Step::Unmounting => {}
        Step::Ejecting => {}
    }
}

pub fn summarize(printer: &Printer, r: &DeviceReport) {
    let dev = &r.dev_path;
    for v in &r.volumes {
        match &v.outcome {
            VolumeOutcome::Skipped(reason) if !reason.is_empty() => {
                printer.say(format!("  {} {}: skipped — {reason}", v.dev, v.fs_type))
            }
            VolumeOutcome::Failed(e) => printer.say(format!("  {} {}: FAILED — {e}", v.dev, v.fs_type)),
            _ => {}
        }
    }
    if r.weak_match {
        printer.say(format!(
            "  note: recognised as D{} by weak identifiers only; if this is a different drive, rescan with --new-drive",
            r.drive_id
        ));
    }
    let msg = if r.ejected {
        format!("Ejected {dev} — safe to remove.")
    } else if r.unmounted {
        format!("Unmounted {dev}.")
    } else if r.interrupted {
        format!("{dev} left mounted read-only; the interrupted scan resumes when you scan it again.")
    } else if r.volumes.iter().any(|v| matches!(v.outcome, VolumeOutcome::Failed(_))) {
        format!("{dev} left mounted because a filesystem failed to scan.")
    } else {
        format!("{dev} left mounted read-only.")
    };
    printer.say(msg);
}

pub fn scan_device(req: &DeviceScanRequest, quiet: bool) -> Result<ExitCode> {
    let term = Term::start(quiet)?;
    let control = term.control();
    let mut on_step = step_printer(term.printer.clone(), req.walk.threads);
    let result = di_scan::device::scan_device(req, &control, &term.progress, &term.phase, &mut on_step);
    let printer = term.printer.clone();
    term.finish();
    let report = result?;
    summarize(&printer, &report);
    let failed = report.volumes.iter().any(|v| matches!(v.outcome, VolumeOutcome::Failed(_)));
    Ok(if report.interrupted {
        ExitCode::from(130)
    } else if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
