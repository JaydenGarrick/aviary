//! What aviary knows about each bird session BEYOND its PTY: the poll's word
//! (session id, busy/idle, `waitingFor`), the classic hooks' last needs-input,
//! and the in-session mod's status file and inbox. Owned by `AgentStore`
//! (like `flock::Workers`), keyed by `SessionKey`; the store keeps the PTYs
//! and the status layering, this keeps the signals.
//!
//! The decisions are pure and live in `status_file` (`ModStatus::reconcile`,
//! `PostStamp`); this is their per-key bookkeeping plus the bounded fs work
//! (one `read_dir` per tick, a read per changed file, plus one inbox
//! `read_dir` per dead-mod session) — UI-thread safe.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::command::SessionInfo;
use crate::config::{Config, SessionKey};
use crate::status::{map_status_str, BotStatus, Detail, StatusKind};
use crate::status_file::{self, ModStatus, PollView, PostStamp, StatusDir};

/// How a prompt reaches a session that is already running.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Delivery {
    /// The poll named the session: a file in `inbox/<sessionId>/` — ONE
    /// ordered queue. A live mod submits it; for a dead mod the store types
    /// it, one file per tick, never into a PTY younger than MARK_AFTER.
    Inbox(String),
    /// No session id yet (the first poll has not named it): keystrokes now.
    Typed,
}

/// A dead mod's inbox is typed only once its last heartbeat is this old, so
/// a stalled mod mid-`$.prompt.submit` and the typed fallback never both run
/// the same file.
const INBOX_TYPE_AFTER_MS: u64 = 2 * status_file::MOD_TRUST.as_millis() as u64;
/// A status record for an id nobody holds is kept this long after it last
/// changed (it may still join a key the next poll names).
const UNHELD_TTL: Duration = Duration::from_secs(10 * 60);

/// One status file that changed this tick, joined to a bird key if the poll
/// has named its session.
pub struct ModChange {
    pub session_id: String,
    pub status: ModStatus,
    pub key: Option<SessionKey>,
}

pub struct ModLayer {
    /// Claude's session id per key, from the poll — the join key for the
    /// mod's `status/<sessionId>.json` and the inbox.
    session_ids: HashMap<SessionKey, String>,
    /// The id a key had when its session went away (stop, exit, relaunch):
    /// the next id the poll names for that key inherits its unsent prompts.
    retired: HashMap<SessionKey, String>,
    /// The latest poll kind per key, and when — kept even while the mod is
    /// alive, for the two places it may speak (`ModStatus::reconcile`).
    polled: HashMap<SessionKey, (StatusKind, Instant)>,
    /// The poll's `waitingFor` per key (absent unless waiting).
    waiting_for: HashMap<SessionKey, String>,
    /// When a classic hook last said needs-input: a busy poll begun before
    /// that dialog opened must not overwrite it (no mod case).
    hook_blocked: HashMap<SessionKey, Instant>,
    /// The mod's latest status per session id (birds AND workers), with when
    /// its state/reason/detail last CHANGED (heartbeats do not).
    mod_status: HashMap<String, (ModStatus, Instant)>,
    status_dir: StatusDir,
    status_path: PathBuf,
    inbox_path: PathBuf,
    stamp: PostStamp,
}

impl ModLayer {
    pub fn new(cfg: &Config) -> ModLayer {
        ModLayer {
            session_ids: HashMap::new(),
            retired: HashMap::new(),
            polled: HashMap::new(),
            waiting_for: HashMap::new(),
            hook_blocked: HashMap::new(),
            mod_status: HashMap::new(),
            status_dir: StatusDir::default(),
            status_path: cfg.status_dir(),
            inbox_path: cfg.inbox_dir(),
            stamp: PostStamp::default(),
        }
    }

    // ------------------------------------------------------------- poll

    /// Record one poll row for a live key; returns the kind it read. A new
    /// session id for the same key (a /clear, a relaunch) carries prompts
    /// already queued for the old id across, or nothing would drain them.
    pub fn note_poll(&mut self, key: &SessionKey, row: &SessionInfo) -> StatusKind {
        let kind = map_status_str(row.status_str());
        self.polled.insert(key.clone(), (kind, Instant::now()));
        if !row.session_id.is_empty() {
            let old = self
                .session_ids
                .insert(key.clone(), row.session_id.clone())
                .or_else(|| self.retired.remove(key));
            if let Some(old) = old {
                if old != row.session_id {
                    status_file::inbox_move(&self.inbox_path, &old, &row.session_id);
                }
            }
        }
        if row.waiting_for.is_empty() {
            self.waiting_for.remove(key);
        } else {
            self.waiting_for.insert(key.clone(), row.waiting_for.clone());
        }
        kind
    }

    /// A busy poll that may predate the dialog a hook just reported.
    pub fn poll_predates_hook(&self, key: &SessionKey, kind: StatusKind) -> bool {
        kind == StatusKind::Working
            && self
                .hook_blocked
                .get(key)
                .is_some_and(|at| at.elapsed() < status_file::POLL_LAG)
    }

    /// Record a classic hook's verdict for a key.
    pub fn note_hook(&mut self, key: &SessionKey, kind: StatusKind) {
        if kind == StatusKind::NeedsInput {
            self.hook_blocked.insert(key.clone(), Instant::now());
        } else {
            self.hook_blocked.remove(key);
        }
    }

    // -------------------------------------------------------------- mod

    /// The mod's word for a key, only while its heartbeat is fresh, weighed
    /// against the poll by `ModStatus::reconcile`.
    pub fn mod_for(&self, key: &SessionKey) -> Option<ModStatus> {
        let sid = self.session_ids.get(key)?;
        let (st, changed) = self.mod_status.get(sid)?;
        if !st.is_alive(status_file::now_ms()) {
            return None;
        }
        let poll = self.polled.get(key).map(|(kind, at)| PollView {
            busy: *kind == StatusKind::Working,
            at: *at,
            waiting_for: self.waiting_for.get(key).cloned(),
        });
        Some(st.reconcile(*changed, poll.as_ref()))
    }

    /// Read every status file that changed since the last tick, drop the
    /// inbox files each one acks (acked is acked, alive or not), and return
    /// the changes joined to bird keys.
    pub fn scan(&mut self) -> Vec<ModChange> {
        let mut out = Vec::new();
        for (sid, st) in self.status_dir.scan(&self.status_path) {
            let changed = match self.mod_status.get(&sid) {
                Some((prev, at)) if prev.same_word(&st) => *at, // a heartbeat
                _ => Instant::now(),
            };
            status_file::inbox_sweep(&self.inbox_path, &sid, &st.inbox_ack);
            self.mod_status.insert(sid.clone(), (st.clone(), changed));
            let key = self
                .session_ids
                .iter()
                .find(|(_, s)| **s == sid)
                .map(|(k, _)| k.clone());
            out.push(ModChange { session_id: sid, status: st, key });
        }
        out
    }

    /// The chip's fine print: the mod's reason and figures while it is alive,
    /// else the poll's `waitingFor` while `blocked`.
    pub fn detail(&self, key: &SessionKey, blocked: bool) -> Detail {
        if let Some(st) = self.mod_for(key) {
            let mod_blocked = matches!(st.status(0), Some(BotStatus::NeedsInput | BotStatus::Failed));
            return Detail {
                reason: mod_blocked.then(|| st.reason_text()).flatten(),
                context_percent: st.context_percent,
                cost_usd: st.cost_usd,
            };
        }
        Detail {
            reason: blocked.then(|| self.waiting_for.get(key).cloned()).flatten(),
            context_percent: None,
            cost_usd: None,
        }
    }

    // ------------------------------------------------------------ inbox

    /// How a prompt should reach this (running) session: the inbox whenever
    /// the poll has named it, mod alive or not — one ordered queue.
    pub fn delivery(&self, key: &SessionKey) -> Delivery {
        match self.session_ids.get(key) {
            Some(sid) => Delivery::Inbox(sid.clone()),
            None => Delivery::Typed,
        }
    }

    /// Queue a prompt for a session id; false when the write failed (the
    /// caller types it instead).
    pub fn post(&mut self, session_id: &str, text: &str) -> bool {
        status_file::inbox_post(&self.inbox_path, session_id, &mut self.stamp, text).is_ok()
    }

    /// The keys whose mod is NOT alive and quiet long enough that typing is
    /// safe, each with the oldest prompt its mod never acked — taken out of
    /// the inbox. The store decides whether the PTY can take it.
    pub fn take_for_typing(&mut self, can_type: impl Fn(&SessionKey) -> bool) -> Vec<(SessionKey, String)> {
        let now_ms = status_file::now_ms();
        let mut out = Vec::new();
        for (key, sid) in &self.session_ids {
            if self.mod_for(key).is_some() || !can_type(key) {
                continue;
            }
            // A mod that went quiet may be mid-submit: wait out a margin.
            let record = self.mod_status.get(sid).map(|(st, _)| st);
            if record.is_some_and(|st| now_ms.saturating_sub(st.updated_at) <= INBOX_TYPE_AFTER_MS) {
                continue;
            }
            // Past the last ack only: a mod that died after submitting must
            // not have it typed a second time.
            let ack = record.map_or("", |st| st.inbox_ack.as_str());
            if let Some((_, path)) = status_file::inbox_pending(&self.inbox_path, sid, ack).into_iter().next() {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let _ = std::fs::remove_file(&path);
                    out.push((key.clone(), text));
                }
            }
        }
        out
    }

    // ------------------------------------------------------------ upkeep

    /// Sweep files nobody holds (birds here + `extra` worker ids) and the
    /// records behind them.
    pub fn gc(&mut self, extra: impl IntoIterator<Item = String>) {
        let live: HashSet<String> = self.session_ids.values().cloned().chain(extra).collect();
        status_file::gc(&self.status_path, &self.inbox_path, &live);
        self.mod_status
            .retain(|sid, (_, changed)| live.contains(sid) || changed.elapsed() < UNHELD_TTL);
    }

    /// THE one place per-key signals are dropped — every per-key map here.
    /// A forgotten key's id is retired, not lost — see `retired`.
    pub fn forget_where(&mut self, gone: impl Fn(&SessionKey) -> bool) {
        let retired: Vec<(SessionKey, String)> = self
            .session_ids
            .iter()
            .filter(|(k, _)| gone(k))
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        self.retired.extend(retired);
        self.session_ids.retain(|k, _| !gone(k));
        self.polled.retain(|k, _| !gone(k));
        self.waiting_for.retain(|k, _| !gone(k));
        self.hook_blocked.retain(|k, _| !gone(k));
    }

    // ------------------------------------------------------- test seams

    #[cfg(test)]
    pub fn set_session_id(&mut self, key: &SessionKey, sid: &str) {
        self.session_ids.insert(key.clone(), sid.into());
    }

    #[cfg(test)]
    pub fn set_poll(&mut self, key: &SessionKey, kind: StatusKind, at: Instant, waiting: Option<&str>) {
        self.polled.insert(key.clone(), (kind, at));
        match waiting {
            Some(w) => self.waiting_for.insert(key.clone(), w.into()),
            None => self.waiting_for.remove(key),
        };
    }

    #[cfg(test)]
    pub fn changed_at(&self, sid: &str) -> Instant {
        self.mod_status[sid].1
    }
}
