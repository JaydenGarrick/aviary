//! The thread — one bird, full screen. The pane IS the conversation: a real
//! Claude Code session on a PTY, so it behaves exactly as it would in its own
//! terminal window.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::action::{Action, Effects, Msg};
use crate::components::{hits, Component};
use crate::keymap::{bind, ch, key, Binding, KeyCode};
use crate::shared::Shared;
use crate::ui::{bird_color, dim, muted, status_span, ACCENT};
use crate::widgets::agent_pane;

#[derive(Default)]
pub struct Thread {
    pane: Rect,
}

static KEYMAP: &[Binding] = &[
    bind(ch('a'), Action::FocusAgent, Some("talk"), "give the bird the keyboard"),
    bind(
        ch('@'),
        Action::Handoff,
        Some("handoff"),
        "hand work to a teammate (this bird packages the context)",
    ),
    bind(ch('x'), Action::StopBot, None, "stop this bird's session"),
    bind(ch('q'), Action::Back, Some("roster"), "back to the roster"),
    bind(
        key(KeyCode::Enter),
        Action::FocusAgent,
        None,
        "give the bird the keyboard",
    ),
];

impl Component for Thread {
    fn keymap(&self) -> &'static [Binding] {
        KEYMAP
    }

    fn update(&mut self, a: Action, s: &mut Shared, fx: &mut Effects) {
        match a {
            Action::FocusAgent => {
                let Some(id) = s.current_bot.clone() else { return };
                // A dead or never-started session relaunches on focus.
                let prompt = s.opening_prompt(&id);
                s.boot_bot(&id, prompt.as_deref());
                s.agent_focused = true;
            }
            Action::StopBot => {
                if let Some(id) = s.current_bot.clone() {
                    s.agents.stop(&id);
                    s.agent_focused = false;
                    fx.flash("stopped — a or ⏎ starts it again, resuming its conversation");
                }
            }
            Action::Handoff => {
                fx.msg(Msg::Compose {
                    source: s.current_bot.clone(),
                    preselect: None,
                });
            }
            _ => {}
        }
    }

    fn on_enter(&mut self, s: &mut Shared, _fx: &mut Effects) {
        let Some(id) = s.current_bot.clone() else { return };
        let prompt = s.opening_prompt(&id);
        s.boot_bot(&id, prompt.as_deref());
    }

    fn handle_mouse(&mut self, m: MouseEvent, s: &mut Shared, _fx: &mut Effects) {
        let over_pane = hits(self.pane, m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if over_pane => {
                s.agent_focused = true;
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if over_pane => {
                let delta = if m.kind == MouseEventKind::ScrollUp { 3 } else { -3 };
                let (col, row) = (
                    m.column.saturating_sub(self.pane.x + 1),
                    m.row.saturating_sub(self.pane.y + 1),
                );
                if let Some(id) = s.current_bot.clone() {
                    if let Some(session) = s.agents.get_mut(&id) {
                        session.term.wheel(delta, col, row);
                    }
                }
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, s: &mut Shared) {
        self.pane = area;
        let Some(id) = s.current_bot.clone() else {
            frame.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(dim("  no bird selected"))),
                area,
            );
            return;
        };
        let Some(bot) = s.config.bot(&id).cloned() else { return };
        let colour = s
            .config
            .bots
            .iter()
            .position(|b| b.id == id)
            .map(bird_color)
            .unwrap_or(ACCENT);
        let status = s.agents.status(&id);

        let mut title = Line::from(vec![
            Span::raw(format!(" {} ", bot.glyph)),
            Span::styled(
                bot.name.clone(),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            dim("  ·  "),
            muted(bot.repo.clone()),
            dim("  ·  "),
            status_span(status),
        ]);
        if let Some(info) = s
            .branches
            .data
            .as_ref()
            .and_then(|v| v.iter().find(|b| b.bot == id))
        {
            title.push_span(dim(format!("  ·  ⎇ {}", info.branch)));
        }
        title.push_span(Span::raw(" "));

        let empty = vec![
            Line::from(""),
            Line::from(vec![
                muted("  press "),
                Span::styled("a", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                muted(format!(" to wake {} — it lives in {}", bot.name, bot.repo)),
            ]),
            Line::from(""),
            Line::from(dim(format!(
                "  its session is named {} and resumes with its full memory;",
                id.session_name()
            ))),
            Line::from(dim(
                "  teammates reach it by that name with SendMessage",
            )),
        ];

        let focused = s.agent_focused;
        agent_pane::draw(frame, area, title, s.agents.get_mut(&id), focused, empty);
    }
}
