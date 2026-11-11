//! Reads an externally written update status file. The server never contacts the network.

pub use craft_cloud_proto::api::AdminUpdateInfo as UpdateInfo;
use serde::Deserialize;
use std::path::PathBuf;

const MAX_STATUS_BYTES: u64 = 4096;

#[derive(Debug, Clone)]
pub struct UpdateSettings {
    pub status_file: Option<PathBuf>,
    pub install_dir: String,
    pub repo: String,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self { status_file: None, install_dir: "/opt/craft-cloud".into(), repo: "itrycodinganditsbad/craft-cloud".into() }
    }
}

#[derive(Deserialize)]
struct Status {
    channel: String,
    latest: String,
    checked_at: String,
}

impl UpdateSettings {
    pub fn from_config(config: &crate::config::UpdatesConfig) -> Self {
        Self {
            status_file: config.status_file.clone(),
            install_dir: config.install_dir.clone().unwrap_or_else(|| "/opt/craft-cloud".into()),
            repo: config.repo.clone(),
        }
    }
}

/// Reads the status file's bytes, at most `MAX_STATUS_BYTES`. Anything that is not a regular file (a symbolic link, a
/// pipe, a device) is refused, and the read is bounded by `take`, not by a length that can change between the check
/// and the read.
fn read_status_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read;
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    // The same check on the opened file: the path may have been replaced since the check above.
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_STATUS_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_STATUS_BYTES {
        return None;
    }
    Some(bytes)
}

pub fn read(settings: &UpdateSettings) -> Option<UpdateInfo> {
    let path = settings.status_file.as_ref()?;
    let bytes = read_status_bytes(path)?;
    let status: Status = serde_json::from_slice(&bytes).ok()?;
    if status.checked_at.is_empty() || status.checked_at.len() > 64 || !status.checked_at.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
        return None;
    }
    let (available, current) = match status.channel.as_str() {
        "release" if valid_release(&status.latest) => {
            let current = env!("CARGO_PKG_VERSION");
            let available = compare_versions(&status.latest, current).is_some_and(|order| order.is_gt());
            (available, current.to_owned())
        }
        "branch" if valid_sha(&status.latest) => {
            let revision = option_env!("CRAFT_CLOUD_REVISION")?;
            if !valid_sha(revision) {
                return None;
            }
            let available = !status.latest.eq_ignore_ascii_case(revision);
            (available, revision.to_owned())
        }
        _ => return None,
    };
    Some(UpdateInfo { available, latest: status.latest, current, channel: status.channel, checked_at: status.checked_at })
}

fn valid_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn valid_release(text: &str) -> bool {
    let value = text.strip_prefix('v').unwrap_or(text);
    let (core, suffix) = value.split_once('-').map_or((value, None), |(a, b)| (a, Some(b)));
    let nums: Vec<_> = core.split('.').collect();
    nums.len() == 3
        && nums.iter().all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        && suffix.is_none_or(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
}

fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    fn parts(v: &str) -> Option<([u64; 3], Option<&str>)> {
        let v = v.strip_prefix('v').unwrap_or(v);
        let (core, pre) = v.split_once('-').map_or((v, None), |(a, b)| (a, Some(b)));
        let nums: Vec<u64> = core.split('.').map(str::parse).collect::<Result<_, _>>().ok()?;
        Some(([*nums.first()?, *nums.get(1)?, *nums.get(2)?], pre))
    }
    let (an, ap) = parts(a)?;
    let (bn, bp) = parts(b)?;
    Some(an.cmp(&bn).then_with(|| match (ap, bp) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => compare_prerelease(x, y),
    }))
}

fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    for (left, right) in a.split('.').zip(b.split('.')) {
        let order = match (left.parse::<u64>(), right.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => left.cmp(right),
        };
        if !order.is_eq() {
            return order;
        }
    }
    a.split('.').count().cmp(&b.split('.').count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_compares_release_versions() {
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Some(std::cmp::Ordering::Equal));
        assert_eq!(compare_versions("1.2.4", "1.2.3"), Some(std::cmp::Ordering::Greater));
        assert_eq!(compare_versions("1.2.2", "1.2.3"), Some(std::cmp::Ordering::Less));
        assert_eq!(compare_versions("1.2.3", "1.2.3-rc.1"), Some(std::cmp::Ordering::Greater));
        assert!(valid_release("v1.2.3-rc.1"));
        assert!(!valid_release("<img>"));
    }

    #[test]
    fn ignores_missing_malformed_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("status.json");
        let settings = UpdateSettings { status_file: Some(file.clone()), ..UpdateSettings::default() };
        assert!(read(&settings).is_none());
        std::fs::write(&file, br#"{"channel":"release","latest":"<img>","checked_at":"today"}"#).unwrap();
        assert!(read(&settings).is_none());
        std::fs::write(&file, vec![b'x'; 4097]).unwrap();
        assert!(read(&settings).is_none());
    }

    #[test]
    fn a_file_at_the_limit_is_read_and_one_byte_more_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("status.json");
        let mut json = br#"{"channel":"release","latest":"v9.9.9","checked_at":"today"}"#.to_vec();
        json.resize(MAX_STATUS_BYTES as usize, b' ');
        std::fs::write(&file, &json).unwrap();
        assert_eq!(read_status_bytes(&file).map(|bytes| bytes.len() as u64), Some(MAX_STATUS_BYTES));
        json.push(b' ');
        std::fs::write(&file, &json).unwrap();
        assert!(read_status_bytes(&file).is_none());
    }

    #[test]
    fn a_directory_is_not_a_status_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_status_bytes(dir.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.json");
        std::fs::write(&target, br#"{"channel":"release","latest":"v9.9.9","checked_at":"today"}"#).unwrap();
        let link = dir.path().join("status.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_status_bytes(&target).is_some());
        assert!(read_status_bytes(&link).is_none());
        let zero = dir.path().join("zero.json");
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        assert!(read_status_bytes(&zero).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("status.json");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|status| status.success()) {
            return; // no mkfifo on this host
        }
        assert!(read_status_bytes(&fifo).is_none());
    }
}
