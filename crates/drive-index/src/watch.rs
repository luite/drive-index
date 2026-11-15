//! `drive-index watch`: react to drives being connected (PLAN.md §2.1–2.2, headless).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use di_core::Catalog;
use di_core::drives::DriveMatch;
use di_model::units::format_size;
use di_platform::{Bus, DeviceEvent, DriveProbe};
use di_scan::ResumeMode;
use di_scan::device::{After, DeviceReport, DeviceScanRequest, IdentityChoice, Step, record_drive, to_record};
use di_walk::{Control, WalkConfig};

use crate::scan_cmd::{step_printer, summarize};
use crate::term::Term;

/// Wait this long after the last event for a disk before acting (partitions and
/// volumes appear as separate events).
const SETTLE: Duration = Duration::from_millis(1500);

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

struct Active {
    disk: String,
    control: Arc<Control>,
    /// Set once the scan starts unmounting: disappear events are then our own doing.
    releasing: Arc<AtomicBool>,
    handle: JoinHandle<Result<DeviceReport>>,
}

enum Decision {
    Scan,
    Skip,
}

pub fn run(opts: &WatchOptions) -> Result<ExitCode> {
    let term = Term::start(opts.quiet)?;
    let rx = di_platform::watch()?;
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

    let mut pending: HashMap<String, Instant> = HashMap::new();
    let mut handled: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut active: Option<Active> = None;
    // Disks we unmounted/ejected ourselves: their disappear events are expected.
    let mut released: HashSet<String> = HashSet::new();

    loop {
        if term.interrupted() && active.is_none() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(DeviceEvent::Appeared(d)) => {
                released.remove(&d);
                pending.insert(d, Instant::now());
            }
            Ok(DeviceEvent::Disappeared(d)) => {
                pending.remove(&d);
                queue.retain(|q| *q != d);
                let was_handled = handled.remove(&d);
                match active.as_ref().filter(|a| a.disk == d) {
                    Some(a) if a.releasing.load(Ordering::SeqCst) => {
                        released.insert(d);
                    }
                    Some(a) => {
                        if !a.control.is_cancelled() {
                            a.control.cancel();
                            term.say(format!("/dev/{d} was removed during the scan; it will resume next time."));
                        }
                    }
                    None if was_handled && !released.contains(&d) => {
                        term.say(format!("/dev/{d} disconnected."));
                    }
                    None => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("device watcher stopped"),
        }

        // Disks whose events have settled.
        let ready: Vec<String> =
            pending.iter().filter(|(_, t)| t.elapsed() >= SETTLE).map(|(d, _)| d.clone()).collect();
        for d in ready {
            pending.remove(&d);
            if term.interrupted() {
                continue;
            }
            // Partitions and APFS containers resolve to their physical disk; handle each once.
            let Ok(id) = di_platform::resolve_whole(&d) else { continue };
            if handled.contains(&id) || queue.contains(&id) || active.as_ref().is_some_and(|a| a.disk == id) {
                continue;
            }
            handled.insert(id.clone());
            let Ok(probe) = di_platform::probe(&id) else { continue };
            if let Decision::Scan = decide(opts, &term, &probe) {
                queue.push_back(id);
            }
        }

        // A finished scan.
        if active.as_ref().is_some_and(|a| a.handle.is_finished()) {
            let a = active.take().unwrap();
            match a.handle.join() {
                Ok(Ok(report)) => {
                    if report.unmounted {
                        released.insert(a.disk.clone());
                    }
                    summarize(&term.printer, &report)
                }
                Ok(Err(e)) => term.say(format!("/dev/{}: scan failed: {e:#}", a.disk)),
                Err(_) => term.say(format!("/dev/{}: scan thread panicked", a.disk)),
            }
        }

        // Start the next one.
        if active.is_none() && !term.interrupted() {
            if let Some(disk) = queue.pop_front() {
                let control = term.new_control();
                let req = DeviceScanRequest {
                    catalog_path: opts.catalog_path.clone(),
                    dev: disk.clone(),
                    label: None,
                    walk: opts.walk.clone(),
                    resume: ResumeMode::Auto,
                    after: opts.after,
                    identity: IdentityChoice::Auto,
                    allow_internal: false,
                };
                let (progress, phase, printer) = (term.progress.clone(), term.phase.clone(), term.printer.clone());
                let c = control.clone();
                let threads = opts.walk.threads;
                let releasing = Arc::new(AtomicBool::new(false));
                let rel = releasing.clone();
                let handle = std::thread::spawn(move || {
                    let mut print = step_printer(printer, threads);
                    let mut on_step = |step: Step| {
                        if matches!(step, Step::Unmounting | Step::Ejecting) {
                            rel.store(true, Ordering::SeqCst);
                        }
                        print(step)
                    };
                    di_scan::device::scan_device(&req, &c, &progress, &phase, &mut on_step)
                });
                active = Some(Active { disk, control, releasing, handle });
            }
        }
    }
    term.say("Stopped watching.");
    term.finish();
    Ok(ExitCode::SUCCESS)
}

fn decide(opts: &WatchOptions, term: &Term, probe: &DriveProbe) -> Decision {
    if !probe.is_external() || (probe.bus == Bus::Image && !opts.include_images) {
        return Decision::Skip;
    }
    let desc = format!(
        "{} ({} {}, {})",
        probe.dev_path,
        probe.model.as_deref().unwrap_or("drive"),
        format_size(probe.capacity_bytes),
        probe.bus.as_str()
    );
    let mut catalog = match Catalog::open(&opts.catalog_path) {
        Ok(c) => c,
        Err(e) => {
            term.say(format!("{desc}: cannot open catalog: {e:#}"));
            return Decision::Skip;
        }
    };
    match catalog.match_drive(&to_record(probe)) {
        Ok(DriveMatch::New) if opts.scan_new => Decision::Scan,
        Ok(DriveMatch::New) => {
            term.say(format!(
                "New drive {desc} is not in the catalog. Scan it with `drive-index scan {}`, or watch with --scan-new.",
                probe.dev_path
            ));
            Decision::Skip
        }
        Ok(DriveMatch::Ambiguous(c)) => {
            let ids: Vec<String> = c.iter().map(|(d, _)| format!("D{d}")).collect();
            term.say(format!(
                "{desc} could be {}; scan it manually with --same-as D<n> or --new-drive.",
                ids.join(" or ")
            ));
            Decision::Skip
        }
        Ok(DriveMatch::Known { drive_id, .. }) => {
            // Record the connection, then check how fresh each filesystem's scan is.
            let fs_ids = match record_drive(&mut catalog, probe, IdentityChoice::SameAs(drive_id), None) {
                Ok((_, _, _, ids)) => ids,
                Err(e) => {
                    term.say(format!("{desc}: {e:#}"));
                    return Decision::Skip;
                }
            };
            let now = di_core::now_secs();
            let mut oldest: Option<i64> = None;
            let mut stale = false;
            for (vol, fs) in probe.volumes.iter().zip(fs_ids) {
                if !vol.scannable() {
                    continue;
                }
                match catalog.last_scanned(fs).ok().flatten() {
                    Some(t) => {
                        oldest = Some(oldest.map_or(t, |o: i64| o.min(t)));
                        if now - t >= opts.rescan_after_secs {
                            stale = true;
                        }
                    }
                    None => stale = true,
                }
            }
            let label = catalog
                .drive_label(drive_id)
                .ok()
                .flatten()
                .map(|l| format!("\"{l}\""))
                .unwrap_or_else(|| "(unlabeled)".into());
            if stale {
                term.say(format!("D{drive_id} {label} connected: {desc}; scanning."));
                Decision::Scan
            } else {
                term.say(format!(
                    "D{drive_id} {label} connected: {desc}; up to date (last scan {} ago).",
                    humanize_age(now - oldest.unwrap_or(now))
                ));
                Decision::Skip
            }
        }
        Err(e) => {
            term.say(format!("{desc}: {e:#}"));
            Decision::Skip
        }
    }
}

fn humanize_age(secs: i64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 2 * 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}
