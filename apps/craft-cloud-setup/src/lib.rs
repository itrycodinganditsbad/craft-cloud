//! Craft Cloud's Windows setup: installs `craft-cloud.exe`, `craft-cloud-agent.exe` and `craft-cloud-agent-bg.exe` for
//! this user into `%LOCALAPPDATA%\Programs\Craft Cloud` (no admin rights), with an "Apps & features" entry and a
//! Start-menu shortcut (unless `--no-start-menu`); `--uninstall` takes them away again.
//!
//! - **Install** ([`install`]): the app is closed and a running agent stopped first, through its own command line
//!   (`quit`, which lets the pass under way finish), so their files can be replaced; then the files, the Uninstall key
//!   (`HKCU\…\Uninstall\CraftCloud`, its `UninstallString` calling this setup, copied into the program folder, with
//!   `--uninstall`), and the shortcut. An agent that ran is started again. An older install in
//!   `%LOCALAPPDATA%\Programs\craft-cloud` whose agent the `Run` value starts is moved: the new agent's
//!   `autostart on` points the value at the new folder; the old folder's files are left alone.
//! - **Uninstall** ([`uninstall`]): stops the agent (`quit`; fine when it isn't running) and the app, removes their
//!   `Run` values (`Craft Cloud`, `Craft Cloud tray`) where they start this install, the shortcut, the program folder
//!   and the Uninstall key. The synced folder and the agent's data folder (the sign-in) stay.
//!
//! The registry and everything that runs a program are behind [`Registry`] and [`Machine`], so the logic is tested
//! against an in-memory registry and temporary folders on every OS, and against the real registry (under a throwaway
//! key) on Windows.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};

#[cfg(windows)]
pub mod windows;

/// The program folder's name under `%LOCALAPPDATA%\Programs`.
pub const DIR_NAME: &str = "Craft Cloud";
/// An older install's folder (made by hand, before there was a setup).
pub const OLD_DIR_NAME: &str = "craft-cloud";
/// The files installed, besides the setup itself.
pub const FILES: [&str; 3] = ["craft-cloud.exe", "craft-cloud-agent.exe", "craft-cloud-agent-bg.exe"];
pub const APP_EXE: &str = "craft-cloud.exe";
pub const AGENT_EXE: &str = "craft-cloud-agent.exe";
pub const AGENT_BG_EXE: &str = "craft-cloud-agent-bg.exe";
pub const SETUP_EXE: &str = "craft-cloud-setup.exe";
/// The `Run` values: the agent's and the app's.
pub const AGENT_RUN_VALUE: &str = "Craft Cloud";
pub const APP_RUN_VALUE: &str = "Craft Cloud tray";
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\CraftCloud";
/// The Start-menu shortcut's file.
pub const SHORTCUT: &str = "Craft Cloud.lnk";

/// The payload: the installed files' names and bytes, built in by `cargo xtask package` (empty in other builds).
#[cfg(payload)]
pub const PAYLOAD: &[(&str, &[u8])] = &[
    (APP_EXE, include_bytes!(concat!(env!("CRAFT_CLOUD_SETUP_PAYLOAD"), "/craft-cloud.exe"))),
    (AGENT_EXE, include_bytes!(concat!(env!("CRAFT_CLOUD_SETUP_PAYLOAD"), "/craft-cloud-agent.exe"))),
    (AGENT_BG_EXE, include_bytes!(concat!(env!("CRAFT_CLOUD_SETUP_PAYLOAD"), "/craft-cloud-agent-bg.exe"))),
];
#[cfg(not(payload))]
pub const PAYLOAD: &[(&str, &[u8])] = &[];

/// Where things go. [`Places::for_user`] is this user's.
#[derive(Debug, Clone)]
pub struct Places {
    /// `%LOCALAPPDATA%\Programs`.
    pub programs: PathBuf,
    /// The Start menu's programs folder (`%APPDATA%\Microsoft\Windows\Start Menu\Programs`).
    pub start_menu: PathBuf,
    /// The keys under `HKEY_CURRENT_USER`.
    pub run_key: String,
    pub uninstall_key: String,
}

impl Places {
    pub fn for_user() -> Result<Self, String> {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute()).ok_or(format!("%{name}% isn't set"));
        Ok(Self {
            programs: var("LOCALAPPDATA")?.join("Programs"),
            start_menu: var("APPDATA")?.join("Microsoft").join("Windows").join("Start Menu").join("Programs"),
            run_key: RUN_KEY.to_owned(),
            uninstall_key: UNINSTALL_KEY.to_owned(),
        })
    }

    /// The program folder.
    pub fn dir(&self) -> PathBuf {
        self.programs.join(DIR_NAME)
    }

    pub fn shortcut(&self) -> PathBuf {
        self.start_menu.join(SHORTCUT)
    }
}

/// The values of `HKEY_CURRENT_USER` the setup reads and writes.
pub trait Registry {
    fn get(&self, key: &str, name: &str) -> Option<String>;
    fn set_string(&self, key: &str, name: &str, value: &str) -> Result<(), String>;
    fn set_dword(&self, key: &str, name: &str, value: u32) -> Result<(), String>;
    /// Removes a value; one that isn't there is fine.
    fn remove_value(&self, key: &str, name: &str) -> Result<(), String>;
    /// Removes a key and everything under it; one that isn't there is fine.
    fn remove_key(&self, key: &str) -> Result<(), String>;
}

/// Running programs.
pub trait Machine {
    /// Stops the agent through `agent`'s command line (`quit`, again until no agent answers, for at most a while).
    /// Whether one was running.
    fn quit_agent(&self, agent: &Path) -> bool;
    /// Closes the app (`craft-cloud.exe`) if it runs.
    fn close_app(&self);
    /// Runs `agent` with `args` (`autostart on`); whether it succeeded, or why not.
    fn run_agent(&self, agent: &Path, args: &[&str]) -> Result<(), String>;
    /// Starts the background agent `agent_bg` (`run --log`), detached.
    fn start_agent(&self, agent_bg: &Path) -> Result<(), String>;
    /// Writes a shortcut at `lnk` to `target`.
    fn shortcut(&self, lnk: &Path, target: &Path) -> Result<(), String>;
    /// Removes the folder `dir` once this process has ended (it holds this setup's own exe).
    fn remove_after_exit(&self, dir: &Path) -> Result<(), String>;
}

/// How to install.
#[derive(Debug, Clone)]
pub struct Options {
    pub start_menu: bool,
    pub version: String,
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub dir: PathBuf,
    /// An agent was running (it was stopped for the files, and started again).
    pub agent_was_running: bool,
    /// An older install's start at login was moved here.
    pub moved_from: Option<PathBuf>,
    /// What went wrong on the way that didn't stop the install (said at the end).
    pub warnings: Vec<String>,
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", path.display())
}

/// Writes `bytes` to `path` through a temporary file renamed into place. A file still held by a program that is
/// ending (Windows keeps an exe locked for a moment) is tried again for a few seconds.
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension("new");
    std::fs::write(&temp, bytes).map_err(io(&temp))?;
    let mut tries = 0;
    loop {
        match std::fs::rename(&temp, path) {
            Ok(()) => return Ok(()),
            Err(_) if tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(format!("{} can't be replaced (is it still running?): {e}", path.display()));
            }
        }
    }
}

/// Whether the command line `command` (a `Run` value) starts a program in `dir`.
pub fn starts_from(command: &str, dir: &Path) -> bool {
    let program = command.trim().trim_start_matches('"');
    let dir = dir.display().to_string();
    let lower = |s: &str| s.to_lowercase().replace('/', "\\");
    let (program, dir) = (lower(program), lower(&dir));
    program.strip_prefix(&dir).is_some_and(|rest| rest.starts_with('\\'))
}

/// Installs `payload` (each file's name and bytes) and `setup` (this setup's own exe, copied in for "Apps & features")
/// at `places`.
pub fn install(
    places: &Places,
    registry: &dyn Registry,
    machine: &dyn Machine,
    payload: &[(&str, &[u8])],
    setup: &Path,
    options: &Options,
) -> Result<Installed, String> {
    if payload.is_empty() {
        return Err("this setup was built without Craft Cloud in it (build it with `cargo xtask package`)".into());
    }
    let dir = places.dir();
    let old = places.programs.join(OLD_DIR_NAME);
    let mut warnings = Vec::new();

    // What runs is stopped first: an exe in use can't be replaced. The app goes before the agent (an open app's Try
    // again would start the old agent again in between); the agent finishes the pass under way.
    machine.close_app();
    let agent_cli = [dir.join(AGENT_EXE), old.join(AGENT_EXE)].into_iter().find(|p| p.is_file());
    let agent_was_running = agent_cli.as_deref().is_some_and(|agent| machine.quit_agent(agent));

    std::fs::create_dir_all(&dir).map_err(io(&dir))?;
    for (name, bytes) in payload {
        write_replacing(&dir.join(name), bytes)?;
    }
    let setup_copy = dir.join(SETUP_EXE);
    if setup != setup_copy {
        let bytes = std::fs::read(setup).map_err(io(setup))?;
        write_replacing(&setup_copy, &bytes)?;
    }

    // The older install's agent starts at login: the new one takes that over (no agent runs now, so the new agent
    // writes its own entry). The old files stay.
    let mut moved_from = None;
    let run_value = registry.get(&places.run_key, AGENT_RUN_VALUE);
    if old.join(AGENT_EXE).is_file() && run_value.as_deref().is_some_and(|v| starts_from(v, &old)) {
        match machine.run_agent(&dir.join(AGENT_EXE), &["autostart", "on"]) {
            Ok(()) => moved_from = Some(old.clone()),
            Err(e) => warnings.push(format!("start at login still runs the old agent in {} ({e})", old.display())),
        }
    }

    let key = &places.uninstall_key;
    let uninstall = format!("\"{}\" --uninstall", setup_copy.display());
    registry.set_string(key, "DisplayName", "Craft Cloud")?;
    registry.set_string(key, "DisplayVersion", &options.version)?;
    registry.set_string(key, "DisplayIcon", &dir.join(APP_EXE).display().to_string())?;
    registry.set_string(key, "InstallLocation", &dir.display().to_string())?;
    registry.set_string(key, "UninstallString", &uninstall)?;
    registry.set_string(key, "QuietUninstallString", &format!("{uninstall} --quiet"))?;
    registry.set_dword(key, "NoModify", 1)?;
    registry.set_dword(key, "NoRepair", 1)?;
    let bytes: usize = payload.iter().map(|(_, b)| b.len()).sum();
    registry.set_dword(key, "EstimatedSize", u32::try_from(bytes / 1024).unwrap_or(u32::MAX))?;

    let shortcut = places.shortcut();
    if options.start_menu {
        if let Some(parent) = shortcut.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        if let Err(e) = machine.shortcut(&shortcut, &dir.join(APP_EXE)) {
            warnings.push(format!("no Start-menu entry ({e})"));
        }
    } else if shortcut.exists() {
        std::fs::remove_file(&shortcut).map_err(io(&shortcut))?;
    }

    if agent_was_running && let Err(e) = machine.start_agent(&dir.join(AGENT_BG_EXE)) {
        warnings.push(format!("the agent couldn't be started again ({e}); it starts at the next login"));
    }
    Ok(Installed { dir, agent_was_running, moved_from, warnings })
}

/// Removes the install at `places`; `running` is this setup's own exe (in the program folder when started from
/// "Apps & features": then the folder goes once this process has ended).
pub fn uninstall(places: &Places, registry: &dyn Registry, machine: &dyn Machine, running: &Path) -> Result<(), String> {
    let dir = places.dir();
    machine.close_app();
    let agent = dir.join(AGENT_EXE);
    if agent.is_file() {
        machine.quit_agent(&agent);
    }
    for value in [AGENT_RUN_VALUE, APP_RUN_VALUE] {
        if registry.get(&places.run_key, value).as_deref().is_some_and(|v| starts_from(v, &dir)) {
            registry.remove_value(&places.run_key, value)?;
        }
    }
    let shortcut = places.shortcut();
    if shortcut.exists() {
        std::fs::remove_file(&shortcut).map_err(io(&shortcut))?;
    }
    registry.remove_key(&places.uninstall_key)?;
    if !dir.exists() {
        return Ok(());
    }
    let running_inside = running.parent().is_some_and(|p| same_path(p, &dir));
    if running_inside {
        for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
            let path = entry.map_err(io(&dir))?.path();
            if !same_path(&path, running) {
                remove_path(&path)?;
            }
        }
        machine.remove_after_exit(&dir)
    } else {
        std::fs::remove_dir_all(&dir).map_err(io(&dir))
    }
}

fn remove_path(path: &Path) -> Result<(), String> {
    if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }.map_err(io(path))
}

/// The same path, as Windows compares them (case doesn't matter).
fn same_path(a: &Path, b: &Path) -> bool {
    a.display().to_string().to_lowercase() == b.display().to_string().to_lowercase()
}

/// An in-memory registry, for tests.
#[derive(Debug, Default)]
pub struct MemoryRegistry {
    pub values: std::sync::Mutex<std::collections::BTreeMap<(String, String), String>>,
}

impl MemoryRegistry {
    fn values(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<(String, String), String>> {
        self.values.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Registry for MemoryRegistry {
    fn get(&self, key: &str, name: &str) -> Option<String> {
        self.values().get(&(key.to_owned(), name.to_owned())).cloned()
    }
    fn set_string(&self, key: &str, name: &str, value: &str) -> Result<(), String> {
        self.values().insert((key.to_owned(), name.to_owned()), value.to_owned());
        Ok(())
    }
    fn set_dword(&self, key: &str, name: &str, value: u32) -> Result<(), String> {
        self.values().insert((key.to_owned(), name.to_owned()), value.to_string());
        Ok(())
    }
    fn remove_value(&self, key: &str, name: &str) -> Result<(), String> {
        self.values().remove(&(key.to_owned(), name.to_owned()));
        Ok(())
    }
    fn remove_key(&self, key: &str) -> Result<(), String> {
        self.values().retain(|(k, _), _| k != key && !k.starts_with(&format!("{key}\\")));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
