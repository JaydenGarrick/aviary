//! The birds themselves: Claude Code sessions keyed by [`SessionKey`] — the
//! bird's primary session (tab 1) plus any human-driven extra tabs.
//!
//! Identity is the session NAME (`aviary-<id>`, tabs `aviary-<id>.<n>`, via
//! `--name`), which makes resume trivial (`--resume <name>`) and makes every
//! bird a stable SendMessage target for its teammates. External signals
//! (rooms, handoffs, routines, webhooks) only ever address the primary.
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
use crate::config::{Bot, BotId, Config, SessionKey, State};
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    pub key: SessionKey,
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
    sessions: HashMap<SessionKey, AgentSession>,
    last_output: HashMap<SessionKey, Instant>,
    /// Latest poll/hook observation per session.
    observed: HashMap<SessionKey, (StatusKind, Instant)>,
    /// When the observed kind last CHANGED (Done age on the chip).
    kind_since: HashMap<SessionKey, (StatusKind, Instant)>,
    // Unread and collab are deliberately per-BIRD: the roster card is the unit
    // of attention, whatever tab produced the signal.
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

    pub fn get(&self, key: &SessionKey) -> Option<&AgentSession> {
        self.sessions.get(key)
    }

    pub fn get_mut(&mut self, key: &SessionKey) -> Option<&mut AgentSession> {
        self.sessions.get_mut(key)
    }

    pub fn running_count(&self) -> usize {
        self.sessions.values().filter(|s| s.term.is_running()).count()
    }

    /// Has this session EVER existed (drives resume vs the first-flight prompt)?
    pub fn has_session_record(&self, key: &SessionKey) -> bool {
        self.state.spawned_once(&key.state_key())
    }

    /// The tab's human label, if the user set one (display only).
    pub fn tab_name(&self, key: &SessionKey) -> Option<&str> {
        self.state.tab_name(&key.state_key())
    }

    /// Label a tab for the strip; an empty name clears the label.
    pub fn set_tab_name(&mut self, cfg: &Config, key: &SessionKey, name: &str) {
        self.state.set_tab_name(&cfg.dir, &key.state_key(), name);
    }

    /// Every tab the strip should show: resumable records ∪ live sessions,
    /// sorted; tab 1 is always present.
    pub fn tabs(&self, id: &BotId) -> Vec<u8> {
        let mut tabs = self.state.spawned_tabs(id);
        tabs.extend(self.sessions.keys().filter(|k| k.bot == *id).map(|k| k.tab));
        tabs.push(1);
        tabs.sort_unstable();
        tabs.dedup();
        tabs
    }

    pub fn routine_last_run(&self, bot: &BotId, routine_id: &str) -> Option<u64> {
        self.state.routine_last_run(bot, routine_id)
    }

    pub fn mark_routine_run(&mut self, cfg: &Config, bot: &BotId, routine_id: &str, epoch: u64) {
        self.state.mark_routine_run(&cfg.dir, bot, routine_id, epoch);
    }

    /// The roster chip: the most attention-worthy status across the bird's
    /// sessions — NeedsInput > Working > Done (youngest) > Exited.
    pub fn status(&self, id: &BotId) -> BotStatus {
        self.status_tabbed(id).0
    }

    /// The roster chip plus WHICH tab it is reporting — named only when the
    /// bird has more than one session, so single-tab birds stay clean.
    pub fn status_tabbed(&self, id: &BotId) -> (BotStatus, Option<u8>) {
        aggregate_status(
            self.sessions
                .keys()
                .filter(|k| k.bot == *id)
                .map(|k| (k.tab, self.status_key(k))),
        )
    }

    /// One session's status, layered most-truthful-first (see module doc).
    pub fn status_key(&self, key: &SessionKey) -> BotStatus {
        let Some(session) = self.sessions.get(key) else {
            return BotStatus::NotStarted;
        };
        if !session.term.is_running() {
            return BotStatus::Exited;
        }
        let output_age = self.last_output.get(key).map(|t| t.elapsed());
        // Streaming overrides everything — a laggy poll can say "idle" while
        // tokens are visibly arriving.
        if output_age.is_some_and(|a| a < OUTPUT_OVERRIDE) {
            return BotStatus::Working;
        }
        if let Some((kind, at)) = self.observed.get(key) {
            if at.elapsed() < POLL_TRUST {
                return match kind {
                    StatusKind::Working => BotStatus::Working,
                    StatusKind::NeedsInput => BotStatus::NeedsInput,
                    StatusKind::Done => BotStatus::Done(self.kind_age(key)),
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

    fn kind_age(&self, key: &SessionKey) -> u64 {
        self.kind_since
            .get(key)
            .map(|(_, t)| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    /// Record an observation; returns the transition if the kind changed.
    fn observe(&mut self, key: &SessionKey, kind: StatusKind) -> Option<Transition> {
        self.observed.insert(key.clone(), (kind, Instant::now()));
        match self.kind_since.get(key) {
            Some((prev, _)) if *prev == kind => None,
            prev => {
                let from = prev.map(|(k, _)| *k).unwrap_or(StatusKind::Working);
                self.kind_since.insert(key.clone(), (kind, Instant::now()));
                Some(Transition {
                    key: key.clone(),
                    from,
                    to: kind,
                })
            }
        }
    }

    /// Is any live session badging as Working right now? Drives the run
    /// loop's frame-rate wakeup so the spinner animates smoothly — the loop
    /// stays fully blocking when nothing spins.
    pub fn has_working(&self) -> bool {
        self.sessions
            .keys()
            .any(|k| matches!(self.status_key(k), BotStatus::Working))
    }

    /// Only badge sessions whose PTY we actually hold — a user's own session
    /// that happens to share a name is not ours.
    fn live_keys(&self) -> Vec<SessionKey> {
        self.sessions
            .iter()
            .filter(|(_, s)| s.term.is_running())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Fold one `claude agents --json` poll in; returns status transitions.
    pub fn apply_poll(&mut self, sessions: &[SessionInfo]) -> Vec<Transition> {
        poll_matches(&self.live_keys(), sessions)
            .into_iter()
            .filter_map(|(key, kind)| self.observe(&key, kind))
            .collect()
    }

    /// Fold one hook event in. A new-format event names its session
    /// (`aviary_session`, injected by `aviary --hook --session <name>`) and
    /// resolves to that exact key; an old or foreign payload falls back to
    /// cwd matching, which can only mean the PRIMARY — tabs share the repo.
    pub fn apply_hook(
        &mut self,
        cfg: &Config,
        aviary_session: Option<&str>,
        cwd: &str,
        event_name: &str,
        detail: &str,
    ) -> Option<Transition> {
        let kind = map_hook_event(event_name, detail)?;
        let key = resolve_hook_key(&cfg.bots, &self.live_keys(), cwd, aviary_session)?;
        self.observe(&key, kind)
    }

    // ------------------------------------------------------------ lifecycle

    /// Make sure ONE session of the bot is live, spawning or resuming as
    /// needed. `prompt`: rides argv on a launch, typed into a running session.
    /// External signals only ever arrive with tab 1 (via `Shared::boot_bot`);
    /// extra tabs are human-driven only.
    pub fn ensure_running_key(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        tab: u8,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let key = SessionKey { bot: bot.id.clone(), tab };
        if let Some(session) = self.sessions.get_mut(&key) {
            if session.term.is_running() {
                if let Some(p) = prompt {
                    // send_line, never a trailing \r in the same burst — the
                    // child's paste detection would swallow the submit.
                    session.term.send_line(p);
                    session.last_prompt = Some(p.to_string());
                }
                return Ok(());
            }
            self.sessions.remove(&key); // exited — replace it
        }
        let resume = self.state.spawned_once(&key.state_key());
        self.launch(cfg, bot, &key, prompt, tx, resume)
    }

    fn launch(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        key: &SessionKey,
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
        let settings = cfg.settings_file_for(bot, key)?;
        let args = launch_args(
            key.session_name(),
            &persona,
            &settings,
            &cfg.dir,
            &cfg.mcp_config_path(),
            prompt,
            resume,
        );

        let term = pty::Terminal::spawn(key.clone(), "claude", &args, &repo, 24, 80, tx.clone())?;
        self.sessions.insert(
            key.clone(),
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
            .insert(key.clone(), (StatusKind::Working, Instant::now()));
        Ok(())
    }

    /// Deliberately abandon the PRIMARY conversation: stop the session, forget
    /// the resume record, and hatch a brand-new one. Tabs are untouched —
    /// they fresh-start individually via [`AgentStore::fresh_key`].
    pub fn fresh_start(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        self.collab.remove(&bot.id);
        self.fresh_key(cfg, bot, 1, prompt, tx)
    }

    /// Abandon ONE tab's conversation and hatch a brand-new session under the
    /// same name — also the escape hatch when a resume record points at a
    /// session claude no longer has (the resume picker dead end). The tab's
    /// label survives; only the conversation is new.
    pub fn fresh_key(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        tab: u8,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let key = SessionKey { bot: bot.id.clone(), tab };
        self.sessions.remove(&key);
        self.state.forget(&cfg.dir, &key.state_key());
        self.launch(cfg, bot, &key, prompt, tx, false)
    }

    /// A room is forming around this bird: abandon its PRIMARY conversation
    /// WITHOUT relaunching. The room's first dispatch hatches it fresh with
    /// the notify prompt ON ARGV — spawning here would turn that first
    /// message into keystrokes at a booting PTY, the race the argv rule
    /// exists for. Tabs are untouched (rooms never address them). Returns
    /// true when there was something to drop: a live session or a record.
    pub fn reset_primary(&mut self, cfg: &Config, id: &BotId) -> bool {
        let key = SessionKey::primary(id.clone());
        let was_live = self.sessions.remove(&key).is_some(); // drop = hang up
        self.observed.remove(&key);
        self.last_output.remove(&key);
        self.kind_since.remove(&key);
        self.collab.remove(id);
        let had_record = self.state.spawned_once(&key.state_key());
        self.state.forget(&cfg.dir, &key.state_key());
        was_live || had_record
    }

    /// Once a second: a fresh session that has survived [`MARK_AFTER`] becomes
    /// that tab's resumable session of record.
    pub fn tick(&mut self, cfg: &Config) {
        let ripe: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.fresh_unmarked && s.term.is_running() && s.spawned_at.elapsed() >= MARK_AFTER
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in ripe {
            self.state.mark_spawned(&cfg.dir, &key.state_key());
            if let Some(s) = self.sessions.get_mut(&key) {
                s.fresh_unmarked = false;
            }
        }
    }

    /// Note PTY activity (from `Event::AgentOutput`). Returns a relaunch
    /// request when a `--resume` died instantly: the named session no longer
    /// exists, so the caller should boot THIS KEY again — the state file
    /// has already been reset to force a fresh spawn.
    pub fn note_output(&mut self, cfg: &Config, key: &SessionKey) -> RelaunchHint {
        self.last_output.insert(key.clone(), Instant::now());
        let Some(session) = self.sessions.get_mut(key) else {
            return RelaunchHint::No;
        };
        if session.term.is_running() || session.exit_handled {
            return RelaunchHint::No;
        }
        session.exit_handled = true;
        if session.resumed && session.spawned_at.elapsed() < RESUME_FAIL_WINDOW {
            self.state.forget(&cfg.dir, &key.state_key());
            self.sessions.remove(key);
            return RelaunchHint::FreshSpawn;
        }
        RelaunchHint::No
    }

    /// Stop EVERY session of the bird (the roster's `x`). Dropping the master
    /// closes the PTY, which hangs up the child.
    pub fn stop(&mut self, id: &BotId) {
        self.sessions.retain(|k, _| k.bot != *id);
        self.observed.retain(|k, _| k.bot != *id);
        self.collab.remove(id);
    }

    /// Stop one tab's session only; the bird's other sessions keep flying.
    pub fn stop_key(&mut self, key: &SessionKey) {
        self.sessions.remove(key);
        self.observed.remove(key);
    }

    /// Close a tab: stop it, forget its resume record and label, so the strip
    /// entry disappears. The claude session itself survives, unreferenced.
    pub fn close_tab(&mut self, cfg: &Config, key: &SessionKey) {
        self.stop_key(key);
        self.last_output.remove(key);
        self.kind_since.remove(key);
        self.state.forget(&cfg.dir, &key.state_key());
        self.state.set_tab_name(&cfg.dir, &key.state_key(), "");
    }

    /// A bird leaving the roster: stop all its sessions and drop all
    /// bookkeeping. The claude sessions themselves survive —
    /// `claude --resume aviary-<id>` works from any terminal.
    pub fn release(&mut self, cfg: &Config, id: &BotId) {
        self.stop(id);
        self.last_output.retain(|k, _| k.bot != *id);
        self.kind_since.retain(|k, _| k.bot != *id);
        self.unread.remove(id);
        self.state.forget_bot(&cfg.dir, id);
    }
}

/// Aggregate per-session statuses into the roster chip: attention first.
/// Returns the winning status and its tab — the tab is `Some` only when more
/// than one session contributed, since "which tab" is noise on a lone bird.
fn aggregate_status(
    statuses: impl Iterator<Item = (u8, BotStatus)>,
) -> (BotStatus, Option<u8>) {
    use BotStatus::*;
    fn rank(s: &BotStatus) -> u8 {
        match s {
            NeedsInput => 4,
            Working => 3,
            Done(_) => 2,
            Exited => 1,
            NotStarted => 0,
        }
    }
    let mut n = 0usize;
    let folded = statuses.fold((NotStarted, 1u8), |(acc, at), (tab, s)| {
        n += 1;
        match (acc, s) {
            // Two finished tabs: the chip shows the FRESHEST completion.
            (Done(a), Done(b)) if b < a => (Done(b), tab),
            (Done(a), Done(_)) => (Done(a), at),
            (acc, s) if rank(&s) > rank(&acc) => (s, tab),
            (acc, _) => (acc, at),
        }
    });
    (folded.0, (n > 1).then_some(folded.1))
}

/// Match poll rows to live keys by EXACT session name — `aviary-swift.2`
/// never badges `aviary-swift`, and a foreign `aviary-swiftly` matches neither.
fn poll_matches(live: &[SessionKey], sessions: &[SessionInfo]) -> Vec<(SessionKey, StatusKind)> {
    live.iter()
        .filter_map(|key| {
            let name = key.session_name();
            sessions
                .iter()
                .find(|s| s.name == name)
                .map(|s| (key.clone(), map_status_str(&s.status)))
        })
        .collect()
}

/// Which session a hook event belongs to. A named event resolves exactly or
/// not at all — never mis-badge; an unnamed one falls back to cwd → primary.
fn resolve_hook_key(
    bots: &[Bot],
    live: &[SessionKey],
    cwd: &str,
    aviary_session: Option<&str>,
) -> Option<SessionKey> {
    if let Some(name) = aviary_session {
        let key = SessionKey::parse_session_name(name)?;
        return (bots.iter().any(|b| b.id == key.bot) && live.contains(&key)).then_some(key);
    }
    let cwd = Path::new(cwd);
    let bot = bots.iter().find(|b| b.repo_path() == cwd)?;
    let key = SessionKey::primary(bot.id.clone());
    live.contains(&key).then_some(key)
}

/// The tab `T` opens next: the lowest unused number ≥ 2.
pub fn lowest_free_tab(tabs: &[u8]) -> u8 {
    (2..u8::MAX).find(|n| !tabs.contains(n)).unwrap_or(u8::MAX)
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

    fn key(id: &str, tab: u8) -> SessionKey {
        SessionKey { bot: BotId(id.into()), tab }
    }

    #[test]
    fn aggregate_status_shows_the_most_attention_worthy_tab() {
        use BotStatus::*;
        let agg = |v: Vec<(u8, BotStatus)>| aggregate_status(v.into_iter());
        assert_eq!(agg(vec![]), (NotStarted, None));
        // A lone session never names its tab — "which tab" is noise then.
        assert_eq!(agg(vec![(1, Working)]), (Working, None));
        assert_eq!(agg(vec![(1, Done(30)), (2, Working)]), (Working, Some(2)));
        assert_eq!(
            agg(vec![(1, Working), (3, NeedsInput), (2, Done(5))]),
            (NeedsInput, Some(3))
        );
        // Two finished tabs: the chip shows the freshest completion.
        assert_eq!(agg(vec![(1, Done(300)), (4, Done(5))]), (Done(5), Some(4)));
        assert_eq!(agg(vec![(1, Exited), (2, Done(9))]), (Done(9), Some(2)));
        assert_eq!(agg(vec![(1, Exited), (2, NotStarted)]), (Exited, Some(1)));
    }

    #[test]
    fn poll_matches_by_exact_name_per_key() {
        let info = |name: &str, status: &str| SessionInfo {
            name: name.into(),
            status: status.into(),
            cwd: String::new(),
        };
        let live = [key("swift", 1), key("swift", 2)];
        let polled = [
            info("aviary-swift", "busy"),
            info("aviary-swift.2", "waiting_for_input"),
            info("aviary-swiftly", "busy"), // foreign bird, not a tab
        ];
        let got = poll_matches(&live, &polled);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&(key("swift", 1), StatusKind::Working)));
        assert!(got.contains(&(key("swift", 2), StatusKind::NeedsInput)));
    }

    #[test]
    fn hook_key_resolution_prefers_the_named_session() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_str().unwrap().to_string();
        let bot = Bot {
            id: BotId("swift".into()),
            name: "Swift".into(),
            glyph: "🕊".into(),
            repo: repo.clone(),
            persona: "birds/swift.md".into(),
            notify: true,
            permissions: None,
            routines: vec![],
        };
        let bots = [bot];
        let live = [key("swift", 1), key("swift", 2)];
        // Named events resolve to their exact tab.
        assert_eq!(
            resolve_hook_key(&bots, &live, &repo, Some("aviary-swift.2")),
            Some(key("swift", 2))
        );
        // A named but dead/unknown session never badges anything.
        assert_eq!(resolve_hook_key(&bots, &live, &repo, Some("aviary-swift.3")), None);
        assert_eq!(resolve_hook_key(&bots, &live, &repo, Some("aviary-ghost")), None);
        // Unnamed (old-format) events fall back to cwd → the primary.
        assert_eq!(
            resolve_hook_key(&bots, &live, &repo, None),
            Some(key("swift", 1))
        );
        assert_eq!(resolve_hook_key(&bots, &live, "/elsewhere", None), None);
        // cwd fallback needs a live primary — tabs alone don't count.
        assert_eq!(resolve_hook_key(&bots, &[key("swift", 2)], &repo, None), None);
    }

    #[test]
    fn reset_primary_forgets_tab_one_only_and_reports_what_it_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift", "raven"]);
        let mut store = AgentStore::new(&cfg);
        let swift = BotId("swift".into());
        store.state.mark_spawned(&cfg.dir, "swift");
        store.state.mark_spawned(&cfg.dir, "swift.2");
        store.state.set_tab_name(&cfg.dir, "swift.2", "review");
        store.set_collab(&swift, Collab::Room("nest".into()));

        assert!(store.reset_primary(&cfg, &swift), "a resume record was dropped");
        assert!(!store.has_session_record(&key("swift", 1)));
        assert!(store.has_session_record(&key("swift", 2)), "tabs are untouched");
        assert_eq!(store.tab_name(&key("swift", 2)), Some("review"));
        assert!(store.collab(&swift).is_none());
        // Persisted: a fresh store reads the same truth.
        assert!(!AgentStore::new(&cfg).has_session_record(&key("swift", 1)));
        // A never-flown bird has nothing to lose.
        assert!(!store.reset_primary(&cfg, &BotId("raven".into())));
    }

    #[test]
    fn lowest_free_tab_fills_gaps() {
        assert_eq!(lowest_free_tab(&[1]), 2);
        assert_eq!(lowest_free_tab(&[1, 2, 4]), 3);
        assert_eq!(lowest_free_tab(&[1, 2, 3, 4]), 5);
    }
}
