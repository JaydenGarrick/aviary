//! Cross-component state. Components get `&mut Shared`; anything one screen
//! changes that another cares about lives here, not in a component.

use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::agent_store::AgentStore;
use crate::command::{BranchInfo, Slot};
use crate::config::{BotId, Config};
use crate::event::Event;
use crate::prompts;
use crate::room;

pub struct Shared {
    pub config: Config,
    pub agents: AgentStore,
    pub watcher: room::Watcher,
    /// Branch + dirty per bot repo — loaded off-thread, shown on the roster.
    pub branches: Slot<Vec<BranchInfo>>,
    /// The thread screen's bot / the room screen's room.
    pub current_bot: Option<BotId>,
    pub current_room: Option<String>,
    /// The embedded terminal holds the keyboard (every key but ctrl+a).
    pub agent_focused: bool,
    /// Transient status line in the chrome.
    pub flash: Option<(Instant, String)>,
    pub tx: Sender<Event>,
}

impl Shared {
    pub fn new(config: Config, tx: Sender<Event>) -> Shared {
        let agents = AgentStore::new(&config);
        Shared {
            config,
            agents,
            watcher: room::Watcher::default(),
            branches: Slot::default(),
            current_bot: None,
            current_room: None,
            agent_focused: false,
            flash: None,
            tx,
        }
    }

    pub fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((Instant::now(), text.into()));
    }

    /// Spawn/resume a bot's session, typing `prompt` into it; errors become a
    /// flash instead of a crash — a missing repo must not take the TUI down.
    pub fn boot_bot(&mut self, id: &BotId, prompt: Option<&str>) {
        let Some(bot) = self.config.bot(id).cloned() else {
            self.flash(format!("no bot named {id:?}"));
            return;
        };
        let tx = self.tx.clone();
        if let Err(e) = self.agents.ensure_running(&self.config, &bot, prompt, &tx) {
            self.flash(format!("{e:#}"));
        }
    }

    /// First prompt for a never-before-spawned bird, None otherwise.
    pub fn opening_prompt(&self, id: &BotId) -> Option<String> {
        let bot = self.config.bot(id)?;
        (!self.agents.has_session_record(id)).then(|| prompts::first_flight(bot))
    }

    /// Abandon the bird's conversation on purpose and start a new one.
    pub fn fresh_bot(&mut self, id: &BotId) {
        let Some(bot) = self.config.bot(id).cloned() else {
            self.flash(format!("no bot named {id:?}"));
            return;
        };
        let tx = self.tx.clone();
        let prompt = prompts::first_flight(&bot);
        if let Err(e) = self
            .agents
            .fresh_start(&self.config, &bot, Some(&prompt), &tx)
        {
            self.flash(format!("{e:#}"));
        } else {
            self.flash(format!("{} — fresh conversation", bot.name));
        }
    }
}
