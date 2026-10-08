//! `aviary` — a roster of repo-resident Claude Code birds.
//!
//! Each bot is a real `claude` session on its own PTY, living in its own repo,
//! named `aviary-<id>` so it resumes across restarts and so its teammates can
//! reach it with SendMessage. This binary is a cockpit: it spawns, names,
//! displays, and connects the birds — the agents do the actual work.

mod action;
mod agent_store;
mod app;
mod clipboard;
mod command;
mod components;
mod config;
mod doctor;
mod event;
mod events;
mod http;
mod keymap;
mod markdown;
mod overlays;
mod prompts;
mod pty;
mod room;
mod routine;
mod shared;
mod ui;
mod widgets;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Non-interactive flags first: Homebrew's `test do` and friends need a way
    // to exercise the binary without a TTY.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("aviary {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    // Hook mode: runs inside every bird's Stop/Notification hook. Fast, silent,
    // and NEVER the TUI — it must not fight the session for the terminal.
    // `--session <name>` tags the event with the emitting session, the only
    // attribution that can tell a bird's tabs apart (they share a cwd).
    if args.iter().any(|a| a == "--hook") {
        let session = args
            .iter()
            .position(|a| a == "--session")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        let dir = config::default_dir()?;
        let _ = events::append_from_stdin(&dir.join("events.jsonl"), session);
        return Ok(());
    }

    if args.iter().any(|a| a == "doctor" || a == "--doctor") {
        return doctor::run();
    }

    install_panic_hook();
    let terminal = ratatui::init();
    mouse_capture(true);
    let result = app::run(terminal);
    mouse_capture(false);
    ratatui::restore();
    result
}

fn print_help() {
    println!(
        "aviary {}\n{}\n\nUSAGE:\n  aviary            open the cockpit\n  aviary doctor     check the environment (claude on PATH, config dir, ...)\n  aviary --hook     [internal] Claude Code Stop/Notification hook sink\n  aviary --version  print the version\n  aviary --help     this text",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_DESCRIPTION"),
    );
}

pub(crate) fn mouse_capture(on: bool) {
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
