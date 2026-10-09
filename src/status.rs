//! The status vocabulary every layer speaks — the bird store, the flock, the
//! mod's status files, the UI. Its own module so those layers depend on it,
//! not on each other. Pure.

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
    /// Worn only by workers: the report said `failed`. Ranks with NeedsInput.
    Failed,
    /// Worn only by workers: idle on an `in-progress` report for `secs`.
    /// Ranks with Done.
    Paused(u64),
}

impl BotStatus {
    /// The coarse kind transitions are measured in: attention, activity, quiet.
    pub fn kind(self) -> StatusKind {
        match self {
            BotStatus::Working => StatusKind::Working,
            BotStatus::NeedsInput | BotStatus::Failed => StatusKind::NeedsInput,
            BotStatus::Done(_) | BotStatus::Paused(_) | BotStatus::Exited | BotStatus::NotStarted => {
                StatusKind::Done
            }
        }
    }
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

/// What a chip cannot say: WHY a bird is blocked, and how full/expensive the
/// session is. From the mod's status file when it is alive, else the poll's
/// `waitingFor`. Figures are absent, never zero, when nobody reports them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Detail {
    /// `permission · Bash`, `question`, `plan review`, `permission prompt` …
    pub reason: Option<String>,
    pub context_percent: Option<f32>,
    pub cost_usd: Option<f32>,
}

