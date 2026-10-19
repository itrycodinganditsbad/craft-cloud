//! The real registry (`HKEY_CURRENT_USER`) and programs, on Windows.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{APP_EXE, Machine, Registry};

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

impl Machine for ThisMachine {
    fn quit_agent(&self, agent: &Path) -> bool {
        // `quit` answers while the agent runs (and while it finishes its pass); once nothing answers it is gone.
        let mut was_running = false;
        for _ in 0..60 {
            match quiet(agent).arg("quit").status() {
                Ok(status) if status.success() => was_running = true,
                _ => break,
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        was_running
    }

    fn close_app(&self) {
        let _ = quiet(Path::new("taskkill")).args(["/IM", APP_EXE, "/F"]).status();
    }

    fn run_agent(&self, agent: &Path, args: &[&str]) -> Result<(), String> {
        let status = quiet(agent).args(args).status().map_err(|e| format!("{}: {e}", agent.display()))?;
        if status.success() { Ok(()) } else { Err(format!("`{} {}` failed ({status})", agent.display(), args.join(" "))) }
    }

    fn start_agent(&self, agent_bg: &Path) -> Result<(), String> {
        quiet(agent_bg)
            .args(["run", "--log"])
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("{}: {e}", agent_bg.display()))
    }

    fn shortcut(&self, lnk: &Path, target: &Path) -> Result<(), String> {
        // mslnk reads the target's folder as text: one that isn't, isn't linked.
        if target.to_str().is_none() || target.parent().is_none() {
            return Err(format!("{} can't be linked", target.display()));
        }
        let link = mslnk::ShellLink::new(target).map_err(|e| format!("{e:?}"))?;
        link.create_lnk(lnk).map_err(|e| format!("{e:?}"))
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
