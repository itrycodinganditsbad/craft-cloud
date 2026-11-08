//! The tray icon on Windows (notification area) and macOS (menu bar), through `tray-icon`. Made on the UI thread
//! (eframe's), which runs the event loop both need. A left click opens the window on Windows; on macOS a click shows the
//! menu, as menu-bar items do.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{Clicks, Item, Tray, TrayState};

/// Whether a left click on the icon opens the window: on Windows; on macOS it shows the menu instead.
pub const LEFT_CLICK_OPENS: bool = cfg!(not(target_os = "macos"));

pub struct NativeTray {
    icon: TrayIcon,
    /// The menu's first line: the sync state, not a button.
    status: MenuItem,
    pause: MenuItem,
    paused: Arc<AtomicBool>,
    shown: Option<(TrayState, bool, String)>,
}

fn icon(state: TrayState) -> Result<tray_icon::Icon, String> {
    let (w, h, rgba) = super::rgba(state.icon_png())?;
    tray_icon::Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())
}

pub fn start(clicks: Clicks) -> Result<NativeTray, String> {
    let items: Vec<(Item, MenuItem)> = Item::ALL.iter().map(|&item| (item, MenuItem::new(item.label(false), true, None))).collect();
    let menu = Menu::new();
    let status = MenuItem::new(super::status_line(None), false, None);
    menu.append(&status).map_err(|e| e.to_string())?;
    menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?;
    for (item, entry) in &items {
        if item.after_separator() {
            menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?;
        }
        menu.append(entry).map_err(|e| e.to_string())?;
    }
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon(TrayState::NeedsAttention)?)
        .with_menu_on_left_click(cfg!(target_os = "macos"))
        .with_tooltip(crate::ui::TITLE)
        .build()
        .map_err(|e| e.to_string())?;
    let paused = Arc::new(AtomicBool::new(false));
    let ids: Vec<(MenuId, Item)> = items.iter().map(|(item, entry)| (entry.id().clone(), *item)).collect();
    let pause = items.iter().find(|(item, _)| *item == Item::Pause).map(|(_, entry)| entry.clone()).ok_or("no Pause entry")?;
    {
        let (clicks, paused) = (clicks.clone(), paused.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if let Some((_, item)) = ids.iter().find(|(id, _)| *id == e.id) {
                clicks.send(item.event(paused.load(Ordering::SeqCst)));
            }
        }));
    }
    // On macOS a click shows the menu (as menu-bar items do), and only that.
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e
            && LEFT_CLICK_OPENS
        {
            clicks.send(Item::Open.event(false));
        }
    }));
    Ok(NativeTray { icon: tray, status, pause, paused, shown: None })
}

impl Tray for NativeTray {
    fn show(&mut self, state: TrayState, paused: bool, line: &str) {
        if self.shown.as_ref().is_some_and(|(s, p, l)| (*s, *p, l.as_str()) == (state, paused, line)) {
            return;
        }
        self.shown = Some((state, paused, line.to_owned()));
        if let Ok(image) = icon(state) {
            let _ = self.icon.set_icon(Some(image));
        }
        self.paused.store(paused, Ordering::SeqCst);
        self.pause.set_text(Item::Pause.label(paused));
        self.status.set_text(line);
    }
}

#[cfg(test)]
mod tests {
    /// macOS shows the menu on a click (with_menu_on_left_click), so the click must not also open the window.
    #[test]
    fn a_left_click_opens_the_window_only_off_macos() {
        assert_eq!(super::LEFT_CLICK_OPENS, !cfg!(target_os = "macos"));
    }
}
