//! The birds themselves: one Claude Code session per bot, keyed by [`BotId`].
//!
//! Identity is the session NAME (`aviary-<id>`, via `--name`), which makes
//! resume trivial (`--resume aviary-<id>`) and makes every bird a stable
//! SendMessage target for its teammates.
//!
//! Status is layered, most-truthful-first:
//!   1. PTY output in the last 2s → Working (streaming IS activity),
//!   2. a fresh `claude agents --json` poll / hook event → busy · idle ·
//!      needs_input (the state a PTY can never show: blocked on a prompt),
//!   3. the output-recency heuristic as the fallback.
//!
//! Transitions feed unread dots and macOS notifications in the shell.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::command::SessionInfo;
use crate::config::{Bot, BotId, Config, State};
use crate::event::Event;
use crate::pty;

pub struct AgentSession {
    pub term: pty::Terminal,
    /// The last prompt aviary itself typed — the sidebar's activity line.
    pub last_prompt: Option<String>,
    pub spawned_at: Instant,
    /// Launched via `--resume`: if it dies within seconds, the named session
    /// is gone and the next launch must be fresh.
    resumed: bool,
    /// Fresh spawn not yet recorded in state — recorded only once the session
    /// SURVIVES [`MARK_AFTER`]. Marking at launch time was a trap: a spawn
    /// that crashed on boot left state claiming a session that never existed,
    /// and every later launch fell into claude's resume picker.
    fresh_unmarked: bool,
    exit_handled: bool,
}

/// The coarse state machine a bird moves through (drives transitions).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusKind {
    Working,
    NeedsInput,
    Done,
}

/// What the UI shows (Done carries its age).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BotStatus {
    NotStarted,
    Working,
    /// Blocked on a permission prompt / question — the state worth a banner.
    NeedsInput,
    /// Finished responding, waiting at its prompt for `secs`.
    Done(u64),
    Exited,
}

/// A status change worth reacting to (unread dot, notification).
pub struct Transition {
    pub id: BotId,
    pub from: StatusKind,
    pub to: StatusKind,
}

/// Map `claude agents --json` status strings; unknown strings read as Done
/// (quiet) rather than inventing urgency.
pub fn map_status_str(s: &str) -> StatusKind {
    let s = s.to_ascii_lowercase();
    if s == "busy" || s.contains("working") || s.contains("running") {
        StatusKind::Working
    } else if s.contains("input") || s.contains("waiting") || s.contains("blocked") {
        StatusKind::NeedsInput
    } else {
        StatusKind::Done
    }
}

/// Hook events → status kinds. `Stop` = finished; Notification subtypes that
/// mean "a human must act" → NeedsInput; `idle_prompt` just means quiet.
pub fn map_hook_event(event_name: &str, detail: &str) -> Option<StatusKind> {
    match event_name {
        "Stop" => Some(StatusKind::Done),
        "Notification" => {
            if detail.contains("idle") {
                Some(StatusKind::Done)
            } else {
                Some(StatusKind::NeedsInput)
            }
        }
        _ => None,
    }
}

/// How recent output must be to count as "working" with no poll data.
const WORKING_WINDOW: Duration = Duration::from_secs(5);
/// Output this fresh overrides a (laggy) poll — streaming IS working.
const OUTPUT_OVERRIDE: Duration = Duration::from_secs(2);
/// How long a poll/hook observation stays authoritative.
const POLL_TRUST: Duration = Duration::from_secs(15);
/// A resumed session dying this fast means "no such session" — fall back fresh.
const RESUME_FAIL_WINDOW: Duration = Duration::from_secs(4);
/// A fresh session must live this long before it counts as resumable.
const MARK_AFTER: Duration = Duration::from_secs(10);
/// A collaboration tag older than this is stale and stops showing.
const COLLAB_TTL: Duration = Duration::from_secs(10 * 60);

/// Who a bird is working WITH right now, as far as aviary brokered it.
#[derive(Clone)]
pub enum Collab {
    /// Sent a handoff to a teammate.
    Delegating(String),
    /// Received a handoff from a teammate.
    Receiving(String),
    /// Working a room thread.
    Room(String),
}

impl Collab {
    /// Sidebar tag: `↗ @raven` · `↘ @swift` · `⇄ #fly-calc`.
    pub fn tag(&self) -> String {
        match self {
            Collab::Delegating(who) => format!("↗ @{who}"),
            Collab::Receiving(who) => format!("↘ @{who}"),
            Collab::Room(room) => format!("⇄ #{room}"),
        }
    }
}

pub struct AgentStore {
    sessions: HashMap<BotId, AgentSession>,
    last_output: HashMap<BotId, Instant>,
    /// Latest poll/hook observation per bot.
    observed: HashMap<BotId, (StatusKind, Instant)>,
    /// When the observed kind last CHANGED (Done age on the chip).
    kind_since: HashMap<BotId, (StatusKind, Instant)>,
    unread: HashMap<BotId, bool>,
    collab: HashMap<BotId, (Collab, Instant)>,
    state: State,
}

impl AgentStore {
    pub fn new(cfg: &Config) -> AgentStore {
        AgentStore {
            sessions: HashMap::new(),
            last_output: HashMap::new(),
            observed: HashMap::new(),
            kind_since: HashMap::new(),
            unread: HashMap::new(),
            collab: HashMap::new(),
            state: State::load(&cfg.dir),
        }
    }

    pub fn set_collab(&mut self, id: &BotId, c: Collab) {
        self.collab.insert(id.clone(), (c, Instant::now()));
    }

    /// The user taking over (typing, direct messages) makes a bird
    /// independent again.
    pub fn clear_collab(&mut self, id: &BotId) {
        self.collab.remove(id);
    }

    pub fn collab(&self, id: &BotId) -> Option<&Collab> {
        self.collab
            .get(id)
            .filter(|(_, since)| since.elapsed() < COLLAB_TTL)
            .map(|(c, _)| c)
    }

    // ------------------------------------------------------------- unread

    pub fn mark_unread(&mut self, id: &BotId) {
        self.unread.insert(id.clone(), true);
    }

    pub fn clear_unread(&mut self, id: &BotId) {
        self.unread.remove(id);
    }

    pub fn is_unread(&self, id: &BotId) -> bool {
        self.unread.get(id).copied().unwrap_or(false)
    }

    // -------------------------------------------------------------- status

    pub fn get(&self, id: &BotId) -> Option<&AgentSession> {
        self.sessions.get(id)
    }

    pub fn get_mut(&mut self, id: &BotId) -> Option<&mut AgentSession> {
        self.sessions.get_mut(id)
    }

    pub fn running_count(&self) -> usize {
        self.sessions.values().filter(|s| s.term.is_running()).count()
    }

    /// Has this bot EVER had a session (drives resume vs the first-flight prompt)?
    pub fn has_session_record(&self, id: &BotId) -> bool {
        self.state.spawned_once(id)
    }

    pub fn routine_last_run(&self, bot: &BotId, routine_id: &str) -> Option<u64> {
        self.state.routine_last_run(bot, routine_id)
    }

    pub fn mark_routine_run(&mut self, cfg: &Config, bot: &BotId, routine_id: &str, epoch: u64) {
        self.state.mark_routine_run(&cfg.dir, bot, routine_id, epoch);
    }

    pub fn status(&self, id: &BotId) -> BotStatus {
        let Some(session) = self.sessions.get(id) else {
            return BotStatus::NotStarted;
        };
        if !session.term.is_running() {
            return BotStatus::Exited;
        }
        let output_age = self.last_output.get(id).map(|t| t.elapsed());
        // Streaming overrides everything — a laggy poll can say "idle" while
        // tokens are visibly arriving.
        if output_age.is_some_and(|a| a < OUTPUT_OVERRIDE) {
            return BotStatus::Working;
        }
        if let Some((kind, at)) = self.observed.get(id) {
            if at.elapsed() < POLL_TRUST {
                return match kind {
                    StatusKind::Working => BotStatus::Working,
                    StatusKind::NeedsInput => BotStatus::NeedsInput,
                    StatusKind::Done => BotStatus::Done(self.kind_age(id)),
                };
            }
        }
        // Fallback heuristic: recent output = working, else done-for-a-while.
        match output_age {
            Some(a) if a < WORKING_WINDOW => BotStatus::Working,
            Some(a) => BotStatus::Done(a.as_secs()),
            None => BotStatus::Working, // just spawned, first paint pending
        }
    }

    fn kind_age(&self, id: &BotId) -> u64 {
        self.kind_since
            .get(id)
            .map(|(_, t)| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    /// Record an observation; returns the transition if the kind changed.
    fn observe(&mut self, id: &BotId, kind: StatusKind) -> Option<Transition> {
        self.observed.insert(id.clone(), (kind, Instant::now()));
        match self.kind_since.get(id) {
            Some((prev, _)) if *prev == kind => None,
            prev => {
                let from = prev.map(|(k, _)| *k).unwrap_or(StatusKind::Working);
                self.kind_since.insert(id.clone(), (kind, Instant::now()));
                Some(Transition {
                    id: id.clone(),
                    from,
                    to: kind,
                })
            }
        }
    }

    /// Fold one `claude agents --json` poll in; returns status transitions.
    pub fn apply_poll(&mut self, cfg: &Config, sessions: &[SessionInfo]) -> Vec<Transition> {
        let mut out = Vec::new();
        for bot in &cfg.bots {
            // Only track birds whose PTY we actually hold — a user's own
            // session that happens to share a name is not ours to badge.
            if !self.sessions.get(&bot.id).is_some_and(|s| s.term.is_running()) {
                continue;
            }
            let name = bot.id.session_name();
            if let Some(info) = sessions.iter().find(|s| s.name == name) {
                let kind = map_status_str(&info.status);
                if let Some(t) = self.observe(&bot.id, kind) {
                    out.push(t);
                }
            }
        }
        out
    }

    /// Fold one hook event in (cwd attributes it to a bird's repo).
    pub fn apply_hook(
        &mut self,
        cfg: &Config,
        cwd: &str,
        event_name: &str,
        detail: &str,
    ) -> Option<Transition> {
        let kind = map_hook_event(event_name, detail)?;
        let cwd = Path::new(cwd);
        let bot = cfg.bots.iter().find(|b| b.repo_path() == cwd)?;
        if !self.sessions.get(&bot.id).is_some_and(|s| s.term.is_running()) {
            return None;
        }
        self.observe(&bot.id.clone(), kind)
    }

    // ------------------------------------------------------------ lifecycle

    /// Make sure the bot has a LIVE session, spawning or resuming as needed.
    /// `prompt`: rides argv on a launch, typed into a running session.
    pub fn ensure_running(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        if let Some(session) = self.sessions.get_mut(&bot.id) {
            if session.term.is_running() {
                if let Some(p) = prompt {
                    session.term.send(p.as_bytes());
                    session.term.send(b"\r");
                    session.last_prompt = Some(p.to_string());
                }
                return Ok(());
            }
            self.sessions.remove(&bot.id); // exited — replace it
        }
        let resume = self.state.spawned_once(&bot.id);
        self.launch(cfg, bot, prompt, tx, resume)
    }

    fn launch(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        prompt: Option<&str>,
        tx: &Sender<Event>,
        resume: bool,
    ) -> Result<()> {
        let repo = bot.repo_path();
        if !repo.is_dir() {
            bail!(
                "{}'s repo is missing: {} — fix it in {}",
                bot.name,
                repo.display(),
                cfg.dir.join("config.json").display()
            );
        }

        let persona = bot.persona_path(&cfg.dir);
        let settings = cfg.settings_file_for(bot)?;
        let args = launch_args(
            bot.id.session_name(),
            &persona,
            &settings,
            &cfg.dir,
            &cfg.mcp_config_path(),
            prompt,
            resume,
        );

        let term = pty::Terminal::spawn(bot.id.clone(), "claude", &args, &repo, 24, 80, tx.clone())?;
        self.sessions.insert(
            bot.id.clone(),
            AgentSession {
                term,
                last_prompt: prompt.map(str::to_string),
                spawned_at: Instant::now(),
                resumed: resume,
                fresh_unmarked: !resume,
                exit_handled: false,
            },
        );
        self.kind_since
            .insert(bot.id.clone(), (StatusKind::Working, Instant::now()));
        Ok(())
    }

    /// Deliberately abandon the current conversation: stop the session, forget
    /// the resume record, and hatch a brand-new one.
    pub fn fresh_start(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        self.sessions.remove(&bot.id);
        self.collab.remove(&bot.id);
        self.state.forget(&cfg.dir, &bot.id);
        self.launch(cfg, bot, prompt, tx, false)
    }

    /// Once a second: a fresh session that has survived [`MARK_AFTER`] becomes
    /// the bot's resumable session of record.
    pub fn tick(&mut self, cfg: &Config) {
        let ripe: Vec<BotId> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.fresh_unmarked && s.term.is_running() && s.spawned_at.elapsed() >= MARK_AFTER
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ripe {
            self.state.mark_spawned(&cfg.dir, &id);
            if let Some(s) = self.sessions.get_mut(&id) {
                s.fresh_unmarked = false;
            }
        }
    }

    /// Note PTY activity (from `Event::AgentOutput`). Returns a relaunch
    /// request when a `--resume` died instantly: the named session no longer
    /// exists, so the caller should `ensure_running` again — the state file
    /// has already been reset to force a fresh spawn.
    pub fn note_output(&mut self, cfg: &Config, id: &BotId) -> RelaunchHint {
        self.last_output.insert(id.clone(), Instant::now());
        let Some(session) = self.sessions.get_mut(id) else {
            return RelaunchHint::No;
        };
        if session.term.is_running() || session.exit_handled {
            return RelaunchHint::No;
        }
        session.exit_handled = true;
        if session.resumed && session.spawned_at.elapsed() < RESUME_FAIL_WINDOW {
            self.state.forget(&cfg.dir, id);
            self.sessions.remove(id);
            return RelaunchHint::FreshSpawn;
        }
        RelaunchHint::No
    }

    /// Dropping the master closes the PTY, which hangs up the child.
    pub fn stop(&mut self, id: &BotId) {
        self.sessions.remove(id);
        self.collab.remove(id);
        self.observed.remove(id);
    }
}

#[derive(PartialEq, Eq)]
pub enum RelaunchHint {
    No,
    /// A resumed session is gone — spawn fresh.
    FreshSpawn,
}

/// Argv for a bird's launch. ORDER IS LOAD-BEARING: `--mcp-config` and
/// `--add-dir` are VARIADIC in the claude CLI and consume values until the
/// next flag — unterminated, they swallow the positional prompt as another
/// config path ("MCP config file not found: <repo>/<prompt text>"). Each is
/// therefore followed by another flag, and the positional prompt only ever
/// follows a single-value flag.
fn launch_args(
    session: String,
    persona: &std::path::Path,
    settings: &std::path::Path,
    cfg_dir: &std::path::Path,
    mcp: &std::path::Path,
    prompt: Option<&str>,
    resume: bool,
) -> Vec<String> {
    let mut args: Vec<String> = if resume {
        vec!["--resume".into(), session]
    } else {
        vec!["--name".into(), session]
    };
    args.extend([
        // The Linear + Figma hooks, independent of repo-level MCP config.
        "--mcp-config".into(),
        mcp.display().to_string(),
        // Rooms + handoffs live under the config dir; make writes there
        // first-class instead of out-of-scope.
        "--add-dir".into(),
        cfg_dir.display().to_string(),
        // Per-session Stop/Notification hooks (+ optional permission allows) —
        // the source of truthful "done / needs input" signals.
        "--settings".into(),
        settings.display().to_string(),
        "--append-system-prompt-file".into(),
        persona.display().to_string(),
    ]);
    if let Some(p) = prompt {
        // The prompt rides argv: typing into a BOOTING pty races the child's
        // first paint, but a positional prompt cannot be dropped.
        args.push(p.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn status_strings_map_conservatively() {
        assert_eq!(map_status_str("busy"), StatusKind::Working);
        assert_eq!(map_status_str("idle"), StatusKind::Done);
        assert_eq!(map_status_str("needs_input"), StatusKind::NeedsInput);
        assert_eq!(map_status_str("waiting_for_input"), StatusKind::NeedsInput);
        // Unknown future value: quiet, never a false alarm.
        assert_eq!(map_status_str("compacting"), StatusKind::Done);
    }

    #[test]
    fn hook_events_map() {
        assert_eq!(map_hook_event("Stop", ""), Some(StatusKind::Done));
        assert_eq!(
            map_hook_event("Notification", "permission_prompt"),
            Some(StatusKind::NeedsInput)
        );
        assert_eq!(
            map_hook_event("Notification", "idle_prompt"),
            Some(StatusKind::Done)
        );
        assert_eq!(map_hook_event("PreToolUse", "x"), None);
    }

    /// The regression this guards: a variadic flag directly before the
    /// positional prompt eats it ("MCP config file not found: …<prompt>").
    #[test]
    fn variadic_flags_never_precede_the_prompt() {
        let args = launch_args(
            "aviary-swift".into(),
            Path::new("/cfg/birds/swift.md"),
            Path::new("/cfg/settings/swift.json"),
            Path::new("/cfg"),
            Path::new("/cfg/mcp.json"),
            Some("You've just been perched."),
            false,
        );
        assert_eq!(args.last().map(String::as_str), Some("You've just been perched."));
        for variadic in ["--mcp-config", "--add-dir"] {
            let i = args.iter().position(|a| a == variadic).unwrap();
            assert!(
                args[i + 2].starts_with("--"),
                "{variadic} must be followed by exactly one value then a flag"
            );
        }
        let i = args.iter().position(|a| a == "--append-system-prompt-file").unwrap();
        assert_eq!(args[i + 1], "/cfg/birds/swift.md");
        assert_eq!(i + 2, args.len() - 1);
        let s = args.iter().position(|a| a == "--settings").unwrap();
        assert_eq!(args[s + 1], "/cfg/settings/swift.json");
    }

    #[test]
    fn resume_swaps_name_for_resume_and_tolerates_no_prompt() {
        let args = launch_args(
            "aviary-raven".into(),
            Path::new("/cfg/birds/raven.md"),
            Path::new("/cfg/settings/raven.json"),
            Path::new("/cfg"),
            Path::new("/cfg/mcp.json"),
            None,
            true,
        );
        assert_eq!(args[0], "--resume");
        assert_eq!(args[1], "aviary-raven");
        assert_eq!(args.last().map(String::as_str), Some("/cfg/birds/raven.md"));
    }
}
