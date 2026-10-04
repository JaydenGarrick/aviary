//! The embedded-terminal pane, as a reusable draw helper.
//!
//! Full width matters: Claude Code reflows to pane width, so the pane is the
//! whole body and renders exactly as it would standalone. Focus is a border +
//! hint change; the one sanctioned draw side effect is the PTY resize, guarded
//! by size-equality inside [`crate::pty::Terminal::resize`].

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use tui_term::widget::PseudoTerminal;

use crate::agent_store::AgentSession;
use crate::ui::{dim, ACCENT, DIM, WARN};

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    title: Line<'static>,
    session: Option<&mut AgentSession>,
    focused: bool,
    empty: Vec<Line<'static>>,
) {
    let border = if focused { ACCENT } else { DIM };
    let hint = if focused {
        " typing goes to the bird · ctrl+a hands the keyboard back "
    } else {
        " a or click to type · esc back to the roster "
    };

    let mut title = title;
    let Some(session) = session else {
        frame.render_widget(
            Paragraph::new(empty)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(DIM))
                        .title(title),
                ),
            area,
        );
        return;
    };

    let back = session.term.scrolled();
    if back > 0 {
        title.push_span(dim("  ·  "));
        title.push_span(Span::styled(
            format!("↑ {back} back — wheel down or type to return"),
            Style::default().fg(WARN),
        ));
    }
    if focused {
        title.push_span(dim("  ·  "));
        title.push_span(Span::styled(
            " keyboard ",
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(title)
        .title_bottom(Line::from(Span::styled(
            hint,
            Style::default().fg(if focused { ACCENT } else { DIM }),
        )));

    let inner = block.inner(area);
    session.term.resize(inner.height, inner.width);

    // Clone the Arc so the read guard does not borrow the session.
    let parser = session.term.parser.clone();
    let Ok(guard) = parser.read() else { return };
    frame.render_widget(PseudoTerminal::new(guard.screen()).block(block), area);
}
