//! The roster — birds first, rooms below, Grok-Bot-shaped: each row is a
//! contact with a live status chip and its last activity.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::action::{Action, Effects, Msg};
use crate::command::{Command, CommandResult};
use crate::components::{hits, Component};
use crate::config::Config;
use crate::keymap::{bind, bind_alias, ch, key, Binding, KeyCode};
use crate::shared::Shared;
use crate::ui::{bird_color, dim, muted, status_span, ACCENT, MUTED};
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
    /// Where each logical row was drawn, for clicks.
    row_rects: Vec<(Rect, usize)>,
    /// Branch refresh cadence: every 15th tick.
    ticks: u32,
}

static KEYMAP: &[Binding] = &[
    bind_alias(
        ch('j'),
        &[key(KeyCode::Down)],
        Action::Down,
        Some("move"),
        "move down",
    ),
    bind_alias(ch('k'), &[key(KeyCode::Up)], Action::Up, None, "move up"),
    bind(ch('g'), Action::Top, None, "jump to the top"),
    bind(ch('G'), Action::Bottom, None, "jump to the bottom"),
    bind(
        key(KeyCode::Enter),
        Action::Confirm,
        Some("open"),
        "open the selected bird's thread / room",
    ),
    bind(ch('a'), Action::FocusAgent, Some("talk"), "open + take the keyboard"),
    bind(ch('@'), Action::Handoff, Some("message"), "compose a message to a bird"),
    bind(ch('x'), Action::StopBot, None, "stop the selected bird's session"),
];

impl Roster {
    fn open_selected(&self, s: &mut Shared, fx: &mut Effects, focus: bool) {
        let all = rows(&s.config);
        match all.get(self.nav.selected) {
            Some(Row::Bot(i)) => {
                let id = s.config.bots[*i].id.clone();
                fx.msg(Msg::OpenThread(id));
                if focus {
                    s.agent_focused = true;
                }
            }
            Some(Row::Room(i)) => {
                let id = s.config.rooms[*i].id.clone();
                fx.msg(Msg::OpenRoom(id));
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
    fn keymap(&self) -> &'static [Binding] {
        KEYMAP
    }

    fn update(&mut self, a: Action, s: &mut Shared, fx: &mut Effects) {
        let len = rows(&s.config).len();
        if self.nav.handle(a, len, Wrap::Clamp) {
            return;
        }
        match a {
            Action::Confirm => self.open_selected(s, fx, false),
            Action::FocusAgent => self.open_selected(s, fx, true),
            Action::StopBot => {
                if let Some(Row::Bot(i)) = rows(&s.config).get(self.nav.selected) {
                    let bot = &s.config.bots[*i];
                    let (id, name) = (bot.id.clone(), bot.name.clone());
                    s.agents.stop(&id);
                    fx.flash(format!("{name} stopped — its session resumes by name"));
                }
            }
            Action::Handoff => {
                let preselect = match rows(&s.config).get(self.nav.selected) {
                    Some(Row::Bot(i)) => Some(s.config.bots[*i].id.clone()),
                    _ => None,
                };
                fx.msg(Msg::Compose {
                    source: None,
                    preselect,
                });
            }
            Action::Reload => self.reload_branches(s, fx),
            _ => {}
        }
    }

    fn on_enter(&mut self, s: &mut Shared, fx: &mut Effects) {
        self.nav.clamp(rows(&s.config).len());
        self.reload_branches(s, fx);
    }

    fn on_tick(&mut self, s: &mut Shared, fx: &mut Effects) {
        self.ticks += 1;
        if self.ticks % 15 == 0 && !s.branches.in_flight {
            self.reload_branches(s, fx);
        }
    }

    fn on_result(&mut self, r: &CommandResult, s: &mut Shared) {
        let CommandResult::Branches { gen, info } = r;
        // Clone is fine: a handful of short strings, every 15 seconds.
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
                if let Some((_, idx)) = self
                    .row_rects
                    .iter()
                    .find(|(r, _)| hits(*r, m.column, m.row))
                {
                    if self.nav.selected == *idx {
                        self.open_selected(s, fx, false); // second click opens
                    } else {
                        self.nav.selected = *idx;
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                self.nav.handle(Action::Down, rows(&s.config).len(), Wrap::Clamp);
            }
            MouseEventKind::ScrollUp => {
                self.nav.handle(Action::Up, rows(&s.config).len(), Wrap::Clamp);
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, s: &mut Shared) {
        self.row_rects.clear();
        let all = rows(&s.config);
        self.nav.clamp(all.len());

        let mut lines: Vec<Line> = vec![Line::from("")];
        let mut line_rows: Vec<Option<usize>> = vec![None];

        lines.push(Line::from(muted("  BIRDS")));
        line_rows.push(None);

        for (idx, row) in all.iter().enumerate() {
            let selected = idx == self.nav.selected;
            let marker = if selected { "▸" } else { " " };
            match row {
                Row::Bot(i) => {
                    let bot = &s.config.bots[*i];
                    let status = s.agents.status(&bot.id);
                    let colour = bird_color(*i);

                    let mut spans = vec![
                        Span::styled(format!("  {marker} "), Style::default().fg(ACCENT)),
                        Span::raw(format!("{} ", bot.glyph)),
                        Span::styled(
                            format!("{:<10}", bot.name),
                            if selected {
                                Style::default().fg(colour).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(colour)
                            },
                        ),
                        muted(format!("{:<12}", repo_tail(&bot.repo))),
                        status_span(status),
                    ];
                    if let Some(info) = s
                        .branches
                        .data
                        .as_ref()
                        .and_then(|v| v.iter().find(|b| b.bot == bot.id))
                    {
                        spans.push(dim(format!(
                            "   ⎇ {}{}",
                            info.branch,
                            if info.dirty { " ●" } else { "" }
                        )));
                    }
                    lines.push(Line::from(spans));
                    line_rows.push(Some(idx));

                    // The activity line: the last thing aviary asked of it.
                    let detail = s
                        .agents
                        .get(&bot.id)
                        .and_then(|sess| sess.last_prompt.clone())
                        .map(|p| truncate(&p, area.width.saturating_sub(16) as usize));
                    if let Some(d) = detail {
                        lines.push(Line::from(dim(format!("        “{d}”"))));
                        line_rows.push(Some(idx));
                    }
                }
                Row::Room(i) => {
                    // Section header before the first room.
                    if *i == 0 {
                        lines.push(Line::from(""));
                        line_rows.push(None);
                        lines.push(Line::from(muted("  ROOMS")));
                        line_rows.push(None);
                    }
                    let room = &s.config.rooms[*i];
                    let members: Vec<String> =
                        room.members.iter().map(|m| format!("@{m}")).collect();
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {marker} "), Style::default().fg(ACCENT)),
                        Span::styled(
                            format!("# {:<12}", room.name),
                            if selected {
                                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(MUTED)
                            },
                        ),
                        dim(members.join(" · ")),
                    ]));
                    line_rows.push(Some(idx));
                }
            }
        }

        if s.config.bots.is_empty() {
            lines.push(Line::from(dim("  no birds yet — n adds one")));
            line_rows.push(None);
        }
        lines.push(Line::from(""));
        line_rows.push(None);
        lines.push(Line::from(dim(
            "  each bird is a claude session living in its own repo, with its own",
        )));
        line_rows.push(None);
        lines.push(Line::from(dim(
            "  context and character — they hand work to each other by name",
        )));
        line_rows.push(None);

        // Record hit rects line-by-line (the list never scrolls in v1: a
        // roster outgrowing a terminal means too many birds, not a scrollbar).
        for (offset, maybe_idx) in line_rows.iter().enumerate() {
            if let Some(idx) = maybe_idx {
                let y = area.y + offset as u16;
                if y < area.y + area.height {
                    self.row_rects.push((
                        Rect {
                            x: area.x,
                            y,
                            width: area.width,
                            height: 1,
                        },
                        *idx,
                    ));
                }
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }
}

fn repo_tail(repo: &str) -> String {
    repo.rsplit('/').next().unwrap_or(repo).to_string()
}

fn truncate(s: &str, max: usize) -> String {
    let max = max.max(8);
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max - 1).collect();
        format!("{kept}…")
    }
}
