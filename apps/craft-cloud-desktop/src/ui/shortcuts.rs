//! The window's keyboard shortcuts, read from the frame's input and turned into the model's events:
//!
//! - Ctrl+1 to Ctrl+9 go to the visible tabs in the header's order (the model skips what a locked page can't leave).
//! - Ctrl+R refreshes the page, where it has Refresh.
//! - Ctrl+F puts the cursor in the page's search field, where it has one.
//!
//! "Ctrl" is Cmd on macOS (egui's `Modifiers::COMMAND`). Esc is not read here: a confirmation and a menu close on it themselves
//! (the confirmation in [`super::confirm`], a menu in egui).

use egui::{Key, KeyboardShortcut, Modifiers};

use crate::model::{Event, Model};

/// The tab shortcuts: the key of the first nine tabs.
const TAB_KEYS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

const REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
const FIND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);

/// The id of the search field of the page that has one (a page has one at a time), for Ctrl+F.
pub(super) fn search_id() -> egui::Id {
    egui::Id::new("craft-cloud-search")
}

/// The shortcut of the tab at `index` in the header (from 0), as the OS writes it ("Ctrl+2", "⌘2"); none past the ninth.
pub(super) fn tab_shortcut(ctx: &egui::Context, index: usize) -> Option<String> {
    TAB_KEYS.get(index).map(|key| ctx.format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, *key)))
}

/// The events the shortcuts pressed this frame stand for. Ctrl+F is carried out here: it moves the focus to the search field
/// of the page when the page has one.
pub(super) fn read(ctx: &egui::Context, model: &Model) -> Vec<Event> {
    let mut events = Vec::new();
    // The tabs, the page and the search work in the signed-in window only.
    let signed_in = model.tab().is_some();
    let (refresh, find) = ctx.input_mut(|i| {
        if signed_in {
            for (index, key) in TAB_KEYS.iter().enumerate() {
                if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, *key)) {
                    events.push(Event::TabNumber(index + 1));
                }
            }
        }
        (signed_in && i.consume_shortcut(&REFRESH), signed_in && i.consume_shortcut(&FIND))
    });
    if refresh {
        events.push(Event::RefreshPage);
    }
    if find && model.has_search() {
        ctx.memory_mut(|m| m.request_focus(search_id()));
    }
    events
}
