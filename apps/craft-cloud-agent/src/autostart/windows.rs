//! Windows: the value [`VALUE`] under `HKEY_CURRENT_USER\<run key>` ([`super::RUN_KEY`]), which Windows runs at the
//! user's login: the background build, `"<dir>\craft-cloud-agent-bg.exe" run`, so no console window opens. The value
//! is written by [`run_value`], a pure function, so its format is tested on every OS; the registry is only touched on
//! Windows.

use std::path::{Path, PathBuf};

#[cfg(windows)]
use super::Roots;
use super::{AutostartError, Launch};

/// The value's name.
pub const VALUE: &str = "Craft Cloud";
/// The background build's file name.
pub const BACKGROUND_EXE: &str = "craft-cloud-agent-bg.exe";

/// The background build next to `exe` (or `exe` itself when it is the background build).
pub fn background_build(exe: &Path) -> Result<PathBuf, AutostartError> {
    let is_bg = exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case(BACKGROUND_EXE));
    let bg = if is_bg { exe.to_owned() } else { exe.with_file_name(BACKGROUND_EXE) };
    if bg.is_file() { Ok(bg) } else { Err(AutostartError::NoBackgroundBuild(bg)) }
}

/// The command line the value holds: the program and every argument in double quotes but `run` (Windows paths can't
/// hold a `"`, so nothing needs escaping inside them).
pub fn run_value(launch: &Launch) -> Result<String, AutostartError> {
    let program = launch.program()?;
    if program.contains('"') {
        return Err(AutostartError::BadPath(launch.exe.clone()));
    }
    let mut line = format!("\"{program}\"");
    for arg in launch.args()? {
        if arg.contains('"') {
            return Err(AutostartError::BadPath(PathBuf::from(arg)));
        }
        if arg.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            line.push_str(&format!(" {arg}"));
        } else {
            line.push_str(&format!(" \"{arg}\""));
        }
    }
    Ok(line)
}

#[cfg(windows)]
fn registry(e: windows_registry::Result<()>) -> Result<(), AutostartError> {
    e.map_err(|e| AutostartError::Registry(e.message()))
}

/// `ERROR_FILE_NOT_FOUND` as an `HRESULT`: the key or value isn't there.
#[cfg(windows)]
const NOT_FOUND: i32 = -2_147_024_894;

/// Writes the value (replacing an earlier one).
#[cfg(windows)]
pub fn register(roots: &Roots, launch: &Launch) -> Result<(), AutostartError> {
    let line = run_value(launch)?;
    let key = windows_registry::CURRENT_USER.create(&roots.run_key).map_err(|e| AutostartError::Registry(e.message()))?;
    registry(key.set_string(VALUE, line))
}

/// Removes the value; one that isn't there (or a key that isn't) is fine.
#[cfg(windows)]
pub fn unregister(roots: &Roots) -> Result<(), AutostartError> {
    let key = match windows_registry::CURRENT_USER.options().read().write().open(&roots.run_key) {
        Ok(key) => key,
        Err(e) if e.code().0 == NOT_FOUND => return Ok(()),
        Err(e) => return Err(AutostartError::Registry(e.message())),
    };
    match key.remove_value(VALUE) {
        Err(e) if e.code().0 == NOT_FOUND => Ok(()),
        other => registry(other),
    }
}

#[cfg(windows)]
pub fn is_registered(roots: &Roots) -> Result<bool, AutostartError> {
    let key = match windows_registry::CURRENT_USER.open(&roots.run_key) {
        Ok(key) => key,
        Err(e) if e.code().0 == NOT_FOUND => return Ok(false),
        Err(e) => return Err(AutostartError::Registry(e.message())),
    };
    match key.get_string(VALUE) {
        Ok(_) => Ok(true),
        Err(e) if e.code().0 == NOT_FOUND => Ok(false),
        Err(e) => Err(AutostartError::Registry(e.message())),
    }
}

/// The value as Windows holds it, for tests (`None`: not there).
#[cfg(windows)]
pub fn read_value(roots: &Roots) -> Result<Option<String>, AutostartError> {
    let Ok(key) = windows_registry::CURRENT_USER.open(&roots.run_key) else { return Ok(None) };
    match key.get_string(VALUE) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.code().0 == NOT_FOUND => Ok(None),
        Err(e) => Err(AutostartError::Registry(e.message())),
    }
}

#[cfg(not(windows))]
pub fn register(_roots: &super::Roots, _launch: &Launch) -> Result<(), AutostartError> {
    Err(AutostartError::Unsupported)
}

#[cfg(not(windows))]
pub fn unregister(_roots: &super::Roots) -> Result<(), AutostartError> {
    Err(AutostartError::Unsupported)
}

#[cfg(not(windows))]
pub fn is_registered(_roots: &super::Roots) -> Result<bool, AutostartError> {
    Err(AutostartError::Unsupported)
}
