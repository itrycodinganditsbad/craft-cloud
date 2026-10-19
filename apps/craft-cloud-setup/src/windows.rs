//! The real registry (`HKEY_CURRENT_USER`) and programs, on Windows.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
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

/// Windows' own program `name` (`tasklist.exe`, `cmd.exe`, …), by its full path in `%SystemRoot%\System32`: a bare
/// name is looked for in the setup's own folder first, where anything may have been put next to a download.
pub fn system_program(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join(name)
}

/// PowerShell running `script`, quietly. What the script works on goes in as environment variables, so nothing in a
/// path is ever read as script.
fn powershell(script: &str) -> Command {
    let mut command = quiet(&system_program(r"WindowsPowerShell\v1.0\powershell.exe"));
    command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]);
    command
}

/// Starts `program` with `args` (one command line, taken as it is) in `dir`, sharing nothing with this process:
/// Windows' shell starts it (PowerShell's `Start-Process`), so it inherits none of this process's handles. Started
/// straight from here, the agent would keep this setup's output (a pipe, when run from WSL or a script) open, and
/// whatever waits for the setup to end would wait for the agent.
pub fn start_detached(program: &Path, args: &str, dir: &Path) -> Command {
    let mut command = powershell(
        "Start-Process -FilePath $env:CRAFT_CLOUD_PROGRAM -ArgumentList $env:CRAFT_CLOUD_ARGS \
         -WorkingDirectory $env:CRAFT_CLOUD_DIR -WindowStyle Hidden",
    );
    command.env("CRAFT_CLOUD_PROGRAM", program).env("CRAFT_CLOUD_ARGS", args).env("CRAFT_CLOUD_DIR", dir);
    command
}

/// The removal's `cmd` line: `rmdir /s /q` of the folder named by `CRAFT_CLOUD_DIR` (cmd expands the variable once;
/// its value is never read as a command, and inside the quotes `&` and spaces are plain). `rmdir /s` removes a
/// junction or link inside the folder without following it, so nothing outside the folder is ever removed.
const RMDIR: &str = r#"rmdir /s /q "%CRAFT_CLOUD_DIR%""#;

/// Removes the folder `dir` once the process `pid` has ended (it holds its own exe, in the folder), trying again for
/// a while if a file is still held: PowerShell waits for the process, then `cmd` removes the folder ([`RMDIR`],
/// started with `Start-Process`, which works in PowerShell's constrained language mode too).
pub fn remove_after(pid: u32, dir: &Path) -> Command {
    let mut command = powershell(
        "Wait-Process -Id $env:CRAFT_CLOUD_PID -Timeout 3600 -ErrorAction SilentlyContinue; \
         for ($i = 0; $i -lt 60 -and (Test-Path -LiteralPath $env:CRAFT_CLOUD_DIR); $i++) { \
         Start-Process -FilePath $env:CRAFT_CLOUD_CMD -ArgumentList $env:CRAFT_CLOUD_RMDIR -Wait -WindowStyle Hidden; \
         if (Test-Path -LiteralPath $env:CRAFT_CLOUD_DIR) { Start-Sleep -Milliseconds 500 } }",
    );
    command
        .env("CRAFT_CLOUD_PID", pid.to_string())
        .env("CRAFT_CLOUD_DIR", dir)
        .env("CRAFT_CLOUD_CMD", system_program("cmd.exe"))
        .env("CRAFT_CLOUD_RMDIR", format!("/d /c {RMDIR}"));
    detached_from(command, dir)
}

/// [`remove_after`] without PowerShell: `cmd` alone asks `tasklist` every second whether the process `pid` (whose
/// exe is `image`) still runs, for up to an hour, then removes the folder ([`RMDIR`], tried again every second for
/// half a minute while a file is still held).
pub fn remove_after_with_cmd(pid: u32, image: &str, dir: &Path) -> Command {
    remove_after_with_cmd_using(&system_program("tasklist.exe"), 3600, pid, image, dir)
}

/// [`remove_after_with_cmd`] with `tasklist` at `tasklist`, asked `rounds` times at most.
///
/// `tasklist`'s answer goes to a file first (`CRAFT_CLOUD_TASKS`, in the temporary folder), so a `tasklist` that
/// fails (and writes nothing) is told apart from one that lists nothing: then cmd waits and asks again, rather than
/// take the setup for ended. `find` and `ping` are named by their full paths: cmd looks in its working folder first.
/// Each `if` is in parentheses of its own: cmd would take a following `& …` as part of it.
fn remove_after_with_cmd_using(tasklist: &Path, rounds: u32, pid: u32, image: &str, dir: &Path) -> Command {
    let (find, ping) = (system_program("find.exe"), system_program("PING.EXE"));
    let wait = format!("\"{}\" -n 2 127.0.0.1 >nul", ping.display());
    let remove = format!("for /l %j in (1,1,30) do @(if exist \"%CRAFT_CLOUD_DIR%\" ({RMDIR} & (if exist \"%CRAFT_CLOUD_DIR%\" ({wait}))))");
    let line = format!(
        "/d /c for /l %i in (1,1,{rounds}) do @((\"{tasklist}\" /FI \"PID eq {pid}\" /NH >\"%CRAFT_CLOUD_TASKS%\") \
         && (\"{find}\" /i \"{image}\" \"%CRAFT_CLOUD_TASKS%\" >nul && {wait} || ({remove} & del \"%CRAFT_CLOUD_TASKS%\" & exit)) || {wait})",
        tasklist = tasklist.display(),
        find = find.display()
    );
    let tasks = std::env::temp_dir().join(format!("craft-cloud-uninstall-{pid}.txt"));
    let mut command = quiet(&system_program("cmd.exe"));
    command.raw_arg(line).env("CRAFT_CLOUD_DIR", dir).env("CRAFT_CLOUD_TASKS", tasks);
    detached_from(command, dir)
}

/// For tests: [`remove_after_with_cmd_using`] with a `tasklist` that can't run, asked `rounds` times.
#[cfg(test)]
pub fn remove_after_with_cmd_without_tasklist(rounds: u32, pid: u32, image: &str, dir: &Path) -> Command {
    remove_after_with_cmd_using(Path::new(r"C:\no-such-folder\tasklist.exe"), rounds, pid, image, dir)
}

/// In a process group of its own, and working in the folder above `dir` (a remover working inside it would hold it).
fn detached_from(mut command: Command, dir: &Path) -> Command {
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).current_dir(dir.parent().unwrap_or(dir));
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
    let ran = quiet(&system_program("tasklist.exe"))
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stdout(Stdio::piped())
        .output()
        .map(|out| (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
        .map_err(|e| e.to_string());
    crate::tasklist_says(ran, pid)
}

/// [`agent_alive`], asked again a few times while `tasklist` can't tell (a hiccup).
fn agent_alive_patiently(pid: u32) -> Result<bool, String> {
    let mut tries = 0;
    loop {
        match agent_alive(pid) {
            Err(_) if tries < 3 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(300));
            }
            answer => return answer,
        }
    }
}

/// Waits up to `within` for the agent `pid` to end; an error when `tasklist` can't tell.
fn ended(pid: u32, within: Duration) -> Result<bool, String> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if !agent_alive_patiently(pid)? {
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
    Ok(agent_alive_patiently(file.pid)?.then_some(file))
}

impl Machine for ThisMachine {
    fn stop_agent(&self, data: &Path) -> AgentStop {
        // Before anything is asked, not knowing whether it runs changes nothing; after, it may be stopping.
        let agent = match running_agent(data) {
            Ok(Some(agent)) => agent,
            Ok(None) => return AgentStop::NotRunning,
            Err(e) => return AgentStop::StillRunning(format!("whether it runs can't be told: {e}")),
        };
        let unknown = |e: String| AgentStop::Unknown(format!("tasklist can't tell: {e}"));
        // Asked first: it finishes the pass under way, for up to a minute.
        let _ = crate::ask_to_quit(&agent);
        match ended(agent.pid, Duration::from_secs(60)) {
            Ok(true) => return AgentStop::Stopped,
            Ok(false) => {}
            Err(e) => return unknown(e),
        }
        // Then ended: the agent's writes are atomic, so a stop at any point is safe (as after a crash).
        let _ = quiet(&system_program("taskkill.exe")).args(["/PID", &agent.pid.to_string(), "/F"]).status();
        match ended(agent.pid, Duration::from_secs(10)) {
            Ok(true) => AgentStop::Stopped,
            Ok(false) => AgentStop::StillRunning(format!("process {} doesn't end", agent.pid)),
            Err(e) => unknown(e),
        }
    }

    fn close_app(&self) {
        let _ = quiet(&system_program("taskkill.exe")).args(["/IM", APP_EXE, "/F"]).status();
    }

    fn run_agent(&self, agent: &Path, args: &[&str]) -> Result<(), String> {
        let status = quiet(agent).args(args).status().map_err(|e| format!("{}: {e}", agent.display()))?;
        if status.success() { Ok(()) } else { Err(format!("`{} {}` failed ({status})", agent.display(), args.join(" "))) }
    }

    fn start_agent(&self, agent_bg: &Path, data: &Path) -> Result<(), String> {
        let dir = agent_bg.parent().ok_or_else(|| format!("{} can't be started", agent_bg.display()))?;
        // Through the shell, so it holds none of this setup's handles; where PowerShell can't run (blocked by policy),
        // straight from here, which works but shares the setup's handles.
        match start_detached(agent_bg, "run --log", dir).status() {
            Ok(status) if status.success() => eprintln!("craft-cloud-setup: the agent was started through PowerShell"),
            failed => {
                let why = failed.map_or_else(|e| e.to_string(), |status| status.to_string());
                eprintln!("craft-cloud-setup: PowerShell couldn't start the agent ({why}); it is started directly");
                quiet(agent_bg)
                    .args(["run", "--log"])
                    .current_dir(dir)
                    .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
                    .spawn()
                    .map_err(|e| format!("{}: {e}", agent_bg.display()))?;
            }
        }
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
        let status = powershell(
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:CRAFT_CLOUD_LNK); \
             $s.TargetPath = $env:CRAFT_CLOUD_TARGET; $s.WorkingDirectory = $env:CRAFT_CLOUD_DIR; $s.Save()",
        )
        .env("CRAFT_CLOUD_LNK", lnk)
        .env("CRAFT_CLOUD_TARGET", target)
        .env("CRAFT_CLOUD_DIR", dir)
        .status()
        .map_err(|e| format!("powershell: {e}"))?;
        if status.success() && lnk.exists() { Ok(()) } else { Err(format!("the shortcut wasn't written ({status})")) }
    }

    fn remove_after_exit(&self, dir: &Path) -> Result<(), String> {
        // Waits for this setup to end, however long its last message stays open: through PowerShell, else `cmd`.
        let pid = std::process::id();
        let Err(e) = remove_after(pid, dir).spawn() else { return Ok(()) };
        eprintln!("craft-cloud-setup: PowerShell can't run ({e}); cmd removes the program folder");
        let image = std::env::current_exe().ok().and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()));
        let image = image.unwrap_or_else(|| crate::SETUP_EXE.to_owned());
        remove_after_with_cmd(pid, &image, dir).spawn().map(|_| ()).map_err(|e| format!("the program folder can't be removed: {e}"))
    }
}
