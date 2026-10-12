//! The built binary, run as a user would: exit codes of usage errors and help, and `status` with nothing set up.

use std::process::{Command, Output};

fn agent(args: &[&str], data: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_craft-cloud-agent")).args(args).env("CRAFT_CLOUD_AGENT_DIR", data).output().unwrap()
}

#[test]
fn a_typo_is_a_failure_not_a_wait_for_approval() {
    let dir = tempfile::tempdir().unwrap();
    let out = agent(&["sync", "--aprove-deletes"], dir.path());
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--aprove-deletes"));
    assert_eq!(agent(&[], dir.path()).status.code(), Some(1), "no command at all");
}

#[test]
fn help_and_version_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let help = agent(&["--help"], dir.path());
    assert_eq!(help.status.code(), Some(0));
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("Exit codes") && text.contains("3 "), "{text}");
    assert_eq!(agent(&["--version"], dir.path()).status.code(), Some(0));
    assert_eq!(agent(&["sync", "--help"], dir.path()).status.code(), Some(0));
}

#[test]
fn status_with_an_empty_data_dir_says_not_signed_in() {
    let dir = tempfile::tempdir().unwrap();
    let out = agent(&["status"], &dir.path().join("data"));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("not signed in"));
}

/// A stop that needs the user exits 3 from the real binary, with the reason on standard error.
#[test]
fn a_damaged_account_file_exits_3() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("account.json"), "{not json").unwrap();
    for command in ["sync", "status"] {
        let out = agent(&[command], dir.path());
        assert_eq!(out.status.code(), Some(3), "{command}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("account.json") && err.contains("move it away"), "{command}: {err}");
    }
}
