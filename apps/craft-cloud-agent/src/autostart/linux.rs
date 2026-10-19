//! Linux: a systemd user unit when the session runs systemd, else an XDG autostart entry.
//!
//! - **systemd** is used when `~/.config/systemd/user` can be made and `systemctl --user show-environment` answers
//!   (the user's own service manager runs). [`UNIT`] is written there (`Restart=on-failure` after 30 s, so a crash
//!   restarts it and a quit doesn't, nor exit code 3: something only a person can put right), then `systemctl --user daemon-reload` and `enable`. A unit of that name already there (one made by
//!   hand) is rewritten, never duplicated. The XDG entry is removed then, so the agent isn't started twice.
//! - **Otherwise** [`DESKTOP`] goes into `~/.config/autostart`, which desktop sessions start at login.
//!
//! Removing disables and deletes whichever is there, without stopping a running agent (it may be the one asking).
//! The files are written by [`unit`] and [`desktop_entry`], pure functions, so their format is tested on every OS.

use std::path::PathBuf;

use super::{AutostartError, Launch, Roots, remove_file, write_file};

/// The systemd user unit's name.
pub const UNIT: &str = "craft-cloud-agent.service";
/// The XDG autostart entry's name.
pub const DESKTOP: &str = "craft-cloud-agent.desktop";

fn unit_path(roots: &Roots) -> PathBuf {
    roots.home.join(".config").join("systemd").join("user").join(UNIT)
}

fn desktop_path(roots: &Roots) -> PathBuf {
    roots.home.join(".config").join("autostart").join(DESKTOP)
}

/// Whether the user's systemd answers, with its unit folder made.
fn systemd(roots: &Roots) -> bool {
    let dir = roots.home.join(".config").join("systemd").join("user");
    std::fs::create_dir_all(&dir).is_ok() && systemctl(roots, &["show-environment"]).unwrap_or(false)
}

fn systemctl(roots: &Roots, args: &[&str]) -> std::io::Result<bool> {
    let mut all = vec!["--user"];
    all.extend_from_slice(args);
    roots.runner.run("systemctl", &all)
}

/// Runs `systemctl --user args`; a failure is an error naming the command.
fn systemctl_ok(roots: &Roots, args: &[&str]) -> Result<(), AutostartError> {
    let path = PathBuf::from(format!("systemctl --user {}", args.join(" ")));
    match systemctl(roots, args) {
        Ok(true) => Ok(()),
        Ok(false) => Err(AutostartError::Io { path, source: std::io::Error::other("it failed") }),
        Err(source) => Err(AutostartError::Io { path, source }),
    }
}

/// Registers `launch`: the systemd unit if the session runs systemd, else the XDG autostart entry.
pub fn register(roots: &Roots, launch: &Launch) -> Result<(), AutostartError> {
    if systemd(roots) {
        write_file(&unit_path(roots), &unit(launch)?)?;
        systemctl_ok(roots, &["daemon-reload"])?;
        systemctl_ok(roots, &["enable", UNIT])?;
        remove_file(&desktop_path(roots))
    } else {
        write_file(&desktop_path(roots), &desktop_entry(launch)?)
    }
}

/// Disables and removes the unit and removes the XDG entry, whichever is there.
pub fn unregister(roots: &Roots) -> Result<(), AutostartError> {
    let unit = unit_path(roots);
    if unit.exists() {
        // With systemd gone (another session type), the file alone goes.
        if systemd(roots) {
            systemctl_ok(roots, &["disable", UNIT])?;
        }
        remove_file(&unit)?;
        if systemd(roots) {
            let _ = systemctl(roots, &["daemon-reload"]);
        }
    }
    remove_file(&desktop_path(roots))
}

/// Whether the agent starts at login: its unit is there and enabled, or its XDG entry is there.
pub fn is_registered(roots: &Roots) -> Result<bool, AutostartError> {
    if unit_path(roots).exists() && systemd(roots) && systemctl(roots, &["is-enabled", "--quiet", UNIT]).unwrap_or(false) {
        return Ok(true);
    }
    Ok(desktop_path(roots).exists())
}

/// The systemd user unit for `launch`.
pub fn unit(launch: &Launch) -> Result<String, AutostartError> {
    let mut command = vec![systemd_quoted(&launch.program()?)];
    command.extend(launch.args()?.iter().map(|a| systemd_quoted(a)));
    Ok(format!(
        "# Written by Craft Cloud (`craft-cloud-agent autostart on`); `autostart off` removes it.\n\
         [Unit]\n\
         Description=Craft Cloud agent: keeps the synced folder in step with the server\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=30\n\
         RestartPreventExitStatus=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        command.join(" ")
    ))
}

/// The XDG autostart entry for `launch`.
pub fn desktop_entry(launch: &Launch) -> Result<String, AutostartError> {
    let mut command = vec![desktop_quoted(&launch.program()?)];
    command.extend(launch.args()?.iter().map(|a| desktop_quoted(a)));
    Ok(format!(
        "# Written by Craft Cloud (`craft-cloud-agent autostart on`); `autostart off` removes it.\n\
         [Desktop Entry]\n\
         Type=Application\n\
         Name=Craft Cloud agent\n\
         Comment=Keeps the synced folder in step with the server\n\
         Exec={}\n\
         NoDisplay=true\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        command.join(" ")
    ))
}

/// One word of `ExecStart`, quoted as systemd reads it (systemd.syntax(7), systemd.service(5)): in double quotes, with
/// `\` and `"` escaped by a backslash, `%` doubled (specifiers) and `$` doubled (variables).
fn systemd_quoted(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    for c in word.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One word of `Exec`, quoted as the Desktop Entry spec reads it: in double quotes, with `"`, `` ` ``, `$` and `\`
/// escaped by a backslash and `%` doubled (field codes); then the value's own escaping doubles every backslash (it is
/// applied before the quoting is read, so a backslash in the path is written as four).
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

/// The unit's path, for tests and messages.
pub fn unit_file(roots: &Roots) -> PathBuf {
    unit_path(roots)
}

/// The XDG entry's path, for tests and messages.
pub fn desktop_file(roots: &Roots) -> PathBuf {
    desktop_path(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(exe: &str) -> Launch {
        Launch { exe: PathBuf::from(exe), log: None }
    }

    /// Every character systemd or the desktop spec treats specially comes out as itself once read back.
    #[test]
    fn special_characters_are_escaped() {
        assert_eq!(systemd_quoted(r#"/a b/c"d\e%f$g"#), r#""/a b/c\"d\\e%%f$$g""#);
        assert_eq!(desktop_quoted(r#"/a b/c"d\e%f$g`h"#), r#""/a b/c\\"d\\\\e%%f\\$g\\`h""#);
        let line = unit(&launch("/opt/x/agent")).unwrap();
        assert!(line.contains("ExecStart=\"/opt/x/agent\" \"run\" \"--log\"\n"), "{line}");
        assert!(matches!(unit(&launch("/opt/x\nExecStart=/bin/evil")), Err(AutostartError::BadPath(_))), "no line breaks");
    }
}
