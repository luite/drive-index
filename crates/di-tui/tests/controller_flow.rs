//! End-to-end: the TUI app with a real controller reacting to an attached disk image.
//! macOS only, opt-in (it attaches and ejects an image):
//!
//!     DI_TEST_IMAGE=/path/to/test.dmg cargo test -p di-tui --test controller_flow -- --ignored --nocapture

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use di_scan::controller::{Controller, ControllerOptions, NewDrivePolicy, Policy};
use di_scan::device::After;
use di_tui::app::{App, ConnStatus, Modal};
use di_walk::WalkConfig;

fn render(app: &mut App) -> String {
    let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();
    term.draw(|f| di_tui::ui::draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait(app: &mut App, what: &str, cond: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cond(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
        app.tick();
    }
}

#[test]
#[ignore]
fn attach_prompt_scan_eject() {
    let image = std::env::var("DI_TEST_IMAGE").expect("set DI_TEST_IMAGE to a .dmg");
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    di_core::Catalog::open(&db).unwrap();
    let ctl = Controller::start(ControllerOptions {
        catalog_path: db.clone(),
        policy: Policy { new_drive: NewDrivePolicy::Ask, rescan_after_secs: 86_400, after: After::Eject, include_images: true },
        walk: WalkConfig { threads: 4, ..WalkConfig::default() },
    })
    .unwrap();
    let mut app = App::new(db, "test".into(), Some(ctl), None);

    let out = std::process::Command::new("hdiutil").args(["attach", "-nomount", &image]).output().unwrap();
    assert!(out.status.success(), "hdiutil attach failed");

    wait(&mut app, "the new-drive prompt", |a| matches!(a.modal, Some(Modal::NewDrive { .. })));
    let s = render(&mut app);
    println!("{s}");
    assert!(s.contains("New drive detected"));

    for c in "Test Image".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    wait(&mut app, "the scan to finish", |a| {
        a.connected.values().any(|c| matches!(c.status, ConnStatus::Done(_) | ConnStatus::Failed(_))) || a.connected.is_empty()
    });
    // Give the drive list a moment to pick up the ejection.
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        app.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    let s = render(&mut app);
    println!("{s}");
    assert!(s.contains("Test Image"), "the drive is listed under its new label");
    assert!(app.log.iter().any(|l| l.text.contains("Ejected")), "the drive was ejected");
}
