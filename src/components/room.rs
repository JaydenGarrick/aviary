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
    /// Lettered quick-reply options parsed from the last bot message.
    options: Vec<String>,
    option_rects: Vec<(Rect, usize)>,
    /// Lines up from the bottom; 0 = follow new messages.
    scroll_up: usize,
    composing: Option<String>,
    /// The composer/hint line — clickable to start writing.
    footer: Rect,
}

/// `A) …` / `A. …` lines at the end of the last BOT message become one-key
/// quick replies — the Grok Bot lettered-options pattern.
fn detect_options(entries: &[Entry]) -> Vec<String> {
    let Some(last) = entries.last() else {
        return Vec::new();
    };
    if last.author == USER_AUTHOR {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in last.body.lines() {
        let t = line.trim();
        let mut chars = t.chars();
        if let (Some(letter @ 'A'..='E'), Some(')' | '.'), Some(' ')) =
            (chars.next(), chars.next(), chars.next())
        {
            let expected = (b'A' + out.len() as u8) as char;
            if letter == expected {
                out.push(t[3..].trim().to_string());
            }
        }
    }
    if out.len() < 2 {
        Vec::new() // a single stray "A) …" is prose, not a menu
    } else {
        out.truncate(5);
        out
    }
}

impl RoomView {
    /// The sidebar's ⏎ on a room lands here: open the composer.
    pub fn start_compose(&mut self) {
        self.composing = Some(String::new());
        self.scroll_up = 0;
    }

    fn compose_with(&mut self, text: String) {
        self.composing = Some(text);
        self.scroll_up = 0;
    }

    fn reload(&mut self, s: &Shared) {
        if let Some(room) = s.current_room.as_deref().and_then(|id| s.config.room(id)) {
            self.entries = room::read(&room.transcript_path(&s.config.dir));
            self.options = detect_options(&self.entries);
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
        let anchor = match room::append(&path, USER_AUTHOR, &text) {
            Ok(header) => header,
            Err(e) => {
                fx.flash(format!("could not write the room: {e:#}"));
                return;
            }
        };
        s.watcher.note_local_append(&room.id);

        let targets = room::dispatch_targets(&room, USER_AUTHOR, &text);
        let count = targets.len();
        for target in targets {
            let prompt = room::notify_prompt(&room, USER_AUTHOR, &s.config.dir, &anchor);
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
            Action::Quick(n) => {
                if let Some(text) = self.options.get((n as usize).saturating_sub(1)) {
                    self.compose_with(text.clone());
                }
            }
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
            MouseEventKind::Down(_) => {
                if let Some((_, idx)) = self
                    .option_rects
                    .iter()
                    .find(|(r, _)| hits(*r, m.column, m.row))
                {
                    if let Some(text) = self.options.get(*idx) {
                        self.compose_with(text.clone());
                    }
                    return;
                }
                if hits(self.footer, m.column, m.row) && self.composing.is_none() {
                    self.start_compose();
                }
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
            // Markdown-ish: fenced code dims verbatim, blockquotes get a bar.
            let mut in_code = false;
            for para in e.body.lines() {
                let t = para.trim_end();
                if t.trim_start().starts_with("```") {
                    in_code = !in_code;
                    body.push(Line::from(dim(format!("    {}", t.trim_start()))));
                    continue;
                }
                if t.trim().is_empty() {
                    body.push(Line::from(""));
                    continue;
                }
                if in_code {
                    body.push(Line::from(Span::styled(
                        format!("      {t}"),
                        Style::default().fg(DIM),
                    )));
                    continue;
                }
                if let Some(q) = t.trim_start().strip_prefix("> ") {
                    for chunk in wrap(q, width.saturating_sub(2)) {
                        body.push(Line::from(vec![
                            dim("    ▏ "),
                            Span::styled(chunk, Style::default().fg(DIM)),
                        ]));
                    }
                    continue;
                }
                for chunk in wrap(t, width) {
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

        // Layout: header · transcript window · quick-reply chips · a REAL
        // bordered composer box (the single gray line was too easy to miss).
        let header_h = 1u16;
        let input_h = 3u16;
        let show_options = !self.options.is_empty() && self.composing.is_none();
        let options_h = if show_options { 1u16 } else { 0 };
        let view_h =
            area.height.saturating_sub(header_h + input_h + options_h + 1) as usize;
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

        // Quick-reply chips: click or 1-5 prefills the composer.
        self.option_rects.clear();
        if show_options {
            use unicode_width::UnicodeWidthStr;
            let y = area.y + area.height.saturating_sub(input_h + 1);
            let mut spans: Vec<Span> = vec![Span::raw(" ")];
            let mut cursor = 1u16;
            for (i, opt) in self.options.iter().enumerate() {
                let label = format!(" {} {} ", i + 1, truncate_opt(opt, 18));
                let w = label.width() as u16;
                self.option_rects.push((
                    Rect { x: area.x + cursor, y, width: w, height: 1 },
                    i,
                ));
                spans.push(Span::styled(
                    label,
                    Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED),
                ));
                spans.push(Span::raw(" "));
                cursor += w + 1;
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect { x: area.x, y, width: area.width, height: 1 },
            );
        }

        // The composer box — looks like an input because it IS the input.
        let input_rect = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(input_h),
            width: area.width,
            height: input_h,
        };
        self.footer = input_rect;

        let composing = self.composing.is_some();
        let border = if composing { ACCENT } else { DIM };
        let hint = if composing {
            " ⏎ send · esc cancel "
        } else {
            " ⏎ or click to write "
        };
        let mut block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(Style::default().fg(border))
            .title_bottom(Line::from(Span::styled(
                hint,
                Style::default().fg(border),
            )));
        if self.scroll_up > 0 {
            block = block.title(Line::from(Span::styled(
                format!(" ↓ {} below — G follows ", self.scroll_up),
                Style::default().fg(ACCENT),
            )));
        }
        let content = match &self.composing {
            Some(buf) => Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    // Long drafts keep their TAIL visible — that's where the
                    // caret lives while typing.
                    format!("{}▌", tail(buf, area.width.saturating_sub(5) as usize)),
                    Style::default()
                        .fg(ratatui::style::Color::Reset)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            None => Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("Message #{}", room.name),
                    Style::default().fg(MUTED),
                ),
                dim("  — no @mention wakes everyone · @name wakes just that bird"),
            ]),
        };
        frame.render_widget(ratatui::widgets::Paragraph::new(content).block(block), input_rect);
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

fn truncate_opt(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max - 1).collect();
        format!("{kept}…")
    }
}

fn tail(s: &str, max: usize) -> String {
    let max = max.max(8);
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let kept: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(author: &str, body: &str) -> Entry {
        Entry {
            author: author.into(),
            when: String::new(),
            body: body.into(),
        }
    }

    #[test]
    fn lettered_options_parse_in_order_from_the_last_bot_message() {
        let entries = vec![entry(
            "swift",
            "Pick one:\nA) This works\nB. Vegetarian\nC) No seafood",
        )];
        let opts = detect_options(&entries);
        assert_eq!(opts, vec!["This works", "Vegetarian", "No seafood"]);
    }

    #[test]
    fn user_messages_and_single_strays_offer_no_options() {
        assert!(detect_options(&[entry(USER_AUTHOR, "A) hello\nB) world")]).is_empty());
        assert!(detect_options(&[entry("swift", "A) just one")]).is_empty());
        // Out-of-order letters are prose, not a menu.
        assert!(detect_options(&[entry("swift", "B) two\nA) one")]).is_empty());
    }
}
