//! `cargo xtask suite-deps <Cargo.lock…>`: the crates that what the Craft apps link (`craft-cloud-kit`,
//! `craft-cloud-egui` and what they pull in, normal dependencies only) brings that none of the given lock files has.
//! The Craft apps take only crates another Craft app already locks, with Loro's own crates the one exception, so the
//! list is split into the crates that come only through Loro and the rest; the command fails when the rest isn't empty.
//! CI doesn't run it (the apps' lock files aren't in this repository): run it with their paths, as
//! `cargo xtask suite-deps ../*craft/Cargo.lock`.

use std::collections::BTreeSet;

/// The crates the Craft apps link.
const APP_CRATES: &[&str] = &["craft-cloud-kit", "craft-cloud-egui", "craft-cloud-app"];
/// The crate whose own dependencies are the accepted exception.
const LORO: &str = "loro";

pub fn run(args: &[&str]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: cargo xtask suite-deps <Cargo.lock>…  (the Craft apps' lock files)".into());
    }
    let mut known = BTreeSet::new();
    for path in args {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        known.extend(lock_names(&text));
    }
    let loro = tree(LORO)?.unwrap_or_default();
    let mut through_loro = BTreeSet::new();
    let mut other = BTreeSet::new();
    for app in APP_CRATES {
        let Some(deps) = tree(app)? else {
            println!("{app}: not in the workspace");
            continue;
        };
        for (name, version) in deps {
            if known.contains(&name) {
                continue;
            }
            let line = format!("{name} {version}");
            let list = if loro.iter().any(|(n, _)| *n == name) { &mut through_loro } else { &mut other };
            list.insert(line);
        }
    }
    println!("suite-deps: {} lock files, {} crate names in them", args.len(), known.len());
    println!("crates of {} that no lock has:", APP_CRATES.join(" and "));
    println!("  through Loro (the accepted exception): {}", if through_loro.is_empty() { "none".to_owned() } else { String::new() });
    for line in &through_loro {
        println!("    {line}");
    }
    println!("  others: {}", if other.is_empty() { "none" } else { "" });
    for line in &other {
        println!("    {line}");
    }
    if other.is_empty() { Ok(()) } else { Err(format!("{} crates no Craft app locks (see above)", other.len())) }
}

/// The package names a `Cargo.lock` lists.
fn lock_names(text: &str) -> BTreeSet<String> {
    text.lines().filter_map(|line| line.strip_prefix("name = \"")).filter_map(|rest| rest.strip_suffix('"')).map(str::to_owned).collect()
}

/// `package`'s normal dependencies (itself included) as (name, version), without the workspace's own crates; `None`
/// when the build has no such package.
fn tree(package: &str) -> Result<Option<BTreeSet<(String, String)>>, String> {
    let mut cmd = crate::cargo();
    cmd.args(["tree", "--locked", "-p", package, "-e", "normal", "--target", "all", "--prefix", "none", "--format", "{p}", "--color", "never"]);
    let output = cmd.output().map_err(|e| format!("cargo tree: failed to spawn: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("did not match any packages") {
            return Ok(None);
        }
        return Err(format!("cargo tree -p {package}: exited with {}: {}", output.status, stderr.trim()));
    }
    Ok(Some(parse_tree(&String::from_utf8_lossy(&output.stdout))))
}

/// `cargo tree --format {p}` lines as (name, version), skipping path crates (the workspace's own: `(/…)` after the
/// version) and the `(*)` of repeats.
fn parse_tree(text: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(name), Some(version)) = (words.next(), words.next()) else { continue };
        let rest: Vec<&str> = words.collect();
        if rest.iter().any(|w| w.starts_with("(/") || w.starts_with("(C:") || w.starts_with("(\\")) {
            continue;
        }
        out.insert((name.to_owned(), version.trim_start_matches('v').to_owned()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_and_trees_parse() {
        let lock = "[[package]]\nname = \"egui\"\nversion = \"0.36.2\"\n\n[[package]]\nname = \"serde\"\n";
        assert_eq!(lock_names(lock), BTreeSet::from(["egui".to_owned(), "serde".to_owned()]));
        let tree = "craft-cloud-kit v0.1.0 (/home/x/crates/kit)\nloro v1.16.2\nserde v1.0.228\nserde v1.0.228 (*)\n";
        let parsed = parse_tree(tree);
        assert_eq!(parsed, BTreeSet::from([("loro".to_owned(), "1.16.2".to_owned()), ("serde".to_owned(), "1.0.228".to_owned())]));
    }
}
