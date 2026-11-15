//! The drive controller (PLAN.md §2.1–2.2): one thread that reacts to device events,
//! applies the policy, queues and runs device scans one at a time, and executes
//! commands from a front end (TUI or the headless `watch`). Front ends receive
//! [`Event`]s and read live progress from the shared counters.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;

use di_core::Catalog;
use di_core::drives::DriveMatch;
use di_platform::{Bus, DeviceEvent, DriveProbe};
use di_walk::{Control, Progress, WalkConfig};

use crate::device::{
    After, DeviceReport, DeviceScanRequest, IdentityChoice, Step, record_drive, report_lines, scan_device, step_text,
    to_record,
};
use crate::run::{PhaseCell, ResumeMode};

/// Wait this long after the last event for a disk before acting.
const SETTLE: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewDrivePolicy {
    /// Ask the front end (TUI).
    Ask,
    /// Scan automatically.
    Scan,
    /// Only report it (headless default).
    Report,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub new_drive: NewDrivePolicy,
    pub rescan_after_secs: i64,
    pub after: After,
    pub include_images: bool,
}

#[derive(Debug, Clone)]
pub struct ControllerOptions {
    pub catalog_path: PathBuf,
    pub policy: Policy,
    pub walk: WalkConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// Not in the catalog: scan it (and how to label it)?
    NewDrive,
    /// Could be one of these known drives `(drive_id, score)`.
    Ambiguous(Vec<(i64, i64)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Queued for scanning.
    Queued,
    /// Known and recently scanned.
    UpToDate { last_scan: i64 },
    /// The front end must decide.
    Ask(Question),
    /// New drive, policy says report only.
    Reported,
    /// Not handled (internal disk, disk image, …).
    Ignored(String),
}

#[derive(Debug)]
pub enum Event {
    Log { level: Level, text: String },
    Attached { disk: String, probe: Box<DriveProbe>, drive_id: Option<i64>, label: Option<String>, outcome: Outcome },
    Detached { disk: String, expected: bool },
    JobStarted { disk: String },
    JobStep { disk: String, text: String },
    JobFinished { disk: String, result: std::result::Result<Box<DeviceReport>, String> },
}

#[derive(Debug, Clone)]
pub enum Command {
    Scan { disk: String, identity: IdentityChoice, label: Option<String> },
    Pause,
    Resume,
    Cancel,
    Unmount(String),
    Eject(String),
    Shutdown,
}

/// The running job, as seen by front ends.
#[derive(Debug, Clone)]
pub struct JobView {
    pub disk: String,
    pub started: Instant,
    pub control: Arc<Control>,
    /// Volume being scanned right now (`disk4s2 exfat "SamsungUSB"`).
    pub volume: Option<String>,
    /// Used bytes of that volume (statfs), the progress denominator.
    pub expected_bytes: Option<u64>,
}

pub struct Controller {
    tx: Sender<Command>,
    pub events: Receiver<Event>,
    pub progress: Arc<Progress>,
    pub phase: Arc<PhaseCell>,
    job: Arc<Mutex<Option<JobView>>>,
    handle: Option<JoinHandle<()>>,
}

impl Controller {
    pub fn start(opts: ControllerOptions) -> Result<Controller> {
        let device_rx = di_platform::watch()?;
        let (tx, cmd_rx) = mpsc::channel();
        let (ev_tx, events) = mpsc::channel();
        let progress = Arc::new(Progress::default());
        let phase = Arc::new(PhaseCell::default());
        let job = Arc::new(Mutex::new(None));
        let state = State {
            opts,
            ev: ev_tx,
            progress: progress.clone(),
            phase: phase.clone(),
            job: job.clone(),
            pending: HashMap::new(),
            handled: HashSet::new(),
            queue: VecDeque::new(),
            active: None,
            released: HashSet::new(),
            shutting_down: false,
        };
        let handle = std::thread::Builder::new()
            .name("drive-controller".into())
            .spawn(move || state.run(device_rx, cmd_rx))?;
        Ok(Controller { tx, events, progress, phase, job, handle: Some(handle) })
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    /// The running job, if any.
    pub fn job(&self) -> Option<JobView> {
        self.job.lock().unwrap().clone()
    }

    /// Cancel any running scan (it stays resumable), stop the controller thread and
    /// return the events it produced while stopping.
    pub fn shutdown(mut self) -> Vec<Event> {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.events.try_iter().collect()
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Job {
    disk: String,
    identity: IdentityChoice,
    label: Option<String>,
}

struct Active {
    disk: String,
    control: Arc<Control>,
    releasing: Arc<AtomicBool>,
    handle: JoinHandle<Result<DeviceReport>>,
}

struct State {
    opts: ControllerOptions,
    ev: Sender<Event>,
    progress: Arc<Progress>,
    phase: Arc<PhaseCell>,
    job: Arc<Mutex<Option<JobView>>>,
    pending: HashMap<String, Instant>,
    handled: HashSet<String>,
    queue: VecDeque<Job>,
    active: Option<Active>,
    released: HashSet<String>,
    shutting_down: bool,
}

impl State {
    fn log(&self, level: Level, text: impl Into<String>) {
        let _ = self.ev.send(Event::Log { level, text: text.into() });
    }

    fn run(mut self, device_rx: Receiver<DeviceEvent>, cmd_rx: Receiver<Command>) {
        loop {
            match device_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(e) => self.on_device(e),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.log(Level::Error, "device watcher stopped");
                    self.shutting_down = true;
                }
            }
            while let Ok(cmd) = cmd_rx.try_recv() {
                self.on_command(cmd);
            }
            if self.shutting_down && self.active.is_none() {
                return;
            }
            self.settle();
            self.reap();
            if !self.shutting_down {
                self.start_next();
            }
        }
    }

    fn on_device(&mut self, e: DeviceEvent) {
        match e {
            DeviceEvent::Appeared(d) => {
                self.released.remove(&d);
                self.pending.insert(d, Instant::now());
            }
            DeviceEvent::Disappeared(d) => {
                self.pending.remove(&d);
                self.queue.retain(|j| j.disk != d);
                let was_handled = self.handled.remove(&d);
                match self.active.as_ref().filter(|a| a.disk == d) {
                    Some(a) if a.releasing.load(Ordering::SeqCst) => {
                        // Our own unmount/eject at the end of the scan.
                        self.released.insert(d.clone());
                        let _ = self.ev.send(Event::Detached { disk: d, expected: true });
                    }
                    Some(a) => {
                        if !a.control.is_cancelled() {
                            a.control.cancel();
                            self.log(Level::Warn, format!("/dev/{d} was removed during the scan; it will resume next time."));
                        }
                    }
                    None if was_handled => {
                        let expected = self.released.contains(&d);
                        let _ = self.ev.send(Event::Detached { disk: d, expected });
                    }
                    None => {}
                }
            }
        }
    }

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Scan { disk, identity, label } => {
                let busy = self.active.as_ref().is_some_and(|a| a.disk == disk) || self.queue.iter().any(|j| j.disk == disk);
                if busy {
                    self.log(Level::Info, format!("/dev/{disk} is already being scanned or queued."));
                } else {
                    self.handled.insert(disk.clone());
                    self.queue.push_back(Job { disk, identity, label });
                }
            }
            Command::Pause => {
                if let Some(a) = &self.active {
                    a.control.pause();
                    self.log(Level::Info, format!("Paused the scan of /dev/{}.", a.disk));
                }
            }
            Command::Resume => {
                if let Some(a) = &self.active {
                    a.control.resume();
                    self.log(Level::Info, format!("Resumed the scan of /dev/{}.", a.disk));
                }
            }
            Command::Cancel => {
                if let Some(a) = &self.active {
                    a.control.cancel();
                    a.control.resume();
                    self.log(Level::Info, format!("Stopping the scan of /dev/{} (it resumes next time).", a.disk));
                }
            }
            Command::Unmount(disk) | Command::Eject(disk) if self.active.as_ref().is_some_and(|a| a.disk == disk) => {
                self.log(Level::Warn, format!("/dev/{disk} is being scanned; pause or stop the scan first."));
            }
            Command::Unmount(disk) => self.release(&disk, false),
            Command::Eject(disk) => self.release(&disk, true),
            Command::Shutdown => {
                self.shutting_down = true;
                self.queue.clear();
                if let Some(a) = &self.active {
                    a.control.cancel();
                    a.control.resume();
                }
            }
        }
    }

    fn release(&mut self, disk: &str, eject: bool) {
        let result = di_platform::probe(disk).and_then(|p| {
            self.released.insert(p.id.clone());
            di_platform::unmount_drive(&p, false)?;
            if eject {
                di_platform::eject(&p)?;
            }
            Ok(p)
        });
        match result {
            Ok(p) if eject => self.log(Level::Info, format!("Ejected {} — safe to remove.", p.dev_path)),
            Ok(p) => self.log(Level::Info, format!("Unmounted {}.", p.dev_path)),
            Err(e) => {
                self.released.remove(disk);
                self.log(Level::Error, format!("/dev/{disk}: {e:#}"))
            }
        }
    }

    fn settle(&mut self) {
        let ready: Vec<String> =
            self.pending.iter().filter(|(_, t)| t.elapsed() >= SETTLE).map(|(d, _)| d.clone()).collect();
        for d in ready {
            self.pending.remove(&d);
            if self.shutting_down {
                continue;
            }
            let Ok(id) = di_platform::resolve_whole(&d) else { continue };
            let busy = self.handled.contains(&id)
                || self.queue.iter().any(|j| j.disk == id)
                || self.active.as_ref().is_some_and(|a| a.disk == id);
            if busy {
                continue;
            }
            self.handled.insert(id.clone());
            let Ok(probe) = di_platform::probe(&id) else { continue };
            let (drive_id, label, outcome) = self.decide(&probe);
            if outcome == Outcome::Queued {
                self.queue.push_back(Job { disk: id.clone(), identity: IdentityChoice::Auto, label: None });
            }
            if matches!(outcome, Outcome::Ignored(_)) && !probe.is_external() {
                continue; // internal disks: not worth an event
            }
            let _ = self.ev.send(Event::Attached { disk: id, probe: Box::new(probe), drive_id, label, outcome });
        }
    }

    fn decide(&self, probe: &DriveProbe) -> (Option<i64>, Option<String>, Outcome) {
        let p = &self.opts.policy;
        if probe.system_disk {
            return (None, None, Outcome::Ignored("system disk".into()));
        }
        if !probe.is_external() {
            return (None, None, Outcome::Ignored("internal disk".into()));
        }
        if probe.bus == Bus::Image && !p.include_images {
            return (None, None, Outcome::Ignored("disk image".into()));
        }
        let mut catalog = match Catalog::open(&self.opts.catalog_path) {
            Ok(c) => c,
            Err(e) => return (None, None, Outcome::Ignored(format!("cannot open catalog: {e:#}"))),
        };
        match catalog.match_drive(&to_record(probe)) {
            Ok(DriveMatch::New) => (
                None,
                None,
                match p.new_drive {
                    NewDrivePolicy::Ask => Outcome::Ask(Question::NewDrive),
                    NewDrivePolicy::Scan => Outcome::Queued,
                    NewDrivePolicy::Report => Outcome::Reported,
                },
            ),
            Ok(DriveMatch::Ambiguous(c)) => (None, None, Outcome::Ask(Question::Ambiguous(c))),
            Ok(DriveMatch::Known { drive_id, .. }) => {
                // Record the connection, then check how fresh each filesystem's scan is.
                let fs_ids = match record_drive(&mut catalog, probe, IdentityChoice::SameAs(drive_id), None) {
                    Ok((_, _, _, ids)) => ids,
                    Err(e) => return (Some(drive_id), None, Outcome::Ignored(format!("{e:#}"))),
                };
                let label = catalog.drive_label(drive_id).ok().flatten();
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
                            stale |= now - t >= p.rescan_after_secs;
                        }
                        None => stale = true,
                    }
                    // Metadata still missing (first connect since the content phase
                    // existed, an interrupted phase, or ffprobe installed since).
                    stale |= crate::content::has_pending_media(catalog.conn(), fs, di_media::locate_tool("ffprobe").is_some())
                        .unwrap_or(false);
                }
                let outcome = if stale { Outcome::Queued } else { Outcome::UpToDate { last_scan: oldest.unwrap_or(now) } };
                (Some(drive_id), label, outcome)
            }
            Err(e) => (None, None, Outcome::Ignored(format!("{e:#}"))),
        }
    }

    fn reap(&mut self) {
        if !self.active.as_ref().is_some_and(|a| a.handle.is_finished()) {
            return;
        }
        let a = self.active.take().unwrap();
        *self.job.lock().unwrap() = None;
        let result = match a.handle.join() {
            Ok(Ok(report)) => {
                if report.unmounted {
                    self.released.insert(a.disk.clone());
                }
                if let Ok(cat) = Catalog::open(&self.opts.catalog_path) {
                    for line in report_lines(&report) {
                        let _ = cat.log_event("info", Some(report.drive_id), "scan", line.trim());
                    }
                }
                Ok(Box::new(report))
            }
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err("scan thread panicked".into()),
        };
        let _ = self.ev.send(Event::JobFinished { disk: a.disk, result });
    }

    fn start_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(job) = self.queue.pop_front() else { return };
        let control = Arc::new(Control::default());
        let req = DeviceScanRequest {
            catalog_path: self.opts.catalog_path.clone(),
            dev: job.disk.clone(),
            label: job.label,
            walk: self.opts.walk.clone(),
            resume: ResumeMode::Auto,
            after: self.opts.policy.after,
            identity: job.identity,
            allow_internal: false,
            content: Some(crate::content::ContentConfig::default()),
        };
        let (progress, phase, ev) = (self.progress.clone(), self.phase.clone(), self.ev.clone());
        let releasing = Arc::new(AtomicBool::new(false));
        let (c, rel, disk) = (control.clone(), releasing.clone(), job.disk.clone());
        let threads = self.opts.walk.threads;
        let view = self.job.clone();
        let handle = std::thread::Builder::new()
            .name(format!("scan-{}", job.disk))
            .spawn(move || {
                let mut on_step = |step: Step| {
                    if matches!(step, Step::Unmounting | Step::Ejecting) {
                        rel.store(true, Ordering::SeqCst);
                    }
                    if let Step::Scanning { vol, mount_point } = &step {
                        if let Some(j) = view.lock().unwrap().as_mut() {
                            j.volume = Some(format!("{} {} \"{}\"", vol.dev, vol.fs_type, vol.label.as_deref().unwrap_or("")));
                            j.expected_bytes = di_walk::host::fs_usage(mount_point).used_bytes;
                        }
                    }
                    if let Some(text) = step_text(&step, threads) {
                        let _ = ev.send(Event::JobStep { disk: disk.clone(), text });
                    }
                };
                scan_device(&req, &c, &progress, &phase, &mut on_step)
            });
        match handle {
            Ok(handle) => {
                *self.job.lock().unwrap() = Some(JobView {
                    disk: job.disk.clone(),
                    started: Instant::now(),
                    control: control.clone(),
                    volume: None,
                    expected_bytes: None,
                });
                let _ = self.ev.send(Event::JobStarted { disk: job.disk.clone() });
                self.active = Some(Active { disk: job.disk, control, releasing, handle });
            }
            Err(e) => self.log(Level::Error, format!("cannot start scan thread: {e}")),
        }
    }
}
