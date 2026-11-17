//! Drawing. Pure functions of `App`; no state changes here except
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
use crate::assist;

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
    match di_core::now_secs() - s {
        d if d < 120 => "now".into(),
        d => di_model::units::format_age(d),
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
    app.hits.examples.clear();
    app.hits.filter_rows.clear();
    draw_tabs(f, app, top);
    match app.screen {
        Screen::Drives => draw_drives(f, app, body),
        Screen::Search => draw_search(f, app, body),
        Screen::Browse => draw_browse(f, app, body),
        Screen::Dupes => draw_dupes(f, app, body),
        Screen::Collections => draw_collections(f, app, body),
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
        Screen::Drives => "s scan  v verify dupes  p pause/resume  x stop  u unmount  e eject  l label  n notes  b browse",
        Screen::Search if app.search.focused => "type a query  Tab complete  ⏎/↓ results  F1 all filters  esc leave the box",
        Screen::Search => "⏎ details  s sort  d reverse  c copy path  b browse here  i/↑ edit query  F1 filters",
        Screen::Browse => "⏎/→ open  ←/⌫ up  s sort",
        Screen::Setup => "r re-check  c copy install command  H hand off a catalog copy",
        Screen::Dupes if app.dupes.editing.is_some() => "type a search query to limit the files  ⏎ apply  esc cancel",
        Screen::Dupes if app.dupes.focus_members => "⏎ details  b browse  ←/esc back to groups",
        Screen::Dupes => "c confidence  m min size  x same drive only  f scope  →/⏎ copies  b browse  r reload",
        Screen::Collections => "←/→ collection  ⏎ files on drive  o only on drive  a at-risk files (1 copy)  r reload",
        _ => "",
    };
    f.render_widget(
        Line::from(vec![Span::styled(format!(" {keys}"), dim()), Span::styled("   1-7 screens  / search  ? help  q quit", dim())]),
        area,
    );
}

// -------------------------------------------------------------------------------------
// Drives
// -------------------------------------------------------------------------------------

fn status_glyph(s: &Option<ConnStatus>) -> Span<'static> {
    match s {
        None => Span::raw("  "),
        Some(ConnStatus::Scanning) | Some(ConnStatus::Queued) => Span::styled("⟳ ", fg(Color::Cyan)),
        Some(ConnStatus::NeedsAnswer) => Span::styled("? ", fg(Color::Yellow)),
        Some(ConnStatus::NothingToScan(_)) => Span::styled("! ", fg(Color::Yellow)),
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
            match (&d.smart, &d.smart_reason) {
                (Some(sv), _) => {
                    let style = match sv.report.passed {
                        Some(false) => fg(Color::Red).add_modifier(Modifier::BOLD),
                        _ if !sv.report.warnings().is_empty() => fg(Color::Yellow),
                        Some(true) => fg(Color::Green),
                        None => Style::new(),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<10}", "SMART"), dim()),
                        Span::styled(sv.report.summary(), style),
                        Span::styled(format!("  ({})", ago(Some(sv.taken_at))), dim()),
                    ]));
                    for w in sv.report.warnings() {
                        lines.push(Line::from(vec![Span::raw(" ".repeat(10)), Span::styled(format!("⚠ {w}"), fg(Color::Yellow))]));
                    }
                }
                (None, Some(reason)) => lines.push(Line::from(vec![Span::styled(format!("{:<10}", "SMART"), dim()), Span::styled(format!("unavailable: {reason}"), dim())])),
                (None, None) => lines.push(kv("SMART", "no reading yet (taken when the drive is scanned)")),
            }
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
        ConnStatus::NothingToScan(why) => format!("nothing to scan: {why}"),
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
    let focused = app.search.focused;
    let box_h = if focused { 6 } else { 4 };
    let [input_area, results_area] = Layout::vertical([Constraint::Length(box_h), Constraint::Min(5)]).areas(area);
    let inner_w = input_area.width.saturating_sub(2) as usize;
    let s = &app.search;
    let q = s.input.text();

    // The query, with the term an error points at highlighted.
    let mut query_line = vec![Span::styled("> ", bold())];
    match s.error.as_ref().filter(|e| e.start <= e.end && q.is_char_boundary(e.start.min(q.len())) && q.is_char_boundary(e.end.min(q.len()))) {
        Some(e) => {
            let (a, b) = (e.start.min(q.len()), e.end.min(q.len()));
            query_line.push(Span::raw(q[..a].to_string()));
            query_line.push(Span::styled(q[a..b].to_string(), fg(Color::Red).add_modifier(Modifier::UNDERLINED)));
            query_line.push(Span::raw(q[b..].to_string()));
        }
        None => query_line.push(Span::raw(q.to_string())),
    }
    let mut lines = vec![Line::from(query_line)];

    if focused {
        let ctx = assist::context(q, s.input.cursor());
        let hint = assist::hint(&ctx, q.trim().is_empty());
        let head_style = if hint.warning { fg(Color::Yellow).add_modifier(Modifier::BOLD) } else { fg(Color::Cyan).add_modifier(Modifier::BOLD) };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(hint.head.clone(), head_style),
            Span::raw("  "),
            Span::styled(hint.text.clone(), dim()),
        ]));
        // Completions (the cycle being stepped through, or what Tab would offer), else examples.
        let (items, current) = match app.active_cycle() {
            Some(c) => (c.items.clone(), Some(c.idx)),
            None => (assist::candidates(&ctx, &app.vocab), None),
        };
        let mut l = vec![Span::raw("  ")];
        if !items.is_empty() {
            l.push(Span::styled("Tab ▸ ", dim()));
            let mut used = 8;
            for (i, c) in items.iter().enumerate() {
                let w = c.display.chars().count() + 2;
                if used + w + 12 > inner_w && i > 0 {
                    l.push(Span::styled(format!("… +{} more", items.len() - i), dim()));
                    break;
                }
                used += w;
                let style = if Some(i) == current { selected() } else { fg(Color::Cyan) };
                l.push(Span::styled(c.display.clone(), style));
                l.push(Span::raw("  "));
            }
        } else if !hint.examples.is_empty() {
            l.push(Span::styled("e.g. ", dim()));
            for e in &hint.examples {
                l.push(Span::raw(e.to_string()));
                l.push(Span::raw("   "));
            }
        }
        lines.push(Line::from(l));
    }

    if let Some(e) = &s.error {
        lines.push(Line::styled(format!("  ✖ {}", e.message), fg(Color::Red)));
    } else if let Some(m) = &s.message {
        lines.push(Line::styled(format!("  {m}"), dim()));
    } else if let Some(sum) = s.summary.as_ref().filter(|_| !q.trim().is_empty()) {
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
    } else if !focused {
        lines.push(Line::styled("  / or i to type a query · F1 lists every filter", dim()));
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
    let b = block(title).border_style(if focused { fg(Color::Cyan) } else { Style::new() });
    f.render_widget(Paragraph::new(lines).block(b), input_area);
    app.hits.search_input = Some(input_area);
    let s = &app.search;
    if focused {
        let x = input_area.x + 3 + s.input.cursor() as u16;
        f.set_cursor_position((x.min(input_area.right().saturating_sub(2)), input_area.y + 1));
    }
    if s.input.text().trim().is_empty() && s.hits.is_empty() {
        draw_cheatsheet(f, app, results_area);
        return;
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

/// Filter groups in reference order.
fn filter_groups() -> Vec<&'static str> {
    let mut groups: Vec<&'static str> = Vec::new();
    for d in di_core::filters::FILTERS {
        if !groups.contains(&d.group) {
            groups.push(d.group);
        }
    }
    groups
}

/// `label` then `items` separated by two spaces, wrapped to `width` with the
/// continuation lines indented under the first item.
fn labeled_wrap(label: &str, items: &[String], label_w: usize, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut cur: Vec<Span> = vec![Span::styled(format!("  {label:<label_w$}"), dim())];
    let mut used = label_w + 2;
    for (i, it) in items.iter().enumerate() {
        let w = it.chars().count() + 2;
        if i > 0 && used + w > width {
            out.push(Line::from(std::mem::take(&mut cur)));
            cur.push(Span::raw(" ".repeat(label_w + 2)));
            used = label_w + 2;
        }
        cur.push(Span::raw(format!("{it}  ")));
        used += w;
    }
    out.push(Line::from(cur));
    out
}

/// Shown instead of results while the query is empty.
fn draw_cheatsheet(f: &mut Frame, app: &mut App, area: Rect) {
    use di_core::filters::{EXAMPLES, FILTERS, TIPS};
    let label_w = 16;
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = vec![Line::styled("Examples (click one to run it)", bold())];
    let example_top = area.y + 2;
    for (i, (q, what)) in EXAMPLES.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{q:<38}"), fg(Color::Cyan)),
            Span::styled(what.to_string(), dim()),
        ]));
        app.hits.examples.push((example_top + i as u16, q));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("Filters", bold()),
        Span::styled("   Tab completes keys and values · F1 shows each with examples", dim()),
    ]));
    for g in filter_groups() {
        let keys: Vec<String> =
            FILTERS.iter().filter(|d| d.group == g).map(|d| if d.key.is_empty() { "word".into() } else { format!("{}:", d.key) }).collect();
        lines.extend(labeled_wrap(g, &keys, label_w, width));
    }
    let v = &app.vocab;
    if !v.collections.is_empty() || !v.categories.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("In this catalog", bold()));
        if !v.collections.is_empty() {
            let c: Vec<String> = v.collections.iter().map(|(s, _)| format!("in:{s}")).collect();
            lines.extend(labeled_wrap("Collections", &c, label_w, width));
        }
        if !v.categories.is_empty() {
            let c: Vec<String> = v.categories.iter().map(|s| format!("type:{s}")).collect();
            lines.extend(labeled_wrap("Categories", &c, label_w, width));
        }
        if !v.drives.is_empty() {
            let c: Vec<String> = v
                .drives
                .iter()
                .take(12)
                .map(|(id, l)| format!("drive:D{id}{}", l.as_deref().map(|l| format!(" ({l})")).unwrap_or_default()))
                .collect();
            lines.extend(labeled_wrap("Drives", &c, label_w, width));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Tips", bold()));
    for t in TIPS {
        lines.push(Line::styled(format!("  {t}"), dim()));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block("Search help")), area);
}

/// The full filter reference (F1): a table, and the selected filter in full below it.
fn draw_filters_modal(f: &mut Frame, app: &mut App, sel: usize) {
    use di_core::filters::FILTERS;
    let area = f.area();
    let r = centered(area, 120, area.height.saturating_sub(2));
    f.render_widget(Clear, r);
    app.hits.modal = Some(r);
    let sel = sel.min(FILTERS.len() - 1);
    let [table_area, detail_area] = Layout::vertical([Constraint::Min(5), Constraint::Length(6)]).areas(r);

    let mut rows: Vec<Row> = Vec::new();
    let mut row_of: Vec<usize> = vec![0; FILTERS.len()];
    let mut filter_at: Vec<Option<usize>> = Vec::new();
    for g in filter_groups() {
        rows.push(Row::new(vec![Cell::from(Span::styled(g, bold()))]));
        filter_at.push(None);
        for (i, d) in FILTERS.iter().enumerate().filter(|(_, d)| d.group == g) {
            row_of[i] = rows.len();
            filter_at.push(Some(i));
            let name = if d.key.is_empty() { "word".to_string() } else { format!("{}:", d.key) };
            rows.push(Row::new(vec![Cell::from(format!("  {name}")).style(fg(Color::Cyan)), Cell::from(d.help)]));
        }
    }
    let title = "Search filters — ↑↓ or a letter to move · ⏎ adds it to the query · esc closes";
    let table = Table::new(rows, [Constraint::Length(14), Constraint::Min(30)]).row_highlight_style(selected()).block(block(title));
    let mut state = TableState::default().with_selected(Some(row_of[sel]));
    f.render_stateful_widget(table, table_area, &mut state);
    let body = rows_area(table_area, false);
    for y in 0..body.height {
        if let Some(Some(i)) = filter_at.get(state.offset() + y as usize) {
            app.hits.filter_rows.push((body.y + y, *i));
        }
    }

    let d = &FILTERS[sel];
    let detail = vec![
        Line::from(vec![Span::styled(d.syntax, bold()), Span::raw("   "), Span::styled(d.help, dim())]),
        Line::from(vec![
            Span::styled("e.g.  ", dim()),
            Span::styled(d.examples.join("    "), fg(Color::Cyan)),
        ]),
        Line::styled(di_core::filters::TIPS.join("  "), dim()),
    ];
    f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: true }).block(block(if d.key.is_empty() { "word".to_string() } else { format!("{}:", d.key) })), detail_area);
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
// Duplicates
// -------------------------------------------------------------------------------------

fn confidence_text(c: di_core::dupes::Confidence) -> &'static str {
    use di_core::dupes::Confidence;
    match c {
        Confidence::Exact => "exact: same full BLAKE3 hash (verified)",
        Confidence::Likely => "likely: same size and quick hash (first and last 64 KiB)",
        Confidence::Probable => "probable: same name and size, content unknown",
    }
}

fn drive_list(ids: &[i64]) -> String {
    let mut v: Vec<String> = ids.iter().take(6).map(|d| format!("D{d}")).collect();
    if ids.len() > 6 {
        v.push(format!("+{}", ids.len() - 6));
    }
    v.join(" ")
}

fn draw_dupes(f: &mut Frame, app: &mut App, area: Rect) {
    use di_core::dupes::Confidence;
    let [head_area, groups_area, members_area] =
        Layout::vertical([Constraint::Length(5), Constraint::Min(6), Constraint::Percentage(35)]).areas(area);
    let d = &app.dupes;

    // Settings and totals.
    let mut levels: Vec<Span> = vec![Span::styled("c ", dim())];
    for c in [Confidence::Exact, Confidence::Likely, Confidence::Probable] {
        let style = if c == d.confidence { selected().add_modifier(Modifier::BOLD) } else { dim() };
        levels.push(Span::styled(format!(" {} ", c.as_str()), style));
        levels.push(Span::raw(" "));
    }
    levels.push(Span::styled(format!("  {}", confidence_text(d.confidence)), dim()));
    let scope = match &d.editing {
        Some(e) => Line::from(vec![Span::styled("f scope: ", dim()), Span::raw(e.text().to_string())]),
        None => Line::from(vec![
            Span::styled("m ", dim()),
            Span::raw(format!("≥ {}", if d.min_size == 0 { "any size".into() } else { format_size(d.min_size) })),
            Span::styled("   x ", dim()),
            Span::raw(if d.same_drive_only { "copies on one drive only" } else { "all copies" }),
            Span::styled("   f ", dim()),
            Span::raw(if d.filter.is_empty() { "all files".to_string() } else { d.filter.clone() }),
        ]),
    };
    let totals = if let Some(e) = &d.error {
        Line::styled(format!("✖ {e}"), fg(Color::Red))
    } else {
        match (&d.summary, d.running) {
            (_, true) => Line::styled("Looking for duplicates…", dim()),
            (Some(s), _) => Line::from(vec![
                Span::styled(format!("{} groups", format_count(s.groups)), bold()),
                Span::raw(format!(" · {} files · ", format_count(s.files))),
                Span::styled(format_size(s.wasted), fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(" in extra copies"),
                Span::styled(
                    if (s.groups as usize) > d.groups.len() { format!("  (largest {} shown)", format_count(d.groups.len() as u64)) } else { String::new() },
                    dim(),
                ),
            ]),
            (None, _) => Line::raw(""),
        }
    };
    let b = block("Duplicates").border_style(if d.editing.is_some() { fg(Color::Cyan) } else { Style::new() });
    f.render_widget(Paragraph::new(vec![Line::from(levels), scope, totals]).block(b), head_area);
    if let Some(e) = &d.editing {
        f.set_cursor_position((head_area.x + 10 + e.cursor() as u16, head_area.y + 2));
    }

    // Groups.
    if d.groups.is_empty() && !d.running && d.error.is_none() {
        let text = match d.confidence {
            Confidence::Exact => "No verified duplicates yet. Exact duplicates need full hashes: connect a drive and press v on \
                                  the Drives screen (or run `drive-index scan --verify`). Press c for likely duplicates.",
            Confidence::Likely => "No duplicates by quick hash. Quick hashes are made in the content phase for files of 1 MiB or more \
                                   whose size matches another file. Press m for a lower minimum size, or c for probable duplicates.",
            Confidence::Probable => "No files share a name and size at this minimum size. Press m to change it.",
        };
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block("Groups")), groups_area);
        f.render_widget(Paragraph::new("").block(block("Copies")), members_area);
        return;
    }
    let rows: Vec<Row> = d
        .groups
        .iter()
        .map(|g| {
            Row::new(vec![
                Cell::from(Line::from(format_size(g.size)).alignment(Alignment::Right)),
                Cell::from(Line::from(g.copies.to_string()).alignment(Alignment::Right)),
                Cell::from(Line::from(format_size(g.wasted)).alignment(Alignment::Right)),
                Cell::from(drive_list(&g.drives)),
                Cell::from(g.name.clone()),
            ])
        })
        .collect();
    let n = rows.len();
    let table = Table::new(rows, [Constraint::Length(10), Constraint::Length(7), Constraint::Length(10), Constraint::Length(22), Constraint::Min(20)])
        .header(Row::new(vec![
            Cell::from(Line::from("Size").alignment(Alignment::Right)),
            Cell::from(Line::from("Copies").alignment(Alignment::Right)),
            Cell::from(Line::from("Extra").alignment(Alignment::Right)),
            Cell::from("Drives"),
            Cell::from("Name"),
        ]).style(dim()))
        .row_highlight_style(if d.focus_members { Style::new().add_modifier(Modifier::BOLD) } else { selected() })
        .block(block(format!("Groups ({})", format_count(n as u64))));
    let mut state = TableState::default().with_selected((n > 0).then_some(d.sel));
    f.render_stateful_widget(table, groups_area, &mut state);
    app.hits.list = Some(ListHit { kind: ListKind::DupeGroups, area: rows_area(groups_area, true), offset: state.offset() });

    // Members of the selected group.
    let d = &app.dupes;
    let rows: Vec<Row> = d
        .members
        .iter()
        .map(|m| {
            Row::new(vec![
                Cell::from(format!("D{} {}", m.drive_id, clip(m.drive_label.as_deref().unwrap_or(""), 14))),
                Cell::from(m.path.clone()),
                Cell::from(local_ns(m.mtime_ns)),
            ])
        })
        .collect();
    let title = match d.groups.get(d.sel) {
        Some(g) => format!("Copies of {} ({} each)", clip(&g.name, 60), format_size(g.size)),
        None => "Copies".into(),
    };
    let m = rows.len();
    let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(20), Constraint::Length(16)])
        .row_highlight_style(if d.focus_members { selected() } else { Style::new() })
        .block(block(title).border_style(if d.focus_members { fg(Color::Cyan) } else { Style::new() }));
    let mut state = TableState::default().with_selected((m > 0).then_some(d.member_sel));
    f.render_stateful_widget(table, members_area, &mut state);
    if d.focus_members || app.hits.list.is_none() {
        app.hits.list = Some(ListHit { kind: ListKind::DupeMembers, area: rows_area(members_area, false), offset: state.offset() });
    }
}

// -------------------------------------------------------------------------------------
// Collections
// -------------------------------------------------------------------------------------

fn bar(frac: f64, width: usize) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let eighths = (frac * width as f64 * 8.0).round() as usize;
    let mut s = "█".repeat(eighths / 8);
    if eighths % 8 > 0 {
        s.push(['▏', '▎', '▍', '▌', '▋', '▊', '▉'][eighths % 8 - 1]);
    }
    let used = s.chars().count();
    s.push_str(&"·".repeat(width.saturating_sub(used)));
    s
}

fn draw_collections(f: &mut Frame, app: &mut App, area: Rect) {
    let c = &app.collections;
    let [tabs_area, summary_area, drives_area, years_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(6), Constraint::Min(5), Constraint::Length(8)]).areas(area);

    let mut tabs: Vec<Span> = vec![Span::styled(" ← ", dim())];
    for (i, col) in c.list.iter().enumerate() {
        let style = if i == c.sel { selected().add_modifier(Modifier::BOLD) } else { Style::new() };
        tabs.push(Span::styled(format!(" {} ", col.name), style));
        tabs.push(Span::raw(" "));
    }
    tabs.push(Span::styled(" →", dim()));
    f.render_widget(Line::from(tabs), tabs_area);

    let Some(cov) = c.coverage.as_ref().filter(|v| c.list.get(c.sel).is_some_and(|l| l.slug == v.collection.slug)) else {
        let text = match (&c.error, c.running, c.list.is_empty()) {
            (Some(e), _, _) => format!("✖ {e}"),
            (None, true, _) => "Working out coverage…".into(),
            (None, false, true) => "No collections in the catalog.".into(),
            _ => String::new(),
        };
        f.render_widget(Paragraph::new(text).block(block("Coverage")), summary_area);
        return;
    };

    let total = cov.items.max(1) as f64;
    let hashed_pct = 100.0 * cov.hashed_items as f64 / total;
    let labels = ["1 copy", "2 copies", "3+ copies"];
    let styles = [fg(Color::Red), fg(Color::Yellow), fg(Color::Green)];
    let bar_w = (summary_area.width as usize).saturating_sub(60).clamp(10, 40);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{} items", format_count(cov.items)), bold()),
        Span::raw(format!(" · {} · ", format_size(cov.bytes))),
        Span::styled(
            format!(
                "{}{}",
                if cov.hashed_items >= cov.items {
                    "all identified by content hash".to_string()
                } else {
                    format!("{hashed_pct:.0}% identified by content hash, the rest by name and size")
                },
                if c.running { "  (updating…)" } else { "" }
            ),
            dim(),
        ),
    ])];
    for i in 0..3 {
        let n = cov.redundancy[i];
        let mut l = vec![
            Span::raw(format!("{:<10}", labels[i])),
            Span::styled(bar(n as f64 / total, bar_w), styles[i]),
            Span::raw(format!(" {:>9} ({:>3.0}%)", format_count(n), 100.0 * n as f64 / total)),
        ];
        if i == 0 && n > 0 {
            l.push(Span::styled(format!("  {} on a single drive (a lists them)", format_size(cov.only_bytes)), fg(Color::Red)));
        }
        lines.push(Line::from(l));
    }
    let title = format!("{} — in:{} ({})", cov.collection.name, cov.collection.slug, cov.collection.query);
    f.render_widget(Paragraph::new(lines).block(block(title)), summary_area);

    // Per drive.
    let share_w = 20;
    let rows: Vec<Row> = cov
        .drives
        .iter()
        .map(|d| {
            let smart = match d.smart_passed {
                Some(true) => Span::styled("ok", fg(Color::Green)),
                Some(false) => Span::styled("FAILING", fg(Color::Red).add_modifier(Modifier::BOLD)),
                None => Span::styled("-", dim()),
            };
            Row::new(vec![
                Cell::from(format!("D{} {}", d.drive_id, clip(d.label.as_deref().unwrap_or(""), 16))),
                Cell::from(Line::from(format_count(d.items)).alignment(Alignment::Right)),
                Cell::from(Line::from(vec![Span::styled(bar(d.pct / 100.0, share_w), fg(Color::Cyan)), Span::raw(format!(" {:>3.0}%", d.pct))])),
                Cell::from(Line::from(format_size(d.bytes)).alignment(Alignment::Right)),
                Cell::from(Line::from(if d.only_items > 0 { format!("{} ({})", format_count(d.only_items), format_size(d.only_bytes)) } else { "-".into() }).alignment(Alignment::Right)),
                Cell::from(Line::from(ago(Some(d.last_seen_at))).alignment(Alignment::Right)),
                Cell::from(smart),
            ])
        })
        .collect();
    let n = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(20),
            Constraint::Length(10),
            Constraint::Length(share_w as u16 + 6),
            Constraint::Length(10),
            Constraint::Length(22),
            Constraint::Length(9),
            Constraint::Length(8),
        ],
    )
    .header(Row::new(vec![
        Cell::from("Drive"),
        Cell::from(Line::from("Items").alignment(Alignment::Right)),
        Cell::from("Share of the collection"),
        Cell::from(Line::from("Size").alignment(Alignment::Right)),
        Cell::from(Line::from("Only on this drive").alignment(Alignment::Right)),
        Cell::from(Line::from("Seen").alignment(Alignment::Right)),
        Cell::from("SMART"),
    ]).style(dim()))
    .row_highlight_style(selected())
    .block(block(format!("Drives ({n})")));
    let mut state = TableState::default().with_selected((n > 0).then_some(c.drive_sel));
    f.render_stateful_widget(table, drives_area, &mut state);
    app.hits.list = Some(ListHit { kind: ListKind::CoverageDrives, area: rows_area(drives_area, true), offset: state.offset() });

    // Items per year (capture year when known).
    let cov = app.collections.coverage.as_ref().unwrap();
    let inner = years_area.width.saturating_sub(2) as usize;
    let years: Vec<&(i32, u64)> = cov.years.iter().rev().take((inner / 5).max(1)).collect::<Vec<_>>().into_iter().rev().collect();
    let max = years.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1) as f64;
    let rows_h = years_area.height.saturating_sub(3) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for level in (1..=rows_h).rev() {
        let mut l = String::new();
        for (_, n) in &years {
            let h = *n as f64 / max * rows_h as f64;
            let cell = if h >= level as f64 { "███" } else if h > (level - 1) as f64 && h > 0.0 { "▄▄▄" } else { "   " };
            l.push_str(&format!(" {cell} "));
        }
        lines.push(Line::styled(l, fg(Color::Cyan)));
    }
    lines.push(Line::styled(years.iter().map(|(y, _)| format!(" {:>3} ", format!("'{:02}", y.rem_euclid(100)))).collect::<String>(), dim()));
    let span = match (years.first(), years.last()) {
        (Some((a, _)), Some((b, _))) => format!("Items per year ({a}–{b}; capture date, else modified)"),
        _ => "Items per year".into(),
    };
    f.render_widget(Paragraph::new(lines).block(block(span)), years_area);
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
    if let Some(Modal::Filters { sel }) = app.modal {
        return draw_filters_modal(f, app, sel);
    }
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
                Line::raw("  v rescan and read likely duplicates in full, making them exact (verified)"),
                Line::styled("Duplicates and Collections", bold()),
                Line::raw("  c confidence  m min size  x same drive  f scope · ←/→ collection  a at-risk files"),
                Line::styled("Search", bold()),
                Line::raw("  type a query: ext:cr2,crw  date:2014-06..2014-09  size:>10M  drive:D3  camera:canon"),
                Line::raw("  -term negates; commas mean OR; Tab completes filter names and values"),
                Line::raw("  F1 lists every filter with examples (⏎ there adds one to the query)"),
                Line::raw("  ⏎ details   s sort   d reverse   c copy path   b browse to the file"),
                Line::styled("Browse", bold()),
                Line::raw("  ⏎/→ open   ←/⌫ up   s sort by size/count/name/modified"),
                Line::raw(""),
                Line::styled("Esc closes this help.", dim()),
            ],
            86,
            21,
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
        Modal::Filters { .. } => unreachable!("drawn by draw_filters_modal"),
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
