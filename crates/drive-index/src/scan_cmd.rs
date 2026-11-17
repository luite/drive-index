//! `drive-index scan <DEVICE|DIR>`

use std::process::ExitCode;
use std::sync::atomic::Ordering;

use anyhow::Result;

use di_model::units::format_count;
use di_scan::device::{DeviceReport, DeviceScanRequest, Step, VolumeOutcome, describe_outcome, report_lines, step_text};
use di_scan::{ScanRequest, ScanStatus, run_dir_scan};

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

pub fn scan_dir(req: &ScanRequest, quiet: bool) -> Result<ExitCode> {
    let term = Term::start(quiet)?;
    let control = term.control();
    let result = run_dir_scan(req, &control, &term.progress, &term.phase);
    let dirs = term.progress.dirs.load(Ordering::Relaxed);
    term.finish();
    let out = result?;
    match out.status {
        ScanStatus::Complete => {
            println!("Scanned D{} — {}.", out.drive_id, describe_outcome(&out, req.walk.threads));
            if let Some(c) = out.content.filter(|c| c.processed > 0) {
                let ending = match &c.stopped {
                    _ if c.cancelled => "; stopped, continues next time".to_string(),
                    Some(t) => format!("; stopped early, the drive is not reading reliably: {}", t.describe()),
                    None => String::new(),
                };
                println!(
                    "Read {} {} for metadata and hashes ({} with metadata{}) in {}{ending}.",
                    format_count(c.processed),
                    if c.processed == 1 { "file" } else { "files" },
                    format_count(c.media),
                    if c.verified > 0 { format!(", {} duplicates verified by full hash", format_count(c.verified)) } else { String::new() },
                    di_model::units::format_duration(std::time::Duration::from_millis(c.ms)),
                );
            }
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
        ScanStatus::Lost => {
            println!(
                "Scan #{} stopped: {} went away during the scan (unmounted or disconnected). The catalog was not \
                 changed; run the same command again to resume.",
                out.scan_id,
                req.dir.display()
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Status lines for each step of a device scan.
pub fn step_printer(printer: Printer, threads: usize) -> impl FnMut(Step) + Send {
    move |step| {
        if let Some(text) = step_text(&step, threads) {
            printer.say(text)
        }
    }
}

pub fn summarize(printer: &Printer, r: &DeviceReport) {
    for line in report_lines(r) {
        printer.say(line);
    }
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
    Ok(if report.disconnected {
        ExitCode::FAILURE
    } else if report.interrupted {
        ExitCode::from(130)
    } else if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
