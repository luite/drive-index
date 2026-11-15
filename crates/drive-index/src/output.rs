//! Plain-text and JSON output for the CLI.

use std::time::Duration;

use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;

use di_core::catalog::{DriveSummary, FsSummary, ScanSummary};
use di_core::search::{Hit, Summary};
use di_model::units::{format_count, format_size};

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



fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
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

pub fn print_probe(p: &di_platform::DriveProbe) {
    let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    let mut flags = Vec::new();
    if p.system_disk {
        flags.push("system");
    }
    flags.push(if p.internal { "internal" } else { "external" });
    if p.removable {
        flags.push("removable");
    }
    println!(
        "{}  {}  {}  {}  {}  [{}]",
        p.dev_path,
        p.model.as_deref().unwrap_or("?"),
        format_size(p.capacity_bytes),
        p.bus.as_str(),
        p.ptable_type.as_deref().unwrap_or("?"),
        flags.join(", ")
    );
    println!(
        "  vendor {}  serial {}  rev {}  medium {}  block {}",
        opt(&p.vendor),
        opt(&p.serial),
        opt(&p.revision),
        p.medium.as_str(),
        p.logical_block_size.map(|b| format!("{b} B")).unwrap_or_else(|| "-".into())
    );
    if let Some(u) = &p.usb {
        println!(
            "  usb {:04x}:{:04x}  {} {}  serial {}  {}",
            u.vendor_id.unwrap_or(0),
            u.product_id.unwrap_or(0),
            opt(&u.manufacturer),
            opt(&u.product),
            opt(&u.serial),
            u.speed_mbps.map(|s| format!("{s} Mb/s")).unwrap_or_default()
        );
    }
    if !p.partitions.is_empty() {
        println!("  {:<3} {:<10} {:>14} {:>10}  {:<24} PARTITION UUID", "#", "DEV", "START", "SIZE", "TYPE");
        for part in &p.partitions {
            println!(
                "  {:<3} {:<10} {:>14} {:>10}  {:<24} {}",
                part.number,
                part.dev,
                format_count(part.start_bytes),
                format_size(part.size_bytes),
                clip(part.type_name.as_deref().or(part.type_id.as_deref()).unwrap_or("-"), 24),
                part.part_uuid.as_deref().unwrap_or("-")
            );
        }
    }
    for v in &p.volumes {
        let state = match (&v.mount_point, v.mounted_writable) {
            (Some(mp), Some(true)) => format!("mounted rw at {}", mp.display()),
            (Some(mp), _) => format!("mounted ro at {}", mp.display()),
            (None, _) => "not mounted".into(),
        };
        println!(
            "  fs {:<9} {:<8} {:<16} part {}  uuid {}  {}{}",
            v.dev,
            v.fs_type,
            clip(v.label.as_deref().unwrap_or("-"), 16),
            v.partition_number.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            v.fs_uuid.as_deref().unwrap_or("-"),
            state,
            v.unscannable_reason.as_ref().map(|r| format!("  (unscannable: {r})")).unwrap_or_default()
        );
    }
    let ids = di_platform::identity::identifiers(p);
    if !ids.is_empty() {
        let list: Vec<String> = ids.iter().map(|i| format!("{}={}", i.kind, i.value)).collect();
        println!("  identifiers: {}", list.join("  "));
    }
    println!();
}
