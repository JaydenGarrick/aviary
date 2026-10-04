//! The thin shell: layered key dispatch, mouse routing, overlay handling, msg
//! fan-out, and the run loop.
//!
//! Layout is Grok-Bot-shaped: the sidebar (roster) is ALWAYS visible and owns
//! the keyboard; the content pane beside it shows whatever the sidebar
//! selection points at — a bird's live session or a room transcript.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::DefaultTerminal;

use crate::action::{Action, Effects, Msg};
use crate::agent_store::Collab;
use crate::command::Executor;
use crate::components::{hits, Component, Components};
use crate::config::{self, BotId, Config};
use crate::event::{self, Event};
use crate::keymap::{self, bind, ch, Binding, GLOBAL};
use crate::overlays::{AddBotForm, ComposeForm, FormEvent, NewRoomForm, Overlay};
use crate::prompts;
use crate::shared::Shared;
use crate::ui;

const FLASH_TTL: Duration = Duration::from_secs(4);
const SIDEBAR_WIDTH: u16 = 34;

/// Keys that act on the CONTENT pane (the room transcript) while the sidebar
/// keeps navigation. Merged into dispatch and hints only when a room is up.
static CONTENT_KEYS: &[Binding] = &[
    bind(ch('i'), Action::Compose, None, "write to the room (same as ⏎)"),
    bind(ch('u'), Action::PageUp, Some("scroll"), "scroll the room up"),
    bind(ch('d'), Action::PageDown, None, "scroll the room down"),
];

pub struct App {
    components: Components,
    overlay: Overlay,
    shared: Shared,
    executor: Executor,
    should_quit: bool,
    sidebar: Rect,
    content: Rect,
}

pub fn run(mut terminal: DefaultTerminal) -> Result<()> {
    let dir = config::default_dir()?;
    let cfg = Config::load_or_scaffold(dir)?;
    let (tx, rx) = event::channel();

    let mut app = App {
        components: Components::new(),
        overlay: Overlay::None,
        shared: Shared::new(cfg, tx.clone()),
        executor: Executor::new(tx),
        should_quit: false,
        sidebar: Rect::default(),
        content: Rect::default(),
    };
    // Point the content pane at the first bird and start the branch load.
    let mut fx = Effects::default();
    app.components.roster.on_enter(&mut app.shared, &mut fx);
    app.apply(fx);

    while !app.should_quit {
        terminal.draw(|frame| app.draw(frame))?;
        match rx.recv() {
            Ok(ev) => {
                app.handle(ev);
                // Coalesce: a chatty bird must not spin the renderer.
                while let Ok(next) = rx.try_recv() {
                    app.handle(next);
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

impl App {
    fn room_selected(&self) -> bool {
        self.shared.current_room.is_some()
    }

    // ---------------------------------------------------------------- events

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::AgentOutput(id) => {
                use crate::agent_store::RelaunchHint;
                if self.shared.agents.note_output(&self.shared.config, &id)
                    == RelaunchHint::FreshSpawn
                {
                    // The named session was gone — start the bird fresh.
                    let prompt = self.shared.opening_prompt(&id);
                    self.shared.boot_bot(&id, prompt.as_deref());
                    self.shared
                        .flash(format!("{id}'s old session was gone — hatched fresh"));
                }
            }
            Event::Done(result) => {
                self.components.roster.on_result(&result, &mut self.shared);
            }
            Event::Tick => self.on_tick(),
            Event::Resize => {}
        }
    }

    fn on_tick(&mut self) {
        if let Some((at, _)) = &self.shared.flash {
            if at.elapsed() > FLASH_TTL {
                self.shared.flash = None;
            }
        }

        // Fresh sessions become resumable only after surviving long enough —
        // a boot crash must never leave a ghost resume record.
        self.shared.agents.tick(&self.shared.config);

        // Room dispatch is aviary-wide: bot appends wake @-mentioned members
        // whether or not anyone is looking at the room.
        let dispatches = self.shared.watcher.poll(&self.shared.config);
        for (target, room_id, prompt) in dispatches {
            self.shared.boot_bot(&target, Some(&prompt));
            self.shared.agents.set_collab(&target, Collab::Room(room_id));
        }

        let mut fx = Effects::default();
        self.components.roster.on_tick(&mut self.shared, &mut fx);
        if self.room_selected() {
            self.components.room.on_tick(&mut self.shared, &mut fx);
        }
        self.apply(fx);
    }

    // ------------------------------------------------------------------ keys
    //
    // The layer order is load-bearing. Interrupts sit ABOVE capturing so
    // ctrl+c quits even from inside a text field.

    fn on_key(&mut self, key: KeyEvent) {
        // ① the bird holds the keyboard — everything goes to it but ctrl+a.
        if self.shared.agent_focused {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
                self.shared.agent_focused = false;
                return;
            }
            let Some(id) = self.shared.current_bot.clone() else {
                self.shared.agent_focused = false;
                return;
            };
            match self.shared.agents.get_mut(&id) {
                Some(s) if s.term.is_running() => {
                    s.term.send_key(key);
                    // The user taking over means the bird works for THEM now.
                    self.shared.agents.clear_collab(&id);
                }
                _ => self.shared.agent_focused = false,
            }
            return;
        }

        // ② help: any key closes.
        if matches!(self.overlay, Overlay::Help) {
            self.overlay = Overlay::None;
            return;
        }

        // ③ interrupts — above capturing, so a text field can't eat ctrl+c.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // ④ modal forms capture everything.
        if !self.overlay.is_none() {
            let event = match &mut self.overlay {
                Overlay::AddBot(f) => f.handle_key(key),
                Overlay::NewRoom(f) => f.handle_key(key),
                Overlay::Compose(f) => f.handle_key(key),
                _ => return,
            };
            self.on_form_event(event);
            return;
        }

        // ④b the room composer captures everything.
        if self.room_selected() && self.components.room.capturing() {
            let mut fx = Effects::default();
            self.components.room.handle_text(key, &mut self.shared, &mut fx);
            self.apply(fx);
            return;
        }

        // ⑤/⑥ sidebar keymap (+ content keys when a room is up), then globals.
        let tables: &[&[Binding]] = if self.room_selected() {
            &[crate::components::roster::KEYMAP, CONTENT_KEYS, GLOBAL]
        } else {
            &[crate::components::roster::KEYMAP, GLOBAL]
        };
        let Some(action) = keymap::resolve(tables, &key) else {
            return;
        };
        self.dispatch(action);
    }

    fn dispatch(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::Help => self.overlay = Overlay::Help,
            Action::NewBot => self.open_new_bot(),
            Action::NewRoom => self.open_new_room(),
            Action::Back => {
                self.shared.agent_focused = false;
            }
            Action::Reload => {
                self.reload_config();
                let mut fx = Effects::default();
                self.components
                    .roster
                    .update(Action::Reload, &mut self.shared, &mut fx);
                if self.room_selected() {
                    self.components
                        .room
                        .update(Action::Reload, &mut self.shared, &mut fx);
                }
                self.apply(fx);
            }
            // Content-pane verbs go to the room view; everything else is the
            // sidebar's (which owns selection and all bird actions).
            Action::PageUp | Action::PageDown | Action::Compose => {
                if self.room_selected() {
                    let mut fx = Effects::default();
                    self.components.room.update(action, &mut self.shared, &mut fx);
                    self.apply(fx);
                }
            }
            other => {
                let mut fx = Effects::default();
                self.components
                    .roster
                    .update(other, &mut self.shared, &mut fx);
                self.apply(fx);
            }
        }
    }

    fn open_new_bot(&mut self) {
        self.overlay = Overlay::AddBot(AddBotForm::new(self.shared.config.bots.len()));
    }

    fn open_new_room(&mut self) {
        if self.shared.config.bots.len() < 2 {
            self.shared.flash("a room needs at least two birds — n adds more");
        } else {
            self.overlay = Overlay::NewRoom(NewRoomForm::new(&self.shared.config.bots));
        }
    }

    fn on_form_event(&mut self, event: FormEvent) {
        match event {
            FormEvent::Consumed => {}
            FormEvent::Cancel => self.overlay = Overlay::None,
            FormEvent::AddBot { name, glyph, repo } => {
                match self.shared.config.add_bot(&name, &name, &glyph, &repo) {
                    Ok(id) => {
                        self.overlay = Overlay::None;
                        self.shared
                            .flash(format!("{glyph} {name} hatched — session {}", id.session_name()));
                    }
                    Err(e) => {
                        if let Overlay::AddBot(f) = &mut self.overlay {
                            f.error = Some(format!("{e:#}"));
                        }
                    }
                }
            }
            FormEvent::NewRoom { name, members } => {
                match self.shared.config.add_room(&name, members) {
                    Ok(id) => {
                        self.overlay = Overlay::None;
                        let mut fx = Effects::default();
                        fx.msg(Msg::OpenRoom(id));
                        self.apply(fx);
                    }
                    Err(e) => {
                        if let Overlay::NewRoom(f) = &mut self.overlay {
                            f.error = Some(format!("{e:#}"));
                        }
                    }
                }
            }
            FormEvent::Compose { source, target, text } => {
                self.overlay = Overlay::None;
                self.send_compose(source, target, text);
            }
        }
    }

    /// The handoff/direct-message submit. With a source bird, the SOURCE owns
    /// packaging (it has the context); the target is booted first so the
    /// SendMessage has a live recipient. Both get collaboration tags.
    fn send_compose(&mut self, source: Option<BotId>, target: BotId, text: String) {
        match source {
            Some(src) => {
                let (Some(from), Some(to)) = (
                    self.shared.config.bot(&src).cloned(),
                    self.shared.config.bot(&target).cloned(),
                ) else {
                    return;
                };
                let wake = self.shared.opening_prompt(&target);
                self.shared.boot_bot(&target, wake.as_deref());
                let prompt = prompts::handoff(&from, &to, &text, &self.shared.config.handoffs_dir());
                self.shared.boot_bot(&src, Some(&prompt));
                self.shared.agents.set_collab(&src, Collab::Delegating(to.id.0.clone()));
                self.shared.agents.set_collab(&target, Collab::Receiving(from.id.0.clone()));
                self.shared
                    .flash(format!("{} is packaging a handoff for {}", from.name, to.name));
            }
            None => {
                let prompt = prompts::direct(&text);
                self.shared.boot_bot(&target, Some(&prompt));
                // A direct user message makes the bird the USER's again.
                self.shared.agents.clear_collab(&target);
                let name = self
                    .shared
                    .config
                    .bot(&target)
                    .map(|b| b.name.clone())
                    .unwrap_or_default();
                self.shared.flash(format!("sent to {name}"));
            }
        }
    }

    // ----------------------------------------------------------------- mouse

    fn on_mouse(&mut self, m: MouseEvent) {
        if matches!(self.overlay, Overlay::Help) {
            self.overlay = Overlay::None;
            return;
        }
        if !self.overlay.is_none() {
            let event = match &mut self.overlay {
                Overlay::AddBot(f) => f.handle_mouse(m),
                Overlay::NewRoom(f) => f.handle_mouse(m),
                Overlay::Compose(f) => f.handle_mouse(m),
                _ => return,
            };
            self.on_form_event(event);
            return;
        }

        let mut fx = Effects::default();
        if hits(self.sidebar, m.column, m.row) {
            self.components.roster.handle_mouse(m, &mut self.shared, &mut fx);
        } else if hits(self.content, m.column, m.row) {
            if self.room_selected() {
                self.components.room.handle_mouse(m, &mut self.shared, &mut fx);
            } else {
                self.components.thread.handle_mouse(m, &mut self.shared, &mut fx);
            }
        }
        self.apply(fx);
    }

    // --------------------------------------------------------------- effects

    fn apply(&mut self, fx: Effects) {
        for cmd in fx.commands {
            self.executor.run(cmd);
        }
        for msg in fx.msgs {
            match msg {
                Msg::OpenRoom(id) => {
                    self.shared.current_room = Some(id.clone());
                    self.shared.current_bot = None;
                    self.components.roster.select_room(&mut self.shared, &id);
                    let mut enter = Effects::default();
                    self.components.room.on_enter(&mut self.shared, &mut enter);
                    self.components.room.start_compose();
                    self.apply(enter);
                }
                Msg::OpenNewBot => self.open_new_bot(),
                Msg::OpenNewRoom => self.open_new_room(),
                Msg::Compose { source, preselect } => {
                    match ComposeForm::new(&self.shared, source, preselect) {
                        Some(form) => self.overlay = Overlay::Compose(form),
                        None => self.shared.flash("no other birds to message — n adds one"),
                    }
                }
                Msg::Flash(text) => self.shared.flash(text),
            }
        }
    }

    fn reload_config(&mut self) {
        match Config::load_or_scaffold(self.shared.config.dir.clone()) {
            Ok(cfg) => {
                self.shared.config = cfg;
                self.components.roster.sync(&mut self.shared);
                self.shared.flash("config reloaded");
            }
            Err(e) => self.shared.flash(format!("reload failed: {e:#}")),
        }
    }

    // ------------------------------------------------------------------ draw

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // brand + flash + live count
                Constraint::Length(1), // air
                Constraint::Min(0),    // sidebar | content
                Constraint::Length(1), // generated hints
            ])
            .split(frame.area());

        ui::draw_brand(
            frame,
            rows[0],
            self.shared.config.bots.len(),
            self.shared.agents.running_count(),
            self.shared.flash.as_ref().map(|(_, m)| m.as_str()),
        );

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(30)])
            .split(rows[2]);
        self.sidebar = cols[0];
        self.content = cols[1];

        self.components.roster.draw(frame, cols[0], &mut self.shared);
        if self.room_selected() {
            self.components.room.draw(frame, cols[1], &mut self.shared);
        } else {
            self.components.thread.draw(frame, cols[1], &mut self.shared);
        }

        let hint_override = if self.shared.agent_focused {
            Some("the bird has the keyboard — every key goes to it except ctrl+a, which hands it back")
        } else if self.room_selected() && self.components.room.capturing() {
            Some("writing to the room — ⏎ sends · esc cancels · @name wakes just that bird")
        } else if !self.overlay.is_none() && !matches!(self.overlay, Overlay::Help) {
            Some("fill the form — ⏎ submits · esc cancels · fields are clickable")
        } else {
            None
        };
        let tables: &[&[Binding]] = if self.room_selected() {
            &[crate::components::roster::KEYMAP, CONTENT_KEYS, GLOBAL]
        } else {
            &[crate::components::roster::KEYMAP, GLOBAL]
        };
        ui::draw_hints(frame, rows[3], tables, hint_override);

        match &mut self.overlay {
            Overlay::None => {}
            Overlay::Help => ui::draw_help(
                frame,
                frame.area(),
                &[
                    ("sidebar", crate::components::roster::KEYMAP),
                    ("room", CONTENT_KEYS),
                    ("everywhere", GLOBAL),
                ],
            ),
            Overlay::AddBot(f) => f.draw(frame, frame.area()),
            Overlay::NewRoom(f) => f.draw(frame, frame.area()),
            Overlay::Compose(f) => f.draw(frame, frame.area()),
        }
    }
}
