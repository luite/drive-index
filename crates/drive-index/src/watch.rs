//! `drive-index watch`: headless front end of the drive controller.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use anyhow::{Result, bail};

use di_model::units::format_size;
use di_platform::DriveProbe;
use di_scan::controller::{Controller, ControllerOptions, Event, NewDrivePolicy, Outcome, Policy, Question};
use di_scan::device::{After, report_lines};
use di_walk::WalkConfig;

use crate::term::{Printer, Term};

pub struct WatchOptions {
    pub catalog_path: PathBuf,
    pub scan_new: bool,
    pub rescan_after_secs: i64,
    pub include_images: bool,
    pub after: After,
    pub walk: WalkConfig,
    pub quiet: bool,
}

/// `30d`, `12h`, `90m`, `2w`, `45s`, `0`.
pub fn parse_age(s: &str) -> Result<i64> {
    if s == "0" {
        return Ok(0);
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().map_err(|_| anyhow::anyhow!("invalid age '{s}' (e.g. 30d, 12h)"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => bail!("invalid age '{s}' (e.g. 30d, 12h)"),
    };
    Ok(n * mult)
}

pub fn humanize_age(secs: i64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 2 * 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

pub fn describe_probe(p: &DriveProbe) -> String {
    format!("{} ({} {}, {})", p.dev_path, p.model.as_deref().unwrap_or("drive"), format_size(p.capacity_bytes), p.bus.as_str())
}

fn print_event(printer: &Printer, ev: Event) {
    match ev {
        Event::Log { text, .. } => printer.say(text),
        Event::Attached { probe, drive_id, label, outcome, .. } => {
            let desc = describe_probe(&probe);
            let who = match (drive_id, label) {
                (Some(id), Some(l)) => format!("D{id} \"{l}\""),
                (Some(id), None) => format!("D{id} (unlabeled)"),
                _ => "New drive".into(),
            };
            let now = di_core::now_secs();
            match outcome {
                Outcome::Queued => printer.say(format!("{who} connected: {desc}; scanning.")),
                Outcome::UpToDate { last_scan } => printer.say(format!(
                    "{who} connected: {desc}; up to date (last scan {} ago).",
                    humanize_age(now - last_scan)
                )),
                Outcome::Reported => printer.say(format!(
                    "New drive {desc} is not in the catalog. Scan it with `drive-index scan {}`, or watch with --scan-new.",
                    probe.dev_path
                )),
                Outcome::Ask(Question::Ambiguous(c)) => {
                    let ids: Vec<String> = c.iter().map(|(d, _)| format!("D{d}")).collect();
                    printer.say(format!(
                        "{desc} could be {}; scan it manually with --same-as D<n> or --new-drive.",
                        ids.join(" or ")
                    ))
                }
                Outcome::Ask(Question::NewDrive) => printer.say(format!("New drive {desc}.")),
                Outcome::Ignored(_) => {}
            }
        }
        Event::Detached { disk, expected } => {
            if !expected {
                printer.say(format!("/dev/{disk} disconnected."))
            }
        }
        Event::JobStarted { .. } => {}
        Event::JobStep { text, .. } => printer.say(text),
        Event::JobFinished { disk, result } => match result {
            Ok(report) => {
                for line in report_lines(&report) {
                    printer.say(line);
                }
            }
            Err(e) => printer.say(format!("/dev/{disk}: scan failed: {e}")),
        },
    }
}

pub fn run(opts: &WatchOptions) -> Result<ExitCode> {
    let ctl = Controller::start(ControllerOptions {
        catalog_path: opts.catalog_path.clone(),
        policy: Policy {
            new_drive: if opts.scan_new { NewDrivePolicy::Scan } else { NewDrivePolicy::Report },
            rescan_after_secs: opts.rescan_after_secs,
            after: opts.after,
            include_images: opts.include_images,
        },
        walk: opts.walk.clone(),
    })?;
    let term = Term::start_shared(opts.quiet, ctl.progress.clone(), ctl.phase.clone())?;
    term.say(format!(
        "Watching for drives (new drives: {}; rescan known drives after {}; then {}). Ctrl-C to stop.",
        if opts.scan_new { "scan" } else { "report only" },
        humanize_age(opts.rescan_after_secs),
        match opts.after {
            After::Eject => "eject",
            After::Unmount => "unmount",
            After::KeepMounted => "keep mounted",
        }
    ));
    while !term.interrupted() {
        match ctl.events.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => print_event(&term.printer, ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Stop: a running scan is interrupted (resumable); show its last messages.
    if ctl.job().is_some() {
        term.say("Stopping the running scan (it resumes next time)…");
    }
    for ev in ctl.shutdown() {
        print_event(&term.printer, ev);
    }
    term.say("Stopped watching.");
    term.finish();
    Ok(ExitCode::SUCCESS)
}
