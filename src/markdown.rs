//! Markdown → styled lines for the room transcript. Pure: text + width in,
//! `Line`s out, so the layout is unit-tested like `room.rs` and `prompts.rs`.
//!
//! Hand-rolled on purpose: birds write regular GFM (tables, bold, inline
//! code, lists, fences), and the half that needs care — width-aware wrapping
//! of styled text and table columns — is ours under any parser. A line the
//! classifier doesn't recognise renders as plain prose, never panics, never
//! drops characters.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The palette the renderer paints with. One per surface; the room builds
/// it from the theme constants in `ui`.
pub struct Theme {
    pub text: Style,
    pub strong: Style,
    pub code: Style,
    pub heading: Style,
    /// Rules, table borders, list markers, the code bar.
    pub quiet: Style,
    pub link: Style,
}

impl Theme {
    pub fn aviary() -> Theme {
        use crate::ui::{ACCENT, DIM, MUTED};
        Theme {
            text: Style::default().fg(MUTED),
            strong: Style::default().fg(Color::Reset).add_modifier(Modifier::BOLD),
            code: Style::default().fg(Color::Indexed(187)),
            heading: Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            quiet: Style::default().fg(DIM),
            link: Style::default().fg(ACCENT).add_modifier(Modifier::UNDERLINED),
        }
    }
}

/// Columns a table cell may shrink to before the table stacks instead.
const MIN_COL: usize = 4;
/// `" │ "` between columns.
const COL_GAP: usize = 3;

/// Render one message body at `width` columns. `mention` maps a bare id
/// (`swift`, `jayden.garrick`) to a colour; `None` renders the mention strong.
pub fn render(
    body: &str,
    width: usize,
    theme: &Theme,
    mention: &dyn Fn(&str) -> Option<Color>,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut out: Vec<Line<'static>> = Vec::new();
    for block in blocks(body) {
        match block {
            Block::Blank => out.push(Line::from("")),
            Block::Rule => out.push(Line::from(Span::styled("─".repeat(width), theme.quiet))),
            Block::Heading(text) => {
                let runs = restyle(runs(&text, theme, mention), theme.heading);
                out.extend(wrap_styled(&runs, width).into_iter().map(Line::from));
            }
            Block::Paragraph(text) => {
                out.extend(
                    wrap_styled(&runs(&text, theme, mention), width)
                        .into_iter()
                        .map(Line::from),
                );
            }
            Block::Quote(text) => {
                for spans in wrap_styled(&runs(&text, theme, mention), width.saturating_sub(2)) {
                    let mut line = vec![Span::styled("▏ ", theme.quiet)];
                    line.extend(spans);
                    out.push(Line::from(line));
                }
            }
            Block::Code(lines) => {
                for code in lines {
                    out.push(Line::from(vec![
                        Span::styled("▏ ", theme.quiet),
                        Span::styled(code.replace('\t', "    "), theme.code),
                    ]));
                }
            }
            Block::Item { indent, marker, text } => {
                let lead = format!("{}{} ", " ".repeat(indent), marker);
                let hang = lead.width();
                let body = wrap_styled(&runs(&text, theme, mention), width.saturating_sub(hang));
                if body.is_empty() {
                    out.push(Line::from(Span::styled(lead.clone(), theme.quiet)));
                }
                for (i, spans) in body.into_iter().enumerate() {
                    let mut line = vec![if i == 0 {
                        Span::styled(lead.clone(), theme.quiet)
                    } else {
                        Span::raw(" ".repeat(hang))
                    }];
                    line.extend(spans);
                    out.push(Line::from(line));
                }
            }
            Block::Table { header, align, rows } => {
                out.extend(table_lines(&header, &align, &rows, width, theme, mention));
            }
        }
    }
    out
}

// -------------------------------------------------------------------- blocks

#[derive(Debug, PartialEq, Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, PartialEq)]
enum Block {
    Blank,
    Rule,
    Heading(String),
    Paragraph(String),
    Quote(String),
    Code(Vec<String>),
    Item {
        indent: usize,
        marker: String,
        text: String,
    },
    Table {
        header: Vec<String>,
        align: Vec<Align>,
        rows: Vec<Vec<String>>,
    },
}

fn fence_of(s: &str) -> Option<&str> {
    if s.starts_with("```") {
        Some("```")
    } else if s.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn heading_text(s: &str) -> Option<&str> {
    let hashes = s.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &s[hashes..];
        if rest.starts_with(' ') {
            return Some(rest.trim());
        }
    }
    None
}

fn is_rule(s: &str) -> bool {
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    chars.len() >= 3 && matches!(chars[0], '-' | '*' | '_') && chars.iter().all(|c| *c == chars[0])
}

/// `- ` · `* ` · `+ ` · task boxes · `1. ` · `1) ` → (indent, marker, text).
fn list_item(raw: &str) -> Option<(usize, String, String)> {
    let indent = raw.len() - raw.trim_start().len();
    let s = raw.trim_start();
    for (prefix, marker) in [("- [ ] ", "☐"), ("- [x] ", "☑"), ("- [X] ", "☑")] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return Some((indent, marker.into(), rest.trim().into()));
        }
    }
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return Some((indent, "•".into(), rest.trim().into()));
        }
    }
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let rest = &s[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((indent, format!("{}.", &s[..digits]), rest.trim().into()));
        }
    }
    None
}

/// Split a table row into trimmed cells. One outer pipe on each side is
/// optional; pipes inside backticks belong to the code, not the grid.
fn cells(row: &str) -> Vec<String> {
    let mut s = row.trim();
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    if let Some(rest) = s.strip_suffix('|') {
        s = rest;
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    for c in s.chars() {
        match c {
            '`' => {
                in_code = !in_code;
                cur.push(c);
            }
            '|' if !in_code => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

fn is_table_row(s: &str) -> bool {
    s.contains('|') && cells(s).len() >= 2 || s.starts_with('|') && s.ends_with('|') && s.len() > 2
}

/// `|---|:---:|---:|` → alignments, or None when any cell isn't a dash run.
fn separator_aligns(s: &str) -> Option<Vec<Align>> {
    let cells = cells(s);
    if cells.is_empty() || !s.contains('-') {
        return None;
    }
    cells
        .iter()
        .map(|c| {
            let c = c.trim();
            let left = c.starts_with(':');
            let right = c.ends_with(':');
            let dashes = c.trim_matches(':');
            if dashes.is_empty() || !dashes.chars().all(|ch| ch == '-') {
                return None;
            }
            Some(match (left, right) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// A line that opens a block of its own, so a paragraph must stop before it.
fn starts_block(lines: &[&str], i: usize) -> bool {
    let s = lines[i].trim();
    s.is_empty()
        || fence_of(s).is_some()
        || heading_text(s).is_some()
        || is_rule(s)
        || s.starts_with('>')
        || list_item(lines[i]).is_some()
        || (i + 1 < lines.len() && is_table_row(s) && separator_aligns(lines[i + 1]).is_some())
}

fn blocks(body: &str) -> Vec<Block> {
    let lines: Vec<&str> = body.lines().collect();
    let mut out: Vec<Block> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let s = raw.trim();

        if let Some(fence) = fence_of(s) {
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with(fence) {
                code.push(lines[i].to_string());
                i += 1;
            }
            i += 1; // the closing fence (or past the end)
            out.push(Block::Code(code));
            continue;
        }
        if s.is_empty() {
            if out.last() != Some(&Block::Blank) {
                out.push(Block::Blank);
            }
            i += 1;
            continue;
        }
        if let Some(text) = heading_text(s) {
            out.push(Block::Heading(text.to_string()));
            i += 1;
            continue;
        }
        if is_rule(s) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if i + 1 < lines.len() && is_table_row(s) {
            if let Some(align) = separator_aligns(lines[i + 1]) {
                let header = cells(s);
                let mut rows = Vec::new();
                i += 2;
                while i < lines.len() && !lines[i].trim().is_empty() && is_table_row(lines[i].trim()) {
                    rows.push(cells(lines[i]));
                    i += 1;
                }
                out.push(Block::Table { header, align, rows });
                continue;
            }
        }
        if let Some((indent, marker, mut text)) = list_item(raw) {
            i += 1;
            // Indented continuation lines belong to the item.
            while i < lines.len() {
                let next = lines[i];
                let next_indent = next.len() - next.trim_start().len();
                if next.trim().is_empty() || next_indent <= indent || starts_block(&lines, i) {
                    break;
                }
                text.push(' ');
                text.push_str(next.trim());
                i += 1;
            }
            out.push(Block::Item { indent, marker, text });
            continue;
        }
        if let Some(q) = s.strip_prefix('>') {
            let mut text = q.trim().to_string();
            i += 1;
            while i < lines.len() {
                let Some(more) = lines[i].trim().strip_prefix('>') else { break };
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(more.trim());
                i += 1;
            }
            out.push(Block::Quote(text));
            continue;
        }
        // Paragraph: soft-break join until something else starts.
        let mut text = s.to_string();
        i += 1;
        while i < lines.len() && !starts_block(&lines, i) {
            text.push(' ');
            text.push_str(lines[i].trim());
            i += 1;
        }
        out.push(Block::Paragraph(text));
    }
    out
}

// -------------------------------------------------------------------- inline

#[derive(Debug, PartialEq, Clone)]
enum Tok {
    Text(String),
    Strong(String),
    Code(String),
    /// The bare id, without `@`.
    Mention(String),
    /// Link text; the URL is dropped from the render.
    Link(String),
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// How much of `s` is a mention name: word chars plus interior dots, so
/// `@jayden.garrick` (a login name) is one mention while `@raven.` ends
/// before the full stop. Mirrors `room::mentions`.
fn mention_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut n = 0;
    while n < b.len() {
        let c = b[n] as char;
        let interior_dot = c == '.' && b.get(n + 1).is_some_and(|&next| is_word(next as char));
        if is_word(c) || interior_dot {
            n += 1;
        } else {
            break;
        }
    }
    n
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = s;
    let mut prev: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        let after = &rest[c.len_utf8()..];
        let mut consumed: Option<(Tok, usize)> = None;
        match c {
            '*' if after.starts_with('*') => {
                let inner = &rest[2..];
                if let Some(end) = inner.find("**") {
                    if end > 0 {
                        consumed = Some((Tok::Strong(inner[..end].to_string()), 2 + end + 2));
                    }
                }
            }
            '`' => {
                if let Some(end) = after.find('`') {
                    if end > 0 {
                        consumed = Some((Tok::Code(after[..end].to_string()), 1 + end + 1));
                    }
                }
            }
            '@' if !prev.is_some_and(|p| p.is_ascii_alphanumeric()) => {
                let n = mention_len(after);
                if n > 0 {
                    consumed = Some((Tok::Mention(after[..n].to_string()), 1 + n));
                }
            }
            '[' => {
                if let Some(mid) = after.find("](") {
                    let url_start = mid + 2;
                    if let Some(end) = after[url_start..].find(')') {
                        let label = &after[..mid];
                        let url = &after[url_start..url_start + end];
                        if !label.is_empty() && !url.is_empty() && !label.contains('\n') {
                            consumed = Some((Tok::Link(label.to_string()), 1 + url_start + end + 1));
                        }
                    }
                }
            }
            _ => {}
        }
        match consumed {
            Some((tok, len)) => {
                if !text.is_empty() {
                    out.push(Tok::Text(std::mem::take(&mut text)));
                }
                out.push(tok);
                prev = rest[..len].chars().last();
                rest = &rest[len..];
            }
            None => {
                text.push(c);
                prev = Some(c);
                rest = after;
            }
        }
    }
    if !text.is_empty() {
        out.push(Tok::Text(text));
    }
    out
}

/// Tokens → styled runs. Strong re-tokenises its inside one level so
/// `**see `x` @raven**` keeps its code and mention styling, bolded.
fn runs(text: &str, theme: &Theme, mention: &dyn Fn(&str) -> Option<Color>) -> Vec<(String, Style)> {
    fn push(out: &mut Vec<(String, Style)>, toks: Vec<Tok>, bold: bool, theme: &Theme, mention: &dyn Fn(&str) -> Option<Color>) {
        let b = |s: Style| if bold { s.add_modifier(Modifier::BOLD) } else { s };
        for tok in toks {
            match tok {
                Tok::Text(t) => out.push((t, if bold { theme.strong } else { theme.text })),
                Tok::Code(t) => out.push((t, b(theme.code))),
                Tok::Link(t) => out.push((t, b(theme.link))),
                Tok::Mention(name) => {
                    let bare = name.strip_prefix("aviary-").unwrap_or(&name);
                    let style = match mention(&bare.to_lowercase()) {
                        Some(colour) => Style::default().fg(colour).add_modifier(Modifier::BOLD),
                        None => theme.strong,
                    };
                    out.push((format!("@{name}"), style));
                }
                Tok::Strong(inner) => {
                    if bold {
                        out.push((inner, theme.strong));
                    } else {
                        push(out, tokenize(&inner), true, theme, mention);
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    push(&mut out, tokenize(text), false, theme, mention);
    out
}

/// Force one style over every run (headings).
fn restyle(runs: Vec<(String, Style)>, style: Style) -> Vec<(String, Style)> {
    runs.into_iter().map(|(t, _)| (t, style)).collect()
}

fn runs_width(runs: &[(String, Style)]) -> usize {
    runs.iter().map(|(t, _)| t.width()).sum()
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

// ---------------------------------------------------------------------- wrap

/// Greedy fill by DISPLAY width across styled runs. A word wider than
/// `width` is hard-broken by glyph; leading spaces on a wrapped line and
/// trailing spaces before a break are dropped; neighbours that share a style
/// merge into one span.
fn wrap_styled(runs: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    // Pieces: alternating word / space chunks, each carrying its style.
    let mut pieces: Vec<(String, Style, bool)> = Vec::new(); // (text, style, is_space)
    for (text, style) in runs {
        let mut cur = String::new();
        let mut cur_space: Option<bool> = None;
        for c in text.chars() {
            let sp = c.is_whitespace();
            if cur_space.is_some_and(|s| s != sp) {
                pieces.push((std::mem::take(&mut cur), *style, cur_space.unwrap()));
            }
            cur_space = Some(sp);
            cur.push(if sp { ' ' } else { c });
        }
        if !cur.is_empty() {
            pieces.push((cur, *style, cur_space.unwrap_or(false)));
        }
    }

    let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
    let mut line: Vec<(String, Style)> = Vec::new();
    let mut used = 0usize;
    let flush = |line: &mut Vec<(String, Style)>, used: &mut usize, lines: &mut Vec<Vec<(String, Style)>>| {
        while line.last().is_some_and(|(t, _)| t.trim().is_empty()) {
            line.pop();
        }
        if let Some((t, _)) = line.last_mut() {
            let trimmed = t.trim_end().to_string();
            *t = trimmed;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(line));
        }
        *used = 0;
    };

    for (text, style, is_space) in pieces {
        let w = text.width();
        if is_space {
            if used == 0 {
                continue; // no leading spaces on a line
            }
            if used + w > width {
                flush(&mut line, &mut used, &mut lines);
            } else {
                line.push((text, style));
                used += w;
            }
            continue;
        }
        if used + w <= width {
            line.push((text, style));
            used += w;
            continue;
        }
        if used > 0 {
            flush(&mut line, &mut used, &mut lines);
        }
        if w <= width {
            line.push((text, style));
            used = w;
            continue;
        }
        // Hard-break an over-wide word by glyph.
        let mut chunk = String::new();
        let mut cw = 0usize;
        for c in text.chars() {
            let gw = c.width().unwrap_or(0);
            if cw + gw > width && !chunk.is_empty() {
                line.push((std::mem::take(&mut chunk), style));
                flush(&mut line, &mut used, &mut lines);
                cw = 0;
            }
            chunk.push(c);
            cw += gw;
        }
        if !chunk.is_empty() {
            line.push((chunk, style));
            used = cw;
        }
    }
    flush(&mut line, &mut used, &mut lines);

    lines
        .into_iter()
        .map(|parts| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (t, style) in parts {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push_str(&t),
                    _ => spans.push(Span::styled(t, style)),
                }
            }
            spans
        })
        .collect()
}

// --------------------------------------------------------------------- table

/// Shave the widest column by one until the row fits — water-filling from
/// the top, so columns end up near-equal under pressure. None when even
/// `min` per column overflows: the table should stack instead.
fn squeeze(natural: &[usize], avail: usize, min: usize) -> Option<Vec<usize>> {
    if natural.len() * min > avail {
        return None;
    }
    let mut w = natural.to_vec();
    let mut total: usize = w.iter().sum();
    while total > avail {
        let (i, _) = w
            .iter()
            .enumerate()
            .filter(|(_, &x)| x > min)
            .max_by_key(|(_, &x)| x)?;
        w[i] -= 1;
        total -= 1;
    }
    Some(w)
}

fn pad(spans: &mut Vec<Span<'static>>, width: usize, align: Align) {
    let w = spans_width(spans);
    let gap = width.saturating_sub(w);
    if gap == 0 {
        return;
    }
    match align {
        Align::Left => spans.push(Span::raw(" ".repeat(gap))),
        Align::Right => spans.insert(0, Span::raw(" ".repeat(gap))),
        Align::Center => {
            spans.insert(0, Span::raw(" ".repeat(gap / 2)));
            spans.push(Span::raw(" ".repeat(gap - gap / 2)));
        }
    }
}

fn table_lines(
    header: &[String],
    align: &[Align],
    rows: &[Vec<String>],
    width: usize,
    theme: &Theme,
    mention: &dyn Fn(&str) -> Option<Color>,
) -> Vec<Line<'static>> {
    let cols = header.len();
    if cols == 0 {
        return Vec::new();
    }
    let normalise = |row: &[String]| -> Vec<Vec<(String, Style)>> {
        (0..cols)
            .map(|c| runs(row.get(c).map(String::as_str).unwrap_or(""), theme, mention))
            .collect()
    };
    let head: Vec<Vec<(String, Style)>> = normalise(header)
        .into_iter()
        .map(|r| r.into_iter().map(|(t, s)| (t, s.add_modifier(Modifier::BOLD))).collect())
        .collect();
    let body: Vec<Vec<Vec<(String, Style)>>> = rows.iter().map(|r| normalise(r)).collect();
    let align_of = |c: usize| align.get(c).copied().unwrap_or(Align::Left);

    let mut natural = vec![1usize; cols];
    for row in std::iter::once(&head).chain(body.iter()) {
        for (c, cell) in row.iter().enumerate() {
            natural[c] = natural[c].max(runs_width(cell)).max(1);
        }
    }
    let avail = width.saturating_sub(COL_GAP * (cols - 1));
    let widths = if natural.iter().sum::<usize>() <= avail {
        natural
    } else {
        match squeeze(&natural, avail, MIN_COL) {
            Some(w) => w,
            None => return stacked(&head, &body, width, theme),
        }
    };

    let emit_row = |cells: &[Vec<(String, Style)>], out: &mut Vec<Line<'static>>| {
        let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
            .iter()
            .enumerate()
            .map(|(c, runs)| wrap_styled(runs, widths[c]))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(0).max(1);
        for r in 0..height {
            let mut line: Vec<Span<'static>> = Vec::new();
            for c in 0..cols {
                let mut cell = wrapped[c].get(r).cloned().unwrap_or_default();
                // The last left-aligned column needs no trailing pad — it
                // only adds spaces to what a drag-copy picks up.
                if c + 1 < cols || align_of(c) != Align::Left {
                    pad(&mut cell, widths[c], align_of(c));
                }
                line.extend(cell);
                if c + 1 < cols {
                    line.push(Span::styled(" │ ", theme.quiet));
                }
            }
            out.push(Line::from(line));
        }
    };

    let mut out = Vec::new();
    emit_row(&head, &mut out);
    let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    out.push(Line::from(Span::styled(rule.join("─┼─"), theme.quiet)));
    for row in &body {
        emit_row(row, &mut out);
    }
    out
}

/// Too narrow for columns: each row becomes `header: value` lines.
fn stacked(
    head: &[Vec<(String, Style)>],
    body: &[Vec<Vec<(String, Style)>>],
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let labels: Vec<String> = head
        .iter()
        .map(|cell| cell.iter().map(|(t, _)| t.as_str()).collect::<String>())
        .collect();
    let mut out = Vec::new();
    for (i, row) in body.iter().enumerate() {
        if i > 0 {
            out.push(Line::from(""));
        }
        for (c, cell) in row.iter().enumerate() {
            let label = format!("{}: ", labels.get(c).map(String::as_str).unwrap_or(""));
            let hang = label.width().min(width / 2);
            let wrapped = wrap_styled(cell, width.saturating_sub(hang));
            if wrapped.is_empty() {
                out.push(Line::from(Span::styled(label, theme.quiet)));
                continue;
            }
            for (r, spans) in wrapped.into_iter().enumerate() {
                let mut line = vec![if r == 0 {
                    Span::styled(label.clone(), theme.quiet)
                } else {
                    Span::raw(" ".repeat(hang))
                }];
                line.extend(spans);
                out.push(Line::from(line));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::aviary()
    }

    fn no_mention(_: &str) -> Option<Color> {
        None
    }

    fn text_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    // ---------------------------------------------------------------- inline

    #[test]
    fn inline_tokens_bold_code_mention_link() {
        let toks = tokenize("see `ListVenue.yml` and **PR A** for @raven, docs [here](https://x.y/z).");
        assert_eq!(
            toks,
            vec![
                Tok::Text("see ".into()),
                Tok::Code("ListVenue.yml".into()),
                Tok::Text(" and ".into()),
                Tok::Strong("PR A".into()),
                Tok::Text(" for ".into()),
                Tok::Mention("raven".into()),
                Tok::Text(", docs ".into()),
                Tok::Link("here".into()),
                Tok::Text(".".into()),
            ]
        );
    }

    #[test]
    fn unmatched_markers_stay_literal_and_emails_are_not_mentions() {
        assert_eq!(tokenize("a ** b ` c"), vec![Tok::Text("a ** b ` c".into())]);
        assert_eq!(tokenize("mail a@b.c"), vec![Tok::Text("mail a@b.c".into())]);
        assert_eq!(tokenize("[not a link]"), vec![Tok::Text("[not a link]".into())]);
        // An empty code span is two literal backticks.
        assert_eq!(tokenize("x `` y"), vec![Tok::Text("x `` y".into())]);
    }

    #[test]
    fn strong_keeps_inner_code_and_mentions_bold() {
        let th = theme();
        let r = runs("**ask `x` of @swift**", &th, &|id| (id == "swift").then_some(Color::Blue));
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].0, "ask ");
        assert!(r[0].1.add_modifier.contains(Modifier::BOLD));
        assert_eq!(r[1].0, "x");
        assert_eq!(r[1].1.fg, th.code.fg);
        assert!(r[1].1.add_modifier.contains(Modifier::BOLD));
        assert_eq!(r[3].0, "@swift");
        assert_eq!(r[3].1.fg, Some(Color::Blue));
    }

    #[test]
    fn dotted_login_names_mention_whole_and_a_full_stop_ends_one() {
        assert_eq!(
            tokenize("cc @jayden.garrick and @night_jar. done"),
            vec![
                Tok::Text("cc ".into()),
                Tok::Mention("jayden.garrick".into()),
                Tok::Text(" and ".into()),
                Tok::Mention("night_jar".into()),
                Tok::Text(". done".into()),
            ]
        );
        let th = theme();
        let colour = |id: &str| (id == "jayden.garrick").then_some(Color::Cyan);
        let r = runs("@jayden.garrick", &th, &colour);
        assert_eq!(r[0].0, "@jayden.garrick");
        assert_eq!(r[0].1.fg, Some(Color::Cyan));
    }

    #[test]
    fn mention_colour_strips_the_session_prefix() {
        let th = theme();
        let r = runs("@aviary-raven", &th, &|id| (id == "raven").then_some(Color::Red));
        assert_eq!(r[0].0, "@aviary-raven");
        assert_eq!(r[0].1.fg, Some(Color::Red));
    }

    // ------------------------------------------------------------------ wrap

    #[test]
    fn wrap_counts_display_width_and_breaks_on_spaces() {
        let th = theme();
        let runs = vec![("🟢 go 🟢 go 🟢 go".to_string(), th.text)];
        let lines = wrap_styled(&runs, 10);
        let out: Vec<String> = lines.iter().map(|l| l.iter().map(|s| s.content.as_ref()).collect()).collect();
        // Each "🟢 go" is 5 cells; two fit in 10 only without the joining space.
        assert_eq!(out, vec!["🟢 go 🟢", "go 🟢 go"]);
    }

    #[test]
    fn wrap_hard_breaks_an_overwide_token_and_merges_styles() {
        let th = theme();
        let runs = vec![
            ("abcdefghijkl".to_string(), th.text),
            ("mn".to_string(), th.text),
        ];
        let lines = wrap_styled(&runs, 5);
        let out: Vec<String> = lines.iter().map(|l| l.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert_eq!(out, vec!["abcde", "fghij", "klmn"]);
        // Same-style neighbours merged into one span.
        assert_eq!(lines[2].len(), 1);
    }

    // ---------------------------------------------------------------- blocks

    #[test]
    fn classifier_covers_every_block_kind() {
        let body = "## Plan\n\
                    first line\n\
                    continues here\n\
                    \n\
                    - one\n  wraps on\n\
                    - [x] two\n\
                    1. three\n\
                    > quoted\n> more\n\
                    ---\n\
                    ```sh\n# not a heading\n| not | a | table |\n```\n\
                    | a | b |\n|---|:-:|\n| 1 | 2 |\n\
                    tail";
        let b = blocks(body);
        assert_eq!(b[0], Block::Heading("Plan".into()));
        assert_eq!(b[1], Block::Paragraph("first line continues here".into()));
        assert_eq!(b[2], Block::Blank);
        assert_eq!(
            b[3],
            Block::Item { indent: 0, marker: "•".into(), text: "one wraps on".into() }
        );
        assert_eq!(b[4], Block::Item { indent: 0, marker: "☑".into(), text: "two".into() });
        assert_eq!(b[5], Block::Item { indent: 0, marker: "1.".into(), text: "three".into() });
        assert_eq!(b[6], Block::Quote("quoted more".into()));
        assert_eq!(b[7], Block::Rule);
        assert_eq!(
            b[8],
            Block::Code(vec!["# not a heading".into(), "| not | a | table |".into()])
        );
        assert_eq!(
            b[9],
            Block::Table {
                header: vec!["a".into(), "b".into()],
                align: vec![Align::Left, Align::Center],
                rows: vec![vec!["1".into(), "2".into()]],
            }
        );
        assert_eq!(b[10], Block::Paragraph("tail".into()));
    }

    #[test]
    fn a_pipe_line_without_a_separator_is_prose() {
        let b = blocks("either | or\nnext line");
        assert_eq!(b, vec![Block::Paragraph("either | or next line".into())]);
    }

    #[test]
    fn cells_respect_inline_code_and_optional_outer_pipes() {
        assert_eq!(cells("| a | `x | y` | c |"), vec!["a", "`x | y`", "c"]);
        assert_eq!(cells("a | b"), vec!["a", "b"]);
        assert_eq!(cells("|---|---|"), vec!["---", "---"]);
    }

    #[test]
    fn unterminated_fence_swallows_the_rest_without_panicking() {
        let b = blocks("```\ncode\nmore");
        assert_eq!(b, vec![Block::Code(vec!["code".into(), "more".into()])]);
    }

    // ----------------------------------------------------------------- table

    #[test]
    fn squeeze_shaves_the_widest_first_and_gives_up_below_min() {
        assert_eq!(squeeze(&[3, 20, 7], 30, 4), Some(vec![3, 20, 7]));
        // 30 → 24: all six come off the widest column.
        assert_eq!(squeeze(&[3, 20, 7], 24, 4), Some(vec![3, 14, 7]));
        // Pressure equalises the top: 20 and 7 meet before 3 is touched.
        assert_eq!(squeeze(&[3, 20, 7], 15, 4), Some(vec![3, 6, 6]));
        assert_eq!(squeeze(&[3, 20, 7], 11, 4), None);
    }

    #[test]
    fn table_fits_naturally_and_pads_columns() {
        let body = "| # | Question | Gate |\n|---|---|---:|\n| 1 | **save**? | PR A |\n| 10 | map | — |";
        let lines = render(body, 60, &theme(), &no_mention);
        let t = texts(&lines);
        assert_eq!(t[0], "#  │ Question │ Gate");
        assert_eq!(t[1], "───┼──────────┼─────");
        assert_eq!(t[2], "1  │ save?    │ PR A");
        assert_eq!(t[3], "10 │ map      │    —");
        // Header cells are bold.
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn table_squeezes_and_wraps_cells_to_the_pane() {
        let body = "| k | value |\n|---|---|\n| a | one two three four five six |";
        let lines = render(body, 20, &theme(), &no_mention);
        let t = texts(&lines);
        // 20 − 3 gap = 17; natural (1, 28) → (1, 16): the long cell wraps.
        assert_eq!(t[0], "k │ value");
        assert!(t[2].starts_with("a │ one two three"));
        assert_eq!(t.len(), 4, "the value column wraps onto a second row line");
        assert!(t[3].starts_with("  │ four five six"));
        assert!(t.iter().all(|l| l.width() <= 20), "{t:?}");
    }

    #[test]
    fn table_too_narrow_for_columns_stacks_rows() {
        let body = "| alpha | beta | gamma | delta |\n|---|---|---|---|\n| 1 | 2 | 3 | 4 |\n| 5 | 6 | 7 | 8 |";
        // 4 columns × MIN 4 + 3 gaps × 3 = 25 > 16: stacked.
        let lines = render(body, 16, &theme(), &no_mention);
        let t = texts(&lines);
        assert_eq!(t[0], "alpha: 1");
        assert_eq!(t[3], "delta: 4");
        assert_eq!(t[4], "");
        assert_eq!(t[5], "alpha: 5");
    }

    #[test]
    fn short_and_long_rows_normalise_to_the_header() {
        let body = "| a | b |\n|---|---|\n| 1 |\n| 2 | 3 | 4 |";
        let t = texts(&render(body, 40, &theme(), &no_mention));
        assert_eq!(t[2], "1 │ ");
        assert_eq!(t[3], "2 │ 3");
    }

    // ---------------------------------------------------------------- render

    #[test]
    fn lists_hang_their_continuation_under_the_text() {
        let body = "- alpha beta gamma delta\n  1. nested item";
        let t = texts(&render(body, 14, &theme(), &no_mention));
        assert_eq!(t[0], "• alpha beta");
        assert_eq!(t[1], "  gamma delta");
        assert_eq!(t[2], "  1. nested");
        assert_eq!(t[3], "     item");
    }

    #[test]
    fn code_blocks_are_verbatim_behind_a_bar_and_quotes_wrap() {
        let body = "```\nlet x = a | b;  **not bold**\n```\n> a quote that wraps";
        let lines = render(body, 14, &theme(), &no_mention);
        let t = texts(&lines);
        assert_eq!(t[0], "▏ let x = a | b;  **not bold**");
        assert_eq!(t[1], "▏ a quote that");
        assert_eq!(t[2], "▏ wraps");
    }

    #[test]
    fn nothing_is_lost_on_odd_input() {
        for body in ["", "|", "| |", "**", "```", "- ", "1. ", "> ", "[](", "####### seven"] {
            let _ = render(body, 20, &theme(), &no_mention);
        }
        let t = texts(&render("####### seven", 40, &theme(), &no_mention));
        assert_eq!(t, vec!["####### seven"]);
    }
}
