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
    /// `claude stop <id>` — the roster's `x` on a worker row (after a confirm).
    /// The worker's conversation is kept; `claude attach <id>` reopens it.
    StopWorker { name: String, id: String },
}

pub enum CommandResult {
    Branches { gen: u64, info: Vec<BranchInfo> },
    Agents { gen: u64, sessions: Vec<SessionInfo> },
    WorkerStopped { name: String, ok: bool, detail: String },
}

pub struct BranchInfo {
    pub bot: BotId,
    pub branch: String,
    pub dirty: bool,
}

/// One row of `claude agents --json`. Tolerant: unknown fields ignored,
/// missing fields default — the schema is observed, not contractual.
///
/// Observed on 2.1.289: interactive rows carry `pid, cwd, kind, startedAt,
/// sessionId, name, status?`; background rows add `id` (the short id that
/// `attach`/`logs`/`stop`/`rm` take — the first 8 chars of `sessionId`) and
/// `state` (`working`, `done`, …).
#[derive(Deserialize, Default, Clone)]
pub struct SessionInfo {
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub name: String,
    /// Observed values: "busy", "idle"; the binary also carries "needs_input".
    #[serde(default)]
    pub status: String,
    /// Finer than `status` on background rows: "working", "done", …
    #[serde(default)]
    pub state: String,
    /// Why a `waiting` row waits, per the docs: "permission prompt" ·
    /// "input needed" · "dialog open" · … Empty unless waiting.
    #[serde(default, rename = "waitingFor")]
    pub waiting_for: String,
    /// The short id (background rows only).
    #[serde(default)]
    pub id: String,
    #[serde(default, rename = "sessionId")]
    pub session_id: String,
    /// "interactive" | "background".
    #[serde(default)]
    pub kind: String,
    /// Epoch millis.
    #[serde(default, rename = "startedAt")]
    pub started_at: u64,
}

impl SessionInfo {
    /// The id `claude attach|logs|stop|rm` take: the row's `id`, else the
    /// 8-char prefix of `sessionId` (the observed short-id shape).
    pub fn short_id(&self) -> Option<String> {
        if !self.id.is_empty() {
            return Some(self.id.clone());
        }
        let prefix: String = self.session_id.chars().take(8).collect();
        (prefix.len() == 8).then_some(prefix)
    }

    /// The status string to map: `status`, or `state` when `status` is empty.
    pub fn status_str(&self) -> &str {
        if self.status.is_empty() {
            &self.state
        } else {
            &self.status
        }
    }
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
        Command::StopWorker { name, id } => {
            let out = Proc::new("claude")
                .args(["stop", &id])
                .stdin(Stdio::null())
                .output();
            let (ok, detail) = match out {
                Ok(o) => {
                    let text = if o.status.success() { &o.stdout } else { &o.stderr };
                    let line = String::from_utf8_lossy(text)
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("")
                        .to_string();
                    (o.status.success(), line)
                }
                Err(e) => (false, e.to_string()),
            };
            Some(CommandResult::WorkerStopped { name, ok, detail })
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
        assert_eq!(v[1].short_id(), None, "no id and no sessionId → nothing to attach");
    }

    #[test]
    fn session_info_short_id_prefers_the_explicit_id() {
        let json = r#"[
          {"pid":1,"id":"2d1601e0","cwd":"/x/.claude/worktrees/probe","kind":"background","startedAt":1791479673254,"sessionId":"2d1601e0-2455-4f7c-8ae2-c575d7764010","name":"aviary-zz_probe-impl","status":"busy","state":"working"},
          {"pid":2,"cwd":"/x","kind":"interactive","startedAt":1,"sessionId":"dc808c13-1c3e-4652-9bd1-6e80a8e71511","name":"aviary-swift","status":"idle"},
          {"pid":3,"cwd":"/x","kind":"interactive","sessionId":"short","name":"x"},
          {"pid":4,"cwd":"/x","kind":"background","sessionId":"abc","name":"y","state":"done"}
        ]"#;
        let v: Vec<SessionInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(v[0].short_id().as_deref(), Some("2d1601e0"));
        assert_eq!(v[0].kind, "background");
        assert_eq!(v[0].started_at, 1791479673254);
        assert_eq!(v[1].short_id().as_deref(), Some("dc808c13"), "sessionId prefix fallback");
        assert_eq!(v[2].short_id(), None, "too short to be an id");
        assert_eq!(v[3].status_str(), "done", "state fills in for a missing status");
        assert_eq!(v[0].status_str(), "busy");
    }

    #[test]
    fn session_info_reads_waiting_for() {
        let json = r#"[
          {"name":"aviary-swift","status":"waiting","waitingFor":"permission prompt","sessionId":"dc808c13-1c3e"},
          {"name":"aviary-raven","status":"busy"}
        ]"#;
        let v: Vec<SessionInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(v[0].waiting_for, "permission prompt");
        assert_eq!(v[1].waiting_for, "", "absent reads as empty, never a guess");
    }

    #[test]
    fn applescript_quotes_escaped() {
        assert_eq!(applescript_escape(r#"say "hi"\now"#), r#"say \"hi\"\\now"#);
    }
}
