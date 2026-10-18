//! The app's own start-at-login entry, so the tray icon is there after a restart (hidden: `--hidden`). The agent has
//! its own entry (`craft-cloud-agent autostart`); this one follows it ([`crate::model::app_autostart_wanted`]): set
//! when joining and at each start, removed at sign-out, and only where there is a tray.
//!
//! - **Windows**: the value [`RUN_VALUE`] under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`:
//!   `"<dir>\craft-cloud.exe" --hidden`.
//! - **macOS**: `~/Library/LaunchAgents/ch.craftcloud.app.plist` (`RunAtLoad`, no `KeepAlive`: a quit app stays quit).
//! - **Linux**: `~/.config/autostart/craft-cloud.desktop` with `Exec=… --hidden`.
//!
//! The entries are written by pure functions (tested against golden files on every OS) into [`Roots`], so tests write
//! into temporary places; the registry is only touched on Windows.

use std::path::{Path, PathBuf};

/// The Windows `Run` value's name.
pub const RUN_VALUE: &str = "Craft Cloud tray";
/// The key of programs Windows starts at the user's login.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The macOS login item's label (also its file's name).
pub const LABEL: &str = "ch.craftcloud.app";
/// The Linux autostart entry's file.
pub const DESKTOP: &str = "craft-cloud.desktop";

/// Where the entry goes.
#[derive(Debug, Clone)]
pub struct Roots {
    /// The home folder (macOS and Linux).
    pub home: PathBuf,
    /// The key under `HKEY_CURRENT_USER` holding the value (Windows).
    pub run_key: String,
}

impl Roots {
    /// This user's.
    pub fn for_user() -> Result<Self, String> {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).filter(|p| p.is_absolute());
        Ok(Self { home: home.ok_or("can't find your home folder")?, run_key: RUN_KEY.to_owned() })
    }

    pub fn plist_file(&self) -> PathBuf {
        self.home.join("Library").join("LaunchAgents").join(format!("{LABEL}.plist"))
    }

    pub fn desktop_file(&self) -> PathBuf {
        self.home.join(".config").join("autostart").join(DESKTOP)
    }
}

/// `path` as plain text for an entry: UTF-8 without control characters.
fn text(path: &Path) -> Result<&str, String> {
    match path.to_str() {
        Some(s) if !s.chars().any(char::is_control) => Ok(s),
        _ => Err(format!("{} can't go into a start-at-login entry (its name isn't plain text)", path.display())),
    }
}

/// The Windows value: the app in double quotes (Windows paths can't hold one), then `--hidden`.
pub fn run_value(exe: &Path) -> Result<String, String> {
    let exe = text(exe)?;
    if exe.contains('"') {
        return Err(format!("{exe} can't go into a start-at-login entry"));
    }
    Ok(format!("\"{exe}\" --hidden"))
}

/// The macOS login item.
pub fn plist(exe: &Path) -> Result<String, String> {
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by the Craft Cloud app; signing out removes it. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key>\n\
         \x20   <string>{LABEL}</string>\n\
         \x20   <key>ProgramArguments</key>\n\
         \x20   <array>\n\
         \x20       <string>{}</string>\n\
         \x20       <string>--hidden</string>\n\
         \x20   </array>\n\
         \x20   <key>RunAtLoad</key>\n\
         \x20   <true/>\n\
         \x20   <key>ProcessType</key>\n\
         \x20   <string>Interactive</string>\n\
         </dict>\n\
         </plist>\n",
        xml_escaped(text(exe)?)
    ))
}

/// The Linux autostart entry.
pub fn desktop_entry(exe: &Path) -> Result<String, String> {
    Ok(format!(
        "# Written by the Craft Cloud app; signing out removes it.\n\
         [Desktop Entry]\n\
         Type=Application\n\
         Name=Craft Cloud\n\
         Comment=Craft Cloud in the tray: your cloud's sync, on this computer\n\
         Exec={} --hidden\n\
         Icon=craft-cloud\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        desktop_quoted(text(exe)?)
    ))
}

/// Text for XML character data.
fn xml_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// One word of `Exec`, as the Desktop Entry spec reads it: in double quotes, with `"`, `` ` ``, `$` and `\` escaped
/// and `%` doubled; then the value's own escaping doubles every backslash.
fn desktop_quoted(word: &str) -> String {
    let mut quoted = String::with_capacity(word.len() + 2);
    quoted.push('"');
    for c in word.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

/// Writes `content` to `path` (its folder made), through a temporary file renamed into place.
fn write(path: &Path, content: &str) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(at)?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, content).map_err(at)?;
    std::fs::rename(&temp, path).map_err(at)
}

fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
        _ => Ok(()),
    }
}

/// Writes (`on`) or removes this OS's entry for the app at `exe`, at `roots`.
pub fn set(roots: &Roots, exe: &Path, on: bool) -> Result<(), String> {
    if cfg!(windows) {
        windows::set(roots, exe, on)
    } else if cfg!(target_vendor = "apple") {
        if on { write(&roots.plist_file(), &plist(exe)?) } else { remove(&roots.plist_file()) }
    } else if on {
        write(&roots.desktop_file(), &desktop_entry(exe)?)
    } else {
        remove(&roots.desktop_file())
    }
}

/// [`set`] for this app, for this user.
pub fn set_for_this_app(on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("this app can't tell where it is: {e}"))?;
    set(&Roots::for_user()?, &exe, on)
}

#[cfg(windows)]
pub mod windows {
    //! The `Run` value in the registry.

    use std::path::Path;

    use super::{RUN_VALUE, Roots};

    /// `ERROR_FILE_NOT_FOUND` as an `HRESULT`: the key or value isn't there.
    const NOT_FOUND: i32 = -2_147_024_894;

    pub fn set(roots: &Roots, exe: &Path, on: bool) -> Result<(), String> {
        if on {
            let line = super::run_value(exe)?;
            let key = windows_registry::CURRENT_USER.create(&roots.run_key).map_err(|e| e.message())?;
            return key.set_string(RUN_VALUE, line).map_err(|e| e.message());
        }
        let key = match windows_registry::CURRENT_USER.options().read().write().open(&roots.run_key) {
            Ok(key) => key,
            Err(e) if e.code().0 == NOT_FOUND => return Ok(()),
            Err(e) => return Err(e.message()),
        };
        match key.remove_value(RUN_VALUE) {
            Err(e) if e.code().0 != NOT_FOUND => Err(e.message()),
            _ => Ok(()),
        }
    }

    /// The value as Windows holds it (`None`: not there).
    pub fn read(roots: &Roots) -> Option<String> {
        windows_registry::CURRENT_USER.open(&roots.run_key).ok()?.get_string(RUN_VALUE).ok()
    }
}

#[cfg(not(windows))]
mod windows {
    use std::path::Path;

    use super::Roots;

    pub fn set(_roots: &Roots, _exe: &Path, _on: bool) -> Result<(), String> {
        Err("the registry is Windows' only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A golden file, with line endings as written (a checkout may have turned them into CRLF).
    fn golden(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("autostart").join(name);
        std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
    }

    /// Each OS's entry, for a path with a space and an `ö`.
    #[test]
    fn each_os_gets_its_entry_with_the_path_quoted() {
        let windows = Path::new(r"C:\Users\Zoë\AppData\Local\Programs\Craft Cloud\craft-cloud.exe");
        assert_eq!(run_value(windows).unwrap(), golden("windows-run-value.txt").trim_end());
        let mac = Path::new("/Users/zoë/Applications/Craft Cloud.app/Contents/MacOS/craft-cloud");
        assert_eq!(plist(mac).unwrap(), golden("ch.craftcloud.app.plist"));
        let linux = Path::new("/home/zoë/My Apps/craft-cloud");
        assert_eq!(desktop_entry(linux).unwrap(), golden("craft-cloud.desktop"));
        assert!(desktop_entry(Path::new("/opt/x\nExec=/bin/evil")).is_err(), "no line breaks");
        assert_eq!(desktop_quoted(r#"/a b/c"d\e%f$g`h"#), r#""/a b/c\\"d\\\\e%%f\\$g\\`h""#);
        assert_eq!(xml_escaped("R&D <x>"), "R&amp;D &lt;x&gt;");
    }

    /// Written once, replaced, removed; removing one that isn't there is fine.
    #[cfg(not(windows))]
    #[test]
    fn the_entry_is_written_and_removed_in_its_place() {
        let home = tempfile::tempdir().unwrap();
        let roots = Roots { home: home.path().to_owned(), run_key: String::new() };
        let exe = Path::new("/opt/Craft Cloud/craft-cloud");
        let file = if cfg!(target_vendor = "apple") { roots.plist_file() } else { roots.desktop_file() };
        set(&roots, exe, true).unwrap();
        set(&roots, exe, true).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("--hidden"));
        assert_eq!(std::fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
        set(&roots, exe, false).unwrap();
        assert!(!file.exists());
        set(&roots, exe, false).unwrap();
    }

    /// The registry value, in a throwaway key that is deleted after.
    #[cfg(windows)]
    #[test]
    fn the_run_value_is_written_and_removed() {
        struct Key(String);
        impl Drop for Key {
            fn drop(&mut self) {
                let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
            }
        }
        let key = Key(format!(r"Software\Craft Cloud Test\app-{}", std::process::id()));
        let roots = Roots { home: PathBuf::from(r"C:\nowhere"), run_key: key.0.clone() };
        let exe = Path::new(r"C:\Users\Zoë\AppData\Local\Programs\Craft Cloud\craft-cloud.exe");
        set(&roots, exe, false).unwrap();
        set(&roots, exe, true).unwrap();
        set(&roots, exe, true).unwrap();
        assert_eq!(windows::read(&roots).as_deref(), Some(golden("windows-run-value.txt").trim_end()));
        set(&roots, exe, false).unwrap();
        assert_eq!(windows::read(&roots), None);
    }
}
