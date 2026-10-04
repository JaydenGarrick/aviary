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
    /// Attribution is by cwd; the id is kept for debugging the events file.
    #[serde(default)]
    #[allow(dead_code)]
    pub session_id: String,
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

/// `aviary --hook`: read the hook payload from stdin, append it as one line.
/// Must be fast and silent — it runs inside every bird's hook.
pub fn append_from_stdin(events_path: &Path) -> std::io::Result<()> {
    let mut input = String::new();
    std::io::Read::take(std::io::stdin(), 64 * 1024).read_to_string(&mut input)?;
    let line = input.replace(['\n', '\r'], " ");
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
}
