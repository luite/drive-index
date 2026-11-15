//! State and keys of the Duplicates and Collections screens. Their queries run on
//! background threads; results arrive through `tick`.

use std::sync::mpsc::{self, Receiver};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use di_core::Catalog;
use di_core::coverage::{self, Collection, Coverage};
use di_core::dupes::{self, Confidence, DupeGroup, DupeMember, DupeQuery, DupeSummary};
use di_core::views;

use crate::app::{App, Modal, Screen};
use crate::input::LineEdit;

/// Minimum sizes `m` cycles through.
pub const MIN_SIZES: [u64; 6] = [0, 1 << 20, 10 << 20, 100 << 20, 1 << 30, 10 << 30];

type GroupsResult = Result<(Vec<DupeGroup>, DupeSummary), String>;

pub struct DupesState {
    pub confidence: Confidence,
    pub min_size: u64,
    pub same_drive_only: bool,
    /// Scope query (e.g. `in:photos-raw`); empty means everything.
    pub filter: String,
    /// Editing the scope query.
    pub editing: Option<LineEdit>,
    pub groups: Vec<DupeGroup>,
    pub summary: Option<DupeSummary>,
    pub sel: usize,
    pub members: Vec<DupeMember>,
    pub member_sel: usize,
    pub focus_members: bool,
    pub error: Option<String>,
    pub running: bool,
    /// Results are current (cleared after scans and setting changes).
    pub loaded: bool,
    rx: Option<Receiver<GroupsResult>>,
}

impl Default for DupesState {
    fn default() -> Self {
        DupesState {
            confidence: Confidence::Likely,
            min_size: 1 << 20,
            same_drive_only: false,
            filter: String::new(),
            editing: None,
            groups: Vec::new(),
            summary: None,
            sel: 0,
            members: Vec::new(),
            member_sel: 0,
            focus_members: false,
            error: None,
            running: false,
            loaded: false,
            rx: None,
        }
    }
}

#[derive(Default)]
pub struct CollectionsState {
    pub list: Vec<Collection>,
    pub sel: usize,
    pub coverage: Option<Coverage>,
    pub drive_sel: usize,
    pub error: Option<String>,
    pub running: bool,
    pub loaded: bool,
    rx: Option<Receiver<Result<Coverage, String>>>,
}

impl App {
    /// Start loads for the visible screen and collect finished ones.
    pub(crate) fn tick_screens(&mut self) {
        if let Some(r) = self.dupes.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.dupes.rx = None;
            self.dupes.running = false;
            match r {
                Ok((groups, summary)) => {
                    self.dupes.groups = groups;
                    self.dupes.summary = Some(summary);
                    self.dupes.error = None;
                    self.dupes.sel = self.dupes.sel.min(self.dupes.groups.len().saturating_sub(1));
                    self.load_members();
                }
                Err(e) => self.dupes.error = Some(e),
            }
        }
        if let Some(r) = self.collections.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.collections.rx = None;
            self.collections.running = false;
            match r {
                Ok(c) => {
                    self.collections.drive_sel = self.collections.drive_sel.min(c.drives.len().saturating_sub(1));
                    self.collections.coverage = Some(c);
                    self.collections.error = None;
                }
                Err(e) => self.collections.error = Some(e),
            }
        }
        match self.screen {
            Screen::Dupes if !self.dupes.loaded && !self.dupes.running => self.load_dupes(),
            Screen::Collections if !self.collections.loaded && !self.collections.running => self.load_coverage(),
            _ => {}
        }
    }

    /// Catalog contents changed (a scan finished): reload when next shown.
    pub(crate) fn invalidate_screens(&mut self) {
        self.dupes.loaded = false;
        self.collections.loaded = false;
    }

    pub fn load_dupes(&mut self) {
        let q = DupeQuery {
            confidence: self.dupes.confidence,
            min_size: self.dupes.min_size,
            filter: Some(self.dupes.filter.clone()).filter(|f| !f.trim().is_empty()),
            same_drive_only: self.dupes.same_drive_only,
            limit: 2000,
        };
        let path = self.catalog_path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = Catalog::open_readonly(&path)
                .map_err(|e| format!("{e:#}"))
                .and_then(|c| dupes::groups(c.conn(), &q).map_err(|e| format!("{e:#}")));
            let _ = tx.send(r);
        });
        self.dupes.rx = Some(rx);
        self.dupes.running = true;
        self.dupes.loaded = true;
    }

    pub(crate) fn load_members(&mut self) {
        self.dupes.member_sel = 0;
        self.dupes.members = match (self.dupes.groups.get(self.dupes.sel), self.catalog()) {
            (Some(g), Some(c)) => dupes::members(c.conn(), self.dupes.confidence, &g.key, g.size).unwrap_or_default(),
            _ => Vec::new(),
        };
    }

    pub fn load_coverage(&mut self) {
        if let Some(c) = self.catalog() {
            self.collections.list = coverage::collections(c.conn()).unwrap_or_default();
        }
        self.collections.loaded = true;
        let Some(col) = self.collections.list.get(self.collections.sel).cloned() else {
            self.collections.coverage = None;
            return;
        };
        let path = self.catalog_path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = Catalog::open_readonly(&path)
                .map_err(|e| format!("{e:#}"))
                .and_then(|c| coverage::coverage(c.conn(), &col.slug).map_err(|e| format!("{e:#}")));
            let _ = tx.send(r);
        });
        self.collections.rx = Some(rx);
        self.collections.running = true;
    }

    /// Show `query` on the Search screen.
    pub(crate) fn search_for(&mut self, query: &str) {
        self.use_example(query);
        self.search.focused = false;
        self.screen = Screen::Search;
    }

    /// Keys while the Duplicates scope query is being edited. Returns true if handled.
    pub(crate) fn dupes_editing_key(&mut self, key: KeyEvent) -> bool {
        let Some(input) = self.dupes.editing.as_mut() else { return false };
        match key.code {
            KeyCode::Esc => self.dupes.editing = None,
            KeyCode::Enter => {
                self.dupes.filter = input.text().trim().to_string();
                self.dupes.editing = None;
                self.dupes.sel = 0;
                self.load_dupes();
            }
            _ => {
                input.handle(key);
            }
        }
        true
    }

    pub(crate) fn dupes_key(&mut self, key: KeyEvent) {
        let d = &mut self.dupes;
        match key.code {
            KeyCode::Char('c') => {
                d.confidence = d.confidence.next();
                d.sel = 0;
                self.load_dupes();
            }
            KeyCode::Char('m') => {
                let i = MIN_SIZES.iter().position(|m| *m == d.min_size).unwrap_or(0);
                d.min_size = MIN_SIZES[(i + 1) % MIN_SIZES.len()];
                d.sel = 0;
                self.load_dupes();
            }
            KeyCode::Char('x') => {
                d.same_drive_only = !d.same_drive_only;
                d.sel = 0;
                self.load_dupes();
            }
            KeyCode::Char('f') => d.editing = Some(LineEdit::with(&d.filter)),
            KeyCode::Char('r') => self.load_dupes(),
            KeyCode::Right | KeyCode::Char('l') if !d.focus_members && !d.members.is_empty() => d.focus_members = true,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc if d.focus_members => d.focus_members = false,
            KeyCode::Enter if !d.focus_members => d.focus_members = !d.members.is_empty(),
            KeyCode::Enter => {
                if let Some(m) = d.members.get(d.member_sel) {
                    let id = m.file_id;
                    if let Some(fd) = self.catalog().and_then(|c| views::file_detail(c.conn(), id).ok().flatten()) {
                        self.modal = Some(Modal::File(Box::new(fd)));
                    }
                }
            }
            KeyCode::Char('b') => {
                let id = if d.focus_members { d.members.get(d.member_sel) } else { d.members.first() }.map(|m| m.file_id);
                if let Some(id) = id {
                    self.browse_to_file(id);
                }
            }
            code if d.focus_members => App::move_sel(&mut d.member_sel, d.members.len(), code),
            code => {
                let before = d.sel;
                App::move_sel(&mut d.sel, d.groups.len(), code);
                if d.sel != before {
                    self.load_members();
                }
            }
        }
    }

    pub(crate) fn collections_key(&mut self, key: KeyEvent) {
        let c = &mut self.collections;
        let n = c.list.len().max(1);
        let slug = c.list.get(c.sel).map(|x| x.slug.clone());
        let drive = c.coverage.as_ref().and_then(|v| v.drives.get(c.drive_sel)).map(|d| d.drive_id);
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                c.sel = (c.sel + n - 1) % n;
                c.drive_sel = 0;
                self.load_coverage();
            }
            KeyCode::Right | KeyCode::Char('l') => {
                c.sel = (c.sel + 1) % n;
                c.drive_sel = 0;
                self.load_coverage();
            }
            KeyCode::Char('r') => self.load_coverage(),
            KeyCode::Char('a') => {
                if let Some(s) = slug {
                    self.search_for(&format!("in:{s} copies:1"));
                }
            }
            KeyCode::Enter => {
                if let (Some(s), Some(d)) = (slug, drive) {
                    self.search_for(&format!("in:{s} drive:D{d}"));
                }
            }
            KeyCode::Char('o') => {
                if let (Some(s), Some(d)) = (slug, drive) {
                    self.search_for(&format!("in:{s} drive:D{d} copies:1"));
                }
            }
            code => {
                let len = c.coverage.as_ref().map(|v| v.drives.len()).unwrap_or(0);
                App::move_sel(&mut c.drive_sel, len, code);
            }
        }
    }
}
