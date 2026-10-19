//! The real registry (`HKEY_CURRENT_USER`) and programs, on Windows.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{APP_EXE, AgentStop, Machine, Registry};

/// No console window for what the setup runs (`CREATE_NO_WINDOW`).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Detached from the setup: a process group of its own (`CREATE_NEW_PROCESS_GROUP`).
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// `ERROR_FILE_NOT_FOUND` as an `HRESULT`: the key or value isn't there.
const NOT_FOUND: i32 = -2_147_024_894;

fn quiet(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    command
}

/// `HKEY_CURRENT_USER`.
pub struct UserRegistry;

impl Registry for UserRegistry {
    fn get(&self, key: &str, name: &str) -> Option<String> {
        windows_registry::CURRENT_USER.open(key).ok()?.get_string(name).ok()
    }

    fn set_string(&self, key: &str, name: &str, value: &str) -> Result<(), String> {
        let key = windows_registry::CURRENT_USER.create(key).map_err(|e| e.message())?;
        key.set_string(name, value).map_err(|e| e.message())
    }

    fn set_dword(&self, key: &str, name: &str, value: u32) -> Result<(), String> {
        let key = windows_registry::CURRENT_USER.create(key).map_err(|e| e.message())?;
        key.set_u32(name, value).map_err(|e| e.message())
    }

    fn remove_value(&self, key: &str, name: &str) -> Result<(), String> {
        let key = match windows_registry::CURRENT_USER.options().read().write().open(key) {
            Ok(key) => key,
            Err(e) if e.code().0 == NOT_FOUND => return Ok(()),
            Err(e) => return Err(e.message()),
        };
        match key.remove_value(name) {
            Err(e) if e.code().0 != NOT_FOUND => Err(e.message()),
            _ => Ok(()),
        }
    }

    fn remove_key(&self, key: &str) -> Result<(), String> {
        match windows_registry::CURRENT_USER.remove_tree(key) {
            Err(e) if e.code().0 != NOT_FOUND => Err(e.message()),
            _ => Ok(()),
        }
    }
}

/// This computer.
pub struct ThisMachine;

/// Whether the process `pid` runs and is an agent (`craft-cloud-agent*.exe`): a pid of an agent that has ended may
/// already belong to another program, which is never touched. An error when `tasklist` can't tell.
fn agent_alive(pid: u32) -> Result<bool, String> {
    let ran = quiet(Path::new("tasklist"))
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stdout(Stdio::piped())
        .output()
        .map(|out| (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
        .map_err(|e| e.to_string());
    crate::tasklist_says(ran, pid)
}

/// Waits up to `within` for the agent `pid` to end; an error as soon as `tasklist` can't tell.
fn ended(pid: u32, within: Duration) -> Result<bool, String> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if !agent_alive(pid)? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The agent running for the data folder `data`, as its `local-api.json` names it; an error when whether it runs
/// can't be told.
fn running_agent(data: &Path) -> Result<Option<crate::AgentFile>, String> {
    let Ok(text) = std::fs::read_to_string(data.join("local-api.json")) else { return Ok(None) };
    let Some(file) = crate::agent_file(&text) else { return Ok(None) };
    Ok(agent_alive(file.pid)?.then_some(file))
}

impl Machine for ThisMachine {
    fn stop_agent(&self, data: &Path) -> AgentStop {
        let unknown = |e: String| AgentStop::StillRunning(format!("whether it runs can't be told: {e}"));
        let agent = match running_agent(data) {
            Ok(Some(agent)) => agent,
            Ok(None) => return AgentStop::NotRunning,
            Err(e) => return unknown(e),
        };
        // Asked first: it finishes the pass under way, for up to a minute.
        let _ = crate::ask_to_quit(&agent);
        match ended(agent.pid, Duration::from_secs(60)) {
            Ok(true) => return AgentStop::Stopped,
            Ok(false) => {}
            Err(e) => return unknown(e),
        }
        // Then ended: the agent's writes are atomic, so a stop at any point is safe (as after a crash).
        let _ = quiet(Path::new("taskkill")).args(["/PID", &agent.pid.to_string(), "/F"]).status();
        match ended(agent.pid, Duration::from_secs(10)) {
            Ok(true) => AgentStop::Stopped,
            Ok(false) => AgentStop::StillRunning(format!("process {} doesn't end", agent.pid)),
            Err(e) => unknown(e),
        }
    }

    fn close_app(&self) {
        let _ = quiet(Path::new("taskkill")).args(["/IM", APP_EXE, "/F"]).status();
    }

    fn run_agent(&self, agent: &Path, args: &[&str]) -> Result<(), String> {
        let status = quiet(agent).args(args).status().map_err(|e| format!("{}: {e}", agent.display()))?;
        if status.success() { Ok(()) } else { Err(format!("`{} {}` failed ({status})", agent.display(), args.join(" "))) }
    }

    fn start_agent(&self, agent_bg: &Path, data: &Path) -> Result<(), String> {
        quiet(agent_bg)
            .args(["run", "--log"])
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|e| format!("{}: {e}", agent_bg.display()))?;
        // It answers once its local API is up (as the app waits for it).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if matches!(running_agent(data), Ok(Some(_))) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err("it didn't answer within 5 seconds".into())
    }

    fn shortcut(&self, lnk: &Path, target: &Path) -> Result<(), String> {
        // Windows' own shortcut writer (WScript.Shell), through PowerShell. The paths go in as environment
        // variables, so nothing in them is ever read as script.
        let dir = target.parent().ok_or_else(|| format!("{} can't be linked", target.display()))?;
        let script = "$s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:CRAFT_CLOUD_LNK); \
                      $s.TargetPath = $env:CRAFT_CLOUD_TARGET; $s.WorkingDirectory = $env:CRAFT_CLOUD_DIR; $s.Save()";
        let status = quiet(Path::new("powershell.exe"))
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
            .env("CRAFT_CLOUD_LNK", lnk)
            .env("CRAFT_CLOUD_TARGET", target)
            .env("CRAFT_CLOUD_DIR", dir)
            .status()
            .map_err(|e| format!("powershell: {e}"))?;
        if status.success() && lnk.exists() { Ok(()) } else { Err(format!("the shortcut wasn't written ({status})")) }
    }

    fn remove_after_exit(&self, dir: &Path) -> Result<(), String> {
        // `cmd` waits a few seconds (this setup has ended by then), then removes the folder. A folder under
        // %LOCALAPPDATA% can't hold a `"`; inside the quotes `&` and spaces are plain.
        let line = format!("/d /c ping -n 4 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display());
        quiet(Path::new("cmd.exe"))
            .raw_arg(line)
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("the program folder can't be removed: {e}"))
    }
}
