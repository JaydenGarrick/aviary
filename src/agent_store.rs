//! The birds themselves: Claude Code sessions keyed by [`SessionKey`] — the
//! bird's primary session (tab 1) plus any human-driven extra tabs.
//!
//! Identity is the session NAME (`aviary-<id>`, tabs `aviary-<id>.<n>`, via
//! `--name`), which makes resume trivial (`--resume <name>`) and makes every
//! bird a stable SendMessage target for its teammates. External signals
//! (rooms, handoffs, routines, webhooks) only ever address the primary.
//!
//! Status is layered, most-truthful-first:
//!   0. the in-session mod's `status/<sessionId>.json`, while its heartbeat
//!      is fresh (<15 s) → it saw the turn start, the dialog open, the turn
//!      die; nothing below can contradict it (save `ModStatus::reconcile`'s
//!      two documented exceptions — the signals live in `mod_layer.rs`),
//!   1. PTY output in the last 2s → Working (streaming IS activity),
//!   2. a fresh `claude agents --json` poll / hook event → busy · idle ·
//!      needs_input (the state a PTY can never show: blocked on a prompt),
//!   3. the output-recency heuristic as the fallback.
//!
//! Transitions feed unread dots and macOS notifications in the shell.
//!
//! Prompts for a RUNNING session go through ONE ordered queue, the inbox
//! (`mod_layer.rs`): a live mod submits them, else this store types them,
//! one per tick — see CLAUDE.md.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::command::SessionInfo;
use crate::config::{Bot, BotId, Config, ConvKey, SessionKey, State};
use crate::event::Event;
use crate::flock::WorkerTransition;
use crate::pty;
use crate::mod_layer::ModLayer;
pub use crate::mod_layer::Delivery;
pub use crate::status::{BotStatus, Detail, StatusKind};
#[cfg(test)]
use crate::status::map_status_str;
use crate::status_file::ModStatus;

pub struct AgentSession {
    pub term: pty::Terminal,
    /// The last prompt aviary itself typed — the sidebar's activity line.
    pub last_prompt: Option<String>,
    pub spawned_at: Instant,
    /// The conversation this PTY runs, and the claude session id pinned for
    /// it (`None` only for a v0.3.1 record resumed by name, until the poll
    /// names it).
    pub conv: ConvKey,
    sid: Option<String>,
    /// The prompt that rode argv — re-sent if this launch dies on arrival.
    argv_prompt: Option<String>,
    /// When aviary last TYPED a prompt here — a switch never lands on it.
    typed_at: Option<Instant>,
    /// Launched via `--resume`: if it dies within seconds, the session is
    /// gone and the next launch must be fresh.
    resumed: bool,
    /// Fresh spawn not yet recorded in state — recorded only once the session
    /// SURVIVES [`MARK_AFTER`]. Marking at launch time was a trap: a spawn
    /// that crashed on boot left state claiming a session that never existed,
    /// and every later launch fell into claude's resume picker.
    fresh_unmarked: bool,
    exit_handled: bool,
    pub kind: SessionKind,
}

/// What a tab's PTY runs: the bird itself, or a VIEWER on one of its workers
/// (`claude attach <id>`). A viewer never earns a resume record — it is not
/// a conversation of the bird's — and closing it never stops the worker.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SessionKind {
    Bird,
    Attached { worker: String, label: String },
}

/// A status change worth reacting to (unread dot, notification).
pub struct Transition {
    pub key: SessionKey,
    pub from: StatusKind,
    pub to: StatusKind,
}

/// Hook events → status kinds. `Stop` = finished; Notification subtypes that
/// mean "a human must act" → NeedsInput; `idle_prompt` just means quiet.
/// `PermissionRequest` fires the moment a permission dialog opens (the
/// Notification's `permission_prompt` only after a ~6 s stall); `StopFailure`
/// is a turn that died — attention, like a blocked bird.
pub fn map_hook_event(event_name: &str, detail: &str) -> Option<StatusKind> {
    match event_name {
        "Stop" => Some(StatusKind::Done),
        "PermissionRequest" | "StopFailure" => Some(StatusKind::NeedsInput),
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
/// Tab 1 switches conversation only once it has been Done this long (secs)
/// — and nothing was typed into it this recently.
const SWITCH_DWELL: u64 = 5;
/// …and the person has not typed into it for this long.
const HUMAN_QUIET: Duration = Duration::from_secs(30);
/// A conversation the person picked holds tab 1 until a turn runs in it, or
/// this long at most — a queued prompt elsewhere must not starve forever.
const PIN_MAX: Duration = Duration::from_secs(5 * 60);

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
    /// Everything known beyond the PTY: the poll's session id and
    /// `waitingFor`, the hooks' last needs-input, the mod's status + inbox.
    signals: ModLayer,
    // Unread and collab are deliberately per-BIRD: the roster card is the unit
    // of attention, whatever tab produced the signal.
    unread: HashMap<BotId, bool>,
    collab: HashMap<BotId, (Collab, Instant)>,
    state: State,
    /// A switch asked for outright, per bird: (conversation, by the person?).
    want: HashMap<BotId, (ConvKey, bool)>,
    /// Prompts for a not-running v0.3.1 conversation with no id to queue
    /// under, oldest first, each with an inbox-style stamp for ordering.
    held: HashMap<ConvKey, std::collections::VecDeque<(String, String)>>,
    /// When the person last typed into each PTY.
    human_input: HashMap<SessionKey, Instant>,
    /// Birds whose tab 1 the person pointed at a conversation, and when.
    pinned: HashMap<BotId, Instant>,
    /// The birds' workers — bird-spawned `claude --bg` sessions, attributed
    /// from the poll by name, never spawned here.
    pub workers: crate::flock::Workers,
}

impl AgentStore {
    pub fn new(cfg: &Config) -> AgentStore {
        AgentStore {
            sessions: HashMap::new(),
            last_output: HashMap::new(),
            observed: HashMap::new(),
            kind_since: HashMap::new(),
            signals: ModLayer::new(cfg),
            unread: HashMap::new(),
            collab: HashMap::new(),
            state: State::load(&cfg.dir),
            want: HashMap::new(),
            held: HashMap::new(),
            human_input: HashMap::new(),
            pinned: HashMap::new(),
            workers: crate::flock::Workers::default(),
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

    /// Live BIRD sessions (viewers are not birds flying).
    pub fn running_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|s| s.kind == SessionKind::Bird && s.term.is_running())
            .count()
    }

    /// Has the conversation this slot runs EVER existed (drives resume vs
    /// the first-flight prompt)?
    pub fn has_session_record(&self, key: &SessionKey) -> bool {
        self.state.created(&self.conv_of(key))
    }

    /// The conversation a slot runs: the live one, else — tab 1 — the
    /// bird's active conversation, or the tab's own.
    pub fn conv_of(&self, key: &SessionKey) -> ConvKey {
        match self.sessions.get(key) {
            Some(s) => s.conv.clone(),
            None if key.tab == 1 => self.state.active_conv(&key.bot),
            None => ConvKey::home(key.clone()),
        }
    }

    /// The tab's human label, if the user set one (display only).
    pub fn tab_name(&self, key: &SessionKey) -> Option<&str> {
        self.state.tab_name(&key.state_key())
    }

    /// The strip's label: a viewer tab names its worker, a bird tab carries
    /// the user's label — and tab 1, once the bird has room conversations,
    /// names the one it runs (`home` / `#nest`) unless the user labelled it.
    pub fn tab_label(&self, key: &SessionKey) -> Option<String> {
        match self.sessions.get(key).map(|s| &s.kind) {
            Some(SessionKind::Attached { label, .. }) => Some(label.clone()),
            _ => self.tab_name(key).map(str::to_string).or_else(|| {
                let conv = self.conv_of(key);
                (key.tab == 1 && (conv.room.is_some() || !self.state.room_convs(&key.bot).is_empty()))
                    .then(|| conv.label())
            }),
        }
    }

    /// The room tab 1 runs, when it is not home — the roster card's tag.
    pub fn active_room(&self, id: &BotId) -> Option<String> {
        self.conv_of(&SessionKey::primary(id.clone())).room
    }

    pub fn is_attached(&self, key: &SessionKey) -> bool {
        matches!(
            self.sessions.get(key).map(|s| &s.kind),
            Some(SessionKind::Attached { .. })
        )
    }

    /// The worker a viewer tab shows (its session name).
    pub fn attached_worker(&self, key: &SessionKey) -> Option<&str> {
        match self.sessions.get(key).map(|s| &s.kind) {
            Some(SessionKind::Attached { worker, .. }) => Some(worker),
            _ => None,
        }
    }

    /// The tab already viewing this worker, if any — never open two.
    pub fn attached_tab(&self, bot: &BotId, worker: &str) -> Option<u8> {
        self.sessions
            .iter()
            .find(|(k, s)| {
                k.bot == *bot && matches!(&s.kind, SessionKind::Attached { worker: w, .. } if w == worker)
            })
            .map(|(k, _)| k.tab)
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
        aggregate_status(self.bird_keys(id).map(|k| (k.tab, self.status_key(k))))
    }

    /// The session the roster chip reports — the tab `status_tabbed` names,
    /// or the lone bird session (which it leaves unnamed), else the primary.
    /// Its `detail` is the reason under the chip.
    pub fn reporting_key(&self, id: &BotId) -> SessionKey {
        let tab = self
            .status_tabbed(id)
            .1
            .or_else(|| self.bird_keys(id).map(|k| k.tab).min())
            .unwrap_or(1);
        SessionKey { bot: id.clone(), tab }
    }

    /// The bird's own sessions — never a worker viewer. The one filter the
    /// chip and its reason both count by.
    fn bird_keys<'a>(&'a self, id: &'a BotId) -> impl Iterator<Item = &'a SessionKey> + 'a {
        self.sessions
            .iter()
            .filter(move |(k, s)| k.bot == *id && s.kind == SessionKind::Bird)
            .map(|(k, _)| k)
    }

    /// One session's status, layered most-truthful-first (see module doc).
    pub fn status_key(&self, key: &SessionKey) -> BotStatus {
        let Some(session) = self.sessions.get(key) else {
            return BotStatus::NotStarted;
        };
        if !session.term.is_running() {
            return BotStatus::Exited;
        }
        // A viewer wears its worker's status — the attach client's redraws
        // are not the worker working.
        if let SessionKind::Attached { worker, .. } = &session.kind {
            return self.workers.status(worker);
        }
        let kind_age = self.kind_age(key);
        pick_status(
            self.mod_for(key).and_then(|st| st.status(kind_age)),
            self.last_output.get(key).map(|t| t.elapsed()),
            self.observed.get(key).map(|(k, at)| (*k, at.elapsed())),
            kind_age,
        )
    }

    fn kind_age(&self, key: &SessionKey) -> u64 {
        self.kind_since
            .get(key)
            .map(|(_, t)| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    /// The mod's word for a key while its heartbeat is fresh; the poll
    /// speaks over it only where `ModStatus::reconcile` says.
    fn mod_for(&self, key: &SessionKey) -> Option<ModStatus> {
        self.signals.mod_for(key)
    }

    /// Is the in-session mod reporting for this key right now?
    pub fn mod_alive(&self, key: &SessionKey) -> bool {
        self.mod_for(key).is_some()
    }

    /// How a prompt should reach this (running) session.
    pub fn delivery(&self, key: &SessionKey) -> Delivery {
        self.signals.delivery(key)
    }

    /// Fold in every status file that changed since the last tick: the
    /// mod's word for birds (by key) and workers (by session id). Returns
    /// the transitions, as the poll does.
    pub fn apply_status_files(&mut self) -> (Vec<Transition>, Vec<WorkerTransition>) {
        let mut birds = Vec::new();
        let mut workers = Vec::new();
        for change in self.signals.scan() {
            if let Some(t) = self.workers.apply_mod(&change.session_id, &change.status) {
                workers.push(t);
            }
            let Some(key) = change.key else { continue };
            let kind = self.mod_for(&key).and_then(|st| st.status(0)).map(BotStatus::kind);
            if let Some(t) = kind.and_then(|k| self.observe(&key, k)) {
                birds.push(t);
            }
        }
        (birds, workers)
    }

    /// The inbox fallback, each tick: for a session whose mod is NOT alive,
    /// TYPE the oldest prompt it never acked, one per tick so the Enter of
    /// one never lands inside the next. A PTY younger than [`MARK_AFTER`] is
    /// still booting: nothing is typed at it — its mod, once up, drains the
    /// same folder (a resume keeps the session id).
    /// Returns how many prompts were typed (tests count them).
    pub fn sweep_inbox(&mut self) -> usize {
        let sessions = &self.sessions;
        let due = self.signals.take_for_typing(|key| {
            sessions.get(key).is_some_and(|s| {
                s.kind == SessionKind::Bird && s.term.is_running() && s.spawned_at.elapsed() >= MARK_AFTER
            })
        });
        let typed = due.len();
        for (key, text) in due {
            if let Some(session) = self.sessions.get_mut(&key) {
                session.term.send_line(&text);
                session.typed_at = Some(Instant::now());
                session.last_prompt = Some(text);
            }
        }
        typed
    }

    /// The banner body for a session that wants the person: the reason the
    /// mod or poll gave (a dead turn says so), else the generic line.
    pub fn attention_text(&self, key: &SessionKey) -> String {
        let failed = matches!(self.status_key(key), BotStatus::Failed);
        match self.detail(key).reason {
            Some(r) if failed => format!("its turn died: {r}"),
            Some(r) => r,
            None => "blocked on a permission prompt or question".into(),
        }
    }

    /// Sweep status files and inbox folders nobody holds any more (once a
    /// minute from the shell).
    pub fn gc(&mut self) {
        // Every conversation on record keeps its queue — one that is not
        // running now drains when it next resumes.
        let held: Vec<String> = self.state.all_sids().map(str::to_string).collect();
        self.signals.gc(self.workers.session_ids().into_iter().chain(held));
    }

    /// Record an observation; returns the transition if the kind changed.
    fn observe(&mut self, key: &SessionKey, kind: StatusKind) -> Option<Transition> {
        self.observed.insert(key.clone(), (kind, Instant::now()));
        if kind == StatusKind::Working && key.tab == 1 {
            self.pinned.remove(&key.bot); // a turn ran in the picked conversation
        }
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
            || self.workers.has_working()
    }

    /// Only badge sessions whose PTY we actually hold — a user's own session
    /// that happens to share a name is not ours.
    fn live_keys(&self) -> Vec<SessionKey> {
        self.sessions
            .iter()
            .filter(|(_, s)| s.kind == SessionKind::Bird && s.term.is_running())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Fold one `claude agents --json` poll in; returns status transitions.
    /// Rows join live keys by PINNED id (see [`poll_matches`]); records the
    /// id (the mod's join key) and `waitingFor`.
    pub fn apply_poll(&mut self, cfg: &Config, sessions: &[SessionInfo]) -> Vec<Transition> {
        let mut out = Vec::new();
        let live: Vec<(SessionKey, Option<String>)> = self
            .live_keys()
            .into_iter()
            .map(|k| {
                let sid = self.sessions.get(&k).and_then(|s| s.sid.clone());
                (k, sid)
            })
            .collect();
        let known: std::collections::HashSet<&str> = self.state.all_sids().collect();
        let matched: Vec<(SessionKey, &SessionInfo)> = poll_matches(&live, &known, sessions);
        for (key, row) in matched {
            let kind = self.signals.note_poll(&key, row);
            // A by-name resume learning its id, or a `/clear` in the PTY:
            // the conversation's record follows the session.
            if let Some(session) = self.sessions.get_mut(&key) {
                if !row.session_id.is_empty() && session.sid.as_deref() != Some(&row.session_id) {
                    session.sid = Some(row.session_id.clone());
                    self.state.set_sid(&cfg.dir, &session.conv, &row.session_id);
                }
            }
            // The mod saw the turn start before the poll can: while it is
            // alive the (laggier) poll must not flap the kind — it speaks only
            // through `reconcile`, inside `mod_for`.
            if let Some(st) = self.mod_for(&key) {
                if let Some(t) = st.status(0).and_then(|s| self.observe(&key, s.kind())) {
                    out.push(t);
                }
                continue;
            }
            if self.signals.poll_predates_hook(&key, kind) {
                continue;
            }
            if let Some(t) = self.observe(&key, kind) {
                out.push(t);
            }
        }
        out
    }

    /// The chip's fine print for one session: a reason while blocked, the
    /// context fill and cost when something reports them.
    pub fn detail(&self, key: &SessionKey) -> Detail {
        let blocked = matches!(self.status_key(key), BotStatus::NeedsInput | BotStatus::Failed);
        self.signals.detail(key, blocked)
    }

    /// Drop every per-session observation of a key (its PTY is gone).
    fn forget_observations(&mut self, key: &SessionKey) {
        self.forget_where(|k| k == key);
    }

    /// THE one place per-key observations are dropped — every new per-key
    /// map is added here, and only here.
    fn forget_where(&mut self, gone: impl Fn(&SessionKey) -> bool) {
        self.last_output.retain(|k, _| !gone(k));
        self.observed.retain(|k, _| !gone(k));
        self.kind_since.retain(|k, _| !gone(k));
        self.human_input.retain(|k, _| !gone(k));
        self.signals.forget_where(gone);
    }

    /// Fold one hook event in. A new-format event names its session
    /// (`aviary_session`, injected by `aviary --hook --session <name>`) and
    /// resolves to that exact key; an old or foreign payload falls back to
    /// cwd matching, which can only mean the PRIMARY — tabs share the repo.
    #[allow(clippy::too_many_arguments)] // one hook payload, field by field
    pub fn apply_hook(
        &mut self,
        cfg: &Config,
        session_id: &str,
        aviary_session: Option<&str>,
        cwd: &str,
        event_name: &str,
        detail: &str,
    ) -> Option<Transition> {
        let kind = map_hook_event(event_name, detail)?;
        // By id first: a hook names the CONVERSATION, and a late one from a
        // conversation this slot no longer runs must not badge its successor.
        if !session_id.is_empty() {
            let live = self.sessions.iter().find(|(_, s)| s.sid.as_deref() == Some(session_id));
            if let Some((key, _)) = live {
                let key = key.clone();
                return self.note_hook_kind(&key, kind);
            }
            if self.state.all_sids().any(|s| s == session_id) {
                return None;
            }
        }
        let key = resolve_hook_key(&cfg.bots, &self.live_keys(), cwd, aviary_session)?;
        self.note_hook_kind(&key, kind)
    }

    fn note_hook_kind(&mut self, key: &SessionKey, kind: StatusKind) -> Option<Transition> {
        if self.mod_alive(key) {
            return None; // the mod already said so, sooner and finer
        }
        self.signals.note_hook(key, kind);
        self.observe(key, kind)
    }

    // ------------------------------------------------------------ lifecycle

    /// Make sure ONE session of the bot is live, spawning or resuming as
    /// needed. `prompt`: rides argv on a launch, typed into a running session.
    /// Runs the slot's CURRENT conversation (tab 1: the active one) — the
    /// human wake. External signals arrive via [`AgentStore::signal`], tab 1
    /// only; extra tabs are human-driven only.
    pub fn ensure_running_key(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        tab: u8,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let key = SessionKey { bot: bot.id.clone(), tab };
        let delivery = self.delivery(&key);
        if let Some(session) = self.sessions.get_mut(&key) {
            if session.term.is_running() {
                if session.kind != SessionKind::Bird {
                    return Ok(()); // a viewer: nothing to type at, nothing to resume
                }
                if let Some(p) = prompt {
                    let posted = match &delivery {
                        // The mod drains the inbox with `$.prompt.submit`:
                        // queued until the session is idle, never keystrokes.
                        Delivery::Inbox(sid) => {
                            self.signals.post(sid, p)
                        }
                        Delivery::Typed => false,
                    };
                    if !posted {
                        // send_line, never a trailing \r in the same burst —
                        // the child's paste detection would swallow the submit.
                        session.term.send_line(p);
                        session.typed_at = Some(Instant::now());
                    }
                    session.last_prompt = Some(p.to_string());
                }
                return Ok(());
            }
            if let SessionKind::Attached { label, .. } = &session.kind {
                // The attach client ended: never replace a viewer with a bird.
                let label = label.clone();
                self.sessions.remove(&key);
                bail!("{label}'s viewer ended — reopen it from the worker row");
            }
            self.sessions.remove(&key); // exited — replace it
            // The dead session's id must not route the next prompt to an
            // inbox nobody drains; the poll re-names the new one.
            self.forget_observations(&key);
        }
        let conv = self.conv_of(&key);
        self.launch_conv(cfg, bot, &conv, prompt, tx)
    }

    /// Launch a conversation into its slot: `--resume` its pinned id once
    /// it is created (by name only for a v0.3.1 record), else fresh under a
    /// newly minted id.
    fn launch_conv(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        conv: &ConvKey,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let mode = self.launch_mode(cfg, conv);
        self.launch(cfg, bot, conv, prompt, tx, mode)
    }

    /// How a conversation launches. A fresh one mints its id here and moves
    /// prompts queued for the id it replaces (a resume that died, an
    /// uncreated spawn) onto the new one.
    fn launch_mode(&mut self, cfg: &Config, conv: &ConvKey) -> Launch {
        if let Some(r) = self.state.record(conv).filter(|r| r.created) {
            let target = r.sid.clone().unwrap_or_else(|| conv.slot.session_name());
            return Launch::Resume { sid: r.sid.clone(), target };
        }
        let (sid, old) = self.state.begin_fresh(&cfg.dir, conv);
        if let Some(old) = old {
            crate::status_file::inbox_move(&cfg.inbox_dir(), &old, &sid);
        }
        Launch::Fresh { sid }
    }

    fn launch(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        conv: &ConvKey,
        prompt: Option<&str>,
        tx: &Sender<Event>,
        mode: Launch,
    ) -> Result<()> {
        let key = &conv.slot;
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
        let resumed = matches!(mode, Launch::Resume { .. });
        let sid = mode.sid().map(str::to_string);
        let args = launch_args(
            key.session_name(),
            &persona,
            &settings,
            &cfg.dir,
            &cfg.mcp_config_path(),
            &cfg.plugin_dir(),
            prompt,
            &mode,
        );

        // The mod inside the bird finds its status/ and inbox/ through this;
        // the workers the bird spawns inherit it.
        let env = [("AVIARY_CONFIG_DIR", cfg.dir.display().to_string())];
        let term = pty::Terminal::spawn(key.clone(), "claude", &args, &repo, &env, 24, 80, tx.clone())?;
        self.sessions.insert(
            key.clone(),
            AgentSession {
                term,
                last_prompt: prompt.map(str::to_string),
                conv: conv.clone(),
                sid: sid.clone(),
                argv_prompt: prompt.map(str::to_string),
                typed_at: None,
                spawned_at: Instant::now(),
                resumed,
                fresh_unmarked: !resumed,
                exit_handled: false,
                kind: SessionKind::Bird,
            },
        );
        self.kind_since
            .insert(key.clone(), (StatusKind::Working, Instant::now()));
        if key.tab == 1 {
            self.state.set_active(&cfg.dir, conv);
        }
        // Known at birth: delivery is the inbox from the first tick, and no
        // id from the slot's previous conversation is inherited.
        if let Some(sid) = &sid {
            self.signals.seed(key, sid);
        }
        Ok(())
    }

    /// Open a VIEWER on one of the bird's workers in tab `key.tab`:
    /// `claude attach <id>` in the bird's repo. Never a resume record
    /// (`fresh_unmarked: false`); closing it leaves the worker running.
    pub fn launch_attach(
        &mut self,
        bot: &Bot,
        key: &SessionKey,
        worker: &str,
        label: &str,
        short_id: &str,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let repo = bot.repo_path();
        if !repo.is_dir() {
            bail!("{}'s repo is missing: {}", bot.name, repo.display());
        }
        self.sessions.remove(key);
        let args = vec!["attach".to_string(), short_id.to_string()];
        let term = pty::Terminal::spawn(key.clone(), "claude", &args, &repo, &[], 24, 80, tx.clone())?;
        self.sessions.insert(
            key.clone(),
            AgentSession {
                term,
                last_prompt: None,
                conv: ConvKey::home(key.clone()),
                sid: None,
                argv_prompt: None,
                typed_at: None,
                spawned_at: Instant::now(),
                resumed: false,
                fresh_unmarked: false,
                exit_handled: false,
                kind: SessionKind::Attached {
                    worker: worker.to_string(),
                    label: label.to_string(),
                },
            },
        );
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
        if self.is_attached(&key) {
            bail!("tab {tab} shows a worker — close it instead");
        }
        let conv = self.conv_of(&key);
        self.sessions.remove(&key);
        self.forget_observations(&key);
        self.state.forget(&cfg.dir, &conv);
        self.launch_conv(cfg, bot, &conv, prompt, tx)
    }

    // ------------------------------------------------------- conversations

    /// An EXTERNAL signal for a bird's conversation — `room: None` is home.
    /// Tab 1 runs one conversation at a time, so a live tab 1 on ANOTHER
    /// conversation never relaunches here: the prompt is queued in the
    /// target's inbox and [`AgentStore::pump_switches`] — the ONE place a
    /// live tab 1 changes conversation — switches when the bird is idle.
    /// A prompt-less signal (a handoff's wake) on another conversation asks
    /// for that switch outright.
    pub fn signal(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        room: Option<&str>,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<Signaled> {
        let key = SessionKey::primary(bot.id.clone());
        let conv = match room {
            Some(r) => ConvKey::room(bot.id.clone(), r),
            None => ConvKey::home(key.clone()),
        };
        let running = self
            .sessions
            .get(&key)
            .filter(|s| s.kind == SessionKind::Bird && s.term.is_running());
        let on_target = running.is_some_and(|s| s.conv == conv);
        match route(running.is_some(), on_target) {
            Route::Launch => {
                self.state.set_active(&cfg.dir, &conv);
                self.ensure_running_key(cfg, bot, 1, prompt, tx)?;
                Ok(Signaled::Launched)
            }
            Route::Deliver => {
                self.ensure_running_key(cfg, bot, 1, prompt, tx)?;
                Ok(Signaled::Delivered)
            }
            Route::Queue => {
                match prompt {
                    Some(p) => self.enqueue(cfg, &conv, p),
                    None => {
                        self.want.insert(bot.id.clone(), (conv, false));
                    }
                }
                Ok(Signaled::Queued)
            }
        }
    }

    /// The person picked a conversation for tab 1 (the tab menu). A stopped
    /// bird just points there (its next wake opens it); a live one switches
    /// at the next idle moment, and stays put until a turn runs in it.
    /// Returns true when the switch is waiting for the bird to go idle.
    pub fn pick_conversation(&mut self, cfg: &Config, conv: &ConvKey) -> bool {
        let key = &conv.slot;
        let running = self
            .sessions
            .get(key)
            .filter(|s| s.kind == SessionKind::Bird && s.term.is_running());
        match running {
            None => {
                self.sessions.remove(key);
                self.state.set_active(&cfg.dir, conv);
                false
            }
            Some(s) if s.conv == *conv => {
                self.want.remove(&key.bot);
                false
            }
            Some(_) => {
                self.want.insert(key.bot.clone(), (conv.clone(), true));
                true
            }
        }
    }

    /// The conversations tab 1 can run: home, then each room's, sorted.
    pub fn conversations(&self, id: &BotId) -> Vec<ConvKey> {
        let mut out = vec![ConvKey::home(SessionKey::primary(id.clone()))];
        out.extend(self.state.room_convs(id).iter().map(|r| ConvKey::room(id.clone(), r)));
        out
    }

    /// Queue a prompt for a conversation tab 1 is NOT running. A v0.3.1
    /// record has no id to name a folder by; it waits in memory instead.
    fn enqueue(&mut self, cfg: &Config, conv: &ConvKey, prompt: &str) {
        let posted = self
            .state
            .queue_sid(&cfg.dir, conv)
            .is_some_and(|sid| self.signals.post(&sid, prompt));
        if !posted {
            let stamp = format!("{:013}-9999.md", crate::status_file::now_ms());
            self.held.entry(conv.clone()).or_default().push_back((stamp, prompt.to_string()));
        }
    }

    /// Has a prompt starting with `prefix` queued for a conversation that is
    /// not running? (A routine never stacks a second firing behind a busy
    /// bird.)
    pub fn queued_with_prefix(&self, conv: &ConvKey, prefix: &str) -> bool {
        let held = self.held.get(conv).is_some_and(|q| q.iter().any(|(_, t)| t.starts_with(prefix)));
        held || self.state.record(conv).and_then(|r| r.sid.as_deref()).is_some_and(|sid| {
            self.signals
                .queued(sid)
                .iter()
                .any(|(_, path)| std::fs::read_to_string(path).is_ok_and(|t| t.starts_with(prefix)))
        })
    }

    /// The person typed into this PTY (switches wait for them to stop).
    pub fn note_human_input(&mut self, key: &SessionKey) {
        self.human_input.insert(key.clone(), Instant::now());
    }

    /// Once a tick: switch each idle tab 1 to the conversation that wants it
    /// (a person's pick, else the OLDEST queued prompt). The oldest prompt
    /// rides argv; the rest drain through the mod. Returns who switched.
    pub fn pump_switches(&mut self, cfg: &Config, tx: &Sender<Event>) -> Vec<(BotId, ConvKey)> {
        let mut out = Vec::new();
        for bot in &cfg.bots {
            let key = SessionKey::primary(bot.id.clone());
            let Some(view) = self.switch_view(&key) else { continue };
            let Some(target) = next_switch(&view) else { continue };
            let human = self.want.get(&bot.id).is_some_and(|(c, h)| *h && *c == target);
            match self.switch_to(cfg, bot, &target, human, tx) {
                Ok(()) => out.push((bot.id.clone(), target)),
                Err(_) => {
                    // The launch failed (a moved repo): the PTY is gone, as
                    // with any failed launch; the queue waits for the next.
                    self.want.remove(&bot.id);
                }
            }
        }
        out
    }

    /// Everything [`next_switch`] weighs for one bird's tab 1 — `None` when
    /// tab 1 is not a live bird session.
    fn switch_view(&self, key: &SessionKey) -> Option<SwitchView> {
        let s = self.sessions.get(key)?;
        if s.kind != SessionKind::Bird || !s.term.is_running() {
            return None;
        }
        let active_queued = s.sid.as_deref().is_some_and(|sid| !self.signals.queued(sid).is_empty())
            || self.held.get(&s.conv).is_some_and(|q| !q.is_empty());
        let queued = self
            .conversations(&key.bot)
            .into_iter()
            .filter(|c| *c != s.conv)
            .filter_map(|c| self.oldest_queued(&c).map(|stamp| (c, stamp)))
            .collect();
        Some(SwitchView {
            done_for: match self.status_key(key) {
                BotStatus::Done(age) => Some(age),
                _ => None,
            },
            pty_age: s.spawned_at.elapsed(),
            active_queued,
            human_idle: self.human_input.get(key).map(Instant::elapsed),
            typed_ago: s.typed_at.map(|t| t.elapsed()),
            workers_busy: self.workers.busy(&key.bot),
            want: self.want.get(&key.bot).map(|(c, _)| c.clone()).filter(|c| *c != s.conv),
            pinned: self
                .pinned
                .get(&key.bot)
                .is_some_and(|at| at.elapsed() < PIN_MAX),
            queued,
        })
    }

    /// The name (stamp) of a not-running conversation's oldest queued
    /// prompt — the order switches are served in.
    fn oldest_queued(&self, conv: &ConvKey) -> Option<String> {
        let held = self.held.get(conv).and_then(|q| q.front()).map(|(stamp, _)| stamp.clone());
        let filed = self
            .state
            .record(conv)
            .and_then(|r| r.sid.as_deref())
            .and_then(|sid| self.signals.queued(sid).into_iter().next())
            .map(|(name, _)| name);
        held.into_iter().chain(filed).min()
    }

    /// Take a conversation's oldest queued prompt out of its queue (it is
    /// about to ride argv).
    fn take_oldest(&mut self, conv: &ConvKey) -> Option<String> {
        if let Some(q) = self.held.get_mut(conv) {
            if let Some((_, text)) = q.pop_front() {
                return Some(text);
            }
        }
        let sid = self.state.record(conv)?.sid.clone()?;
        let (_, path) = self.signals.queued(&sid).into_iter().next()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        Some(text)
    }

    /// Relaunch tab 1 on `target`, its oldest queued prompt on argv.
    fn switch_to(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        target: &ConvKey,
        human: bool,
        tx: &Sender<Event>,
    ) -> Result<()> {
        let key = SessionKey::primary(bot.id.clone());
        let prompt = self.take_oldest(target);
        self.sessions.remove(&key); // drop = hang up
        self.forget_observations(&key);
        self.want.remove(&bot.id);
        match (&target.room, prompt.is_some()) {
            (Some(room), true) => self.set_collab(&bot.id, Collab::Room(room.clone())),
            (None, _) if matches!(self.collab(&bot.id), Some(Collab::Room(_))) => {
                self.collab.remove(&bot.id);
            }
            _ => {}
        }
        if human {
            self.pinned.insert(bot.id.clone(), Instant::now());
        } else {
            self.pinned.remove(&bot.id);
        }
        self.launch_conv(cfg, bot, target, prompt.as_deref(), tx)
    }

    /// A room left the roster: its conversations are forgotten and their
    /// queues dropped. A bird running one goes home — now if it is not
    /// live, else at its next idle moment.
    pub fn forget_room(&mut self, cfg: &Config, room: &str) {
        for sid in self.state.forget_room(&cfg.dir, room) {
            let _ = std::fs::remove_dir_all(cfg.inbox_dir().join(&sid));
        }
        self.held.retain(|c, _| c.room.as_deref() != Some(room));
        for bot in &cfg.bots {
            if self.state.active_room(&bot.id) != Some(room) {
                continue;
            }
            let home = ConvKey::home(SessionKey::primary(bot.id.clone()));
            if self.switch_view(&home.slot).is_some() {
                self.want.insert(bot.id.clone(), (home, false));
            } else {
                self.sessions.remove(&home.slot);
                self.state.set_active(&cfg.dir, &home);
            }
        }
    }

    /// Once a second: a fresh session that has survived [`MARK_AFTER`] becomes
    /// that tab's resumable session of record; a viewer whose attach client
    /// ended (worker stopped or removed, or the user left it) closes itself.
    /// Returns the viewer tabs that closed, so the shell can leave them.
    pub fn tick(&mut self, cfg: &Config) -> Vec<SessionKey> {
        let ripe: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.fresh_unmarked && s.term.is_running() && s.spawned_at.elapsed() >= MARK_AFTER
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in ripe {
            if let Some(s) = self.sessions.get_mut(&key) {
                s.fresh_unmarked = false;
                self.state.mark_created(&cfg.dir, &s.conv);
            }
        }
        let ended: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.kind != SessionKind::Bird && !s.term.is_running())
            .map(|(k, _)| k.clone())
            .collect();
        for key in &ended {
            self.sessions.remove(key);
            self.forget_observations(key);
        }
        ended
    }

    /// Note PTY activity (from `Event::AgentOutput`). Returns a relaunch
    /// request when a `--resume` died instantly: the session no longer
    /// exists, so the caller should boot THIS KEY again with the prompt that
    /// rode the dead launch — the record is already uncreated, so that boot
    /// mints a fresh id and carries the old id's queue over.
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
            let prompt = session.argv_prompt.clone();
            let conv = session.conv.clone();
            self.state.uncreate(&cfg.dir, &conv);
            self.sessions.remove(key);
            self.forget_observations(key);
            return RelaunchHint::FreshSpawn(prompt);
        }
        RelaunchHint::No
    }

    /// Stop EVERY session of the bird (the roster's `x`). Dropping the master
    /// closes the PTY, which hangs up the child. Viewer tabs drop too — their
    /// workers keep running, they just lose their window.
    pub fn stop(&mut self, id: &BotId) {
        self.sessions.retain(|k, _| k.bot != *id);
        self.forget_where(|k| k.bot == *id);
        self.collab.remove(id);
    }

    /// Stop one tab's session only; the bird's other sessions keep flying.
    pub fn stop_key(&mut self, key: &SessionKey) {
        self.sessions.remove(key);
        self.forget_observations(key);
    }

    /// Close a tab: stop it, forget its resume record and label, so the strip
    /// entry disappears. The claude session itself survives, unreferenced.
    pub fn close_tab(&mut self, cfg: &Config, key: &SessionKey) {
        let viewer = self.is_attached(key);
        self.stop_key(key);
        if viewer {
            return; // a viewer has no record and no label of its own
        }
        self.state.forget(&cfg.dir, &ConvKey::home(key.clone()));
        self.state.set_tab_name(&cfg.dir, &key.state_key(), "");
    }

    /// A bird leaving the roster: stop all its sessions and drop all
    /// bookkeeping. The claude sessions themselves survive —
    /// `claude --resume <session id>` works from any terminal.
    pub fn release(&mut self, cfg: &Config, id: &BotId) {
        self.stop(id);
        self.unread.remove(id);
        self.want.remove(id);
        self.pinned.remove(id);
        self.held.retain(|c, _| c.slot.bot != *id);
        self.workers.forget_bot(id);
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
            NeedsInput | Failed => 4,
            Working => 3,
            Done(_) | Paused(_) => 2,
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

/// The layering itself, pure (see the module doc): the mod while alive,
/// else streaming output, else a fresh poll/hook observation, else the
/// output-recency heuristic.
fn pick_status(
    modded: Option<BotStatus>,
    output_age: Option<Duration>,
    observed: Option<(StatusKind, Duration)>,
    kind_age: u64,
) -> BotStatus {
    if let Some(s) = modded {
        return s;
    }
    // Streaming overrides a laggy poll — it can say "idle" while tokens are
    // visibly arriving.
    if output_age.is_some_and(|a| a < OUTPUT_OVERRIDE) {
        return BotStatus::Working;
    }
    if let Some((kind, age)) = observed {
        if age < POLL_TRUST {
            return match kind {
                StatusKind::Working => BotStatus::Working,
                StatusKind::NeedsInput => BotStatus::NeedsInput,
                // How long the kind has stood (`kind_since`), not output age.
                StatusKind::Done => BotStatus::Done(kind_age),
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

/// Match poll rows to live keys: by the PINNED session id first. A name is
/// shared by every conversation the slot ever ran, so the name fallback —
/// for a v0.3.1 record resumed by name, or a `/clear` that re-minted the id
/// inside the PTY — takes only a row that is UNIQUE under that name and
/// whose id belongs to no other conversation on record (`known`).
/// `aviary-swift.2` never badges `aviary-swift`; `aviary-swiftly` neither.
fn poll_matches<'a>(
    live: &[(SessionKey, Option<String>)],
    known: &std::collections::HashSet<&str>,
    sessions: &'a [SessionInfo],
) -> Vec<(SessionKey, &'a SessionInfo)> {
    live.iter()
        .filter_map(|(key, sid)| {
            if let Some(row) = sid
                .as_deref()
                .and_then(|sid| sessions.iter().find(|s| s.session_id == sid))
            {
                return Some((key.clone(), row));
            }
            let name = key.session_name();
            let mut named = sessions.iter().filter(|s| s.name == name);
            let row = named.next()?;
            let unique = named.next().is_none();
            (unique && !known.contains(row.session_id.as_str())).then(|| (key.clone(), row))
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

/// Where an external signal's prompt went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signaled {
    /// Tab 1 was not live: launched on the target, prompt on argv.
    Launched,
    /// Tab 1 already runs the target: delivered as any prompt is.
    Delivered,
    /// Tab 1 runs ANOTHER conversation: queued for the next idle switch.
    Queued,
}

#[derive(PartialEq, Eq, Debug)]
enum Route {
    Launch,
    Deliver,
    Queue,
}

/// A signal's route — never a relaunch of a live tab 1 (only
/// [`AgentStore::pump_switches`] does that, behind its guards).
fn route(live: bool, on_target: bool) -> Route {
    match (live, on_target) {
        (false, _) => Route::Launch,
        (true, true) => Route::Deliver,
        (true, false) => Route::Queue,
    }
}

/// What [`next_switch`] weighs for one live tab 1.
#[derive(Default)]
struct SwitchView {
    /// Some(age secs) while the status is Done.
    done_for: Option<u64>,
    pty_age: Duration,
    /// The running conversation still has prompts to drain.
    active_queued: bool,
    /// Since the person last typed into it (None: never).
    human_idle: Option<Duration>,
    /// Since aviary last typed into it (None: never).
    typed_ago: Option<Duration>,
    /// The bird has workers running — their reports address the
    /// conversation that spawned them.
    workers_busy: bool,
    /// A switch asked for outright (a pick, a handoff's wake, a deleted room).
    want: Option<ConvKey>,
    /// The person's pick holds tab 1 until a turn runs in it.
    pinned: bool,
    /// Not-running conversations with queued prompts, each with the stamp
    /// of its oldest.
    queued: Vec<(ConvKey, String)>,
}

/// Should tab 1 switch now, and to what? Only a bird that is truly idle —
/// Done for a while, booted, drained, untouched by the person and by
/// aviary's typing, no workers out. Then: an outright want, else (unless
/// pinned) the conversation whose queued prompt is OLDEST.
fn next_switch(v: &SwitchView) -> Option<ConvKey> {
    let idle = v.done_for.is_some_and(|age| age >= SWITCH_DWELL)
        && v.pty_age >= MARK_AFTER
        && !v.active_queued
        && v.human_idle.is_none_or(|d| d >= HUMAN_QUIET)
        && v.typed_ago.is_none_or(|d| d.as_secs() >= SWITCH_DWELL)
        && !v.workers_busy;
    if !idle {
        return None;
    }
    if let Some(c) = &v.want {
        return Some(c.clone());
    }
    if v.pinned {
        return None;
    }
    v.queued.iter().min_by(|a, b| a.1.cmp(&b.1)).map(|(c, _)| c.clone())
}

#[derive(PartialEq, Eq, Debug)]
pub enum RelaunchHint {
    No,
    /// A resumed session is gone — spawn fresh, re-sending the prompt that
    /// rode the dead launch (if any).
    FreshSpawn(Option<String>),
}

/// How [`launch_args`] starts a conversation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Launch {
    /// A new conversation under an id aviary minted (`--session-id`).
    Fresh { sid: String },
    /// An existing one: `target` is its id — or, for a v0.3.1 record whose
    /// id was never learned, its name (`sid: None`).
    Resume { sid: Option<String>, target: String },
}

impl Launch {
    pub fn sid(&self) -> Option<&str> {
        match self {
            Launch::Fresh { sid } => Some(sid),
            Launch::Resume { sid, .. } => sid.as_deref(),
        }
    }
}

/// Argv for a bird's launch. ORDER IS LOAD-BEARING: `--mcp-config` and
/// `--add-dir` are VARIADIC in the claude CLI and consume values until the
/// next flag — unterminated, they swallow the positional prompt as another
/// config path ("MCP config file not found: <repo>/<prompt text>"). Each is
/// therefore followed by another flag, and the positional prompt only ever
/// follows a single-value flag. `--plugin-dir` is single-valued (repeatable,
/// not variadic) but sits ahead of `--settings` all the same.
#[allow(clippy::too_many_arguments)] // an argv builder: every path is its own flag
fn launch_args(
    session: String,
    persona: &std::path::Path,
    settings: &std::path::Path,
    cfg_dir: &std::path::Path,
    mcp: &std::path::Path,
    plugin: &std::path::Path,
    prompt: Option<&str>,
    mode: &Launch,
) -> Vec<String> {
    let mut args: Vec<String> = match mode {
        Launch::Resume { target, .. } => vec!["--resume".into(), target.clone()],
        // The name addresses the bird (SendMessage, hooks); the id IS the
        // conversation — pinned so nothing has to learn it afterwards.
        Launch::Fresh { sid } => vec!["--name".into(), session, "--session-id".into(), sid.clone()],
    };
    args.extend([
        // The Linear + Figma hooks, independent of repo-level MCP config.
        "--mcp-config".into(),
        mcp.display().to_string(),
        // Rooms + handoffs live under the config dir; make writes there
        // first-class instead of out-of-scope.
        "--add-dir".into(),
        cfg_dir.display().to_string(),
        // The flock skills (orchestrator + worker), for this session only.
        "--plugin-dir".into(),
        plugin.display().to_string(),
        // The classic hooks (`HOOKED_EVENTS`) (+ optional permission allows) —
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
    use crate::status_file;
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
        // The stable floor under the mod: a dialog opening, a turn dying.
        assert_eq!(map_hook_event("PermissionRequest", ""), Some(StatusKind::NeedsInput));
        assert_eq!(map_hook_event("StopFailure", ""), Some(StatusKind::NeedsInput));
        assert_eq!(map_hook_event("PreToolUse", "x"), None);
        // Every event the settings file hooks maps to something.
        for event in crate::config::HOOKED_EVENTS {
            assert!(map_hook_event(event, "").is_some(), "{event} must map");
        }
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
            Path::new("/cfg/plugin"),
            Some("You've just been perched."),
            &Launch::Fresh { sid: "sid-1".into() },
        );
        assert_eq!(args.last().map(String::as_str), Some("You've just been perched."));
        let p = args.iter().position(|a| a == "--plugin-dir").unwrap();
        assert_eq!(args[p + 1], "/cfg/plugin");
        assert!(args[p + 2].starts_with("--"), "--plugin-dir takes one value then a flag");
        assert!(p < args.iter().position(|a| a == "--settings").unwrap());
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
        // A fresh launch is NAMED for addressing and PINNED for identity.
        assert_eq!(args[..4], ["--name", "aviary-swift", "--session-id", "sid-1"]);
    }

    #[test]
    fn resume_swaps_name_for_resume_and_tolerates_no_prompt() {
        let args = launch_args(
            "aviary-raven".into(),
            Path::new("/cfg/birds/raven.md"),
            Path::new("/cfg/settings/raven.json"),
            Path::new("/cfg"),
            Path::new("/cfg/mcp.json"),
            Path::new("/cfg/plugin"),
            None,
            &Launch::Resume { sid: Some("sid-r".into()), target: "sid-r".into() },
        );
        assert_eq!(args[..2], ["--resume", "sid-r"]);
        assert!(!args.iter().any(|a| a == "--name" || a == "--session-id"));
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
        // Worker-only states: Failed is attention, Paused is quiet.
        assert_eq!(agg(vec![(1, Working), (2, Failed)]), (Failed, Some(2)));
        assert_eq!(agg(vec![(1, Paused(5)), (2, Working)]), (Working, Some(2)));
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
            session_id: format!("{name}-uuid"),
            cwd: String::new(),
            ..Default::default()
        };
        let live = [(key("swift", 1), None), (key("swift", 2), None)];
        let polled = [
            info("aviary-swift", "busy"),
            info("aviary-swift.2", "waiting_for_input"),
            info("aviary-swiftly", "busy"), // foreign bird, not a tab
        ];
        let got: Vec<(SessionKey, StatusKind, &str)> = poll_matches(&live, &Default::default(), &polled)
            .into_iter()
            .map(|(k, row)| (k, map_status_str(row.status_str()), row.session_id.as_str()))
            .collect();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&(key("swift", 1), StatusKind::Working, "aviary-swift-uuid")));
        assert!(got.contains(&(key("swift", 2), StatusKind::NeedsInput, "aviary-swift.2-uuid")));
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

    /// A fresh launch mints and PINS an id; once created it resumes BY ID
    /// (a name shared by two sessions is refused); a v0.3.1 record with no
    /// id resumes by name; a dead resume mints anew and carries its queue.
    #[test]
    fn launch_mode_pins_resumes_by_id_and_remints_after_a_dead_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let home = ConvKey::home(key("swift", 1));

        let Launch::Fresh { sid: first } = store.launch_mode(&cfg, &home) else { panic!("never flown: fresh") };
        assert_eq!(store.state.record(&home).and_then(|r| r.sid.as_deref()), Some(first.as_str()));
        store.state.mark_created(&cfg.dir, &home);
        assert_eq!(
            store.launch_mode(&cfg, &home),
            Launch::Resume { sid: Some(first.clone()), target: first.clone() }
        );
        // Persisted across restarts.
        assert_eq!(AgentStore::new(&cfg).state.record(&home).unwrap().sid.as_deref(), Some(first.as_str()));

        // The resume died on arrival: uncreated → a NEW id (claude refuses a
        // reused one), and prompts queued for the dead id follow it.
        let mut stamp = status_file::PostStamp::default();
        status_file::inbox_post(&cfg.inbox_dir(), &first, &mut stamp, "queued").unwrap();
        store.state.uncreate(&cfg.dir, &home);
        let Launch::Fresh { sid: second } = store.launch_mode(&cfg, &home) else { panic!("fresh again") };
        assert_ne!(first, second);
        assert_eq!(status_file::inbox_pending(&cfg.inbox_dir(), &second, "").len(), 1);

        // A v0.3.1 state file: spawned, never named — resumed by name once.
        std::fs::write(cfg.dir.join("state.json"), r#"{"spawned":["swift","swift.2"]}"#).unwrap();
        let mut legacy = AgentStore::new(&cfg);
        assert_eq!(
            legacy.launch_mode(&cfg, &ConvKey::home(key("swift", 2))),
            Launch::Resume { sid: None, target: "aviary-swift.2".into() }
        );
    }

    /// Rows join by PINNED id; the name fallback takes only a row that is
    /// unique under the name and owned by no other conversation on record.
    #[test]
    fn poll_matches_by_pinned_id_before_name() {
        let row = |name: &str, sid: &str| SessionInfo {
            name: name.into(),
            status: "idle".into(),
            session_id: sid.into(),
            ..Default::default()
        };
        let known: std::collections::HashSet<&str> = ["home-sid", "room-sid"].into_iter().collect();
        // A foreign `claude --resume` of the same name listed FIRST: the id wins.
        let polled = [row("aviary-swift", "foreign"), row("aviary-swift", "room-sid")];
        let got = poll_matches(&[(key("swift", 1), Some("room-sid".into()))], &known, &polled);
        assert_eq!(got[0].1.session_id, "room-sid");
        // Name fallback: never when the name is ambiguous…
        assert!(poll_matches(&[(key("swift", 1), None)], &known, &polled).is_empty());
        // …nor when the row is another conversation on record…
        let home_only = [row("aviary-swift", "home-sid")];
        assert!(poll_matches(&[(key("swift", 1), Some("room-sid".into()))], &known, &home_only).is_empty());
        // …but a unique, unknown row (a by-name resume, a `/clear`) joins.
        let cleared = [row("aviary-swift", "brand-new")];
        let got = poll_matches(&[(key("swift", 1), Some("room-sid".into()))], &known, &cleared);
        assert_eq!(got[0].1.session_id, "brand-new");
    }

    /// A hook names its conversation: one from a conversation on record that
    /// no slot runs now is dropped, never pinned on the slot's successor.
    #[test]
    fn hooks_from_a_conversation_no_longer_running_are_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let home = ConvKey::home(key("swift", 1));
        let (old, _) = store.state.begin_fresh(&cfg.dir, &home);
        assert!(store
            .apply_hook(&cfg, &old, Some("aviary-swift"), "", "PermissionRequest", "")
            .is_none());
    }

    /// gc keeps the queue of every conversation on record, running or not —
    /// a room conversation drains its inbox when it next resumes.
    #[test]
    fn gc_keeps_the_queues_of_conversations_on_record() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let (held, _) = store.state.begin_fresh(&cfg.dir, &ConvKey::home(key("swift", 2)));
        let mut stamp = status_file::PostStamp::default();
        for sid in [held.as_str(), "orphan"] {
            status_file::inbox_post(&cfg.inbox_dir(), sid, &mut stamp, "x").unwrap();
            let old = std::time::SystemTime::now() - Duration::from_secs(3600);
            std::fs::File::open(cfg.inbox_dir().join(sid)).unwrap().set_modified(old).unwrap();
        }
        store.gc();
        assert!(cfg.inbox_dir().join(&held).is_dir());
        assert!(!cfg.inbox_dir().join("orphan").exists());
    }

    /// A signal never relaunches a live tab 1: it launches a stopped one,
    /// delivers to the conversation running, else queues.
    #[test]
    fn route_never_relaunches_a_live_tab_one() {
        assert_eq!(route(false, false), Route::Launch);
        assert_eq!(route(false, true), Route::Launch);
        assert_eq!(route(true, true), Route::Deliver);
        assert_eq!(route(true, false), Route::Queue);
    }

    fn idle_view() -> SwitchView {
        SwitchView {
            done_for: Some(SWITCH_DWELL),
            pty_age: MARK_AFTER,
            ..Default::default()
        }
    }

    /// Tab 1 switches only when truly idle — and then to an outright want,
    /// else (unless the person pinned it) the OLDEST queued prompt.
    #[test]
    fn next_switch_waits_for_true_idle_then_serves_the_oldest() {
        let nest = ConvKey::room(BotId("swift".into()), "nest");
        let ops = ConvKey::room(BotId("swift".into()), "ops");
        let home = ConvKey::home(key("swift", 1));
        let queued = vec![(nest.clone(), "0000000000200-0000.md".into()), (ops.clone(), "0000000000100-0000.md".into())];
        let v = SwitchView { queued: queued.clone(), ..idle_view() };
        assert_eq!(next_switch(&v), Some(ops.clone()), "oldest first, across rooms");
        assert_eq!(next_switch(&idle_view()), None, "nothing queued, nothing to do");

        // Every guard holds it back on its own.
        let held_back = [
            SwitchView { done_for: None, ..SwitchView { queued: queued.clone(), ..idle_view() } },
            SwitchView { done_for: Some(SWITCH_DWELL - 1), queued: queued.clone(), ..idle_view() },
            SwitchView { pty_age: MARK_AFTER - Duration::from_secs(1), queued: queued.clone(), ..idle_view() },
            SwitchView { active_queued: true, queued: queued.clone(), ..idle_view() },
            SwitchView { human_idle: Some(Duration::from_secs(3)), queued: queued.clone(), ..idle_view() },
            SwitchView { typed_ago: Some(Duration::from_secs(1)), queued: queued.clone(), ..idle_view() },
            SwitchView { workers_busy: true, queued: queued.clone(), ..idle_view() },
            SwitchView { pinned: true, queued: queued.clone(), ..idle_view() },
        ];
        for (i, v) in held_back.iter().enumerate() {
            assert_eq!(next_switch(v), None, "guard #{i}");
        }
        // A long-quiet person does not block; a want beats the queue and a pin.
        let v = SwitchView { human_idle: Some(HUMAN_QUIET), queued: queued.clone(), ..idle_view() };
        assert_eq!(next_switch(&v), Some(ops));
        let v = SwitchView { want: Some(home.clone()), pinned: true, queued, ..idle_view() };
        assert_eq!(next_switch(&v), Some(home));
    }

    /// Queued prompts for a conversation tab 1 is not running: filed under
    /// a minted id (a never-launched room) — or, for a v0.3.1 record with
    /// no id, held in memory — served oldest first, taken for argv.
    #[test]
    fn queues_for_not_running_conversations_serve_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let nest = ConvKey::room(BotId("swift".into()), "nest");
        store.enqueue(&cfg, &nest, "first");
        store.enqueue(&cfg, &nest, "second");
        let sid = store.state.record(&nest).and_then(|r| r.sid.clone()).expect("minted to queue under");
        assert!(!store.state.created(&nest), "queued, never launched");
        assert_eq!(store.signals.queued(&sid).len(), 2);
        assert!(store.oldest_queued(&nest).is_some());
        assert_eq!(store.take_oldest(&nest).as_deref(), Some("first"));
        assert_eq!(store.signals.queued(&sid).len(), 1);
        // Its first launch is FRESH under a new id, carrying the rest along.
        let Launch::Fresh { sid: new } = store.launch_mode(&cfg, &nest) else { panic!() };
        assert_eq!(store.signals.queued(&new).len(), 1);

        // A v0.3.1 home with no id: held in memory, same order.
        std::fs::write(cfg.dir.join("state.json"), r#"{"spawned":["swift"]}"#).unwrap();
        let mut legacy = AgentStore::new(&cfg);
        let home = ConvKey::home(key("swift", 1));
        legacy.enqueue(&cfg, &home, "[routine r] go");
        assert!(legacy.queued_with_prefix(&home, "[routine r]"));
        assert!(!legacy.queued_with_prefix(&home, "[routine q]"));
        assert_eq!(legacy.take_oldest(&home).as_deref(), Some("[routine r] go"));
        assert!(legacy.oldest_queued(&home).is_none());
    }

    /// Tab 1's conversation when nothing runs: the active one. A room's
    /// deletion forgets its conversations and queues, and a stopped bird
    /// active on it goes home at once.
    #[test]
    fn active_conversation_survives_restarts_and_room_deletion_sends_it_home() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift", "raven"]);
        let mut store = AgentStore::new(&cfg);
        let primary = key("swift", 1);
        let nest = ConvKey::room(BotId("swift".into()), "nest");
        assert_eq!(store.conv_of(&primary), ConvKey::home(primary.clone()));
        store.state.set_active(&cfg.dir, &nest);
        store.enqueue(&cfg, &ConvKey::room(BotId("raven".into()), "nest"), "hi");
        assert_eq!(AgentStore::new(&cfg).conv_of(&primary), nest, "persisted");
        assert_eq!(key("swift", 2), AgentStore::new(&cfg).conv_of(&key("swift", 2)).slot, "tabs run their own");
        assert_eq!(store.conversations(&BotId("swift".into())), [ConvKey::home(primary.clone())]);

        store.forget_room(&cfg, "nest");
        assert_eq!(store.conv_of(&primary), ConvKey::home(primary.clone()));
        assert!(store.state.room_convs(&BotId("raven".into())).is_empty());
        assert_eq!(std::fs::read_dir(cfg.inbox_dir()).map_or(0, |d| d.count()), 0, "queues dropped");
    }

    /// The person's pick on a stopped bird points tab 1 there at once.
    #[test]
    fn picking_a_conversation_for_a_stopped_bird_points_tab_one_there() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let nest = ConvKey::room(BotId("swift".into()), "nest");
        assert!(!store.pick_conversation(&cfg, &nest), "nothing to wait for");
        assert_eq!(store.conv_of(&key("swift", 1)), nest);
    }

    #[test]
    fn lowest_free_tab_fills_gaps() {
        assert_eq!(lowest_free_tab(&[1]), 2);
        assert_eq!(lowest_free_tab(&[1, 2, 4]), 3);
        assert_eq!(lowest_free_tab(&[1, 2, 3, 4]), 5);
    }

    #[test]
    fn status_layers_mod_over_output_over_poll_over_heuristic() {
        use BotStatus::*;
        let s = Duration::from_secs;
        // A live mod outranks everything, streaming output included.
        assert_eq!(pick_status(Some(NeedsInput), Some(s(0)), Some((StatusKind::Working, s(1))), 0), NeedsInput);
        assert_eq!(pick_status(Some(Failed), Some(s(0)), None, 0), Failed);
        assert_eq!(pick_status(Some(Done(7)), None, Some((StatusKind::Working, s(1))), 7), Done(7));
        // No mod: fresh output beats a laggy poll …
        assert_eq!(pick_status(None, Some(s(1)), Some((StatusKind::Done, s(1))), 0), Working);
        // … a fresh poll beats the heuristic …
        assert_eq!(pick_status(None, Some(s(3)), Some((StatusKind::NeedsInput, s(14))), 0), NeedsInput);
        assert_eq!(pick_status(None, Some(s(60)), Some((StatusKind::Working, s(14))), 0), Working);
        // … an observed Done wears the KIND's age, not the output's …
        assert_eq!(pick_status(None, Some(s(3)), Some((StatusKind::Done, s(3))), 300), Done(300));
        // … a stale poll is forgotten …
        assert_eq!(pick_status(None, Some(s(60)), Some((StatusKind::Working, s(16))), 0), Done(60));
        assert_eq!(pick_status(None, Some(s(3)), Some((StatusKind::Done, s(16))), 0), Working);
        // … and a session with no paint yet is working on its first one.
        assert_eq!(pick_status(None, None, None, 0), Working);
    }

    /// A status file keyed by the poll's session id reaches the bird whose
    /// key the poll named; a stale heartbeat stops counting.
    #[test]
    fn mod_status_joins_birds_by_session_id_while_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let swift = key("swift", 1);
        store.signals.set_session_id(&swift, "sid-1");
        let now = status_file::now_ms();
        let write = |state: &str, extra: &str, at: u64| {
            std::fs::write(
                cfg.status_dir().join("sid-1.json"),
                format!(r#"{{"v":1,"session_id":"sid-1","state":"{state}",{extra}"updated_at":{at}}}"#),
            )
            .unwrap();
        };
        write("needs-input", r#""reason":"permission","detail":"Bash · cargo test","context_percent":72.4,"cost_usd":1.5,"#, now);
        let (birds, workers) = store.apply_status_files();
        assert!(workers.is_empty());
        assert_eq!(birds.len(), 1, "the first word is a transition");
        assert_eq!(birds[0].key, swift);
        assert_eq!(birds[0].to, StatusKind::NeedsInput);
        assert!(store.mod_alive(&swift));
        assert_eq!(store.delivery(&swift), Delivery::Inbox("sid-1".into()));
        let d = store.detail(&swift);
        assert_eq!(d.reason.as_deref(), Some("permission · Bash · cargo test"));
        assert_eq!(d.context_percent, Some(72.4));
        assert_eq!(d.cost_usd, Some(1.5));
        // While the mod is alive the poll must not flap the kind.
        let polled = [SessionInfo {
            name: "aviary-swift".into(),
            status: "idle".into(),
            session_id: "sid-1".into(),
            ..Default::default()
        }];
        // (apply_poll only badges LIVE keys; none here — but the guard is the point.)
        assert!(store.apply_poll(&cfg, &polled).is_empty());
        assert!(store
            .apply_hook(&cfg, "", Some("aviary-swift"), "", "Stop", "")
            .is_none());
        // The same file, unchanged, is not news; a change is.
        assert!(store.apply_status_files().0.is_empty());
        write("done", "", now + 1);
        let (birds, _) = store.apply_status_files();
        assert_eq!(birds.len(), 1);
        assert_eq!(birds[0].to, StatusKind::Done);
        assert_eq!(store.detail(&swift).reason, None, "quiet birds carry no reason");
        // A stale heartbeat: the mod no longer speaks for the key.
        write("working", "", now - 20_000);
        store.apply_status_files();
        assert!(!store.mod_alive(&swift));
        assert_eq!(
            store.delivery(&swift),
            Delivery::Inbox("sid-1".into()),
            "one queue: a dead mod's inbox is typed by the sweep, in order"
        );
        // An ended session is not alive however fresh.
        write("ended", "", now);
        store.apply_status_files();
        assert!(!store.mod_alive(&swift));
        // A key the poll never named has nothing to join.
        assert_eq!(store.delivery(&key("swift", 2)), Delivery::Typed);
    }

    #[test]
    fn inbox_sweep_drops_acked_files_and_types_for_a_dead_mod() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let swift = key("swift", 1);
        store.signals.set_session_id(&swift, "sid-1");
        let mut stamp = status_file::PostStamp::default();
        let first = status_file::inbox_post(&cfg.inbox_dir(), "sid-1", &mut stamp, "one").unwrap();
        let second = status_file::inbox_post(&cfg.inbox_dir(), "sid-1", &mut stamp, "two").unwrap();
        // Alive and acked up to the first file: it goes, the second waits.
        std::fs::write(
            cfg.status_dir().join("sid-1.json"),
            format!(
                r#"{{"v":1,"state":"working","inbox_ack":"{first}","updated_at":{}}}"#,
                status_file::now_ms()
            ),
        )
        .unwrap();
        store.apply_status_files();
        assert_eq!(store.sweep_inbox(), 0, "a live mod gets nothing typed");
        let left = status_file::inbox_pending(&cfg.inbox_dir(), "sid-1", "");
        assert_eq!(left.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), [&second]);
        // Dead mod, but no live PTY for the key either: nothing to type into.
        std::fs::remove_file(cfg.status_dir().join("sid-1.json")).unwrap();
        store.apply_status_files();
        assert_eq!(store.sweep_inbox(), 0, "no live PTY to type into");
        assert_eq!(status_file::inbox_pending(&cfg.inbox_dir(), "sid-1", "").len(), 1, "kept for a session that may come back");
    }

    /// No event marks a permission dialog as answered; a poll a full period
    /// later that reads busy with nothing waiting means it was approved.
    #[test]
    fn a_later_busy_poll_clears_an_answered_permission_only() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let swift = key("swift", 1);
        store.signals.set_session_id(&swift, "sid-1");
        let write = |state: &str, reason: &str| {
            std::fs::write(
                cfg.status_dir().join("sid-1.json"),
                format!(
                    r#"{{"v":1,"state":"{state}","reason":"{reason}","detail":"Bash · cargo test","updated_at":{}}}"#,
                    status_file::now_ms()
                ),
            )
            .unwrap();
        };
        write("needs-input", "permission");
        store.apply_status_files();
        let blocked = |s: &AgentStore| s.mod_for(&swift).unwrap().state == "needs-input";
        assert!(blocked(&store));
        // A busy poll taken right after the dialog opened proves nothing.
        store.signals.set_poll(&swift, StatusKind::Working, Instant::now(), None);
        assert!(blocked(&store));
        // A full period later, busy with nothing waiting: approved, running.
        let changed = store.signals.changed_at("sid-1");
        store.signals.set_poll(&swift, StatusKind::Working, changed + status_file::POLL_LAG, None);
        assert!(!blocked(&store));
        assert_eq!(store.detail(&swift).reason, None);
        // Still waiting per the poll: the dialog is open.
        store.signals.set_poll(&swift, StatusKind::Working, changed + status_file::POLL_LAG, Some("permission prompt"));
        assert!(blocked(&store));
        store.signals.set_poll(&swift, StatusKind::Working, changed + status_file::POLL_LAG, None);
        // A heartbeat does not reset the clock; a NEW word does.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write("needs-input", "permission");
        store.apply_status_files();
        assert!(!blocked(&store), "same word, heartbeat only");
        write("needs-input", "question");
        store.apply_status_files();
        assert!(blocked(&store), "a question is never cleared by the poll");
        // A dialog the mod cannot see (MCP elicitation …): a fresh poll's
        // waitingFor turns the mod's done into needs-you, with the reason.
        write("done", "");
        store.apply_status_files();
        assert!(!blocked(&store));
        store.signals.set_poll(&swift, StatusKind::NeedsInput, Instant::now(), Some("dialog open"));
        assert!(blocked(&store));
        assert_eq!(store.detail(&swift).reason.as_deref(), Some("dialog open"));
    }

    /// The invariant that keeps conversations apart: a PINNED launch seeds
    /// its id and clears the slot's retired one, so the prompts queued for
    /// the conversation the slot ran before stay with THAT conversation.
    #[test]
    fn a_pinned_launch_never_inherits_the_previous_conversations_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let swift = key("swift", 1);
        store.signals.set_session_id(&swift, "home-sid");
        let mut stamp = status_file::PostStamp::default();
        status_file::inbox_post(&cfg.inbox_dir(), "home-sid", &mut stamp, "for home").unwrap();
        store.forget_observations(&swift); // the slot switches conversations…
        store.signals.seed(&swift, "room-sid"); // …to one launched under a pinned id
        assert_eq!(store.delivery(&swift), Delivery::Inbox("room-sid".into()), "inbox from birth");
        let row = SessionInfo { name: "aviary-swift".into(), status: "idle".into(), session_id: "room-sid".into(), ..Default::default() };
        store.signals.note_poll(&swift, &row);
        assert_eq!(status_file::inbox_pending(&cfg.inbox_dir(), "home-sid", "").len(), 1, "home keeps its queue");
        assert!(status_file::inbox_pending(&cfg.inbox_dir(), "room-sid", "").is_empty());
    }

    /// A relaunch drops the key's session id; the next id the poll names for
    /// that key inherits the prompts still queued for the old one.
    #[test]
    fn a_relaunch_carries_unsent_prompts_to_the_new_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::test_flock(tmp.path(), &["swift"]);
        let mut store = AgentStore::new(&cfg);
        let swift = key("swift", 1);
        store.signals.set_session_id(&swift, "old-sid");
        assert_eq!(store.delivery(&swift), Delivery::Inbox("old-sid".into()));
        let mut stamp = status_file::PostStamp::default();
        let queued = status_file::inbox_post(&cfg.inbox_dir(), "old-sid", &mut stamp, "hi").unwrap();
        store.forget_observations(&swift); // the exited-replace path
        assert_eq!(store.delivery(&swift), Delivery::Typed, "no id until the poll names the new one");
        let row = SessionInfo { name: "aviary-swift".into(), status: "idle".into(), session_id: "new-sid".into(), ..Default::default() };
        store.signals.note_poll(&swift, &row);
        let moved = status_file::inbox_pending(&cfg.inbox_dir(), "new-sid", "");
        assert_eq!(moved.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), [&queued]);
        assert!(!cfg.inbox_dir().join("old-sid").exists());
    }
}
