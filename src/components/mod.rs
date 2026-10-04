//! One module per screen, each owning its state, input, draw, and hit rects.

pub mod room;
pub mod roster;
pub mod thread;

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use ratatui::Frame;

use crate::action::{Action, Effects};
use crate::command::CommandResult;
use crate::shared::Shared;

pub trait Component {
    /// A text field owns the keyboard; the keymap is bypassed.
    fn capturing(&self) -> bool {
        false
    }
    fn handle_text(&mut self, _k: KeyEvent, _s: &mut Shared, _fx: &mut Effects) {}
    fn handle_mouse(&mut self, _m: MouseEvent, _s: &mut Shared, _fx: &mut Effects) {}
    fn update(&mut self, a: Action, s: &mut Shared, fx: &mut Effects);
    fn on_enter(&mut self, _s: &mut Shared, _fx: &mut Effects) {}
    /// Once a second, only while visible.
    fn on_tick(&mut self, _s: &mut Shared, _fx: &mut Effects) {}
    fn on_result(&mut self, _r: &CommandResult, _s: &mut Shared) {}
    fn draw(&mut self, f: &mut Frame, area: Rect, s: &mut Shared);
}

/// Static registry — no `Box<dyn>`, no downcasting. The sidebar (roster) is
/// always visible and owns the keys; thread/room are the content pane, picked
/// by the sidebar selection. Bots and rooms are STATE inside these components,
/// never new components.
pub struct Components {
    pub roster: roster::Roster,
    pub thread: thread::Thread,
    pub room: room::RoomView,
}

impl Components {
    pub fn new() -> Components {
        Components {
            roster: roster::Roster::default(),
            thread: thread::Thread::default(),
            room: room::RoomView::default(),
        }
    }
}

pub fn hits(r: Rect, x: u16, y: u16) -> bool {
    r.width > 0 && x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}
