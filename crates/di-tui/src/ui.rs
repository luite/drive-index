//! Drawing (PLAN.md §7). Pure functions of `App`; no state changes here except
//! scroll offsets in table/list state.

use std::sync::atomic::Ordering;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap};

use di_core::search::SortKey;
use di_core::views::BrowseSort;
use di_doctor::Status;
use di_model::units::{format_count, format_duration, format_size};
use di_scan::Phase;
use di_scan::controller::Level;

use crate::app::{App, ConnStatus, ListHit, ListKind, Modal, Screen};

// -------------------------------------------------------------------------------------
// Styling (16-colour safe, NO_COLOR respected)
// -------------------------------------------------------------------------------------

fn colors() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn fg(c: Color) -> Style {
    if colors() { Style::new().fg(c) } else { Style::new() }
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

fn block(title: impl Into<String>) -> Block<'static> {
    Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(Span::styled(format!(" {} ", title.into()), bold()))
}

fn local_time(secs: Option<i64>) -> String {
    secs.and_then(|s| jiff::Timestamp::from_second(s).ok())
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

fn local_ns(ns: Option<i64>) -> String {
    local_time(ns.map(|n| n.div_euclid(1_000_000_000)))
}

fn ago(secs: Option<i64>) -> String {
    let Some(s) = secs else { return "never".into() };
    let d = di_core::now_secs() - s;
    match d {
        d if d < 120 => "now".into(),
        d if d < 7200 => format!("{}m", d / 60),
        d if d < 2 * 86_400 => format!("{}h", d / 3600),
        d if d < 60 * 86_400 => format!("{}d", d / 86_400),
        d if d < 730 * 86_400 => format!("{}mo", d / (30 * 86_400)),
        d => format!("{}y", d / (365 * 86_400)),
    }
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

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

// -------------------------------------------------------------------------------------
// Frame
// -------------------------------------------------------------------------------------

pub fn draw(f: &mut Frame, app: &mut App) {
    let [top, body, status, keys] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());
    app.hits.list = None;
    app.hits.search_input = None;
    app.hits.modal = None;
    draw_tabs(f, app, top);
    match app.screen {
        Screen::Drives => draw_drives(f, app, body),
        Screen::Search => draw_search(f, app, body),
        Screen::Browse => draw_browse(f, app, body),
        Screen::Dupes => draw_placeholder(
            f,
            body,
            "Duplicates",
            "Duplicate detection needs content hashes, which arrive with the content phase in M4.",
        ),
        Screen::Collections => draw_placeholder(
            f,
            body,
            "Collections",
            "Collection coverage (\"which drives hold how much of my RAW photos\") arrives in M4. Meanwhile, search with in:photos-raw.",
        ),
        Screen::Log => draw_log(f, app, body),
        Screen::Setup => draw_setup(f, app, body),
    }
    draw_status(f, app, status);
    draw_keys(f, app, keys);
    if app.modal.is_some() {
        draw_modal(f, app);
    }
}

fn draw_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let mut spans = vec![Span::styled(" drive-index ", bold())];
    let mut x = area.x + spans[0].width() as u16;
    app.hits.tabs.clear();
    app.hits.tabs_y = area.y;
    for (i, s) in Screen::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, s.title());
        let w = label.chars().count() as u16;
        app.hits.tabs.push((x, x + w, *s));
        x += w;
        spans.push(if *s == app.screen { Span::styled(label, selected().add_modifier(Modifier::BOLD)) } else { Span::raw(label) });
    }
    let online = app.connected.len();
    let mut right = format!("● {online} online");
    if app.job_running() {
        right.push_str("  ⟳ scanning");
    }
    if app.viewer.is_some() {
        right.push_str("  viewer mode");
    }
    let used: usize = spans.iter().map(|s| s.width()).sum();
    let pad = (area.width as usize).saturating_sub(used + right.chars().count() + 1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, dim()));
    f.render_widget(Line::from(spans), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let text = match (&app.toast, &app.viewer) {
        (Some((t, _)), _) => Line::from(Span::styled(format!(" {t}"), fg(Color::Yellow))),
        (None, Some(v)) => Line::from(Span::styled(format!(" Viewer mode: {v} controls drives; search and browse work."), dim())),
        _ => {
            let total: u64 = app.catalog_drives.iter().map(|d| d.files).sum();
            Line::from(Span::styled(
                format!(
                    " {} · {} drives · {} files",
                    app.catalog_path.display(),
                    app.catalog_drives.len(),
                    format_count(total)
                ),
                dim(),
            ))
        }
    };
    f.render_widget(text, area);
}

fn draw_keys(f: &mut Frame, app: &App, area: Rect) {
    let keys = match app.screen {
        Screen::Drives => "s scan  p pause/resume  x stop  u unmount  e eject  l label  n notes  b browse",
        Screen::Search if app.search.focused => "type a query  ⏎/↓ results  esc leave the box",
        Screen::Search => "⏎ details  s sort  d reverse  c copy path  b browse here  i/↑ edit query",
        Screen::Browse => "⏎/→ open  ←/⌫ up  s sort",
        Screen::Setup => "r re-check  c copy install command  H hand off a catalog copy",
        _ => "",
    };
    f.render_widget(
        Line::from(vec![Span::styled(format!(" {keys}"), dim()), Span::styled("   1-7 screens  / search  ? help  q quit", dim())]),
        area,
    );
}

fn draw_placeholder(f: &mut Frame, area: Rect, title: &str, text: &str) {
    let p = Paragraph::new(text).block(block(title)).wrap(Wrap { trim: true }).alignment(Alignment::Center);
    f.render_widget(p, centered(area, 70, 7));
}

// -------------------------------------------------------------------------------------
// Drives
// -------------------------------------------------------------------------------------

fn status_glyph(s: &Option<ConnStatus>) -> Span<'static> {
    match s {
        None => Span::raw("  "),
        Some(ConnStatus::Scanning) | Some(ConnStatus::Queued) => Span::styled("⟳ ", fg(Color::Cyan)),
        Some(ConnStatus::NeedsAnswer) => Span::styled("? ", fg(Color::Yellow)),
        Some(ConnStatus::Failed(_)) => Span::styled("✖ ", fg(Color::Red)),
        Some(_) => Span::styled("● ", fg(Color::Green)),
    }
}

fn draw_drives(f: &mut Frame, app: &mut App, area: Rect) {
    let wide = area.width >= 110;
    let (list_area, right) = if wide {
        let [l, r] = Layout::horizontal([Constraint::Length(44), Constraint::Min(40)]).areas(area);
        (l, r)
    } else {
        let [l, r] = Layout::vertical([Constraint::Length(8), Constraint::Min(10)]).areas(area);
        (l, r)
    };
    let [detail_area, jobs_area] = Layout::vertical([Constraint::Min(8), Constraint::Length(10)]).areas(right);

    // List
    let rows: Vec<Row> = app
        .drives
        .iter()
        .map(|d| {
            Row::new(vec![
                Cell::from(status_glyph(&d.status)),
                Cell::from(d.drive_id.map(|i| format!("D{i}")).unwrap_or_else(|| "new".into())),
                Cell::from(clip(&d.label, 20)),
                Cell::from(format_size(d.capacity)),
                Cell::from(if d.disk.is_some() { "now".to_string() } else { ago(d.last_seen_at) }),
            ])
        })
        .collect();
    let empty = rows.is_empty();
    let table = Table::new(rows, [Constraint::Length(2), Constraint::Length(4), Constraint::Min(10), Constraint::Length(9), Constraint::Length(5)])
        .header(Row::new(vec!["", "ID", "Label", "Size", "Seen"]).style(dim()))
        .row_highlight_style(selected())
        .block(block("Drives"));
    let mut state = TableState::default().with_selected((!empty).then_some(app.drive_sel));
    f.render_stateful_widget(table, list_area, &mut state);
    app.hits.list = Some(ListHit { kind: ListKind::Drives, area: rows_area(list_area, true), offset: state.offset() });
    if empty {
        let hint = Paragraph::new("No drives yet. Connect one, or scan a directory with `drive-index scan DIR`.")
            .wrap(Wrap { trim: true })
            .style(dim());
        f.render_widget(hint, list_area.inner(ratatui::layout::Margin::new(2, 2)));
    }

    draw_drive_detail(f, app, detail_area);
    draw_jobs(f, app, jobs_area);
}

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<10}"), dim()), Span::raw(v.into())])
}

fn draw_drive_detail(f: &mut Frame, app: &App, area: Rect) {
    let row = app.drives.get(app.drive_sel);
    let connected = row.and_then(|r| r.disk.as_ref()).and_then(|d| app.connected.get(d));
    let mut lines: Vec<Line> = Vec::new();
    let title;
    match (&app.detail, connected) {
        (Some(d), conn) => {
            title = format!("D{} · {}", d.id, d.label.as_deref().unwrap_or("(unlabeled)"));
            let model = [d.vendor.as_deref(), d.model.as_deref()].iter().flatten().cloned().collect::<Vec<_>>().join(" ");
            lines.push(kv("Model", if model.is_empty() { "-".into() } else { model }));
            lines.push(kv("Serial", d.serial.clone().unwrap_or_else(|| "-".into())));
            let mut bus = d.bus.clone().unwrap_or_else(|| "-".into());
            if let (Some(v), Some(p)) = (d.usb_vendor_id, d.usb_product_id) {
                bus.push_str(&format!("  {v:04x}:{p:04x}"));
            }
            if let Some(s) = d.usb_speed_mbps {
                bus.push_str(&format!("  {s} Mb/s"));
            }
            if let Some(s) = &d.usb_serial {
                bus.push_str(&format!("  USB serial {s}"));
            }
            lines.push(kv("Bus", bus));
            lines.push(kv(
                "Medium",
                format!(
                    "{}  ·  {} capacity  ·  {} partition table",
                    d.medium.as_deref().unwrap_or("unknown"),
                    format_size(d.capacity_bytes),
                    d.ptable_type.as_deref().unwrap_or("?")
                ),
            ));
            lines.push(kv(
                "Seen",
                format!(
                    "first {} · last {} · {} connects · scanned {}",
                    local_time(Some(d.first_seen_at)),
                    if conn.is_some() { "now".into() } else { local_time(Some(d.last_seen_at)) },
                    d.connect_count,
                    ago(d.last_scanned_at)
                ),
            ));
            if let Some(n) = &d.notes {
                lines.push(kv("Notes", n.clone()));
            }
            if let Some(c) = conn {
                lines.push(kv("Status", format!("connected as {} — {}", c.probe.dev_path, status_text(&c.status))));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("{:<6} {:<7} {:<18} {:>9} {:>9} {:>10}  {}", "FS", "TYPE", "LABEL", "SIZE", "USED", "FILES", "SCANNED"),
                dim(),
            ));
            for fs in &d.filesystems {
                lines.push(Line::raw(format!(
                    "{:<6} {:<7} {:<18} {:>9} {:>9} {:>10}  {}",
                    format!("D{}.{}", d.id, fs.ordinal),
                    clip(&fs.fs_type, 7),
                    clip(fs.label.as_deref().unwrap_or("-"), 18),
                    fs.size_bytes.map(format_size).unwrap_or_else(|| "-".into()),
                    fs.used_bytes.map(format_size).unwrap_or_else(|| "-".into()),
                    fs.files.map(format_count).unwrap_or_else(|| "-".into()),
                    match (&fs.unscannable_reason, fs.scanned_at) {
                        (Some(r), _) => clip(r, 40),
                        (None, t) => ago(t),
                    }
                )));
            }
        }
        (None, Some(c)) => {
            let p = &c.probe;
            title = format!("{} · new drive", p.dev_path);
            lines.push(kv("Model", format!("{} {}", p.vendor.as_deref().unwrap_or(""), p.model.as_deref().unwrap_or("-"))));
            lines.push(kv("Serial", p.serial.clone().unwrap_or_else(|| "-".into())));
            lines.push(kv("Bus", p.bus.as_str().to_string()));
            lines.push(kv("Capacity", format_size(p.capacity_bytes)));
            lines.push(kv("Status", status_text(&c.status)));
            lines.push(Line::raw(""));
            for v in &p.volumes {
                lines.push(Line::raw(format!(
                    "{:<10} {:<7} {:<18} {}",
                    v.dev,
                    v.fs_type,
                    clip(v.label.as_deref().unwrap_or("-"), 18),
                    v.unscannable_reason.as_deref().unwrap_or("")
                )));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled("Press s to scan it into the catalog.", dim()));
        }
        (None, None) => {
            title = "Drive".into();
            lines.push(Line::styled("Select a drive.", dim()));
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)).block(block(title)), area);
}

fn status_text(s: &ConnStatus) -> String {
    match s {
        ConnStatus::Idle => "idle".into(),
        ConnStatus::Queued => "queued for scanning".into(),
        ConnStatus::Scanning => "scanning".into(),
        ConnStatus::UpToDate => "up to date".into(),
        ConnStatus::NeedsAnswer => "waiting for your answer".into(),
        ConnStatus::Done(m) => m.clone(),
        ConnStatus::Failed(e) => format!("failed: {e}"),
    }
}

fn draw_jobs(f: &mut Frame, app: &App, area: Rect) {
    let b = block("Jobs");
    let inner = b.inner(area);
    f.render_widget(b, area);
    let job = app.ctl.as_ref().and_then(|c| c.job().map(|j| (j, c.progress.clone(), c.phase.get())));
    let Some((job, p, phase)) = job else {
        let mut lines: Vec<Line> = app.job_lines.iter().map(|l| Line::raw(l.clone())).collect();
        if lines.is_empty() {
            lines.push(Line::styled(
                if app.ctl.is_some() { "No scan running. Connect a drive, or select one and press s." } else { "Viewer mode: no scans run here." },
                dim(),
            ));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    };
    let [head, gauge, stats, path, rest] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    let paused = job.control.is_paused();
    let state = if job.control.is_cancelled() {
        "stopping"
    } else if paused {
        "PAUSED"
    } else {
        match phase {
            Phase::Walking => "scanning",
            Phase::Aggregating => "totals",
            Phase::Merging => "merging",
            Phase::Content => "reading metadata",
            _ => "preparing",
        }
    };
    f.render_widget(
        Line::from(vec![
            Span::styled(format!("/dev/{} ", job.disk), bold()),
            Span::raw(job.volume.clone().unwrap_or_default()),
            Span::styled(format!("  {state}"), if paused { fg(Color::Yellow) } else { fg(Color::Cyan) }),
        ]),
        head,
    );
    let done = p.alloc_bytes.load(Ordering::Relaxed);
    let (c_done, c_total) = (p.content_done.load(Ordering::Relaxed), p.content_total.load(Ordering::Relaxed));
    let (ratio, label) = match job.expected_bytes {
        _ if phase == Phase::Content && c_total > 0 => (
            (c_done as f64 / c_total as f64).min(1.0),
            format!("metadata {}/{} files · {} read", format_count(c_done), format_count(c_total), format_size(p.content_bytes.load(Ordering::Relaxed))),
        ),
        Some(total) if total > 0 && phase == Phase::Walking => {
            let r = (done as f64 / total as f64).min(0.99);
            (r, format!("{:.0}% of {} used", r * 100.0, format_size(total)))
        }
        _ if matches!(phase, Phase::Aggregating | Phase::Merging) => (0.99, "finishing".into()),
        _ => (0.0, String::new()),
    };
    f.render_widget(Gauge::default().ratio(ratio).label(label).gauge_style(fg(Color::Cyan)), gauge);
    f.render_widget(
        Line::raw(format!(
            "{} files · {} dirs · {} · {}/s · {}",
            format_count(p.files.load(Ordering::Relaxed)),
            format_count(p.dirs.load(Ordering::Relaxed)),
            format_size(p.bytes.load(Ordering::Relaxed)),
            format_count(app.rate.files_per_sec as u64),
            format_duration(job.started.elapsed())
        )),
        stats,
    );
    f.render_widget(Line::styled(clip(&p.current_path(), path.width as usize), dim()), path);
    let lines: Vec<Line> = app.job_lines.iter().rev().take(rest.height as usize).rev().map(|l| Line::raw(l.clone())).collect();
    f.render_widget(Paragraph::new(lines), rest);
}

// -------------------------------------------------------------------------------------
// Search
// -------------------------------------------------------------------------------------

fn draw_search(f: &mut Frame, app: &mut App, area: Rect) {
    let [input_area, results_area] = Layout::vertical([Constraint::Length(5), Constraint::Min(5)]).areas(area);
    let s = &app.search;
    let mut lines = vec![Line::from(vec![Span::styled("> ", bold()), Span::raw(s.input.text().to_string())])];
    if let Some(e) = &s.error {
        let q = s.input.text();
        let start = q[..e.start.min(q.len())].chars().count();
        let width = q[e.start.min(q.len())..e.end.min(q.len())].chars().count().max(1);
        lines.push(Line::styled(format!("  {}{}", " ".repeat(start), "^".repeat(width)), fg(Color::Red)));
        lines.push(Line::styled(format!("  {}", e.message), fg(Color::Red)));
    } else if let Some(m) = &s.message {
        lines.push(Line::styled(format!("  {m}"), dim()));
    } else if let Some(sum) = &s.summary {
        let per: Vec<String> = sum.per_drive.iter().take(6).map(|d| format!("D{} {}", d.drive_id, format_count(d.files))).collect();
        lines.push(Line::styled(
            format!(
                "  Σ {} files · {} · {}{}",
                format_count(sum.files),
                format_size(sum.bytes),
                per.join(" · "),
                if s.running { "  (searching…)" } else { "" }
            ),
            dim(),
        ));
    }
    let title = format!(
        "Search  sort: {}{}",
        match s.sort {
            SortKey::Path => "path",
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Mtime => "modified",
        },
        if s.descending { " ↓" } else { " ↑" }
    );
    let b = block(title).border_style(if s.focused { fg(Color::Cyan) } else { Style::new() });
    f.render_widget(Paragraph::new(lines).block(b), input_area);
    app.hits.search_input = Some(input_area);
    let s = &app.search;
    if s.focused {
        let x = input_area.x + 3 + s.input.cursor() as u16;
        f.set_cursor_position((x.min(input_area.right().saturating_sub(2)), input_area.y + 1));
    }

    let rows: Vec<Row> = s
        .hits
        .iter()
        .map(|h| {
            Row::new(vec![
                Cell::from(format!("D{} {}", h.drive_id, clip(h.drive_label.as_deref().unwrap_or(""), 12))),
                Cell::from(match &h.link_target {
                    Some(t) => format!("{} -> {t}", h.path),
                    None => h.path.clone(),
                }),
                Cell::from(Line::from(format_size(h.size)).alignment(Alignment::Right)),
                Cell::from(local_ns(h.mtime_ns)),
                Cell::from(h.media.as_ref().map(|m| m.line()).unwrap_or_default()).style(dim()),
            ])
        })
        .collect();
    let n = rows.len();
    let info_w = if s.hits.iter().any(|h| h.media.is_some()) { results_area.width.saturating_sub(70).clamp(0, 44) } else { 0 };
    let table = Table::new(
        rows,
        [Constraint::Length(16), Constraint::Min(20), Constraint::Length(10), Constraint::Length(16), Constraint::Length(info_w)],
    )
        .header(Row::new(vec!["Drive", "Path", "Size", "Modified", "Info"]).style(dim()))
        .row_highlight_style(if s.focused { Style::new() } else { selected() })
        .block(block(format!("Results ({})", format_count(n as u64))));
    let mut state = TableState::default().with_selected((n > 0).then_some(s.sel));
    f.render_stateful_widget(table, results_area, &mut state);
    app.hits.list = Some(ListHit { kind: ListKind::Results, area: rows_area(results_area, true), offset: state.offset() });
}

// -------------------------------------------------------------------------------------
// Browse
// -------------------------------------------------------------------------------------

fn draw_browse(f: &mut Frame, app: &mut App, area: Rect) {
    let b = &app.browse;
    let Some(dir) = &b.dir else {
        let rows: Vec<Row> = b
            .filesystems
            .iter()
            .map(|c| {
                Row::new(vec![format!("D{}.{}", c.drive_id, c.ordinal), c.fs_type.clone(), c.label.clone().unwrap_or_else(|| "-".into())])
            })
            .collect();
        let n = rows.len();
        let table = Table::new(rows, [Constraint::Length(8), Constraint::Length(8), Constraint::Min(10)])
            .header(Row::new(vec!["FS", "Type", "Label"]).style(dim()))
            .row_highlight_style(selected())
            .block(block("Browse: pick a filesystem"));
        let mut state = TableState::default().with_selected((n > 0).then_some(b.fs_sel));
        f.render_stateful_widget(table, area, &mut state);
        app.hits.list = Some(ListHit { kind: ListKind::BrowseFs, area: rows_area(area, true), offset: state.offset() });
        return;
    };
    let fs = b.filesystems.iter().find(|x| x.fs_id == dir.filesystem_id);
    let fs_name = fs
        .map(|c| format!("D{}.{} {} ({})", c.drive_id, c.ordinal, c.label.clone().unwrap_or_default(), c.fs_type))
        .unwrap_or_default();
    let title = format!(
        "{fs_name} : {}   {} · {} files   sort: {}",
        dir.path,
        format_size(dir.tree_bytes),
        format_count(dir.tree_files),
        match b.sort {
            BrowseSort::Size => "size",
            BrowseSort::Count => "count",
            BrowseSort::Name => "name",
            BrowseSort::Mtime => "modified",
        }
    );
    let total = dir.tree_bytes.max(1);
    let rows: Vec<Row> = b
        .entries
        .iter()
        .map(|e| {
            let frac = e.bytes as f64 / total as f64;
            let filled = (frac * 10.0).round() as usize;
            Row::new(vec![
                Cell::from(Line::from(format_size(e.bytes)).alignment(Alignment::Right)),
                Cell::from(format!("[{}{}]", "#".repeat(filled.min(10)), " ".repeat(10 - filled.min(10)))),
                Cell::from(Line::from(format!("{:.0}%", frac * 100.0)).alignment(Alignment::Right)),
                Cell::from(if e.is_dir { format!("{}/", e.name) } else { e.name.clone() })
                    .style(if e.is_dir { bold() } else { Style::new() }),
                Cell::from(if e.is_dir { format!("{} files", format_count(e.files)) } else { String::new() }),
                Cell::from(local_ns(e.mtime_ns)),
            ])
        })
        .collect();
    let n = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(5),
            Constraint::Min(20),
            Constraint::Length(16),
            Constraint::Length(16),
        ],
    )
    .row_highlight_style(selected())
    .block(block(title));
    let mut state = TableState::default().with_selected((n > 0).then_some(b.sel));
    f.render_stateful_widget(table, area, &mut state);
    app.hits.list = Some(ListHit { kind: ListKind::BrowseEntries, area: rows_area(area, false), offset: state.offset() });
}

/// Data rows of a bordered table (minus the header row, if any).
fn rows_area(area: Rect, header: bool) -> Rect {
    let top = 1 + header as u16;
    Rect { x: area.x + 1, y: area.y + top, width: area.width.saturating_sub(2), height: area.height.saturating_sub(top + 1) }
}

// -------------------------------------------------------------------------------------
// Log and Setup
// -------------------------------------------------------------------------------------

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .log
        .iter()
        .take(area.height as usize)
        .map(|l| {
            let (glyph, style) = match l.level {
                Level::Info => ("·", Style::new()),
                Level::Warn => ("⚠", fg(Color::Yellow)),
                Level::Error => ("✖", fg(Color::Red)),
            };
            Line::from(vec![
                Span::styled(format!("{} ", local_time(Some(l.at))), dim()),
                Span::styled(format!("{glyph} "), style),
                Span::styled(l.text.clone(), style),
            ])
        })
        .collect();
    let empty = lines.is_empty();
    let p = if empty { Paragraph::new(Line::styled("Nothing logged yet.", dim())) } else { Paragraph::new(lines) };
    f.render_widget(p.block(block("Log (newest first)")), area);
}

fn draw_setup(f: &mut Frame, app: &App, area: Rect) {
    let [checks_area, catalog_area] = Layout::vertical([Constraint::Min(8), Constraint::Length(5)]).areas(area);
    match &app.setup.report {
        None => f.render_widget(Paragraph::new(Line::styled("Checking tools and permissions…", dim())).block(block("Setup")), checks_area),
        Some(r) => {
            let mut rows: Vec<Row> = Vec::new();
            for c in &r.checks {
                let (st, style) = match c.status {
                    Status::Ok => ("✔ ok", fg(Color::Green)),
                    Status::Warn => ("⚠ warning", fg(Color::Yellow)),
                    Status::Missing => ("✖ missing", fg(Color::Red)),
                    Status::Error => ("✖ error", fg(Color::Red)),
                    Status::Info => ("· info", dim()),
                };
                let mut note = c.enables.to_string();
                if let Some(d) = &c.detail {
                    note = format!("{note} — {d}");
                }
                rows.push(Row::new(vec![
                    Cell::from(c.name),
                    Cell::from(st).style(style),
                    Cell::from(c.version.clone().unwrap_or_else(|| "-".into())),
                    Cell::from(note),
                ]));
                if let Some(fix) = c.fix.as_ref().filter(|_| c.status != Status::Ok) {
                    rows.push(Row::new(vec![Cell::from(""), Cell::from(""), Cell::from(""), Cell::from(format!("fix: {fix}")).style(fg(Color::Yellow))]));
                }
            }
            let title = format!(
                "Setup — {} · {}{}{}",
                r.host,
                r.os,
                if r.running_as_root { " · root" } else { "" },
                if app.setup.running { " · re-checking…" } else { "" }
            );
            let mut table = Table::new(rows, [Constraint::Length(18), Constraint::Length(11), Constraint::Length(10), Constraint::Min(20)])
                .header(Row::new(vec!["Component", "Status", "Version", "Enables / details"]).style(dim()))
                .block(block(title));
            if let Some(cmd) = &r.install_command {
                table = table.footer(Row::new(vec![Cell::from(""), Cell::from(""), Cell::from(""), Cell::from(format!("Install all missing: {cmd}  (c copies)"))]));
            }
            f.render_widget(table, checks_area);
        }
    }
    let size = std::fs::metadata(&app.catalog_path).map(|m| format_size(m.len())).unwrap_or_else(|_| "not created yet".into());
    let lines = vec![
        kv("Path", app.catalog_path.display().to_string()),
        kv("Size", size),
        kv("Move", "H writes a consistent copy to the current directory; import it on the other host with `drive-index db import`."),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(block("Catalog")), catalog_area);
}

// -------------------------------------------------------------------------------------
// Modals
// -------------------------------------------------------------------------------------

fn draw_modal(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let Some(modal) = app.modal.take() else { return };
    draw_modal_inner(f, app, area, &modal);
    app.modal = Some(modal);
}

fn draw_modal_inner(f: &mut Frame, app: &mut App, area: Rect, modal: &Modal) {
    let (title, lines, w, h): (String, Vec<Line>, u16, u16) = match modal {
        Modal::Help => (
            "Help".into(),
            vec![
                Line::styled("Screens", bold()),
                Line::raw("  1-7 or Tab/Shift-Tab switch screens, / jumps to search, q quits"),
                Line::styled("Drives", bold()),
                Line::raw("  s scan the selected (connected) drive   p pause/resume   x stop (resumes later)"),
                Line::raw("  u unmount   e eject   l label   n notes   b/⏎ browse its files"),
                Line::styled("Search", bold()),
                Line::raw("  type a query: ext:cr2,crw  date:2014-06..2014-09  size:>10M  drive:D3  path:DCIM"),
                Line::raw("  -term negates; type:photo_raw  in:photos-raw  trash:yes  hidden:no"),
                Line::raw("  ⏎ details   s sort   d reverse   c copy path   b browse to the file"),
                Line::styled("Browse", bold()),
                Line::raw("  ⏎/→ open   ←/⌫ up   s sort by size/count/name/modified"),
                Line::raw(""),
                Line::styled("Esc closes this help.", dim()),
            ],
            86,
            17,
        ),
        Modal::ConfirmQuit => (
            "Quit?".into(),
            vec![
                Line::raw("A scan is running. Quitting stops it; it resumes where it left off"),
                Line::raw("the next time the drive is scanned."),
                Line::raw(""),
                Line::styled("y quit   any other key: stay", bold()),
            ],
            72,
            7,
        ),
        Modal::Label { drive_id, input } => (
            format!("Label D{drive_id}"),
            vec![Line::raw(format!("> {}", input.text())), Line::raw(""), Line::styled("⏎ save (empty clears)   esc cancel", dim())],
            60,
            5,
        ),
        Modal::Notes { drive_id, input } => (
            format!("Notes for D{drive_id}"),
            vec![Line::raw(format!("> {}", input.text())), Line::raw(""), Line::styled("⏎ save   esc cancel", dim())],
            70,
            5,
        ),
        Modal::NewDrive { desc, input, .. } => (
            "New drive detected".into(),
            vec![
                Line::raw(desc.clone()),
                Line::raw(""),
                Line::raw(format!("Label: {}", input.text())),
                Line::raw(""),
                Line::styled("⏎ scan it (with this label)   esc not now", bold()),
            ],
            70,
            8,
        ),
        Modal::Ambiguous { desc, candidates, sel, .. } => {
            let mut lines = vec![Line::raw(desc.clone()), Line::raw("matches more than one known drive. Which is it?"), Line::raw("")];
            for (i, (id, label)) in candidates.iter().enumerate() {
                let text = format!("  D{id} {label}");
                lines.push(if i == *sel { Line::styled(text, selected()) } else { Line::raw(text) });
            }
            let new = "  a new drive";
            lines.push(if *sel == candidates.len() { Line::styled(new, selected()) } else { Line::raw(new) });
            lines.push(Line::raw(""));
            lines.push(Line::styled("↑↓ choose   ⏎ scan as that drive   esc not now", dim()));
            ("Which drive is this?".into(), lines, 70, candidates.len() as u16 + 9)
        }
        Modal::File(d) => {
            let opt = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
            let lines = vec![
                kv("Drive", format!("D{} {}", d.drive_id, d.drive_label.as_deref().unwrap_or(""))),
                kv("FS", format!("{} ({})", d.fs_label.as_deref().unwrap_or("-"), d.fs_type)),
                kv("Path", d.path.clone()),
                kv("Kind", d.kind.clone()),
                kv("Size", format!("{} ({} bytes), {} allocated", format_size(d.size), format_count(d.size), d.alloc_size.map(format_size).unwrap_or_else(|| "-".into()))),
                kv("Modified", local_ns(d.mtime_ns)),
                kv("Created", local_ns(d.btime_ns)),
                kv("Changed", local_ns(d.ctime_ns)),
                kv("Mode", d.mode.map(|m| format!("{:o}", m)).unwrap_or_else(|| "-".into())),
                kv("Owner", format!("uid {} gid {}", opt(d.uid), opt(d.gid))),
                kv("Inode", format!("{} ({} links)", opt(d.inode), opt(d.nlink))),
                kv("Flags", if d.flags.is_empty() { "-".into() } else { d.flags.join(", ") }),
                kv("Link", d.link_target.clone().unwrap_or_else(|| "-".into())),
                kv("Scanned", local_time(d.scanned_at)),
                kv("Mounted", d.mount_point.clone().unwrap_or_else(|| "-".into())),
                kv("Hash", d.quick_hash.as_ref().map(|h| format!("{h} (quick)")).unwrap_or_else(|| "-".into())),
            ];
            let mut lines = lines;
            if let Some(m) = &d.media {
                lines.push(Line::raw(""));
                for (k, v) in m.lines() {
                    lines.push(kv(k, v));
                }
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled("c copy path   esc close", dim()));
            let h = lines.len() as u16 + 2;
            ("File".into(), lines, 100, h)
        }
    };
    let r = centered(area, w, h);
    f.render_widget(Clear, r);
    app.hits.modal = Some(r);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block(title)), r);
    if let Modal::Label { input, .. } | Modal::Notes { input, .. } = modal {
        f.set_cursor_position((r.x + 3 + input.cursor() as u16, r.y + 1));
    }
    if let Modal::NewDrive { input, .. } = modal {
        f.set_cursor_position((r.x + 8 + input.cursor() as u16, r.y + 3));
    }
}
