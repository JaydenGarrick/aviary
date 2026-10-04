//! Background work. Subprocess spawns never run on the UI thread.
//!
//! The executor is thread-per-command (the workload is a few short-lived git
//! calls, not a job queue); every result comes home as `Event::Done` and is
//! fanned to components, which drop stale generations themselves.

use std::path::PathBuf;
use std::process::{Command as Proc, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use crate::config::BotId;
use crate::event::Event;

pub enum Command {
    /// Branch + dirty state for every bot's repo — the roster's `⎇` column.
    LoadBranches { gen: u64, repos: Vec<(BotId, PathBuf)> },
}

pub enum CommandResult {
    Branches { gen: u64, info: Vec<BranchInfo> },
}

pub struct BranchInfo {
    pub bot: BotId,
    pub branch: String,
    pub dirty: bool,
}

/// A slot for data loaded off-thread. `in_flight` keeps old data on screen
/// during a refresh instead of flashing back to a loading state.
pub struct Slot<T> {
    pub data: Option<T>,
    pub in_flight: bool,
    pub gen: u64,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            data: None,
            in_flight: false,
            gen: 0,
        }
    }
}

impl<T> Slot<T> {
    /// Bump the generation and mark a request in flight; returns the gen to
    /// stamp on the command.
    pub fn begin(&mut self) -> u64 {
        self.gen += 1;
        self.in_flight = true;
        self.gen
    }

    /// Accept a result only if it answers the latest request.
    pub fn accept(&mut self, gen: u64, data: T) -> bool {
        if gen != self.gen {
            return false; // stale — a newer request is in flight
        }
        self.data = Some(data);
        self.in_flight = false;
        true
    }
}

pub struct Executor {
    tx: Sender<Event>,
}

impl Executor {
    pub fn new(tx: Sender<Event>) -> Executor {
        Executor { tx }
    }

    pub fn run(&self, cmd: Command) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = execute(cmd);
            let _ = tx.send(Event::Done(Box::new(result)));
        });
    }
}

fn execute(cmd: Command) -> CommandResult {
    match cmd {
        Command::LoadBranches { gen, repos } => {
            let info = repos
                .into_iter()
                .map(|(bot, repo)| BranchInfo {
                    branch: git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])
                        .unwrap_or_else(|| "—".into()),
                    dirty: git(&repo, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty()),
                    bot,
                })
                .collect();
            CommandResult::Branches { gen, info }
        }
    }
}

fn git(repo: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Proc::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_drops_stale_generations() {
        let mut slot: Slot<u32> = Slot::default();
        let g1 = slot.begin();
        let g2 = slot.begin(); // a newer request supersedes g1
        assert!(!slot.accept(g1, 1), "stale result must be dropped");
        assert_eq!(slot.data, None);
        assert!(slot.accept(g2, 2));
        assert_eq!(slot.data, Some(2));
        assert!(!slot.in_flight);
    }

    #[test]
    fn slot_keeps_old_data_during_refresh() {
        let mut slot: Slot<u32> = Slot::default();
        let g = slot.begin();
        slot.accept(g, 7);
        slot.begin();
        assert_eq!(slot.data, Some(7), "old data stays visible mid-refresh");
        assert!(slot.in_flight);
    }
}
