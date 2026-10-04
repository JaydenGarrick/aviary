//! Modal forms, owned by the shell. While one is open it captures every key
//! (layer 4); submitting hands a typed result back for the shell to apply.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::{Bot, BotId};
use crate::shared::Shared;
use crate::ui::{bird_color, centered, dim, muted, wrap, ACCENT, BAD, DIM};

pub enum Overlay {
    None,
    Help,
    AddBot(AddBotForm),
    NewRoom(NewRoomForm),
    Compose(ComposeForm),
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

fn popup(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
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

// -------------------------------------------------------------------- add bot

pub struct AddBotForm {
    field: usize, // 0 name · 1 repo · 2 glyph
    name: String,
    repo: String,
    glyph: String,
    pub error: Option<String>,
}

impl AddBotForm {
    pub fn new(existing: usize) -> AddBotForm {
        AddBotForm {
            field: 0,
            name: String::new(),
            repo: String::new(),
            glyph: GLYPHS[existing % GLYPHS.len()].to_string(),
            error: None,
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
            KeyCode::Enter => {
                if self.name.trim().is_empty() || self.repo.trim().is_empty() {
                    self.error = Some("a bird needs a name and a repo path".into());
                } else {
                    return FormEvent::AddBot {
                        name: self.name.trim().to_string(),
                        glyph: if self.glyph.trim().is_empty() {
                            GLYPHS[0].to_string()
                        } else {
                            self.glyph.trim().to_string()
                        },
                        repo: self.repo.trim().to_string(),
                    };
                }
            }
            _ => {}
        }
        FormEvent::Consumed
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
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
        lines.push(Line::from(dim("  ⏎ hatch · ⇥ next field · esc cancel")));
        popup(frame, area, "new bird", lines);
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

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        match k.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Tab => self.field = 1 - self.field,
            KeyCode::Enter => {
                if self.name.trim().is_empty() {
                    self.error = Some("a room needs a name".into());
                } else {
                    return FormEvent::NewRoom {
                        name: self.name.trim().to_string(),
                        members: self.members(),
                    };
                }
            }
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

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
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
                    "   j/k move · space toggles"
                } else {
                    "   ⇥ to edit"
                }),
            ]),
        ];
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
        lines.push(Line::from(dim("  ⏎ create · ⇥ fields · esc cancel")));
        popup(frame, area, "new room", lines);
    }
}

// -------------------------------------------------------------------- compose

/// One composer, two modes. With a `source` bird (opened from its thread) the
/// SOURCE packages the context and hands off; without one (roster) the text
/// goes straight to the target.
pub struct ComposeForm {
    pub source: Option<BotId>,
    targets: Vec<(BotId, String, String)>, // id, glyph, name
    target_idx: usize,
    text: String,
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
        })
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
            KeyCode::Enter => {
                let text = self.text.trim().to_string();
                if !text.is_empty() {
                    return FormEvent::Compose {
                        source: self.source.clone(),
                        target: self.targets[self.target_idx].0.clone(),
                        text,
                    };
                }
            }
            _ => {}
        }
        FormEvent::Consumed
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let title = if self.source.is_some() { "handoff" } else { "message a bird" };

        let mut chips: Vec<Span> = vec![muted(" to  ")];
        for (i, (_, glyph, name)) in self.targets.iter().enumerate() {
            let on = i == self.target_idx;
            chips.push(Span::styled(
                format!(" {glyph} {name} "),
                if on {
                    Style::default()
                        .fg(ACCENT)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(DIM)
                },
            ));
            chips.push(Span::raw(" "));
        }
        chips.push(dim("  ◂ ▸ picks"));

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
        lines.push(Line::from(dim("  ⏎ send · ◂ ▸ target · esc cancel")));
        popup(frame, area, title, lines);
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
