//! Cross-component state. Components get `&mut Shared`; anything one screen
//! changes that another cares about lives here, not in a component.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::agent_store::AgentStore;
use crate::command::{BranchInfo, Slot};
use crate::config::{BotId, Config, SessionKey};
use crate::event::Event;
use crate::events::EventsReader;
use crate::prompts;
use crate::room;
use crate::routine::LocalTime;

pub struct Shared {
    pub config: Config,
    pub agents: AgentStore,
    pub watcher: room::Watcher,
    /// Branch + dirty per bot repo — loaded off-thread, shown on the roster.
    pub branches: Slot<Vec<BranchInfo>>,
    /// In-flight/gen guard for `claude agents --json` polling.
    pub agents_poll: Slot<()>,
    /// Offset-tracked reader of the hook events file.
    pub events: EventsReader,
    /// The thread screen's bot / the room screen's room.
    pub current_bot: Option<BotId>,
    /// Which of the bot's session tabs the thread pane shows (1 = primary).
    pub current_tab: u8,
    pub current_room: Option<String>,
    /// The embedded terminal holds the keyboard (every key but ctrl+a).
    pub agent_focused: bool,
    /// Transient status line in the chrome.
    pub flash: Option<(Instant, String)>,
    pub tx: Sender<Event>,
    clock: LocalTime,
    clock_at: Instant,
}

impl Shared {
    pub fn new(config: Config, tx: Sender<Event>) -> Shared {
        let agents = AgentStore::new(&config);
        Shared {
            config,
            agents,
            watcher: room::Watcher::default(),
            branches: Slot::default(),
            agents_poll: Slot::default(),
            events: EventsReader::default(),
            current_bot: None,
            current_tab: 1,
            current_room: None,
            agent_focused: false,
            flash: None,
            tx,
            clock: LocalTime::detect(),
            clock_at: Instant::now(),
        }
    }

    /// Local-time view for routine scheduling, re-detected hourly (DST).
    pub fn local_clock(&mut self) -> LocalTime {
        if self.clock_at.elapsed() > Duration::from_secs(3600) {
            self.clock = LocalTime::detect();
            self.clock_at = Instant::now();
        }
        self.clock
    }

    pub fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((Instant::now(), text.into()));
    }

    /// The session the thread pane is showing right now.
    pub fn current_key(&self) -> Option<SessionKey> {
        self.current_bot.clone().map(|bot| SessionKey {
            bot,
            tab: self.current_tab,
        })
    }

    /// Spawn/resume a bot's PRIMARY session, typing `prompt` into it. Every
    /// external signal lands here; extra tabs are only ever booted by a human
    /// via [`Shared::boot_key`].
    pub fn boot_bot(&mut self, id: &BotId, prompt: Option<&str>) {
        self.boot_key(&SessionKey::primary(id.clone()), prompt);
    }

    /// Spawn/resume one session of a bot, typing `prompt` into it; errors
    /// become a flash instead of a crash — a missing repo must not take the
    /// TUI down.
    pub fn boot_key(&mut self, key: &SessionKey, prompt: Option<&str>) {
        let Some(bot) = self.config.bot(&key.bot).cloned() else {
            self.flash(format!("no bot named {:?}", key.bot));
            return;
        };
        let tx = self.tx.clone();
        if let Err(e) = self
            .agents
            .ensure_running_key(&self.config, &bot, key.tab, prompt, &tx)
        {
            self.flash(format!("{e:#}"));
        }
    }

    /// First prompt for a never-before-spawned session, None otherwise.
    pub fn opening_prompt(&self, key: &SessionKey) -> Option<String> {
        let bot = self.config.bot(&key.bot)?;
        (!self.agents.has_session_record(key)).then(|| prompts::first_flight(bot))
    }

    /// Abandon the bird's PRIMARY conversation on purpose and start a new one.
    pub fn fresh_bot(&mut self, id: &BotId) {
        let Some(bot) = self.config.bot(id).cloned() else {
            self.flash(format!("no bot named {id:?}"));
            return;
        };
        // Show the session that was just hatched, not a stale tab.
        if self.current_bot.as_ref() == Some(id) {
            self.current_tab = 1;
        }
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
