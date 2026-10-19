//! macOS: a launch agent, `~/Library/LaunchAgents/ch.craftcloud.agent.plist`. launchd starts it at login
//! (`RunAtLoad`) and again after a crash, but not after a clean quit (`KeepAlive` with `SuccessfulExit` false), and
//! never sooner than 30 s after the last start (`ThrottleInterval`). The agent writes its own `run.log` (`run --log`,
//! rotated); what still reaches its standard output or error (a crash's message) goes to [`LAUNCHD_LOG`] in the data
//! folder (`StandardOutPath`, `StandardErrorPath`; launchd appends and doesn't rotate, but next to nothing goes
//! there). The file is written by [`plist`], a pure function, so its format is tested on every OS.

use std::path::PathBuf;

use super::{AutostartError, Launch, Roots, remove_file, write_file};

/// The launch agent's label, also its file's name.
pub const LABEL: &str = "ch.craftcloud.agent";
/// Where launchd writes what reaches the agent's own output, in the data folder.
pub const LAUNCHD_LOG: &str = "launchd.log";
/// The shortest time between two starts launchd makes, in seconds.
pub const THROTTLE_SECS: u32 = 30;

/// The launch agent's file.
pub fn plist_file(roots: &Roots) -> PathBuf {
    roots.home.join("Library").join("LaunchAgents").join(format!("{LABEL}.plist"))
}

/// Writes the launch agent for `launch` (replacing an earlier one); it runs from the next login on.
pub fn register(roots: &Roots, launch: &Launch) -> Result<(), AutostartError> {
    write_file(&plist_file(roots), &plist(launch)?)
}

/// Removes the launch agent; one that isn't there is fine.
pub fn unregister(roots: &Roots) -> Result<(), AutostartError> {
    remove_file(&plist_file(roots))
}

pub fn is_registered(roots: &Roots) -> Result<bool, AutostartError> {
    Ok(plist_file(roots).exists())
}

/// The launch agent's property list for `launch`.
pub fn plist(launch: &Launch) -> Result<String, AutostartError> {
    let mut arguments = format!("        <string>{}</string>\n", xml_escaped(&launch.program()?));
    for arg in launch.args()? {
        arguments.push_str(&format!("        <string>{}</string>\n", xml_escaped(&arg)));
    }
    // launchd keeps nothing the process prints unless told where: `launchd.log` in the data folder, both streams.
    let mut log = String::new();
    if let Some(path) = &launch.log {
        let path = xml_escaped(&super::text(path)?);
        for key in ["StandardOutPath", "StandardErrorPath"] {
            log.push_str(&format!("    <key>{key}</key>\n    <string>{path}</string>\n"));
        }
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by Craft Cloud (`craft-cloud-agent autostart on`); `autostart off` removes it. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key>\n\
         \x20   <string>{LABEL}</string>\n\
         \x20   <key>ProgramArguments</key>\n\
         \x20   <array>\n\
         {arguments}\
         \x20   </array>\n\
         {log}\
         \x20   <key>RunAtLoad</key>\n\
         \x20   <true/>\n\
         \x20   <key>KeepAlive</key>\n\
         \x20   <dict>\n\
         \x20       <key>SuccessfulExit</key>\n\
         \x20       <false/>\n\
         \x20   </dict>\n\
         \x20   <key>ThrottleInterval</key>\n\
         \x20   <integer>{THROTTLE_SECS}</integer>\n\
         </dict>\n\
         </plist>\n"
    ))
}

/// Text for XML character data: `&`, `<`, `>`, `"` and `'` as entities.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_special_characters_are_escaped() {
        assert_eq!(xml_escaped(r#"/A & B/<c>/"d"/'e'"#), "/A &amp; B/&lt;c&gt;/&quot;d&quot;/&apos;e&apos;");
        let launch = Launch { exe: "/Apps/R&D <x>/agent".into(), log: None };
        assert!(plist(&launch).unwrap().contains("<string>/Apps/R&amp;D &lt;x&gt;/agent</string>"));
    }
}
