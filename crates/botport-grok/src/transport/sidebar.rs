//! The app's sidebar is virtualized: on a large account (64 bots, live 2026-11-16) it draws only the rows that fit
//! in its scroller (about 15). Two consequences, both handled here as pure logic so the fake and the debug-port
//! transport share them:
//! - the second-route check in `bots` compares the rendered rows as a consistent SUBSET of the app's list;
//! - selecting a bot first scrolls the app's own sidebar scroller until that bot's row is rendered.
use super::{TResult, TransportError};

/// The result of comparing the rendered sidebar rows with the app's list (both as `(id, name)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowCheck {
    /// Bots in the app's list (not hidden).
    pub listed: usize,
    /// Rows rendered in the sidebar.
    pub rendered: usize,
}

impl RowCheck {
    /// `matches sidebar` when every listed bot is rendered, else `15 of 64 rendered rows match (virtualized sidebar)`.
    pub fn check_line(&self) -> String {
        if self.rendered == self.listed {
            "matches sidebar".into()
        } else {
            format!(
                "{} of {} rendered rows match (virtualized sidebar)",
                self.rendered, self.listed
            )
        }
    }
}

/// Every rendered row must be a listed bot with the same id and name, with no row twice. Rows not rendered are
/// fine (virtualization). Err: why the routes disagree.
pub fn check_rows(listed: &[(&str, &str)], rendered: &[(&str, &str)]) -> Result<RowCheck, String> {
    let mut seen = std::collections::HashSet::new();
    let mut bad = 0usize;
    for (id, name) in rendered {
        if !seen.insert(*id) {
            bad += 1;
            continue;
        }
        match listed.iter().find(|(lid, _)| lid == id) {
            Some((_, lname)) if lname == name => {}
            _ => bad += 1,
        }
    }
    if bad > 0 || (rendered.is_empty() && !listed.is_empty()) {
        return Err(format!(
            "the sidebar shows {} rows that differ from the app's list ({} of them not in the list or renamed)",
            rendered.len(),
            bad
        ));
    }
    Ok(RowCheck {
        listed: listed.len(),
        rendered: rendered.len(),
    })
}

/// One step of the sidebar's own scroller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Top,
    Down,
}

/// The app's sidebar as the scroll search sees it.
pub trait Sidebar {
    /// Is the bot's row rendered (in the DOM) right now?
    fn rendered(&mut self, id: &str) -> TResult<bool>;
    /// Move the sidebar's scroller; true if it moved (false: already at the top / the end).
    fn scroll(&mut self, step: Scroll) -> TResult<bool>;
}

/// Upper bound on scroll steps (64 bots at ~0.7 of a 15-row viewport is ~7 steps; this leaves room for thousands).
pub const MAX_STEPS: usize = 400;

/// Scroll the sidebar until `id`'s row is rendered: as is, then from the top down, re-measuring after each
/// step. Returns the number of scroll steps taken. Err when the end is reached and the row never appeared.
pub fn scroll_to_row(s: &mut dyn Sidebar, id: &str) -> TResult<usize> {
    if s.rendered(id)? {
        return Ok(0);
    }
    let mut steps = 1;
    s.scroll(Scroll::Top)?;
    if s.rendered(id)? {
        return Ok(steps);
    }
    while steps < MAX_STEPS {
        steps += 1;
        let moved = s.scroll(Scroll::Down)?;
        if s.rendered(id)? {
            return Ok(steps);
        }
        if !moved {
            return Err(TransportError::App(format!(
                "bot row not found in the sidebar after scrolling it to the end ({steps} steps)"
            )));
        }
    }
    Err(TransportError::App(format!(
        "bot row not found in the sidebar after {MAX_STEPS} scroll steps"
    )))
}

/// A synthetic virtualized sidebar: `ids` in order, `window` rows drawn from `top`, `step` rows per Down.
#[derive(Debug, Clone)]
pub struct VirtualSidebar {
    pub ids: Vec<String>,
    pub window: usize,
    pub step: usize,
    pub top: usize,
    pub scrolls: usize,
}

impl VirtualSidebar {
    pub fn new(ids: Vec<String>, window: usize, top: usize) -> Self {
        VirtualSidebar {
            ids,
            window,
            step: (window * 7 / 10).max(1),
            top,
            scrolls: 0,
        }
    }
    pub fn visible(&self) -> &[String] {
        let end = (self.top + self.window).min(self.ids.len());
        &self.ids[self.top.min(end)..end]
    }
    fn max_top(&self) -> usize {
        self.ids.len().saturating_sub(self.window)
    }
}

impl Sidebar for VirtualSidebar {
    fn rendered(&mut self, id: &str) -> TResult<bool> {
        Ok(self.visible().iter().any(|v| v == id))
    }
    fn scroll(&mut self, step: Scroll) -> TResult<bool> {
        self.scrolls += 1;
        let before = self.top;
        self.top = match step {
            Scroll::Top => 0,
            Scroll::Down => (self.top + self.step).min(self.max_top()),
        };
        Ok(self.top != before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("b{i:02}")).collect()
    }

    #[test]
    fn a_consistent_subset_passes_with_counts() {
        let all = ids(64);
        let listed: Vec<(&str, &str)> = all.iter().map(|i| (i.as_str(), i.as_str())).collect();
        let r = check_rows(&listed, &listed[..15]).unwrap();
        assert_eq!((r.listed, r.rendered), (64, 15));
        assert_eq!(
            r.check_line(),
            "15 of 64 rendered rows match (virtualized sidebar)"
        );
        assert_eq!(
            check_rows(&listed, &listed).unwrap().check_line(),
            "matches sidebar"
        );
    }

    #[test]
    fn a_rendered_row_missing_from_the_list_or_renamed_fails() {
        let listed = [("a", "A"), ("b", "B")];
        assert!(check_rows(&listed, &[("a", "A"), ("x", "X")]).is_err());
        assert!(check_rows(&listed, &[("a", "Z")]).is_err());
        assert!(check_rows(&listed, &[("a", "A"), ("a", "A")]).is_err());
        assert!(check_rows(&listed, &[]).is_err());
    }

    #[test]
    fn scrolls_down_until_an_off_screen_row_renders() {
        let mut s = VirtualSidebar::new(ids(64), 15, 0);
        let n = scroll_to_row(&mut s, "b50").unwrap();
        assert!(n > 1 && s.visible().iter().any(|v| v == "b50"));
        assert_eq!(scroll_to_row(&mut s, "b50").unwrap(), 0);
    }

    #[test]
    fn scrolls_back_to_the_top_for_a_row_above() {
        let mut s = VirtualSidebar::new(ids(64), 15, 49);
        assert_eq!(scroll_to_row(&mut s, "b03").unwrap(), 1);
        assert!(scroll_to_row(&mut s, "b63").is_ok());
    }

    #[test]
    fn a_row_that_never_renders_fails_clearly() {
        let mut s = VirtualSidebar::new(ids(64), 15, 0);
        let e = scroll_to_row(&mut s, "nope").unwrap_err().to_string();
        assert!(e.contains("not found in the sidebar"), "{e}");
        assert!(s.scrolls < MAX_STEPS);
    }
}
