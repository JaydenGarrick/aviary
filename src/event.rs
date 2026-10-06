//! Everything that can wake the render loop arrives on one channel.
//!
//! The workload is a handful of long-lived streams (keyboard, a 1s ticker, one
//! PTY reader per bird, command results), not thousands of tasks — plain
//! threads over `mpsc`, no async runtime.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crossterm::event::{self as term, Event as TermEvent, KeyEvent, KeyEventKind, MouseEvent};

use crate::command::CommandResult;
use crate::config::SessionKey;

/// A wake-up for the render loop.
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// Redraw only — ratatui re-measures the frame on the next `draw`.
    Resize,
    /// A bird's embedded terminal produced output, or its child exited.
    /// Tagged per session: the status chips key off exactly who spoke.
    AgentOutput(SessionKey),
    /// A background command finished (see [`crate::command`]).
    Done(Box<CommandResult>),
    /// An authenticated inbound webhook (see [`crate::http`]).
    Webhook(Box<crate::http::Webhook>),
    /// Once a second — drives room watching, status chips, and elapsed timers.
    Tick,
}

/// Start the input + ticker threads and hand back the shared channel.
///
/// The sender comes back alongside the receiver because every later producer
/// (PTY readers, the command executor) clones it.
pub fn channel() -> (Sender<Event>, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    spawn_input(tx.clone());
    spawn_ticker(tx.clone());
    (tx, rx)
}

fn spawn_ticker(tx: Sender<Event>) {
    thread::spawn(move || loop {
        thread::sleep(std::time::Duration::from_secs(1));
        if tx.send(Event::Tick).is_err() {
            break;
        }
    });
}

fn spawn_input(tx: Sender<Event>) {
    thread::spawn(move || {
        // Blocking read: no polling loop, no idle CPU burn.
        while let Ok(event) = term::read() {
            let out = match event {
                // Windows reports press AND release; act on press only.
                TermEvent::Key(k) if k.kind == KeyEventKind::Press => Event::Key(k),
                TermEvent::Mouse(m) => Event::Mouse(m),
                TermEvent::Resize(_, _) => Event::Resize,
                _ => continue,
            };
            if tx.send(out).is_err() {
                break;
            }
        }
    });
}
