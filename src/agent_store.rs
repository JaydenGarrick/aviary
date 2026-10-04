//! The birds themselves: one Claude Code session per bot, keyed by [`BotId`].
//!
//! Identity is the session NAME (`aviary-<id>`, via `--name`), which makes
//! resume trivial (`--resume aviary-<id>`) and makes every bird a stable
//! SendMessage target for its teammates. No jsonl bookkeeping — Claude Code
//! owns session storage; aviary only remembers whether a bot has ever spawned.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::config::{Bot, BotId, Config, State};
use crate::event::Event;
use crate::pty;

pub struct AgentSession {
    pub term: pty::Terminal,
    /// The last prompt aviary itself typed — the roster's activity line.
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BotStatus {
    NotStarted,
    /// PTY output within the last few seconds.
    Working,
    /// Running, quiet for `secs`.
    Idle(u64),
    Exited,
}

/// How recent output must be to count as "working" on the roster.
const WORKING_WINDOW: Duration = Duration::from_secs(5);
/// A resumed session dying this fast means "no such session" — fall back fresh.
const RESUME_FAIL_WINDOW: Duration = Duration::from_secs(4);
/// A fresh session must live this long before it counts as resumable.
const MARK_AFTER: Duration = Duration::from_secs(10);
/// A collaboration tag older than this is stale and stops showing.
const COLLAB_TTL: Duration = Duration::from_secs(10 * 60);

/// Who a bird is working WITH right now, as far as aviary brokered it.
/// (Bird-initiated SendMessages inside the sessions are invisible here —
/// no tag means "independent, as far as we know".)
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
    sessions: HashMap<BotId, AgentSession>,
    last_output: HashMap<BotId, Instant>,
    collab: HashMap<BotId, (Collab, Instant)>,
    state: State,
}

impl AgentStore {
    pub fn new(cfg: &Config) -> AgentStore {
        AgentStore {
            sessions: HashMap::new(),
            last_output: HashMap::new(),
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

    pub fn get(&self, id: &BotId) -> Option<&AgentSession> {
        self.sessions.get(id)
    }

    pub fn get_mut(&mut self, id: &BotId) -> Option<&mut AgentSession> {
        self.sessions.get_mut(id)
    }

    pub fn running_count(&self) -> usize {
        self.sessions.values().filter(|s| s.term.is_running()).count()
    }

    /// Has this bot EVER had a session (drives resume vs the first-flight prompt)?
    pub fn has_session_record(&self, id: &BotId) -> bool {
        self.state.spawned_once(id)
    }

    /// Make sure the bot has a LIVE session, spawning or resuming as needed.
    /// `prompt`: typed in after launch (or into the running session).
    pub fn ensure_running(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        if let Some(session) = self.sessions.get_mut(&bot.id) {
            if session.term.is_running() {
                if let Some(p) = prompt {
                    session.term.send(p.as_bytes());
                    session.term.send(b"\r");
                    session.last_prompt = Some(p.to_string());
                }
                return Ok(());
            }
            self.sessions.remove(&bot.id); // exited — replace it
        }
        self.launch(cfg, bot, prompt, tx, self.state.spawned_once(&bot.id))
    }

    fn launch(
        &mut self,
        cfg: &Config,
        bot: &Bot,
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

        let args = launch_args(
            bot.id.session_name(),
            &persona,
            &cfg.dir,
            &cfg.mcp_config_path(),
            prompt,
            resume,
        );

        let term = pty::Terminal::spawn(bot.id.clone(), "claude", &args, &repo, 24, 80, tx.clone())?;
        self.sessions.insert(
            bot.id.clone(),
            AgentSession {
                term,
                last_prompt: prompt.map(str::to_string),
                spawned_at: Instant::now(),
                resumed: resume,
                fresh_unmarked: !resume,
                exit_handled: false,
            },
        );
        Ok(())
    }

    /// Deliberately abandon the current conversation: stop the session, forget
    /// the resume record, and hatch a brand-new one.
    pub fn fresh_start(
        &mut self,
        cfg: &Config,
        bot: &Bot,
        prompt: Option<&str>,
        tx: &Sender<Event>,
    ) -> Result<()> {
        self.sessions.remove(&bot.id);
        self.state.forget(&cfg.dir, &bot.id);
        self.launch(cfg, bot, prompt, tx, false)
    }

    /// Once a second: a fresh session that has survived [`MARK_AFTER`] becomes
    /// the bot's resumable session of record.
    pub fn tick(&mut self, cfg: &Config) {
        let ripe: Vec<BotId> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.fresh_unmarked && s.term.is_running() && s.spawned_at.elapsed() >= MARK_AFTER
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ripe {
            self.state.mark_spawned(&cfg.dir, &id);
            if let Some(s) = self.sessions.get_mut(&id) {
                s.fresh_unmarked = false;
            }
        }
    }

    /// Note PTY activity (from `Event::AgentOutput`). Returns a relaunch
    /// request when a `--resume` died instantly: the named session no longer
    /// exists, so the caller should `ensure_running` again — the state file
    /// has already been reset to force a fresh spawn.
    pub fn note_output(&mut self, cfg: &Config, id: &BotId) -> RelaunchHint {
        self.last_output.insert(id.clone(), Instant::now());
        let Some(session) = self.sessions.get_mut(id) else {
            return RelaunchHint::No;
        };
        if session.term.is_running() || session.exit_handled {
            return RelaunchHint::No;
        }
        session.exit_handled = true;
        if session.resumed && session.spawned_at.elapsed() < RESUME_FAIL_WINDOW {
            self.state.forget(&cfg.dir, id);
            self.sessions.remove(id);
            return RelaunchHint::FreshSpawn;
        }
        RelaunchHint::No
    }

    pub fn status(&self, id: &BotId) -> BotStatus {
        match self.sessions.get(id) {
            None => BotStatus::NotStarted,
            Some(s) if !s.term.is_running() => BotStatus::Exited,
            Some(_) => match self.last_output.get(id) {
                Some(t) if t.elapsed() < WORKING_WINDOW => BotStatus::Working,
                Some(t) => BotStatus::Idle(t.elapsed().as_secs()),
                None => BotStatus::Working, // just spawned, first paint pending
            },
        }
    }

    /// Dropping the master closes the PTY, which hangs up the child.
    pub fn stop(&mut self, id: &BotId) {
        self.sessions.remove(id);
        self.collab.remove(id);
    }
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
/// therefore followed by another flag, and the prompt only ever follows the
/// single-value `--append-system-prompt-file`.
fn launch_args(
    session: String,
    persona: &std::path::Path,
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
    use super::launch_args;
    use std::path::Path;

    /// The regression this guards: a variadic flag directly before the
    /// positional prompt eats it ("MCP config file not found: …<prompt>").
    #[test]
    fn variadic_flags_never_precede_the_prompt() {
        let args = launch_args(
            "aviary-swift".into(),
            Path::new("/cfg/birds/swift.md"),
            Path::new("/cfg"),
            Path::new("/cfg/mcp.json"),
            Some("You've just been perched."),
            false,
        );
        assert_eq!(args.last().map(String::as_str), Some("You've just been perched."));
        // Every variadic flag's values are terminated by another flag.
        for variadic in ["--mcp-config", "--add-dir"] {
            let i = args.iter().position(|a| a == variadic).unwrap();
            assert!(
                args[i + 2].starts_with("--"),
                "{variadic} must be followed by exactly one value then a flag"
            );
        }
        // The prompt sits right after the single-value persona flag's value.
        let i = args.iter().position(|a| a == "--append-system-prompt-file").unwrap();
        assert_eq!(args[i + 1], "/cfg/birds/swift.md");
        assert_eq!(i + 2, args.len() - 1);
    }

    #[test]
    fn resume_swaps_name_for_resume_and_tolerates_no_prompt() {
        let args = launch_args(
            "aviary-raven".into(),
            Path::new("/cfg/birds/raven.md"),
            Path::new("/cfg"),
            Path::new("/cfg/mcp.json"),
            None,
            true,
        );
        assert_eq!(args[0], "--resume");
        assert_eq!(args[1], "aviary-raven");
        // No prompt: the single-value persona flag's value ends the argv.
        assert_eq!(args.last().map(String::as_str), Some("/cfg/birds/raven.md"));
    }
}
