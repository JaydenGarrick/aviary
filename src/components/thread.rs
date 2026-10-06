//! The content pane for a bird: its Claude Code session, full height, beside
//! the sidebar. All bird ACTIONS (wake, stop, fresh, handoff) belong to the
//! sidebar, which owns the selection — this pane owns only its SESSION TABS:
//! extra, human-driven parallel sessions of the viewed bird. External signals
//! never land on a tab; they always wake the primary (tab 1).

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::action::{Action, Effects};
use crate::components::{hits, Component};
use crate::config::SessionKey;
use crate::keymap::{bind, ch, Binding};
use crate::shared::Shared;
use crate::ui::{bird_color, dim, muted, status_glyph, status_span, ACCENT};
use crate::widgets::agent_pane;
use crate::widgets::list_nav::{ListNav, Wrap};

/// Session-tab keys, merged into dispatch and hints while a bird's pane is up.
pub static KEYMAP: &[Binding] = &[
    bind(ch(']'), Action::NextTab, Some("tabs"), "next session tab"),
    bind(ch('['), Action::PrevTab, None, "previous session tab"),
    bind(ch('T'), Action::NewTab, None, "open another session of this bird"),
    bind(ch('W'), Action::CloseTab, None, "close the viewed tab (tab 1 refuses)"),
    bind(ch('R'), Action::RenameTab, None, "label the viewed tab"),
];

/// The tab strip's `[+]` button, stored beside real (1-based) tab numbers.
const NEW_TAB: u8 = 0;

#[derive(Default)]
pub struct Thread {
    pane: Rect,
    /// Click targets in the tab strip: each tab, plus `[+]` as [`NEW_TAB`].
    tab_rects: Vec<(Rect, u8)>,
    /// Recent handoff briefs involving the viewed bird (empty-state content).
    feed: Vec<String>,
    feed_for: Option<crate::config::BotId>,
    ticks: u32,
}

impl Thread {
    fn refresh_feed(&mut self, s: &Shared) {
        let Some(id) = s.current_bot.clone() else {
            self.feed.clear();
            return;
        };
        let needle_from = format!("-{id}-to-");
        let needle_to = format!("-to-{id}.md");
        let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(s.config.handoffs_dir()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.contains(&needle_from) || name.ends_with(&needle_to) {
                    if let Ok(meta) = e.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            found.push((mtime, name));
                        }
                    }
                }
            }
        }
        found.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        self.feed = found.into_iter().take(3).map(|(_, n)| n).collect();
        self.feed_for = Some(id);
    }

    /// Open one more session of the viewed bird and show it.
    fn new_tab(&self, s: &mut Shared) {
        let Some(id) = s.current_bot.clone() else { return };
        let tab = crate::agent_store::lowest_free_tab(&s.agents.tabs(&id));
        s.current_tab = tab;
        let key = SessionKey { bot: id, tab };
        let prompt = s.opening_prompt(&key);
        s.boot_key(&key, prompt.as_deref());
        s.agent_focused = true;
        s.flash(format!("tab {tab} — session {}", key.session_name()));
    }

    fn select_tab(&self, s: &mut Shared, tab: u8) {
        if s.current_tab != tab {
            s.current_tab = tab;
            // A tab switch re-targets the keyboard; never leave it pointed at
            // a session that may not be running.
            s.agent_focused = false;
        }
        if let Some(id) = s.current_bot.clone() {
            s.agents.clear_unread(&id);
        }
    }
}

impl Component for Thread {
    fn update(&mut self, a: Action, s: &mut Shared, fx: &mut Effects) {
        let Some(id) = s.current_bot.clone() else { return };
        match a {
            Action::NextTab | Action::PrevTab => {
                let tabs = s.agents.tabs(&id);
                let mut nav = ListNav {
                    selected: tabs.iter().position(|t| *t == s.current_tab).unwrap_or(0),
                };
                let dir = if a == Action::NextTab { Action::Down } else { Action::Up };
                nav.handle(dir, tabs.len(), Wrap::Cycle);
                self.select_tab(s, tabs[nav.selected]);
            }
            Action::NewTab => self.new_tab(s),
            Action::RenameTab => {
                fx.msg(crate::action::Msg::OpenRenameTab(SessionKey {
                    bot: id,
                    tab: s.current_tab,
                }));
            }
            Action::CloseTab => {
                if s.current_tab == 1 {
                    s.flash("tab 1 is the bird itself — x on the sidebar stops it");
                    return;
                }
                let key = SessionKey { bot: id, tab: s.current_tab };
                s.agents.close_tab(&s.config, &key);
                s.current_tab = 1;
                s.agent_focused = false;
                s.flash(format!("closed tab {} — back to tab 1", key.tab));
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, s: &mut Shared, _fx: &mut Effects) {
        self.ticks += 1;
        if self.feed_for != s.current_bot || self.ticks % 5 == 0 {
            self.refresh_feed(s);
        }
    }

    fn handle_mouse(&mut self, m: MouseEvent, s: &mut Shared, _fx: &mut Effects) {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            if let Some((_, tab)) = self
                .tab_rects
                .iter()
                .find(|(r, _)| hits(*r, m.column, m.row))
            {
                match *tab {
                    NEW_TAB => self.new_tab(s),
                    t => self.select_tab(s, t),
                }
                return;
            }
        }
        let over_pane = hits(self.pane, m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if over_pane => {
                // Clicking the pane wakes the viewed session and hands it the
                // keyboard, exactly like ⏎ on the sidebar.
                if let Some(key) = s.current_key() {
                    let prompt = s.opening_prompt(&key);
                    s.boot_key(&key, prompt.as_deref());
                    s.agent_focused = true;
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if over_pane => {
                let delta = if m.kind == MouseEventKind::ScrollUp { 3 } else { -3 };
                let (col, row) = (
                    m.column.saturating_sub(self.pane.x + 1),
                    m.row.saturating_sub(self.pane.y + 1),
                );
                if let Some(key) = s.current_key() {
                    if let Some(session) = s.agents.get_mut(&key) {
                        session.term.wheel(delta, col, row);
                    }
                }
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, s: &mut Shared) {
        self.tab_rects.clear();
        let Some(id) = s.current_bot.clone() else {
            self.pane = area;
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

        // The tab strip — always up, so [+] is discoverable before any second
        // tab exists.
        let tabs = s.agents.tabs(&id);
        let mut body = area;
        if area.height > 1 {
            let strip = Rect { height: 1, ..area };
            body = Rect {
                y: area.y + 1,
                height: area.height - 1,
                ..area
            };
            let mut spans: Vec<Span> = vec![Span::raw(" ")];
            let mut x = strip.x + 1;
            for t in &tabs {
                let key = SessionKey { bot: id.clone(), tab: *t };
                let selected = *t == s.current_tab;
                let style = if selected {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(crate::ui::DIM)
                };
                let label = s
                    .agents
                    .tab_name(&key)
                    .map(|n| n.chars().take(12).collect::<String>());
                let open = match label {
                    Some(n) => format!("[{t}:{n} "),
                    None => format!("[{t} "),
                };
                // Rect width: the label's chars, the 1-wide glyph, and "]".
                let width = (open.chars().count() + 2) as u16;
                spans.push(Span::styled(open, style));
                spans.push(status_glyph(s.agents.status_key(&key)));
                spans.push(Span::styled("]", style));
                spans.push(Span::raw(" "));
                self.tab_rects.push((Rect { x, width, ..strip }, *t));
                x += width + 1;
            }
            spans.push(dim("[+]"));
            self.tab_rects.push((Rect { x, width: 3, ..strip }, NEW_TAB));
            frame.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), strip);
        }
        self.pane = body;

        let key = SessionKey { bot: id.clone(), tab: s.current_tab };
        let status = s.agents.status_key(&key);

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

        let mut empty = vec![
            Line::from(""),
            Line::from(vec![
                muted("  press "),
                Span::styled("⏎", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                muted(format!(" to wake {} — it lives in {}", bot.name, bot.repo)),
            ]),
            Line::from(""),
            Line::from(dim(format!(
                "  its session is named {} and resumes with its full memory;",
                key.session_name()
            ))),
            Line::from(dim("  teammates reach it by that name with SendMessage")),
            Line::from(""),
            Line::from(dim("  N starts a brand-new conversation · p opens its profile")),
            Line::from(dim("  T opens a parallel session in another tab")),
        ];
        if !self.feed.is_empty() {
            empty.push(Line::from(""));
            empty.push(Line::from(muted("  recent handoffs")));
            for name in &self.feed {
                empty.push(Line::from(vec![
                    Span::styled("    ⇄ ", Style::default().fg(ACCENT)),
                    dim(name.clone()),
                ]));
            }
            empty.push(Line::from(dim(format!(
                "    (briefs live in {})",
                s.config.handoffs_dir().display()
            ))));
        }

        let focused = s.agent_focused;
        agent_pane::draw(frame, body, title, s.agents.get_mut(&key), focused, empty);
    }
}
