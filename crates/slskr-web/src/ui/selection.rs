//! `ListSelection`: file-manager style multi-selection for a list of rows (no checkboxes).
//!
//! Click selects one row, Ctrl/Cmd+click toggles, Shift+click selects the range from the
//! anchor. The list element handles keys with [`ListSelection::on_keydown`]: Up/Down/Home/End
//! move the focus (Shift extends), Space toggles, Ctrl/Cmd+A selects all, Escape clears and
//! Enter returns [`ListAction::Activate`]. Rows carry `data-index="{i}"` and
//! `tabindex=selection.tabindex(i)` (roving tabindex), `aria-selected=selection.is_selected(i)`.

use std::collections::BTreeSet;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// What a key press asks the list owner to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListAction {
    /// Enter: act on the selection (e.g. download it).
    Activate,
}

#[derive(Clone, Copy)]
pub struct ListSelection {
    selected: RwSignal<BTreeSet<usize>>,
    anchor: RwSignal<Option<usize>>,
    focused: RwSignal<usize>,
    len: RwSignal<usize>,
}

impl ListSelection {
    pub fn new(len: usize) -> Self {
        Self {
            selected: RwSignal::new(BTreeSet::new()),
            anchor: RwSignal::new(None),
            focused: RwSignal::new(0),
            len: RwSignal::new(len),
        }
    }

    /// Changes the number of rows, dropping selected rows past the end.
    pub fn set_len(&self, len: usize) {
        self.len.set(len);
        self.selected.update(|set| set.retain(|index| *index < len));
        if self.focused.get_untracked() >= len {
            self.focused.set(len.saturating_sub(1));
        }
    }

    /// A pointer click on row `index` with the modifier state of the event.
    pub fn click(&self, index: usize, ctrl: bool, shift: bool) {
        let anchor = self.anchor.get_untracked();
        match (ctrl, shift, anchor) {
            (_, true, Some(anchor)) => {
                let range = anchor.min(index)..=anchor.max(index);
                self.selected.update(|set| {
                    if !ctrl {
                        set.clear();
                    }
                    set.extend(range);
                });
            }
            (true, _, _) => {
                self.toggle(index);
                self.anchor.set(Some(index));
            }
            _ => {
                self.selected.set(BTreeSet::from([index]));
                self.anchor.set(Some(index));
            }
        }
        self.focused.set(index);
    }

    pub fn toggle(&self, index: usize) {
        self.selected.update(|set| {
            if !set.remove(&index) {
                set.insert(index);
            }
        });
    }

    pub fn select_all(&self) {
        let len = self.len.get_untracked();
        self.selected.set((0..len).collect());
    }

    pub fn clear(&self) {
        self.selected.set(BTreeSet::new());
        self.anchor.set(None);
    }

    /// Whether row `index` is selected (tracked).
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.with(|set| set.contains(&index))
    }

    pub fn is_selected_untracked(&self, index: usize) -> bool {
        self.selected.with_untracked(|set| set.contains(&index))
    }

    /// Selected rows in order (tracked).
    pub fn selected(&self) -> Vec<usize> {
        self.selected.with(|set| set.iter().copied().collect())
    }

    pub fn selected_untracked(&self) -> Vec<usize> {
        self.selected
            .with_untracked(|set| set.iter().copied().collect())
    }

    /// Number of selected rows (tracked).
    pub fn count(&self) -> usize {
        self.selected.with(BTreeSet::len)
    }

    /// The row that holds the focus (roving tabindex).
    pub fn focused(&self) -> usize {
        self.focused.get_untracked()
    }

    /// `tabindex` for row `index`: `"0"` for the focus row, `"-1"` otherwise (tracked).
    pub fn tabindex(&self, index: usize) -> &'static str {
        if self.focused.get() == index {
            "0"
        } else {
            "-1"
        }
    }

    /// Handles a key press on the list element (the event's current target). Moves DOM
    /// focus to the `[data-index]` row it selects.
    pub fn on_keydown(&self, event: &web_sys::KeyboardEvent, len: usize) -> Option<ListAction> {
        if len == 0 {
            return None;
        }
        let ctrl = event.ctrl_key() || event.meta_key();
        let shift = event.shift_key();
        let current = self.focused.get_untracked().min(len - 1);
        let target = match event.key().as_str() {
            "ArrowDown" => Some((current + 1).min(len - 1)),
            "ArrowUp" => Some(current.saturating_sub(1)),
            "Home" => Some(0),
            "End" => Some(len - 1),
            " " if !ctrl => {
                self.toggle(current);
                self.anchor.set(Some(current));
                event.prevent_default();
                return None;
            }
            "a" | "A" if ctrl => {
                self.select_all();
                event.prevent_default();
                return None;
            }
            "Escape" => {
                if self.selected.with_untracked(BTreeSet::is_empty) {
                    return None;
                }
                self.clear();
                event.prevent_default();
                return None;
            }
            "Enter" => {
                event.prevent_default();
                return Some(ListAction::Activate);
            }
            _ => None,
        }?;
        event.prevent_default();
        if shift {
            if self.anchor.get_untracked().is_none() {
                self.anchor.set(Some(current));
            }
            self.click(target, ctrl, true);
        } else {
            self.focused.set(target);
        }
        focus_row(event, target);
        None
    }
}

/// Focuses the `[data-index="{index}"]` element inside the event's current target.
fn focus_row(event: &web_sys::KeyboardEvent, index: usize) {
    let row = event
        .current_target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        .and_then(|list| {
            list.query_selector(&format!("[data-index=\"{index}\"]"))
                .ok()
                .flatten()
        })
        .and_then(|row| row.dyn_into::<web_sys::HtmlElement>().ok());
    if let Some(row) = row {
        let _ = row.focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_ctrl_and_shift() {
        let owner = Owner::new();
        owner.with(|| {
            let selection = ListSelection::new(10);
            selection.click(2, false, false);
            selection.click(5, true, false);
            assert_eq!(selection.selected_untracked(), [2, 5]);
            // Shift extends from the anchor (5), replacing the selection.
            selection.click(3, false, true);
            assert_eq!(selection.selected_untracked(), [3, 4, 5]);
            // Ctrl+Shift adds the range.
            selection.click(0, false, false);
            selection.click(2, true, true);
            assert_eq!(selection.selected_untracked(), [0, 1, 2]);
            selection.click(1, true, false);
            assert_eq!(selection.selected_untracked(), [0, 2]);
            selection.select_all();
            assert_eq!(selection.selected_untracked().len(), 10);
            selection.set_len(4);
            assert_eq!(selection.selected_untracked(), [0, 1, 2, 3]);
            selection.clear();
            assert!(selection.selected_untracked().is_empty());
        });
    }
}
