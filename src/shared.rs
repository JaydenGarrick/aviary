//! Cross-component state. Components get `&mut Shared`; anything one screen
//! changes that another cares about lives here, not in a component.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::agent_store::AgentStore;
use crate::command::{BranchInfo, Slot};
use crate::config::{BotId, Config, Room, SessionKey};
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

    /// First prompt for a never-before-spawned session, None otherwise — and
    /// never for a viewer tab, which is not a conversation of the bird's.
    pub fn opening_prompt(&self, key: &SessionKey) -> Option<String> {
        if self.agents.is_attached(key) {
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
        let prompt = prompts::first_flight(&bot);
        if let Err(e) = self
            .agents
            .fresh_key(&self.config, &bot, key.tab, Some(&prompt), &tx)
        {
            self.flash(format!("{e:#}"));
        } else {
            self.flash(format!("{} tab {} — fresh conversation", bot.name, key.tab));
        }
    }

    /// A new room: every member's primary conversation ends now and starts
    /// over on the room's first message (the prompt rides argv then — see
    /// `AgentStore::reset_primary`). Returns the names of birds that had a
    /// live session or a resume record to lose.
    pub fn reset_for_room(&mut self, room: &Room) -> Vec<String> {
        let mut reset = Vec::new();
        for id in &room.members {
            let Some(name) = self.config.bot(id).map(|b| b.name.clone()) else {
                continue;
            };
            if self.agents.reset_primary(&self.config, id) {
                reset.push(name);
            }
            // Never leave the keyboard pointed at a PTY that was just dropped.
            if self.current_bot.as_ref() == Some(id) && self.current_tab == 1 {
                self.agent_focused = false;
            }
        }
        reset
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
