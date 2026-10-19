//! Install, upgrade, move and uninstall against temporary folders, a recording machine and an in-memory registry (on
//! every OS), and the real registry under a throwaway key (on Windows).

use std::sync::{Arc, Mutex};

use super::*;

/// Records what it was asked; the agent "runs" while `running` says so.
#[derive(Default)]
struct Recorder {
    running: Mutex<bool>,
    /// The agent doesn't stop, even when ended.
    stuck: bool,
    /// The agent accepts the quit, then whether it has ended can't be told (tasklist fails).
    unknowable: bool,
    /// Moving a file or folder fails (one is still in use).
    rename_fails: bool,
    /// Where `autostart on` writes the agent's Run value, as the agent does (for the program it was run as)…
    registry: Option<(Arc<MemoryRegistry>, String)>,
    /// …unless an old agent still running answered it and registered itself.
    old_answers: bool,
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
    fn stop_agent(&self, _data: &Path) -> AgentStop {
        self.note("stop agent".into());
        if self.stuck {
            AgentStop::StillRunning("process 4242 doesn't end".into())
        } else if self.unknowable {
            *self.running.lock().unwrap() = false;
            AgentStop::Unknown("tasklist can't tell: tasklist failed".into())
        } else if std::mem::replace(&mut *self.running.lock().unwrap(), false) {
            AgentStop::Stopped
        } else {
            AgentStop::NotRunning
        }
    }
    fn close_app(&self) {
        self.note("close app".into());
    }
    fn run_agent(&self, agent: &Path, args: &[&str]) -> Result<(), String> {
        self.note(format!("{} {}", name(agent), args.join(" ")));
        if let (["autostart", "on"], Some((registry, key)), false) = (args, &self.registry, self.old_answers) {
            let value = format!("\"{}\" run --log", agent.with_file_name(AGENT_BG_EXE).display());
            registry.set_string(key, AGENT_RUN_VALUE, &value)?;
        }
        Ok(())
    }
    fn start_agent(&self, agent_bg: &Path, _data: &Path) -> Result<(), String> {
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
    fn rename(&self, from: &Path, to: &Path) -> Result<(), String> {
        if self.rename_fails {
            return Err(format!("{} can't be moved (is it still running?)", from.display()));
        }
        std::fs::rename(from, to).map_err(|e| e.to_string())
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
    let data = local.join("craft-cloud-agent");
    let places = Places {
        programs: local.join("Programs"),
        start_menu: root.path().join("Start Menu").join("Programs"),
        run_key: format!(r"{keys}\Run"),
        uninstall_key: format!(r"{keys}\Uninstall\CraftCloud"),
        agent_data: data.clone(),
        app_files: local.join(APP_FILES_DIR),
        patience: std::time::Duration::from_millis(300),
    };
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
    assert_eq!(machine.asked(), ["close app", "stop agent"], "no agent was running: none is started");
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
    // The app first: an open app would start the old agent again (Try again) between the quit and its close.
    assert_eq!(machine.asked(), ["close app", "stop agent", "start Craft Cloud/craft-cloud-agent-bg.exe"]);
    assert_eq!(std::fs::read(u.places.dir().join(AGENT_EXE)).unwrap(), b"agent v2");
    assert!(u.data.join("account.json").exists() && u.synced.join("notes.txt").exists());
}

/// An install that fails once the agent is stopped (here the setup's own exe can't be read) starts the agent again,
/// from the folder its start at login names, and says so.
#[test]
fn an_install_that_fails_after_the_stop_starts_the_agent_again() {
    let u = user("Test");
    let (registry, machine) = (MemoryRegistry::default(), Recorder::default());
    let old: &[(&str, &[u8])] = &[(APP_EXE, b"app v1"), (AGENT_EXE, b"agent v1"), (AGENT_BG_EXE, b"agent-bg v1")];
    install(&u.places, &registry, &machine, old, &u.setup, &options(true)).unwrap();
    let value = format!("\"{}\" run --log", u.places.dir().join(AGENT_BG_EXE).display());
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &value).unwrap();
    *machine.running.lock().unwrap() = true;
    machine.asked.lock().unwrap().clear();

    let gone = u.setup.with_file_name("not-there.exe");
    let e = install(&u.places, &registry, &machine, V2, &gone, &options(true)).unwrap_err();
    assert!(e.contains("not-there.exe") && e.ends_with("The agent was started again."), "{e}");
    assert_eq!(machine.asked(), ["close app", "stop agent", "start Craft Cloud/craft-cloud-agent-bg.exe"]);

    // Nothing to start (the program folder can't even be made, and there is no older install): said too.
    let bare = user("Bare");
    std::fs::write(&bare.places.programs, "not a folder").unwrap();
    let machine = Recorder::default();
    *machine.running.lock().unwrap() = true;
    let e = install(&bare.places, &registry, &machine, V2, &bare.setup, &options(true)).unwrap_err();
    assert!(e.ends_with("The agent couldn't be started again (no agent is there to start); it starts at the next login."), "{e}");
    assert_eq!(machine.asked(), ["close app", "stop agent"]);

    // Not running before: nothing is started.
    let machine = Recorder::default();
    let e = install(&u.places, &registry, &machine, V2, &gone, &options(true)).unwrap_err();
    assert!(!e.contains("agent"), "{e}");
    assert_eq!(machine.asked(), ["close app", "stop agent"]);
}

/// An uninstall that fails after the agent was stopped and its start at login removed (here the shortcut can't be
/// removed): the Run values are put back and the agent is started again; without a running agent, only put back.
#[test]
fn an_uninstall_that_fails_after_the_stop_puts_the_agent_back() {
    let u = user("Test");
    let registry = MemoryRegistry::default();
    install(&u.places, &registry, &Recorder::default(), V2, &u.setup, &options(false)).unwrap();
    let value = format!("\"{}\" run --log", u.places.dir().join(AGENT_BG_EXE).display());
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &value).unwrap();
    let before = registry.values.lock().unwrap().clone();
    // A "shortcut" that is a folder with something in it: removing it fails.
    std::fs::create_dir_all(u.places.shortcut().join("x")).unwrap();

    let machine = Recorder::default();
    *machine.running.lock().unwrap() = true;
    let e = uninstall(&u.places, &registry, &machine, &u.setup).unwrap_err();
    assert!(e.contains("The agent was started again.") && e.ends_with("Start at login was put back."), "{e}");
    assert_eq!(machine.asked(), ["close app", "stop agent", "start Craft Cloud/craft-cloud-agent-bg.exe"]);
    assert_eq!(*registry.values.lock().unwrap(), before, "the Run value is back, the entry stays");

    let machine = Recorder::default();
    let e = uninstall(&u.places, &registry, &machine, &u.setup).unwrap_err();
    assert!(e.ends_with("Start at login was put back.") && !e.contains("started again"), "{e}");
    assert_eq!(machine.asked(), ["close app", "stop agent"]);
    assert_eq!(*registry.values.lock().unwrap(), before);
}

/// The commonest failure: a file in the program folder is still held, so it can't be moved aside. Nothing in the
/// folder has been touched: the Run values are put back and the agent started again. From "Apps & features" too.
#[test]
fn an_uninstall_with_a_file_still_held_leaves_the_folder_whole_and_puts_the_agent_back() {
    let u = user("Test");
    let registry = MemoryRegistry::default();
    install(&u.places, &registry, &Recorder::default(), V2, &u.setup, &options(false)).unwrap();
    let dir = u.places.dir();
    let value = format!("\"{}\" run --log", dir.join(AGENT_BG_EXE).display());
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &value).unwrap();
    let before = registry.values.lock().unwrap().clone();
    for running in [u.setup.clone(), dir.join(SETUP_EXE)] {
        let machine = Recorder { rename_fails: true, ..Recorder::default() };
        *machine.running.lock().unwrap() = true;
        let e = uninstall(&u.places, &registry, &machine, &running).unwrap_err();
        assert!(e.contains("can't be moved") && e.contains("The agent was started again.") && e.ends_with("Start at login was put back."), "{e}");
        assert_eq!(machine.asked(), ["close app", "stop agent", "start Craft Cloud/craft-cloud-agent-bg.exe"]);
        assert_eq!(*registry.values.lock().unwrap(), before);
        for (file, bytes) in V2 {
            assert_eq!(std::fs::read(dir.join(file)).unwrap(), *bytes, "{file} is in place");
        }
        let siblings: Vec<_> = std::fs::read_dir(&u.places.programs).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(siblings, [DIR_NAME], "nothing left aside");
    }
}

/// The agent accepts the quit, then tasklist fails: the install and the uninstall stop with nothing else changed,
/// and the agent (which may be stopping) is started again from the folder its start at login names.
#[test]
fn an_agent_asked_to_quit_whose_end_cant_be_told_is_started_again() {
    let u = user("Test");
    let registry = MemoryRegistry::default();
    let old: &[(&str, &[u8])] = &[(APP_EXE, b"app v1"), (AGENT_EXE, b"agent v1"), (AGENT_BG_EXE, b"agent-bg v1")];
    install(&u.places, &registry, &Recorder::default(), old, &u.setup, &options(true)).unwrap();
    let value = format!("\"{}\" run --log", u.places.dir().join(AGENT_BG_EXE).display());
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &value).unwrap();
    let before = registry.values.lock().unwrap().clone();

    let machine = Recorder { unknowable: true, ..Recorder::default() };
    *machine.running.lock().unwrap() = true;
    let e = install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap_err();
    assert!(e.contains("can't be told") && e.ends_with("The agent was started again."), "{e}");
    assert_eq!(machine.asked(), ["close app", "stop agent", "start Craft Cloud/craft-cloud-agent-bg.exe"]);
    assert_eq!(*registry.values.lock().unwrap(), before);
    assert_eq!(std::fs::read(u.places.dir().join(AGENT_EXE)).unwrap(), b"agent v1", "the files are the old ones");

    machine.asked.lock().unwrap().clear();
    let e = uninstall(&u.places, &registry, &machine, &u.setup).unwrap_err();
    assert!(e.ends_with("The agent was started again."), "{e}");
    assert_eq!(machine.asked(), ["close app", "stop agent", "start Craft Cloud/craft-cloud-agent-bg.exe"]);
    assert_eq!(*registry.values.lock().unwrap(), before);
}

/// BEAST's install, made by hand in `Programs\craft-cloud`: its agent starts at login from there. The new agent takes
/// start at login over; the old files stay.
#[test]
fn an_older_install_is_moved_over() {
    let u = user("Test");
    let registry = Arc::new(MemoryRegistry::default());
    let machine = Recorder { registry: Some((registry.clone(), u.places.run_key.clone())), ..Recorder::default() };
    let old = u.places.programs.join(OLD_DIR_NAME);
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join(AGENT_EXE), "agent v0").unwrap();
    std::fs::write(old.join(AGENT_BG_EXE), "agent-bg v0").unwrap();
    let value = format!("\"{}\" run", old.join(AGENT_BG_EXE).display());
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &value).unwrap();
    *machine.running.lock().unwrap() = true;
    let installed = install(&u.places, &*registry, &machine, V2, &u.setup, &options(true)).unwrap();
    assert_eq!(installed.moved_from, Some(old.clone()));
    assert!(installed.warnings.is_empty(), "{:?}", installed.warnings);
    assert_eq!(
        machine.asked(),
        ["close app", "stop agent", "Craft Cloud/craft-cloud-agent.exe autostart on", "start Craft Cloud/craft-cloud-agent-bg.exe"],
        "the old agent stopped, moved by the new one with no agent running, started from the new folder"
    );
    let now = registry.get(&u.places.run_key, AGENT_RUN_VALUE).unwrap();
    assert!(starts_from(&now, &u.places.dir()), "{now}");
    assert_eq!(std::fs::read(old.join(AGENT_EXE)).unwrap(), b"agent v0", "the old files are left alone");

    // An `autostart on` that didn't land (an old agent answered it and registered itself) is no move: it is said.
    let u = user("Test");
    let registry = Arc::new(MemoryRegistry::default());
    let machine = Recorder { registry: Some((registry.clone(), u.places.run_key.clone())), old_answers: true, ..Recorder::default() };
    let old = u.places.programs.join(OLD_DIR_NAME);
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join(AGENT_EXE), "agent v0").unwrap();
    let value = format!("\"{}\" run", old.join(AGENT_BG_EXE).display());
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &value).unwrap();
    let installed = install(&u.places, &*registry, &machine, V2, &u.setup, &options(true)).unwrap();
    assert_eq!(installed.moved_from, None);
    assert!(installed.warnings.iter().any(|w| w.contains("still runs the old agent")), "{:?}", installed.warnings);

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
    std::fs::create_dir_all(&u.places.app_files).unwrap();
    std::fs::write(u.places.app_files.join("window.lock"), "").unwrap();
    let old_loose = u.places.app_files.with_file_name("craft-cloud-tray.port");
    std::fs::write(&old_loose, "").unwrap();
    machine.asked.lock().unwrap().clear();

    uninstall(&u.places, &registry, &machine, &u.setup).unwrap();
    assert_eq!(machine.asked(), ["close app", "stop agent"]);
    assert!(!u.places.app_files.exists() && !old_loose.exists(), "the app's single-instance files go, old names too");
    assert_eq!(registry.get(&u.places.run_key, AGENT_RUN_VALUE), None);
    assert_eq!(registry.get(&u.places.run_key, APP_RUN_VALUE), None);
    assert!(registry.get(&u.places.run_key, "Something else").is_some(), "other programs' values stay");
    assert_eq!(registry.get(&u.places.uninstall_key, "DisplayName"), None);
    assert!(!dir.exists() && !u.places.shortcut().exists());
    assert!(u.data.join("account.json").exists() && u.synced.join("notes.txt").exists());
    // Again, with nothing there: fine. Something else in the app's folder stays, and so does the folder.
    std::fs::create_dir_all(&u.places.app_files).unwrap();
    std::fs::write(u.places.app_files.join("mine.txt"), "mine").unwrap();
    uninstall(&u.places, &registry, &machine, &u.setup).unwrap();
    assert!(u.places.app_files.join("mine.txt").exists(), "only the app's own names are removed");

    // From "Apps & features": the setup in the folder runs; everything else goes now, the folder after it ends.
    install(&u.places, &registry, &machine, V2, &u.setup, &options(true)).unwrap();
    machine.asked.lock().unwrap().clear();
    let inside = dir.join(SETUP_EXE);
    uninstall(&u.places, &registry, &machine, &inside).unwrap();
    let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, [SETUP_EXE]);
    assert_eq!(machine.asked().last().cloned(), Some(format!("remove after exit {}", dir.display())));
}

/// An agent that can't be stopped (asked, waited for, ended, still there): nothing is changed, on install and on
/// uninstall, and it is said.
#[test]
fn an_agent_that_cant_be_stopped_changes_nothing() {
    let u = user("Test");
    let registry = MemoryRegistry::default();
    let old: &[(&str, &[u8])] = &[(APP_EXE, b"app v1"), (AGENT_EXE, b"agent v1"), (AGENT_BG_EXE, b"agent-bg v1")];
    install(&u.places, &registry, &Recorder::default(), old, &u.setup, &options(true)).unwrap();
    let old_value = format!("\"{}\" run --log", u.places.dir().join(AGENT_BG_EXE).display());
    registry.set_string(&u.places.run_key, AGENT_RUN_VALUE, &old_value).unwrap();
    let before = registry.values.lock().unwrap().clone();
    let shortcut = std::fs::read(u.places.shortcut()).unwrap();

    let stuck = Recorder { stuck: true, ..Recorder::default() };
    let e = install(&u.places, &registry, &stuck, V2, &u.setup, &options(false)).unwrap_err();
    assert!(e.contains("couldn't be stopped") && e.contains("Nothing was changed"), "{e}");
    assert_eq!(stuck.asked(), ["close app", "stop agent"], "nothing after the stop");
    assert_eq!(*registry.values.lock().unwrap(), before);
    for (file, bytes) in old {
        assert_eq!(std::fs::read(u.places.dir().join(file)).unwrap(), *bytes, "{file}");
    }
    assert_eq!(std::fs::read(u.places.shortcut()).unwrap(), shortcut);

    let e = uninstall(&u.places, &registry, &stuck, &u.setup).unwrap_err();
    assert!(e.contains("Nothing was changed"), "{e}");
    assert_eq!(*registry.values.lock().unwrap(), before);
    assert!(u.places.dir().join(AGENT_EXE).is_file() && u.places.shortcut().exists());
}

#[test]
fn a_running_agents_file_is_read_and_it_is_asked_to_quit() {
    let text = "{\n  \"schema\": 1,\n  \"port\": 51234,\n  \"token\": \"a1b2c3\",\n  \"pid\": 4242\n}";
    assert_eq!(agent_file(text), Some(AgentFile { pid: 4242, port: 51234, token: "a1b2c3".into() }));
    assert_eq!(agent_file("{\"port\":1,\"token\":\"a\\r\\nX: y\",\"pid\":2}"), None, "no header injection");
    assert_eq!(agent_file("{\"port\":1,\"pid\":2}"), None);
    let tasklist = "\"craft-cloud-agent-bg.exe\",\"4242\",\"Console\",\"1\",\"21,004 K\"\r\n";
    assert!(lists_agent(tasklist, 4242));
    assert!(!lists_agent(tasklist, 424));
    assert!(!lists_agent("\"notepad.exe\",\"4242\",\"Console\",\"1\",\"9,000 K\"", 4242), "a reused pid is another program");
    assert!(!lists_agent("INFO: No tasks are running which match the specified criteria.", 4242));
    // Only an answer counts: a tasklist that can't run, fails or says nothing isn't an agent that has ended.
    assert_eq!(tasklist_says(Ok((true, tasklist.into())), 4242), Ok(true));
    assert_eq!(tasklist_says(Ok((true, "INFO: No tasks are running which match the specified criteria.\r\n".into())), 4242), Ok(false));
    assert!(tasklist_says(Err("not found".into()), 4242).is_err());
    assert!(tasklist_says(Ok((false, String::new())), 4242).is_err());
    assert!(tasklist_says(Ok((true, " \r\n".into())), 4242).is_err());

    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let agent = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = vec![0u8; 1024];
        let n = stream.read(&mut request).unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}").unwrap();
        String::from_utf8_lossy(&request[..n]).into_owned()
    });
    ask_to_quit(&AgentFile { pid: 1, port, token: "a1b2c3".into() }).unwrap();
    let request = agent.join().unwrap();
    assert!(request.starts_with("POST /v1/quit HTTP/1.1\r\n"), "{request}");
    assert!(request.contains(&format!("Host: 127.0.0.1:{port}\r\n")) && request.contains("Authorization: Bearer a1b2c3\r\n"), "{request}");
}

/// Files moved aside that can't be deleted at once (here a read-only folder): the uninstall finishes, and the
/// moved-aside folder goes once the setup has ended.
#[cfg(unix)]
#[test]
fn files_moved_aside_that_cant_be_deleted_yet_go_after_the_setup() {
    use std::os::unix::fs::PermissionsExt;
    let u = user("Test");
    let registry = MemoryRegistry::default();
    install(&u.places, &registry, &Recorder::default(), V2, &u.setup, &options(false)).unwrap();
    let dir = u.places.dir();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let machine = Recorder::default();
    uninstall(&u.places, &registry, &machine, &u.setup).unwrap();
    let aside = dir.with_file_name(format!("{DIR_NAME}.removing-{}", std::process::id()));
    std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!dir.exists() && registry.get(&u.places.uninstall_key, "DisplayName").is_none());
    assert_eq!(machine.asked().last().cloned(), Some(format!("remove after exit {}", aside.display())));
}

/// A file that is held for a moment (a program ending) is removed once it is free.
#[cfg(unix)]
#[test]
fn a_file_held_for_a_moment_is_removed_once_free() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let held = dir.path().join("held");
    std::fs::create_dir(&held).unwrap();
    std::fs::write(held.join("craft-cloud.exe"), "x").unwrap();
    std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o500)).unwrap();
    let freed = held.clone();
    let freeing = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::set_permissions(&freed, std::fs::Permissions::from_mode(0o700)).unwrap();
    });
    remove_patiently(&held.join("craft-cloud.exe"), std::time::Duration::from_secs(10)).unwrap();
    freeing.join().unwrap();
    assert!(!held.join("craft-cloud.exe").exists());
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

/// Waits (up to 30 s) for `done`; whether it came.
#[cfg(windows)]
fn comes(done: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done() {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    true
}

/// A program the setup starts (the agent) shares none of its handles: with the setup's output a pipe (as from WSL),
/// the pipe ends with the start, not with the program, which runs on.
#[cfg(windows)]
#[test]
fn a_detached_start_keeps_none_of_the_setups_handles() {
    let dir = tempfile::tempdir().unwrap();
    let done = dir.path().join("done.txt");
    let cmd = std::env::var_os("ComSpec").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    // Twenty seconds of `ping`, then a file.
    let args = format!("/d /c ping -n 21 127.0.0.1 >nul & echo done> \"{}\"", done.display());
    let started = std::time::Instant::now();
    let out = crate::windows::start_detached(&cmd, &args, dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    // PowerShell's own start may take seconds on a cold machine; the program runs twenty.
    assert!(started.elapsed() < std::time::Duration::from_secs(15), "the pipe stayed open while the program ran: {:?}", started.elapsed());
    assert!(!done.exists(), "it runs on");
    assert!(comes(|| done.exists()), "and it was started");
}

/// Uninstalling from "Apps & features": the program folder goes once the setup in it has ended, however long its
/// last message stayed open (here a `ping` of seven seconds stands for the setup), not after a fixed delay. Through
/// PowerShell, and through `cmd` alone (where PowerShell can't run). A link inside the folder is removed, not followed.
#[cfg(windows)]
#[test]
fn the_program_folder_goes_once_the_setup_has_ended() {
    for with_cmd in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Craft & Cloud");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "keep").unwrap();
        std::fs::write(dir.join(SETUP_EXE), "setup").unwrap();
        std::fs::write(dir.join("sub").join("file.txt"), "file").unwrap();
        let junction = std::process::Command::new(crate::windows::system_program("cmd.exe"))
            .args(["/d", "/c", "mklink", "/J"])
            .arg(dir.join("link"))
            .arg(&outside)
            .output()
            .unwrap();
        assert!(junction.status.success(), "{junction:?}");
        let mut setup = std::process::Command::new(crate::windows::system_program("PING.EXE"))
            .args(["-n", "8", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut remover = if with_cmd {
            crate::windows::remove_after_with_cmd(setup.id(), "ping.exe", &dir)
        } else {
            crate::windows::remove_after(setup.id(), &dir)
        }
        .spawn()
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(4));
        assert!(dir.join(SETUP_EXE).exists(), "not while the setup runs (cmd: {with_cmd})");
        setup.wait().unwrap();
        remover.wait().unwrap();
        assert!(!dir.exists(), "gone once it has ended (cmd: {with_cmd})");
        assert!(outside.join("keep.txt").exists(), "the link wasn't followed (cmd: {with_cmd})");
    }
}

/// The `cmd` remover with a `tasklist` that can't run never takes the setup for ended: it waits, and the folder stays.
#[cfg(windows)]
#[test]
fn the_cmd_remover_waits_while_tasklist_cant_tell() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("Craft Cloud");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(SETUP_EXE), "setup").unwrap();
    // Three rounds of asking, a second's wait each, then it gives up (an hour in the real one).
    let started = std::time::Instant::now();
    let mut remover = crate::windows::remove_after_with_cmd_without_tasklist(3, std::process::id(), "craft-cloud-setup.exe", &dir).spawn().unwrap();
    remover.wait().unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_secs(2), "it waited each round (a broken line ends at once): {:?}", started.elapsed());
    assert!(dir.join(SETUP_EXE).exists(), "nothing was removed while it couldn't tell");
    let _ = std::fs::remove_file(std::env::temp_dir().join(format!("craft-cloud-uninstall-{}.txt", std::process::id())));
}
