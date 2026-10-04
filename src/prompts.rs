//! The exact text aviary types into a bird's session — pure functions so the
//! wording (the real interface between the TUI and the agents) is testable.

use crate::config::Bot;

/// Linear/Figma references found in a composer message.
pub struct Links {
    pub linear: Vec<String>,
    pub figma: Vec<String>,
}

/// Detect ticket and design references without a regex dependency:
/// whole-token URL matches plus `ABC-123`-shaped ticket keys.
pub fn detect_links(text: &str) -> Links {
    let mut linear = Vec::new();
    let mut figma = Vec::new();
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != '-');
        if token.contains("linear.app/") {
            linear.push(token.to_string());
        } else if token.contains("figma.com/") {
            figma.push(token.to_string());
        } else if is_ticket_key(token) {
            linear.push(token.to_string());
        }
    }
    Links { linear, figma }
}

/// `ABC-123`: 2+ uppercase letters, a dash, digits — the Linear key shape.
fn is_ticket_key(token: &str) -> bool {
    let Some((team, num)) = token.split_once('-') else {
        return false;
    };
    team.len() >= 2
        && team.chars().all(|c| c.is_ascii_uppercase())
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
}

fn link_instructions(text: &str) -> String {
    let links = detect_links(text);
    let mut out = String::new();
    if !links.linear.is_empty() {
        out.push_str(&format!(
            " Fetch {} via the Linear MCP tools before starting.",
            links.linear.join(", ")
        ));
    }
    if !links.figma.is_empty() {
        out.push_str(&format!(
            " Pull the design context for {} via the Figma MCP tools.",
            links.figma.join(", ")
        ));
    }
    out
}

/// Typed into the SOURCE bot when the user composes a handoff from its thread:
/// the bot owns the packaging, because the bot owns the context.
pub fn handoff(from: &Bot, to: &Bot, message: &str, handoffs_dir: &std::path::Path) -> String {
    format!(
        "Hand off to @{to_session} ({to_name}, resident of {to_repo}): {message}. \
         Package the relevant context — files, endpoints, constraints, decisions already \
         made — and send it to the session named '{to_session}' with SendMessage. If the \
         package is longer than a paragraph, first write a handoff brief to \
         {dir}/<timestamp>-{from_id}-to-{to_id}.md and message them the path.{links}",
        to_session = to.id.session_name(),
        to_name = to.name,
        to_repo = to.repo,
        message = message.trim(),
        dir = handoffs_dir.display(),
        from_id = from.id,
        to_id = to.id,
        links = link_instructions(message),
    )
}

/// Typed straight into the TARGET bot (roster compose — no source bot).
pub fn direct(message: &str) -> String {
    format!("{}{}", message.trim(), link_instructions(message))
}

/// The opener typed into a fresh bird so its first turn grounds itself.
pub fn first_flight(bot: &Bot) -> String {
    format!(
        "You've just been perched. In one short message: who you are, which repo you're \
         sitting in, and the current branch. Then wait — {name} doesn't chatter.",
        name = bot.name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Bot, BotId};

    fn bot(id: &str, name: &str) -> Bot {
        Bot {
            id: BotId(id.into()),
            name: name.into(),
            glyph: "x".into(),
            repo: format!("~/dev/{id}"),
            persona: format!("birds/{id}.md"),
        }
    }

    #[test]
    fn detects_ticket_keys_and_urls() {
        let l = detect_links("see BLA-123 and https://linear.app/blackbird/issue/BLA-9 plus https://www.figma.com/design/abc?node-id=1");
        assert_eq!(l.linear.len(), 2);
        assert_eq!(l.figma.len(), 1);
        // not tickets: lowercase team, no digits, bare words
        let none = detect_links("ab-12 ABC-x grep-1 covered");
        assert!(none.linear.is_empty() && none.figma.is_empty());
    }

    #[test]
    fn handoff_names_target_session_and_brief_path() {
        let s = handoff(
            &bot("swift", "Swift"),
            &bot("raven", "Raven"),
            "GET /v1/fly_balance needs pending_credits — BLA-123",
            std::path::Path::new("/tmp/handoffs"),
        );
        assert!(s.contains("@aviary-raven"));
        assert!(s.contains("SendMessage"));
        assert!(s.contains("/tmp/handoffs/<timestamp>-swift-to-raven.md"));
        assert!(s.contains("Fetch BLA-123 via the Linear MCP tools"));
    }

    #[test]
    fn direct_appends_figma_instruction() {
        let s = direct("match this: https://figma.com/design/xyz");
        assert!(s.contains("Figma MCP"));
        assert!(direct("plain ask") == "plain ask");
    }
}
