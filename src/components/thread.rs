//! The content pane for a bird: its Claude Code session, full height, beside
//! the sidebar. All bird ACTIONS (wake, stop, fresh, handoff) belong to the
//! sidebar, which owns the selection — this pane owns only its SESSION TABS:
//! extra, human-driven parallel sessions of the viewed bird. External signals
//! never land on a tab; they always wake the primary (tab 1).

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::action::{Action, Effects, Msg};
use crate::components::{hits, Component};
use crate::config::SessionKey;
use crate::keymap::{bind, ch, label_for, Binding, GLOBAL};
use crate::shared::Shared;
use crate::ui::{bird_color, centered, dim, muted, status_glyph, status_span, ACCENT, MUTED};
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
            // A viewer tab in view reads its worker's news too.
            let key = SessionKey { bot: id, tab };
            if let Some(w) = s.agents.attached_worker(&key).map(str::to_string) {
                s.agents.workers.clear_unread(&w);
            }
        }
    }

    /// A viewer tab has no conversation of its own to rename or restart.
    fn flash_viewer(&self, s: &mut Shared) {
        let close = label_for(&[KEYMAP], Action::CloseTab).unwrap_or_default();
        let tab = s.current_tab;
        s.flash(format!("tab {tab} shows a worker — {close} closes it"));
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
            Action::FreshTab => {
                let key = SessionKey { bot: id, tab: s.current_tab };
                if s.agents.is_attached(&key) {
                    self.flash_viewer(s);
                    return;
                }
                s.fresh_key(&key);
                s.agent_focused = true;
            }
            Action::RenameTab => {
                let key = SessionKey { bot: id, tab: s.current_tab };
                if s.agents.is_attached(&key) {
                    self.flash_viewer(s);
                    return;
                }
                fx.msg(crate::action::Msg::OpenRenameTab(key));
            }
            Action::CloseTab => {
                if s.current_tab == 1 {
                    s.flash("tab 1 is the bird itself — x on the sidebar stops it");
                    return;
                }
                let key = SessionKey { bot: id, tab: s.current_tab };
                let viewer = s.agents.is_attached(&key).then(|| s.agents.tab_label(&key)).flatten();
                s.agents.close_tab(&s.config, &key);
                s.current_tab = 1;
                s.agent_focused = false;
                s.flash(match viewer {
                    Some(label) => format!("closed {label}'s tab — the worker keeps running"),
                    None => format!("closed tab {} — back to tab 1", key.tab),
                });
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

    fn handle_mouse(&mut self, m: MouseEvent, s: &mut Shared, fx: &mut Effects) {
        if let MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) = m.kind {
            if let Some((_, tab)) = self
                .tab_rects
                .iter()
                .find(|(r, _)| hits(*r, m.column, m.row))
            {
                // Clicking the SELECTED tab again opens its menu (the roster's
                // "click again to step in" idiom) — right-click does too, but
                // iTerm2 keeps right-clicks for its own menu by default, so
                // the left-click path is the one that works everywhere.
                match (*tab, button) {
                    (NEW_TAB, _) => self.new_tab(s),
                    (t, MouseButton::Left) if t != s.current_tab => self.select_tab(s, t),
                    (t, _) => {
                        self.select_tab(s, t);
                        fx.msg(crate::action::Msg::OpenTabMenu {
                            x: m.column,
                            y: m.row,
                        });
                    }
                }
                return;
            }
        }
        let over_pane = hits(self.pane, m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if over_pane => {
                // An empty aviary: the pane IS the hatch button.
                if s.config.bots.is_empty() {
                    fx.msg(Msg::OpenNewBot);
                    return;
                }
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
            if s.config.bots.is_empty() {
                draw_empty_aviary(frame, area);
            } else {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(Line::from(dim("  no bird selected"))),
                    area,
                );
            }
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
        // tab exists. Chips wrap onto more rows instead of clipping a long
        // label; the selected chip spells its status out, the rest glyph only.
        let tabs = s.agents.tabs(&id);
        let mut body = area;
        if area.height > 1 {
            // A chip is (its spans, width, tab number). Widths assume the
            // status glyphs render 1 cell wide, like the rest of the UI does.
            let mut chips: Vec<(Vec<Span>, u16, u8)> = Vec::new();
            for t in &tabs {
                let key = SessionKey { bot: id.clone(), tab: *t };
                let colour = bird_color(*t as usize - 1);
                let label = match s.agents.tab_label(&key) {
                    Some(n) => format!("{t}:{n}"),
                    None => format!("{t}"),
                };
                let status = s.agents.status_key(&key);
                if *t == s.current_tab {
                    let text = format!(" {label} {} ", crate::ui::status_text(status));
                    let w = text.chars().count() as u16;
                    let chip = Span::styled(
                        text,
                        Style::default()
                            .bg(colour)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    );
                    chips.push((vec![chip], w, *t));
                } else {
                    let open = format!("[{label} ");
                    let w = (open.chars().count() + 2) as u16;
                    let style = Style::default().fg(colour);
                    chips.push((
                        vec![
                            Span::styled(open, style),
                            status_glyph(status),
                            Span::styled("]", style),
                        ],
                        w,
                        *t,
                    ));
                }
            }
            chips.push((vec![dim("[+]")], 3, NEW_TAB));

            // Wrap chips into rows; never let the strip eat the whole pane.
            let max_rows = area.height.saturating_sub(4).max(1);
            let right = area.x + area.width;
            let (mut x, mut row) = (area.x + 1, 0u16);
            let mut rows: Vec<Vec<Span>> = vec![vec![Span::raw(" ")]];
            for (spans, w, t) in chips {
                if x + w > right && x > area.x + 1 && row + 1 < max_rows {
                    row += 1;
                    x = area.x + 1;
                    rows.push(vec![Span::raw(" ")]);
                }
                self.tab_rects.push((
                    Rect { x, y: area.y + row, width: w.min(right.saturating_sub(x)), height: 1 },
                    t,
                ));
                let line = rows.last_mut().expect("rows starts non-empty");
                line.extend(spans);
                line.push(Span::raw(" "));
                x += w + 1;
            }
            let strip_h = rows.len() as u16;
            let strip = Rect { height: strip_h, ..area };
            body = Rect {
                y: area.y + strip_h,
                height: area.height - strip_h,
                ..area
            };
            frame.render_widget(
                ratatui::widgets::Paragraph::new(
                    rows.into_iter().map(Line::from).collect::<Vec<_>>(),
                ),
                strip,
            );
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
        if s.agents.is_attached(&key) {
            if let Some(label) = s.agents.tab_label(&key) {
                title.spans.push(dim("  ·  "));
                title.spans.push(muted(format!("viewing {label}")));
            }
        }
        // The mod's figures (context fill, cost) when the session reports them.
        let figures = crate::ui::detail_figures(&s.agents.detail(&key));
        if !figures.is_empty() {
            title.spans.push(dim("  ·  "));
            title.spans.push(muted(figures));
        }
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
            Line::from(dim(if s.agents.has_session_record(&key) {
                format!(
                    "  ⏎ RESUMES its {} conversation ({}) with its full memory",
                    s.agents.conv_of(&key).label(),
                    key.session_name()
                )
            } else {
                format!(
                    "  ⏎ starts a BRAND-NEW {} conversation named {}",
                    s.agents.conv_of(&key).label(),
                    key.session_name()
                )
            })),
            Line::from(dim("  teammates reach it by that name with SendMessage")),
            Line::from(""),
            Line::from(dim("  T opens a parallel session in another tab · p profile")),
            Line::from(dim(
                "  fresh conversation: N (tab 1) or the tab menu (click the tab again)",
            )),
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

/// Zero state — a fresh install has no birds at all. The keys come from the
/// GLOBAL table so the prose can't drift from dispatch; a click anywhere in
/// the pane opens the same form the key does.
fn draw_empty_aviary(frame: &mut Frame, area: Rect) {
    let hatch = label_for(&[GLOBAL], Action::NewBot).unwrap_or_default();
    let help = label_for(&[GLOBAL], Action::Help).unwrap_or_default();
    let key = |k: &str| {
        Span::styled(
            format!("{k:>12}  "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    };
    let lines = vec![
        Line::from("🪺").centered(),
        Line::from(""),
        Line::from(Span::styled(
            "the aviary is empty",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ))
        .centered(),
        Line::from(""),
        Line::from(dim("a bird is a Claude Code session that lives in one repo")).centered(),
        Line::from(dim("and remembers across restarts — hatch one per repo you work in")).centered(),
        Line::from(""),
        Line::from(vec![key(&hatch), muted("hatch your first bird — or click here")]),
        Line::from(vec![key(&help), muted("every key")]),
        Line::from(""),
        Line::from(dim("aviary doctor checks claude, repos, hooks and config")).centered(),
    ];
    let rect = centered(60, lines.len() as u16, area);
    frame.render_widget(ratatui::widgets::Paragraph::new(lines), rect);
}
