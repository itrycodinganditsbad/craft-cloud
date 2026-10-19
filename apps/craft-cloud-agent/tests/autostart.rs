//! The start-at-login entries: what each OS gets, for a path with a space and a non-ASCII name (golden files in
//! `tests/autostart/`), and registering, adopting and removing them in temporary places. The Linux and macOS files are
//! plain files written by pure functions, so those tests run on every OS (Linux's `systemctl` calls are recorded, not
//! run); the registry test runs on Windows only.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use craft_cloud_agent::autostart::{Launch, Roots, Runner, linux, macos, windows};

/// Records `systemctl` calls and answers them: `systemd` says whether the user's systemd answers at all.
struct Recorder {
    systemd: bool,
    calls: Mutex<Vec<String>>,
}

impl Runner for Recorder {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
        self.calls.lock().unwrap().push(format!("{program} {}", args.join(" ")));
        Ok(self.systemd)
    }
}

fn roots(home: &Path, systemd: bool) -> (Roots, Arc<Recorder>) {
    let recorder = Arc::new(Recorder { systemd, calls: Mutex::new(Vec::new()) });
    (Roots { home: home.to_owned(), run_key: String::new(), runner: recorder.clone(), default_data: None }, recorder)
}

/// A golden file, with line endings as written (a checkout may have turned them into CRLF).
fn golden(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("autostart").join(name);
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

/// The macOS agent, with launchd writing what it catches to `launchd.log` in the default data folder.
fn mac_launch() -> Launch {
    Launch {
        exe: PathBuf::from("/Users/zoë/Library/Application Support/Craft Cloud & Co/craft-cloud-agent"),
        log: Some(PathBuf::from("/Users/zoë/Library/Application Support/craft-cloud-agent/launchd.log")),
    }
}

fn launch(exe: &str) -> Launch {
    Launch { exe: PathBuf::from(exe), log: None }
}

#[test]
fn each_os_gets_its_entry_with_the_path_quoted() {
    let unix = launch("/home/zoë/My Apps/craft-cloud-agent");
    assert_eq!(linux::unit(&unix).unwrap(), golden("craft-cloud-agent.service"));
    assert_eq!(linux::desktop_entry(&unix).unwrap(), golden("craft-cloud-agent.desktop"));
    let mac = mac_launch();
    assert_eq!(macos::plist(&mac).unwrap(), golden("ch.craftcloud.agent.plist"));
    let win = launch(r"C:\Users\Zoë\AppData\Local\Programs\Craft Cloud\craft-cloud-agent-bg.exe");
    assert_eq!(windows::run_value(&win).unwrap(), golden("windows-run-value.txt").trim_end());
}

/// The entry belongs to the agent with the default data folder only.
#[test]
fn the_entry_belongs_to_the_default_data_folder() {
    let home = tempfile::tempdir().unwrap();
    let mut roots = roots(home.path(), false).0;
    roots.default_data = Some(PathBuf::from("/home/zoë/.local/share/craft-cloud-agent"));
    assert!(roots.owns(Path::new("/home/zoë/.local/share/craft-cloud-agent")));
    assert!(!roots.owns(Path::new("/home/zoë/spare")));
    // The same folder written another way.
    assert!(roots.owns(Path::new("/home/zoë/.local/share/spare/../craft-cloud-agent")));
    assert!(roots.owns(Path::new("/home/zoë/./.local/share/craft-cloud-agent/")));
    assert!(!roots.owns(Path::new("/home/zoë/.local/share/craft-cloud-agent/../spare")));
    roots.default_data = None;
    assert!(!roots.owns(Path::new("/home/zoë/.local/share/craft-cloud-agent")), "no default: nobody's");
}

#[test]
fn a_systemd_unit_is_written_once_enabled_and_removed() {
    let home = tempfile::tempdir().unwrap();
    let (roots, calls) = roots(home.path(), true);
    let agent = launch("/opt/Craft Cloud/craft-cloud-agent");
    linux::register(&roots, &agent).unwrap();
    linux::register(&roots, &agent).unwrap();
    let units: Vec<_> = std::fs::read_dir(home.path().join(".config/systemd/user")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(units, vec![std::ffi::OsString::from(linux::UNIT)], "one unit");
    assert_eq!(std::fs::read_to_string(linux::unit_file(&roots)).unwrap(), linux::unit(&agent).unwrap());
    assert!(!linux::desktop_file(&roots).exists(), "not both");
    let calls_now = calls.calls.lock().unwrap().clone();
    assert!(calls_now.contains(&"systemctl --user daemon-reload".to_owned()), "{calls_now:?}");
    assert!(calls_now.contains(&format!("systemctl --user enable {}", linux::UNIT)), "{calls_now:?}");
    assert!(linux::is_registered(&roots).unwrap());

    linux::unregister(&roots).unwrap();
    assert!(!linux::unit_file(&roots).exists());
    assert!(calls.calls.lock().unwrap().contains(&format!("systemctl --user disable {}", linux::UNIT)));
    assert!(!calls.calls.lock().unwrap().iter().any(|c| c.contains(" stop ")), "a running agent is never stopped");
    linux::unregister(&roots).unwrap();
}

/// lptp1's unit, made by hand before the agent could: adopted, rewritten with this agent's path.
#[test]
fn a_hand_made_unit_is_adopted() {
    let home = tempfile::tempdir().unwrap();
    let (roots, _) = roots(home.path(), true);
    let unit = linux::unit_file(&roots);
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "[Service]\nExecStart=/home/fabio/bin/craft-cloud-agent run\n[Install]\nWantedBy=default.target\n").unwrap();
    let agent = launch("/home/fabio/.local/lib/craft-cloud/craft-cloud-agent");
    linux::register(&roots, &agent).unwrap();
    let written = std::fs::read_to_string(&unit).unwrap();
    assert_eq!(written, linux::unit(&agent).unwrap());
    assert!(!written.contains("/home/fabio/bin/"), "{written}");
    assert_eq!(std::fs::read_dir(unit.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn without_systemd_an_xdg_autostart_entry_is_written() {
    let home = tempfile::tempdir().unwrap();
    let (roots, _) = roots(home.path(), false);
    let agent = launch("/home/zoë/My Apps/craft-cloud-agent");
    linux::register(&roots, &agent).unwrap();
    linux::register(&roots, &agent).unwrap();
    assert_eq!(std::fs::read_to_string(linux::desktop_file(&roots)).unwrap(), golden("craft-cloud-agent.desktop"));
    assert_eq!(std::fs::read_dir(home.path().join(".config/autostart")).unwrap().count(), 1);
    assert!(!linux::unit_file(&roots).exists());
    assert!(linux::is_registered(&roots).unwrap());
    linux::unregister(&roots).unwrap();
    assert!(!linux::desktop_file(&roots).exists());
    assert!(!linux::is_registered(&roots).unwrap());
    linux::unregister(&roots).unwrap();
}

#[test]
fn a_launch_agent_is_written_once_and_removed() {
    let home = tempfile::tempdir().unwrap();
    let (roots, _) = roots(home.path(), false);
    let agent = mac_launch();
    assert!(!macos::is_registered(&roots).unwrap());
    macos::register(&roots, &agent).unwrap();
    macos::register(&roots, &agent).unwrap();
    assert_eq!(std::fs::read_to_string(macos::plist_file(&roots)).unwrap(), golden("ch.craftcloud.agent.plist"));
    assert_eq!(std::fs::read_dir(home.path().join("Library/LaunchAgents")).unwrap().count(), 1);
    assert!(macos::is_registered(&roots).unwrap());
    macos::unregister(&roots).unwrap();
    assert!(!macos::is_registered(&roots).unwrap());
    macos::unregister(&roots).unwrap();
}

/// The registry value, in a throwaway key under `HKCU\Software\Craft Cloud Test` that is deleted after.
#[cfg(windows)]
#[test]
fn a_run_value_is_written_once_and_removed() {
    struct Key(String);
    impl Drop for Key {
        fn drop(&mut self) {
            let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
        }
    }
    let key = Key(format!(r"Software\Craft Cloud Test\{}", uuid::Uuid::now_v7()));
    let home = tempfile::tempdir().unwrap();
    let roots = Roots { run_key: key.0.clone(), ..roots(home.path(), false).0 };
    let agent = launch(r"C:\Users\Zoë\AppData\Local\Programs\Craft Cloud\craft-cloud-agent-bg.exe");
    assert!(!windows::is_registered(&roots).unwrap());
    windows::unregister(&roots).unwrap();
    windows::register(&roots, &agent).unwrap();
    windows::register(&roots, &agent).unwrap();
    assert_eq!(windows::read_value(&roots).unwrap().as_deref(), Some(golden("windows-run-value.txt").trim_end()));
    let values = windows_registry::CURRENT_USER.open(&key.0).unwrap().values().unwrap().count();
    assert_eq!(values, 1, "one value");
    assert!(windows::is_registered(&roots).unwrap());
    windows::unregister(&roots).unwrap();
    assert!(!windows::is_registered(&roots).unwrap());
    windows::unregister(&roots).unwrap();
}
