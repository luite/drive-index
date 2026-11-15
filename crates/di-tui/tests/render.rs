//! Render the TUI against a real catalog with ratatui's TestBackend and drive it
//! with key events (PLAN.md §13).

use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use di_scan::{PhaseCell, ResumeMode, ScanRequest, run_dir_scan};
use di_tui::app::{App, Modal, Screen};
use di_tui::ui;
use di_walk::{Control, Progress, WalkConfig};

fn catalog_with_photos(tmp: &Path) -> std::path::PathBuf {
    let data = tmp.join("data");
    for (p, n) in [
        ("Photos/2014/Iceland/IMG_4411.CR2", 24_000),
        ("Photos/2014/Iceland/IMG_4412.CR2", 23_000),
        ("Photos/2015/beach.jpg", 5_000),
        ("Video/wedding.mov", 400_000),
    ] {
        let path = data.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; n]).unwrap();
    }
    let db = tmp.join("index.db");
    let req = ScanRequest {
        catalog_path: db.clone(),
        dir: data,
        label: Some("Photo Archive".into()),
        walk: WalkConfig { threads: 2, ..WalkConfig::default() },
        resume: ResumeMode::Auto,
        content: None,
    };
    run_dir_scan(&req, &Control::default(), &Progress::default(), &PhaseCell::default()).unwrap();
    db
}

fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| ui::draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn key(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn type_text(app: &mut App, s: &str) {
    for c in s.chars() {
        key(app, KeyCode::Char(c));
    }
}

/// Tick until `cond` holds (search and doctor run on background threads).
fn wait(app: &mut App, cond: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond(app) {
        assert!(Instant::now() < deadline, "timed out waiting");
        std::thread::sleep(Duration::from_millis(20));
        app.tick();
    }
}

#[test]
fn screens_render_and_respond() {
    let tmp = tempfile::tempdir().unwrap();
    let db = catalog_with_photos(tmp.path());
    let mut app = App::new(db, "test-host".into(), None, Some("test controller".into()));

    // Drives
    let s = render(&mut app, 140, 40);
    assert!(s.contains("Photo Archive"), "{s}");
    assert!(s.contains("D1"));
    assert!(s.contains("Viewer mode"));
    assert!(s.contains("1 Drives"));

    // Actions that need the controller explain themselves in viewer mode.
    key(&mut app, KeyCode::Char('e'));
    assert!(app.toast.as_ref().unwrap().0.contains("not connected") || app.toast.as_ref().unwrap().0.contains("Viewer"));

    // Search as you type.
    key(&mut app, KeyCode::Char('/'));
    assert_eq!(app.screen, Screen::Search);
    type_text(&mut app, "ext:cr2");
    wait(&mut app, |a| a.search.hits.len() == 2);
    let s = render(&mut app, 140, 40);
    assert!(s.contains("/Photos/2014/Iceland/IMG_4411.CR2"), "{s}");
    assert!(s.contains("Σ 2 files"));

    // Query errors are explained; the hint line warns while typing.
    for _ in 0..7 {
        key(&mut app, KeyCode::Backspace);
    }
    type_text(&mut app, "bogus:1");
    wait(&mut app, |a| a.search.error.is_some());
    let s = render(&mut app, 140, 40);
    assert!(s.contains("✖ unknown filter 'bogus:'"), "{s}");
    assert!(s.contains("bogus: is not a filter"), "{s}");

    // Detail popup from a result.
    for _ in 0..7 {
        key(&mut app, KeyCode::Backspace);
    }
    type_text(&mut app, "wedding");
    wait(&mut app, |a| a.search.hits.len() == 1);
    key(&mut app, KeyCode::Enter); // leave the input
    key(&mut app, KeyCode::Enter); // open details
    assert!(matches!(app.modal, Some(Modal::File(_))));
    let s = render(&mut app, 140, 40);
    assert!(s.contains("/Video/wedding.mov") && s.contains("391 KiB"), "{s}");
    key(&mut app, KeyCode::Esc);
    assert!(app.modal.is_none());

    // Browse: pick the filesystem, descend, go back up.
    key(&mut app, KeyCode::Char('3'));
    let s = render(&mut app, 140, 40);
    assert!(s.contains("D1.1"), "{s}");
    key(&mut app, KeyCode::Enter);
    let s = render(&mut app, 140, 40);
    assert!(s.contains("Video/") && s.contains("Photos/"), "{s}");
    // Largest first: Video (400 kB) before Photos (52 kB).
    assert!(s.find("Video/").unwrap() < s.find("Photos/").unwrap());
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Enter);
    let s = render(&mut app, 140, 40);
    assert!(s.contains(": /Photos") && s.contains("2014/"), "{s}");
    key(&mut app, KeyCode::Backspace);
    assert_eq!(app.browse.dir.as_ref().unwrap().path, "/");

    // Setup shows the doctor report once it arrives.
    key(&mut app, KeyCode::Char('7'));
    wait(&mut app, |a| a.setup.report.is_some());
    let s = render(&mut app, 140, 40);
    assert!(s.contains("smartctl"), "{s}");

    // Help overlay.
    key(&mut app, KeyCode::Char('?'));
    let s = render(&mut app, 140, 40);
    assert!(s.contains("Help") && s.contains("ext:cr2,crw"));
    key(&mut app, KeyCode::Esc);

    // Every screen draws in a small terminal without panicking.
    for n in '1'..='7' {
        key(&mut app, KeyCode::Char(n));
        let s = render(&mut app, 80, 24);
        assert!(s.contains("drive-index"));
    }

    // q quits (no scan running).
    key(&mut app, KeyCode::Char('q'));
    assert!(app.quit);
}

#[test]
fn drive_detail_shows_smart() {
    let tmp = tempfile::tempdir().unwrap();
    let db = catalog_with_photos(tmp.path());
    {
        let cat = di_core::Catalog::open(&db).unwrap();
        let r = di_model::smart::SmartReport {
            device_type: "sat".into(),
            passed: Some(true),
            temperature_c: Some(31),
            power_on_hours: Some(12431),
            reallocated_sectors: Some(0),
            pending_sectors: Some(8),
            ..Default::default()
        };
        cat.save_smart(1, &Ok(r)).unwrap();
    }
    let mut app = App::new(db, "test-host".into(), None, Some("t".into()));
    let s = render(&mut app, 140, 40);
    assert!(s.contains("PASSED · 31 °C · 12,431 h · realloc 0 · pending 8"), "{s}");
    assert!(s.contains("⚠ 8 sectors pending reallocation"), "{s}");
}

#[test]
fn label_and_notes_dialogs_save() {
    let tmp = tempfile::tempdir().unwrap();
    let db = catalog_with_photos(tmp.path());
    let mut app = App::new(db.clone(), "test-host".into(), None, Some("t".into()));
    key(&mut app, KeyCode::Char('l'));
    assert!(matches!(app.modal, Some(Modal::Label { .. })));
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)); // clear
    type_text(&mut app, "Blue WD");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('n'));
    type_text(&mut app, "shelf B");
    key(&mut app, KeyCode::Enter);
    let s = render(&mut app, 140, 40);
    assert!(s.contains("Blue WD") && s.contains("shelf B"), "{s}");
}

fn click(app: &mut App, x: u16, y: u16) {
    app.on_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE });
}

fn scroll(app: &mut App, down: bool) {
    let kind = if down { MouseEventKind::ScrollDown } else { MouseEventKind::ScrollUp };
    app.on_mouse(MouseEvent { kind, column: 10, row: 10, modifiers: KeyModifiers::NONE });
}

/// Position of `needle` in a rendered frame.
fn find(frame: &str, needle: &str) -> (u16, u16) {
    for (y, line) in frame.lines().enumerate() {
        if let Some(b) = line.find(needle) {
            return (line[..b].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} not on screen:\n{frame}");
}

#[test]
fn mouse_tabs_rows_double_click_and_scroll() {
    let tmp = tempfile::tempdir().unwrap();
    let db = catalog_with_photos(tmp.path());
    let mut app = App::new(db, "test-host".into(), None, Some("t".into()));

    // Click the "3 Browse" tab.
    let frame = render(&mut app, 140, 40);
    let (x, y) = find(&frame, "3 Browse");
    click(&mut app, x + 2, y);
    assert_eq!(app.screen, Screen::Browse);

    // Double-click the filesystem row to open it, then a directory row.
    let frame = render(&mut app, 140, 40);
    let (x, y) = find(&frame, "D1.1");
    click(&mut app, x, y);
    click(&mut app, x, y);
    assert!(app.browse.dir.is_some(), "double-click opened the filesystem");
    let frame = render(&mut app, 140, 40);
    let (x, y) = find(&frame, "Photos/");
    click(&mut app, x, y);
    assert_eq!(app.browse.entries[app.browse.sel].name, "Photos", "single click selects");
    click(&mut app, x, y);
    assert_eq!(app.browse.dir.as_ref().unwrap().path, "/Photos");

    // Search results: click selects, double-click opens details, outside click closes.
    key(&mut app, KeyCode::Char('/'));
    type_text(&mut app, "ext:cr2");
    wait(&mut app, |a| a.search.hits.len() == 2);
    let frame = render(&mut app, 140, 40);
    let (x, y) = find(&frame, "IMG_4412.CR2");
    click(&mut app, x, y);
    assert!(!app.search.focused);
    assert_eq!(app.search.sel, 1);
    click(&mut app, x, y);
    assert!(matches!(app.modal, Some(Modal::File(_))));
    render(&mut app, 140, 40);
    click(&mut app, 0, 39);
    assert!(app.modal.is_none(), "clicking outside the popup closes it");

    // Clicking the query box focuses it again; the wheel moves the selection.
    let frame = render(&mut app, 140, 40);
    let (x, y) = find(&frame, "> ext:cr2");
    click(&mut app, x + 3, y);
    assert!(app.search.focused);
    scroll(&mut app, true);
    assert!(!app.search.focused);
    assert_eq!(app.search.sel, 1, "scroll is clamped to the results");
    scroll(&mut app, false);
    assert_eq!(app.search.sel, 0);
    assert!(!app.search.focused, "scrolling up does not jump into the query box");
}

#[test]
fn search_help_completes_and_explains() {
    let tmp = tempfile::tempdir().unwrap();
    let db = catalog_with_photos(tmp.path());
    let mut app = App::new(db, "test-host".into(), None, Some("t".into()));
    key(&mut app, KeyCode::Char('/'));

    // Empty query: a cheat sheet with clickable examples.
    let s = render(&mut app, 140, 40);
    assert!(s.contains("Search help") && s.contains("Examples") && s.contains("camera:"), "{s}");
    let (x, y) = find(&s, "dup:likely size:>100M");
    click(&mut app, x, y);
    assert_eq!(app.search.input.text(), "dup:likely size:>100M");
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));

    // A partial key: the hint explains it and Tab completes it.
    type_text(&mut app, "siz");
    let s = render(&mut app, 140, 40);
    assert!(s.contains("size:>10M") && s.contains("binary"), "{s}");
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.search.input.text(), "size:");
    assert!(app.search.focused, "Tab completed instead of leaving the box");

    // Several keys: Tab cycles through them.
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut app, "c");
    key(&mut app, KeyCode::Tab);
    let first = app.search.input.text().to_string();
    key(&mut app, KeyCode::Tab);
    let second = app.search.input.text().to_string();
    assert!(first.starts_with('c') && first.ends_with(':') && first != second, "{first} {second}");
    let s = render(&mut app, 140, 40);
    assert!(s.contains("Tab ▸"), "{s}");

    // Values: categories come from the catalog.
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    wait(&mut app, |a| !a.vocab.categories.is_empty());
    type_text(&mut app, "type:photo_r");
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.search.input.text(), "type:photo_raw");
    wait(&mut app, |a| a.search.hits.len() == 2);

    // A misspelled key gets a suggestion that Tab applies.
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut app, "sise:");
    let s = render(&mut app, 140, 40);
    assert!(s.contains("did you mean size:?"), "{s}");
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.search.input.text(), "size:");

    // F1: the full reference; Enter adds the selected filter to the query.
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut app, "iceland");
    key(&mut app, KeyCode::F(1));
    let s = render(&mut app, 140, 40);
    assert!(s.contains("Search filters") && s.contains("Photos & video"), "{s}");
    key(&mut app, KeyCode::Char('t')); // jump to the first key starting with t: type
    key(&mut app, KeyCode::Enter);
    assert!(app.modal.is_none());
    assert_eq!(app.search.input.text(), "iceland type:");
    assert!(app.search.focused);

    // Small terminals still draw.
    let s = render(&mut app, 80, 24);
    assert!(s.contains("drive-index"));
    key(&mut app, KeyCode::F(1));
    let s = render(&mut app, 80, 24);
    assert!(s.contains("Search filters"));
}
