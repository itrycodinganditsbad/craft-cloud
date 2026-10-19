//! `cargo xtask package`: release builds of the app and the agent, packed for the OS this runs on, into
//! `<target>/package/`:
//!
//! - **Windows**: `craft-cloud-setup.exe`, the setup with the three exes built in (built after them, with
//!   `CRAFT_CLOUD_SETUP_PAYLOAD` naming their folder).
//! - **macOS**: `Craft Cloud.app` (`Contents/MacOS/craft-cloud` and `craft-cloud-agent`, `Info.plist` with the bundle ID
//!   `ch.craftcloud.app`, `Resources/icon.icns`), zipped with `ditto`. Unsigned.
//! - **Linux**: `craft-cloud-<version>-linux-<arch>.tar.gz`: both programs, the icon, and `install.sh` and
//!   `uninstall.sh` (`packaging/linux/`), which install into `~/.local`.
//!
//! Each package carries `LICENSES.txt` (`crate::notices`), installed next to the programs: Craft Cloud's own licences
//! and NOTICE, and those of every third-party crate compiled into the programs, read from the crates cargo fetched.
//!
//! The last line printed is the package's path.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::notices::LICENSES;

/// The target folder cargo builds into: `$CARGO_TARGET_DIR` (relative to the workspace) or `target/`.
fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => root.join(dir),
        None => root.join("target"),
    }
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to).map(|_| ()).map_err(|e| format!("{} → {}: {e}", from.display(), to.display()))
}

fn fresh_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

pub fn run(root: &Path) -> Result<(), String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| format!("Cargo.toml: {e}"))?;
    let version = crate::version::read(&manifest)?;
    let target = target_dir(root);
    let out = target.join("package");
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    crate::cargo_step(&["build", "--release", "--locked", "-p", "craft-cloud-desktop", "--bin", "craft-cloud", "-p", "craft-cloud-agent", "--bins"])?;
    let release = target.join("release");
    crate::notices::write(root, &release.join(LICENSES))?;
    let package = if cfg!(windows) {
        windows(&release, &out)?
    } else if cfg!(target_os = "macos") {
        macos(root, &release, &out, &version)?
    } else {
        linux(root, &release, &out, &version)?
    };
    println!("{}", package.display());
    Ok(())
}

/// The setup, built with the exes and the licences in it.
fn windows(release: &Path, out: &Path) -> Result<PathBuf, String> {
    let mut build = crate::cargo();
    build.args(["build", "--release", "--locked", "-p", "craft-cloud-setup"]).env("CRAFT_CLOUD_SETUP_PAYLOAD", release);
    crate::run(build, "cargo build --release -p craft-cloud-setup (with the payload)")?;
    let setup = out.join("craft-cloud-setup.exe");
    copy(&release.join("craft-cloud-setup.exe"), &setup)?;
    Ok(setup)
}

/// The app bundle's `Info.plist`.
pub fn info_plist(version: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>CFBundleIdentifier</key>\n\
         \x20   <string>ch.craftcloud.app</string>\n\
         \x20   <key>CFBundleName</key>\n\
         \x20   <string>Craft Cloud</string>\n\
         \x20   <key>CFBundleDisplayName</key>\n\
         \x20   <string>Craft Cloud</string>\n\
         \x20   <key>CFBundleExecutable</key>\n\
         \x20   <string>craft-cloud</string>\n\
         \x20   <key>CFBundleIconFile</key>\n\
         \x20   <string>icon</string>\n\
         \x20   <key>CFBundlePackageType</key>\n\
         \x20   <string>APPL</string>\n\
         \x20   <key>CFBundleShortVersionString</key>\n\
         \x20   <string>{version}</string>\n\
         \x20   <key>CFBundleVersion</key>\n\
         \x20   <string>{version}</string>\n\
         \x20   <key>LSMinimumSystemVersion</key>\n\
         \x20   <string>11.0</string>\n\
         \x20   <key>LSUIElement</key>\n\
         \x20   <false/>\n\
         \x20   <key>NSHighResolutionCapable</key>\n\
         \x20   <true/>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// `Craft Cloud.app`, zipped.
fn macos(root: &Path, release: &Path, out: &Path, version: &str) -> Result<PathBuf, String> {
    let app = out.join("Craft Cloud.app");
    fresh_dir(&app)?;
    let contents = app.join("Contents");
    let (macos, resources) = (contents.join("MacOS"), contents.join("Resources"));
    for dir in [&macos, &resources] {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    for name in ["craft-cloud", "craft-cloud-agent"] {
        copy(&release.join(name), &macos.join(name))?;
    }
    copy(&root.join("apps").join("craft-cloud-desktop").join("assets").join("icon.icns"), &resources.join("icon.icns"))?;
    copy(&release.join(LICENSES), &resources.join(LICENSES))?;
    std::fs::write(contents.join("Info.plist"), info_plist(version)).map_err(|e| format!("Info.plist: {e}"))?;
    let zip = out.join(format!("craft-cloud-{version}-macos-{}.zip", std::env::consts::ARCH));
    let _ = std::fs::remove_file(&zip);
    let mut ditto = Command::new("ditto");
    ditto.args(["-c", "-k", "--sequesterRsrc", "--keepParent"]).arg(&app).arg(&zip);
    crate::run(ditto, "ditto (zip the app)")?;
    Ok(zip)
}

/// The tarball's name, without `.tar.gz`.
pub fn linux_name(version: &str, arch: &str) -> String {
    format!("craft-cloud-{version}-linux-{arch}")
}

/// The tarball: the programs, the icon, the licences and the scripts in a folder of the package's name.
fn linux(root: &Path, release: &Path, out: &Path, version: &str) -> Result<PathBuf, String> {
    let name = linux_name(version, std::env::consts::ARCH);
    let stage = out.join(&name);
    fresh_dir(&stage)?;
    for program in ["craft-cloud", "craft-cloud-agent"] {
        copy(&release.join(program), &stage.join(program))?;
    }
    copy(&root.join("apps").join("craft-cloud-desktop").join("assets").join("icon.png"), &stage.join("craft-cloud.png"))?;
    copy(&release.join(LICENSES), &stage.join(LICENSES))?;
    for script in ["install.sh", "uninstall.sh"] {
        let to = stage.join(script);
        copy(&root.join("packaging").join("linux").join(script), &to)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("{}: {e}", to.display()))?;
        }
    }
    let tarball = out.join(format!("{name}.tar.gz"));
    let mut tar = Command::new("tar");
    tar.arg("-czf").arg(&tarball).arg("-C").arg(out).arg(&name);
    crate::run(tar, "tar (pack the tarball)")?;
    Ok(tarball)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_the_bundles_plist() {
        assert_eq!(linux_name("0.1.0", "x86_64"), "craft-cloud-0.1.0-linux-x86_64");
        let plist = info_plist("0.1.0");
        assert!(plist.contains("<string>ch.craftcloud.app</string>") && plist.contains("<key>LSUIElement</key>\n    <false/>"), "{plist}");
        assert!(plist.contains("<string>craft-cloud</string>") && plist.contains("<string>0.1.0</string>"));
    }

    /// `install.sh` and `uninstall.sh` in a temporary home, with stand-ins for the programs (the agent records how it
    /// was called): the files, links, menu entry and icon go in and out; start at login is moved to the new copy when
    /// the agent had it; the synced and data folders stay.
    #[cfg(target_os = "linux")]
    #[test]
    fn install_and_uninstall_scripts_in_a_temporary_home() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::root();
        let tmp = std::env::temp_dir().join(format!("craft-cloud-install-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (home, src) = (tmp.join("home"), tmp.join("craft-cloud-0.0.1-linux-x86_64"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let calls = tmp.join("calls.txt");
        // The agent: never running (quit fails), records every call.
        let agent = format!("#!/bin/sh\necho \"agent $*\" >> \"{}\"\n[ \"$1\" = quit ] && exit 1\nexit 0\n", calls.display());
        std::fs::write(src.join("craft-cloud-agent"), agent).unwrap();
        std::fs::write(src.join("craft-cloud"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(src.join("craft-cloud.png"), "png").unwrap();
        std::fs::write(src.join(LICENSES), "licences").unwrap();
        for script in ["install.sh", "uninstall.sh"] {
            std::fs::copy(root.join("packaging").join("linux").join(script), src.join(script)).unwrap();
        }
        for file in ["craft-cloud-agent", "craft-cloud", "install.sh", "uninstall.sh"] {
            std::fs::set_permissions(src.join(file), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A synced folder, the agent's data folder, and the agent's own start-at-login unit (lptp1's).
        let synced = home.join("Craft Cloud");
        let data = home.join(".local").join("share").join("craft-cloud-agent");
        let unit = home.join(".config").join("systemd").join("user").join("craft-cloud-agent.service");
        for dir in [&synced, &data, &unit.parent().unwrap().to_owned()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(synced.join("notes.txt"), "mine").unwrap();
        std::fs::write(data.join("account.json"), "{}").unwrap();
        std::fs::write(&unit, "[Service]\nExecStart=/home/fabio/bin/craft-cloud-agent run\n").unwrap();
        let app_entry = home.join(".config").join("autostart").join("craft-cloud.desktop");
        std::fs::create_dir_all(app_entry.parent().unwrap()).unwrap();
        std::fs::write(&app_entry, "[Desktop Entry]\n").unwrap();

        let sh =
            |script: &str| Command::new("sh").arg(src.join(script)).env_clear().env("HOME", &home).env("PATH", "/usr/bin:/bin").output().unwrap();
        let installed = sh("install.sh");
        assert!(installed.status.success(), "{installed:?}");
        let lib = home.join(".local").join("lib").join("craft-cloud");
        for program in ["craft-cloud", "craft-cloud-agent"] {
            assert!(lib.join(program).is_file(), "{program}");
            let link = home.join(".local").join("bin").join(program);
            assert_eq!(std::fs::read_link(&link).unwrap(), lib.join(program));
        }
        let menu = std::fs::read_to_string(home.join(".local/share/applications/craft-cloud.desktop")).unwrap();
        assert!(menu.contains(&format!("Exec=\"{}\"", lib.join("craft-cloud").display())) && menu.contains("Name=Craft Cloud"), "{menu}");
        assert!(home.join(".local/share/icons/hicolor/256x256/apps/craft-cloud.png").is_file());
        assert_eq!(std::fs::read_to_string(lib.join(LICENSES)).unwrap(), "licences", "the licences go along");
        assert!(!home.join("Desktop").exists(), "nothing on the desktop");
        let said = std::fs::read_to_string(&calls).unwrap();
        assert!(said.contains("agent quit") && said.contains("agent autostart on"), "{said}");
        assert!(!said.contains("agent run"), "an agent that wasn't running isn't started: {said}");

        // Again with --no-menu: the menu entry an earlier install made goes, the rest stays.
        let again = Command::new("sh")
            .arg(src.join("install.sh"))
            .arg("--no-menu")
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(again.status.success(), "{again:?}");
        assert!(!home.join(".local/share/applications/craft-cloud.desktop").exists(), "no menu entry");
        assert!(lib.join("craft-cloud").is_file() && home.join(".local/share/icons/hicolor/256x256/apps/craft-cloud.png").is_file());
        let wrong = Command::new("sh").arg(src.join("install.sh")).arg("--no-such").env_clear().env("HOME", &home).output().unwrap();
        assert_eq!(wrong.status.code(), Some(2), "{wrong:?}");

        let app_files = home.join(".local/share/craft-cloud-app");
        std::fs::create_dir_all(&app_files).unwrap();
        std::fs::write(app_files.join("window.lock"), "").unwrap();
        let old_loose = home.join(".local/share/craft-cloud-tray.lock");
        std::fs::write(&old_loose, "").unwrap();
        let removed = sh("uninstall.sh");
        assert!(removed.status.success(), "{removed:?}");
        assert!(!app_files.exists() && !old_loose.exists(), "the app's single-instance files go, old names too");
        assert!(!lib.exists() && !home.join(".local/bin/craft-cloud").exists());
        assert!(!home.join(".local/share/applications/craft-cloud.desktop").exists());
        assert!(!app_entry.exists(), "the app's own login entry goes");
        assert!(std::fs::read_to_string(&calls).unwrap().contains("agent autostart off"));
        assert!(synced.join("notes.txt").exists() && data.join("account.json").exists(), "the files and the sign-in stay");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An unpacked tarball in a temporary home, with an agent stand-in that answers `quit` `quits` times (it runs,
    /// finishing a pass) and then no more, a `systemctl` stand-in, and the agent's systemd unit: lptp1's upgrade. The
    /// running agent's process is a `sleep` of `lasts` seconds (none for 0), named in `local-api.json`; every call
    /// made while it still runs is recorded with " [old agent running]".
    #[cfg(target_os = "linux")]
    struct Upgrade {
        tmp: PathBuf,
        home: PathBuf,
        src: PathBuf,
        calls: PathBuf,
        bin: PathBuf,
        pid: Option<u32>,
    }

    #[cfg(target_os = "linux")]
    impl Upgrade {
        fn new(name: &str, quits: u32, lasts: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let tmp = std::env::temp_dir().join(format!("craft-cloud-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            let (home, src, bin) = (tmp.join("home"), tmp.join("pkg"), tmp.join("fakebin"));
            let data = home.join(".local/share/craft-cloud-agent");
            for dir in [&home, &src, &bin, &data] {
                std::fs::create_dir_all(dir).unwrap();
            }
            // The agent's process: a `sleep` that isn't this test's child (so it never lingers as a zombie).
            let pid = (lasts > 0).then(|| {
                let out = Command::new("sh").args(["-c", &format!("sleep {lasts} >/dev/null 2>&1 & echo $!")]).output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().unwrap()
            });
            if let Some(pid) = pid {
                std::fs::write(
                    data.join("local-api.json"),
                    format!("{{\n  \"schema\": 1,\n  \"port\": 1,\n  \"token\": \"t\",\n  \"pid\": {pid}\n}}"),
                )
                .unwrap();
            }
            let pid_text = pid.map(|p| p.to_string()).unwrap_or_default();
            let note = format!("$(if [ -n \"{pid_text}\" ] && kill -0 {pid_text} 2>/dev/null; then echo ' [old agent running]'; fi)");
            let calls = tmp.join("calls.txt");
            let count = tmp.join("quits.txt");
            let agent = format!(
                "#!/bin/sh\necho \"$0 $*{note}\" >> \"{calls}\"\nif [ \"$1\" = quit ]; then\n  n=$(cat \"{count}\" 2>/dev/null || echo 0)\n  \
                 n=$((n + 1)); echo $n > \"{count}\"\n  [ $n -le {quits} ] && exit 0\n  exit 1\nfi\nexit 0\n",
                calls = calls.display(),
                count = count.display()
            );
            std::fs::write(src.join("craft-cloud-agent"), agent).unwrap();
            std::fs::write(src.join("craft-cloud"), "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::write(src.join("craft-cloud.png"), "png").unwrap();
            std::fs::write(src.join(LICENSES), "licences").unwrap();
            std::fs::copy(crate::root().join("packaging/linux/install.sh"), src.join("install.sh")).unwrap();
            std::fs::write(bin.join("systemctl"), format!("#!/bin/sh\necho \"systemctl $*{note}\" >> \"{}\"\n", calls.display())).unwrap();
            for file in [src.join("craft-cloud-agent"), src.join("craft-cloud"), src.join("install.sh"), bin.join("systemctl")] {
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let unit = home.join(".config/systemd/user/craft-cloud-agent.service");
            std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
            std::fs::write(&unit, "[Service]\nExecStart=/home/fabio/.local/bin/craft-cloud-agent run\n").unwrap();
            Self { tmp, home, src, calls, bin, pid }
        }

        fn without_unit(self) -> Self {
            std::fs::remove_file(self.home.join(".config/systemd/user/craft-cloud-agent.service")).unwrap();
            self
        }

        fn install(&self, wait: &str) -> std::process::Output {
            Command::new("sh")
                .arg(self.src.join("install.sh"))
                .env_clear()
                .env("HOME", &self.home)
                .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
                .env("CRAFT_CLOUD_INSTALL_WAIT", wait)
                .output()
                .unwrap()
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.calls).unwrap_or_default().lines().map(str::to_owned).collect()
        }

        /// The old agent's process ends (its pass is done).
        fn agent_ends(&self) {
            if let Some(pid) = self.pid {
                let _ = Command::new("kill").arg(pid.to_string()).status();
            }
        }

        fn src_agent(&self) -> String {
            self.src.join("craft-cloud-agent").display().to_string()
        }

        fn lib_agent(&self) -> String {
            self.home.join(".local/lib/craft-cloud/craft-cloud-agent").display().to_string()
        }

        /// Waits (up to 20 s) for the calls to end with `last`: the agent is started again in the background.
        fn ends_with(&self, last: &str) -> Vec<String> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let calls = self.calls();
                if calls.last().map(String::as_str) == Some(last) || std::time::Instant::now() > deadline {
                    return calls;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for Upgrade {
        fn drop(&mut self) {
            self.agent_ends();
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }

    /// The agent runs (it answers `quit` twice while it finishes its pass, and its process ends a moment after its API
    /// is gone): its process is waited for, then the new copy is put in place, its start at login pointed at it, and
    /// systemd stops whatever the unit still runs and starts the agent. Nothing happens while the old process runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_upgrade_waits_for_the_agents_process_then_starts_it_under_systemd() {
        let up = Upgrade::new("upgrade", 2, 3);
        let out = up.install("60");
        assert!(out.status.success(), "{out:?}");
        let quit = format!("{} quit [old agent running]", up.src_agent());
        let expected = [
            quit.clone(),
            quit.clone(),
            quit,
            format!("{} autostart on", up.lib_agent()),
            "systemctl --user stop craft-cloud-agent.service".to_owned(),
            "systemctl --user start craft-cloud-agent.service".to_owned(),
        ];
        assert_eq!(up.calls(), expected, "nothing after the quits until the old process had ended");
    }

    /// The same without a systemd unit: the new copy is run once the old process has ended.
    #[cfg(target_os = "linux")]
    #[test]
    fn without_a_unit_an_upgrade_runs_the_new_copy_once_the_old_one_has_ended() {
        let up = Upgrade::new("upgrade-nounit", 1, 3).without_unit();
        let out = up.install("60");
        assert!(out.status.success(), "{out:?}");
        let run = format!("{} run --log", up.lib_agent());
        let calls = up.ends_with(&run);
        assert_eq!(calls.last(), Some(&run), "{calls:?}");
        assert!(calls.iter().filter(|c| c.ends_with("[old agent running]")).all(|c| c.ends_with("quit [old agent running]")), "{calls:?}");
    }

    /// An agent still finishing its sync (its process doesn't end) when the wait runs out: nothing is installed, and
    /// once its process has ended (it has accepted the quit; nothing else would restart it) systemd starts it again.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_agent_that_doesnt_stop_in_time_is_started_again_once_it_has_ended_and_nothing_changes() {
        let up = Upgrade::new("timeout", 1, 120);
        let out = up.install("2");
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("nothing was changed, and it starts again once done"), "{out:?}");
        assert!(!up.home.join(".local/lib/craft-cloud").exists(), "nothing installed");
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(!up.calls().iter().any(|c| c.starts_with("systemctl")), "not while it runs: {:?}", up.calls());
        up.agent_ends();
        let start = "systemctl --user start craft-cloud-agent.service";
        assert_eq!(up.ends_with(start).last().map(String::as_str), Some(start));
    }

    /// The same without a unit: the copy installed earlier is run again; the unpacked package's never is.
    #[cfg(target_os = "linux")]
    #[test]
    fn without_a_unit_an_agent_that_doesnt_stop_in_time_is_run_again_from_the_installed_copy() {
        let up = Upgrade::new("timeout-nounit", 1, 120).without_unit();
        let lib = up.home.join(".local/lib/craft-cloud");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::copy(up.src.join("craft-cloud-agent"), lib.join("craft-cloud-agent")).unwrap();
        let out = up.install("2");
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        up.agent_ends();
        let run = format!("{} run --log", up.lib_agent());
        assert_eq!(up.ends_with(&run).last(), Some(&run));

        // Nothing installed and nothing on PATH: nothing is started, and that is said.
        let bare = Upgrade::new("timeout-bare", 1, 120).without_unit();
        let out = bare.install("2");
        assert!(String::from_utf8_lossy(&out.stderr).contains("start it again with"), "{out:?}");
        bare.agent_ends();
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(!bare.calls().iter().any(|c| c.contains("run --log")), "{:?}", bare.calls());
    }

    /// A failure once the agent is stopped (here `~/.local/bin` is a file, so linking fails) still starts it again.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_install_after_the_stop_starts_the_agent_again() {
        let up = Upgrade::new("failed", 1, 1);
        std::fs::create_dir_all(up.home.join(".local")).unwrap();
        std::fs::write(up.home.join(".local/bin"), "not a folder").unwrap();
        let out = up.install("60");
        assert!(!out.status.success(), "{out:?}");
        assert_eq!(up.calls().last().map(String::as_str), Some("systemctl --user start craft-cloud-agent.service"), "{out:?}");
    }
}
