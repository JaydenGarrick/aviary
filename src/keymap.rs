//! One table per component: key → action → hint → help, from a single source.
//!
//! The hints bar and the help overlay are both GENERATED from these tables, so
//! they cannot drift from the real bindings — the failure mode the mb cockpit
//! had with hand-written hint strings.

use crossterm::event::{KeyEvent, KeyModifiers};
pub use crossterm::event::KeyCode;

use crate::action::Action;

/// A key pattern. `Char` patterns match the character exactly (case included)
/// and ignore SHIFT, because terminals deliver `G` as `shift+g`; everything
/// else requires the modifier set to match exactly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyPat {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

pub const fn key(code: KeyCode) -> KeyPat {
    KeyPat {
        code,
        mods: KeyModifiers::NONE,
    }
}

pub const fn ch(c: char) -> KeyPat {
    key(KeyCode::Char(c))
}

impl KeyPat {
    pub fn matches(&self, k: &KeyEvent) -> bool {
        if self.code != k.code {
            return false;
        }
        match self.code {
            KeyCode::Char(_) => {
                // SHIFT is already encoded in the char's case.
                let relevant = k.modifiers - KeyModifiers::SHIFT;
                relevant == self.mods
            }
            _ => k.modifiers == self.mods,
        }
    }

    /// How the key is shown in hints and help ("⏎", "ctrl+a", "G").
    pub fn label(&self) -> String {
        let base = match self.code {
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "⏎".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Tab => "⇥".into(),
            KeyCode::Up => "↑".into(),
            KeyCode::Down => "↓".into(),
            KeyCode::Left => "←".into(),
            KeyCode::Right => "→".into(),
            KeyCode::PageUp => "pgup".into(),
            KeyCode::PageDown => "pgdn".into(),
            other => format!("{other:?}").to_lowercase(),
        };
        if self.mods.contains(KeyModifiers::CONTROL) {
            format!("ctrl+{base}")
        } else {
            base
        }
    }
}

/// One binding. `hint` is the short label for the bottom bar (None = help
/// overlay only); `help` is the full description.
pub struct Binding {
    pub pat: KeyPat,
    /// Extra patterns that mean the same thing (arrow-key aliases). Aliases
    /// never appear in hints — the primary pattern is the advertised one.
    pub aliases: &'static [KeyPat],
    pub action: Action,
    pub hint: Option<&'static str>,
    pub help: &'static str,
}

pub const fn bind(
    pat: KeyPat,
    action: Action,
    hint: Option<&'static str>,
    help: &'static str,
) -> Binding {
    Binding {
        pat,
        aliases: &[],
        action,
        hint,
        help,
    }
}

pub const fn bind_alias(
    pat: KeyPat,
    aliases: &'static [KeyPat],
    action: Action,
    hint: Option<&'static str>,
    help: &'static str,
) -> Binding {
    Binding {
        pat,
        aliases,
        action,
        hint,
        help,
    }
}

/// First matching binding wins; the shell consults tables in precedence order
/// (active component before globals), so shadowing falls out of call order.
pub fn resolve(tables: &[&[Binding]], k: &KeyEvent) -> Option<Action> {
    for table in tables {
        for b in *table {
            if b.pat.matches(k) || b.aliases.iter().any(|a| a.matches(k)) {
                return Some(b.action);
            }
        }
    }
    None
}

/// The hints line: every hinted binding, component table first.
pub fn hints(tables: &[&[Binding]]) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    for table in tables {
        for b in *table {
            if let Some(hint) = b.hint {
                out.push((b.pat.label(), hint));
            }
        }
    }
    out
}

/// Grouped rows for the help overlay: (group title, [(key label, help)]).
pub fn help_groups(groups: &[(&'static str, &[Binding])]) -> Vec<(String, Vec<(String, String)>)> {
    groups
        .iter()
        .map(|(title, table)| {
            let rows = table
                .iter()
                .map(|b| (b.pat.label(), b.help.to_string()))
                .collect();
            (title.to_string(), rows)
        })
        .collect()
}

// -------------------------------------------------------------------- global

use Action as A;

/// Bindings live everywhere a component table doesn't shadow them.
pub static GLOBAL: &[Binding] = &[
    bind(ch('q'), A::Quit, None, "quit aviary (birds keep flying — sessions resume by name)"),
    bind(ch('?'), A::Help, Some("keys"), "help overlay"),
    bind(ch('n'), A::NewBot, None, "hatch a bird (new repo bot)"),
    // `c`, not `g` — the sidebar's g (jump to top) would shadow it.
    bind(ch('c'), A::NewRoom, None, "create a room (group chat)"),
    bind(ch('r'), A::Reload, None, "reload config + repo state"),
    bind(key(KeyCode::Esc), A::Back, None, "hand the keyboard back to the sidebar"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    const fn ctrl(c: char) -> KeyPat {
        KeyPat {
            code: KeyCode::Char(c),
            mods: KeyModifiers::CONTROL,
        }
    }

    static COMPONENT: &[Binding] = &[
        bind(ch('q'), A::Back, None, "back (shadows global quit)"),
        bind_alias(
            ch('j'),
            &[key(KeyCode::Down)],
            A::Down,
            Some("move"),
            "move down",
        ),
        bind(ch('G'), A::Bottom, None, "bottom"),
        bind(ctrl('a'), A::FocusAgent, None, "focus"),
    ];

    #[test]
    fn component_shadows_global() {
        let k = press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(resolve(&[COMPONENT, GLOBAL], &k), Some(A::Back));
        assert_eq!(resolve(&[GLOBAL], &k), Some(A::Quit));
    }

    #[test]
    fn aliases_match_but_do_not_hint() {
        let down = press(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(resolve(&[COMPONENT], &down), Some(A::Down));
        let hinted = hints(&[COMPONENT]);
        assert_eq!(hinted.len(), 1);
        assert_eq!(hinted[0].0, "j");
    }

    #[test]
    fn shift_char_matches_exact_case() {
        // Terminals report 'G' with the SHIFT modifier set.
        let shift_g = press(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(resolve(&[COMPONENT], &shift_g), Some(A::Bottom));
        let plain_g = press(KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(resolve(&[COMPONENT], &plain_g), None);
    }

    #[test]
    fn ctrl_requires_modifier() {
        let plain = press(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(resolve(&[COMPONENT], &plain), None);
        let ctrl_a = press(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(resolve(&[COMPONENT], &ctrl_a), Some(A::FocusAgent));
    }

    #[test]
    fn unbound_falls_through() {
        let k = press(KeyCode::Char('z'), KeyModifiers::NONE);
        assert_eq!(resolve(&[COMPONENT, GLOBAL], &k), None);
    }
}
