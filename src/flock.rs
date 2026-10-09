//! The flock: worker sessions a bird spawns ITSELF (`claude --bg --worktree
//! <slug> --name aviary-<bird>_<slug>-<role> …`), attributed from the
//! `claude agents --json` poll by NAME. Aviary never spawns a worker; it
//! only watches, attaches, and stops.
//!
//! Name grammar: `aviary-<bird>_<slug>-<role>` — `_` is the worker separator
//! (`slug()` never emits it, so a bird id can never contain one), `.` is the
//! tab separator (never present in a worker name), and the role is the last
//! `-` segment. A worker is only attributed when `<bird>` is a configured
//! bird, so a stranger's `aviary-x_y-impl` shows nowhere.
//!
//! Status, most-truthful-first — the report file refines "idle":
//!   1. poll stopped/exited/completed → Exited,
//!   2. a live mod (its status file, via `ModStatus::reconcile` — the same
//!      rule the birds wear) → its running state; its `done` goes to 4/5,
//!   3. poll busy → Working · poll waiting/blocked → NeedsInput,
//!   4. idle + a report → its `status:` (done · needs-input · failed · in-progress),
//!   5. idle, no report → Done (aged since the kind last changed).
//!
//! A worker leaves the roster after [`MISSED_POLLS_BEFORE_DROP`] polls without
//! it: the executor delivers an EMPTY poll on any failure, and one such poll
//! must never wipe the flock.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::time::{Instant, SystemTime};

use crate::status::{map_status_str, BotStatus, StatusKind};
use crate::command::SessionInfo;
use crate::config::{Bot, BotId};
use crate::status_file::{self, ModStatus};

/// Separates the bird id from the workstream in a worker's session name.
pub const SEP: char = '_';
const PREFIX: &str = "aviary-";
/// Polls a worker may be missing from before its row is dropped (≈ 9 s at
/// the 3 s cadence).
pub const MISSED_POLLS_BEFORE_DROP: u8 = 3;
/// A report larger than this is not a report.
const REPORT_CAP: u64 = 64 * 1024;
/// `status:` must appear within the first lines of a report.
const REPORT_HEAD_LINES: usize = 40;

// ------------------------------------------------------------------ names

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Impl,
    Research,
    Review,
    Pr,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Impl, Role::Research, Role::Review, Role::Pr];

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Impl => "impl",
            Role::Research => "research",
            Role::Review => "review",
            Role::Pr => "pr",
        }
    }
}

/// A parsed worker session name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkerName {
    pub bot: BotId,
    pub slug: String,
    pub role: Role,
}

impl WorkerName {
    /// `aviary-<bot>_<slug>-<role>`; `None` for everything else — birds,
    /// tabs, auto-named sessions like `aviary-0c`, and a collision-renamed
    /// worker (`…-impl-graceful-unicorn`), whose last segment is no role.
    pub fn parse(name: &str) -> Option<WorkerName> {
        let rest = name.strip_prefix(PREFIX)?;
        let (bot, tail) = rest.split_once(SEP)?;
        if bot.is_empty() || bot.contains('.') {
            return None;
        }
        let (slug, role) = tail.rsplit_once('-')?;
        let role = Role::parse(role)?;
        if !valid_slug(slug) {
            return None;
        }
        Some(WorkerName {
            bot: BotId(bot.to_string()),
            slug: slug.to_string(),
            role,
        })
    }

    /// The roster row / tab label: `<slug>-<role>`.
    pub fn label(&self) -> String {
        format!("{}-{}", self.slug, self.role.as_str())
    }
}

impl fmt::Display for WorkerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}{SEP}{}-{}", self.bot, self.slug, self.role.as_str())
    }
}

/// `[a-z0-9]([a-z0-9-]*[a-z0-9])?` — what the orchestrator skill allows.
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

// ---------------------------------------------------------------- reports

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReportStatus {
    Done,
    NeedsInput,
    Failed,
    InProgress,
}

/// The first `status:` line in a report's head. Case-insensitive; tolerates
/// list markers, `**status:**`, backticks, `needs_input` / `needs input`,
/// and trailing words. The untouched template line (`done | needs-input |
/// …`) and unknown values read as no status at all.
pub fn parse_report(text: &str) -> Option<ReportStatus> {
    for line in text.lines().take(REPORT_HEAD_LINES) {
        let lower = line.trim().to_ascii_lowercase();
        let lower = lower.trim_start_matches(['-', '*', '>', '#', '_', ' ']);
        let Some(rest) = lower.strip_prefix("status") else {
            continue;
        };
        let rest = rest.trim_start_matches(['*', '_', ' ']);
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        if rest.contains('|') {
            return None; // the template's menu of values, not a value
        }
        let rest = rest.trim_start_matches(['*', '_', ' ', '`']);
        let value: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' '))
            .collect();
        let value = value.trim().replace([' ', '_'], "-");
        return match value.as_str() {
            "done" => Some(ReportStatus::Done),
            "needs-input" => Some(ReportStatus::NeedsInput),
            "failed" => Some(ReportStatus::Failed),
            "in-progress" => Some(ReportStatus::InProgress),
            _ => None,
        };
    }
    None
}

// ---------------------------------------------------------------- workers

/// What the poll said about a worker, mapped conservatively.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PollKind {
    Working,
    NeedsInput,
    Idle,
    Exited,
}

/// stopped/exited/completed/killed → Exited; the rest through
/// [`map_status_str`], so an unknown value stays quiet (Idle).
fn map_worker_status(s: &str) -> PollKind {
    let l = s.to_ascii_lowercase();
    if ["stopped", "exited", "completed", "killed", "dead", "terminated"]
        .iter()
        .any(|w| l.contains(w))
    {
        return PollKind::Exited;
    }
    match map_status_str(&l) {
        StatusKind::Working => PollKind::Working,
        StatusKind::NeedsInput => PollKind::NeedsInput,
        StatusKind::Done => PollKind::Idle,
    }
}

struct Report {
    status: Option<ReportStatus>,
    mtime: SystemTime,
}

pub struct Worker {
    pub name: WorkerName,
    /// The full session name — the map key, the report file stem, and the
    /// SendMessage address.
    pub session_name: String,
    /// The short id `claude attach|logs|stop|rm` take.
    pub id: String,
    /// Claude's full session id — the mod's status file is named by it.
    pub session_id: String,
    pub cwd: String,
    poll: (PollKind, Instant),
    /// When the coarse kind last changed — the chip's age while idle.
    kind_since: (StatusKind, Instant),
    report: Option<Report>,
    /// The mod's latest word, from `status/<sessionId>.json`, and when its
    /// state/reason/detail last changed (heartbeats do not count).
    mod_state: Option<ModStatus>,
    mod_changed: Instant,
    /// The poll row's `waitingFor` (empty unless waiting).
    waiting_for: String,
    missed: u8,
    unread: bool,
}

/// A status change worth reacting to (unread dot, notification).
pub struct WorkerTransition {
    pub session_name: String,
    pub bot: BotId,
    pub label: String,
    pub from: StatusKind,
    pub to: StatusKind,
    /// The full status behind `to` (Exited and Failed matter to the shell).
    pub status: BotStatus,
}

#[derive(Default)]
pub struct Workers {
    by_name: HashMap<String, Worker>,
}

impl Workers {
    /// Fold one poll in: attribute rows whose name parses to a CONFIGURED
    /// bird, keep the best row per name, age the absentees, and return the
    /// transitions. A worker's first sight is silent — discovering one that
    /// is already idle is not news.
    pub fn apply_poll(&mut self, bots: &[Bot], rows: &[SessionInfo]) -> Vec<WorkerTransition> {
        let now = Instant::now();
        let mut present: HashSet<String> = HashSet::new();
        for (name, row) in best_rows(rows) {
            let Some(parsed) = WorkerName::parse(name) else {
                continue;
            };
            if !bots.iter().any(|b| b.id == parsed.bot) {
                continue;
            }
            present.insert(name.to_string());
            let kind = map_worker_status(row.status_str());
            let id = row.short_id().unwrap_or_default();
            match self.by_name.get_mut(name) {
                Some(w) => {
                    w.poll = (kind, now);
                    w.missed = 0;
                    if !id.is_empty() {
                        w.id = id;
                    }
                    if !row.session_id.is_empty() {
                        w.session_id = row.session_id.clone();
                    }
                    w.cwd = row.cwd.clone();
                    w.waiting_for = row.waiting_for.clone();
                }
                None => {
                    let mut w = Worker {
                        name: parsed,
                        session_name: name.to_string(),
                        id,
                        session_id: row.session_id.clone(),
                        cwd: row.cwd.clone(),
                        poll: (kind, now),
                        kind_since: (StatusKind::Done, now),
                        report: None,
                        mod_state: None,
                        mod_changed: now,
                        waiting_for: row.waiting_for.clone(),
                        missed: 0,
                        unread: false,
                    };
                    w.kind_since = (derive(&w).kind(), now);
                    self.by_name.insert(name.to_string(), w);
                }
            }
        }

        let mut gone = Vec::new();
        for (name, w) in self.by_name.iter_mut() {
            if !present.contains(name) {
                w.missed += 1;
                if w.missed >= MISSED_POLLS_BEFORE_DROP {
                    gone.push(name.clone());
                }
            }
        }
        for name in gone {
            self.by_name.remove(&name);
        }

        let mut names: Vec<String> = present.into_iter().collect();
        names.sort();
        names.iter().filter_map(|n| self.settle(n)).collect()
    }

    /// Each tick: stat every known worker's `<session_name>.md`; re-read and
    /// re-parse only when its mtime changed (a report is replaced whole, not
    /// appended). Bounded local fs work — stays on the UI thread.
    pub fn tick_reports(&mut self, reports_dir: &Path) -> Vec<WorkerTransition> {
        let mut names: Vec<String> = self.by_name.keys().cloned().collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let path = reports_dir.join(format!("{name}.md"));
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let w = self.by_name.get_mut(&name).expect("known name");
            if w.report.as_ref().is_some_and(|r| r.mtime == mtime) {
                continue;
            }
            let status = if meta.len() <= REPORT_CAP {
                std::fs::read_to_string(&path).ok().and_then(|t| parse_report(&t))
            } else {
                None
            };
            // Remember the mtime even when unparseable, so junk is read once.
            w.report = Some(Report { status, mtime });
            if let Some(t) = self.settle(&name) {
                out.push(t);
            }
        }
        out
    }

    /// Fold in the mod's status file for one session id (the worker it
    /// names, if we hold it); returns the transition, as the poll does.
    pub fn apply_mod(&mut self, session_id: &str, status: &ModStatus) -> Option<WorkerTransition> {
        let name = self
            .by_name
            .values()
            .find(|w| w.session_id == session_id)
            .map(|w| w.session_name.clone())?;
        let w = self.by_name.get_mut(&name)?;
        let same = w.mod_state.as_ref().is_some_and(|p| p.same_word(status));
        if !same {
            w.mod_changed = Instant::now();
        }
        w.mod_state = Some(status.clone());
        self.settle(&name)
    }

    /// Every worker's Claude session id (the GC's "still held" set).
    pub fn session_ids(&self) -> Vec<String> {
        self.by_name
            .values()
            .map(|w| w.session_id.clone())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// The chip for one worker; `NotStarted` for a name we do not hold.
    pub fn status(&self, session_name: &str) -> BotStatus {
        self.by_name
            .get(session_name)
            .map(derive)
            .unwrap_or(BotStatus::NotStarted)
    }

    pub fn get(&self, session_name: &str) -> Option<&Worker> {
        self.by_name.get(session_name)
    }

    /// A bird's workers, sorted by label so rows never jump.
    pub fn for_bot(&self, bot: &BotId) -> Vec<&Worker> {
        let mut v: Vec<&Worker> = self.by_name.values().filter(|w| w.name.bot == *bot).collect();
        v.sort_by_key(|w| w.name.label());
        v
    }

    pub fn has_working(&self) -> bool {
        self.by_name
            .values()
            .any(|w| matches!(derive(w), BotStatus::Working))
    }

    pub fn mark_unread(&mut self, session_name: &str) {
        if let Some(w) = self.by_name.get_mut(session_name) {
            w.unread = true;
        }
    }

    pub fn clear_unread(&mut self, session_name: &str) {
        if let Some(w) = self.by_name.get_mut(session_name) {
            w.unread = false;
        }
    }

    pub fn is_unread(&self, session_name: &str) -> bool {
        self.by_name.get(session_name).is_some_and(|w| w.unread)
    }

    /// A bird leaving the roster takes its worker rows with it (the sessions
    /// themselves keep running — they are not ours to stop).
    pub fn forget_bot(&mut self, bot: &BotId) {
        self.by_name.retain(|_, w| w.name.bot != *bot);
    }

    /// Re-derive one worker's status; a change of coarse kind is a transition.
    fn settle(&mut self, session_name: &str) -> Option<WorkerTransition> {
        let w = self.by_name.get_mut(session_name)?;
        let status = derive(w);
        let to = status.kind();
        let (from, _) = w.kind_since;
        if from == to {
            return None;
        }
        w.kind_since = (to, Instant::now());
        Some(WorkerTransition {
            session_name: w.session_name.clone(),
            bot: w.name.bot.clone(),
            label: w.name.label(),
            from,
            to,
            status,
        })
    }
}

/// The precedence rule (module doc).
/// The poll owns exit; a live mod owns the running states (it saw the turn
/// start, the dialog open, the turn die) and hands "done" to the idle
/// branch; else the poll's live states; else idle, which the report refines.
fn derive(w: &Worker) -> BotStatus {
    // The poll speaks over a live mod only where `ModStatus::reconcile`
    // says — the same rule the birds wear.
    let poll = status_file::PollView {
        busy: w.poll.0 == PollKind::Working,
        at: w.poll.1,
        waiting_for: (!w.waiting_for.is_empty()).then(|| w.waiting_for.clone()),
    };
    let modded = w
        .mod_state
        .as_ref()
        .filter(|s| s.is_alive(status_file::now_ms()))
        .and_then(|s| s.reconcile(w.mod_changed, Some(&poll)).status(0));
    match (w.poll.0, modded) {
        (PollKind::Exited, _) => BotStatus::Exited,
        (_, Some(BotStatus::Done(_))) => idle(w),
        (_, Some(live)) => live,
        (PollKind::Working, None) => BotStatus::Working,
        (PollKind::NeedsInput, None) => BotStatus::NeedsInput,
        (PollKind::Idle, None) => idle(w),
    }
}

/// Idle, refined by the report: done · needs-input · failed · in-progress.
fn idle(w: &Worker) -> BotStatus {
    match w.report.as_ref().and_then(|r| r.status.map(|s| (s, r.mtime))) {
        Some((status, mtime)) => {
            let age = mtime.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            match status {
                ReportStatus::Done => BotStatus::Done(age),
                ReportStatus::NeedsInput => BotStatus::NeedsInput,
                ReportStatus::Failed => BotStatus::Failed,
                ReportStatus::InProgress => BotStatus::Paused(age),
            }
        }
        None => BotStatus::Done(w.kind_since.1.elapsed().as_secs()),
    }
}

/// One row per name: prefer a row that is not exited, then a background
/// one, then the newest — a stale duplicate never shadows the live worker.
fn best_rows(rows: &[SessionInfo]) -> Vec<(&str, &SessionInfo)> {
    let mut best: HashMap<&str, &SessionInfo> = HashMap::new();
    for row in rows {
        if row.name.is_empty() {
            continue;
        }
        let better = match best.get(row.name.as_str()) {
            None => true,
            Some(cur) => rank_row(row) > rank_row(cur),
        };
        if better {
            best.insert(&row.name, row);
        }
    }
    let mut v: Vec<(&str, &SessionInfo)> = best.into_iter().collect();
    v.sort_by_key(|(n, _)| *n);
    v
}

fn rank_row(r: &SessionInfo) -> (bool, bool, u64) {
    (
        map_worker_status(r.status_str()) != PollKind::Exited,
        r.kind == "background",
        r.started_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SessionKey;

    fn bot(id: &str) -> Bot {
        Bot {
            id: BotId(id.into()),
            name: id.into(),
            glyph: "🪶".into(),
            repo: "/tmp".into(),
            persona: format!("birds/{id}.md"),
            notify: true,
            permissions: None,
            routines: Vec::new(),
        }
    }

    fn row(name: &str, status: &str) -> SessionInfo {
        SessionInfo {
            name: name.into(),
            status: status.into(),
            id: "2d1601e0".into(),
            session_id: format!("{name}-uuid"),
            kind: "background".into(),
            started_at: 10,
            ..Default::default()
        }
    }

    #[test]
    fn mod_state_refines_worker_status_while_alive() {
        let bots = [bot("swift")];
        let mut w = Workers::default();
        let name = "aviary-swift_x-impl";
        w.apply_poll(&bots, &[row(name, "idle")]);
        assert_eq!(w.status(name), BotStatus::Done(0));
        assert_eq!(w.session_ids(), vec![format!("{name}-uuid")]);
        let now = status_file::now_ms();
        let st = |state: &str, at: u64| {
            ModStatus::parse(&format!(r#"{{"v":1,"state":"{state}","updated_at":{at}}}"#)).unwrap()
        };
        // Unknown session id: nobody's.
        assert!(w.apply_mod("stranger", &st("working", now)).is_none());
        // The mod saw the turn start before the poll did.
        let t = w.apply_mod(&format!("{name}-uuid"), &st("working", now)).unwrap();
        assert_eq!(t.to, StatusKind::Working);
        assert_eq!(w.status(name), BotStatus::Working);
        // A laggy poll saying idle does not win against a live mod.
        assert!(w.apply_poll(&bots, &[row(name, "idle")]).is_empty());
        assert_eq!(w.status(name), BotStatus::Working);
        // The mod's failed wears Failed; its done falls to the report branch.
        w.apply_mod(&format!("{name}-uuid"), &st("failed", now));
        assert_eq!(w.status(name), BotStatus::Failed);
        w.apply_mod(&format!("{name}-uuid"), &st("done", now));
        assert!(matches!(w.status(name), BotStatus::Done(_)));
        // An approved permission: a busy poll a full period later clears it.
        w.apply_mod(&format!("{name}-uuid"), &ModStatus::parse(&format!(
            r#"{{"v":1,"state":"needs-input","reason":"permission","updated_at":{now}}}"#
        )).unwrap());
        w.apply_poll(&bots, &[row(name, "busy")]);
        assert_eq!(w.status(name), BotStatus::NeedsInput, "a poll right after proves nothing");
        w.by_name.get_mut(name).unwrap().mod_changed -= status_file::POLL_LAG;
        w.apply_poll(&bots, &[row(name, "busy")]);
        assert_eq!(w.status(name), BotStatus::Working);
        // A stale mod hands back to the poll …
        w.apply_mod(&format!("{name}-uuid"), &st("needs-input", now - 20_000));
        w.apply_poll(&bots, &[row(name, "busy")]);
        assert_eq!(w.status(name), BotStatus::Working);
        // … and the poll's exit beats a mod however fresh.
        w.apply_mod(&format!("{name}-uuid"), &st("working", now));
        w.apply_poll(&bots, &[row(name, "stopped")]);
        assert_eq!(w.status(name), BotStatus::Exited);
    }

    #[test]
    fn worker_names_parse_only_the_grammar() {
        let ok = WorkerName::parse("aviary-swift_x-impl").unwrap();
        assert_eq!(ok.bot, BotId("swift".into()));
        assert_eq!(ok.slug, "x");
        assert_eq!(ok.role, Role::Impl);
        let multi = WorkerName::parse("aviary-swift_a-b-c-review").unwrap();
        assert_eq!(multi.slug, "a-b-c");
        assert_eq!(multi.role, Role::Review);
        for bad in [
            "aviary-swift",                          // a bird
            "aviary-swift.2",                        // a tab
            "aviary-0c",                             // auto-named stranger
            "aviary-00",                             // auto-named stranger
            "aviary-swift_x-impl-graceful-unicorn",  // collision-renamed
            "aviary-sw.ift_x-impl",                  // tab marker in the bird
            "aviary-_x-impl",                        // empty bird
            "aviary-swift_-impl",                    // empty slug
            "aviary-swift_x-boss",                   // unknown role
            "aviary-swift_X-impl",                   // uppercase slug
            "aviary-swift_x-impl.2",                 // tab marker after the role
            "aviary-swift_x_y-impl",                 // second separator in the slug
            "swift_x-impl",                          // no prefix
            "",
        ] {
            assert!(WorkerName::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn worker_name_display_round_trips_and_labels() {
        for name in ["aviary-swift_x-impl", "aviary-raven_auth-refresh-pr", "aviary-a-b_c-research"] {
            let parsed = WorkerName::parse(name).unwrap();
            assert_eq!(parsed.to_string(), name);
        }
        assert_eq!(WorkerName::parse("aviary-swift_auth-refresh-impl").unwrap().label(), "auth-refresh-impl");
    }

    #[test]
    fn worker_grammar_is_disjoint_from_session_keys() {
        for worker in ["aviary-swift_x-impl", "aviary-swift_a-b-review"] {
            assert!(WorkerName::parse(worker).is_some());
            assert!(SessionKey::parse_session_name(worker).is_none(), "{worker} is not a bird");
        }
        for session in ["aviary-swift", "aviary-swift.2", "aviary-night-jar.3"] {
            assert!(SessionKey::parse_session_name(session).is_some());
            assert!(WorkerName::parse(session).is_none(), "{session} is not a worker");
        }
    }

    #[test]
    fn report_parser_finds_the_first_status_line() {
        use ReportStatus::*;
        assert_eq!(parse_report("# x report\n\nstatus: done\n"), Some(Done));
        assert_eq!(parse_report("**status:** needs-input"), Some(NeedsInput));
        assert_eq!(parse_report("- Status: Needs Input"), Some(NeedsInput));
        assert_eq!(parse_report("status: in_progress — more steps"), Some(InProgress));
        assert_eq!(parse_report("status: `failed`"), Some(Failed));
        assert_eq!(parse_report("status: done (all checks pass)"), Some(Done));
        assert_eq!(parse_report("---\nname: x\n---\n# r\nstatus: done"), Some(Done));
        assert_eq!(parse_report("status: done | needs-input | failed | in-progress"), None, "the template menu");
        assert_eq!(parse_report("status: finished"), None);
        assert_eq!(parse_report("no status here"), None);
        let late = format!("{}status: done", "line\n".repeat(REPORT_HEAD_LINES));
        assert_eq!(parse_report(&late), None, "beyond the head");
    }

    #[test]
    fn attribution_requires_a_configured_bird() {
        let mut flock = Workers::default();
        let bots = [bot("swift")];
        let rows = [
            row("aviary-swift_x-impl", "busy"),
            row("aviary-raven_y-impl", "busy"),  // no such bird
            row("aviary-swift", "busy"),         // the bird itself
            row("aviary-swift.2", "idle"),       // a tab
        ];
        let t = flock.apply_poll(&bots, &rows);
        assert!(t.is_empty(), "first sight is silent");
        assert_eq!(flock.for_bot(&BotId("swift".into())).len(), 1);
        assert!(flock.get("aviary-raven_y-impl").is_none());
        assert_eq!(flock.status("aviary-swift_x-impl"), BotStatus::Working);
        assert_eq!(flock.get("aviary-swift_x-impl").unwrap().id, "2d1601e0");
    }

    #[test]
    fn empty_polls_keep_workers_until_the_grace() {
        let mut flock = Workers::default();
        let bots = [bot("swift")];
        flock.apply_poll(&bots, &[row("aviary-swift_x-impl", "idle")]);
        for _ in 1..MISSED_POLLS_BEFORE_DROP {
            flock.apply_poll(&bots, &[]);
            assert!(flock.get("aviary-swift_x-impl").is_some(), "a failed poll never wipes the flock");
        }
        flock.apply_poll(&bots, &[]);
        assert!(flock.get("aviary-swift_x-impl").is_none(), "gone after the grace");
    }

    #[test]
    fn duplicate_names_prefer_live_then_background_then_newest() {
        let mut flock = Workers::default();
        let bots = [bot("swift")];
        let mut stale = row("aviary-swift_x-impl", "stopped");
        stale.id = "old".into();
        stale.started_at = 99;
        let mut interactive = row("aviary-swift_x-impl", "busy");
        interactive.kind = "interactive".into();
        interactive.id = "view".into();
        let mut newest = row("aviary-swift_x-impl", "idle");
        newest.started_at = 20;
        newest.id = "new".into();
        flock.apply_poll(&bots, &[stale, interactive, newest.clone(), row("aviary-swift_x-impl", "idle")]);
        assert_eq!(flock.get("aviary-swift_x-impl").unwrap().id, "new");
        assert_eq!(flock.status("aviary-swift_x-impl"), BotStatus::Done(0));
    }

    #[test]
    fn report_precedence_cases() {
        let mut flock = Workers::default();
        let bots = [bot("swift")];
        let name = "aviary-swift_x-impl";
        flock.apply_poll(&bots, &[row(name, "busy")]);
        let now = SystemTime::now();
        let set = |flock: &mut Workers, status| {
            flock.by_name.get_mut(name).unwrap().report = Some(Report { status, mtime: now });
        };
        // busy beats any report
        set(&mut flock, Some(ReportStatus::Done));
        assert_eq!(flock.status(name), BotStatus::Working);
        // idle + done → done, with a transition Working → Done
        let t = flock.apply_poll(&bots, &[row(name, "idle")]);
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].from, t[0].to), (StatusKind::Working, StatusKind::Done));
        assert!(matches!(flock.status(name), BotStatus::Done(_)));
        // a permission prompt beats a done report
        flock.apply_poll(&bots, &[row(name, "waiting_for_input")]);
        assert_eq!(flock.status(name), BotStatus::NeedsInput);
        // idle + failed → Failed, coarse NeedsInput (no transition: already attention)
        set(&mut flock, Some(ReportStatus::Failed));
        let t = flock.apply_poll(&bots, &[row(name, "idle")]);
        assert!(t.is_empty());
        assert_eq!(flock.status(name), BotStatus::Failed);
        // idle + in-progress → Paused (quiet)
        set(&mut flock, Some(ReportStatus::InProgress));
        let t = flock.apply_poll(&bots, &[row(name, "idle")]);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].to, StatusKind::Done);
        assert!(matches!(flock.status(name), BotStatus::Paused(_)));
        // idle + needs-input report → NeedsInput (a transition up)
        set(&mut flock, Some(ReportStatus::NeedsInput));
        let t = flock.apply_poll(&bots, &[row(name, "idle")]);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].to, StatusKind::NeedsInput);
        // exited beats everything
        let t = flock.apply_poll(&bots, &[row(name, "stopped")]);
        assert_eq!(flock.status(name), BotStatus::Exited);
        assert_eq!(t[0].status, BotStatus::Exited);
        assert_eq!(t[0].to, StatusKind::Done);
        // an unparseable report is no report
        set(&mut flock, None);
        flock.apply_poll(&bots, &[row(name, "idle")]);
        assert!(matches!(flock.status(name), BotStatus::Done(_)));
        assert!(!flock.has_working());
    }

    #[test]
    fn first_sight_is_silent_then_transitions_fire() {
        let mut flock = Workers::default();
        let bots = [bot("swift")];
        let name = "aviary-swift_x-impl";
        assert!(flock.apply_poll(&bots, &[row(name, "idle")]).is_empty());
        let t = flock.apply_poll(&bots, &[row(name, "busy")]);
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].from, t[0].to), (StatusKind::Done, StatusKind::Working));
        assert_eq!(t[0].label, "x-impl");
        assert_eq!(t[0].bot, BotId("swift".into()));
        assert!(flock.has_working());
        assert!(flock.apply_poll(&bots, &[row(name, "busy")]).is_empty(), "no change, no transition");
        flock.mark_unread(name);
        assert!(flock.is_unread(name));
        flock.clear_unread(name);
        assert!(!flock.is_unread(name));
        flock.forget_bot(&BotId("swift".into()));
        assert!(flock.for_bot(&BotId("swift".into())).is_empty());
        assert_eq!(flock.status(name), BotStatus::NotStarted);
    }

    #[test]
    fn tick_reports_rereads_only_on_mtime_change() {
        let dir = std::env::temp_dir().join(format!("aviary-flock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut flock = Workers::default();
        let bots = [bot("swift")];
        let name = "aviary-swift_x-impl";
        flock.apply_poll(&bots, &[row(name, "idle")]);
        assert!(flock.tick_reports(&dir).is_empty(), "no file, nothing to say");

        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, "# x report\n\nstatus: needs-input\n").unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        let t = flock.tick_reports(&dir);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].to, StatusKind::NeedsInput);
        assert_eq!(flock.status(name), BotStatus::NeedsInput);

        // Same mtime: not re-read even though the bytes changed.
        std::fs::write(&path, "# x report\n\nstatus: done\n").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        assert!(flock.tick_reports(&dir).is_empty());
        assert_eq!(flock.status(name), BotStatus::NeedsInput);

        // A newer mtime is read: done, aged from the file's mtime.
        let newer = old + std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(newer).unwrap();
        let t = flock.tick_reports(&dir);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].to, StatusKind::Done);
        assert!(matches!(flock.status(name), BotStatus::Done(age) if age > 60));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
