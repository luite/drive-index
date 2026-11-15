//! Application state and behaviour (everything except drawing).

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use di_core::Catalog;
use di_core::catalog::DriveSummary;
use di_core::query::QueryError;
use di_core::search::{Hit, SortKey, Summary};
use di_core::views::{self, BrowseEntry, BrowseSort, DirInfo, DriveDetail, FileDetail, FsChoice};
use di_doctor::Report;
use di_platform::DriveProbe;
use di_scan::controller::{Command, Controller, Event, Level, Outcome, Question};
use di_scan::device::{IdentityChoice, report_lines};

use crate::assist::{self, Candidate, Vocab};
use crate::input::LineEdit;
use crate::screens::{CollectionsState, DupesState};
use crate::search_worker::{self, SearchWorker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Drives,
    Search,
    Browse,
    Dupes,
    Collections,
    Log,
    Setup,
}

impl Screen {
    pub const ALL: [Screen; 7] =
        [Screen::Drives, Screen::Search, Screen::Browse, Screen::Dupes, Screen::Collections, Screen::Log, Screen::Setup];

    pub fn title(self) -> &'static str {
        match self {
            Screen::Drives => "Drives",
            Screen::Search => "Search",
            Screen::Browse => "Browse",
            Screen::Dupes => "Dupes",
            Screen::Collections => "Collections",
            Screen::Log => "Log",
            Screen::Setup => "Setup",
        }
    }

    fn index(self) -> usize {
        Screen::ALL.iter().position(|s| *s == self).unwrap()
    }
}

/// A connected drive, as reported by the controller.
#[derive(Debug, Clone)]
pub struct Connected {
    pub probe: DriveProbe,
    pub drive_id: Option<i64>,
    pub status: ConnStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnStatus {
    Idle,
    Queued,
    Scanning,
    UpToDate,
    NeedsAnswer,
    Done(String),
    Failed(String),
}

/// One row of the Drives list.
#[derive(Debug, Clone)]
pub struct DriveRow {
    pub drive_id: Option<i64>,
    /// Connected disk (`disk4`, `sdb`).
    pub disk: Option<String>,
    pub label: String,
    pub capacity: u64,
    pub last_seen_at: Option<i64>,
    pub status: Option<ConnStatus>,
}

#[derive(Debug)]
pub enum Modal {
    Help,
    Label { drive_id: i64, input: LineEdit },
    Notes { drive_id: i64, input: LineEdit },
    NewDrive { disk: String, desc: String, input: LineEdit },
    Ambiguous { disk: String, desc: String, candidates: Vec<(i64, String)>, sel: usize },
    ConfirmQuit,
    File(Box<FileDetail>),
    /// The search filter reference; `sel` indexes `di_core::filters::FILTERS`.
    Filters { sel: usize },
}

/// Where things were drawn in the last frame, for mapping mouse clicks.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    pub tabs_y: u16,
    /// (x start, x end exclusive, screen)
    pub tabs: Vec<(u16, u16, Screen)>,
    pub list: Option<ListHit>,
    pub search_input: Option<Rect>,
    pub modal: Option<Rect>,
    /// Example queries on the search cheat sheet: (row, query).
    pub examples: Vec<(u16, &'static str)>,
    /// Filter reference rows: (row, index into FILTERS).
    pub filter_rows: Vec<(u16, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Drives,
    Results,
    BrowseFs,
    BrowseEntries,
    DupeGroups,
    DupeMembers,
    CoverageDrives,
}

/// A table's data rows on screen.
#[derive(Debug, Clone, Copy)]
pub struct ListHit {
    pub kind: ListKind,
    pub area: Rect,
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: i64,
    pub level: Level,
    pub text: String,
}

/// Tab cycling through several completions.
pub struct Cycle {
    pub start: usize,
    pub items: Vec<Candidate>,
    pub idx: usize,
    /// The query right after the last insertion; any other edit ends the cycle.
    pub after: String,
}

pub struct SearchState {
    pub input: LineEdit,
    pub cycle: Option<Cycle>,
    pub focused: bool,
    pub hits: Vec<Hit>,
    pub summary: Option<Summary>,
    pub error: Option<QueryError>,
    pub message: Option<String>,
    pub sel: usize,
    pub sort: SortKey,
    pub descending: bool,
    pub limit: usize,
    pub dirty_at: Option<Instant>,
    pub generation: u64,
    pub running: bool,
}

pub struct BrowseState {
    pub filesystems: Vec<FsChoice>,
    pub fs_sel: usize,
    pub dir: Option<DirInfo>,
    pub entries: Vec<BrowseEntry>,
    pub sel: usize,
    pub sort: BrowseSort,
}

pub struct SetupState {
    pub report: Option<Report>,
    pub running: bool,
    rx: Option<std::sync::mpsc::Receiver<Report>>,
}

/// Rate estimate for the jobs panel.
#[derive(Default)]
pub struct Rate {
    last: Option<(Instant, u64)>,
    pub files_per_sec: f64,
}

pub struct App {
    pub screen: Screen,
    pub catalog_path: PathBuf,
    pub host: String,
    /// Viewer mode: why this instance does not control drives.
    pub viewer: Option<String>,
    pub ctl: Option<Controller>,

    pub drives: Vec<DriveRow>,
    pub catalog_drives: Vec<DriveSummary>,
    pub connected: BTreeMap<String, Connected>,
    pub drive_sel: usize,
    pub detail: Option<DriveDetail>,
    pub job_lines: VecDeque<String>,
    pub rate: Rate,

    pub search: SearchState,
    worker: SearchWorker,
    /// Catalog values for completing filter values.
    pub vocab: Vocab,
    vocab_rx: Option<std::sync::mpsc::Receiver<Vocab>>,
    pub browse: BrowseState,
    pub dupes: DupesState,
    pub collections: CollectionsState,
    pub log: VecDeque<LogLine>,
    pub setup: SetupState,
    pub modal: Option<Modal>,
    pub toast: Option<(String, Instant)>,
    pub quit: bool,
    pub hits: Hits,
    last_click: Option<(Instant, ListKind, usize)>,
    last_refresh: Instant,
}

const LOG_LINES: usize = 500;

impl App {
    pub fn new(catalog_path: PathBuf, host: String, ctl: Option<Controller>, viewer: Option<String>) -> App {
        let worker = SearchWorker::start(catalog_path.clone());
        let mut app = App {
            screen: Screen::Drives,
            catalog_path,
            host,
            viewer,
            ctl,
            drives: Vec::new(),
            catalog_drives: Vec::new(),
            connected: BTreeMap::new(),
            drive_sel: 0,
            detail: None,
            job_lines: VecDeque::new(),
            rate: Rate::default(),
            search: SearchState {
                input: LineEdit::default(),
                cycle: None,
                focused: false,
                hits: Vec::new(),
                summary: None,
                error: None,
                message: None,
                sel: 0,
                sort: SortKey::Path,
                descending: false,
                limit: 500,
                dirty_at: None,
                generation: 0,
                running: false,
            },
            worker,
            vocab: Vocab::default(),
            vocab_rx: None,
            browse: BrowseState {
                filesystems: Vec::new(),
                fs_sel: 0,
                dir: None,
                entries: Vec::new(),
                sel: 0,
                sort: BrowseSort::Size,
            },
            dupes: DupesState::default(),
            collections: CollectionsState::default(),
            log: VecDeque::new(),
            setup: SetupState { report: None, running: false, rx: None },
            modal: None,
            toast: None,
            quit: false,
            hits: Hits::default(),
            last_click: None,
            last_refresh: Instant::now() - Duration::from_secs(60),
        };
        app.load_log();
        app.refresh();
        app.run_doctor();
        app.load_vocab();
        app
    }

    /// Reload completion values in the background (after start and after scans).
    fn load_vocab(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = self.catalog_path.clone();
        std::thread::spawn(move || {
            if let Ok(c) = Catalog::open_readonly(&path) {
                let _ = tx.send(Vocab::load(c.conn()));
            }
        });
        self.vocab_rx = Some(rx);
    }

    pub(crate) fn catalog(&self) -> Option<Catalog> {
        Catalog::open_readonly(&self.catalog_path).ok()
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn push_log(&mut self, level: Level, text: impl Into<String>) {
        self.log.push_front(LogLine { at: di_core::now_secs(), level, text: text.into() });
        self.log.truncate(LOG_LINES);
    }

    fn load_log(&mut self) {
        if let Some(cat) = self.catalog() {
            if let Ok(events) = cat.recent_events(200) {
                for e in events.into_iter().rev() {
                    let level = match e.level.as_str() {
                        "error" => Level::Error,
                        "warn" => Level::Warn,
                        _ => Level::Info,
                    };
                    self.log.push_front(LogLine { at: e.ts, level, text: e.message });
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------
    // Periodic work
    // ---------------------------------------------------------------------------------

    /// Called every loop iteration: controller events, search results, timers.
    pub fn tick(&mut self) {
        let events: Vec<Event> = match &self.ctl {
            Some(c) => c.events.try_iter().collect(),
            None => Vec::new(),
        };
        let had_events = !events.is_empty();
        for ev in events {
            self.on_event(ev);
        }
        if had_events || self.last_refresh.elapsed() >= Duration::from_secs(3) {
            self.refresh();
        }
        // Debounced search.
        if self.search.dirty_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(150)) {
            self.search.dirty_at = None;
            self.submit_search();
        }
        while let Ok(resp) = self.worker.results.try_recv() {
            self.on_search_result(resp);
        }
        self.tick_screens();
        if let Some(v) = self.vocab_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.vocab = v;
            self.vocab_rx = None;
        }
        if let Some(rx) = &self.setup.rx {
            if let Ok(r) = rx.try_recv() {
                self.setup.report = Some(r);
                self.setup.running = false;
                self.setup.rx = None;
            }
        }
        if self.toast.as_ref().is_some_and(|(_, t)| t.elapsed() >= Duration::from_secs(6)) {
            self.toast = None;
        }
        // Scan rate (EWMA).
        if let Some(c) = &self.ctl {
            let files = c.progress.files.load(Ordering::Relaxed);
            let now = Instant::now();
            match self.rate.last {
                Some((t, f)) if now.duration_since(t) >= Duration::from_millis(500) => {
                    let inst = files.saturating_sub(f) as f64 / now.duration_since(t).as_secs_f64();
                    self.rate.files_per_sec = if self.rate.files_per_sec == 0.0 { inst } else { self.rate.files_per_sec * 0.6 + inst * 0.4 };
                    self.rate.last = Some((now, files));
                }
                None => self.rate.last = Some((now, files)),
                _ => {}
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Log { level, text } => self.push_log(level, text),
            Event::Attached { disk, probe, drive_id, label, outcome } => {
                let desc = format!(
                    "{} {} {} ({})",
                    probe.dev_path,
                    probe.model.as_deref().unwrap_or("drive"),
                    di_model::units::format_size(probe.capacity_bytes),
                    probe.bus.as_str()
                );
                let who = match (drive_id, &label) {
                    (Some(id), Some(l)) => format!("D{id} \"{l}\""),
                    (Some(id), None) => format!("D{id}"),
                    _ => "New drive".into(),
                };
                let status = match &outcome {
                    Outcome::Queued => {
                        self.push_log(Level::Info, format!("{who} connected: {desc}; scanning."));
                        ConnStatus::Queued
                    }
                    Outcome::UpToDate { .. } => {
                        self.push_log(Level::Info, format!("{who} connected: {desc}; up to date."));
                        ConnStatus::UpToDate
                    }
                    Outcome::Reported => ConnStatus::Idle,
                    Outcome::Ignored(_) => ConnStatus::Idle,
                    Outcome::Ask(Question::NewDrive) => {
                        self.push_log(Level::Info, format!("New drive connected: {desc}."));
                        if self.modal.is_none() {
                            self.modal = Some(Modal::NewDrive { disk: disk.clone(), desc: desc.clone(), input: LineEdit::default() });
                        }
                        ConnStatus::NeedsAnswer
                    }
                    Outcome::Ask(Question::Ambiguous(c)) => {
                        let names: Vec<(i64, String)> = c
                            .iter()
                            .map(|(id, _)| {
                                let label = self
                                    .catalog_drives
                                    .iter()
                                    .find(|d| d.id == *id)
                                    .and_then(|d| d.label.clone())
                                    .unwrap_or_default();
                                (*id, label)
                            })
                            .collect();
                        self.push_log(Level::Warn, format!("{desc} matches more than one known drive."));
                        if self.modal.is_none() {
                            self.modal = Some(Modal::Ambiguous { disk: disk.clone(), desc, candidates: names, sel: 0 });
                        }
                        ConnStatus::NeedsAnswer
                    }
                };
                self.connected.insert(disk, Connected { probe: *probe, drive_id, status });
            }
            Event::Detached { disk, expected } => {
                if !expected {
                    self.push_log(Level::Info, format!("/dev/{disk} disconnected."));
                }
                self.connected.remove(&disk);
            }
            Event::JobStarted { disk } => {
                self.job_lines.clear();
                self.rate = Rate::default();
                if let Some(c) = self.connected.get_mut(&disk) {
                    c.status = ConnStatus::Scanning;
                }
            }
            Event::JobStep { text, .. } => {
                self.push_log(Level::Info, text.trim().to_string());
                self.job_lines.push_back(text);
                while self.job_lines.len() > 8 {
                    self.job_lines.pop_front();
                }
            }
            Event::JobFinished { disk, result } => {
                let status = match result {
                    Ok(report) => {
                        let lines = report_lines(&report);
                        let last = lines.last().cloned().unwrap_or_default();
                        for l in lines {
                            self.push_log(Level::Info, l.trim().to_string());
                            self.job_lines.push_back(l);
                        }
                        if report.drive_id > 0 {
                            if let Some(c) = self.connected.get_mut(&disk) {
                                c.drive_id = Some(report.drive_id);
                            }
                        }
                        self.toast(last.clone());
                        ConnStatus::Done(last)
                    }
                    Err(e) => {
                        self.push_log(Level::Error, format!("/dev/{disk}: {e}"));
                        self.toast(format!("/dev/{disk}: scan failed"));
                        ConnStatus::Failed(e)
                    }
                };
                if let Some(c) = self.connected.get_mut(&disk) {
                    c.status = status;
                }
                self.load_vocab();
                self.invalidate_screens();
            }
        }
    }

    /// Reload drives (catalog + connected) and the selected drive's details.
    pub fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        let selected = self.drives.get(self.drive_sel).map(|r| (r.drive_id, r.disk.clone()));
        let cat = self.catalog();
        self.catalog_drives = cat.as_ref().and_then(|c| c.drives().ok()).unwrap_or_default();
        let mut rows: Vec<DriveRow> = Vec::new();
        // Connected drives first (unknown ones, then known), then the rest of the catalog.
        for (disk, c) in &self.connected {
            if c.drive_id.is_none() {
                rows.push(DriveRow {
                    drive_id: None,
                    disk: Some(disk.clone()),
                    label: c.probe.model.clone().unwrap_or_else(|| "new drive".into()),
                    capacity: c.probe.capacity_bytes,
                    last_seen_at: None,
                    status: Some(c.status.clone()),
                });
            }
        }
        for d in &self.catalog_drives {
            let conn = self.connected.iter().find(|(_, c)| c.drive_id == Some(d.id));
            rows.push(DriveRow {
                drive_id: Some(d.id),
                disk: conn.map(|(k, _)| k.clone()),
                label: d.label.clone().unwrap_or_else(|| "(unlabeled)".into()),
                capacity: d.capacity_bytes,
                last_seen_at: Some(d.last_seen_at),
                status: conn.map(|(_, c)| c.status.clone()),
            });
        }
        rows.sort_by_key(|r| (r.disk.is_none(), r.drive_id.unwrap_or(0)));
        self.drives = rows;
        // Keep the selection on the same drive.
        if let Some((id, disk)) = selected {
            if let Some(i) = self.drives.iter().position(|r| (r.drive_id.is_some() && r.drive_id == id) || (id.is_none() && r.disk == disk)) {
                self.drive_sel = i;
            }
        }
        self.drive_sel = self.drive_sel.min(self.drives.len().saturating_sub(1));
        self.detail = match (self.drives.get(self.drive_sel).and_then(|r| r.drive_id), &cat) {
            (Some(id), Some(c)) => views::drive_detail(c.conn(), id).ok().flatten(),
            _ => None,
        };
        if let Some(c) = &cat {
            self.browse.filesystems = views::browsable_filesystems(c.conn()).unwrap_or_default();
        }
    }

    fn run_doctor(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let host = self.host.clone();
        std::thread::spawn(move || {
            let _ = tx.send(di_doctor::run_checks(&host));
        });
        self.setup.rx = Some(rx);
        self.setup.running = true;
    }

    // ---------------------------------------------------------------------------------
    // Search
    // ---------------------------------------------------------------------------------

    fn submit_search(&mut self) {
        let q = self.search.input.text().trim().to_string();
        if q.is_empty() {
            self.search.hits.clear();
            self.search.summary = None;
            self.search.error = None;
            self.search.running = false;
            return;
        }
        self.search.generation += 1;
        self.search.running = true;
        self.worker.submit(search_worker::Request {
            generation: self.search.generation,
            query: q,
            sort: self.search.sort,
            descending: self.search.descending,
            limit: self.search.limit,
        });
    }

    fn query_edited(&mut self) {
        self.search.dirty_at = Some(Instant::now());
        self.search.sel = 0;
        self.search.limit = 500;
    }

    /// Tab completion at the cursor. Several candidates: the first Tab inserts the
    /// first, repeated Tabs cycle. Returns false if there was nothing to complete.
    pub fn complete(&mut self, forward: bool) -> bool {
        let text = self.search.input.text().to_string();
        if let Some(c) = self.search.cycle.as_mut().filter(|c| c.after == text) {
            let n = c.items.len();
            let old = c.items[c.idx].insert.chars().count();
            c.idx = if forward { (c.idx + 1) % n } else { (c.idx + n - 1) % n };
            self.search.input.replace(c.start, c.start + old, &c.items[c.idx].insert);
            c.after = self.search.input.text().to_string();
            self.query_edited();
            return true;
        }
        self.search.cycle = None;
        let ctx = assist::context(&text, self.search.input.cursor());
        let items = assist::candidates(&ctx, &self.vocab);
        let Some((start, end)) = assist::replace_range(&ctx).filter(|_| !items.is_empty()) else { return false };
        let idx = if forward { 0 } else { items.len() - 1 };
        self.search.input.replace(start, end, &items[idx].insert);
        if items.len() > 1 {
            self.search.cycle = Some(Cycle { start, items, idx, after: self.search.input.text().to_string() });
        }
        self.query_edited();
        true
    }

    /// The active completion cycle, if the query has not been edited since.
    pub fn active_cycle(&self) -> Option<&Cycle> {
        self.search.cycle.as_ref().filter(|c| c.after == self.search.input.text())
    }

    /// Insert `key:` from the filter reference and continue typing in the box.
    fn insert_filter(&mut self, i: usize) {
        let Some(doc) = di_core::filters::FILTERS.get(i) else { return };
        let mut text = self.search.input.text().to_string();
        if !doc.key.is_empty() {
            if !text.is_empty() && !text.ends_with(' ') {
                text.push(' ');
            }
            text.push_str(doc.key);
            text.push(':');
            self.search.input.set(&text);
            self.query_edited();
        }
        self.screen = Screen::Search;
        self.search.focused = true;
    }

    /// Run an example query from the cheat sheet.
    pub fn use_example(&mut self, q: &str) {
        self.search.input.set(q);
        self.search.cycle = None;
        self.query_edited();
        self.search.focused = true;
    }

    fn on_search_result(&mut self, resp: search_worker::Response) {
        if resp.generation != self.search.generation {
            return;
        }
        self.search.running = false;
        match resp.outcome {
            search_worker::Outcome::Hits { hits, summary } => {
                self.search.hits = hits;
                self.search.summary = Some(summary);
                self.search.error = None;
                self.search.message = None;
                self.search.sel = self.search.sel.min(self.search.hits.len().saturating_sub(1));
            }
            search_worker::Outcome::QueryError(e) => self.search.error = Some(e),
            search_worker::Outcome::Failed(m) => self.search.message = Some(m),
        }
    }

    // ---------------------------------------------------------------------------------
    // Browse
    // ---------------------------------------------------------------------------------

    pub fn open_dir(&mut self, dir_id: i64) {
        let Some(cat) = self.catalog() else { return };
        self.browse.dir = views::dir_info(cat.conn(), dir_id).ok().flatten();
        self.browse.entries = views::list_dir(cat.conn(), dir_id, self.browse.sort, 5000).unwrap_or_default();
        self.browse.sel = 0;
    }

    /// Open the file's directory on the Browse screen with the file selected.
    pub(crate) fn browse_to_file(&mut self, file_id: i64) {
        let Some(cat) = self.catalog() else { return };
        let dir: Option<i64> = cat.conn().query_row("SELECT dir_id FROM files WHERE id = ?1", [file_id], |r| r.get(0)).ok();
        let Some(dir) = dir else { return };
        self.open_dir(dir);
        if let Some(i) = self.browse.entries.iter().position(|e| !e.is_dir && e.id == file_id) {
            self.browse.sel = i;
        }
        if let Some(fs) = self.browse.dir.as_ref().map(|d| d.filesystem_id) {
            if let Some(i) = self.browse.filesystems.iter().position(|f| f.fs_id == fs) {
                self.browse.fs_sel = i;
            }
        }
        self.screen = Screen::Browse;
    }

    fn browse_up(&mut self) {
        let Some(dir) = self.browse.dir.clone() else { return };
        match dir.parent_id {
            Some(p) => {
                self.open_dir(p);
                if let Some(i) = self.browse.entries.iter().position(|e| e.is_dir && e.id == dir.id) {
                    self.browse.sel = i;
                }
            }
            None => {
                self.browse.dir = None;
                self.browse.entries.clear();
            }
        }
    }

    // ---------------------------------------------------------------------------------
    // Keys
    // ---------------------------------------------------------------------------------

    fn send(&mut self, cmd: Command) -> bool {
        match &self.ctl {
            Some(c) => {
                c.send(cmd);
                true
            }
            None => {
                self.toast(format!(
                    "Viewer mode: {} controls drives.",
                    self.viewer.as_deref().unwrap_or("another drive-index process")
                ));
                false
            }
        }
    }

    pub fn job_running(&self) -> bool {
        self.ctl.as_ref().is_some_and(|c| c.job().is_some())
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        if self.modal.is_some() {
            self.modal_key(key);
            return;
        }
        // Text input on the Search screen swallows printable keys.
        if key.code == KeyCode::F(1) {
            self.modal = Some(Modal::Filters { sel: 0 });
            return;
        }
        if self.screen == Screen::Dupes && self.dupes_editing_key(key) {
            return;
        }
        if self.screen == Screen::Search && self.search.focused {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Down => self.search.focused = false,
                // Tab completes; with nothing to complete it moves to the results.
                KeyCode::Tab => {
                    if !self.complete(true) {
                        self.search.focused = false;
                    }
                }
                KeyCode::BackTab => {
                    self.complete(false);
                }
                _ => {
                    if self.search.input.handle(key) {
                        self.query_edited();
                    }
                }
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => return self.request_quit(),
            KeyCode::Char('?') => return self.modal = Some(Modal::Help),
            KeyCode::Char('/') => {
                self.screen = Screen::Search;
                self.search.focused = true;
                return;
            }
            KeyCode::Char(c @ '1'..='7') => {
                self.screen = Screen::ALL[(c as u8 - b'1') as usize];
                return;
            }
            KeyCode::Tab => {
                self.screen = Screen::ALL[(self.screen.index() + 1) % Screen::ALL.len()];
                return;
            }
            KeyCode::BackTab => {
                self.screen = Screen::ALL[(self.screen.index() + Screen::ALL.len() - 1) % Screen::ALL.len()];
                return;
            }
            _ => {}
        }
        match self.screen {
            Screen::Drives => self.drives_key(key),
            Screen::Search => self.search_key(key),
            Screen::Browse => self.browse_key(key),
            Screen::Dupes => self.dupes_key(key),
            Screen::Collections => self.collections_key(key),
            Screen::Setup => self.setup_key(key),
            Screen::Log => {}
        }
    }

    fn list_len(&self, kind: ListKind) -> usize {
        match kind {
            ListKind::Drives => self.drives.len(),
            ListKind::Results => self.search.hits.len(),
            ListKind::BrowseFs => self.browse.filesystems.len(),
            ListKind::BrowseEntries => self.browse.entries.len(),
            ListKind::DupeGroups => self.dupes.groups.len(),
            ListKind::DupeMembers => self.dupes.members.len(),
            ListKind::CoverageDrives => self.collections.coverage.as_ref().map(|c| c.drives.len()).unwrap_or(0),
        }
    }

    fn set_selection(&mut self, kind: ListKind, i: usize) {
        match kind {
            ListKind::Drives => {
                self.drive_sel = i;
                self.refresh();
            }
            ListKind::Results => {
                self.search.focused = false;
                self.search.sel = i;
            }
            ListKind::BrowseFs => self.browse.fs_sel = i,
            ListKind::BrowseEntries => self.browse.sel = i,
            ListKind::DupeGroups => {
                self.dupes.focus_members = false;
                if self.dupes.sel != i {
                    self.dupes.sel = i;
                    self.load_members();
                }
            }
            ListKind::DupeMembers => {
                self.dupes.focus_members = true;
                self.dupes.member_sel = i;
            }
            ListKind::CoverageDrives => self.collections.drive_sel = i,
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let code = if m.kind == MouseEventKind::ScrollDown { KeyCode::Down } else { KeyCode::Up };
                if matches!(self.modal, Some(Modal::Filters { .. })) {
                    for _ in 0..3 {
                        self.modal_key(KeyEvent::new(code, KeyModifiers::NONE));
                    }
                    return;
                }
                if self.modal.is_some() {
                    return;
                }
                if self.screen == Screen::Search {
                    self.search.focused = false;
                }
                for _ in 0..3 {
                    // Scrolling up past the first result must not jump into the query box.
                    if self.screen == Screen::Search && code == KeyCode::Up && self.search.sel == 0 {
                        break;
                    }
                    self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(modal) = self.hits.modal {
                    // Clicking outside an informational popup closes it.
                    if !inside(modal)
                        && matches!(self.modal, Some(Modal::Help) | Some(Modal::File(_)) | Some(Modal::Filters { .. }))
                    {
                        self.modal = None;
                        return;
                    }
                    let row = self.hits.filter_rows.iter().find(|(ry, _)| *ry == y).map(|(_, i)| *i);
                    if let (Some(Modal::Filters { sel }), Some(i)) = (self.modal.as_mut(), row) {
                        let double = *sel == i && self.last_click.is_some_and(|(t, _, j)| j == i && t.elapsed() < Duration::from_millis(450));
                        *sel = i;
                        self.last_click = Some((Instant::now(), ListKind::Results, i));
                        if double {
                            self.modal = None;
                            self.last_click = None;
                            self.insert_filter(i);
                        }
                    }
                    return;
                }
                if y == self.hits.tabs_y {
                    if let Some(&(_, _, screen)) = self.hits.tabs.iter().find(|(a, b, _)| x >= *a && x < *b) {
                        self.screen = screen;
                    }
                    return;
                }
                if self.screen == Screen::Search && self.hits.search_input.is_some_and(inside) {
                    self.search.focused = true;
                    return;
                }
                if self.screen == Screen::Search {
                    if let Some(&(_, q)) = self.hits.examples.iter().find(|(ry, _)| *ry == y) {
                        self.use_example(q);
                        return;
                    }
                }
                let Some(list) = self.hits.list.filter(|l| inside(l.area)) else { return };
                let i = list.offset + (y - list.area.y) as usize;
                if i >= self.list_len(list.kind) {
                    return;
                }
                self.set_selection(list.kind, i);
                let double = self
                    .last_click
                    .is_some_and(|(t, k, j)| k == list.kind && j == i && t.elapsed() < Duration::from_millis(450));
                self.last_click = Some((Instant::now(), list.kind, i));
                if double {
                    self.last_click = None;
                    self.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                }
            }
            _ => {}
        }
    }

    fn request_quit(&mut self) {
        if self.job_running() {
            self.modal = Some(Modal::ConfirmQuit);
        } else {
            self.quit = true;
        }
    }

    pub(crate) fn move_sel(sel: &mut usize, len: usize, key: KeyCode) {
        if len == 0 {
            *sel = 0;
            return;
        }
        *sel = match key {
            KeyCode::Up | KeyCode::Char('k') => sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (*sel + 1).min(len - 1),
            KeyCode::PageUp => sel.saturating_sub(20),
            KeyCode::PageDown => (*sel + 20).min(len - 1),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => len - 1,
            _ => *sel,
        };
    }

    fn drives_key(&mut self, key: KeyEvent) {
        let row = self.drives.get(self.drive_sel).cloned();
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') | KeyCode::Home | KeyCode::End
            | KeyCode::PageUp | KeyCode::PageDown => {
                Self::move_sel(&mut self.drive_sel, self.drives.len(), key.code);
                self.refresh();
            }
            KeyCode::Char('s') => match row.and_then(|r| r.disk) {
                Some(disk) => {
                    if self.send(Command::Scan { disk: disk.clone(), identity: IdentityChoice::Auto, label: None }) {
                        self.toast(format!("Scanning /dev/{disk}…"));
                    }
                }
                None => self.toast("That drive is not connected."),
            },
            KeyCode::Char('v') => match row.as_ref().and_then(|r| r.disk.clone()) {
                Some(disk) => {
                    if self.send(Command::Verify(disk.clone())) {
                        self.toast(format!("Verifying likely duplicates on /dev/{disk}…"));
                    }
                }
                None => self.toast("Connect the drive to verify its duplicates."),
            },
            KeyCode::Char('p') => {
                if let Some(job) = self.ctl.as_ref().and_then(|c| c.job()) {
                    if job.control.is_paused() {
                        self.send(Command::Resume);
                    } else {
                        self.send(Command::Pause);
                    }
                } else {
                    self.toast("No scan is running.");
                }
            }
            KeyCode::Char('x') => {
                if self.job_running() {
                    self.send(Command::Cancel);
                } else {
                    self.toast("No scan is running.");
                }
            }
            KeyCode::Char('u') | KeyCode::Char('e') => match row.and_then(|r| r.disk) {
                Some(disk) => {
                    let eject = key.code == KeyCode::Char('e');
                    if self.send(if eject { Command::Eject(disk.clone()) } else { Command::Unmount(disk.clone()) }) {
                        self.toast(format!("{} /dev/{disk}…", if eject { "Ejecting" } else { "Unmounting" }));
                    }
                }
                None => self.toast("That drive is not connected."),
            },
            KeyCode::Char('l') => match row.as_ref().and_then(|r| r.drive_id) {
                Some(id) => {
                    let current = self.detail.as_ref().and_then(|d| d.label.clone()).unwrap_or_default();
                    self.modal = Some(Modal::Label { drive_id: id, input: LineEdit::with(&current) });
                }
                None => self.toast("Scan the drive first; then it can be labelled."),
            },
            KeyCode::Char('n') => match row.as_ref().and_then(|r| r.drive_id) {
                Some(id) => {
                    let current = self.detail.as_ref().and_then(|d| d.notes.clone()).unwrap_or_default();
                    self.modal = Some(Modal::Notes { drive_id: id, input: LineEdit::with(&current) });
                }
                None => self.toast("Scan the drive first; then it can have notes."),
            },
            KeyCode::Char('b') | KeyCode::Enter => {
                // Browse the selected drive's first scanned filesystem.
                if let Some(fs) = self.detail.as_ref().and_then(|d| d.filesystems.iter().find(|f| f.root_dir.is_some())) {
                    let root = fs.root_dir.unwrap();
                    if let Some(i) = self.browse.filesystems.iter().position(|f| f.fs_id == fs.id) {
                        self.browse.fs_sel = i;
                    }
                    self.open_dir(root);
                    self.screen = Screen::Browse;
                }
            }
            KeyCode::Char('r') => self.refresh(),
            _ => {}
        }
    }

    fn search_key(&mut self, key: KeyEvent) {
        let len = self.search.hits.len();
        match key.code {
            KeyCode::Up if self.search.sel == 0 => self.search.focused = true,
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') | KeyCode::PageUp | KeyCode::PageDown
            | KeyCode::Home | KeyCode::End | KeyCode::Char('g') | KeyCode::Char('G') => {
                Self::move_sel(&mut self.search.sel, len, key.code);
                // Near the end of a truncated result set: fetch more.
                let total = self.search.summary.as_ref().map(|s| s.files as usize).unwrap_or(0);
                if len > 0 && self.search.sel + 50 >= len && len == self.search.limit && total > len {
                    self.search.limit += 1000;
                    self.submit_search();
                }
            }
            KeyCode::Char('i') | KeyCode::Esc => self.search.focused = true,
            KeyCode::Char('s') => {
                self.search.sort = match self.search.sort {
                    SortKey::Path => SortKey::Size,
                    SortKey::Size => SortKey::Mtime,
                    SortKey::Mtime => SortKey::Name,
                    SortKey::Name => SortKey::Path,
                };
                self.search.descending = matches!(self.search.sort, SortKey::Size | SortKey::Mtime);
                self.submit_search();
            }
            KeyCode::Char('d') => {
                self.search.descending = !self.search.descending;
                self.submit_search();
            }
            KeyCode::Enter => {
                if let Some(h) = self.search.hits.get(self.search.sel) {
                    let id = h.file_id;
                    if let Some(d) = self.catalog().and_then(|c| views::file_detail(c.conn(), id).ok().flatten()) {
                        self.modal = Some(Modal::File(Box::new(d)));
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(h) = self.search.hits.get(self.search.sel) {
                    let text = h.path.clone();
                    let msg = if copy_to_clipboard(&text) { "Copied the path." } else { "No clipboard tool found." };
                    self.toast(msg);
                }
            }
            KeyCode::Char('b') => {
                if let Some(id) = self.search.hits.get(self.search.sel).map(|h| h.file_id) {
                    self.browse_to_file(id);
                }
            }
            _ => {}
        }
    }

    fn browse_key(&mut self, key: KeyEvent) {
        if self.browse.dir.is_none() {
            // Picking a filesystem.
            match key.code {
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    if let Some(fs) = self.browse.filesystems.get(self.browse.fs_sel) {
                        let root = fs.root_dir;
                        self.open_dir(root);
                    }
                }
                code => Self::move_sel(&mut self.browse.fs_sel, self.browse.filesystems.len(), code),
            }
            return;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(e) = self.browse.entries.get(self.browse.sel).cloned() {
                    if e.is_dir {
                        self.open_dir(e.id);
                    } else if let Some(d) = self.catalog().and_then(|c| views::file_detail(c.conn(), e.id).ok().flatten()) {
                        self.modal = Some(Modal::File(Box::new(d)));
                    }
                }
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.browse_up(),
            KeyCode::Char('s') => {
                self.browse.sort = match self.browse.sort {
                    BrowseSort::Size => BrowseSort::Count,
                    BrowseSort::Count => BrowseSort::Name,
                    BrowseSort::Name => BrowseSort::Mtime,
                    BrowseSort::Mtime => BrowseSort::Size,
                };
                if let Some(d) = self.browse.dir.clone() {
                    let sel_name = self.browse.entries.get(self.browse.sel).map(|e| e.name.clone());
                    self.open_dir(d.id);
                    if let Some(n) = sel_name {
                        self.browse.sel = self.browse.entries.iter().position(|e| e.name == n).unwrap_or(0);
                    }
                }
            }
            code => Self::move_sel(&mut self.browse.sel, self.browse.entries.len(), code),
        }
    }

    fn setup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('r') if !self.setup.running => self.run_doctor(),
            KeyCode::Char('c') => {
                let cmd = self.setup.report.as_ref().and_then(|r| r.install_command.clone());
                match cmd {
                    Some(c) => {
                        let ok = copy_to_clipboard(&c);
                        self.toast(if ok { format!("Copied: {c}") } else { "No clipboard tool found.".into() });
                    }
                    None => self.toast("Nothing to install."),
                }
            }
            KeyCode::Char('H') => {
                let stamp = jiff::Zoned::now().strftime("%Y-%m-%d-%H%M%S").to_string();
                let dest = std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(format!("drive-index-{}-{stamp}.db", self.host));
                let msg = match Catalog::open(&self.catalog_path).and_then(|c| c.handoff_copy(&dest)) {
                    Ok(()) => format!("Wrote {}", dest.display()),
                    Err(e) => format!("Handoff failed: {e:#}"),
                };
                self.push_log(Level::Info, msg.clone());
                self.toast(msg);
            }
            _ => {}
        }
    }

    fn modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        let keep = match modal {
            Modal::Help | Modal::File(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => None,
                KeyCode::Char('c') => {
                    if let Modal::File(d) = &modal {
                        let ok = copy_to_clipboard(&d.path);
                        self.toast(if ok { "Copied the path." } else { "No clipboard tool found." });
                    }
                    Some(modal)
                }
                _ => Some(modal),
            },
            Modal::Filters { mut sel } => {
                let n = di_core::filters::FILTERS.len();
                match key.code {
                    KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q') => None,
                    KeyCode::Enter => {
                        self.insert_filter(sel);
                        None
                    }
                    KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                        Self::move_sel(&mut sel, n, key.code);
                        Some(Modal::Filters { sel })
                    }
                    KeyCode::Char(c) => {
                        // Jump to the next filter starting with the letter.
                        let c = c.to_ascii_lowercase();
                        let next = (1..=n).map(|d| (sel + d) % n).find(|i| di_core::filters::FILTERS[*i].key.starts_with(c));
                        Some(Modal::Filters { sel: next.unwrap_or(sel) })
                    }
                    _ => Some(Modal::Filters { sel }),
                }
            }
            Modal::ConfirmQuit => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.quit = true;
                    None
                }
                _ => None,
            },
            Modal::Label { drive_id, mut input } => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    let text = input.text().trim().to_string();
                    let r = Catalog::open(&self.catalog_path)
                        .and_then(|c| c.set_drive_label(drive_id, (!text.is_empty()).then_some(text.as_str())));
                    self.toast(match r {
                        Ok(_) if text.is_empty() => format!("D{drive_id} label cleared"),
                        Ok(_) => format!("D{drive_id} labelled \"{text}\""),
                        Err(e) => format!("Could not save the label: {e:#}"),
                    });
                    self.refresh();
                    None
                }
                _ => {
                    input.handle(key);
                    Some(Modal::Label { drive_id, input })
                }
            },
            Modal::Notes { drive_id, mut input } => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    let text = input.text().trim().to_string();
                    let r = Catalog::open(&self.catalog_path)
                        .and_then(|c| c.set_drive_notes(drive_id, (!text.is_empty()).then_some(text.as_str())));
                    self.toast(match r {
                        Ok(_) => format!("D{drive_id} notes saved"),
                        Err(e) => format!("Could not save the notes: {e:#}"),
                    });
                    self.refresh();
                    None
                }
                _ => {
                    input.handle(key);
                    Some(Modal::Notes { drive_id, input })
                }
            },
            Modal::NewDrive { disk, desc, mut input } => match key.code {
                KeyCode::Esc => {
                    if let Some(c) = self.connected.get_mut(&disk) {
                        c.status = ConnStatus::Idle;
                    }
                    self.toast(format!("Ignoring /dev/{disk} for now (press s on it to scan later)."));
                    None
                }
                KeyCode::Enter => {
                    let label = Some(input.text().trim().to_string()).filter(|s| !s.is_empty());
                    self.send(Command::Scan { disk: disk.clone(), identity: IdentityChoice::New, label });
                    if let Some(c) = self.connected.get_mut(&disk) {
                        c.status = ConnStatus::Queued;
                    }
                    None
                }
                _ => {
                    input.handle(key);
                    Some(Modal::NewDrive { disk, desc, input })
                }
            },
            Modal::Ambiguous { disk, desc, candidates, mut sel } => match key.code {
                KeyCode::Esc => None,
                KeyCode::Up | KeyCode::Down => {
                    // Candidates plus a final "new drive" option.
                    Self::move_sel(&mut sel, candidates.len() + 1, key.code);
                    Some(Modal::Ambiguous { disk, desc, candidates, sel })
                }
                KeyCode::Enter => {
                    let identity = match candidates.get(sel) {
                        Some((id, _)) => IdentityChoice::SameAs(*id),
                        None => IdentityChoice::New,
                    };
                    self.send(Command::Scan { disk, identity, label: None });
                    None
                }
                _ => Some(Modal::Ambiguous { disk, desc, candidates, sel }),
            },
        };
        self.modal = keep;
    }
}

/// Copy text to the system clipboard (pbcopy, wl-copy, xclip, xsel).
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command as Cmd, Stdio};
    let candidates: &[(&str, &[&str])] =
        &[("pbcopy", &[]), ("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])];
    for (prog, args) in candidates {
        if let Ok(mut child) = Cmd::new(prog).args(*args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return true;
            }
        }
    }
    false
}
