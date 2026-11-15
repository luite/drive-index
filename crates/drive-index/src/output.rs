//! Plain-text and JSON output for the CLI.

use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;

use di_core::catalog::{DriveSummary, FsSummary, ScanSummary};
use di_core::search::{Hit, Summary};
use di_model::units::{format_count, format_size};
use di_scan::{Phase, PhaseCell};
use di_walk::{Control, Progress};

pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{:.0} ms", s * 1000.0)
    } else if s < 60.0 {
        format!("{s:.1} s")
    } else if s < 3600.0 {
        format!("{}m {:02}s", s as u64 / 60, s as u64 % 60)
    } else {
        format!("{}h {:02}m", s as u64 / 3600, (s as u64 % 3600) / 60)
    }
}

fn local_ns(ns: Option<i64>) -> String {
    ns.and_then(|ns| Timestamp::from_nanosecond(ns as i128).ok())
        .map(|t| t.to_zoned(TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

fn local_secs(s: Option<i64>) -> String {
    local_ns(s.map(|s| s.saturating_mul(1_000_000_000)))
}

fn terminal_width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 { ws.ws_col as usize } else { 100 }
}

/// Keep the end of a path, which is the informative part: `…/2014-07 Iceland/RAW`.
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep: String = s.chars().skip(n - max.saturating_sub(1)).collect();
    format!("…{keep}")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Redraw a one-line progress display on stderr until the scan is done.
pub fn progress_loop(p: &Progress, phase: &PhaseCell, control: &Control, start: Instant) {
    let mut err = std::io::stderr();
    let mut last = (Instant::now(), 0u64);
    let mut rate = 0.0f64;
    loop {
        let ph = phase.get();
        if ph == Phase::Done {
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
            return;
        }
        let files = p.files.load(Ordering::Relaxed);
        let now = Instant::now();
        let dt = now.duration_since(last.0).as_secs_f64();
        if dt >= 0.5 {
            let inst = (files - last.1) as f64 / dt;
            rate = if rate == 0.0 { inst } else { rate * 0.6 + inst * 0.4 };
            last = (now, files);
        }
        let label = match ph {
            Phase::Preparing => "preparing",
            Phase::Walking if control.is_cancelled() => "stopping",
            Phase::Walking => "scanning",
            Phase::Aggregating => "totals",
            Phase::Merging => "merging",
            Phase::Done => "done",
        };
        let head = format!(
            "{label:<9} {} files  {} dirs  {}  {}/s  {}  ",
            format_count(files),
            format_count(p.dirs.load(Ordering::Relaxed)),
            format_size(p.bytes.load(Ordering::Relaxed)),
            format_count(rate as u64),
            format_duration(start.elapsed()),
        );
        let width = terminal_width();
        let room = width.saturating_sub(head.chars().count() + 1);
        let path = if ph == Phase::Walking && room > 10 { tail(&p.current_path(), room) } else { String::new() };
        let _ = write!(err, "\r\x1b[2K{head}{path}");
        let _ = err.flush();
        std::thread::sleep(Duration::from_millis(150));
    }
}

pub fn print_hits(hits: &[Hit]) {
    if hits.is_empty() {
        println!("No matches.");
        return;
    }
    println!("{:<18} {:>10}  {:<16}  PATH", "DRIVE", "SIZE", "MODIFIED");
    for h in hits {
        let drive = format!("D{} {}", h.drive_id, h.drive_label.as_deref().unwrap_or(""));
        let path = match &h.link_target {
            Some(t) => format!("{} -> {}", h.path, t),
            None => h.path.clone(),
        };
        println!(
            "{:<18} {:>10}  {:<16}  {}",
            clip(&drive, 18),
            format_size(h.size),
            local_ns(h.mtime_ns),
            path
        );
    }
}

pub fn print_summary(s: &Summary, shown: Option<usize>) {
    let mut line = format!("Σ {} files · {}", format_count(s.files), format_size(s.bytes));
    if !s.per_drive.is_empty() {
        let drives: Vec<String> = s
            .per_drive
            .iter()
            .take(8)
            .map(|d| format!("D{} {}", d.drive_id, format_count(d.files)))
            .collect();
        line.push_str(&format!(" · {} drive(s): {}", s.per_drive.len(), drives.join(" · ")));
    }
    if let Some(n) = shown {
        if (n as u64) < s.files {
            line.push_str(&format!("  (showing {n}; use --limit)"));
        }
    }
    println!("{line}");
}

pub fn print_drives(drives: &[DriveSummary], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(drives)?);
        return Ok(());
    }
    if drives.is_empty() {
        println!("No drives in the catalog yet. Scan one with `drive-index scan <DIR>`.");
        return Ok(());
    }
    println!("{:<5} {:<24} {:<8} {:>10} {:>12} {:>10}  {:<16}", "ID", "LABEL", "BUS", "CAPACITY", "FILES", "BYTES", "LAST SCANNED");
    for d in drives {
        println!(
            "{:<5} {:<24} {:<8} {:>10} {:>12} {:>10}  {:<16}",
            format!("D{}", d.id),
            clip(d.label.as_deref().unwrap_or("(unlabeled)"), 24),
            d.bus.as_deref().unwrap_or("-"),
            format_size(d.capacity_bytes),
            format_count(d.files),
            format_size(d.bytes),
            local_secs(d.last_scanned_at),
        );
    }
    Ok(())
}

pub fn print_filesystems(list: &[FsSummary], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    println!("{:<7} {:<8} {:<20} {:>10} {:>10} {:>12}  {:<16}  MOUNT", "ID", "TYPE", "LABEL", "SIZE", "USED", "FILES", "SCANNED");
    for f in list {
        println!(
            "{:<7} {:<8} {:<20} {:>10} {:>10} {:>12}  {:<16}  {}",
            format!("D{}.{}", f.drive_id, f.ordinal),
            f.fs_type,
            clip(f.label.as_deref().unwrap_or("-"), 20),
            f.size_bytes.map(format_size).unwrap_or_else(|| "-".into()),
            f.used_bytes.map(format_size).unwrap_or_else(|| "-".into()),
            f.files.map(format_count).unwrap_or_else(|| "-".into()),
            local_secs(f.scanned_at),
            f.mount_point.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

pub fn print_scans(list: &[ScanSummary], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    println!(
        "{:<6} {:<6} {:<12} {:<16} {:<16} {:>9} {:>12} {:>10} {:>10} {:>7}",
        "SCAN", "DRIVE", "STATUS", "METHOD", "STARTED", "TIME", "FILES", "DIRS", "BYTES", "ERRORS"
    );
    for s in list {
        println!(
            "{:<6} {:<6} {:<12} {:<16} {:<16} {:>9} {:>12} {:>10} {:>10} {:>7}",
            format!("#{}", s.id),
            format!("D{}", s.drive_id),
            s.status,
            s.method,
            local_secs(Some(s.started_at)),
            s.active_ms.map(|ms| format_duration(Duration::from_millis(ms as u64))).unwrap_or_else(|| "-".into()),
            format_count(s.files),
            format_count(s.dirs),
            format_size(s.bytes),
            s.errors,
        );
    }
    Ok(())
}
