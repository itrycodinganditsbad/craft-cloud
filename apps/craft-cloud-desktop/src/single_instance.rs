//! One app per user. The first `craft-cloud` holds a lock on [`LOCK_FILE`] and listens on a loopback port it writes,
//! with a random token, to [`PORT_FILE`]; both are in the folder above the agent's data folder (the per-user data
//! folder: `%LOCALAPPDATA%`, `~/Library/Application Support`, `~/.local/share`). A second start finds the lock
//! taken, connects to that port and sends `show <token>`: the first brings its window forward (also from the tray)
//! and the second exits, so there is never a second tray icon.
//!
//! The lock is the OS's file lock (`File::try_lock`), which goes with the process, so a crashed app leaves nothing
//! that holds up the next. The channel is loopback TCP, as the agent's local API: plain `std` on every OS. The
//! token keeps other users' programs (which can reach the port, not the file) from poking it.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The lock's file.
pub const LOCK_FILE: &str = "craft-cloud-app.lock";
/// Where the first app listens: `<port> <token>`.
pub const PORT_FILE: &str = "craft-cloud-app.port";
/// How long a second start waits for the first to answer (it may be starting too).
const PATIENCE: Duration = Duration::from_secs(3);

/// What a start found.
pub enum Instance {
    /// This is the first: show the window, and listen for later starts.
    First(First),
    /// Another app runs: ask it to show its window ([`Running::show`]), then exit.
    Running(Running),
}

/// The first app's lock and listener.
pub struct First {
    /// Held (locked) while this lives.
    lock: File,
    listener: TcpListener,
    token: String,
}

/// Another app's address.
pub struct Running {
    dir: PathBuf,
}

/// The folder for the lock and the channel: the one above the agent's data folder.
pub fn folder() -> Option<PathBuf> {
    craft_cloud_proto::local::data_dir(|name| std::env::var_os(name)).and_then(|data| data.parent().map(Path::to_owned))
}

/// Takes the lock in `dir`, or finds the app that holds it.
pub fn claim(dir: &Path) -> std::io::Result<Instance> {
    std::fs::create_dir_all(dir)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Instance::Running(Running { dir: dir.to_owned() })),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    let token = token()?;
    let port = listener.local_addr()?.port();
    // Written whole, then renamed into place, so a second start never reads half of it.
    let temp = dir.join(format!("{PORT_FILE}.tmp"));
    std::fs::write(&temp, format!("{port} {token}\n"))?;
    std::fs::rename(&temp, dir.join(PORT_FILE))?;
    Ok(Instance::First(First { lock, listener, token }))
}

fn token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl First {
    /// Listens on a thread of its own for the rest of the process: `on_show` is called for every later start.
    pub fn listen(self, on_show: impl Fn() + Send + 'static) {
        let First { lock, listener, token } = self;
        let _ = std::thread::Builder::new().name("craft-cloud-single".into()).spawn(move || {
            // Held as long as the process lives.
            let _held = lock;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if asked_to_show(stream, &token) {
                    on_show();
                }
            }
        });
    }
}

/// The most a line of the channel may be.
const MAX_LINE: u64 = 128;

/// Whether `stream` sent `show <token>` (answered `ok`). Only a short line is read, and a peer that sends nothing is
/// given up on after a second.
fn asked_to_show(stream: TcpStream, token: &str) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut line = String::new();
    if BufReader::new((&stream).take(MAX_LINE)).read_line(&mut line).is_err() || line.trim_end() != format!("show {token}") {
        return false;
    }
    let _ = (&stream).write_all(b"ok\n");
    true
}

impl Running {
    /// Asks the first app to show its window; an error if it doesn't answer within a few seconds.
    pub fn show(&self) -> std::io::Result<()> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            match self.try_show() {
                Ok(()) => return Ok(()),
                // The first may be starting: its port file comes a moment after its lock.
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(e),
            }
        }
    }

    fn try_show(&self) -> std::io::Result<()> {
        let text = std::fs::read_to_string(self.dir.join(PORT_FILE))?;
        let (port, token) = text.trim().split_once(' ').ok_or_else(|| std::io::Error::other("the port file doesn't read"))?;
        let port: u16 = port.parse().map_err(|_| std::io::Error::other("the port file doesn't read"))?;
        let mut stream = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_secs(1))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(format!("show {token}\n").as_bytes())?;
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
        let Instance::First(first) = claim(dir.path()).unwrap() else { panic!("not first") };
        let (shown_tx, shown) = channel();
        first.listen(move || {
            let _ = shown_tx.send(());
        });
        let Instance::Running(running) = claim(dir.path()).unwrap() else { panic!("a second first") };
        running.show().unwrap();
        shown.recv_timeout(Duration::from_secs(10)).unwrap();
        running.show().unwrap();
        shown.recv_timeout(Duration::from_secs(10)).unwrap();
    }

    #[test]
    fn a_wrong_token_shows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let Instance::First(first) = claim(dir.path()).unwrap() else { panic!("not first") };
        let (shown_tx, shown) = channel();
        first.listen(move || {
            let _ = shown_tx.send(());
        });
        let text = std::fs::read_to_string(dir.path().join(PORT_FILE)).unwrap();
        let port: u16 = text.split(' ').next().unwrap().parse().unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"show not-the-token\n").unwrap();
        assert!(shown.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn a_lock_given_up_is_free_again() {
        let dir = tempfile::tempdir().unwrap();
        let first = claim(dir.path()).unwrap();
        assert!(matches!(first, Instance::First(_)));
        drop(first);
        assert!(matches!(claim(dir.path()).unwrap(), Instance::First(_)));
    }
}
