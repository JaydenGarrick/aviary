//! One selection cursor to rule out six hand-rolled clamp implementations.

use crate::action::Action;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wrap {
    /// Stop at the ends.
    Clamp,
    /// Wrap around them. No caller yet outside tests — kept because a cycling
    /// list (mb's Run switcher shape) is the known next consumer.
    #[allow(dead_code)]
    Cycle,
}

#[derive(Default)]
pub struct ListNav {
    pub selected: usize,
}

impl ListNav {
    /// Apply a navigation action against a list of `len`; returns true when
    /// the action was navigation (handled), false to let the caller try it.
    pub fn handle(&mut self, a: Action, len: usize, wrap: Wrap) -> bool {
        if len == 0 {
            return matches!(
                a,
                Action::Up | Action::Down | Action::Top | Action::Bottom
            );
        }
        let last = len - 1;
        match a {
            Action::Down => {
                self.selected = match wrap {
                    Wrap::Clamp => (self.selected + 1).min(last),
                    Wrap::Cycle => (self.selected + 1) % len,
                }
            }
            Action::Up => {
                self.selected = match wrap {
                    Wrap::Clamp => self.selected.saturating_sub(1),
                    Wrap::Cycle => (self.selected + len - 1) % len,
                }
            }
            Action::Top => self.selected = 0,
            Action::Bottom => self.selected = last,
            _ => return false,
        }
        true
    }

    /// After the list shrinks, keep the cursor on a real row.
    pub fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action as A;

    #[test]
    fn clamp_stops_at_ends() {
        let mut nav = ListNav::default();
        assert!(nav.handle(A::Up, 3, Wrap::Clamp));
        assert_eq!(nav.selected, 0);
        nav.handle(A::Bottom, 3, Wrap::Clamp);
        nav.handle(A::Down, 3, Wrap::Clamp);
        assert_eq!(nav.selected, 2);
    }

    #[test]
    fn cycle_wraps_both_ways() {
        let mut nav = ListNav::default();
        nav.handle(A::Up, 3, Wrap::Cycle);
        assert_eq!(nav.selected, 2);
        nav.handle(A::Down, 3, Wrap::Cycle);
        assert_eq!(nav.selected, 0);
    }

    #[test]
    fn clamp_after_shrink() {
        let mut nav = ListNav { selected: 5 };
        nav.clamp(2);
        assert_eq!(nav.selected, 1);
        nav.clamp(0);
        assert_eq!(nav.selected, 0);
    }

    #[test]
    fn empty_list_swallows_navigation_only() {
        let mut nav = ListNav::default();
        assert!(nav.handle(A::Down, 0, Wrap::Clamp));
        assert!(!nav.handle(A::Confirm, 0, Wrap::Clamp));
        assert_eq!(nav.selected, 0);
    }
}
