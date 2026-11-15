//! The interactive terminal UI (PLAN.md §7).

pub mod app;
pub mod assist;
pub mod input;
pub mod screens;
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
    /// Capture the mouse (disables the terminal's own text selection while running).
    pub mouse: bool,
}

pub fn run(opts: TuiOptions) -> Result<()> {
    let ctl = match opts.controller {
        Some(c) => Some(Controller::start(c)?),
        None => None,
    };
    let mut app = app::App::new(opts.catalog_path, opts.host, ctl, opts.viewer_reason);
    let mut terminal = ratatui::init();
    if opts.mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::EnableMouseCapture);
        // ratatui's panic hook restores the terminal but not mouse capture.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::DisableMouseCapture);
            prev(info);
        }));
    }
    let result = (|| -> Result<()> {
        while !app.quit {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(100))? {
                // Drain everything queued (fast scrolling sends bursts).
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                        Event::Mouse(m) => app.on_mouse(m),
                        _ => {}
                    }
                    if app.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            app.tick();
        }
        Ok(())
    })();
    if opts.mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::DisableMouseCapture);
    }
    ratatui::restore();
    // Stop the controller: a running scan is interrupted and resumes next time.
    if let Some(c) = app.ctl.take() {
        let _ = c.shutdown();
    }
    result
}
