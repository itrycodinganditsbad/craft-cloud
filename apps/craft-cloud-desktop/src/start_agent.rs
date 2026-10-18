//! Starting the agent when none answers: `craft-cloud-agent run --log` from this app's own folder
//! (`craft-cloud-agent-bg.exe`, the build without a console, on Windows), detached and hidden, then asking for it
//! again for up to [`WAIT`]. The app never syncs by itself; the agent writes what it says to `run.log` in its data
//! folder.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_app::{Error, Status};

use crate::agent::{Agent, Starter};

/// The agent's program next to this app.
pub const AGENT_EXE: &str = if cfg!(windows) { "craft-cloud-agent-bg.exe" } else { "craft-cloud-agent" };
/// How long the agent has to answer once started.
pub const WAIT: Duration = Duration::from_secs(5);
/// How often it is asked meanwhile.
const EVERY: Duration = Duration::from_millis(100);

/// Windows: no console window for the child (`CREATE_NO_WINDOW`), and a process group of its own, so a Ctrl-C meant
/// for this app never reaches it (`CREATE_NEW_PROCESS_GROUP`).
#[cfg(windows)]
const CREATION_FLAGS: u32 = 0x0800_0000 | 0x0000_0200;

/// The agent in the folder of the app at `app`.
pub fn agent_next_to(app: &Path) -> PathBuf {
    app.with_file_name(AGENT_EXE)
}

/// Starts `exe run --log`, detached: no console, no input or output, in a process group of its own (so it keeps
/// running when this app or its terminal goes). A thread waits for it, so it leaves no zombie behind on Unix.
pub fn spawn(exe: &Path) -> std::io::Result<()> {
    let mut command = Command::new(exe);
    command.args(["run", "--log"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATION_FLAGS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let _ = std::thread::Builder::new().name("craft-cloud-agent-wait".into()).spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// The agent's status: at once if one answers, else after starting one with `spawn` and asking every `every` for up
/// to `wait`. Why not, for the Details of "didn't start".
pub fn start_and_wait(agent: &dyn Agent, spawn: impl FnOnce() -> std::io::Result<()>, wait: Duration, every: Duration) -> Result<Status, String> {
    match agent.connect() {
        Ok(status) => return Ok(status),
        Err(e @ Error::Version(_)) => return Err(e.to_string()),
        Err(_) => {}
    }
    spawn().map_err(|e| format!("it couldn't be started: {e}"))?;
    let deadline = Instant::now() + wait;
    loop {
        std::thread::sleep(every);
        match agent.connect() {
            Ok(status) => return Ok(status),
            Err(e) if Instant::now() >= deadline => return Err(format!("it didn't answer within {} seconds ({e})", wait.as_secs())),
            Err(_) => {}
        }
    }
}

/// The real starter: the agent next to this program.
pub fn starter() -> Starter {
    Arc::new(|agent: &dyn Agent| {
        let app = std::env::current_exe().map_err(|e| format!("this app can't tell where it is: {e}"))?;
        let exe = agent_next_to(&app);
        start_and_wait(agent, || spawn(&exe).map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", exe.display()))), WAIT, EVERY)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::fake::FakeAgent;
    use std::sync::atomic::Ordering;

    #[test]
    fn the_agent_is_the_one_next_to_the_app() {
        let app = Path::new("/opt/Craft Cloud/craft-cloud");
        let agent = agent_next_to(app);
        assert_eq!(agent.parent(), app.parent());
        assert_eq!(agent.file_name().and_then(|n| n.to_str()), Some(if cfg!(windows) { "craft-cloud-agent-bg.exe" } else { "craft-cloud-agent" }));
    }

    #[test]
    fn a_running_agent_isnt_started_again() {
        let agent = FakeAgent::new(crate::model::tests::status(true));
        let started = start_and_wait(&agent, || panic!("started"), WAIT, EVERY).unwrap();
        assert!(started.signed_in);
    }

    #[test]
    fn a_started_agent_is_waited_for() {
        let agent = FakeAgent::new(crate::model::tests::status(false));
        agent.gone.store(true, Ordering::SeqCst);
        let status = start_and_wait(
            &agent,
            || {
                agent.gone.store(false, Ordering::SeqCst);
                Ok(())
            },
            WAIT,
            Duration::from_millis(1),
        );
        assert!(!status.unwrap().signed_in);
    }

    #[test]
    fn an_agent_that_doesnt_start_or_answer_says_why() {
        let agent = FakeAgent::new(crate::model::tests::status(false));
        agent.gone.store(true, Ordering::SeqCst);
        let why = start_and_wait(&agent, || Err(std::io::Error::other("no such file")), WAIT, EVERY).unwrap_err();
        assert!(why.contains("couldn't be started") && why.contains("no such file"), "{why}");
        let started = Instant::now();
        let why = start_and_wait(&agent, || Ok(()), Duration::from_millis(300), Duration::from_millis(10)).unwrap_err();
        assert!(why.contains("didn't answer"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(5), "bounded by the wait");
    }

    /// An agent that isn't there is an error to show, not a panic.
    #[test]
    fn spawning_a_missing_agent_fails() {
        let missing = Path::new("/no/such/craft-cloud-agent");
        assert!(spawn(missing).is_err());
    }
}
