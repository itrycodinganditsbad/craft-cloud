//! Install, upgrade, move and uninstall against temporary folders, a recording machine and an in-memory registry (on
//! every OS), and the real registry under a throwaway key (on Windows).

use std::sync::Mutex;

use super::*;

/// Records what it was asked; the agent "runs" while `running` says so.
#[derive(Default)]
struct Recorder {
    running: Mutex<bool>,
    asked: Mutex<Vec<String>>,
}

impl Recorder {
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
    fn note(&self, what: String) {
        self.asked.lock().unwrap().push(what);
    }
}

fn name(path: &Path) -> String {
    let dir = path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{dir}/{}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
}

impl Machine for Recorder {
    fn quit_agent(&self, agent: &Path) -> bool {
        self.note(format!("quit {}", name(agent)));
        std::mem::replace(&mut *self.running.lock().unwrap(), false)
    }
    fn close_app(&self) {
        self.note("close app".into());
    }
    fn run_agent(&self, agent: &Path, args: &[&str]) -> Result<(), String> {
        self.note(format!("{} {}", name(agent), args.join(" ")));
        Ok(())
    }
    fn start_agent(&self, agent_bg: &Path) -> Result<(), String> {
        self.note(format!("start {}", name(agent_bg)));
        *self.running.lock().unwrap() = true;
        Ok(())
    }
    fn shortcut(&self, lnk: &Path, target: &Path) -> Result<(), String> {
        std::fs::write(lnk, target.display().to_string()).map_err(|e| e.to_string())
    }
    fn remove_after_exit(&self, dir: &Path) -> Result<(), String> {
        self.note(format!("remove after exit {}", dir.display()));
        Ok(())
    }
}

/// A user's folders in a temporary place: programs, the Start menu, the agent's data folder and the synced folder.
struct User {
    _root: tempfile::TempDir,
    places: Places,
    data: PathBuf,
    synced: PathBuf,
    setup: PathBuf,
}

fn user(keys: &str) -> User {
    let root = tempfile::tempdir().unwrap();
    let local = root.path().join("AppData").join("Local");
    let places = Places {
        programs: local.join("Programs"),
        start_menu: root.path().join("Start Menu").join("Programs"),
        run_key: format!(r"{keys}\Run"),
        uninstall_key: format!(r"{keys}\Uninstall\CraftCloud"),
    };
    let data = local.join("craft-cloud-agent");
    let synced = root.path().join("Craft Cloud");
    for dir in [&data, &synced] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(data.join("account.json"), "{}").unwrap();
    std::fs::write(synced.join("notes.txt"), "mine").unwrap();
    let setup = root.path().join("Downloads").join(SETUP_EXE);
    std::fs::create_dir_all(setup.parent().unwrap()).unwrap();
    std::fs::write(&setup, "setup v2").unwrap();
    User { _root: root, places, data, synced, setup }
}

const V2: &[(&str, &[u8])] = &[(APP_EXE, b"app v2"), (AGENT_EXE, b"agent v2"), (AGENT_BG_EXE, b"agent-bg v2")];

fn options(start_menu: bool) -> Options {
    Options { start_menu, version: "0.0.2".into() }
}

#[test]
fn a_fresh_install_writes_the_files_the_entry_and_the_shortcut() {
    let u = user("Test");
    let (registry, machine) = (MemoryRegistry::default(), Recorder::default());
    let installed = install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap();
    let dir = u.places.dir();
    assert_eq!(installed, Installed { dir: dir.clone(), agent_was_running: false, moved_from: None, warnings: Vec::new() });
    assert!(dir.ends_with(Path::new("Programs").join("Craft Cloud")));
    for (file, bytes) in V2 {
        assert_eq!(std::fs::read(dir.join(file)).unwrap(), *bytes);
    }
    assert_eq!(std::fs::read(dir.join(SETUP_EXE)).unwrap(), b"setup v2", "the setup is copied in for uninstalling");
    let key = &u.places.uninstall_key;
    assert_eq!(registry.get(key, "DisplayName").as_deref(), Some("Craft Cloud"));
    assert_eq!(registry.get(key, "DisplayVersion").as_deref(), Some("0.0.2"));
    assert_eq!(registry.get(key, "Publisher"), None);
    assert_eq!(registry.get(key, "UninstallString"), Some(format!("\"{}\" --uninstall", dir.join(SETUP_EXE).display())));
    assert_eq!(std::fs::read_to_string(u.places.shortcut()).unwrap(), dir.join(APP_EXE).display().to_string());
    assert_eq!(machine.asked(), ["close app"], "nothing to quit: no agent installed");
    // No leftovers of the replacing.
    assert!(!std::fs::read_dir(&dir).unwrap().any(|e| e.unwrap().path().extension().is_some_and(|x| x == "new")));
}

#[test]
fn no_start_menu_means_no_shortcut() {
    let u = user("Test");
    let (registry, machine) = (MemoryRegistry::default(), Recorder::default());
    install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap();
    assert!(u.places.shortcut().exists());
    install(&u.places, &registry, &machine, V2, &u.setup, &options(false)).unwrap();
    assert!(!u.places.shortcut().exists(), "taken away when installed again without it");
}

#[test]
fn an_upgrade_stops_the_agent_and_the_app_replaces_the_files_and_starts_the_agent_again() {
    let u = user("Test");
    let (registry, machine) = (MemoryRegistry::default(), Recorder::default());
    let old: &[(&str, &[u8])] = &[(APP_EXE, b"app v1"), (AGENT_EXE, b"agent v1"), (AGENT_BG_EXE, b"agent-bg v1")];
    install(&u.places, &registry, &machine, old, &u.setup, &options(true)).unwrap();
    *machine.running.lock().unwrap() = true;
    machine.asked.lock().unwrap().clear();
    let installed = install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap();
    assert!(installed.agent_was_running);
    assert_eq!(machine.asked(), ["quit Craft Cloud/craft-cloud-agent.exe", "close app", "start Craft Cloud/craft-cloud-agent-bg.exe"]);
    assert_eq!(std::fs::read(u.places.dir().join(AGENT_EXE)).unwrap(), b"agent v2");
    assert!(u.data.join("account.json").exists() && u.synced.join("notes.txt").exists());
}

/// BEAST's install, made by hand in `Programs\craft-cloud`: its agent starts at login from there. The new agent takes
/// start at login over; the old files stay.
#[test]
fn an_older_install_is_moved_over() {
    let u = user("Test");
    let (registry, machine) = (MemoryRegistry::default(), Recorder::default());
    let old = u.places.programs.join(OLD_DIR_NAME);
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join(AGENT_EXE), "agent v0").unwrap();
    std::fs::write(old.join(AGENT_BG_EXE), "agent-bg v0").unwrap();
    let value = format!("\"{}\" run", old.join(AGENT_BG_EXE).display());
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &value).unwrap();
    *machine.running.lock().unwrap() = true;
    let installed = install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap();
    assert_eq!(installed.moved_from, Some(old.clone()));
    assert_eq!(
        machine.asked(),
        [
            "quit craft-cloud/craft-cloud-agent.exe",
            "close app",
            "Craft Cloud/craft-cloud-agent.exe autostart on",
            "start Craft Cloud/craft-cloud-agent-bg.exe"
        ],
        "stopped through the old agent, moved by the new one with no agent running, started from the new folder"
    );
    assert_eq!(std::fs::read(old.join(AGENT_EXE)).unwrap(), b"agent v0", "the old files are left alone");

    // An old folder whose agent doesn't start at login is only left alone.
    let u = user("Test");
    let (registry, machine) = (MemoryRegistry::default(), Recorder::default());
    let old = u.places.programs.join(OLD_DIR_NAME);
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join(AGENT_EXE), "agent v0").unwrap();
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, r#""D:\dev\craft-cloud-agent-bg.exe" run"#).unwrap();
    assert_eq!(install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap().moved_from, None);
}

/// Review focus: uninstall stops the agent and the app, removes their Run values, the shortcut, the folder and the
/// key, and keeps the synced folder and the data folder; it works with no agent running, and from inside the folder.
#[test]
fn uninstall_takes_everything_away_but_the_files_and_the_sign_in() {
    let u = user("Test");
    let (registry, machine) = (MemoryRegistry::default(), Recorder::default());
    install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap();
    let dir = u.places.dir();
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &format!("\"{}\" run --log", dir.join(AGENT_BG_EXE).display())).unwrap();
    registry.set_string(&u.places.run_key, APP_RUN_VALUE, &format!("\"{}\" --hidden", dir.join(APP_EXE).display())).unwrap();
    registry.set_string(&u.places.run_key, "Something else", r#""C:\Other\other.exe""#).unwrap();
    machine.asked.lock().unwrap().clear();

    uninstall(&u.places, &registry, &machine, &u.setup).unwrap();
    assert_eq!(machine.asked(), ["quit Craft Cloud/craft-cloud-agent.exe", "close app"]);
    assert_eq!(registry.get(&u.places.run_key, AGENT_RUN_VALUE), None);
    assert_eq!(registry.get(&u.places.run_key, APP_RUN_VALUE), None);
    assert!(registry.get(&u.places.run_key, "Something else").is_some(), "other programs' values stay");
    assert_eq!(registry.get(&u.places.uninstall_key, "DisplayName"), None);
    assert!(!dir.exists() && !u.places.shortcut().exists());
    assert!(u.data.join("account.json").exists() && u.synced.join("notes.txt").exists());
    // Again, with nothing there: fine.
    uninstall(&u.places, &registry, &machine, &u.setup).unwrap();

    // From "Apps & features": the setup in the folder runs; everything else goes now, the folder after it ends.
    install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap();
    machine.asked.lock().unwrap().clear();
    let inside = dir.join(SETUP_EXE);
    uninstall(&u.places, &registry, &machine, &inside).unwrap();
    let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, [SETUP_EXE]);
    assert_eq!(machine.asked().last().cloned(), Some(format!("remove after exit {}", dir.display())));
}

#[test]
fn a_setup_without_its_payload_installs_nothing() {
    let u = user("Test");
    let e = install(&u.places, &MemoryRegistry::default(), &Recorder::default(), &[], &u.setup, &options(true)).unwrap_err();
    assert!(e.contains("cargo xtask package"), "{e}");
    assert!(!u.places.dir().exists());
}

#[test]
fn run_values_are_matched_to_the_folder_they_start_from() {
    let dir = Path::new(r"C:\Users\Zoë\AppData\Local\Programs\Craft Cloud");
    assert!(starts_from(r#""C:\Users\Zoë\AppData\Local\Programs\Craft Cloud\craft-cloud-agent-bg.exe" run --log"#, dir));
    assert!(starts_from(r#""c:\users\zoë\appdata\local\programs\craft cloud\craft-cloud.exe" --hidden"#, dir), "case doesn't matter");
    assert!(!starts_from(r#""C:\Users\Zoë\AppData\Local\Programs\Craft Cloud Beta\craft-cloud.exe""#, dir));
    assert!(!starts_from(r#""D:\dev\craft-cloud-agent-bg.exe" run"#, dir));
}

/// The same against the real registry, under `HKCU\Software\Craft Cloud Test\…` (deleted after).
#[cfg(windows)]
#[test]
fn install_and_uninstall_with_the_real_registry() {
    struct Key(String);
    impl Drop for Key {
        fn drop(&mut self) {
            let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
        }
    }
    let key = Key(format!(r"Software\Craft Cloud Test\setup-{}", std::process::id()));
    let u = user(&key.0);
    let (registry, machine) = (crate::windows::UserRegistry, Recorder::default());
    install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap();
    assert_eq!(registry.get(&u.places.uninstall_key, "DisplayName").as_deref(), Some("Craft Cloud"));
    let dword = windows_registry::CURRENT_USER.open(&u.places.uninstall_key).unwrap().get_u32("NoModify").unwrap();
    assert_eq!(dword, 1);
    let dir = u.places.dir();
    registry.set_string(&u.places.run_key, APP_RUN_VALUE, &format!("\"{}\" --hidden", dir.join(APP_EXE).display())).unwrap();
    uninstall(&u.places, &registry, &machine, &u.setup).unwrap();
    assert_eq!(registry.get(&u.places.uninstall_key, "DisplayName"), None);
    assert_eq!(registry.get(&u.places.run_key, APP_RUN_VALUE), None);
    assert!(!dir.exists() && u.data.join("account.json").exists());
}

/// The real shortcut writer makes a `.lnk` (a shell link's header).
#[cfg(windows)]
#[test]
fn the_start_menu_shortcut_is_a_shell_link() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("craft-cloud.exe");
    std::fs::write(&target, "app").unwrap();
    let lnk = dir.path().join(SHORTCUT);
    crate::windows::ThisMachine.shortcut(&lnk, &target).unwrap();
    let bytes = std::fs::read(&lnk).unwrap();
    assert_eq!(bytes[..4], [0x4c, 0, 0, 0], "a shell link starts with its header size");
}
