//! Plain-text and JSON output for the CLI.

use std::time::Duration;

use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;

use di_core::catalog::{DriveSummary, FsSummary, ScanSummary};
use di_core::search::{Hit, Summary};
use di_model::units::{format_count, format_size};

pub use di_model::units::format_duration;

fn local_ns(ns: Option<i64>) -> String {
    ns.and_then(|ns| Timestamp::from_nanosecond(ns as i128).ok())
        .map(|t| t.to_zoned(TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

pub fn local_time(s: Option<i64>) -> String {
    local_secs(s)
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
    let info = hits.iter().any(|h| h.media.is_some());
    if info {
        println!("{:<18} {:>10}  {:<16}  {:<40}  PATH", "DRIVE", "SIZE", "MODIFIED", "INFO");
    } else {
        println!("{:<18} {:>10}  {:<16}  PATH", "DRIVE", "SIZE", "MODIFIED");
    }
    for h in hits {
        let drive = format!("D{} {}", h.drive_id, h.drive_label.as_deref().unwrap_or(""));
        let path = match &h.link_target {
            Some(t) => format!("{} -> {}", h.path, t),
            None => h.path.clone(),
        };
        if info {
            let line = h.media.as_ref().map(|m| m.line()).unwrap_or_default();
            println!(
                "{:<18} {:>10}  {:<16}  {:<40}  {}",
                clip(&drive, 18),
                format_size(h.size),
                local_ns(h.mtime_ns),
                clip(&line, 40),
                path
            );
        } else {
            println!("{:<18} {:>10}  {:<16}  {}", clip(&drive, 18), format_size(h.size), local_ns(h.mtime_ns), path);
        }
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

pub fn print_doctor(r: &di_doctor::Report, db: &std::path::Path, state: &std::path::Path) {
    use di_doctor::Status;
    println!(
        "drive-index doctor — {} · {} · running as {}{}",
        r.host,
        r.os,
        if r.running_as_root { "root" } else { "a normal user" },
        r.installer.as_ref().map(|i| format!(" · package manager: {i}")).unwrap_or_default()
    );
    println!();
    println!("{:<20} {:<12} {:<10} ENABLES", "COMPONENT", "STATUS", "VERSION");
    for c in &r.checks {
        let status = match c.status {
            Status::Ok => "✔ ok",
            Status::Warn => "⚠ warning",
            Status::Missing => "✖ missing",
            Status::Error => "✖ error",
            Status::Info => "· info",
        };
        println!(
            "{:<20} {:<12} {:<10} {}",
            c.name,
            status,
            clip(c.version.as_deref().unwrap_or("-"), 10),
            c.enables
        );
        if let Some(d) = &c.detail {
            println!("{:<20} {d}", "");
        }
        if let Some(f) = c.fix.as_ref().filter(|_| c.status != Status::Ok) {
            println!("{:<20} fix: {f}", "");
        }
    }
    println!();
    if let Some(cmd) = &r.install_command {
        println!("Install everything missing:  {cmd}");
    }
    let size = std::fs::metadata(db).map(|m| format_size(m.len())).unwrap_or_else(|_| "not created yet".into());
    println!("Catalog: {} ({size})", db.display());
    println!("Host state: {}", state.display());
}

/// The search language reference (`search --filters`).
pub fn print_filters(json: bool) -> anyhow::Result<()> {
    use di_core::filters::{EXAMPLES, FILTERS, TIPS};
    if json {
        println!("{}", serde_json::to_string_pretty(FILTERS)?);
        return Ok(());
    }
    let mut group = "";
    for d in FILTERS {
        if d.group != group {
            group = d.group;
            println!("\n{group}");
        }
        println!("  {:<46} {}", d.syntax, d.help);
        if !d.examples.is_empty() {
            println!("  {:<46} e.g. {}", "", d.examples.join("   "));
        }
    }
    println!("\nTips");
    for t in TIPS {
        println!("  {t}");
    }
    println!("\nExamples");
    for (q, what) in EXAMPLES {
        println!("  {q:<40} {what}");
    }
    Ok(())
}

pub fn print_dupes(cat: &di_core::Catalog, q: &di_core::dupes::DupeQuery, files: bool, json: bool) -> Result<()> {
    use di_core::dupes::{groups, members};
    let conn = cat.conn();
    let (list, sum) = groups(conn, q)?;
    if json {
        let out: Vec<serde_json::Value> = list
            .iter()
            .map(|g| {
                let mut v = serde_json::to_value(g).unwrap();
                if files {
                    v["files"] = serde_json::to_value(members(conn, q.confidence, &g.key, g.size).unwrap_or_default()).unwrap();
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "summary": sum, "groups": out }))?);
        return Ok(());
    }
    println!(
        "{} duplicate groups ({}), {} files, {} in extra copies",
        format_count(sum.groups),
        q.confidence.as_str(),
        format_count(sum.files),
        format_size(sum.wasted)
    );
    if list.is_empty() {
        if q.confidence == di_core::dupes::Confidence::Exact {
            println!("Exact duplicates need full hashes: scan connected drives with --verify.");
        }
        return Ok(());
    }
    println!();
    println!("{:>10} {:>6} {:>10}  {:<18} Name", "Size", "Copies", "Extra", "Drives");
    for g in &list {
        let drives: Vec<String> = g.drives.iter().map(|d| format!("D{d}")).collect();
        println!(
            "{:>10} {:>6} {:>10}  {:<18} {}",
            format_size(g.size),
            g.copies,
            format_size(g.wasted),
            clip(&drives.join(" "), 18),
            g.name
        );
        if files {
            for m in members(conn, q.confidence, &g.key, g.size)? {
                println!("{:>30}  D{:<3} {}", "", m.drive_id, m.path);
            }
        }
    }
    if (sum.groups as usize) > list.len() {
        println!("… {} more groups (--limit)", format_count(sum.groups - list.len() as u64));
    }
    Ok(())
}

pub fn print_collections(list: &[di_core::coverage::Collection], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    println!("{:<16} {:<24} Query", "Slug", "Name");
    for c in list {
        println!("{:<16} {:<24} {}", c.slug, c.name, c.query);
    }
    println!("\n`drive-index coverage <slug>` shows which drives hold it.");
    Ok(())
}

pub fn print_coverage(c: &di_core::coverage::Coverage, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(c)?);
        return Ok(());
    }
    let total = c.items.max(1) as f64;
    println!("{} (in:{}, {})", c.collection.name, c.collection.slug, c.collection.query);
    println!(
        "{} items, {}; {} identified by content hash",
        format_count(c.items),
        format_size(c.bytes),
        format_count(c.hashed_items)
    );
    for (label, n) in ["1 copy", "2 copies", "3+ copies"].iter().zip(c.redundancy) {
        println!("  {label:<10} {:>10} ({:>3.0}%)", format_count(n), 100.0 * n as f64 / total);
    }
    if c.redundancy[0] > 0 {
        println!(
            "  {} on a single drive: drive-index search in:{} copies:1",
            format_size(c.only_bytes),
            c.collection.slug
        );
    }
    println!();
    println!("{:<22} {:>10} {:>6} {:>10} {:>20}  {:<16} SMART", "Drive", "Items", "Share", "Size", "Only here", "Last seen");
    for d in &c.drives {
        let label = format!("D{} {}", d.drive_id, d.label.as_deref().unwrap_or(""));
        let only = if d.only_items > 0 { format!("{} ({})", format_count(d.only_items), format_size(d.only_bytes)) } else { "-".into() };
        let smart = match d.smart_passed {
            Some(true) => "ok",
            Some(false) => "FAILING",
            None => "-",
        };
        println!(
            "{:<22} {:>10} {:>5.0}% {:>10} {:>20}  {:<16} {smart}",
            clip(&label, 22),
            format_count(d.items),
            d.pct,
            format_size(d.bytes),
            only,
            local_secs(Some(d.last_seen_at))
        );
    }
    if !c.years.is_empty() {
        println!();
        let max = c.years.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1) as f64;
        for (y, n) in &c.years {
            println!("  {y}  {:<40} {}", "█".repeat(((*n as f64 / max) * 40.0).ceil() as usize), format_count(*n));
        }
    }
    Ok(())
}

fn opt<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_else(|| "-".into())
}

pub fn print_smart(p: &di_platform::DriveProbe, r: &di_model::smart::SmartReport) {
    println!("{} — {} {}", p.dev_path, r.model.as_deref().or(p.model.as_deref()).unwrap_or("drive"), format_size(p.capacity_bytes));
    println!("  Health      {}", r.summary());
    for w in r.warnings() {
        println!("  ⚠ {w}");
    }
    println!("  Protocol    {} (smartctl -d {})", opt(&r.protocol), r.device_type);
    println!("  Serial      {}", opt(&r.serial));
    println!("  WWN         {}", opt(&r.wwn));
    println!("  Firmware    {}", opt(&r.firmware));
    if let Some(rpm) = r.rotation_rpm {
        println!("  Rotation    {}", if rpm == 0 { "solid state".to_string() } else { format!("{rpm} rpm") });
    }
    println!("  Power       {} hours, {} cycles", opt(&r.power_on_hours), opt(&r.power_cycles));
    if r.nvme_unsafe_shutdowns.is_some() {
        println!("  Unsafe shutdowns {}", opt(&r.nvme_unsafe_shutdowns));
    }
    if r.udma_crc_errors.is_some() {
        println!("  CRC errors  {}", opt(&r.udma_crc_errors));
    }
}

pub fn print_smart_history(
    id: i64,
    history: &[di_core::smart::SmartView],
    access: &di_core::smart::SmartAccess,
    json: bool,
) -> Result<()> {
    if json {
        let rows: Vec<serde_json::Value> = history
            .iter()
            .map(|h| {
                let r = &h.report;
                serde_json::json!({
                    "taken_at": h.taken_at, "passed": r.passed, "health_source": r.health_source.as_str(),
                    "failing_attributes": r.failing_attributes.iter()
                        .map(|a| serde_json::json!({ "id": a.id, "name": a.name, "now": a.now, "prefail": a.prefail }))
                        .collect::<Vec<_>>(),
                    "temperature_c": r.temperature_c,
                    "power_on_hours": r.power_on_hours, "power_cycles": r.power_cycles,
                    "reallocated_sectors": r.reallocated_sectors, "pending_sectors": r.pending_sectors,
                    "offline_uncorrectable": r.offline_uncorrectable, "udma_crc_errors": r.udma_crc_errors,
                    "nvme_percentage_used": r.nvme_percentage_used, "nvme_available_spare": r.nvme_available_spare,
                    "nvme_media_errors": r.nvme_media_errors, "nvme_unsafe_shutdowns": r.nvme_unsafe_shutdowns,
                    "data_written_bytes": r.data_written_bytes,
                })
            })
            .collect();
        let out = serde_json::json!({
            "drive_id": id,
            "available": access.available,
            "unavailable_reason": access.reason,
            "device_type": access.device_type,
            "readings": rows,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if history.is_empty() {
        match &access.reason {
            Some(r) => println!("D{id}: no SMART readings ({r})."),
            None => println!("D{id}: no SMART readings yet; they are taken when the drive is scanned."),
        }
        return Ok(());
    }
    println!("D{id} SMART readings, newest first:");
    for h in history {
        println!("  {}  {}", local_secs(Some(h.taken_at)), h.report.summary());
    }
    for w in history[0].report.warnings() {
        println!("  ⚠ {w}");
    }
    if let Some(r) = &access.reason {
        println!("  (last attempt: {r})");
    }
    Ok(())
}
