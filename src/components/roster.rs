//! The sidebar — birds first, rooms below, Grok-Bot-shaped: a persistent
//! contact list whose selection drives the content pane beside it. Moving the
//! cursor switches the conversation; ⏎/`a` steps INTO it (keyboard to the
//! bird, or the room composer).

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::action::{Action, Effects, Msg};
use crate::command::{Command, CommandResult};
use crate::components::{hits, Component};
use crate::config::Config;
use crate::keymap::{bind, bind_alias, ch, key, Binding, KeyCode};
use crate::shared::Shared;
use crate::ui::{bird_color, dim, muted, status_span, ACCENT, DIM, MUTED};
use crate::widgets::list_nav::{ListNav, Wrap};

/// One logical row: a bird or a room, in display order.
#[derive(Clone, Copy)]
pub enum Row {
    Bot(usize),
    Room(usize),
}

pub fn rows(cfg: &Config) -> Vec<Row> {
    let mut out: Vec<Row> = (0..cfg.bots.len()).map(Row::Bot).collect();
    out.extend((0..cfg.rooms.len()).map(Row::Room));
    out
}

#[derive(Default)]
pub struct Roster {
    nav: ListNav,
    /// Where each card was drawn, for clicks.
    row_rects: Vec<(Rect, usize)>,
    /// Scroll offset in rows — follows the selection so it's never clipped.
    scroll: u16,
    /// The footer buttons: `+ bird` and `+ room`.
    new_bot_rect: Rect,
    new_room_rect: Rect,
    /// Branch refresh cadence: every 15th tick.
    ticks: u32,
}

pub static KEYMAP: &[Binding] = &[
    bind_alias(
        ch('j'),
        &[key(KeyCode::Down)],
        Action::Down,
        Some("move"),
        "select the next bird/room (the pane follows)",
    ),
    bind_alias(ch('k'), &[key(KeyCode::Up)], Action::Up, None, "select the previous"),
    bind(ch('g'), Action::Top, None, "jump to the top"),
    bind(ch('G'), Action::Bottom, None, "jump to the bottom"),
    bind(
        key(KeyCode::Enter),
        Action::Confirm,
        Some("open"),
        "step into the conversation (wake the bird / write the room)",
    ),
    bind(ch('a'), Action::FocusAgent, Some("talk"), "wake the bird and take the keyboard"),
    bind(
        ch('@'),
        Action::Handoff,
        Some("handoff"),
        "hand work to a teammate (the selected bird packages its context)",
    ),
    bind(ch('x'), Action::StopBot, None, "stop the selected bird's session"),
    bind(
        ch('N'),
        Action::FreshStart,
        None,
        "abandon the bird's conversation and hatch a fresh one",
    ),
    bind(
        ch('p'),
        Action::Profile,
        None,
        "bird profile: persona, routines, notifications",
    ),
];

impl Roster {
    /// Keep the content pane pointed at the selection — the whole sidebar model.
    pub fn sync(&mut self, s: &mut Shared) {
        let all = rows(&s.config);
        self.nav.clamp(all.len());
        match all.get(self.nav.selected) {
            Some(Row::Bot(i)) => {
                let id = s.config.bots[*i].id.clone();
                if s.current_bot.as_ref() != Some(&id) {
                    s.agent_focused = false;
                }
                // Looking at a bird reads its news.
                s.agents.clear_unread(&id);
                s.current_bot = Some(id);
                s.current_room = None;
            }
            Some(Row::Room(i)) => {
                let id = s.config.rooms[*i].id.clone();
                s.current_room = Some(id);
                s.current_bot = None;
                s.agent_focused = false;
            }
            None => {
                s.current_bot = None;
                s.current_room = None;
                s.agent_focused = false;
            }
        }
    }

    /// Move the cursor to a room (after creating one).
    pub fn select_room(&mut self, s: &mut Shared, room_id: &str) {
        if let Some(idx) = s.config.rooms.iter().position(|r| r.id == room_id) {
            self.nav.selected = s.config.bots.len() + idx;
        }
        self.sync(s);
    }

    /// Step into the selection: wake a bird and hand it the keyboard, or open
    /// the room composer (via the shell, which owns the room component).
    fn confirm(&mut self, s: &mut Shared, fx: &mut Effects) {
        self.sync(s);
        match rows(&s.config).get(self.nav.selected) {
            Some(Row::Bot(_)) => {
                if let Some(id) = s.current_bot.clone() {
                    let prompt = s.opening_prompt(&id);
                    s.boot_bot(&id, prompt.as_deref());
                    s.agent_focused = true;
                }
            }
            Some(Row::Room(_)) => {
                if let Some(id) = s.current_room.clone() {
                    fx.msg(Msg::OpenRoom(id));
                }
            }
            None => {}
        }
    }

    fn reload_branches(&mut self, s: &mut Shared, fx: &mut Effects) {
        let gen = s.branches.begin();
        let repos = s
            .config
            .bots
            .iter()
            .map(|b| (b.id.clone(), b.repo_path()))
            .collect();
        fx.command(Command::LoadBranches { gen, repos });
    }
}

impl Component for Roster {
    fn update(&mut self, a: Action, s: &mut Shared, fx: &mut Effects) {
        let len = rows(&s.config).len();
        if self.nav.handle(a, len, Wrap::Clamp) {
            self.sync(s);
            return;
        }
        match a {
            Action::Confirm | Action::FocusAgent => self.confirm(s, fx),
            Action::StopBot => {
                if let Some(Row::Bot(i)) = rows(&s.config).get(self.nav.selected) {
                    let bot = &s.config.bots[*i];
                    let (id, name) = (bot.id.clone(), bot.name.clone());
                    s.agents.stop(&id);
                    s.agent_focused = false;
                    fx.flash(format!("{name} stopped — its session resumes by name"));
                }
            }
            Action::FreshStart => {
                if let Some(Row::Bot(i)) = rows(&s.config).get(self.nav.selected) {
                    let id = s.config.bots[*i].id.clone();
                    s.fresh_bot(&id);
                    s.agent_focused = true;
                }
            }
            Action::Handoff => {
                if let Some(Row::Bot(i)) = rows(&s.config).get(self.nav.selected) {
                    fx.msg(Msg::Compose {
                        source: Some(s.config.bots[*i].id.clone()),
                        preselect: None,
                    });
                }
            }
            Action::Profile => {
                if let Some(Row::Bot(i)) = rows(&s.config).get(self.nav.selected) {
                    fx.msg(Msg::OpenProfile(s.config.bots[*i].id.clone()));
                }
            }
            Action::Reload => self.reload_branches(s, fx),
            _ => {}
        }
    }

    fn on_enter(&mut self, s: &mut Shared, fx: &mut Effects) {
        self.sync(s);
        self.reload_branches(s, fx);
    }

    fn on_tick(&mut self, s: &mut Shared, fx: &mut Effects) {
        self.ticks += 1;
        if self.ticks % 15 == 0 && !s.branches.in_flight {
            self.reload_branches(s, fx);
        }
    }

    fn on_result(&mut self, r: &CommandResult, s: &mut Shared) {
        let CommandResult::Branches { gen, info } = r else {
            return;
        };
        s.branches.accept(
            *gen,
            info.iter()
                .map(|b| crate::command::BranchInfo {
                    bot: b.bot.clone(),
                    branch: b.branch.clone(),
                    dirty: b.dirty,
                })
                .collect(),
        );
    }

    fn handle_mouse(&mut self, m: MouseEvent, s: &mut Shared, fx: &mut Effects) {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if hits(self.new_bot_rect, m.column, m.row) {
                    fx.msg(Msg::OpenNewBot);
                    return;
                }
                if hits(self.new_room_rect, m.column, m.row) {
                    fx.msg(Msg::OpenNewRoom);
                    return;
                }
                if let Some((_, idx)) = self
                    .row_rects
                    .iter()
                    .find(|(r, _)| hits(*r, m.column, m.row))
                {
                    if self.nav.selected == *idx {
                        self.confirm(s, fx); // second click steps in
                    } else {
                        self.nav.selected = *idx;
                        self.sync(s);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                self.nav.handle(Action::Down, rows(&s.config).len(), Wrap::Clamp);
                self.sync(s);
            }
            MouseEventKind::ScrollUp => {
                self.nav.handle(Action::Up, rows(&s.config).len(), Wrap::Clamp);
                self.sync(s);
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, s: &mut Shared) {
        self.row_rects.clear();
        let all = rows(&s.config);
        self.nav.clamp(all.len());

        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(DIM));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Card stream: section headers (1 row) + one rounded card per bird or
        // room (CARD_H rows). A scroll offset in rows keeps the SELECTED card
        // fully visible; partially-clipped cards are hidden, not mangled.
        const CARD_H: u16 = 4;
        enum Item {
            Header(&'static str, u16),
            Card(usize),
        }
        let mut items: Vec<Item> = vec![Item::Header("BIRDS", 1)];
        for (idx, row) in all.iter().enumerate() {
            if matches!(row, Row::Room(0)) {
                items.push(Item::Header("ROOMS", 2));
            }
            items.push(Item::Card(idx));
        }

        let avail = inner.height.saturating_sub(1); // footer keeps the last row
        let item_h = |item: &Item| match item {
            Item::Header(_, h) => *h,
            Item::Card(_) => CARD_H,
        };

        // Selected card's row span, for scroll-follow.
        let (mut sel_start, mut sel_end) = (0u16, 0u16);
        let mut cursor = 0u16;
        for item in &items {
            let h = item_h(item);
            if matches!(item, Item::Card(idx) if *idx == self.nav.selected) {
                (sel_start, sel_end) = (cursor, cursor + h);
            }
            cursor += h;
        }
        let total = cursor;
        if sel_start < self.scroll {
            self.scroll = sel_start;
        }
        if sel_end > self.scroll + avail {
            self.scroll = sel_end - avail;
        }
        self.scroll = self.scroll.min(total.saturating_sub(avail.min(total)));

        let name_w = inner.width.saturating_sub(8) as usize;
        let mut y = 0u16;
        let mut clipped = 0usize;
        for item in &items {
            let h = item_h(item);
            let top = y as i32 - self.scroll as i32;
            y += h;
            if top < 0 || top as u16 + h > avail {
                if matches!(item, Item::Card(_)) {
                    clipped += 1;
                }
                continue;
            }
            let rect = Rect {
                x: inner.x,
                y: inner.y + top as u16,
                width: inner.width,
                height: h,
            };
            match item {
                Item::Header(label, h) => {
                    frame.render_widget(
                        Paragraph::new(Line::from(muted(format!(" {label}")))),
                        Rect {
                            y: rect.y + h - 1, // blank leading row for ROOMS
                            height: 1,
                            ..rect
                        },
                    );
                }
                Item::Card(idx) => {
                    let selected = *idx == self.nav.selected;
                    self.row_rects.push((rect, *idx));
                    let (title, body) = match all[*idx] {
                        Row::Bot(i) => {
                            let bot = &s.config.bots[i];
                            let colour = bird_color(i);
                            let mut title = vec![
                                Span::raw(format!(" {} ", bot.glyph)),
                                Span::styled(
                                    format!("{:<9}", truncate(&bot.name, 9)),
                                    if selected {
                                        Style::default().fg(colour).add_modifier(Modifier::BOLD)
                                    } else {
                                        Style::default().fg(colour)
                                    },
                                ),
                                status_span(s.agents.status(&bot.id)),
                            ];
                            if s.agents.is_unread(&bot.id) {
                                // Grok-Bot unread dot: news since you last looked.
                                title.push(Span::styled(" ●", Style::default().fg(ACCENT)));
                            }

                            // Body: collab tag, then last prompt or branch.
                            let collab = s.agents.collab(&bot.id).map(|c| c.tag());
                            let tag_w = collab.as_ref().map_or(0, |t| t.chars().count() + 1);
                            let preview = s
                                .agents
                                .get(&bot.id)
                                .and_then(|sess| sess.last_prompt.clone())
                                .map(|p| {
                                    format!("“{}”", truncate(&p, name_w.saturating_sub(tag_w)))
                                })
                                .or_else(|| {
                                    s.branches.data.as_ref().and_then(|v| {
                                        v.iter().find(|b| b.bot == bot.id).map(|b| {
                                            format!(
                                                "⎇ {}{}",
                                                truncate(
                                                    &b.branch,
                                                    name_w.saturating_sub(4 + tag_w)
                                                ),
                                                if b.dirty { " ●" } else { "" }
                                            )
                                        })
                                    })
                                });
                            let mut body = vec![Span::raw(" ")];
                            if let Some(tag) = collab {
                                body.push(Span::styled(
                                    format!("{tag} "),
                                    Style::default().fg(crate::ui::WARN),
                                ));
                            }
                            match preview {
                                Some(p) => body.push(dim(p)),
                                None => body.push(dim("resting")),
                            }
                            (Line::from(title), Line::from(body))
                        }
                        Row::Room(i) => {
                            let room = &s.config.rooms[i];
                            let title = vec![
                                Span::styled(
                                    format!(" # {:<10}", truncate(&room.name, 10)),
                                    if selected {
                                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                                    } else {
                                        Style::default().fg(MUTED)
                                    },
                                ),
                                dim(format!("{} birds", room.members.len())),
                            ];
                            let members: Vec<String> =
                                room.members.iter().map(|m| format!("@{m}")).collect();
                            let body = vec![
                                Span::raw(" "),
                                dim(truncate(&members.join(" "), name_w + 4)),
                            ];
                            (Line::from(title), Line::from(body))
                        }
                    };

                    let card = Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(if selected { ACCENT } else { DIM }));
                    let card_inner = card.inner(rect);
                    frame.render_widget(card, rect);
                    frame.render_widget(Paragraph::new(vec![title, body]), card_inner);
                }
            }
        }

        if s.config.bots.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(dim(" no birds — n adds one"))),
                Rect {
                    x: inner.x,
                    y: inner.y + 1,
                    width: inner.width,
                    height: 1,
                },
            );
        }
        if clipped > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(dim(format!(" ↕ {clipped} more off-screen"))))
                    .alignment(ratatui::layout::Alignment::Right),
                Rect {
                    x: inner.x,
                    y: inner.y + avail.saturating_sub(1),
                    width: inner.width,
                    height: 1,
                },
            );
        }

        // Footer buttons, pinned to the sidebar's bottom line.
        let footer_y = inner.y + inner.height.saturating_sub(1);
        let bot_label = " + bird ";
        let room_label = " + room ";
        self.new_bot_rect = Rect {
            x: inner.x + 1,
            y: footer_y,
            width: bot_label.chars().count() as u16,
            height: 1,
        };
        self.new_room_rect = Rect {
            x: self.new_bot_rect.x + self.new_bot_rect.width + 2,
            y: footer_y,
            width: room_label.chars().count() as u16,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(bot_label, Style::default().fg(ACCENT)),
                Span::raw("  "),
                Span::styled(room_label, Style::default().fg(ACCENT)),
                dim("  n · c"),
            ])),
            Rect {
                x: inner.x,
                y: footer_y,
                width: inner.width,
                height: 1,
            },
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    let max = max.max(4);
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max - 1).collect();
        format!("{kept}…")
    }
}
