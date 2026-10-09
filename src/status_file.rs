//! The in-session mod's files — how a Claude session and the cockpit talk
//! without a socket (fs-as-state, like everything else here):
//!
//!   status/<sessionId>.json   the mod (`plugin/hooks/register.ts`) writes it
//!                             on every state change and heartbeats it every
//!                             5 s; aviary reads changed files each tick and
//!                             joins them to a bird or worker by the session
//!                             id the `claude agents --json` poll returns.
//!   inbox/<sessionId>/*.md    one prompt per file for a RUNNING session.
//!                             The mod submits each with `$.prompt.submit`
//!                             (a turn of its own, once idle) and acks the
//!                             file name in its status; aviary deletes what
//!                             is acked. A dead mod's unacked files are typed
//!                             the old way (`pty::Terminal::send_line`).
//!
//! The reader is tolerant: unknown fields ignored, missing ones default, a
//! half-written file (the mod's write is not atomic) parses as nothing and
//! is read again next tick. A thin fs adapter (scan · inbox · gc) around
//! pure parse/decide logic (`ModStatus::parse`, `status`, `reconcile`);
//! unit-tested against a tempdir.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::status::BotStatus;

/// A status older than this (by its own `updated_at`) is a dead mod: the
/// heartbeat is every 5 s, so three missed beats.
pub const MOD_TRUST: Duration = Duration::from_secs(15);
/// Files for sessions nobody holds are swept once this old.
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);
/// A status file larger than this is not one of ours.
const FILE_CAP: u64 = 64 * 1024;
/// The schema this binary reads; the shipped mod writes the same number.
const SCHEMA: u32 = 1;

/// How long after the mod's word changed a poll must be before it may say
/// "the permission dialog was answered" — one poll period, so a poll that
/// started before the dialog opened never clears it.
pub const POLL_LAG: Duration = Duration::from_secs(3);

/// What the latest `claude agents --json` row said about a session, as
/// [`ModStatus::reconcile`] weighs it.
#[derive(Clone, Debug)]
pub struct PollView {
    /// The row read busy/working.
    pub busy: bool,
    /// When aviary read it.
    pub at: Instant,
    /// The row's `waitingFor`, if any — the engine's record of an open dialog.
    pub waiting_for: Option<String>,
}

/// Inbox file stamps for one poster: `<ms>-<seq>`, strictly increasing even
/// when the clock steps back or two posts share a millisecond — the mod
/// acks by NAME and the sweep deletes everything up to the ack.
#[derive(Default, Debug)]
pub struct PostStamp {
    last_ms: u64,
    seq: u32,
}

impl PostStamp {
    fn next(&mut self, now_ms: u64) -> (u64, u32) {
        if now_ms > self.last_ms {
            self.last_ms = now_ms;
            self.seq = 0;
        } else {
            self.seq += 1;
            if self.seq > 9_999 {
                self.last_ms += 1;
                self.seq = 0;
            }
        }
        (self.last_ms, self.seq)
    }
}

/// One `status/<sessionId>.json`, as the mod writes it.
#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
pub struct ModStatus {
    #[serde(default)]
    pub v: u32,
    #[serde(default)]
    pub session_id: String,
    /// working · needs-input · done · failed · compacting · ended
    #[serde(default)]
    pub state: String,
    /// permission · question · plan review · interrupted · error
    #[serde(default)]
    pub reason: String,
    /// `Bash · cargo test` — the tool and the head of its argument.
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub context_percent: Option<f32>,
    #[serde(default)]
    pub cost_usd: Option<f32>,
    /// The last inbox file the mod submitted.
    #[serde(default)]
    pub inbox_ack: String,
    /// Epoch millis of the write.
    #[serde(default)]
    pub updated_at: u64,
}

impl ModStatus {
    /// Parse one file; `None` for junk, a half write, or another schema —
    /// the mod is aviary-owned, so a mismatch means an older/newer binary.
    pub fn parse(text: &str) -> Option<ModStatus> {
        let s: ModStatus = serde_json::from_str(text).ok()?;
        (s.v == SCHEMA && !s.state.is_empty()).then_some(s)
    }

    /// Fresh enough to trust over the poll and the PTY heuristic.
    pub fn is_alive(&self, now_ms: u64) -> bool {
        self.state != "ended"
            && now_ms.saturating_sub(self.updated_at) < MOD_TRUST.as_millis() as u64
    }

    /// The chip the state wears; `done_age` is how long the kind has stood.
    /// `None` for a state this binary does not map (ended, or a newer one).
    pub fn status(&self, done_age: u64) -> Option<BotStatus> {
        Some(match self.state.as_str() {
            "working" | "compacting" => BotStatus::Working,
            "needs-input" => BotStatus::NeedsInput,
            "failed" => BotStatus::Failed,
            "done" => BotStatus::Done(done_age),
            _ => return None,
        })
    }

    /// Did the mod's WORD change (state · reason · detail)? A heartbeat
    /// rewrites `updated_at` only; `POLL_LAG` is measured from the word.
    pub fn same_word(&self, other: &ModStatus) -> bool {
        (&self.state, &self.reason, &self.detail) == (&other.state, &other.reason, &other.detail)
    }

    /// The mod's word, weighed against the latest poll — the TWO documented
    /// places the poll may speak over a live mod (CLAUDE.md, status
    /// precedence), for birds and workers alike:
    ///   · no engine event marks a permission dialog ANSWERED: a poll taken
    ///     ≥ [`POLL_LAG`] after the mod said `needs-input · permission` that
    ///     reads busy with nothing waiting means it was approved → working;
    ///   · the mod sees permission/question/plan dialogs only: a fresh poll
    ///     `waitingFor` (MCP elicitation, sandbox, …) on a session the mod
    ///     calls quiet or working → needs-input with that reason. It is set
    ///     only while waiting, so it cannot flap.
    /// `changed` is when the mod's state/reason/detail last changed.
    pub fn reconcile(&self, changed: Instant, poll: Option<&PollView>) -> ModStatus {
        let mut st = self.clone();
        let Some(poll) = poll else {
            return st;
        };
        let answered = st.state == "needs-input"
            && st.reason == "permission"
            && poll.busy
            && poll.waiting_for.is_none()
            && poll.at.checked_duration_since(changed).is_some_and(|d| d >= POLL_LAG);
        if answered {
            st.state = "working".into();
            st.reason.clear();
            st.detail.clear();
        } else if st.state != "needs-input" && st.state != "failed" && poll.at.elapsed() < MOD_TRUST {
            if let Some(why) = &poll.waiting_for {
                st.state = "needs-input".into();
                st.reason = why.clone();
                st.detail.clear();
            }
        }
        st
    }

    /// `permission · Bash · cargo test` — reason and detail joined; `None`
    /// when the mod gave neither.
    pub fn reason_text(&self) -> Option<String> {
        let parts: Vec<&str> = [self.reason.as_str(), self.detail.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mtime-tracked reader of the status directory: each scan returns only the
/// files that changed. Bounded local fs work — one `read_dir` plus a read
/// per changed file — so it stays on the UI thread.
#[derive(Default)]
pub struct StatusDir {
    seen: HashMap<String, (SystemTime, u64)>,
}

impl StatusDir {
    pub fn scan(&mut self, dir: &Path) -> Vec<(String, ModStatus)> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut present = HashSet::new();
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .map(str::to_string)
            else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() || meta.len() > FILE_CAP {
                continue;
            }
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            present.insert(id.clone());
            let stamp = (mtime, meta.len());
            if self.seen.get(&id) == Some(&stamp) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // Remember the stamp whatever the text parsed as: a half write
            // lands with a new (mtime, len) and is read again then, while a
            // file that never parses (junk, another schema) is read once,
            // not every tick.
            self.seen.insert(id.clone(), stamp);
            let Some(status) = ModStatus::parse(&text) else {
                continue;
            };
            out.push((id, status));
        }
        self.seen.retain(|id, _| present.contains(id));
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

// ------------------------------------------------------------------- inbox

/// Post one prompt for a running session. Names sort in post order:
/// `<epoch ms, 13 digits>-<seq, 4 digits>.md`. Written whole then renamed,
/// so the mod never reads half a prompt.
pub fn inbox_post(
    inbox_dir: &Path,
    session_id: &str,
    stamp: &mut PostStamp,
    text: &str,
) -> std::io::Result<String> {
    let folder = inbox_dir.join(session_id);
    std::fs::create_dir_all(&folder)?;
    let (ms, seq) = stamp.next(now_ms());
    let name = format!("{ms:013}-{seq:04}.md");
    let tmp = folder.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, folder.join(&name))?;
    Ok(name)
}

/// The files the mod has not acked (names after `ack`), oldest first.
pub fn inbox_pending(inbox_dir: &Path, session_id: &str, ack: &str) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(inbox_dir.join(session_id)) else {
        return Vec::new();
    };
    let mut v: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            (name.ends_with(".md") && !name.starts_with('.') && name.as_str() > ack)
                .then(|| (name, e.path()))
        })
        .collect();
    v.sort();
    v
}

/// A session id changed under the same key (a /clear, a relaunch): move the
/// old id's unsent prompts into the new id's queue, names kept (still in
/// post order), and drop the old folder. Returns how many moved.
pub fn inbox_move(inbox_dir: &Path, from: &str, to: &str) -> usize {
    let pending = inbox_pending(inbox_dir, from, "");
    if pending.is_empty() {
        let _ = std::fs::remove_dir_all(inbox_dir.join(from));
        return 0;
    }
    let dest = inbox_dir.join(to);
    if std::fs::create_dir_all(&dest).is_err() {
        return 0;
    }
    let moved = pending
        .into_iter()
        .filter(|(name, path)| std::fs::rename(path, dest.join(name)).is_ok())
        .count();
    let _ = std::fs::remove_dir_all(inbox_dir.join(from));
    moved
}

/// Delete what the mod acked (names up to and including `ack`).
pub fn inbox_sweep(inbox_dir: &Path, session_id: &str, ack: &str) -> usize {
    if ack.is_empty() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(inbox_dir.join(session_id)) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.ends_with(".md") && name.as_str() <= ack && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// Sweep what nobody holds: status files and inbox folders whose session id
/// is not `live` and whose mtime is older than [`STALE_AFTER`]. Returns how
/// many entries went.
pub fn gc(status_dir: &Path, inbox_dir: &Path, live: &HashSet<String>) -> usize {
    let now = SystemTime::now();
    let stale = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_AFTER)
    };
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(status_dir) {
        for e in entries.flatten() {
            let path = e.path();
            let id = path
                .file_name()
                .and_then(|f| f.to_str())
                .and_then(|f| f.strip_suffix(".json"))
                .unwrap_or_default();
            if !id.is_empty() && !live.contains(id) && stale(&path) && std::fs::remove_file(&path).is_ok() {
                n += 1;
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(inbox_dir) {
        for e in entries.flatten() {
            let path = e.path();
            let id = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
            if !id.is_empty()
                && !live.contains(id)
                && path.is_dir()
                && stale(&path)
                && std::fs::remove_dir_all(&path).is_ok()
            {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &str, updated_at: u64) -> String {
        format!(r#"{{"v":1,"session_id":"s","state":"{state}","updated_at":{updated_at}}}"#)
    }

    #[test]
    fn parses_v1_and_ignores_unknown_fields() {
        let s = ModStatus::parse(
            r#"{"v":1,"session_id":"abc","state":"needs-input","reason":"permission","detail":"Bash · cargo test","context_percent":72.4,"cost_usd":1.4,"inbox_ack":"x","updated_at":5,"future":true}"#,
        )
        .unwrap();
        assert_eq!(s.session_id, "abc");
        assert_eq!(s.reason_text().as_deref(), Some("permission · Bash · cargo test"));
        assert_eq!(s.context_percent, Some(72.4));
        assert_eq!(s.cost_usd, Some(1.4));
        assert_eq!(s.inbox_ack, "x");
        // Another schema or no state: not ours.
        assert!(ModStatus::parse(r#"{"v":2,"state":"working"}"#).is_none());
        assert!(ModStatus::parse(r#"{"v":1}"#).is_none());
        assert!(ModStatus::parse("{\"v\":1,\"state\":\"wor").is_none(), "a half write");
        assert!(ModStatus::parse("junk").is_none());
        // Figures absent, never zeroed; no reason text when the mod gave none.
        let bare = ModStatus::parse(&status("working", 1)).unwrap();
        assert_eq!(bare.context_percent, None);
        assert_eq!(bare.reason_text(), None);
    }

    #[test]
    fn alive_is_a_fresh_heartbeat_and_not_ended() {
        let now = 1_000_000;
        assert!(ModStatus::parse(&status("done", now - 14_000)).unwrap().is_alive(now));
        assert!(!ModStatus::parse(&status("done", now - 15_000)).unwrap().is_alive(now));
        assert!(!ModStatus::parse(&status("ended", now)).unwrap().is_alive(now));
        // A clock slightly ahead of ours is still fresh.
        assert!(ModStatus::parse(&status("working", now + 500)).unwrap().is_alive(now));
    }

    #[test]
    fn states_map_to_chips() {
        let chip = |s: &str| ModStatus::parse(&status(s, 1)).unwrap().status(42);
        assert_eq!(chip("working"), Some(BotStatus::Working));
        assert_eq!(chip("compacting"), Some(BotStatus::Working));
        assert_eq!(chip("needs-input"), Some(BotStatus::NeedsInput));
        assert_eq!(chip("failed"), Some(BotStatus::Failed));
        assert_eq!(chip("done"), Some(BotStatus::Done(42)));
        assert_eq!(chip("ended"), None, "exit is the poll's and the PTY's to report");
        assert_eq!(chip("dreaming"), None, "a newer state stays quiet");
    }

    #[test]
    fn scan_reads_changed_files_once_and_skips_half_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut reader = StatusDir::default();
        assert!(reader.scan(dir).is_empty(), "no dir yet is fine");
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("a.json"), status("working", 1)).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a status").unwrap();
        let first = reader.scan(dir);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "a");
        assert_eq!(first[0].1.state, "working");
        assert!(reader.scan(dir).is_empty(), "unchanged files are not re-read");
        // A half write parses as nothing (and is not re-read until it changes) …
        std::fs::write(dir.join("a.json"), "{\"v\":1,\"state\":\"nee").unwrap();
        assert!(reader.scan(dir).is_empty());
        assert!(reader.scan(dir).is_empty(), "junk is read once, not every tick");
        // … until the write lands.
        std::fs::write(dir.join("a.json"), status("needs-input", 2)).unwrap();
        let again = reader.scan(dir);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].1.state, "needs-input");
        // A file that vanishes and comes back is news again.
        std::fs::remove_file(dir.join("a.json")).unwrap();
        assert!(reader.scan(dir).is_empty());
        std::fs::write(dir.join("a.json"), status("needs-input", 2)).unwrap();
        assert_eq!(reader.scan(dir).len(), 1);
    }

    #[test]
    fn inbox_posts_in_order_then_sweeps_only_what_is_acked() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let mut stamp = PostStamp::default();
        let first = inbox_post(&inbox, "sid", &mut stamp, "hello").unwrap();
        let second = inbox_post(&inbox, "sid", &mut stamp, "world").unwrap();
        assert!(first < second, "names sort in post order even within one ms");
        assert!(first.ends_with("-0000.md"), "a fresh stamp starts its millisecond at 0");
        let pending = inbox_pending(&inbox, "sid", "");
        assert_eq!(pending.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), [&first, &second]);
        assert_eq!(std::fs::read_to_string(&pending[0].1).unwrap(), "hello");
        // No temp files leak into the listing.
        assert!(!pending.iter().any(|(n, _)| n.starts_with('.')));
        assert_eq!(inbox_sweep(&inbox, "sid", ""), 0, "no ack, nothing swept");
        assert_eq!(inbox_sweep(&inbox, "sid", &first), 1);
        let left = inbox_pending(&inbox, "sid", "");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, second);
        assert!(inbox_pending(&inbox, "sid", &second).is_empty(), "acked files are not pending");
        assert!(inbox_pending(&inbox, "nobody", "").is_empty());
        // A /clear gives the key a new id: the unsent prompt follows it.
        assert_eq!(inbox_move(&inbox, "sid", "sid-new"), 1);
        assert!(!inbox.join("sid").exists());
        assert_eq!(inbox_pending(&inbox, "sid-new", "")[0].0, second);
        assert_eq!(inbox_move(&inbox, "nobody", "x"), 0);
    }

    #[test]
    fn gc_sweeps_only_stale_files_of_unknown_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let status_dir = tmp.path().join("status");
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&status_dir).unwrap();
        for id in ["live-old", "dead-old", "dead-fresh"] {
            std::fs::write(status_dir.join(format!("{id}.json")), status("done", 1)).unwrap();
            inbox_post(&inbox, id, &mut PostStamp::default(), "x").unwrap();
        }
        let old = SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
        for id in ["live-old", "dead-old"] {
            std::fs::File::options()
                .write(true)
                .open(status_dir.join(format!("{id}.json")))
                .unwrap()
                .set_modified(old)
                .unwrap();
            std::fs::File::open(inbox.join(id)).unwrap().set_modified(old).unwrap();
        }
        let live: HashSet<String> = ["live-old".to_string()].into_iter().collect();
        assert_eq!(gc(&status_dir, &inbox, &live), 2, "one status file + one inbox folder");
        assert!(status_dir.join("live-old.json").is_file(), "held sessions are kept however old");
        assert!(status_dir.join("dead-fresh.json").is_file(), "fresh files wait out the grace");
        assert!(!status_dir.join("dead-old.json").exists());
        assert!(!inbox.join("dead-old").exists());
        assert!(inbox.join("live-old").is_dir());
    }

    #[test]
    fn post_stamps_only_ever_increase() {
        let mut stamp = PostStamp::default();
        assert_eq!(stamp.next(1000), (1000, 0));
        assert_eq!(stamp.next(1000), (1000, 1), "same ms: the seq breaks the tie");
        assert_eq!(stamp.next(900), (1000, 2), "a clock stepping back never goes below");
        assert_eq!(stamp.next(2000), (2000, 0));
        stamp.seq = 9_999;
        assert_eq!(stamp.next(2000), (2001, 0), "seq overflow carries into the ms");
    }

    #[test]
    fn reconcile_lets_the_poll_speak_only_where_documented() {
        let st = |state: &str, reason: &str| ModStatus {
            v: 1,
            state: state.into(),
            reason: reason.into(),
            detail: "Bash · cargo test".into(),
            ..Default::default()
        };
        let changed = Instant::now();
        let poll = |busy: bool, after: Duration, waiting: Option<&str>| PollView {
            busy,
            at: changed + after,
            waiting_for: waiting.map(str::to_string),
        };
        let perm = st("needs-input", "permission");
        assert_eq!(perm.reconcile(changed, None), perm, "no poll, no say");
        // A busy poll too soon after the dialog opened proves nothing …
        assert_eq!(perm.reconcile(changed, Some(&poll(true, Duration::ZERO, None))).state, "needs-input");
        // … a full period later, it was approved.
        let ok = perm.reconcile(changed, Some(&poll(true, POLL_LAG, None)));
        assert_eq!((ok.state.as_str(), ok.reason.as_str()), ("working", ""));
        // Still waiting, or idle: the dialog stands.
        assert_eq!(perm.reconcile(changed, Some(&poll(true, POLL_LAG, Some("permission prompt")))).state, "needs-input");
        assert_eq!(perm.reconcile(changed, Some(&poll(false, POLL_LAG, None))).state, "needs-input");
        // A question is never cleared by the poll.
        let q = st("needs-input", "question");
        assert_eq!(q.reconcile(changed, Some(&poll(true, POLL_LAG, None))).reason, "question");
        // A dialog the mod cannot see surfaces from a fresh waitingFor.
        let seen = st("done", "").reconcile(changed, Some(&poll(false, Duration::ZERO, Some("dialog open"))));
        assert_eq!((seen.state.as_str(), seen.reason.as_str()), ("needs-input", "dialog open"));
        assert_eq!(st("failed", "error").reconcile(changed, Some(&poll(false, Duration::ZERO, Some("x")))).state, "failed");
    }
}
