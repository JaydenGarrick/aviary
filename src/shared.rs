//! Cross-component state. Components get `&mut Shared`; anything one screen
//! changes that another cares about lives here, not in a component.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::agent_store::{AgentStore, Signaled};
use crate::command::{BranchInfo, Slot};
use crate::config::{BotId, Config, ConvKey, SessionKey};
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

    /// An EXTERNAL signal for a bird — every room, handoff, routine and
    /// webhook lands here, on the PRIMARY: `room: Some` for that room's
    /// conversation, `None` for home. Tab 1 switches between them only when
    /// idle (`AgentStore::signal` / `pump_switches`); extra tabs are only
    /// ever booted by a human via [`Shared::boot_key`]. `None` on error
    /// (flashed).
    pub fn signal(&mut self, id: &BotId, room: Option<&str>, prompt: Option<&str>) -> Option<Signaled> {
        let Some(bot) = self.config.bot(id).cloned() else {
            self.flash(format!("no bot named {id:?}"));
            return None;
        };
        let tx = self.tx.clone();
        match self.agents.signal(&self.config, &bot, room, prompt, &tx) {
            Ok(s) => Some(s),
            Err(e) => {
                self.flash(format!("{e:#}"));
                None
            }
        }
    }

    /// The person points a bird's tab 1 at one of its conversations.
    pub fn pick_conversation(&mut self, conv: &ConvKey) {
        let name = self
            .config
            .bot(&conv.slot.bot)
            .map_or_else(|| conv.slot.bot.0.clone(), |b| b.name.clone());
        let label = conv.label();
        if self.agents.pick_conversation(&self.config, conv) {
            self.flash(format!("{name} → {label} when it's idle"));
        } else {
            self.flash(format!("{name} — tab 1 is {label}"));
        }
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

    /// First prompt for a never-before-spawned session, None otherwise —
    /// never for a viewer tab (not a conversation of the bird's), nor for a
    /// room conversation (the room's notify prompt introduces the room; the
    /// persona rides the system prompt either way).
    pub fn opening_prompt(&self, key: &SessionKey) -> Option<String> {
        if self.agents.is_attached(key) || self.agents.conv_of(key).room.is_some() {
            return None;
        }
        let bot = self.config.bot(&key.bot)?;
        (!self.agents.has_session_record(key)).then(|| prompts::first_flight(bot))
    }

    /// Open (or jump to) a viewer tab on one of a bird's workers:
    /// `claude attach <id>` on the lowest free tab. Never two viewers on one
    /// worker. Closing the tab leaves the worker running.
    pub fn attach_worker(&mut self, bot: &BotId, worker: &str, label: &str, id: &str) {
        let Some(b) = self.config.bot(bot).cloned() else {
            self.flash(format!("no bot named {bot:?}"));
            return;
        };
        self.current_room = None;
        self.current_bot = Some(bot.clone());
        if let Some(tab) = self.agents.attached_tab(bot, worker) {
            self.current_tab = tab;
            self.agent_focused = true;
            self.agents.workers.clear_unread(worker);
            return;
        }
        let tab = crate::agent_store::lowest_free_tab(&self.agents.tabs(bot));
        let key = SessionKey { bot: bot.clone(), tab };
        let tx = self.tx.clone();
        match self.agents.launch_attach(&b, &key, worker, label, id, &tx) {
            Ok(()) => {
                self.current_tab = tab;
                self.agent_focused = true;
                self.agents.workers.clear_unread(worker);
                let close = crate::keymap::label_for(
                    &[crate::components::thread::KEYMAP],
                    crate::action::Action::CloseTab,
                )
                .unwrap_or_default();
                self.flash(format!(
                    "tab {tab} — viewing {label} · {close} closes it, the worker keeps running"
                ));
            }
            Err(e) => self.flash(format!("{e:#}")),
        }
    }

    /// Abandon ONE tab's conversation on purpose and start a new one there.
    pub fn fresh_key(&mut self, key: &SessionKey) {
        let Some(bot) = self.config.bot(&key.bot).cloned() else {
            self.flash(format!("no bot named {:?}", key.bot));
            return;
        };
        let tx = self.tx.clone();
        // A room conversation is introduced by the room, never first_flight.
        let home = self.agents.conv_of(key).room.is_none();
        let prompt = home.then(|| prompts::first_flight(&bot));
        if let Err(e) = self
            .agents
            .fresh_key(&self.config, &bot, key.tab, prompt.as_deref(), &tx)
        {
            self.flash(format!("{e:#}"));
        } else {
            self.flash(format!("{} tab {} — fresh conversation", bot.name, key.tab));
        }
    }

    /// Abandon the conversation the bird's tab 1 runs (home or a room's) on
    /// purpose and start a new one; the others are untouched.
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
        let conv = self.agents.conv_of(&SessionKey::primary(id.clone()));
        let prompt = conv.room.is_none().then(|| prompts::first_flight(&bot));
        if let Err(e) = self
            .agents
            .fresh_start(&self.config, &bot, prompt.as_deref(), &tx)
        {
            self.flash(format!("{e:#}"));
        } else {
            self.flash(format!("{} — fresh {} conversation", bot.name, conv.label()));
        }
    }
}
