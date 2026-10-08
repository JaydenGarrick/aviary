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
use crate::flock::Workers;
use crate::keymap::{bind, bind_alias, ch, key, label_for, Binding, KeyCode, GLOBAL};
use crate::shared::Shared;
use crate::ui::{bird_color, dim, muted, status_span, ACCENT, DIM, MUTED};
use crate::widgets::list_nav::{ListNav, Wrap};

/// One logical row: a bird, one of its workers, or a room, in display order.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Bot(usize),
    /// A worker under bird `bot`, keyed by its session name.
    Worker { bot: usize, name: String },
    Room(usize),
}

/// Birds, each followed by its workers (sorted by label), then rooms. Pure:
/// tests seed `Workers` through `apply_poll`.
pub fn rows(cfg: &Config, workers: &Workers) -> Vec<Row> {
    let mut out = Vec::new();
    for (i, bot) in cfg.bots.iter().enumerate() {
        out.push(Row::Bot(i));
        out.extend(workers.for_bot(&bot.id).into_iter().map(|w| Row::Worker {
            bot: i,
            name: w.session_name.clone(),
        }));
    }
    out.extend((0..cfg.rooms.len()).map(Row::Room));
    out
}

fn rows_of(s: &Shared) -> Vec<Row> {
    rows(&s.config, &s.agents.workers)
}

/// The row index of a room — rooms sit after every bird and worker.
pub fn room_row(rows: &[Row], cfg: &Config, room_id: &str) -> Option<usize> {
    let idx = cfg.rooms.iter().position(|r| r.id == room_id)?;
    rows.iter().position(|r| matches!(r, Row::Room(i) if *i == idx))
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
    bind(
        ch('D'),
        Action::Delete,
        None,
        "remove the selected bird/room from the roster (asks first)",
    ),
    bind(
        ch('y'),
        Action::Yank,
        None,
        "copy the pane — the bird's screen, or the last room message as markdown",
    ),
];

impl Roster {
    /// Keep the content pane pointed at the selection — the whole sidebar model.
    pub fn sync(&mut self, s: &mut Shared) {
        let all = rows_of(s);
        self.nav.clamp(all.len());
        match all.get(self.nav.selected) {
            Some(Row::Bot(i)) => {
                let id = s.config.bots[*i].id.clone();
                if s.current_bot.as_ref() != Some(&id) {
                    s.agent_focused = false;
                    // A different bird starts on its primary tab.
                    s.current_tab = 1;
                }
                // Looking at a bird reads its news.
                s.agents.clear_unread(&id);
                s.current_bot = Some(id);
                s.current_room = None;
            }
            Some(Row::Worker { bot, name }) => {
                let id = s.config.bots[*bot].id.clone();
                if s.current_bot.as_ref() != Some(&id) {
                    s.agent_focused = false;
                    s.current_tab = 1;
                }
                // The pane follows: a viewer tab on this worker comes up if open.
                if let Some(tab) = s.agents.attached_tab(&id, name) {
                    if s.current_tab != tab {
                        s.agent_focused = false;
                    }
                    s.current_tab = tab;
                }
                // Looking at a worker reads ITS news; the bird's dot is its own.
                s.agents.workers.clear_unread(name);
                s.current_bot = Some(id);
                s.current_room = None;
            }
            Some(Row::Room(i)) => {
                let id = s.config.rooms[*i].id.clone();
                s.current_room = Some(id);
                s.current_bot = None;
                s.current_tab = 1;
                s.agent_focused = false;
            }
            None => {
                s.current_bot = None;
                s.current_room = None;
                s.current_tab = 1;
                s.agent_focused = false;
            }
        }
    }

    /// Move the cursor to a room (after creating one).
    pub fn select_room(&mut self, s: &mut Shared, room_id: &str) {
        if let Some(idx) = room_row(&rows_of(s), &s.config, room_id) {
            self.nav.selected = idx;
        }
        self.sync(s);
    }

    /// Step into the selection: wake a bird and hand it the keyboard, or open
    /// the room composer (via the shell, which owns the room component).
    /// Open the context menu for the row at `idx`, anchored at the click.
    fn open_context(&mut self, idx: usize, x: u16, y: u16, s: &Shared, fx: &mut Effects) {
        let target = match rows_of(s).get(idx) {
            Some(Row::Bot(i)) => crate::action::RosterTarget::Bird(s.config.bots[*i].id.clone()),
            Some(Row::Room(i)) => crate::action::RosterTarget::Room(s.config.rooms[*i].id.clone()),
            Some(Row::Worker { bot, name }) => {
                let Some(w) = s.agents.workers.get(name) else { return };
                crate::action::RosterTarget::Worker {
                    bot: s.config.bots[*bot].id.clone(),
                    name: name.clone(),
                    label: w.name.label(),
                    id: w.id.clone(),
                }
            }
            None => return,
        };
        fx.msg(Msg::OpenContext { target, x, y });
    }

    fn confirm(&mut self, s: &mut Shared, fx: &mut Effects) {
        self.sync(s);
        match rows_of(s).get(self.nav.selected).cloned() {
            Some(Row::Worker { bot, name }) => self.attach(bot, name, s, fx),
            Some(Row::Bot(_)) => {
                // Wake the tab in view — ⏎ with tab 2 up must not boot tab 1
                // underneath the keyboard focus.
                if let Some(key) = s.current_key() {
                    let prompt = s.opening_prompt(&key);
                    s.boot_key(&key, prompt.as_deref());
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

    /// ⏎ / `a` on a worker row: a viewer tab on it (needs its short id).
    fn attach(&mut self, bot: usize, name: String, s: &mut Shared, fx: &mut Effects) {
        let Some(w) = s.agents.workers.get(&name) else { return };
        let label = w.name.label();
        if w.id.is_empty() {
            fx.flash(format!("no attach id for {label} yet"));
            return;
        }
        fx.msg(Msg::AttachWorker {
            bot: s.config.bots[bot].id.clone(),
            name,
            label,
            id: w.id.clone(),
        });
    }

    /// Keys on a worker row: ⏎/`a` attach, `x` stops (after a confirm); the
    /// bird-only verbs say so instead of acting on the bird underneath.
    fn update_worker(&mut self, a: Action, bot: usize, name: String, s: &mut Shared, fx: &mut Effects) {
        match a {
            Action::Confirm | Action::FocusAgent => {
                self.sync(s);
                self.attach(bot, name, s, fx);
            }
            Action::StopBot => {
                if let Some(w) = s.agents.workers.get(&name) {
                    fx.msg(Msg::Confirm(crate::action::ConfirmTarget::StopWorker {
                        label: w.name.label(),
                        id: w.id.clone(),
                        name,
                    }));
                }
            }
            Action::Reload => self.reload_branches(s, fx),
            Action::FreshStart | Action::Handoff | Action::Profile | Action::Delete => {
                let label = s
                    .agents
                    .workers
                    .get(&name)
                    .map(|w| w.name.label())
                    .unwrap_or(name);
                let attach = label_for(&[KEYMAP], Action::Confirm).unwrap_or_default();
                let stop = label_for(&[KEYMAP], Action::StopBot).unwrap_or_default();
                fx.flash(format!("{label} is a worker — {attach} attaches · {stop} stops it"));
            }
            _ => {}
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
        let len = rows_of(s).len();
        if self.nav.handle(a, len, Wrap::Clamp) {
            self.sync(s);
            return;
        }
        if let Some(Row::Worker { bot, name }) = rows_of(s).get(self.nav.selected).cloned() {
            self.update_worker(a, bot, name, s, fx);
            return;
        }
        match a {
            Action::Confirm | Action::FocusAgent => self.confirm(s, fx),
            Action::StopBot => {
                if let Some(Row::Bot(i)) = rows_of(s).get(self.nav.selected) {
                    let bot = &s.config.bots[*i];
                    let (id, name) = (bot.id.clone(), bot.name.clone());
                    s.agents.stop(&id);
                    s.agent_focused = false;
                    fx.flash(format!("{name} stopped — its session resumes by name"));
                }
            }
            Action::FreshStart => {
                if let Some(Row::Bot(i)) = rows_of(s).get(self.nav.selected) {
                    let id = s.config.bots[*i].id.clone();
                    s.fresh_bot(&id);
                    s.agent_focused = true;
                }
            }
            Action::Handoff => {
                if let Some(Row::Bot(i)) = rows_of(s).get(self.nav.selected) {
                    fx.msg(Msg::Compose {
                        source: Some(s.config.bots[*i].id.clone()),
                        preselect: None,
                    });
                }
            }
            Action::Profile => {
                if let Some(Row::Bot(i)) = rows_of(s).get(self.nav.selected) {
                    fx.msg(Msg::OpenProfile(s.config.bots[*i].id.clone()));
                }
            }
            Action::Delete => {
                let target = match rows_of(s).get(self.nav.selected) {
                    Some(Row::Bot(i)) => {
                        crate::action::RosterTarget::Bird(s.config.bots[*i].id.clone())
                    }
                    Some(Row::Room(i)) => {
                        crate::action::RosterTarget::Room(s.config.rooms[*i].id.clone())
                    }
                    _ => return,
                };
                fx.msg(Msg::Confirm(crate::action::ConfirmTarget::Delete(target)));
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
            // Right-click: select the card and open its context menu there.
            MouseEventKind::Down(MouseButton::Right) => {
                let hit = self
                    .row_rects
                    .iter()
                    .find(|(r, _)| hits(*r, m.column, m.row))
                    .map(|(_, idx)| *idx);
                if let Some(idx) = hit {
                    self.nav.selected = idx;
                    self.sync(s);
                    self.open_context(idx, m.column, m.row, s, fx);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if hits(self.new_bot_rect, m.column, m.row) {
                    fx.msg(Msg::OpenNewBot);
                    return;
                }
                if hits(self.new_room_rect, m.column, m.row) {
                    fx.msg(Msg::OpenNewRoom);
                    return;
                }
                let hit = self
                    .row_rects
                    .iter()
                    .find(|(r, _)| hits(*r, m.column, m.row))
                    .map(|(_, idx)| *idx);
                if let Some(idx) = hit {
                    if self.nav.selected == idx {
                        // Second click on the selected card: its context menu,
                        // matching the session tabs (right-click is kept by
                        // some terminals, e.g. iTerm2). ⏎ still steps in.
                        self.open_context(idx, m.column, m.row, s, fx);
                    } else {
                        self.nav.selected = idx;
                        self.sync(s);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                self.nav.handle(Action::Down, rows_of(s).len(), Wrap::Clamp);
                self.sync(s);
            }
            MouseEventKind::ScrollUp => {
                self.nav.handle(Action::Up, rows_of(s).len(), Wrap::Clamp);
                self.sync(s);
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, s: &mut Shared) {
        self.row_rects.clear();
        let all = rows_of(s);
        self.nav.clamp(all.len());

        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(DIM));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Card stream: section headers (1 row) + one rounded card per bird or
        // room (CARD_H rows), each bird's workers as 1-row lines beneath its
        // card. A scroll offset in rows keeps the SELECTED item fully
        // visible; partially-clipped items are hidden, not mangled.
        const CARD_H: u16 = 4;
        enum Item {
            Header(&'static str, u16),
            Card(usize),
            Worker(usize),
        }
        let mut items: Vec<Item> = vec![Item::Header("BIRDS", 1)];
        for (idx, row) in all.iter().enumerate() {
            match row {
                Row::Room(0) => {
                    items.push(Item::Header("ROOMS", 2));
                    items.push(Item::Card(idx));
                }
                Row::Worker { .. } => items.push(Item::Worker(idx)),
                _ => items.push(Item::Card(idx)),
            }
        }

        let avail = inner.height.saturating_sub(1); // footer keeps the last row
        let item_h = |item: &Item| match item {
            Item::Header(_, h) => *h,
            Item::Card(_) => CARD_H,
            Item::Worker(_) => 1,
        };

        // Selected card's row span, for scroll-follow.
        let (mut sel_start, mut sel_end) = (0u16, 0u16);
        let mut cursor = 0u16;
        for item in &items {
            let h = item_h(item);
            if matches!(item, Item::Card(idx) | Item::Worker(idx) if *idx == self.nav.selected) {
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
                if matches!(item, Item::Card(_) | Item::Worker(_)) {
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
                Item::Worker(idx) => {
                    let selected = *idx == self.nav.selected;
                    self.row_rects.push((rect, *idx));
                    let Row::Worker { name, .. } = &all[*idx] else { continue };
                    let Some(w) = s.agents.workers.get(name) else { continue };
                    let mut line = vec![
                        Span::styled(
                            "   └ ",
                            Style::default().fg(if selected { ACCENT } else { DIM }),
                        ),
                        Span::styled(
                            format!("{:<11}", truncate(&w.name.label(), 11)),
                            if selected {
                                Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(MUTED)
                            },
                        ),
                        Span::raw(" "),
                        status_span(s.agents.workers.status(name)),
                    ];
                    if s.agents.workers.is_unread(name) {
                        line.push(Span::styled(" ●", Style::default().fg(ACCENT)));
                    }
                    frame.render_widget(Paragraph::new(Line::from(line)), rect);
                }
                Item::Card(idx) => {
                    let selected = *idx == self.nav.selected;
                    self.row_rects.push((rect, *idx));
                    let (title, body) = match &all[*idx] {
                        Row::Worker { .. } => continue,
                        Row::Bot(i) => {
                            let i = *i;
                            let bot = &s.config.bots[i];
                            let colour = bird_color(i);
                            let (status, status_tab) = s.agents.status_tabbed(&bot.id);
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
                                status_span(status),
                            ];
                            // Multi-tab birds say WHICH tab the chip reports.
                            if let Some(tab) = status_tab {
                                title.push(dim(format!("·{tab}")));
                            }
                            if s.agents.is_unread(&bot.id) {
                                // Grok-Bot unread dot: news since you last looked.
                                title.push(Span::styled(" ●", Style::default().fg(ACCENT)));
                            }

                            // Body: collab tag, then last prompt or branch.
                            let collab = s.agents.collab(&bot.id).map(|c| c.tag());
                            let tag_w = collab.as_ref().map_or(0, |t| t.chars().count() + 1);
                            let preview = s
                                .agents
                                .get(&crate::config::SessionKey::primary(bot.id.clone()))
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
                            let room = &s.config.rooms[*i];
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

        // Keys named in prose come from the table, like the hints bar does.
        let hatch_key = label_for(&[GLOBAL], Action::NewBot).unwrap_or_default();
        let room_key = label_for(&[GLOBAL], Action::NewRoom).unwrap_or_default();
        if s.config.bots.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(dim(format!(
                    " no birds yet — {hatch_key} hatches one"
                )))),
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
                dim(format!("  {hatch_key} · {room_key}")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::SessionInfo;
    use crate::config::{test_flock, BotId};

    fn worker_row(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.into(),
            status: "idle".into(),
            id: "abcd1234".into(),
            kind: "background".into(),
            ..Default::default()
        }
    }

    #[test]
    fn rows_nest_workers_under_their_bird_then_rooms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_flock(tmp.path(), &["swift", "raven"]);
        cfg.add_room("launch", vec![BotId("swift".into()), BotId("raven".into())])
            .unwrap();
        let mut workers = Workers::default();
        workers.apply_poll(
            &cfg.bots,
            &[
                worker_row("aviary-swift_b-review"),
                worker_row("aviary-raven_x-impl"),
                worker_row("aviary-swift_a-impl"),
                worker_row("aviary-heron_y-impl"), // no such bird: nowhere
            ],
        );
        let all = rows(&cfg, &workers);
        assert_eq!(
            all,
            vec![
                Row::Bot(0),
                Row::Worker { bot: 0, name: "aviary-swift_a-impl".into() },
                Row::Worker { bot: 0, name: "aviary-swift_b-review".into() },
                Row::Bot(1),
                Row::Worker { bot: 1, name: "aviary-raven_x-impl".into() },
                Row::Room(0),
            ]
        );
        // Birds without workers render exactly as before: bots, then rooms.
        let bare = rows(&cfg, &Workers::default());
        assert_eq!(bare, vec![Row::Bot(0), Row::Bot(1), Row::Room(0)]);
    }

    #[test]
    fn room_row_finds_rooms_after_worker_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_flock(tmp.path(), &["swift", "raven"]);
        let launch = cfg
            .add_room("launch", vec![BotId("swift".into()), BotId("raven".into())])
            .unwrap();
        let standup = cfg
            .add_room("standup", vec![BotId("swift".into()), BotId("raven".into())])
            .unwrap();
        let mut workers = Workers::default();
        workers.apply_poll(&cfg.bots, &[worker_row("aviary-swift_a-impl")]);
        let all = rows(&cfg, &workers);
        assert_eq!(room_row(&all, &cfg, &launch), Some(3));
        assert_eq!(room_row(&all, &cfg, &standup), Some(4));
        assert_eq!(room_row(&all, &cfg, "nope"), None);
    }
}
