//! The interactive terminal UI (PLAN.md §7).

pub mod app;
pub mod input;
pub mod search_worker;
pub mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use di_scan::controller::{Controller, ControllerOptions};

pub struct TuiOptions {
    pub catalog_path: PathBuf,
    pub host: String,
    /// Controller settings; `None` runs as a viewer.
    pub controller: Option<ControllerOptions>,
    /// Why this instance is a viewer (shown in the status line).
    pub viewer_reason: Option<String>,
}

pub fn run(opts: TuiOptions) -> Result<()> {
    let ctl = match opts.controller {
        Some(c) => Some(Controller::start(c)?),
        None => None,
    };
    let mut app = app::App::new(opts.catalog_path, opts.host, ctl, opts.viewer_reason);
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        while !app.quit {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                    Event::Resize(..) => {}
                    _ => {}
                }
            }
            app.tick();
        }
        Ok(())
    })();
    ratatui::restore();
    // Stop the controller: a running scan is interrupted and resumes next time.
    if let Some(c) = app.ctl.take() {
        let _ = c.shutdown();
    }
    result
}
