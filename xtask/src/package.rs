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
//! The last line printed is the package's path.

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// The setup, built with the exes in it.
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

/// The tarball: the programs, the icon and the scripts in a folder of the package's name.
fn linux(root: &Path, release: &Path, out: &Path, version: &str) -> Result<PathBuf, String> {
    let name = linux_name(version, std::env::consts::ARCH);
    let stage = out.join(&name);
    fresh_dir(&stage)?;
    for program in ["craft-cloud", "craft-cloud-agent"] {
        copy(&release.join(program), &stage.join(program))?;
    }
    copy(&root.join("apps").join("craft-cloud-desktop").join("assets").join("icon.png"), &stage.join("craft-cloud.png"))?;
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
        assert!(!home.join("Desktop").exists(), "nothing on the desktop");
        let said = std::fs::read_to_string(&calls).unwrap();
        assert!(said.contains("agent quit") && said.contains("agent autostart on"), "{said}");
        assert!(!said.contains("agent run"), "an agent that wasn't running isn't started: {said}");

        let removed = sh("uninstall.sh");
        assert!(removed.status.success(), "{removed:?}");
        assert!(!lib.exists() && !home.join(".local/bin/craft-cloud").exists());
        assert!(!home.join(".local/share/applications/craft-cloud.desktop").exists());
        assert!(!app_entry.exists(), "the app's own login entry goes");
        assert!(std::fs::read_to_string(&calls).unwrap().contains("agent autostart off"));
        assert!(synced.join("notes.txt").exists() && data.join("account.json").exists(), "the files and the sign-in stay");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
