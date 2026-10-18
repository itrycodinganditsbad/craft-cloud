//! The tray icon, where the desktop has a tray: synced, syncing, offline, needs attention or paused (from the 2 s
//! status poll), with Open, Open folder, Pause or Resume, and Quit. `tray-icon` on Windows and macOS
//! ([`windows_macos`]); a StatusNotifierItem over the session D-Bus on Linux ([`linux`]), only when a
//! `org.kde.StatusNotifierWatcher` is there (waybar's tray, KDE, GNOME with the extension). Without a tray the window
//! is the only way in, and closing it ends the app (see [`crate::model`]).
//!
//! The menu's clicks come to the model as events on the outside channel; the icon follows [`state_of`] the status.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub mod windows_macos;

use std::sync::Arc;
use std::sync::mpsc::Sender;

use craft_cloud_app::{RunState, Status};

use crate::model::Event;

/// What the tray icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Synced,
    Syncing,
    Offline,
    NeedsAttention,
    Paused,
}

impl TrayState {
    /// The state's icon (32 px, from `assets/`).
    pub fn icon_png(self) -> &'static [u8] {
        match self {
            TrayState::Synced => include_bytes!("../../assets/tray-synced.png"),
            TrayState::Syncing => include_bytes!("../../assets/tray-syncing.png"),
            TrayState::Offline => include_bytes!("../../assets/tray-offline.png"),
            TrayState::NeedsAttention => include_bytes!("../../assets/tray-attention.png"),
            TrayState::Paused => include_bytes!("../../assets/tray-paused.png"),
        }
    }

    /// The state in words, under the tooltip's title where the tray shows one.
    pub fn words(self) -> &'static str {
        match self {
            TrayState::Synced => "Up to date",
            TrayState::Syncing => "Syncing…",
            TrayState::Offline => "Offline",
            TrayState::NeedsAttention => "Needs your attention",
            TrayState::Paused => "Paused",
        }
    }
}

/// The tray's state for the agent's last status (`None`: no agent answers).
pub fn state_of(status: Option<&Status>) -> TrayState {
    let Some(status) = status else { return TrayState::NeedsAttention };
    if !status.signed_in || status.removed {
        return TrayState::NeedsAttention;
    }
    match &status.run_state {
        RunState::PausedByUser => TrayState::Paused,
        RunState::Paused { .. } | RunState::SignedOut => TrayState::NeedsAttention,
        RunState::Syncing => TrayState::Syncing,
        RunState::Idle if status.online => TrayState::Synced,
        RunState::Idle | RunState::Retrying { .. } | RunState::Stopping => TrayState::Offline,
    }
}

/// A menu entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Open,
    OpenFolder,
    /// Pause, or Resume while paused.
    Pause,
    Quit,
}

impl Item {
    /// The entries, in order (a separator before Pause and before Quit).
    pub const ALL: [Item; 4] = [Item::Open, Item::OpenFolder, Item::Pause, Item::Quit];

    pub fn label(self, paused: bool) -> &'static str {
        match self {
            Item::Open => "Open Craft Cloud",
            Item::OpenFolder => "Open folder",
            Item::Pause if paused => "Resume",
            Item::Pause => "Pause",
            Item::Quit => "Quit",
        }
    }

    /// What a click on it asks of the model.
    pub fn event(self, paused: bool) -> Event {
        match self {
            Item::Open => Event::Show,
            Item::OpenFolder => Event::OpenFolder,
            Item::Pause if paused => Event::Resume,
            Item::Pause => Event::Pause,
            Item::Quit => Event::Quit,
        }
    }

    /// Whether a separator comes before it.
    pub fn after_separator(self) -> bool {
        matches!(self, Item::Pause | Item::Quit)
    }
}

/// A tray icon that is up.
pub trait Tray {
    /// Shows `state`; `paused` makes Pause read Resume.
    fn show(&mut self, state: TrayState, paused: bool);
}

/// Where the menu's clicks go: the model's outside channel, then a wake of the UI.
#[derive(Clone)]
pub struct Clicks {
    pub to: Sender<Event>,
    pub wake: Arc<dyn Fn() + Send + Sync>,
}

impl Clicks {
    pub fn send(&self, event: Event) {
        if self.to.send(event).is_ok() {
            (self.wake)();
        }
    }
}

/// The tray icon, if this desktop has a tray (and it could be made); `None` otherwise, said on standard error.
pub fn start(clicks: Clicks) -> Option<Box<dyn Tray>> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let made = windows_macos::start(clicks).map(|tray| Some(Box::new(tray) as Box<dyn Tray>));
    #[cfg(target_os = "linux")]
    let made = linux::start(clicks).map(|tray| tray.map(|tray| Box::new(tray) as Box<dyn Tray>));
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let made: Result<Option<Box<dyn Tray>>, String> = {
        let _ = clicks;
        Ok(None)
    };
    match made {
        Ok(tray) => tray,
        Err(e) => {
            eprintln!("Craft Cloud: no tray icon ({e}); the window is the way in");
            None
        }
    }
}

/// A PNG of the app's icons as RGBA: width, height and pixels.
pub fn rgba(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let size = reader.output_buffer_size().ok_or("the icon is too big")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err("the icon isn't 8-bit RGBA".into());
    }
    buf.truncate(info.buffer_size());
    Ok((info.width, info.height, buf))
}

/// The app's own icon (256 px), for the window.
pub const APP_ICON: &[u8] = include_bytes!("../../assets/icon.png");

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        crate::model::tests::status(true)
    }

    #[test]
    fn the_state_comes_from_the_status() {
        assert_eq!(state_of(Some(&status())), TrayState::Synced);
        assert_eq!(state_of(Some(&Status { run_state: RunState::Syncing, ..status() })), TrayState::Syncing);
        assert_eq!(state_of(Some(&Status { online: false, ..status() })), TrayState::Offline);
        let retrying = Status { online: false, run_state: RunState::Retrying { message: "can't reach it".into() }, ..status() };
        assert_eq!(state_of(Some(&retrying)), TrayState::Offline);
        assert_eq!(state_of(Some(&Status { run_state: RunState::PausedByUser, ..status() })), TrayState::Paused);
        let stuck = Status { run_state: RunState::Paused { reason: "the folder is missing".into() }, ..status() };
        assert_eq!(state_of(Some(&stuck)), TrayState::NeedsAttention);
        assert_eq!(state_of(Some(&Status { removed: true, ..stuck })), TrayState::NeedsAttention);
        assert_eq!(state_of(Some(&crate::model::tests::status(false))), TrayState::NeedsAttention, "not joined yet");
        assert_eq!(state_of(None), TrayState::NeedsAttention, "no agent");
    }

    #[test]
    fn the_menu_reads_and_does_what_it_says() {
        let labels: Vec<&str> = Item::ALL.iter().map(|i| i.label(false)).collect();
        assert_eq!(labels, ["Open Craft Cloud", "Open folder", "Pause", "Quit"]);
        assert_eq!(Item::Pause.label(true), "Resume");
        assert_eq!(Item::Pause.event(true), Event::Resume);
        assert_eq!(Item::Open.event(false), Event::Show);
        assert_eq!(Item::Quit.event(false), Event::Quit);
    }

    #[test]
    fn every_icon_decodes() {
        for state in [TrayState::Synced, TrayState::Syncing, TrayState::Offline, TrayState::NeedsAttention, TrayState::Paused] {
            let (w, h, pixels) = rgba(state.icon_png()).unwrap();
            assert_eq!((w, h, pixels.len()), (32, 32, 32 * 32 * 4), "{state:?}");
            assert!(pixels.chunks(4).any(|p| p[3] == 255) && pixels.chunks(4).any(|p| p[3] == 0), "{state:?}");
        }
        let (w, h, _) = rgba(APP_ICON).unwrap();
        assert_eq!((w, h), (256, 256));
    }
}
