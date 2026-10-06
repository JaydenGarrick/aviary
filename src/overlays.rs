//! Modal forms, owned by the shell. While one is open it captures every key
//! (layer 4); submitting hands a typed result back for the shell to apply.
//! Fully mouse-operable: click a field to focus it, a chip/member to pick it,
//! the hint line to submit, anywhere outside the popup to cancel.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::components::hits;
use crate::config::{Bot, BotId};
use crate::shared::Shared;
use crate::ui::{bird_color, centered, dim, muted, wrap, ACCENT, BAD, DIM};

pub enum Overlay {
    None,
    Help,
    AddBot(AddBotForm),
    NewRoom(NewRoomForm),
    Compose(ComposeForm),
    Profile(ProfileView),
    Confirm(ConfirmForm),
    Context(ContextMenu),
    RenameTab(RenameTabForm),
    TabMenu(TabMenu),
}

impl Overlay {
    pub fn is_none(&self) -> bool {
        matches!(self, Overlay::None)
    }
}

pub enum FormEvent {
    Consumed,
    Cancel,
    AddBot {
        name: String,
        glyph: String,
        repo: String,
    },
    NewRoom {
        name: String,
        members: Vec<BotId>,
    },
    Compose {
        source: Option<BotId>,
        target: BotId,
        text: String,
    },
    /// Profile actions — handled by the shell, overlay stays open.
    ToggleNotify(BotId),
    OpenPersona(BotId),
    /// Profile shortcut for a fresh conversation (closes the overlay).
    FreshStart(BotId),
    /// The confirm dialog said yes: remove this from the roster.
    Delete(crate::action::RosterTarget),
    // -- context-menu picks, executed by the shell
    Talk(BotId),
    Handoff(BotId),
    Profile(BotId),
    StopBird(BotId),
    WriteRoom(String),
    /// Open the release/delete CONFIRM dialog (menus never delete directly).
    AskDelete(crate::action::RosterTarget),
    /// Label a session tab (display only; empty clears).
    RenameTab {
        key: crate::config::SessionKey,
        name: String,
    },
    /// A tab-menu pick: a tab Action for the thread pane to run on the
    /// CURRENT tab (the right-click selected it first).
    Tab(crate::action::Action),
}

const GLYPHS: [&str; 6] = ["🐦", "🦉", "🦅", "🪿", "🐧", "🦜"];

fn field_line(label: &str, value: &str, active: bool, hint: &str) -> Line<'static> {
    let marker = if active { "▸" } else { " " };
    let caret = if active { "▌" } else { "" };
    let value_span = if value.is_empty() {
        if active {
            Span::styled(caret.to_string(), Style::default().fg(ACCENT))
        } else {
            dim(format!("— {hint} —"))
        }
    } else {
        Span::styled(
            format!("{value}{caret}"),
            Style::default().add_modifier(Modifier::BOLD),
        )
    };
    Line::from(vec![
        Span::styled(format!(" {marker} "), Style::default().fg(ACCENT)),
        muted(format!("{label:<10}")),
        value_span,
    ])
}

/// Render the popup and return its rect, so forms can map click targets to the
/// line positions inside it (inner lines start at rect.y + 1).
fn popup(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) -> Rect {
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = centered(66, height, area);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::default().fg(ACCENT),
                )),
        ),
        rect,
    );
    rect
}

fn line_rect(popup: Rect, index: usize) -> Rect {
    let y = popup.y + 1 + index as u16;
    if y >= popup.y + popup.height.saturating_sub(1) {
        return Rect::default();
    }
    Rect {
        x: popup.x + 1,
        y,
        width: popup.width.saturating_sub(2),
        height: 1,
    }
}

fn error_lines(error: &Option<String>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(e) = error {
        out.push(Line::from(""));
        for chunk in wrap(e, 58) {
            out.push(Line::from(Span::styled(
                format!("  {chunk}"),
                Style::default().fg(BAD),
            )));
        }
    }
    out
}

/// Left click only; anything outside the popup cancels.
fn click(m: &MouseEvent, popup: Rect) -> Option<(u16, u16)> {
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if hits(popup, m.column, m.row) {
                Some((m.column, m.row))
            } else {
                None
            }
        }
        _ => Some((u16::MAX, u16::MAX)), // wheel/drag: consumed, no target
    }
}

// -------------------------------------------------------------------- add bot

pub struct AddBotForm {
    field: usize, // 0 name · 1 repo · 2 glyph
    name: String,
    repo: String,
    glyph: String,
    pub error: Option<String>,
    popup: Rect,
    field_rects: [Rect; 3],
    submit: Rect,
}

impl AddBotForm {
    pub fn new(existing: usize) -> AddBotForm {
        AddBotForm {
            field: 0,
            name: String::new(),
            repo: String::new(),
            glyph: GLYPHS[existing % GLYPHS.len()].to_string(),
            error: None,
            popup: Rect::default(),
            field_rects: [Rect::default(); 3],
            submit: Rect::default(),
        }
    }

    fn try_submit(&mut self) -> FormEvent {
        if self.name.trim().is_empty() || self.repo.trim().is_empty() {
            self.error = Some("a bird needs a name and a repo path".into());
            FormEvent::Consumed
        } else {
            FormEvent::AddBot {
                name: self.name.trim().to_string(),
                glyph: if self.glyph.trim().is_empty() {
                    GLYPHS[0].to_string()
                } else {
                    self.glyph.trim().to_string()
                },
                repo: self.repo.trim().to_string(),
            }
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => self.field = (self.field + 2) % 3,
            KeyCode::Backspace => {
                self.error = None;
                match self.field {
                    0 => self.name.pop(),
                    1 => self.repo.pop(),
                    _ => self.glyph.pop().map(|_| ' '),
                };
            }
            KeyCode::Char(c) => {
                self.error = None;
                match self.field {
                    0 => self.name.push(c),
                    1 => self.repo.push(c),
                    _ => self.glyph.push(c),
                }
            }
            KeyCode::Enter => return self.try_submit(),
            _ => {}
        }
        FormEvent::Consumed
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> FormEvent {
        let Some((x, y)) = click(&m, self.popup) else {
            return FormEvent::Cancel;
        };
        for (i, r) in self.field_rects.iter().enumerate() {
            if hits(*r, x, y) {
                self.field = i;
                return FormEvent::Consumed;
            }
        }
        if hits(self.submit, x, y) {
            return self.try_submit();
        }
        FormEvent::Consumed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::from(""),
            field_line("name", &self.name, self.field == 0, "what the bird is called"),
            field_line("repo", &self.repo, self.field == 1, "path to its repo (~ ok)"),
            field_line("glyph", &self.glyph, self.field == 2, "an emoji face"),
            Line::from(""),
            Line::from(dim(
                "  a persona file is created in birds/ — edit it to shape the character",
            )),
        ];
        lines.extend(error_lines(&self.error));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ⏎ hatch", Style::default().fg(ACCENT)),
            dim(" · ⇥ next field · esc cancel  (all clickable)"),
        ]));
        let submit_idx = lines.len() - 1;

        self.popup = popup(frame, area, "new bird", lines);
        for (i, slot) in self.field_rects.iter_mut().enumerate() {
            *slot = line_rect(self.popup, i + 1);
        }
        self.submit = line_rect(self.popup, submit_idx);
    }
}

// ----------------------------------------------------------------- rename tab

pub struct RenameTabForm {
    key: crate::config::SessionKey,
    name: String,
    popup: Rect,
    submit: Rect,
}

impl RenameTabForm {
    pub fn new(key: crate::config::SessionKey, current: String) -> RenameTabForm {
        RenameTabForm {
            key,
            name: current,
            popup: Rect::default(),
            submit: Rect::default(),
        }
    }

    fn submit(&self) -> FormEvent {
        FormEvent::RenameTab {
            key: self.key.clone(),
            name: self.name.trim().to_string(),
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Backspace => {
                self.name.pop();
            }
            KeyCode::Char(c) => self.name.push(c),
            KeyCode::Enter => return self.submit(),
            _ => {}
        }
        FormEvent::Consumed
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> FormEvent {
        let Some((x, y)) = click(&m, self.popup) else {
            return FormEvent::Cancel;
        };
        if hits(self.submit, x, y) {
            return self.submit();
        }
        FormEvent::Consumed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(""),
            field_line("label", &self.name, true, "a short label — empty clears it"),
            Line::from(""),
            Line::from(dim(format!(
                "  display only — the session stays {}",
                self.key.session_name()
            ))),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ⏎ save", Style::default().fg(ACCENT)),
                dim(" · esc cancel"),
            ]),
        ];
        let submit_idx = lines.len() - 1;
        self.popup = popup(frame, area, &format!("name tab {}", self.key.tab), lines);
        self.submit = line_rect(self.popup, submit_idx);
    }
}

// ------------------------------------------------------------------- new room

pub struct NewRoomForm {
    /// 0 = typing the name; 1 = picking members.
    field: usize,
    name: String,
    cursor: usize,
    chosen: Vec<bool>,
    bots: Vec<(BotId, String, String)>, // id, glyph, name
    pub error: Option<String>,
    popup: Rect,
    name_rect: Rect,
    member_rects: Vec<Rect>,
    submit: Rect,
}

impl NewRoomForm {
    pub fn new(bots: &[Bot]) -> NewRoomForm {
        NewRoomForm {
            field: 0,
            name: String::new(),
            cursor: 0,
            chosen: vec![true; bots.len()], // everyone in by default; space evicts
            bots: bots
                .iter()
                .map(|b| (b.id.clone(), b.glyph.clone(), b.name.clone()))
                .collect(),
            error: None,
            popup: Rect::default(),
            name_rect: Rect::default(),
            member_rects: Vec::new(),
            submit: Rect::default(),
        }
    }

    fn members(&self) -> Vec<BotId> {
        self.bots
            .iter()
            .zip(&self.chosen)
            .filter(|(_, &c)| c)
            .map(|((id, _, _), _)| id.clone())
            .collect()
    }

    fn try_submit(&mut self) -> FormEvent {
        if self.name.trim().is_empty() {
            self.error = Some("a room needs a name".into());
            FormEvent::Consumed
        } else {
            FormEvent::NewRoom {
                name: self.name.trim().to_string(),
                members: self.members(),
            }
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Tab => self.field = 1 - self.field,
            KeyCode::Enter => return self.try_submit(),
            KeyCode::Backspace if self.field == 0 => {
                self.error = None;
                self.name.pop();
            }
            KeyCode::Char(c) if self.field == 0 => {
                self.error = None;
                self.name.push(c);
            }
            KeyCode::Char('j') | KeyCode::Down if self.field == 1 => {
                self.cursor = (self.cursor + 1).min(self.bots.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up if self.field == 1 => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Char(' ') if self.field == 1 => {
                if let Some(c) = self.chosen.get_mut(self.cursor) {
                    *c = !*c;
                    self.error = None;
                }
            }
            _ => {}
        }
        FormEvent::Consumed
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> FormEvent {
        let Some((x, y)) = click(&m, self.popup) else {
            return FormEvent::Cancel;
        };
        if hits(self.name_rect, x, y) {
            self.field = 0;
            return FormEvent::Consumed;
        }
        for (i, r) in self.member_rects.clone().iter().enumerate() {
            if hits(*r, x, y) {
                self.field = 1;
                self.cursor = i;
                if let Some(c) = self.chosen.get_mut(i) {
                    *c = !*c;
                    self.error = None;
                }
                return FormEvent::Consumed;
            }
        }
        if hits(self.submit, x, y) {
            return self.try_submit();
        }
        FormEvent::Consumed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::from(""),
            field_line("name", &self.name, self.field == 0, "what the room is about"),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    if self.field == 1 { " ▸ " } else { "   " },
                    Style::default().fg(ACCENT),
                ),
                muted("birds in the room"),
                dim(if self.field == 1 {
                    "   j/k move · space or click toggles"
                } else {
                    "   ⇥ or click to edit"
                }),
            ]),
        ];
        let first_member_idx = lines.len();
        for (i, ((_, glyph, name), &on)) in self.bots.iter().zip(&self.chosen).enumerate() {
            let cursor = if self.field == 1 && i == self.cursor { "▸" } else { " " };
            let mark = if on { "◉" } else { "○" };
            lines.push(Line::from(vec![
                Span::styled(format!("   {cursor} "), Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{mark} "),
                    Style::default().fg(if on { ACCENT } else { DIM }),
                ),
                Span::styled(
                    format!("{glyph} {name}"),
                    Style::default().fg(bird_color(i)),
                ),
            ]));
        }
        lines.extend(error_lines(&self.error));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ⏎ create", Style::default().fg(ACCENT)),
            dim(" · ⇥ fields · esc cancel  (all clickable)"),
        ]));
        let submit_idx = lines.len() - 1;

        self.popup = popup(frame, area, "new room", lines);
        self.name_rect = line_rect(self.popup, 1);
        self.member_rects = (0..self.bots.len())
            .map(|i| line_rect(self.popup, first_member_idx + i))
            .collect();
        self.submit = line_rect(self.popup, submit_idx);
    }
}

// -------------------------------------------------------------------- compose

/// One composer, two modes. With a `source` bird (the sidebar's selection) the
/// SOURCE packages the context and hands off; without one the text goes
/// straight to the target.
pub struct ComposeForm {
    pub source: Option<BotId>,
    targets: Vec<(BotId, String, String)>, // id, glyph, name
    target_idx: usize,
    text: String,
    popup: Rect,
    chip_rects: Vec<(Rect, usize)>,
    submit: Rect,
}

impl ComposeForm {
    pub fn new(shared: &Shared, source: Option<BotId>, preselect: Option<BotId>) -> Option<ComposeForm> {
        let targets: Vec<(BotId, String, String)> = shared
            .config
            .bots
            .iter()
            .filter(|b| Some(&b.id) != source.as_ref())
            .map(|b| (b.id.clone(), b.glyph.clone(), b.name.clone()))
            .collect();
        if targets.is_empty() {
            return None;
        }
        let target_idx = preselect
            .and_then(|p| targets.iter().position(|(id, _, _)| id == &p))
            .unwrap_or(0);
        Some(ComposeForm {
            source,
            targets,
            target_idx,
            text: String::new(),
            popup: Rect::default(),
            chip_rects: Vec::new(),
            submit: Rect::default(),
        })
    }

    fn try_submit(&mut self) -> FormEvent {
        let text = self.text.trim().to_string();
        if text.is_empty() {
            return FormEvent::Consumed;
        }
        FormEvent::Compose {
            source: self.source.clone(),
            target: self.targets[self.target_idx].0.clone(),
            text,
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Left | KeyCode::BackTab => {
                self.target_idx = (self.target_idx + self.targets.len() - 1) % self.targets.len();
            }
            KeyCode::Right | KeyCode::Tab => {
                self.target_idx = (self.target_idx + 1) % self.targets.len();
            }
            KeyCode::Backspace => {
                self.text.pop();
            }
            KeyCode::Char(c) => self.text.push(c),
            KeyCode::Enter => return self.try_submit(),
            _ => {}
        }
        FormEvent::Consumed
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> FormEvent {
        let Some((x, y)) = click(&m, self.popup) else {
            return FormEvent::Cancel;
        };
        if let Some((_, idx)) = self.chip_rects.iter().find(|(r, _)| hits(*r, x, y)) {
            self.target_idx = *idx;
            return FormEvent::Consumed;
        }
        if hits(self.submit, x, y) {
            return self.try_submit();
        }
        FormEvent::Consumed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let title = if self.source.is_some() { "handoff" } else { "message a bird" };

        // Chips, measuring display width as we go so clicks land exactly.
        let mut chips: Vec<Span> = vec![muted(" to  ")];
        let mut offsets: Vec<(u16, u16, usize)> = Vec::new(); // (x offset, width, idx)
        let mut cursor = " to  ".width() as u16;
        for (i, (_, glyph, name)) in self.targets.iter().enumerate() {
            let label = format!(" {glyph} {name} ");
            let w = label.width() as u16;
            offsets.push((cursor, w, i));
            let on = i == self.target_idx;
            chips.push(Span::styled(
                label,
                if on {
                    Style::default()
                        .fg(ACCENT)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(DIM)
                },
            ));
            chips.push(Span::raw(" "));
            cursor += w + 1;
        }
        chips.push(dim("  ◂ ▸ or click"));

        let mut lines = vec![Line::from(""), Line::from(chips), Line::from("")];
        let shown = tail(&self.text, 56);
        lines.push(Line::from(vec![
            Span::styled(" > ", Style::default().fg(ACCENT)),
            Span::styled(
                format!("{shown}▌"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
        if let Some(src) = &self.source {
            lines.push(Line::from(dim(format!(
                "  @{src} packages its context and SendMessages the ask — linear/figma",
            ))));
            lines.push(Line::from(dim(
                "  links are detected and fetched via MCP on the other side",
            )));
        } else {
            lines.push(Line::from(dim(
                "  goes straight into the bird's session (spawning it if asleep)",
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ⏎ send", Style::default().fg(ACCENT)),
            dim(" · ◂ ▸ target · esc cancel  (all clickable)"),
        ]));
        let submit_idx = lines.len() - 1;

        self.popup = popup(frame, area, title, lines);
        let chip_line = line_rect(self.popup, 1);
        self.chip_rects = offsets
            .into_iter()
            .map(|(dx, w, i)| {
                (
                    Rect {
                        x: chip_line.x + dx,
                        y: chip_line.y,
                        width: w,
                        height: 1,
                    },
                    i,
                )
            })
            .collect();
        self.submit = line_rect(self.popup, submit_idx);
    }
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let kept: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{kept}")
}

// --------------------------------------------------------------------- context

#[derive(Clone, Copy)]
enum MenuAct {
    Talk,
    Handoff,
    Profile,
    Fresh,
    Stop,
    Release,
    Write,
    DeleteRoom,
}

/// The right-click menu for a sidebar card, anchored at the click. Keyboard
/// works too (j/k + ⏎); hovering moves the cursor; clicking outside closes.
pub struct ContextMenu {
    target: crate::action::RosterTarget,
    items: Vec<(&'static str, MenuAct)>,
    cursor: usize,
    anchor: (u16, u16),
    popup: Rect,
    item_rects: Vec<Rect>,
}

impl ContextMenu {
    pub fn new(target: crate::action::RosterTarget, x: u16, y: u16) -> ContextMenu {
        use crate::action::RosterTarget;
        let items: Vec<(&'static str, MenuAct)> = match &target {
            RosterTarget::Bird(_) => vec![
                ("talk — keyboard to the bird", MenuAct::Talk),
                ("handoff to a teammate", MenuAct::Handoff),
                ("profile — persona · routines", MenuAct::Profile),
                ("fresh conversation", MenuAct::Fresh),
                ("stop the session", MenuAct::Stop),
                ("release from the roster", MenuAct::Release),
            ],
            RosterTarget::Room(_) => vec![
                ("write to the room", MenuAct::Write),
                ("delete the room", MenuAct::DeleteRoom),
            ],
        };
        ContextMenu {
            target,
            items,
            cursor: 0,
            anchor: (x, y),
            popup: Rect::default(),
            item_rects: Vec::new(),
        }
    }

    fn pick(&self, act: MenuAct) -> FormEvent {
        use crate::action::RosterTarget;
        match (&self.target, act) {
            (RosterTarget::Bird(id), MenuAct::Talk) => FormEvent::Talk(id.clone()),
            (RosterTarget::Bird(id), MenuAct::Handoff) => FormEvent::Handoff(id.clone()),
            (RosterTarget::Bird(id), MenuAct::Profile) => FormEvent::Profile(id.clone()),
            (RosterTarget::Bird(id), MenuAct::Fresh) => FormEvent::FreshStart(id.clone()),
            (RosterTarget::Bird(id), MenuAct::Stop) => FormEvent::StopBird(id.clone()),
            (_, MenuAct::Release | MenuAct::DeleteRoom) => {
                FormEvent::AskDelete(self.target.clone())
            }
            (RosterTarget::Room(id), MenuAct::Write) => FormEvent::WriteRoom(id.clone()),
            _ => FormEvent::Cancel,
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => FormEvent::Cancel,
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor = (self.cursor + 1) % self.items.len();
                FormEvent::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor = (self.cursor + self.items.len() - 1) % self.items.len();
                FormEvent::Consumed
            }
            KeyCode::Enter => self.pick(self.items[self.cursor].1),
            _ => FormEvent::Consumed,
        }
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> FormEvent {
        if let MouseEventKind::Moved | MouseEventKind::Drag(_) = m.kind {
            if let Some(i) = self
                .item_rects
                .iter()
                .position(|r| hits(*r, m.column, m.row))
            {
                self.cursor = i;
            }
            return FormEvent::Consumed;
        }
        let Some((x, y)) = click(&m, self.popup) else {
            return FormEvent::Cancel;
        };
        if let Some(i) = self.item_rects.iter().position(|r| hits(*r, x, y)) {
            return self.pick(self.items[i].1);
        }
        FormEvent::Consumed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let width = (self
            .items
            .iter()
            .map(|(label, _)| label.width())
            .max()
            .unwrap_or(10) as u16
            + 4)
            .min(area.width);
        let height = (self.items.len() as u16 + 2).min(area.height);
        let x = self.anchor.0.min(area.width.saturating_sub(width));
        let y = self.anchor.1.min(area.height.saturating_sub(height));
        self.popup = Rect {
            x,
            y,
            width,
            height,
        };

        frame.render_widget(Clear, self.popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT));
        let inner = block.inner(self.popup);
        frame.render_widget(block, self.popup);

        self.item_rects.clear();
        let lines: Vec<Line> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, (label, act))| {
                self.item_rects.push(Rect {
                    x: inner.x,
                    y: inner.y + i as u16,
                    width: inner.width,
                    height: 1,
                });
                let destructive = matches!(act, MenuAct::Release | MenuAct::DeleteRoom);
                let style = if i == self.cursor {
                    Style::default()
                        .fg(if destructive { BAD } else { ACCENT })
                        .add_modifier(Modifier::REVERSED)
                } else if destructive {
                    Style::default().fg(BAD)
                } else {
                    Style::default().fg(crate::ui::MUTED)
                };
                Line::from(Span::styled(format!(" {label} "), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

// -------------------------------------------------------------------- tab menu

/// Right-click menu for a session tab. The click already selected the tab, so
/// every pick is just a thread-pane Action on the current tab.
pub struct TabMenu {
    items: Vec<(&'static str, crate::action::Action)>,
    cursor: usize,
    anchor: (u16, u16),
    popup: Rect,
    item_rects: Vec<Rect>,
}

impl TabMenu {
    pub fn new(tab: u8, x: u16, y: u16) -> TabMenu {
        use crate::action::Action;
        let mut items = vec![
            ("name the tab", Action::RenameTab),
            ("fresh conversation here", Action::FreshTab),
            ("new tab beside it", Action::NewTab),
        ];
        if tab != 1 {
            items.push(("close the tab", Action::CloseTab));
        }
        TabMenu {
            items,
            cursor: 0,
            anchor: (x, y),
            popup: Rect::default(),
            item_rects: Vec::new(),
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => FormEvent::Cancel,
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor = (self.cursor + 1) % self.items.len();
                FormEvent::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor = (self.cursor + self.items.len() - 1) % self.items.len();
                FormEvent::Consumed
            }
            KeyCode::Enter => FormEvent::Tab(self.items[self.cursor].1),
            _ => FormEvent::Consumed,
        }
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> FormEvent {
        if let MouseEventKind::Moved | MouseEventKind::Drag(_) = m.kind {
            if let Some(i) = self
                .item_rects
                .iter()
                .position(|r| hits(*r, m.column, m.row))
            {
                self.cursor = i;
            }
            return FormEvent::Consumed;
        }
        let Some((x, y)) = click(&m, self.popup) else {
            return FormEvent::Cancel;
        };
        if let Some(i) = self.item_rects.iter().position(|r| hits(*r, x, y)) {
            return FormEvent::Tab(self.items[i].1);
        }
        FormEvent::Consumed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let width = (self
            .items
            .iter()
            .map(|(label, _)| label.width())
            .max()
            .unwrap_or(10) as u16
            + 4)
            .min(area.width);
        let height = (self.items.len() as u16 + 2).min(area.height);
        let x = self.anchor.0.min(area.width.saturating_sub(width));
        let y = self.anchor.1.min(area.height.saturating_sub(height));
        self.popup = Rect { x, y, width, height };

        frame.render_widget(Clear, self.popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT));
        let inner = block.inner(self.popup);
        frame.render_widget(block, self.popup);

        self.item_rects.clear();
        let lines: Vec<Line> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, (label, act))| {
                self.item_rects.push(Rect {
                    x: inner.x,
                    y: inner.y + i as u16,
                    width: inner.width,
                    height: 1,
                });
                let destructive = matches!(act, crate::action::Action::CloseTab);
                let style = if i == self.cursor {
                    Style::default()
                        .fg(if destructive { BAD } else { ACCENT })
                        .add_modifier(Modifier::REVERSED)
                } else if destructive {
                    Style::default().fg(BAD)
                } else {
                    Style::default().fg(crate::ui::MUTED)
                };
                Line::from(Span::styled(format!(" {label} "), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

// --------------------------------------------------------------------- confirm

/// A small yes/no gate for roster deletions — destructive enough to ask,
/// cheap enough that the answer is one keystroke.
pub struct ConfirmForm {
    target: crate::action::RosterTarget,
    question: String,
    note: String,
    popup: Rect,
    yes: Rect,
    no: Rect,
}

impl ConfirmForm {
    pub fn new(target: crate::action::RosterTarget, question: String, note: String) -> ConfirmForm {
        ConfirmForm {
            target,
            question,
            note,
            popup: Rect::default(),
            yes: Rect::default(),
            no: Rect::default(),
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Char('y') | KeyCode::Enter => FormEvent::Delete(self.target.clone()),
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => FormEvent::Cancel,
            _ => FormEvent::Consumed,
        }
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> FormEvent {
        let Some((x, y)) = click(&m, self.popup) else {
            return FormEvent::Cancel;
        };
        if hits(self.yes, x, y) {
            return FormEvent::Delete(self.target.clone());
        }
        if hits(self.no, x, y) {
            return FormEvent::Cancel;
        }
        FormEvent::Consumed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  {}", self.question),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for chunk in wrap(&self.note, 58) {
            lines.push(Line::from(dim(format!("  {chunk}"))));
        }
        lines.push(Line::from(""));
        let buttons_idx = lines.len();
        lines.push(Line::from(vec![
            Span::styled(
                "   y release   ",
                Style::default().fg(BAD).add_modifier(Modifier::REVERSED),
            ),
            Span::raw("   "),
            Span::styled(
                "   n keep   ",
                Style::default().fg(DIM).add_modifier(Modifier::REVERSED),
            ),
        ]));
        lines.push(Line::from(""));

        self.popup = popup(frame, area, "sure?", lines);
        let row = line_rect(self.popup, buttons_idx);
        self.yes = Rect { width: 15, ..row };
        self.no = Rect {
            x: row.x + 18,
            width: 12,
            ..row
        };
    }
}

// --------------------------------------------------------------------- profile

/// The bird's profile — Grok Bot's bot-profile screen, terminal-shaped:
/// identity, session facts, routines, and the notifications toggle.
pub struct ProfileView {
    pub id: BotId,
    popup: Rect,
    notify_rect: Rect,
    persona_rect: Rect,
    fresh_rect: Rect,
}

impl ProfileView {
    pub fn new(id: BotId) -> ProfileView {
        ProfileView {
            id,
            popup: Rect::default(),
            notify_rect: Rect::default(),
            persona_rect: Rect::default(),
            fresh_rect: Rect::default(),
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => FormEvent::Cancel,
            KeyCode::Char(' ') => FormEvent::ToggleNotify(self.id.clone()),
            KeyCode::Char('e') => FormEvent::OpenPersona(self.id.clone()),
            KeyCode::Char('N') => FormEvent::FreshStart(self.id.clone()),
            _ => FormEvent::Consumed,
        }
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> FormEvent {
        let Some((x, y)) = click(&m, self.popup) else {
            return FormEvent::Cancel;
        };
        if hits(self.notify_rect, x, y) {
            return FormEvent::ToggleNotify(self.id.clone());
        }
        if hits(self.persona_rect, x, y) {
            return FormEvent::OpenPersona(self.id.clone());
        }
        if hits(self.fresh_rect, x, y) {
            return FormEvent::FreshStart(self.id.clone());
        }
        FormEvent::Consumed
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, s: &Shared) {
        let Some(bot) = s.config.bot(&self.id) else { return };
        let status = s.agents.status(&self.id);

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw(format!("  {} ", bot.glyph)),
                Span::styled(
                    bot.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                dim("   "),
                crate::ui::status_span(status),
            ]),
            Line::from(vec![muted("  repo      "), Span::raw(bot.repo.clone())]),
            Line::from(vec![
                muted("  session   "),
                Span::raw(self.id.session_name()),
                // The profile is about the bird = its PRIMARY session; tabs
                // live in the thread pane's strip.
                dim(
                    if s.agents.has_session_record(&crate::config::SessionKey::primary(
                        self.id.clone(),
                    )) {
                        "  (resumes with full memory)"
                    } else {
                        "  (never flown)"
                    },
                ),
            ]),
            Line::from(""),
        ];

        let notify_idx = lines.len();
        lines.push(Line::from(vec![
            Span::styled(
                if bot.notify { "  ◉ " } else { "  ○ " },
                Style::default().fg(if bot.notify { ACCENT } else { DIM }),
            ),
            Span::raw("notifications"),
            dim("  — banner when this bird finishes or needs input · space/click"),
        ]));

        let persona_idx = lines.len();
        lines.push(Line::from(vec![
            Span::styled("  ✎ ", Style::default().fg(ACCENT)),
            Span::raw("persona"),
            muted(format!("  {}", bot.persona)),
            dim("  · e/click opens"),
        ]));

        lines.push(Line::from(""));
        lines.push(Line::from(muted("  routines")));
        if bot.routines.is_empty() {
            lines.push(Line::from(dim(
                "    none — add {id, schedule, prompt} under this bird in config.json",
            )));
            lines.push(Line::from(dim(
                "    (daily@HH:MM · weekdays@HH:MM · every:<N>m|h; fires while aviary runs)",
            )));
        } else {
            for r in &bot.routines {
                let last = s
                    .agents
                    .routine_last_run(&self.id, &r.id)
                    .map(|_| "has fired")
                    .unwrap_or("never fired");
                lines.push(Line::from(vec![
                    Span::styled("    ⏱ ", Style::default().fg(ACCENT)),
                    Span::raw(format!("{:<14}", r.id)),
                    muted(format!("{:<16}", r.schedule)),
                    dim(last),
                ]));
            }
        }

        lines.push(Line::from(""));
        let fresh_idx = lines.len();
        lines.push(Line::from(vec![
            Span::styled("  ↺ ", Style::default().fg(ACCENT)),
            Span::raw("fresh conversation"),
            dim("  — abandon this one and hatch anew · N/click"),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(dim("  esc closes")));

        self.popup = popup(frame, area, &format!("{} {}", bot.glyph, bot.name), lines);
        self.notify_rect = line_rect(self.popup, notify_idx);
        self.persona_rect = line_rect(self.popup, persona_idx);
        self.fresh_rect = line_rect(self.popup, fresh_idx);
    }
}
