//! Rooms: group chats as append-only markdown transcripts.
//!
//! The transcript file IS the room — bots append to it, aviary renders it and
//! notifies members. Same crash-proof fs-as-state model as everything else:
//! no IPC, survives restarts of either side, and a human can read the room in
//! any editor.
//!
//! Dispatch is mention-driven to bound bot chatter: a USER message with no
//! mentions reaches every member, but a BOT's append only notifies the birds
//! it @-mentions — a bot must name a teammate to keep a chain going.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{BotId, Config, Room};

/// The author name aviary writes for the human.
pub const USER_AUTHOR: &str = "jayden";

pub struct Entry {
    pub author: String,
    pub when: String,
    pub body: String,
}

/// Parse `### @author — time` blocks. Anything before the first header (the
/// title line) is skipped; a malformed header starts no entry and falls into
/// the previous body, which keeps odd bot output visible instead of lost.
pub fn parse(text: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("### @") {
            let (author, when) = match rest.split_once('—') {
                Some((a, w)) => (a.trim().to_string(), w.trim().to_string()),
                None => (rest.trim().to_string(), String::new()),
            };
            entries.push(Entry {
                author,
                when,
                body: String::new(),
            });
        } else if let Some(cur) = entries.last_mut() {
            if !cur.body.is_empty() {
                cur.body.push('\n');
            }
            cur.body.push_str(line);
        }
    }
    for e in &mut entries {
        e.body = e.body.trim().to_string();
    }
    entries
}

pub fn read(path: &Path) -> Vec<Entry> {
    std::fs::read_to_string(path)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// Append one block in the same shape the personas teach the birds.
pub fn append(path: &Path, author: &str, text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    writeln!(f, "\n### @{author} — {}\n{}", local_now(), text.trim())?;
    Ok(())
}

/// `@word` tokens (letters, digits, dashes) anywhere in a message.
pub fn mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-' || bytes[end] == b'_')
            {
                end += 1;
            }
            if end > start {
                let name = text[start..end].to_lowercase();
                // Both spellings work: `@raven` and `@aviary-raven`.
                let name = name.strip_prefix("aviary-").unwrap_or(&name).to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Who a new message should wake. OUTSIDE authors (the user, a webhook) with
/// no mentions reach the whole room; a member bot must @-mention to pass the
/// floor — that asymmetry is what bounds bot chatter. The author never
/// notifies itself.
pub fn dispatch_targets(room: &Room, author: &str, text: &str) -> Vec<BotId> {
    let named = mentions(text);
    let author_is_outsider = !room.members.iter().any(|m| m.0 == author);
    room.members
        .iter()
        .filter(|m| m.0 != author)
        .filter(|m| {
            if named.is_empty() {
                author_is_outsider
            } else {
                named.contains(&m.0)
            }
        })
        .cloned()
        .collect()
}

/// Tracks per-room entry counts so only NEW appends dispatch. Initialized to
/// the current count on first sight — history is never replayed.
#[derive(Default)]
pub struct Watcher {
    seen: HashMap<String, usize>,
}

impl Watcher {
    /// Poll every room; return (target bot, room id, notify prompt) for fresh
    /// appends. Appends made through aviary's own composer are pre-counted via
    /// [`Watcher::note_local_append`], so only bot/external writes dispatch.
    pub fn poll(&mut self, cfg: &Config) -> Vec<(BotId, String, String)> {
        let mut out = Vec::new();
        for room in &cfg.rooms {
            let entries = read(&room.transcript_path(&cfg.dir));
            let seen = match self.seen.get(&room.id) {
                Some(&n) => n,
                None => {
                    // First sight (startup): adopt without replaying.
                    self.seen.insert(room.id.clone(), entries.len());
                    continue;
                }
            };
            for e in entries.iter().skip(seen) {
                for target in dispatch_targets(room, &e.author, &e.body) {
                    out.push((
                        target,
                        room.id.clone(),
                        notify_prompt(room, &e.author, &cfg.dir),
                    ));
                }
            }
            self.seen.insert(room.id.clone(), entries.len());
        }
        out
    }

    /// The composer already dispatched this append — count it as seen.
    pub fn note_local_append(&mut self, room_id: &str) {
        *self.seen.entry(room_id.to_string()).or_insert(0) += 1;
    }
}

/// What gets typed into a member bot's session when the room has news.
pub fn notify_prompt(room: &Room, author: &str, dir: &Path) -> String {
    format!(
        "[#{room}] New message from @{author} in the room transcript {path}. Read it. \
         If you are addressed or have something material to add, APPEND your reply to that \
         file as a `### @<your-id> — <YYYY-MM-DD HH:MM>` block (never edit earlier content), \
         @-mentioning whoever should act next. If nothing is needed from you, do nothing.",
        room = room.id,
        author = author,
        path = room.transcript_path(dir).display(),
    )
}

/// Local wall-clock without a chrono dependency: the `date` binary, with a
/// unix-seconds fallback — the same trade mb's gate logger makes.
pub fn local_now() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            format!(
                "unix:{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn room_with(members: &[&str]) -> Room {
        Room {
            id: "test".into(),
            name: "test".into(),
            members: members.iter().map(|m| BotId(m.to_string())).collect(),
        }
    }

    #[test]
    fn parses_blocks_and_skips_title() {
        let text = "# #room — @a · @b\n\n### @swift — 2026-10-04 10:00\nhello @raven\nsecond line\n\n### @raven — 2026-10-04 10:01\ncaw";
        let entries = parse(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].author, "swift");
        assert_eq!(entries[0].body, "hello @raven\nsecond line");
        assert_eq!(entries[1].when, "2026-10-04 10:01");
    }

    #[test]
    fn mentions_normalize_prefix_and_dedupe() {
        let m = mentions("ping @Raven and @aviary-raven, also @swift!");
        assert_eq!(m, vec!["raven".to_string(), "swift".to_string()]);
        assert!(mentions("no birds here, email a@b.c ok").contains(&"b".to_string()));
    }

    #[test]
    fn user_without_mentions_reaches_everyone() {
        let room = room_with(&["swift", "raven", "weaver"]);
        let t = dispatch_targets(&room, USER_AUTHOR, "morning birds");
        assert_eq!(t.len(), 3);
        // Any outside author (a webhook) gets the same reach.
        let t = dispatch_targets(&room, "webhook", "CI is red");
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn bot_without_mentions_reaches_nobody() {
        let room = room_with(&["swift", "raven"]);
        assert!(dispatch_targets(&room, "swift", "thinking out loud").is_empty());
    }

    #[test]
    fn mentions_target_members_only_and_never_the_author() {
        let room = room_with(&["swift", "raven"]);
        let t = dispatch_targets(&room, "swift", "@swift @raven @outsider look");
        assert_eq!(t, vec![BotId("raven".into())]);
    }

    #[test]
    fn watcher_adopts_history_then_dispatches_only_new() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::load_or_scaffold(tmp.path().to_path_buf()).unwrap();
        cfg.add_room("nest", vec![BotId("swift".into()), BotId("raven".into())])
            .unwrap();
        let path = cfg.rooms[0].transcript_path(&cfg.dir);
        append(&path, "swift", "pre-existing @raven history").unwrap();

        let mut w = Watcher::default();
        assert!(w.poll(&cfg).is_empty(), "history must not replay");

        append(&path, "swift", "fresh ask for @raven").unwrap();
        let dispatches = w.poll(&cfg);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].0, BotId("raven".into()));
        assert_eq!(dispatches[0].1, "nest");
        assert!(dispatches[0].2.contains("@swift"));
        assert!(w.poll(&cfg).is_empty(), "no double dispatch");
    }

    #[test]
    fn local_appends_do_not_redispatch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::load_or_scaffold(tmp.path().to_path_buf()).unwrap();
        cfg.add_room("nest", vec![BotId("swift".into()), BotId("raven".into())])
            .unwrap();
        let path = cfg.rooms[0].transcript_path(&cfg.dir);

        let mut w = Watcher::default();
        w.poll(&cfg); // adopt empty
        append(&path, USER_AUTHOR, "hello room").unwrap();
        w.note_local_append("nest");
        assert!(w.poll(&cfg).is_empty(), "composer already dispatched this");
    }
}
