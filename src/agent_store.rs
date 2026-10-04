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

pub struct AgentStore {
    sessions: HashMap<BotId, AgentSession>,
    last_output: HashMap<BotId, Instant>,
    state: State,
}

impl AgentStore {
    pub fn new(cfg: &Config) -> AgentStore {
        AgentStore {
            sessions: HashMap::new(),
            last_output: HashMap::new(),
            state: State::load(&cfg.dir),
        }
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

        let mut args: Vec<String> = if resume {
            vec!["--resume".into(), bot.id.session_name()]
        } else {
            vec!["--name".into(), bot.id.session_name()]
        };
        args.extend([
            "--append-system-prompt-file".into(),
            persona.display().to_string(),
            // Rooms + handoffs live under the config dir; make writes there
            // first-class instead of out-of-scope.
            "--add-dir".into(),
            cfg.dir.display().to_string(),
            // The Linear + Figma hooks, independent of repo-level MCP config.
            "--mcp-config".into(),
            cfg.mcp_config_path().display().to_string(),
        ]);
        if let Some(p) = prompt {
            // The prompt rides argv: typing into a BOOTING pty races the
            // child's first paint, but a positional prompt cannot be dropped.
            args.push(p.to_string());
        }

        let term = pty::Terminal::spawn(bot.id.clone(), "claude", &args, &repo, 24, 80, tx.clone())?;
        self.sessions.insert(
            bot.id.clone(),
            AgentSession {
                term,
                last_prompt: prompt.map(str::to_string),
                spawned_at: Instant::now(),
                resumed: resume,
                exit_handled: false,
            },
        );
        if !resume {
            self.state.mark_spawned(&cfg.dir, &bot.id);
        }
        Ok(())
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
    }
}

#[derive(PartialEq, Eq)]
pub enum RelaunchHint {
    No,
    /// A resumed session is gone — spawn fresh.
    FreshSpawn,
}
