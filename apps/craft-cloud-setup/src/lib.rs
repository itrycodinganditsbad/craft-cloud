//! Craft Cloud's Windows setup: installs `craft-cloud.exe`, `craft-cloud-agent.exe` and `craft-cloud-agent-bg.exe`
//! (with `LICENSES.txt`) for this user into `%LOCALAPPDATA%\Programs\Craft Cloud` (no admin rights), with an "Apps & features" entry and a
//! Start-menu shortcut (unless `--no-start-menu`); `--uninstall` takes them away again.
//!
//! - **Install** ([`install`]): the app is closed and a running agent stopped first: asked to quit through its local
//!   API (it finishes the pass under way), waited for up to a minute, then ended, and its process checked gone. An
//!   agent that can't be stopped stops the install before anything is changed. Then the files, the Uninstall key
//!   (`HKCU\…\Uninstall\CraftCloud`, its `UninstallString` calling this setup, copied into the program folder, with
//!   `--uninstall`), and the shortcut. An agent that ran is started again (and waited for), also when the install
//!   fails after the stop. An older install in
//!   `%LOCALAPPDATA%\Programs\craft-cloud` whose agent the `Run` value starts is moved: the new agent's
//!   `autostart on` points the value at the new folder (read back to check); the old folder's files are left alone.
//! - **Uninstall** ([`uninstall`]): closes the app and stops the agent as above (fine when it isn't running), removes their
//!   `Run` values (`Craft Cloud`, `Craft Cloud tray`) where they start this install, the shortcut, the app's
//!   single-instance files, the program folder and the Uninstall key. The synced folder and the agent's data folder (the sign-in) stay.
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
pub const FILES: [&str; 4] = ["craft-cloud.exe", "craft-cloud-agent.exe", "craft-cloud-agent-bg.exe", LICENSES];
pub const APP_EXE: &str = "craft-cloud.exe";
pub const AGENT_EXE: &str = "craft-cloud-agent.exe";
pub const AGENT_BG_EXE: &str = "craft-cloud-agent-bg.exe";
pub const SETUP_EXE: &str = "craft-cloud-setup.exe";
/// Craft Cloud's licences and those of the third-party material in it (written by `cargo xtask package`).
pub const LICENSES: &str = "LICENSES.txt";
/// The `Run` values: the agent's and the app's.
pub const AGENT_RUN_VALUE: &str = "Craft Cloud";
pub const APP_RUN_VALUE: &str = "Craft Cloud tray";
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\CraftCloud";
/// The app's folder for its single-instance files, next to the agent's data folder.
pub const APP_FILES_DIR: &str = "craft-cloud-app";
/// The Start-menu shortcut's file.
pub const SHORTCUT: &str = "Craft Cloud.lnk";

/// The payload: the installed files' names and bytes, built in by `cargo xtask package` (empty in other builds).
#[cfg(payload)]
pub const PAYLOAD: &[(&str, &[u8])] = &[
    (APP_EXE, include_bytes!(concat!(env!("CRAFT_CLOUD_SETUP_PAYLOAD"), "/craft-cloud.exe"))),
    (AGENT_EXE, include_bytes!(concat!(env!("CRAFT_CLOUD_SETUP_PAYLOAD"), "/craft-cloud-agent.exe"))),
    (AGENT_BG_EXE, include_bytes!(concat!(env!("CRAFT_CLOUD_SETUP_PAYLOAD"), "/craft-cloud-agent-bg.exe"))),
    (LICENSES, include_bytes!(concat!(env!("CRAFT_CLOUD_SETUP_PAYLOAD"), "/LICENSES.txt"))),
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
    /// The agent's data folder, where a running agent's `local-api.json` names its pid, port and token.
    pub agent_data: PathBuf,
    /// The app's single-instance files (`%LOCALAPPDATA%\craft-cloud-app`, next to the agent's data folder).
    pub app_files: PathBuf,
    /// How long a file still held by a program that is ending is tried again for.
    pub patience: std::time::Duration,
}

impl Places {
    pub fn for_user() -> Result<Self, String> {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute()).ok_or(format!("%{name}% isn't set"));
        let local = var("LOCALAPPDATA")?;
        // As the agent finds it: `CRAFT_CLOUD_AGENT_DIR`, else `%LOCALAPPDATA%\craft-cloud-agent`.
        let agent_data = var("CRAFT_CLOUD_AGENT_DIR").unwrap_or_else(|_| local.join("craft-cloud-agent"));
        // As the app finds it: next to the agent's data folder.
        let app_files = agent_data.parent().unwrap_or(&local).join(APP_FILES_DIR);
        Ok(Self {
            programs: local.join("Programs"),
            start_menu: var("APPDATA")?.join("Microsoft").join("Windows").join("Start Menu").join("Programs"),
            run_key: RUN_KEY.to_owned(),
            uninstall_key: UNINSTALL_KEY.to_owned(),
            agent_data,
            app_files,
            patience: std::time::Duration::from_secs(10),
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

/// How stopping the agent went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStop {
    /// No agent was running.
    NotRunning,
    /// One was running, and its process has ended.
    Stopped,
    /// One runs, and couldn't be stopped (why).
    StillRunning(String),
    /// One was asked to quit (or ended), and then whether it has ended couldn't be told (why): it may be stopped, so
    /// it is started again.
    Unknown(String),
}

/// Running programs.
pub trait Machine {
    /// Stops the agent whose data folder is `data`: asks it to quit (it finishes the pass under way), waits up to a
    /// minute for its process to end, then ends it (the agent is safe to stop at any point: its writes are atomic),
    /// and checks it is gone.
    fn stop_agent(&self, data: &Path) -> AgentStop;
    /// Closes the app (`craft-cloud.exe`) if it runs.
    fn close_app(&self);
    /// Runs `agent` with `args` (`autostart on`); whether it succeeded, or why not.
    fn run_agent(&self, agent: &Path, args: &[&str]) -> Result<(), String>;
    /// Starts the background agent `agent_bg` (`run --log`), detached, and waits (a few seconds) for it to answer for
    /// the data folder `data`.
    fn start_agent(&self, agent_bg: &Path, data: &Path) -> Result<(), String>;
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

    // What runs is stopped first: an exe in use can't be replaced. The app goes before the agent (an open app's Try
    // again would start the old agent again in between); the agent finishes the pass under way.
    // Only once the old agent's process is gone is anything changed: an agent still running would keep its exes
    // locked and answer the new agent's `autostart on` itself (registering its own, old, program).
    machine.close_app();
    let agent_was_running = match machine.stop_agent(&places.agent_data) {
        AgentStop::NotRunning => false,
        AgentStop::Stopped => true,
        AgentStop::StillRunning(why) => return Err(still_running(&why)),
        AgentStop::Unknown(why) => return Err(restarted_after(places, registry, machine, &unknown_stop(&why))),
    };

    // From here on the agent is stopped: if the install fails, the agent is started again, so this computer keeps
    // syncing.
    match install_stopped(places, registry, machine, payload, setup, options) {
        Ok((moved_from, mut warnings)) => {
            if agent_was_running && let Err(e) = machine.start_agent(&dir.join(AGENT_BG_EXE), &places.agent_data) {
                warnings.push(format!("the agent couldn't be started again ({e}); it starts at the next login"));
            }
            Ok(Installed { dir, agent_was_running, moved_from, warnings })
        }
        Err(e) if agent_was_running => Err(restarted_after(places, registry, machine, &e)),
        Err(e) => Err(e),
    }
}

/// The install's changes, once nothing runs: the files, the move of an older install's start at login, the "Apps &
/// features" entry and the shortcut. Where an older install was moved from, and the warnings.
fn install_stopped(
    places: &Places,
    registry: &dyn Registry,
    machine: &dyn Machine,
    payload: &[(&str, &[u8])],
    setup: &Path,
    options: &Options,
) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let dir = places.dir();
    let old = places.programs.join(OLD_DIR_NAME);
    let mut warnings = Vec::new();
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
    // writes its own entry). The old files stay. The value is read back: only a value that starts the new agent is a
    // move.
    let mut moved_from = None;
    let run_value = registry.get(&places.run_key, AGENT_RUN_VALUE);
    if old.join(AGENT_EXE).is_file() && run_value.as_deref().is_some_and(|v| starts_from(v, &old)) {
        let moved =
            machine.run_agent(&dir.join(AGENT_EXE), &["autostart", "on"]).and_then(|()| match registry.get(&places.run_key, AGENT_RUN_VALUE) {
                Some(v) if starts_from(&v, &dir) => Ok(()),
                other => Err(format!("the Run value is still {}", other.unwrap_or_else(|| "missing".into()))),
            });
        match moved {
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

    Ok((moved_from, warnings))
}

/// The install failed (`failure`) after the agent was stopped: it is started again, from the folder its start at
/// login names (the new one or the older install's), else from the new folder, else the older one. What is said.
fn restarted_after(places: &Places, registry: &dyn Registry, machine: &dyn Machine, failure: &str) -> String {
    let (dir, old) = (places.dir(), places.programs.join(OLD_DIR_NAME));
    let run_value = registry.get(&places.run_key, AGENT_RUN_VALUE).unwrap_or_default();
    let named = [&dir, &old].into_iter().find(|d| starts_from(&run_value, d));
    let from = named.into_iter().chain([&dir, &old]).find(|d| d.join(AGENT_BG_EXE).is_file());
    let started = match from {
        Some(from) => machine.start_agent(&from.join(AGENT_BG_EXE), &places.agent_data),
        None => Err("no agent is there to start".into()),
    };
    match started {
        Ok(()) => format!("{failure}. The agent was started again."),
        Err(e) => format!("{failure}. The agent couldn't be started again ({e}); it starts at the next login."),
    }
}

/// Removes the install at `places`; `running` is this setup's own exe (in the program folder when started from
/// "Apps & features": then the folder goes once this process has ended).
pub fn uninstall(places: &Places, registry: &dyn Registry, machine: &dyn Machine, running: &Path) -> Result<(), String> {
    machine.close_app();
    let was_running = match machine.stop_agent(&places.agent_data) {
        AgentStop::NotRunning => false,
        AgentStop::Stopped => true,
        AgentStop::StillRunning(why) => return Err(still_running(&why)),
        AgentStop::Unknown(why) => return Err(restarted_after(places, registry, machine, &unknown_stop(&why))),
    };
    let mut removed_values = Vec::new();
    uninstall_stopped(places, registry, machine, running, &mut removed_values)
        .map_err(|e| put_back(places, registry, machine, was_running, &removed_values, &e))
}

/// The uninstall's changes once nothing runs; the `Run` values it removed (name, value) go into `removed_values`.
fn uninstall_stopped(
    places: &Places,
    registry: &dyn Registry,
    machine: &dyn Machine,
    running: &Path,
    removed_values: &mut Vec<(&'static str, String)>,
) -> Result<(), String> {
    let dir = places.dir();
    for value in [AGENT_RUN_VALUE, APP_RUN_VALUE] {
        if let Some(v) = registry.get(&places.run_key, value).filter(|v| starts_from(v, &dir)) {
            registry.remove_value(&places.run_key, value)?;
            removed_values.push((value, v));
        }
    }
    let shortcut = places.shortcut();
    if shortcut.exists() {
        std::fs::remove_file(&shortcut).map_err(io(&shortcut))?;
    }
    // The files before the "Apps & features" entry: if one can't go, the entry stays, to run it again.
    if dir.exists() {
        let running_inside = running.parent().is_some_and(|p| same_path(p, &dir));
        if running_inside {
            for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
                let path = entry.map_err(io(&dir))?.path();
                if !same_path(&path, running) {
                    remove_patiently(&path, places.patience)?;
                }
            }
            machine.remove_after_exit(&dir)?;
        } else {
            remove_patiently(&dir, places.patience)?;
        }
    }
    registry.remove_key(&places.uninstall_key)?;
    remove_app_files(&places.app_files);
    Ok(())
}

/// The uninstall failed (`failure`) after the agent was stopped and maybe its start at login removed: while the
/// agent's program is still there, its `Run` values are put back and, if it ran, it is started again, so this
/// computer keeps syncing until the uninstall is run again. What is said.
fn put_back(
    places: &Places,
    registry: &dyn Registry,
    machine: &dyn Machine,
    was_running: bool,
    removed_values: &[(&'static str, String)],
    failure: &str,
) -> String {
    if !places.dir().join(AGENT_BG_EXE).is_file() {
        return format!("{failure}. Run the uninstall again to finish it.");
    }
    for (name, value) in removed_values {
        let _ = registry.set_string(&places.run_key, name, value);
    }
    if was_running {
        format!("{} Start at login was put back.", restarted_after(places, registry, machine, failure))
    } else {
        format!("{failure}. Start at login was put back.")
    }
}

/// The app's single-instance files in `app_files`, the folder itself once empty, and the loose ones older versions
/// kept beside it. Only these names: anything else there stays. Last, and best effort: a file an app that couldn't be
/// closed still holds stays, and is harmless.
fn remove_app_files(app_files: &Path) {
    for name in ["window.lock", "window.port", "window.port.tmp", "tray.lock", "tray.port", "tray.port.tmp"] {
        let _ = std::fs::remove_file(app_files.join(name));
    }
    let _ = std::fs::remove_dir(app_files);
    if let Some(parent) = app_files.parent() {
        for old in ["craft-cloud-app.lock", "craft-cloud-app.port", "craft-cloud-tray.lock", "craft-cloud-tray.port"] {
            let _ = std::fs::remove_file(parent.join(old));
        }
    }
}

/// What is said when the agent was asked to quit and then whether it ended couldn't be told: nothing else was changed
/// (and it is started again).
fn unknown_stop(why: &str) -> String {
    format!(
        "Craft Cloud's agent was asked to quit, but whether it has stopped can't be told ({why}). Nothing else was changed: try again in a minute"
    )
}

/// What is said when the agent can't be stopped: nothing was changed.
fn still_running(why: &str) -> String {
    format!("Craft Cloud's agent is still running and couldn't be stopped ({why}). Nothing was changed: try again in a minute.")
}

/// A running agent's `local-api.json`: its pid, and the port and token of its local API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFile {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

/// `text` (`local-api.json`, `{"schema":1,"port":…,"token":"…","pid":…}`) read for what the setup needs; `None` if it
/// doesn't read. A token that isn't plain letters and digits is refused (it goes into a header).
pub fn agent_file(text: &str) -> Option<AgentFile> {
    fn field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
        let at = text.find(&format!("\"{name}\""))? + name.len() + 2;
        let rest = text.get(at..)?.trim_start().strip_prefix(':')?.trim_start();
        match rest.strip_prefix('"') {
            Some(quoted) => quoted.get(..quoted.find('"')?),
            None => rest.get(..rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())),
        }
    }
    let token = field(text, "token")?;
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(AgentFile { pid: field(text, "pid")?.parse().ok()?, port: field(text, "port")?.parse().ok()?, token: token.to_owned() })
}

/// Whether `tasklist /FO CSV /NH` output lists `pid` as an agent (`craft-cloud-agent*.exe`): a pid of an agent that
/// has ended may belong to another program by now, which is never touched.
pub fn lists_agent(csv: &str, pid: u32) -> bool {
    csv.lines().any(|line| {
        let mut columns = line.split("\",\"");
        let image = columns.next().unwrap_or_default().trim_start_matches('"').to_lowercase();
        image.starts_with("craft-cloud-agent") && columns.next() == Some(pid.to_string().as_str())
    })
}

/// What a `tasklist /FI "PID eq <pid>" /FO CSV /NH` run says about the agent `pid`: whether it runs, or why that
/// can't be told. Only an answer counts: `tasklist` that couldn't run, failed or said nothing (it always prints a
/// row or a note that nothing matched) is never taken for an agent that has ended.
pub fn tasklist_says(ran: Result<(bool, String), String>, pid: u32) -> Result<bool, String> {
    match ran {
        Err(e) => Err(format!("tasklist can't run: {e}")),
        Ok((false, _)) => Err("tasklist failed".into()),
        Ok((true, out)) if out.trim().is_empty() => Err("tasklist answered nothing".into()),
        Ok((true, out)) => Ok(lists_agent(&out, pid)),
    }
}

/// Asks the agent at `file`'s port to quit (`POST /v1/quit` on its local API, as an app does). Whether it said yes.
pub fn ask_to_quit(file: &AgentFile) -> Result<(), String> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], file.port));
    let timeout = std::time::Duration::from_secs(5);
    let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let request = format!(
        "POST /v1/quit HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        file.port, file.token
    );
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut head = [0u8; 64];
    let n = stream.read(&mut head).map_err(|e| e.to_string())?;
    let line = String::from_utf8_lossy(head.get(..n).unwrap_or_default()).into_owned();
    if line.starts_with("HTTP/1.1 200") { Ok(()) } else { Err(format!("it answered {}", line.lines().next().unwrap_or_default())) }
}

/// Removes `path` (a file or a folder), trying again for up to `patience` while it fails: a program that has just
/// been stopped may still hold its exe for a moment.
pub fn remove_patiently(path: &Path, patience: std::time::Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + patience;
    loop {
        match remove_path(path) {
            Ok(()) => return Ok(()),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(format!("{e} (is it still running?)")),
        }
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
