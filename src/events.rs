//! The hook-event pipeline: birds' sessions run `aviary --hook` on Stop and
//! Notification (wired per-session via `--settings`, verified working), which
//! appends one JSON line to `~/.config/aviary/events.jsonl`. The shell reads
//! new lines each tick — push-latency signals with the same crash-proof
//! fs-as-state model as the room watcher.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;

/// One hook payload line (tolerant: extra fields ignored, missing default).
#[derive(Deserialize, Default)]
pub struct HookEvent {
    /// Claude's own uuid — the CONVERSATION. Resolved first: a late hook
    /// from a conversation its slot no longer runs is dropped.
    #[serde(default)]
    pub session_id: String,
    /// Our session name, injected by `aviary --hook --session <name>` — the
    /// only attribution that can tell a bird's tabs apart (they share a cwd).
    /// Absent on old/foreign payloads, which fall back to cwd → primary.
    #[serde(default)]
    pub aviary_session: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub hook_event_name: String,
    /// Notification payloads vary; capture the likely discriminators.
    #[serde(default)]
    pub notification_type: String,
    #[serde(default)]
    pub message: String,
}

impl HookEvent {
    /// The string `map_hook_event` matches NeedsInput/idle against.
    pub fn detail(&self) -> String {
        format!("{} {}", self.notification_type, self.message).to_lowercase()
    }
}

/// Offset-tracked reader. If the file shrank (manual cleanup), start over.
#[derive(Default)]
pub struct EventsReader {
    offset: u64,
}

impl EventsReader {
    pub fn poll(&mut self, path: &Path) -> Vec<HookEvent> {
        let Ok(mut f) = std::fs::File::open(path) else {
            return Vec::new();
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            self.offset = 0; // truncated/rotated — re-read from the top
        }
        if len == self.offset {
            return Vec::new();
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return Vec::new();
        }
        // A hook may be mid-append: only consume complete lines.
        let consumed = match buf.rfind('\n') {
            Some(i) => i + 1,
            None => return Vec::new(),
        };
        self.offset += consumed as u64;
        buf[..consumed]
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<HookEvent>(l).ok())
            .collect()
    }
}

/// `aviary --hook [--session <name>]`: read the hook payload from stdin, tag
/// it with our session name, append it as one line. Must be fast and silent —
/// it runs inside every bird's hook.
pub fn append_from_stdin(events_path: &Path, session: Option<&str>) -> std::io::Result<()> {
    let mut input = String::new();
    std::io::Read::take(std::io::stdin(), 64 * 1024).read_to_string(&mut input)?;
    let line = tag_line(&input.replace(['\n', '\r'], " "), session);
    if line.trim().is_empty() {
        return Ok(());
    }
    if let Some(parent) = events_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)?;
    writeln!(f, "{}", line.trim())?;
    Ok(())
}

/// Inject `"aviary_session"` into a JSON payload line. Anything that is not a
/// JSON object passes through untouched — junk stays junk, never lost.
fn tag_line(line: &str, session: Option<&str>) -> String {
    let Some(session) = session else {
        return line.to_string();
    };
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(mut v) if v.is_object() => {
            v["aviary_session"] = serde_json::Value::String(session.to_string());
            v.to_string()
        }
        _ => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_only_new_complete_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mut reader = EventsReader::default();

        std::fs::write(
            &path,
            "{\"session_id\":\"a\",\"cwd\":\"/x\",\"hook_event_name\":\"Stop\"}\n",
        )
        .unwrap();
        let first = reader.poll(&path);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].hook_event_name, "Stop");
        assert!(reader.poll(&path).is_empty(), "no re-reads");

        // A partial line (no newline yet) must not be consumed…
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{{\"hook_event_name\":\"Notifi").unwrap();
        assert!(reader.poll(&path).is_empty());
        // …until it completes.
        writeln!(f, "cation\",\"cwd\":\"/y\",\"notification_type\":\"permission_prompt\"}}").unwrap();
        let second = reader.poll(&path);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].cwd, "/y");
        assert!(second[0].detail().contains("permission_prompt"));
    }

    #[test]
    fn truncation_resets_the_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mut reader = EventsReader::default();
        std::fs::write(&path, "{\"hook_event_name\":\"Stop\"}\n").unwrap();
        assert_eq!(reader.poll(&path).len(), 1);
        std::fs::write(&path, "{\"hook_event_name\":\"Stop\"}\n").unwrap(); // shorter-or-equal rewrite
        reader.offset = 999; // simulate stale offset beyond new length
        assert_eq!(reader.poll(&path).len(), 1, "shrunk file re-reads from top");
    }

    #[test]
    fn junk_lines_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        std::fs::write(&path, "not json\n{\"hook_event_name\":\"Stop\"}\n").unwrap();
        let mut reader = EventsReader::default();
        assert_eq!(reader.poll(&path).len(), 1);
    }

    #[test]
    fn tag_line_injects_the_session_and_spares_junk() {
        let tagged = tag_line("{\"hook_event_name\":\"Stop\",\"cwd\":\"/x\"}", Some("aviary-swift.2"));
        let ev: HookEvent = serde_json::from_str(&tagged).unwrap();
        assert_eq!(ev.aviary_session.as_deref(), Some("aviary-swift.2"));
        assert_eq!(ev.hook_event_name, "Stop");
        // No session, non-object JSON, and junk all pass through verbatim.
        assert_eq!(tag_line("{\"a\":1}", None), "{\"a\":1}");
        assert_eq!(tag_line("[1,2]", Some("aviary-x")), "[1,2]");
        assert_eq!(tag_line("not json", Some("aviary-x")), "not json");
    }
}
