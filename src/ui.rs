//! Theme and chrome. The air comes from restraint: one accent, one line of
//! brand, a spacer, the body, and a GENERATED hints row.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::agent_store::BotStatus;
use crate::keymap::{self, Binding};

pub const OK: Color = Color::Indexed(71);
pub const WARN: Color = Color::Indexed(179);
pub const BAD: Color = Color::Indexed(167);
pub const ACCENT: Color = Color::Indexed(214);
pub const MUTED: Color = Color::Indexed(246);
pub const DIM: Color = Color::Indexed(240);

/// Per-bird identity colors, assigned by roster position.
pub const BIRD_COLORS: [Color; 6] = [
    Color::Indexed(75),  // soft blue
    Color::Indexed(176), // soft violet
    Color::Indexed(108), // sage
    Color::Indexed(180), // sand
    Color::Indexed(139), // mauve
    Color::Indexed(73),  // teal
];

pub fn bird_color(idx: usize) -> Color {
    BIRD_COLORS[idx % BIRD_COLORS.len()]
}

pub fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(MUTED))
}

pub fn dim(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(DIM))
}

/// Status chip: shape AND colour, so the roster reads without colour too.
/// "done" = finished responding, waiting at its prompt; "needs you" = blocked
/// on a permission prompt or question (from polls/hooks — the real signal).
pub fn status_span(status: BotStatus) -> Span<'static> {
    match status {
        BotStatus::Working => Span::styled("● working", Style::default().fg(OK)),
        BotStatus::NeedsInput => Span::styled(
            "⏸ needs you",
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ),
        BotStatus::Done(secs) => Span::styled(
            format!("✔ done {}", human_duration(secs)),
            Style::default().fg(MUTED),
        ),
        BotStatus::NotStarted => Span::styled("○ not started", Style::default().fg(DIM)),
        BotStatus::Exited => Span::styled("✗ exited", Style::default().fg(BAD)),
    }
}

/// The chip's glyph alone — tab-strip sized, same shapes and colours.
pub fn status_glyph(status: BotStatus) -> Span<'static> {
    match status {
        BotStatus::Working => Span::styled("●", Style::default().fg(OK)),
        BotStatus::NeedsInput => Span::styled("⏸", Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
        BotStatus::Done(_) => Span::styled("✔", Style::default().fg(MUTED)),
        BotStatus::NotStarted => Span::styled("○", Style::default().fg(DIM)),
        BotStatus::Exited => Span::styled("✗", Style::default().fg(BAD)),
    }
}

pub fn human_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// Greedy word wrap for lines that need their own prefix.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

pub fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

// --------------------------------------------------------------------- chrome

pub fn draw_brand(
    frame: &mut Frame,
    area: Rect,
    birds: usize,
    running: usize,
    flash: Option<&str>,
) {
    let mut spans = vec![Span::styled(
        " AVIARY",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    if let Some(msg) = flash {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(msg.to_string(), Style::default().fg(WARN)));
    }

    let right = if running > 0 {
        format!("● {running} flying · {birds} birds ")
    } else {
        format!("{birds} birds ")
    };
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(used + right.chars().count());
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(if running > 0 {
        Span::styled(right, Style::default().fg(OK))
    } else {
        dim(right)
    });
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The hints row, generated from the active binding tables.
pub fn draw_hints(frame: &mut Frame, area: Rect, tables: &[&[Binding]], override_text: Option<&str>) {
    if let Some(text) = override_text {
        frame.render_widget(Paragraph::new(Line::from(dim(format!(" {text}")))), area);
        return;
    }
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, hint)) in keymap::hints(tables).into_iter().enumerate() {
        if i > 0 {
            spans.push(dim(" · "));
        }
        spans.push(Span::styled(key, Style::default().fg(MUTED)));
        spans.push(dim(format!(" {hint}")));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The help overlay, generated from the same tables that dispatch keys.
pub fn draw_help(frame: &mut Frame, area: Rect, groups: &[(&'static str, &[Binding])]) {
    let mut lines: Vec<Line> = Vec::new();
    for (title, rows) in keymap::help_groups(groups) {
        lines.push(Line::from(Span::styled(
            format!("  {}", title.to_uppercase()),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for (key, help) in rows {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{key:<10}"), Style::default().fg(BIRD_COLORS[0])),
                muted(help),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(dim(
        "  birds are claude sessions named aviary-<id> — they resume by name",
    )));

    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = centered(72, height, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(Span::styled(" keys ", Style::default().fg(ACCENT))),
        ),
        popup,
    );
}
