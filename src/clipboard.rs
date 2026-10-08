//! The one clipboard path. Pipes into `pbcopy` — no dependency, the same
//! trade the room timestamps make with `date`.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// Put `text` on the macOS clipboard. False when pbcopy is missing or fails.
pub fn copy(text: &str) -> bool {
    let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}
