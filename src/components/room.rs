//! The room view — a rendered group-chat transcript with an inline composer.
//!
//! The transcript file is the source of truth; this component re-reads it on
//! tick, renders it through `markdown` (cached until the text or width
//! changes), and draws it bottom-anchored, like a chat. Dispatch of NEW
//! appends is the shell's room watcher — this view only reads, composes,
//! sends, and copies.
//!
//! Copying: a drag over the transcript paints a selection in CONTENT
//! coordinates (body line, display column) and copies it on release — a live
//! append shifts rows on screen, never the lines under the pointer. `y`
//! copies the last message's raw markdown instead.

use crossterm::event::{KeyCode as CKey, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::action::{Action, Effects};
use crate::agent_store::Collab;
use crate::clipboard;
use crate::components::{hits, Component};
use crate::config::{BotId, Room};
use crate::keymap::{label_for, GLOBAL};
use crate::markdown;
use crate::room::{self, Entry};
use crate::shared::Shared;
use crate::ui::{bird_color, dim, status_span, ACCENT, DIM, MUTED};

/// Indent under every author header; stripped again on copy.
const GUTTER: &str = "    ";

#[derive(Default)]
pub struct RoomView {
    entries: Vec<Entry>,
    /// Lettered quick-reply options parsed from the last bot message.
    options: Vec<String>,
    option_rects: Vec<(Rect, usize)>,
    /// Lines up from the bottom; 0 = follow new messages.
    scroll_up: usize,
    composing: Option<String>,
    /// The composer box — clickable to start writing.
    footer: Rect,
    /// The rendered transcript, rebuilt only when its key changes.
    cache: Option<Rendered>,
    /// The transcript viewport, and the body line drawn at its first row.
    view: Rect,
    first_visible: usize,
    /// Where Down landed, content coords (line, col) — a click until a Drag
    /// converts it into a selection.
    press: Option<(usize, usize)>,
    /// anchor → cursor, content coords. Some = a selection is live.
    sel: Option<((usize, usize), (usize, usize))>,
}

struct Rendered {
    /// room id · width · entry count · last body length · roster size.
    key: (String, u16, usize, usize, usize),
    lines: Vec<Line<'static>>,
    /// Plain text per line — what a drag copies.
    text: Vec<String>,
}

/// `A) …` / `A. …` lines at the end of the last BOT message become one-key
/// quick replies — the Grok Bot lettered-options pattern. `user_name` is
/// the human's author name: their own messages never offer options.
fn detect_options(entries: &[Entry], user_name: &str) -> Vec<String> {
    let Some(last) = entries.last() else {
        return Vec::new();
    };
    if last.author == user_name {
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

    /// A selection is painted (the hints bar says so).
    pub fn selecting(&self) -> bool {
        self.sel.is_some()
    }

    fn compose_with(&mut self, text: String) {
        self.composing = Some(text);
        self.scroll_up = 0;
    }

    fn reload(&mut self, s: &Shared) {
        if let Some(room) = s.current_room.as_deref().and_then(|id| s.config.room(id)) {
            self.entries = room::read(&room.transcript_path(&s.config.dir));
            self.options = detect_options(&self.entries, &s.config.user_name);
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
        let anchor = match room::append(&path, &s.config.user_name, &text) {
            Ok(header) => header,
            Err(e) => {
                fx.flash(format!("could not write the room: {e:#}"));
                return;
            }
        };
        s.watcher.note_local_append(&room.id);

        let targets = room::dispatch_targets(&room, &s.config.user_name, &text);
        let count = targets.len();
        for target in targets {
            let prompt = room::notify_prompt(&room, &s.config.user_name, &s.config.dir, &anchor);
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

    /// Rebuild the rendered transcript when the text, width, or roster
    /// (colours) changed — otherwise the cache serves every frame.
    fn ensure_rendered(&mut self, s: &Shared, room: &Room, width: u16) {
        let key = (
            room.id.clone(),
            width,
            self.entries.len(),
            self.entries.last().map(|e| e.body.len()).unwrap_or(0),
            s.config.bots.len(),
        );
        if self.cache.as_ref().is_some_and(|c| c.key == key) {
            return;
        }
        let lines = render_transcript(&self.entries, s, width);
        let text = lines
            .iter()
            .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect::<String>())
            .collect();
        self.cache = Some(Rendered { key, lines, text });
    }

    fn lines_len(&self) -> usize {
        self.cache.as_ref().map(|c| c.lines.len()).unwrap_or(0)
    }

    /// Screen cell → content coords, clamped to the transcript.
    fn cell(&self, m: &MouseEvent) -> (usize, usize) {
        let row = m.row.saturating_sub(self.view.y) as usize;
        let line = (self.first_visible + row).min(self.lines_len().saturating_sub(1));
        let col = m
            .column
            .saturating_sub(self.view.x)
            .min(self.view.width.saturating_sub(1)) as usize;
        (line, col)
    }

    fn copy_selection(&mut self, fx: &mut Effects) {
        let Some((a, b)) = self.sel.take() else { return };
        let text = self
            .cache
            .as_ref()
            .map(|c| selection::extract(&c.text, a, b, GUTTER.len()))
            .unwrap_or_default();
        let n = text.lines().count();
        fx.flash(if n == 0 {
            "nothing in the selection".to_string()
        } else if !clipboard::copy(&text) {
            "pbcopy failed".to_string()
        } else {
            format!("copied {n} line{}", if n == 1 { "" } else { "s" })
        });
    }

    /// `y`: the last message, raw markdown — tables and links intact.
    fn yank_last(&self, fx: &mut Effects) {
        match self.entries.last() {
            Some(e) if !e.body.trim().is_empty() => {
                fx.flash(if clipboard::copy(&e.body) {
                    format!("copied @{}'s message as markdown", e.author)
                } else {
                    "pbcopy failed".to_string()
                });
            }
            _ => fx.flash("nothing to copy — the room is empty"),
        }
    }
}

/// Author blocks → lines: a coloured header, the body through the markdown
/// renderer behind the gutter, a blank line. The zero state when empty.
fn render_transcript(entries: &[Entry], s: &Shared, width: u16) -> Vec<Line<'static>> {
    let theme = markdown::Theme::aviary();
    let me = s.config.user_name.as_str();
    // Mentions arrive lowercased; the human's name keeps its case.
    let mention = |id: &str| -> Option<Color> {
        if id.eq_ignore_ascii_case(me) {
            return Some(ACCENT);
        }
        s.config
            .bots
            .iter()
            .position(|b| b.id.0 == id)
            .map(bird_color)
    };
    let body_width = width.saturating_sub(GUTTER.len() as u16 + 1) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    for e in entries {
        let colour = if e.author == me {
            ACCENT
        } else {
            colour_for(s, &BotId(e.author.clone()))
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  @{}", e.author),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            dim(format!("  {}", e.when)),
        ]));
        for md in markdown::render(&e.body, body_width, &theme, &mention) {
            let mut spans = vec![Span::raw(GUTTER)];
            spans.extend(md.spans);
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
    }
    if entries.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(dim("  nothing yet — ⏎ writes the first message")));
        lines.push(Line::from(dim(
            "  birds reply only when @-mentioned or when they have something material",
        )));
    }
    lines
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

    fn update(&mut self, a: Action, s: &mut Shared, fx: &mut Effects) {
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
            Action::Yank => self.yank_last(fx),
            _ => {}
        }
    }

    fn on_enter(&mut self, s: &mut Shared, _fx: &mut Effects) {
        self.scroll_up = 0;
        self.composing = None;
        self.press = None;
        self.sel = None;
        self.cache = None;
        self.reload(s);
    }

    fn on_tick(&mut self, s: &mut Shared, _fx: &mut Effects) {
        self.reload(s);
    }

    fn handle_mouse(&mut self, m: MouseEvent, _s: &mut Shared, fx: &mut Effects) {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_up = self.scroll_up.saturating_add(3),
            MouseEventKind::ScrollDown => self.scroll_up = self.scroll_up.saturating_sub(3),
            MouseEventKind::Down(button) => {
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
                if hits(self.footer, m.column, m.row) {
                    if self.composing.is_none() {
                        self.start_compose();
                    }
                    return;
                }
                if button == MouseButton::Left && hits(self.view, m.column, m.row) {
                    // Arm: a click until a Drag turns it into a selection.
                    self.press = Some(self.cell(&m));
                    self.sel = None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(anchor) = self.sel.map(|(a, _)| a).or(self.press) {
                    self.sel = Some((anchor, self.cell(&m)));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Copy-on-release: ⌘C never reaches the app under mouse
                // capture, and a later keypress would race new appends.
                if self.sel.is_some() {
                    self.copy_selection(fx);
                }
                self.press = None; // a plain click on the transcript does nothing
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

        // Body: the cached render of the transcript.
        self.ensure_rendered(s, &room, area.width);
        let total = self.lines_len();

        // Layout: header · transcript window · quick-reply chips · a REAL
        // bordered composer box (the single gray line was too easy to miss).
        let header_h = 1u16;
        let input_h = 3u16;
        let show_options = !self.options.is_empty() && self.composing.is_none();
        let options_h = if show_options { 1u16 } else { 0 };
        let view_h = area
            .height
            .saturating_sub(header_h + input_h + options_h + 1) as usize;
        let max_up = total.saturating_sub(view_h);
        self.scroll_up = self.scroll_up.min(max_up);
        let end = total - self.scroll_up.min(total);
        let start = end.saturating_sub(view_h);
        self.view = Rect {
            x: area.x,
            y: area.y + header_h + 1,
            width: area.width,
            height: view_h as u16,
        };
        self.first_visible = start;

        frame.render_widget(
            Paragraph::new(Line::from(header)),
            Rect { height: header_h, ..area },
        );
        if let Some(cache) = &self.cache {
            frame.render_widget(Paragraph::new(cache.lines[start..end].to_vec()), self.view);
        }

        // Selection highlight: a post-pass over the cells just drawn.
        if let Some((a, b)) = self.sel.map(|(a, b)| selection::range(a, b)) {
            let view = self.view;
            let last_col = view.width.saturating_sub(1) as usize;
            let buf = frame.buffer_mut();
            for r in 0..view.height {
                let line = start + r as usize;
                if line < a.0 || line > b.0 {
                    continue;
                }
                let from = if line == a.0 { a.1 } else { 0 };
                let to = if line == b.0 { b.1 } else { last_col };
                for c in from..=to.min(last_col) {
                    if let Some(cell) = buf.cell_mut((view.x + c as u16, view.y + r)) {
                        cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                    }
                }
            }
        }

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
            " ⏎ send · esc cancel · no @mention wakes everyone · @name wakes just that bird "
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
                        .fg(Color::Reset)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            None => {
                // Keys named from the tables, so the prose can't drift.
                let yank = label_for(&[crate::components::roster::KEYMAP], Action::Yank)
                    .unwrap_or_default();
                let mouse = label_for(&[GLOBAL], Action::ToggleMouse).unwrap_or_default();
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        format!("Message #{}", room.name),
                        Style::default().fg(MUTED),
                    ),
                    dim(format!(
                        "  — drag copies · {yank} copies the last message · {mouse} frees the mouse"
                    )),
                ])
            }
        };
        frame.render_widget(ratatui::widgets::Paragraph::new(content).block(block), input_rect);
    }
}

fn colour_for(s: &Shared, id: &BotId) -> Color {
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

/// Pure selection math over rendered lines — content coords are (line
/// index, display column); the cursor cell is included.
mod selection {
    use unicode_width::UnicodeWidthChar;

    type Cell = (usize, usize);

    /// Order the two ends (a drag can run up-left).
    pub fn range(a: Cell, b: Cell) -> (Cell, Cell) {
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// The glyphs whose cells intersect display columns `from..=to`.
    pub fn slice_cols(text: &str, from: usize, to: usize) -> String {
        let mut out = String::new();
        let mut col = 0usize;
        for c in text.chars() {
            let w = c.width().unwrap_or(0).max(1);
            if col + w > from && col <= to {
                out.push(c);
            }
            col += w;
            if col > to {
                break;
            }
        }
        out
    }

    /// Linear (row-major) extraction: first line from `a.1`, middle lines
    /// whole, last line to `b.1`. Line ends are trimmed and up to `gutter`
    /// leading spaces come off each line, so pasted text starts flush while
    /// nested indents keep their shape.
    pub fn extract(lines: &[String], a: Cell, b: Cell, gutter: usize) -> String {
        let (a, b) = range(a, b);
        let mut out: Vec<String> = Vec::new();
        for (i, text) in lines.iter().enumerate().take(b.0 + 1).skip(a.0) {
            let from = if i == a.0 { a.1 } else { 0 };
            let to = if i == b.0 { b.1 } else { usize::MAX };
            let piece = slice_cols(text, from, to);
            let piece = piece.trim_end();
            let lead = piece.len() - piece.trim_start_matches(' ').len();
            out.push(piece[lead.min(gutter)..].to_string());
        }
        out.join("\n")
    }
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
        let opts = detect_options(&entries, "jayden");
        assert_eq!(opts, vec!["This works", "Vegetarian", "No seafood"]);
    }

    #[test]
    fn user_messages_and_single_strays_offer_no_options() {
        assert!(detect_options(&[entry("jayden", "A) hello\nB) world")], "jayden").is_empty());
        assert!(detect_options(&[entry("swift", "A) just one")], "jayden").is_empty());
        // Out-of-order letters are prose, not a menu.
        assert!(detect_options(&[entry("swift", "B) two\nA) one")], "jayden").is_empty());
    }

    #[test]
    fn selection_range_orders_a_drag_that_ran_backwards() {
        assert_eq!(selection::range((5, 2), (3, 9)), ((3, 9), (5, 2)));
        assert_eq!(selection::range((3, 9), (3, 2)), ((3, 2), (3, 9)));
        assert_eq!(selection::range((1, 1), (1, 1)), ((1, 1), (1, 1)));
    }

    #[test]
    fn slice_cols_counts_wide_glyphs_as_two_cells() {
        // "🟢" occupies cols 0-1, "a" col 2, "b" col 3.
        assert_eq!(selection::slice_cols("🟢ab", 1, 2), "🟢a");
        assert_eq!(selection::slice_cols("🟢ab", 2, 3), "ab");
        assert_eq!(selection::slice_cols("abc", 1, 1), "b");
        assert_eq!(selection::slice_cols("abc", 5, 9), "");
    }

    #[test]
    fn extract_is_linear_dedents_the_gutter_and_trims_line_ends() {
        let lines: Vec<String> = vec![
            "  @raven  11:16".into(),
            "    the one list, merged   ".into(),
            "    • alpha".into(),
            "      nested".into(),
            "".into(),
        ];
        // Mid-line start on the header, through to col 6 (the "a") of the bullet.
        let got = selection::extract(&lines, (0, 2), (2, 6), 4);
        assert_eq!(got, "@raven  11:16\nthe one list, merged\n• a");
        // Same cell → one glyph; nested indent keeps its extra two spaces.
        assert_eq!(selection::extract(&lines, (3, 6), (3, 6), 4), "n");
        assert_eq!(selection::extract(&lines, (3, 0), (3, 99), 4), "  nested");
        // Backwards drag works the same.
        assert_eq!(selection::extract(&lines, (2, 6), (0, 2), 4), got);
    }
}
