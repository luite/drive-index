//! Terminal plumbing shared by `scan` and `watch`: a redrawn progress line on stderr,
//! status messages that don't collide with it, and Ctrl-C handling.

use std::fmt::Display;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use di_model::units::{format_count, format_size};
use di_scan::{Phase, PhaseCell};
use di_walk::{Control, Progress};

use crate::output::format_duration;

/// Cloneable, thread-safe handle for printing status lines.
#[derive(Clone)]
pub struct Printer {
    lock: Arc<Mutex<()>>,
    show_progress: bool,
}

impl Printer {
    pub fn say(&self, msg: impl Display) {
        let _g = self.lock.lock().unwrap();
        if self.show_progress {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        }
        println!("{msg}");
    }
}

pub struct Term {
    pub progress: Arc<Progress>,
    pub phase: Arc<PhaseCell>,
    pub printer: Printer,
    current: Arc<Mutex<Arc<Control>>>,
    interrupted: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    reporter: Option<JoinHandle<()>>,
}

impl Term {
    pub fn start(quiet: bool) -> Result<Term> {
        Term::start_shared(quiet, Arc::new(Progress::default()), Arc::new(PhaseCell::default()))
    }

    /// Like `start`, but showing progress counters owned by someone else (the controller).
    pub fn start_shared(quiet: bool, progress: Arc<Progress>, phase: Arc<PhaseCell>) -> Result<Term> {
        let show = !quiet && std::io::stderr().is_terminal();
        let current = Arc::new(Mutex::new(Arc::new(Control::default())));
        let interrupted = Arc::new(AtomicBool::new(false));
        {
            let (current, interrupted) = (current.clone(), interrupted.clone());
            let presses = AtomicUsize::new(0);
            ctrlc::set_handler(move || {
                if presses.fetch_add(1, Ordering::SeqCst) == 0 {
                    interrupted.store(true, Ordering::SeqCst);
                    current.lock().unwrap().cancel();
                } else {
                    // Second Ctrl-C: quit now. Staging databases stay consistent.
                    std::process::exit(130);
                }
            })
            .context("installing Ctrl-C handler")?;
        }
        let printer = Printer { lock: Arc::new(Mutex::new(())), show_progress: show };
        let stop = Arc::new(AtomicBool::new(false));
        let reporter = show.then(|| {
            let (progress, phase, stop, printer, current) =
                (progress.clone(), phase.clone(), stop.clone(), printer.clone(), current.clone());
            std::thread::spawn(move || progress_loop(&progress, &phase, &stop, &printer, &current))
        });
        Ok(Term { progress, phase, printer, current, interrupted, stop, reporter })
    }

    /// The control of the current operation (cancelled by Ctrl-C).
    pub fn control(&self) -> Arc<Control> {
        self.current.lock().unwrap().clone()
    }

    pub fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    pub fn say(&self, msg: impl Display) {
        self.printer.say(msg)
    }

    pub fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(r) = self.reporter.take() {
            let _ = r.join();
        }
    }
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep: String = s.chars().skip(n - max.saturating_sub(1)).collect();
    format!("…{keep}")
}

fn terminal_width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 { ws.ws_col as usize } else { 100 }
}

fn progress_loop(p: &Progress, phase: &PhaseCell, stop: &AtomicBool, printer: &Printer, current: &Mutex<Arc<Control>>) {
    let mut last = (Instant::now(), 0u64);
    let mut rate = 0.0f64;
    let mut started: Option<Instant> = None;
    let mut drawn = false;
    while !stop.load(Ordering::SeqCst) {
        let ph = phase.get();
        let active = matches!(ph, Phase::Walking | Phase::Aggregating | Phase::Merging | Phase::Content);
        let _g = printer.lock.lock().unwrap();
        let mut err = std::io::stderr();
        if !active {
            if drawn {
                let _ = write!(err, "\r\x1b[2K");
                let _ = err.flush();
                drawn = false;
            }
            started = None;
            rate = 0.0;
        } else {
            let start = *started.get_or_insert_with(Instant::now);
            let files = p.files.load(Ordering::Relaxed);
            let now = Instant::now();
            let dt = now.duration_since(last.0).as_secs_f64();
            if dt >= 0.5 {
                let inst = files.saturating_sub(last.1) as f64 / dt;
                rate = if rate == 0.0 { inst } else { rate * 0.6 + inst * 0.4 };
                last = (now, files);
            }
            let cancelled = current.lock().unwrap().is_cancelled();
            let label = match ph {
                Phase::Walking | Phase::Content if cancelled => "stopping",
                Phase::Walking => "scanning",
                Phase::Aggregating => "totals",
                Phase::Content => "metadata",
                _ => "merging",
            };
            if ph == Phase::Content {
                let head = format!(
                    "{label:<9} {}/{} files  {} read  {}  ",
                    format_count(p.content_done.load(Ordering::Relaxed)),
                    format_count(p.content_total.load(Ordering::Relaxed)),
                    format_size(p.content_bytes.load(Ordering::Relaxed)),
                    format_duration(start.elapsed()),
                );
                let room = terminal_width().saturating_sub(head.chars().count() + 1);
                let path = if room > 10 { tail(&p.current_path(), room) } else { String::new() };
                let _ = write!(err, "\r\x1b[2K{head}{path}");
                let _ = err.flush();
                drawn = true;
                drop(_g);
                std::thread::sleep(Duration::from_millis(150));
                continue;
            }
            let head = format!(
                "{label:<9} {} files  {} dirs  {}  {}/s  {}  ",
                format_count(files),
                format_count(p.dirs.load(Ordering::Relaxed)),
                format_size(p.bytes.load(Ordering::Relaxed)),
                format_count(rate as u64),
                format_duration(start.elapsed()),
            );
            let room = terminal_width().saturating_sub(head.chars().count() + 1);
            let path = if ph == Phase::Walking && room > 10 { tail(&p.current_path(), room) } else { String::new() };
            let _ = write!(err, "\r\x1b[2K{head}{path}");
            let _ = err.flush();
            drawn = true;
        }
        drop(_g);
        std::thread::sleep(Duration::from_millis(150));
    }
    if drawn {
        let _g = printer.lock.lock().unwrap();
        let _ = write!(std::io::stderr(), "\r\x1b[2K");
    }
}
