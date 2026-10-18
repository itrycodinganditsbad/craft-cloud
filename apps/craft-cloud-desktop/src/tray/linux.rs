//! The tray on Linux: a StatusNotifierItem (`org.kde.StatusNotifierItem`) with its menu (`com.canonical.dbusmenu`) on
//! the session D-Bus, through zbus (pure Rust: no GTK, no libdbus). It is made only when an
//! `org.kde.StatusNotifierWatcher` owns its name on the bus (waybar's tray, KDE, GNOME with the AppIndicator extension):
//! without one there is no tray, and the window is the way in.
//!
//! The item is served at `/StatusNotifierItem` under the name `org.kde.StatusNotifierItem-<pid>-1`, registered with the
//! watcher, and its icon is sent as pixels (`IconPixmap`, ARGB). zbus answers calls on a thread of its own; clicks go to
//! the model through [`Clicks`].

use std::collections::HashMap;

use zbus::blocking::Connection;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, StructureBuilder, Value};

use super::{Clicks, Item, Tray, TrayState};

/// The watcher's bus name, object and interface.
pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";

/// An icon as SNI wants it: width, height, ARGB32 in network byte order.
type Pixmap = (i32, i32, Vec<u8>);
/// A menu entry's properties.
type Props = HashMap<String, OwnedValue>;
/// A menu layout node, `(ia{sv}av)`: id, properties, children (each a node in a variant).
type Node = (i32, Props, Vec<OwnedValue>);

/// `png_bytes` as an SNI pixmap.
pub fn pixmap(png_bytes: &[u8]) -> Result<Pixmap, String> {
    let (w, h, rgba) = super::rgba(png_bytes)?;
    let argb = rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, a]| [a, r, g, b]).collect();
    Ok((i32::try_from(w).map_err(|e| e.to_string())?, i32::try_from(h).map_err(|e| e.to_string())?, argb))
}

/// The StatusNotifierItem.
struct StatusItem {
    state: TrayState,
    clicks: Clicks,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl StatusItem {
    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".into()
    }
    #[zbus(property)]
    fn id(&self) -> String {
        "craft-cloud".into()
    }
    #[zbus(property)]
    fn title(&self) -> String {
        crate::ui::TITLE.into()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }
    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        pixmap(self.state.icon_png()).into_iter().collect()
    }
    #[zbus(property)]
    fn overlay_icon_name(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        (String::new(), Vec::new(), crate::ui::TITLE.into(), self.state.words().into())
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(MENU_PATH).unwrap_or_default()
    }

    /// A click on the icon: the window.
    fn activate(&self, _x: i32, _y: i32) {
        self.clicks.send(Item::Open.event(false));
    }
    fn secondary_activate(&self, _x: i32, _y: i32) {
        self.clicks.send(Item::Open.event(false));
    }
    /// The host shows the menu itself (from `Menu`).
    fn context_menu(&self, _x: i32, _y: i32) {}
    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// The menu (`com.canonical.dbusmenu`): the root (0) and one entry per [`Item`] (ids from 1), with separators (ids
/// from 100).
struct DbusMenu {
    paused: bool,
    revision: u32,
    clicks: Clicks,
}

/// A menu entry's id.
fn entry_id(index: usize) -> i32 {
    i32::try_from(index).map_or(0, |i| i + 1)
}

impl DbusMenu {
    /// The entries, in order: id and properties (separators included).
    fn entries(&self) -> Vec<(i32, Props)> {
        let mut out = Vec::new();
        for (index, item) in Item::ALL.iter().enumerate() {
            if item.after_separator() {
                let mut props = HashMap::new();
                props.insert("type".to_owned(), OwnedValue::from(zbus::zvariant::Str::from("separator")));
                out.push((100 + entry_id(index), props));
            }
            let mut props = HashMap::new();
            props.insert("label".to_owned(), OwnedValue::from(zbus::zvariant::Str::from(item.label(self.paused))));
            props.insert("enabled".to_owned(), OwnedValue::from(true));
            props.insert("visible".to_owned(), OwnedValue::from(true));
            out.push((entry_id(index), props));
        }
        out
    }

    fn item(id: i32) -> Option<Item> {
        Item::ALL.iter().enumerate().find(|(index, _)| entry_id(*index) == id).map(|(_, item)| *item)
    }
}

fn fail(e: zbus::zvariant::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    /// The layout under `parent_id`: here always the whole menu (it is one level deep).
    fn get_layout(&self, _parent_id: i32, _recursion_depth: i32, _property_names: Vec<String>) -> zbus::fdo::Result<(u32, Node)> {
        let mut children = Vec::new();
        for (id, props) in self.entries() {
            let child = StructureBuilder::new().add_field(id).add_field(props).add_field(Vec::<OwnedValue>::new()).build().map_err(fail)?;
            children.push(OwnedValue::try_from(Value::Structure(child)).map_err(fail)?);
        }
        let mut root = HashMap::new();
        root.insert("children-display".to_owned(), OwnedValue::from(zbus::zvariant::Str::from("submenu")));
        Ok((self.revision, (0, root, children)))
    }

    fn get_group_properties(&self, ids: Vec<i32>, _property_names: Vec<String>) -> Vec<(i32, Props)> {
        self.entries().into_iter().filter(|(id, _)| ids.is_empty() || ids.contains(id)).collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let mut entry =
            self.entries().into_iter().find(|(entry, _)| *entry == id).ok_or_else(|| zbus::fdo::Error::InvalidArgs("no such entry".into()))?;
        entry.1.remove(&name).ok_or_else(|| zbus::fdo::Error::InvalidArgs("no such property".into()))
    }

    /// A click: `clicked` on an entry.
    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id == "clicked"
            && let Some(item) = Self::item(id)
        {
            self.clicks.send(item.event(self.paused));
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event_id, data, timestamp) in events {
            self.event(id, event_id, data, timestamp);
        }
        Vec::new()
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".into()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "normal".into()
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(signal)]
    async fn layout_updated(emitter: &SignalEmitter<'_>, revision: u32, parent: i32) -> zbus::Result<()>;
}

/// The tray on a bus.
pub struct SniTray {
    connection: Connection,
    shown: Option<(TrayState, bool)>,
}

/// Whether a StatusNotifierWatcher (a tray) is on the bus `connection` is on.
pub fn watcher_present(connection: &Connection) -> bool {
    let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(connection) else { return false };
    let Ok(name) = zbus::names::BusName::try_from(WATCHER) else { return false };
    dbus.name_has_owner(name).unwrap_or(false)
}

/// The tray on the session bus; `None` without a StatusNotifierWatcher there (or without a session bus).
pub fn start(clicks: Clicks) -> Result<Option<SniTray>, String> {
    match Connection::session() {
        Ok(connection) => start_on(connection, clicks),
        Err(_) => Ok(None),
    }
}

/// [`start`] on `connection` (tests use a bus of their own).
pub fn start_on(connection: Connection, clicks: Clicks) -> Result<Option<SniTray>, String> {
    if !watcher_present(&connection) {
        return Ok(None);
    }
    let e = |e: zbus::Error| e.to_string();
    {
        let server = connection.object_server();
        server.at(ITEM_PATH, StatusItem { state: TrayState::NeedsAttention, clicks: clicks.clone() }).map_err(e)?;
        server.at(MENU_PATH, DbusMenu { paused: false, revision: 1, clicks }).map_err(e)?;
    }
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    connection.request_name(name.as_str()).map_err(e)?;
    {
        let watcher = zbus::blocking::Proxy::new(&connection, WATCHER, WATCHER_PATH, WATCHER).map_err(e)?;
        watcher.call::<_, _, ()>("RegisterStatusNotifierItem", &(name.as_str(),)).map_err(e)?;
    }
    Ok(Some(SniTray { connection, shown: None }))
}

impl Tray for SniTray {
    fn show(&mut self, state: TrayState, paused: bool) {
        if self.shown == Some((state, paused)) {
            return;
        }
        let before = self.shown.replace((state, paused));
        let server = self.connection.object_server();
        if before.map(|b| b.0) != Some(state)
            && let Ok(item) = server.interface::<_, StatusItem>(ITEM_PATH)
        {
            item.get_mut().state = state;
            let _ = zbus::block_on(StatusItem::new_icon(item.signal_emitter()));
            let _ = zbus::block_on(StatusItem::new_tool_tip(item.signal_emitter()));
        }
        if before.map(|b| b.1) != Some(paused)
            && let Ok(menu) = server.interface::<_, DbusMenu>(MENU_PATH)
        {
            let revision = {
                let mut menu = menu.get_mut();
                menu.paused = paused;
                menu.revision = menu.revision.wrapping_add(1);
                menu.revision
            };
            let _ = zbus::block_on(DbusMenu::layout_updated(menu.signal_emitter(), revision, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Event;
    use std::io::BufRead;
    use std::sync::{Arc, Mutex};

    /// A private session bus for one test: `dbus-daemon`, stopped when dropped. `None` where there is no `dbus-daemon`.
    struct Bus {
        daemon: std::process::Child,
        address: String,
    }

    impl Bus {
        fn start() -> Option<Self> {
            let mut daemon = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            let mut address = String::new();
            std::io::BufReader::new(daemon.stdout.take()?).read_line(&mut address).ok()?;
            Some(Self { daemon, address: address.trim().to_owned() })
        }

        fn connect(&self) -> Connection {
            zbus::blocking::connection::Builder::address(self.address.as_str()).unwrap().build().unwrap()
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
        }
    }

    /// A watcher that records who registered.
    struct Watcher(Arc<Mutex<Vec<String>>>);

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl Watcher {
        fn register_status_notifier_item(&self, service: String) {
            self.0.lock().unwrap().push(service);
        }
    }

    fn clicks() -> (Clicks, std::sync::mpsc::Receiver<Event>) {
        let (to, rx) = std::sync::mpsc::channel();
        (Clicks { to, wake: Arc::new(|| {}) }, rx)
    }

    /// Review focus: no StatusNotifierWatcher (Hyprland without waybar's tray, GNOME without the extension): no tray.
    #[test]
    fn without_a_watcher_there_is_no_tray() {
        let Some(bus) = Bus::start() else {
            eprintln!("no dbus-daemon here: the D-Bus tray tests are skipped");
            return;
        };
        assert!(start_on(bus.connect(), clicks().0).unwrap().is_none());
    }

    /// With a watcher: the item registers, serves its icon and menu, a click on Quit reaches the model, and pausing
    /// makes the menu read Resume.
    #[test]
    fn with_a_watcher_the_item_registers_and_its_menu_works() {
        let Some(bus) = Bus::start() else {
            eprintln!("no dbus-daemon here: the D-Bus tray tests are skipped");
            return;
        };
        let registered = Arc::new(Mutex::new(Vec::new()));
        let _watcher = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(WATCHER)
            .unwrap()
            .serve_at(WATCHER_PATH, Watcher(registered.clone()))
            .unwrap()
            .build()
            .unwrap();
        let (clicks, events) = clicks();
        let mut tray = start_on(bus.connect(), clicks).unwrap().expect("a tray");
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        assert_eq!(registered.lock().unwrap().as_slice(), std::slice::from_ref(&name));

        let host = bus.connect();
        let item = zbus::blocking::Proxy::new(&host, name.as_str(), ITEM_PATH, "org.kde.StatusNotifierItem").unwrap();
        let pixmaps: Vec<Pixmap> = item.get_property("IconPixmap").unwrap();
        assert_eq!(pixmaps.first().map(|p| (p.0, p.1, p.2.len())), Some((32, 32, 32 * 32 * 4)));
        let title: String = item.get_property("Title").unwrap();
        assert_eq!(title, "Craft Cloud");
        let menu_path: OwnedObjectPath = item.get_property("Menu").unwrap();
        assert_eq!(menu_path.as_str(), MENU_PATH);
        item.call::<_, _, ()>("Activate", &(0i32, 0i32)).unwrap();
        assert_eq!(events.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), Event::Show);

        let menu = zbus::blocking::Proxy::new(&host, name.as_str(), MENU_PATH, "com.canonical.dbusmenu").unwrap();
        let labels = |menu: &zbus::blocking::Proxy<'_>| -> Vec<String> {
            let (_, (_, _, children)): (u32, Node) = menu.call("GetLayout", &(0i32, -1i32, Vec::<String>::new())).unwrap();
            children
                .into_iter()
                .filter_map(|child| {
                    let (_, props, _): Node = child.try_into().ok()?;
                    String::try_from(props.get("label")?.try_clone().ok()?).ok()
                })
                .collect()
        };
        assert_eq!(labels(&menu), ["Open Craft Cloud", "Open folder", "Pause", "Quit"]);
        menu.call::<_, _, ()>("Event", &(4i32, "clicked", Value::from(0i32), 0u32)).unwrap();
        assert_eq!(events.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), Event::Quit);

        tray.show(TrayState::Paused, true);
        assert_eq!(labels(&menu), ["Open Craft Cloud", "Open folder", "Resume", "Quit"]);
        menu.call::<_, _, ()>("Event", &(3i32, "clicked", Value::from(0i32), 0u32)).unwrap();
        assert_eq!(events.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), Event::Resume);
        // Read afresh, as a host does on NewToolTip (the proxy above keeps what it read first).
        let properties =
            zbus::blocking::fdo::PropertiesProxy::builder(&host).destination(name.as_str()).unwrap().path(ITEM_PATH).unwrap().build().unwrap();
        let interface = zbus::names::InterfaceName::try_from("org.kde.StatusNotifierItem").unwrap();
        let tip: (String, Vec<Pixmap>, String, String) = properties.get(interface, "ToolTip").unwrap().try_into().unwrap();
        assert_eq!((tip.2.as_str(), tip.3.as_str()), ("Craft Cloud", "Paused"));
    }

    #[test]
    fn pixmaps_are_argb() {
        let (w, h, argb) = pixmap(TrayState::Synced.icon_png()).unwrap();
        let (_, _, rgba) = super::super::rgba(TrayState::Synced.icon_png()).unwrap();
        assert_eq!((w, h), (32, 32));
        assert_eq!(argb[..4], [rgba[3], rgba[0], rgba[1], rgba[2]]);
    }
}
