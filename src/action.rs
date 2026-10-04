//! What a keystroke MEANS, separated from what key it was.
//!
//! Components receive [`Action`]s (resolved through their keymap tables) and
//! answer with [`Effects`] — commands for the executor and messages for the
//! shell. Generic verbs (`Up`, `Confirm`) are interpreted per component.

use crate::command::Command;
use crate::config::BotId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    // -- global
    Quit,
    Help,
    Back,
    Reload,
    NewBot,
    NewRoom,
    Handoff,
    // -- generic navigation, interpreted per component
    Up,
    Down,
    Top,
    Bottom,
    Confirm,
    // -- thread / agent
    FocusAgent,
    StopBot,
    // -- scrolling panes
    PageUp,
    PageDown,
    // -- room
    Compose,
}

/// Cross-component notifications, fanned out by the shell.
pub enum Msg {
    /// Switch screens. The shell swaps the active component and calls `on_enter`.
    OpenThread(BotId),
    OpenRoom(String),
    /// Open the compose overlay. With a `source`, that bird packages a handoff;
    /// without one the text goes straight to the target.
    Compose {
        source: Option<BotId>,
        preselect: Option<BotId>,
    },
    /// Show a transient status line in the chrome.
    Flash(String),
}

#[derive(Default)]
pub struct Effects {
    pub commands: Vec<Command>,
    pub msgs: Vec<Msg>,
}

impl Effects {
    pub fn msg(&mut self, m: Msg) {
        self.msgs.push(m);
    }

    pub fn command(&mut self, c: Command) {
        self.commands.push(c);
    }

    pub fn flash(&mut self, text: impl Into<String>) {
        self.msgs.push(Msg::Flash(text.into()));
    }
}
