//! `aviary` — a roster of repo-resident Claude Code birds.
//!
//! Each bot is a real `claude` session on its own PTY, living in its own repo,
//! named `aviary-<id>` so it resumes across restarts and so its teammates can
//! reach it with SendMessage. This binary is a cockpit: it spawns, names,
//! displays, and connects the birds — the agents do the actual work.

mod action;
mod agent_store;
mod app;
mod command;
mod components;
mod config;
mod event;
mod keymap;
mod overlays;
mod prompts;
mod pty;
mod room;
mod shared;
mod ui;
mod widgets;

use anyhow::Result;

fn main() -> Result<()> {
    install_panic_hook();
    let terminal = ratatui::init();
    mouse_capture(true);
    let result = app::run(terminal);
    mouse_capture(false);
    ratatui::restore();
    result
}

fn mouse_capture(on: bool) {
    use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    let mut out = std::io::stdout();
    let _ = if on {
        crossterm::execute!(out, EnableMouseCapture)
    } else {
        crossterm::execute!(out, DisableMouseCapture)
    };
}

/// Panicking inside raw mode leaves the user staring at a terminal that no
/// longer echoes. Restore the screen first, then let the default hook print
/// its report into a terminal that can actually display it.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        mouse_capture(false);
        ratatui::restore();
        previous(info);
    }));
}
