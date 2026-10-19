//! One of each per user: the app's window, and (on Linux) its tray process. The first of a kind holds a lock on its
//! `<name>.lock` and listens on a loopback port it writes, with a random token, to `<name>.port`; both are in the
//! folder above the agent's data folder (the per-user data folder: `%LOCALAPPDATA%`, `~/Library/Application Support`,
//! `~/.local/share`). A later start finds the lock taken, connects to that port and sends a [`Command`]: `show` (the
//! first brings its window forward, also from the tray, and the second exits, so there is never a second tray icon)
//! or `quit` (the tray's Quit ends the window too, and the window's Quit the tray).
//!
//! The lock is the OS's file lock (`File::try_lock`), which goes with the process, so a crashed app leaves nothing
//! that holds up the next. The channel is loopback TCP, as the agent's local API: plain `std` on every OS. The
//! token keeps other users' programs (which can reach the port, not the file) from poking it.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a second start waits for the first to answer (it may be starting too).
const PATIENCE: Duration = Duration::from_secs(3);

/// Which kind of process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Name {
    /// The window (on Windows and macOS also the tray).
    Window,
    /// Linux: the tray process (`craft-cloud --tray`).
    Tray,
}

impl Name {
    fn stem(self) -> &'static str {
        match self {
            Name::Window => "craft-cloud-app",
            Name::Tray => "craft-cloud-tray",
        }
    }

    /// The lock's file.
    pub fn lock_file(self) -> String {
        format!("{}.lock", self.stem())
    }

    /// Where the first listens: `<port> <token>`.
    pub fn port_file(self) -> String {
        format!("{}.port", self.stem())
    }
}

/// What a later start asks the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Show the window.
    Show,
    /// End (the other process quit Craft Cloud).
    Quit,
}

impl Command {
    fn word(self) -> &'static str {
        match self {
            Command::Show => "show",
            Command::Quit => "quit",
        }
    }
}

/// What a start found.
pub enum Instance {
    /// This is the first: listen for later starts.
    First(First),
    /// Another one runs: send it a command ([`Running::send`]), then exit.
    Running(Running),
}

/// The first's lock and listener.
pub struct First {
    /// Held (locked) while this lives.
    lock: File,
    listener: TcpListener,
    token: String,
}

/// Another one's address.
pub struct Running {
    dir: PathBuf,
    name: Name,
}

/// The folder for the locks and the channels: the one above the agent's data folder.
pub fn folder() -> Option<PathBuf> {
    craft_cloud_proto::local::data_dir(|name| std::env::var_os(name)).and_then(|data| data.parent().map(Path::to_owned))
}

/// Takes the lock of `name` in `dir`, or finds the process that holds it.
pub fn claim(dir: &Path, name: Name) -> std::io::Result<Instance> {
    std::fs::create_dir_all(dir)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(name.lock_file()))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Instance::Running(Running { dir: dir.to_owned(), name })),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    let token = token()?;
    let port = listener.local_addr()?.port();
    // Written whole, then renamed into place, so a later start never reads half of it.
    let temp = dir.join(format!("{}.tmp", name.port_file()));
    std::fs::write(&temp, format!("{port} {token}\n"))?;
    std::fs::rename(&temp, dir.join(name.port_file()))?;
    Ok(Instance::First(First { lock, listener, token }))
}

/// Whether a process of `name` runs (holds its lock), without taking it for long.
pub fn is_running(dir: &Path, name: Name) -> bool {
    let Ok(lock) = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(name.lock_file())) else { return false };
    matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Sends `command` to the running process of `name`, if one answers (within a few seconds).
pub fn send(dir: &Path, name: Name, command: Command) -> std::io::Result<()> {
    Running { dir: dir.to_owned(), name }.send(command)
}

fn token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl First {
    /// Listens on a thread of its own for the rest of the process: `on` is called with every command a later start
    /// (or the other process) sends.
    pub fn listen(self, on: impl Fn(Command) + Send + 'static) {
        let First { lock, listener, token } = self;
        let _ = std::thread::Builder::new().name("craft-cloud-single".into()).spawn(move || {
            // Held as long as the process lives.
            let _held = lock;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if let Some(command) = asked(stream, &token) {
                    on(command);
                }
            }
        });
    }
}

/// The most a line of the channel may be.
const MAX_LINE: u64 = 128;

/// The command `stream` sent with the token (answered `ok`). Only a short line is read, and a peer that sends nothing
/// is given up on after a second.
fn asked(stream: TcpStream, token: &str) -> Option<Command> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_LINE)).read_line(&mut line).ok()?;
    let command = [Command::Show, Command::Quit].into_iter().find(|c| line.trim_end() == format!("{} {token}", c.word()))?;
    let _ = (&stream).write_all(b"ok\n");
    Some(command)
}

impl Running {
    /// Asks the first to show its window; an error if it doesn't answer within a few seconds.
    pub fn show(&self) -> std::io::Result<()> {
        self.send(Command::Show)
    }

    /// Sends `command` to the first; an error if it doesn't answer within a few seconds.
    pub fn send(&self, command: Command) -> std::io::Result<()> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            match self.try_send(command) {
                Ok(()) => return Ok(()),
                // The first may be starting: its port file comes a moment after its lock.
                Err(_) if Instant::now() < deadline && is_running(&self.dir, self.name) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(e),
            }
        }
    }

    fn try_send(&self, command: Command) -> std::io::Result<()> {
        let text = std::fs::read_to_string(self.dir.join(self.name.port_file()))?;
        let (port, token) = text.trim().split_once(' ').ok_or_else(|| std::io::Error::other("the port file doesn't read"))?;
        let port: u16 = port.parse().map_err(|_| std::io::Error::other("the port file doesn't read"))?;
        let mut stream = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_secs(1))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(format!("{} {token}\n", command.word()).as_bytes())?;
        let mut answer = String::new();
        BufReader::new((&stream).take(MAX_LINE)).read_line(&mut answer)?;
        if answer.trim_end() == "ok" { Ok(()) } else { Err(std::io::Error::other("the running app didn't answer")) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// Review focus: two starts in one test, in a temporary folder: the first holds it, the second finds it, asks it to
    /// show its window and is told yes. Once the first is gone, a start is the first again.
    #[test]
    fn a_second_start_shows_the_first_and_exits() {
        let dir = tempfile::tempdir().unwrap();
        let Instance::First(first) = claim(dir.path(), Name::Window).unwrap() else { panic!("not first") };
        let (shown_tx, shown) = channel();
        first.listen(move |command| {
            let _ = shown_tx.send(command);
        });
        let Instance::Running(running) = claim(dir.path(), Name::Window).unwrap() else { panic!("a second first") };
        running.show().unwrap();
        assert_eq!(shown.recv_timeout(Duration::from_secs(10)).unwrap(), Command::Show);
        running.send(Command::Quit).unwrap();
        assert_eq!(shown.recv_timeout(Duration::from_secs(10)).unwrap(), Command::Quit);
    }

    /// The window and the tray process are one each, apart: each can tell whether the other runs, and send it a
    /// command without taking its place.
    #[test]
    fn the_window_and_the_tray_are_one_each() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_running(dir.path(), Name::Tray));
        assert!(send(dir.path(), Name::Tray, Command::Quit).is_err(), "nobody to tell");
        let Instance::First(tray) = claim(dir.path(), Name::Tray).unwrap() else { panic!("not first") };
        let Instance::First(_window) = claim(dir.path(), Name::Window).unwrap() else { panic!("the window waits for the tray") };
        assert!(is_running(dir.path(), Name::Tray) && is_running(dir.path(), Name::Window));
        let (told, heard) = channel();
        tray.listen(move |command| {
            let _ = told.send(command);
        });
        send(dir.path(), Name::Tray, Command::Quit).unwrap();
        assert_eq!(heard.recv_timeout(Duration::from_secs(10)).unwrap(), Command::Quit);
        assert!(is_running(dir.path(), Name::Tray), "asking doesn't take the lock");
    }

    #[test]
    fn a_wrong_token_shows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let Instance::First(first) = claim(dir.path(), Name::Window).unwrap() else { panic!("not first") };
        let (shown_tx, shown) = channel();
        first.listen(move |command| {
            let _ = shown_tx.send(command);
        });
        let text = std::fs::read_to_string(dir.path().join(Name::Window.port_file())).unwrap();
        let port: u16 = text.split(' ').next().unwrap().parse().unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"show not-the-token\n").unwrap();
        assert!(shown.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn a_lock_given_up_is_free_again() {
        let dir = tempfile::tempdir().unwrap();
        let first = claim(dir.path(), Name::Window).unwrap();
        assert!(matches!(first, Instance::First(_)));
        drop(first);
        // A process this test binary is spawning meanwhile (another test) holds the lock's file for an instant
        // between its fork and its exec.
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_running(dir.path(), Name::Window) {
            assert!(Instant::now() < deadline, "the lock is still taken");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(claim(dir.path(), Name::Window).unwrap(), Instance::First(_)));
    }
}
