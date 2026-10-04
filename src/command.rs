//! Background work. Subprocess spawns never run on the UI thread.
//!
//! The executor is thread-per-command (the workload is a few short-lived
//! subprocess calls, not a job queue); results come home as `Event::Done` and
//! are fanned to components, which drop stale generations themselves.
//! Fire-and-forget commands (notifications, `open`) produce no result.

use std::path::PathBuf;
use std::process::{Command as Proc, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use serde::Deserialize;

use crate::config::BotId;
use crate::event::Event;

pub enum Command {
    /// Branch + dirty state for every bot's repo — the sidebar's `⎇` line.
    LoadBranches { gen: u64, repos: Vec<(BotId, PathBuf)> },
    /// `claude agents --json` — the authoritative session states.
    PollAgents { gen: u64 },
    /// macOS banner via osascript ("finishes or needs input").
    Notify { title: String, body: String },
    /// `open <path>` — personas in the user's editor, handoff briefs, etc.
    Open { path: PathBuf },
}

pub enum CommandResult {
    Branches { gen: u64, info: Vec<BranchInfo> },
    Agents { gen: u64, sessions: Vec<SessionInfo> },
}

pub struct BranchInfo {
    pub bot: BotId,
    pub branch: String,
    pub dirty: bool,
}

/// One row of `claude agents --json`. Tolerant: unknown fields ignored,
/// missing fields default — the schema is observed, not contractual.
#[derive(Deserialize, Default)]
pub struct SessionInfo {
    /// Kept for cwd-based attribution if session naming ever changes shape.
    #[serde(default)]
    #[allow(dead_code)]
    pub cwd: String,
    #[serde(default)]
    pub name: String,
    /// Observed values: "busy", "idle"; the binary also carries "needs_input".
    #[serde(default)]
    pub status: String,
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
            if let Some(result) = execute(cmd) {
                let _ = tx.send(Event::Done(Box::new(result)));
            }
        });
    }
}

fn execute(cmd: Command) -> Option<CommandResult> {
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
            Some(CommandResult::Branches { gen, info })
        }
        Command::PollAgents { gen } => {
            let sessions = Proc::new("claude")
                .args(["agents", "--json"])
                .stdin(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| serde_json::from_slice::<Vec<SessionInfo>>(&o.stdout).ok())
                .unwrap_or_default();
            Some(CommandResult::Agents { gen, sessions })
        }
        Command::Notify { title, body } => {
            let script = format!(
                "display notification \"{}\" with title \"{}\" sound name \"Glass\"",
                applescript_escape(&body),
                applescript_escape(&title),
            );
            let _ = Proc::new("osascript").args(["-e", &script]).status();
            None
        }
        Command::Open { path } => {
            let _ = Proc::new("open").arg(path).status();
            None
        }
    }
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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

    #[test]
    fn session_info_tolerates_unknown_and_missing_fields() {
        let json = r#"[
          {"pid":1,"cwd":"/x","kind":"interactive","startedAt":1,"sessionId":"s","name":"aviary-swift","status":"busy","future_field":true},
          {"cwd":"/y"}
        ]"#;
        let v: Vec<SessionInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(v[0].name, "aviary-swift");
        assert_eq!(v[0].status, "busy");
        assert_eq!(v[1].name, "");
    }

    #[test]
    fn applescript_quotes_escaped() {
        assert_eq!(applescript_escape(r#"say "hi"\now"#), r#"say \"hi\"\\now"#);
    }
}
