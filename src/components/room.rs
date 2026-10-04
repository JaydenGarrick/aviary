//! The room view — a rendered group-chat transcript with an inline composer.
//!
//! The transcript file is the source of truth; this component re-reads it on
//! tick and renders bottom-anchored, like a chat. Dispatch of NEW appends is
//! the shell's room watcher — this view only reads, composes, and sends.

use crossterm::event::{KeyCode as CKey, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::action::{Action, Effects};
use crate::agent_store::Collab;
use crate::components::{hits, Component};
use crate::config::BotId;
use crate::room::{self, Entry, USER_AUTHOR};
use crate::shared::Shared;
use crate::ui::{bird_color, dim, status_span, wrap, ACCENT, DIM, MUTED};

#[derive(Default)]
pub struct RoomView {
    entries: Vec<Entry>,
    /// Lines up from the bottom; 0 = follow new messages.
    scroll_up: usize,
    composing: Option<String>,
    /// The composer/hint line — clickable to start writing.
    footer: Rect,
}

impl RoomView {
    /// The sidebar's ⏎ on a room lands here: open the composer.
    pub fn start_compose(&mut self) {
        self.composing = Some(String::new());
        self.scroll_up = 0;
    }

    fn reload(&mut self, s: &Shared) {
        if let Some(room) = s.current_room.as_deref().and_then(|id| s.config.room(id)) {
            self.entries = room::read(&room.transcript_path(&s.config.dir));
        }
    }

    /// Append the user's message and wake the birds it should wake.
    fn submit(&mut self, text: String, s: &mut Shared, fx: &mut Effects) {
        let Some(room) = s
            .current_room
            .as_deref()
            .and_then(|id| s.config.room(id))
            .cloned()
        else {
            return;
        };
        let path = room.transcript_path(&s.config.dir);
        if let Err(e) = room::append(&path, USER_AUTHOR, &text) {
            fx.flash(format!("could not write the room: {e:#}"));
            return;
        }
        s.watcher.note_local_append(&room.id);

        let targets = room::dispatch_targets(&room, USER_AUTHOR, &text);
        let count = targets.len();
        for target in targets {
            let prompt = room::notify_prompt(&room, USER_AUTHOR, &s.config.dir);
            s.boot_bot(&target, Some(&prompt));
            s.agents.set_collab(&target, Collab::Room(room.id.clone()));
        }
        fx.flash(match count {
            0 => "sent — no birds to wake".to_string(),
            1 => "sent — woke 1 bird".to_string(),
            n => format!("sent — woke {n} birds"),
        });
        self.reload(s);
        self.scroll_up = 0;
    }
}

impl Component for RoomView {
    fn capturing(&self) -> bool {
        self.composing.is_some()
    }

    fn handle_text(&mut self, k: KeyEvent, s: &mut Shared, fx: &mut Effects) {
        let Some(buf) = self.composing.as_mut() else { return };
        match k.code {
            CKey::Esc => self.composing = None,
            CKey::Enter => {
                let text = std::mem::take(buf).trim().to_string();
                self.composing = None;
                if !text.is_empty() {
                    self.submit(text, s, fx);
                }
            }
            CKey::Backspace => {
                buf.pop();
            }
            CKey::Char(c) => buf.push(c),
            _ => {}
        }
    }

    fn update(&mut self, a: Action, s: &mut Shared, _fx: &mut Effects) {
        match a {
            Action::Compose => self.start_compose(),
            Action::PageUp => self.scroll_up = self.scroll_up.saturating_add(8),
            Action::PageDown => self.scroll_up = self.scroll_up.saturating_sub(8),
            Action::Reload => self.reload(s),
            _ => {}
        }
    }

    fn on_enter(&mut self, s: &mut Shared, _fx: &mut Effects) {
        self.scroll_up = 0;
        self.composing = None;
        self.reload(s);
    }

    fn on_tick(&mut self, s: &mut Shared, _fx: &mut Effects) {
        self.reload(s);
    }

    fn handle_mouse(&mut self, m: MouseEvent, _s: &mut Shared, _fx: &mut Effects) {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_up = self.scroll_up.saturating_add(3),
            MouseEventKind::ScrollDown => self.scroll_up = self.scroll_up.saturating_sub(3),
            MouseEventKind::Down(_)
                if hits(self.footer, m.column, m.row) && self.composing.is_none() =>
            {
                self.start_compose();
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, s: &mut Shared) {
        let Some(room) = s
            .current_room
            .as_deref()
            .and_then(|id| s.config.room(id))
            .cloned()
        else {
            frame.render_widget(Paragraph::new(Line::from(dim("  no room selected"))), area);
            return;
        };

        // Header: room name + member strip with live status glyphs.
        let mut header = vec![
            Span::styled(
                format!(" #{}", room.name),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            dim("   "),
        ];
        for (i, member) in room.members.iter().enumerate() {
            if i > 0 {
                header.push(dim(" · "));
            }
            let colour = colour_for(s, member);
            header.push(Span::styled(format!("@{member} "), Style::default().fg(colour)));
            header.push(status_span(s.agents.status(member)));
        }

        // Body: transcript rendered as wrapped author blocks.
        let width = area.width.saturating_sub(6) as usize;
        let mut body: Vec<Line> = Vec::new();
        for e in &self.entries {
            let colour = if e.author == USER_AUTHOR {
                ACCENT
            } else {
                colour_for(s, &BotId(e.author.clone()))
            };
            body.push(Line::from(vec![
                Span::styled(
                    format!("  @{}", e.author),
                    Style::default().fg(colour).add_modifier(Modifier::BOLD),
                ),
                dim(format!("  {}", e.when)),
            ]));
            for para in e.body.lines() {
                if para.trim().is_empty() {
                    body.push(Line::from(""));
                    continue;
                }
                for chunk in wrap(para, width) {
                    body.push(Line::from(Span::styled(
                        format!("    {chunk}"),
                        Style::default().fg(MUTED),
                    )));
                }
            }
            body.push(Line::from(""));
        }
        if self.entries.is_empty() {
            body.push(Line::from(""));
            body.push(Line::from(dim("  nothing yet — ⏎ writes the first message")));
            body.push(Line::from(dim(
                "  birds reply only when @-mentioned or when they have something material",
            )));
        }

        // Layout: header · transcript window · composer/hint line.
        let header_h = 1u16;
        let footer_h = 1u16;
        let view_h = area.height.saturating_sub(header_h + footer_h + 1) as usize;
        let max_up = body.len().saturating_sub(view_h);
        self.scroll_up = self.scroll_up.min(max_up);
        let end = body.len() - self.scroll_up.min(body.len());
        let start = end.saturating_sub(view_h);

        frame.render_widget(
            Paragraph::new(Line::from(header)),
            Rect { height: header_h, ..area },
        );
        frame.render_widget(
            Paragraph::new(body[start..end].to_vec()),
            Rect {
                x: area.x,
                y: area.y + header_h + 1,
                width: area.width,
                height: view_h as u16,
            },
        );

        let footer_y = area.y + area.height.saturating_sub(1);
        self.footer = Rect {
            x: area.x,
            y: footer_y,
            width: area.width,
            height: 1,
        };
        let footer = match &self.composing {
            Some(buf) => Line::from(vec![
                Span::styled(" > ", Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{buf}▌"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                dim("   ⏎ send · esc cancel · @name wakes just that bird"),
            ]),
            None => {
                let mut spans = vec![dim(" ⏎ or click here to message the room")];
                if self.scroll_up > 0 {
                    spans.push(Span::styled(
                        format!("   ↓ {} below — G follows", self.scroll_up),
                        Style::default().fg(ACCENT),
                    ));
                }
                Line::from(spans)
            }
        };
        frame.render_widget(
            Paragraph::new(footer).style(Style::default().fg(DIM)),
            Rect {
                x: area.x,
                y: footer_y,
                width: area.width,
                height: footer_h,
            },
        );
    }
}

fn colour_for(s: &Shared, id: &BotId) -> ratatui::style::Color {
    s.config
        .bots
        .iter()
        .position(|b| &b.id == id)
        .map(bird_color)
        .unwrap_or(MUTED)
}
