//! What a keystroke MEANS, separated from what key it was.
//!
//! Components receive [`Action`]s (resolved through their keymap tables) and
//! answer with [`Effects`] — commands for the executor and messages for the
//! shell. Generic verbs (`Up`, `Confirm`) are interpreted per component.

use crate::command::Command;
use crate::config::BotId;

/// What a roster card / row points at (context menus, confirms).
#[derive(Clone)]
pub enum RosterTarget {
    Bird(BotId),
    Room(String),
    /// A worker row under a bird: its session name, label, and short id.
    Worker {
        bot: BotId,
        name: String,
        label: String,
        id: String,
    },
}

/// What the confirm dialog is gating.
#[derive(Clone)]
pub enum ConfirmTarget {
    /// Remove a bird/room from the roster.
    Delete(RosterTarget),
    /// `claude stop <id>` on a bird's worker.
    StopWorker {
        name: String,
        label: String,
        id: String,
    },
}

impl ConfirmTarget {
    pub fn yes_label(&self) -> &'static str {
        match self {
            ConfirmTarget::Delete(_) => "y release",
            ConfirmTarget::StopWorker { .. } => "y stop",
        }
    }

    pub fn no_label(&self) -> &'static str {
        match self {
            ConfirmTarget::Delete(_) => "n keep",
            ConfirmTarget::StopWorker { .. } => "n leave it",
        }
    }
}

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
    /// Abandon the bird's conversation and hatch a brand-new session.
    FreshStart,
    /// Open the selected bird's profile (persona, routines, notifications).
    Profile,
    /// Remove the selected bird/room from the roster (confirmed first).
    Delete,
    /// Release/recapture the mouse: released, the terminal's native
    /// drag-select and copy work everywhere.
    ToggleMouse,
    /// Copy the pane: the bird's visible screen, or in a room the last
    /// message as raw markdown.
    Yank,
    // -- session tabs (the thread pane's parallel sessions of one bird)
    NextTab,
    PrevTab,
    /// Open one more session of the selected bird, on the lowest free tab.
    NewTab,
    /// Stop the viewed tab and forget its resume record (tab 1 refuses).
    CloseTab,
    /// Label the viewed tab (display only).
    RenameTab,
    /// Abandon the viewed tab's conversation and hatch a brand-new session —
    /// also the escape when a resume record points at a vanished session.
    FreshTab,
    // -- scrolling panes
    PageUp,
    PageDown,
    // -- room
    Compose,
    /// Pick a lettered quick-reply option (1-based).
    Quick(u8),
}

/// Cross-component notifications, fanned out by the shell.
pub enum Msg {
    /// Point the content pane at a room and open its composer.
    OpenRoom(String),
    /// Open the new-bird / new-room forms (the sidebar's footer buttons).
    OpenNewBot,
    OpenNewRoom,
    /// Open a bird's profile overlay.
    OpenProfile(BotId),
    /// Open the confirm dialog (roster deletions, stopping a worker).
    Confirm(ConfirmTarget),
    /// Open a viewer tab (`claude attach <id>`) on a bird's worker.
    AttachWorker {
        bot: BotId,
        name: String,
        label: String,
        id: String,
    },
    /// Open the right-click context menu for a card, anchored at the click.
    OpenContext {
        target: RosterTarget,
        x: u16,
        y: u16,
    },
    /// Open the compose overlay. With a `source`, that bird packages a handoff;
    /// without one the text goes straight to the target.
    Compose {
        source: Option<BotId>,
        preselect: Option<BotId>,
    },
    /// Open the rename-tab form for one session tab.
    OpenRenameTab(crate::config::SessionKey),
    /// Open the right-click menu for the CURRENT tab, anchored at the click.
    OpenTabMenu { x: u16, y: u16 },
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
