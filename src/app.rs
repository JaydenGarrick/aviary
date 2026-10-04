//! The thin shell: layered key dispatch, overlay routing, msg fan-out, and the
//! run loop. Everything screen-specific lives in `components/`; everything
//! shared lives in `shared.rs`. This file should stay small — growth here is
//! the smell the mb migration plan exists to prevent.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::DefaultTerminal;

use crate::action::{Action, Effects, Msg};
use crate::command::Executor;
use crate::components::{Component, Components, Screen};
use crate::config::{self, Config};
use crate::event::{self, Event};
use crate::keymap::{self, GLOBAL};
use crate::overlays::{AddBotForm, ComposeForm, FormEvent, NewRoomForm, Overlay};
use crate::prompts;
use crate::shared::Shared;
use crate::ui;

const FLASH_TTL: Duration = Duration::from_secs(4);

pub struct App {
    screen: Screen,
    components: Components,
    overlay: Overlay,
    shared: Shared,
    executor: Executor,
    should_quit: bool,
}

pub fn run(mut terminal: DefaultTerminal) -> Result<()> {
    let dir = config::default_dir()?;
    let cfg = Config::load_or_scaffold(dir)?;
    let (tx, rx) = event::channel();

    let mut app = App {
        screen: Screen::Roster,
        components: Components::new(),
        overlay: Overlay::None,
        shared: Shared::new(cfg, tx.clone()),
        executor: Executor::new(tx),
        should_quit: false,
    };
    app.goto(Screen::Roster);

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
                self.components
                    .active_mut(self.screen)
                    .on_result(&result, &mut self.shared);
                // Branch info is roster data even when another screen is up.
                if self.screen != Screen::Roster {
                    self.components.roster.on_result(&result, &mut self.shared);
                }
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

        // Room dispatch is aviary-wide: bot appends wake @-mentioned members
        // whether or not anyone is looking at the room.
        let dispatches = self.shared.watcher.poll(&self.shared.config);
        for (target, prompt) in dispatches {
            self.shared.boot_bot(&target, Some(&prompt));
        }

        let mut fx = Effects::default();
        self.components
            .active_mut(self.screen)
            .on_tick(&mut self.shared, &mut fx);
        self.apply(fx);
    }

    // ------------------------------------------------------------------ keys
    //
    // The layer order is load-bearing; see the plan. Interrupts sit ABOVE
    // capturing so ctrl+c quits even from inside a text field.

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
                Some(s) if s.term.is_running() => s.term.send_key(key),
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
            self.on_form_key(key);
            return;
        }

        // ④b a component text field (the room composer) captures everything.
        let active = self.components.active_mut(self.screen);
        if active.capturing() {
            let mut fx = Effects::default();
            active.handle_text(key, &mut self.shared, &mut fx);
            self.apply(fx);
            return;
        }

        // ⑤/⑥ component keymap, then globals.
        let Some(action) = keymap::resolve(&[active.keymap(), GLOBAL], &key) else {
            return;
        };
        match action {
            Action::Quit => self.should_quit = true,
            Action::Help => self.overlay = Overlay::Help,
            Action::NewBot => {
                self.overlay = Overlay::AddBot(AddBotForm::new(self.shared.config.bots.len()));
            }
            Action::NewRoom => {
                if self.shared.config.bots.len() < 2 {
                    self.shared.flash("a room needs at least two birds — n adds more");
                } else {
                    self.overlay = Overlay::NewRoom(NewRoomForm::new(&self.shared.config.bots));
                }
            }
            Action::Back => {
                self.shared.agent_focused = false;
                if self.screen != Screen::Roster {
                    self.goto(Screen::Roster);
                }
            }
            Action::Reload => {
                self.reload_config();
                let mut fx = Effects::default();
                let screen = self.screen;
                self.components
                    .active_mut(screen)
                    .update(Action::Reload, &mut self.shared, &mut fx);
                self.apply(fx);
            }
            other => {
                let mut fx = Effects::default();
                self.components
                    .active_mut(self.screen)
                    .update(other, &mut self.shared, &mut fx);
                self.apply(fx);
            }
        }
    }

    fn on_form_key(&mut self, key: KeyEvent) {
        let event = match &mut self.overlay {
            Overlay::AddBot(f) => f.handle_key(key),
            Overlay::NewRoom(f) => f.handle_key(key),
            Overlay::Compose(f) => f.handle_key(key),
            _ => return,
        };
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
    /// SendMessage has a live recipient.
    fn send_compose(&mut self, source: Option<crate::config::BotId>, target: crate::config::BotId, text: String) {
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
                self.shared
                    .flash(format!("{} is packaging a handoff for {}", from.name, to.name));
            }
            None => {
                let prompt = prompts::direct(&text);
                self.shared.boot_bot(&target, Some(&prompt));
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
            return; // forms are keyboard-only
        }
        let mut fx = Effects::default();
        self.components
            .active_mut(self.screen)
            .handle_mouse(m, &mut self.shared, &mut fx);
        self.apply(fx);
    }

    // --------------------------------------------------------------- effects

    fn apply(&mut self, fx: Effects) {
        for cmd in fx.commands {
            self.executor.run(cmd);
        }
        for msg in fx.msgs {
            match msg {
                Msg::OpenThread(id) => {
                    self.shared.current_bot = Some(id);
                    self.goto(Screen::Thread);
                }
                Msg::OpenRoom(id) => {
                    self.shared.current_room = Some(id);
                    self.goto(Screen::Room);
                }
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

    fn goto(&mut self, screen: Screen) {
        self.screen = screen;
        let mut fx = Effects::default();
        self.components
            .active_mut(screen)
            .on_enter(&mut self.shared, &mut fx);
        self.apply(fx);
    }

    fn reload_config(&mut self) {
        match Config::load_or_scaffold(self.shared.config.dir.clone()) {
            Ok(cfg) => {
                self.shared.config = cfg;
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
                Constraint::Min(0),    // body
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

        self.components
            .active_mut(self.screen)
            .draw(frame, rows[2], &mut self.shared);

        let hint_override = if self.shared.agent_focused {
            Some("the bird has the keyboard — every key goes to it except ctrl+a, which hands it back")
        } else if !self.overlay.is_none() && !matches!(self.overlay, Overlay::Help) {
            Some("esc cancels")
        } else {
            None
        };
        let active_keymap = self.components.active_mut(self.screen).keymap();
        ui::draw_hints(frame, rows[3], &[active_keymap, GLOBAL], hint_override);

        match &self.overlay {
            Overlay::None => {}
            Overlay::Help => {
                let title = match self.screen {
                    Screen::Roster => "roster",
                    Screen::Thread => "thread",
                    Screen::Room => "room",
                };
                ui::draw_help(frame, frame.area(), &[(title, active_keymap), ("everywhere", GLOBAL)]);
            }
            Overlay::AddBot(f) => f.draw(frame, frame.area()),
            Overlay::NewRoom(f) => f.draw(frame, frame.area()),
            Overlay::Compose(f) => f.draw(frame, frame.area()),
        }
    }
}
