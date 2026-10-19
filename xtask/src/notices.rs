//! `LICENSES.txt`, the licence notices that go in every package (`cargo xtask package`): Craft Cloud's own `NOTICE`,
//! `LICENSE-MIT` and `LICENSE-APACHE`, then every third-party crate compiled into the shipped programs (the normal
//! dependencies of the app, the agent and, on Windows, the setup, for this target; no build or dev dependencies and no
//! proc-macros, which don't end up in a binary), ordered by name and version. For each: its name, version and licence
//! expression, and the text of every licence, notice and copying file in its folder (a text already given is named,
//! not repeated); for MPL-2.0 crates, where their unmodified source is. A crate with no such file is covered only when
//! it can be taken under plain MIT or Apache-2.0 (whose texts are at the top); any other fails the package.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The file's name in every package.
pub const LICENSES: &str = "LICENSES.txt";

/// Files a crate keeps besides the usual ones at its top: the fonts egui builds into the app carry theirs with them.
const MORE_FILES: &[(&str, &[&str])] =
    &[("epaint_default_fonts", &["fonts/UFL.txt", "fonts/OFL.txt", "fonts/Hack-Regular.txt", "fonts/emoji-icon-font-mit-license.txt"])];

/// Crates that ship no licence file, whose licence text is the same as another crate's in the programs: (crate, the
/// crate whose text it is, why). `clipboard-win` leaves its BSL-1.0 file out of its package; `error-code`, by the same
/// author under the same licence and a dependency of it, ships it.
const SAME_TEXT_AS: &[(&str, &str, &str)] = &[("clipboard-win", "error-code", "the same author's, under the same licence")];

/// A third-party crate in the programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crate {
    pub name: String,
    pub version: String,
    /// Its SPDX expression, as its manifest gives it.
    pub license: String,
    /// Its folder (with its `Cargo.toml`).
    pub dir: PathBuf,
    /// The manifest's `license-file`, relative to `dir`.
    pub license_file: Option<String>,
}

/// Writes `LICENSES.txt` for the programs `cargo xtask package` builds on this OS to `to`.
pub fn write(root: &Path, to: &Path) -> Result<(), String> {
    let crates = shipped(root)?;
    let own = |name: &str| read(&root.join(name));
    let text = render(&own("NOTICE")?, &own("LICENSE-MIT")?, &own("LICENSE-APACHE")?, &crates)?;
    std::fs::write(to, text).map_err(|e| format!("{}: {e}", to.display()))
}

/// The packaged programs' crates: `-p` for each.
fn packages() -> Vec<&'static str> {
    let mut packages = vec!["craft-cloud-desktop", "craft-cloud-agent"];
    if cfg!(windows) {
        packages.push("craft-cloud-setup");
    }
    packages
}

/// The third-party crates compiled into the packaged programs, by name and version.
pub fn shipped(root: &Path) -> Result<Vec<Crate>, String> {
    let mut tree = crate::cargo();
    tree.args(["tree", "--locked", "-e", "normal,no-proc-macro", "--prefix", "none", "--format", "{p}", "--color", "never"]);
    for package in packages() {
        tree.args(["-p", package]);
    }
    let in_tree = parse_tree(&output(tree, "cargo tree")?);
    let mut metadata = crate::cargo();
    metadata.args(["metadata", "--format-version", "1", "--locked"]).current_dir(root);
    let all = parse_metadata(&output(metadata, "cargo metadata")?)?;
    // Workspace crates (no source) are Craft Cloud's own.
    Ok(all.into_iter().filter(|((name, version), _)| in_tree.contains(&(name.clone(), version.clone()))).filter_map(|(_, c)| c).collect())
}

fn output(mut cmd: std::process::Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: failed to spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("{what}: exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `cargo tree --format {p}`'s lines (`name vX.Y.Z [(…)] [(*)]`) as (name, version).
pub fn parse_tree(text: &str) -> std::collections::BTreeSet<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let (name, version) = (words.next()?, words.next()?.strip_prefix('v')?);
            Some((name.to_owned(), version.to_owned()))
        })
        .collect()
}

/// `cargo metadata`'s packages by (name, version), ordered: a third-party one as a [`Crate`], a workspace one (no
/// source) as `None`.
pub fn parse_metadata(json: &str) -> Result<BTreeMap<(String, String), Option<Crate>>, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("cargo metadata: {e}"))?;
    let packages = value.get("packages").and_then(|p| p.as_array()).ok_or("cargo metadata: no packages")?;
    let mut crates = BTreeMap::new();
    for package in packages {
        let text = |key: &str| package.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        let (Some(name), Some(version), Some(manifest)) = (text("name"), text("version"), text("manifest_path")) else {
            return Err("cargo metadata: a package without its name, version or manifest".into());
        };
        let third_party = package.get("source").is_some_and(|s| !s.is_null());
        let dir = Path::new(&manifest).parent().map(Path::to_owned).unwrap_or_default();
        let krate = third_party.then(|| Crate {
            name: name.clone(),
            version: version.clone(),
            license: text("license").unwrap_or_default(),
            dir,
            license_file: text("license_file"),
        });
        crates.insert((name, version), krate);
    }
    Ok(crates)
}

fn read(path: &Path) -> Result<String, String> {
    // The same bytes from every checkout (a Windows one may have CRLF).
    std::fs::read_to_string(path).map(|text| text.replace("\r\n", "\n")).map_err(|e| format!("{}: {e}", path.display()))
}

/// Whether a file at a crate's top is a licence, notice or copying file.
fn is_licence_file(name: &str) -> bool {
    let upper = name.to_uppercase();
    ["LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT", "UNLICENSE"].iter().any(|start| upper.starts_with(start))
}

/// The licence files of `krate`, relative to its folder, ordered.
fn licence_files(krate: &Crate) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(&krate.dir).map_err(|e| format!("{}: {e}", krate.dir.display()))?;
    let mut files: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| is_licence_file(name))
        .collect();
    files.extend(krate.license_file.iter().cloned());
    for (name, more) in MORE_FILES {
        if *name == krate.name {
            files.extend(more.iter().map(|f| (*f).to_owned()));
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Whether `license` can be taken as plain MIT or plain Apache-2.0 (one of its `OR` choices, with no `AND`).
fn plain_mit_or_apache(license: &str) -> bool {
    let spaced = license.replace(['(', ')'], " ").replace('/', " OR ");
    !spaced.split_whitespace().any(|t| t == "AND") && spaced.split(" OR ").map(str::trim).any(|choice| choice == "MIT" || choice == "Apache-2.0")
}

/// `LICENSES.txt` from Craft Cloud's `notice`, `mit` and `apache` and `crates`' files.
pub fn render(notice: &str, mit: &str, apache: &str, crates: &[Crate]) -> Result<String, String> {
    let rule = "=".repeat(80);
    let normal = |text: &str| text.replace("\r\n", "\n").trim_end().to_owned();
    let mut out = format!(
        "Craft Cloud: its licences, and those of every third-party crate compiled into it.\n\n{rule}\n{}\n\n{rule}\n{}\n\n{rule}\n{}\n",
        normal(notice),
        normal(mit),
        normal(apache)
    );
    out.push_str(&format!("\n{rule}\nThird-party crates, by name and version\n{rule}\n"));
    let mut sorted: Vec<&Crate> = crates.iter().collect();
    sorted.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    // Each text once: later crates with the same text name the first.
    let mut given: BTreeMap<String, String> = BTreeMap::new();
    let mut missing = Vec::new();
    for krate in sorted {
        let license = if krate.license.is_empty() { "(none given)" } else { krate.license.as_str() };
        out.push_str(&format!("\n--- {} {} ({license})\n", krate.name, krate.version));
        if krate.license.contains("MPL") {
            out.push_str(&format!("Used unmodified; its source: https://crates.io/crates/{}/{}\n", krate.name, krate.version));
        }
        let files = licence_files(krate)?;
        let same_as = SAME_TEXT_AS
            .iter()
            .find(|(name, _, _)| *name == krate.name)
            .and_then(|(_, from, why)| crates.iter().find(|c| c.name == *from).map(|c| (c, why)));
        if files.is_empty() {
            if let Some((from, why)) = same_as {
                out.push_str(&format!("It ships no licence file; its licence text is {} {}'s ({why}), given with it.\n", from.name, from.version));
            } else if plain_mit_or_apache(&krate.license) {
                out.push_str("It ships no licence file; it is used under the MIT or Apache-2.0 terms given at the top.\n");
            } else {
                missing.push(format!("{} {} ({license})", krate.name, krate.version));
            }
        }
        for file in files {
            let text = normal(&read(&krate.dir.join(&file))?);
            let here = format!("{} {} {file}", krate.name, krate.version);
            match given.get(&text) {
                Some(first) => out.push_str(&format!("\n{file}: the same text as {first}, above.\n")),
                None => {
                    out.push_str(&format!("\n{file}:\n\n{text}\n"));
                    given.insert(text, here);
                }
            }
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!("crates with no licence file, and not under plain MIT or Apache-2.0 (add their text):\n  {}", missing.join("\n  ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn krate(dir: &Path, name: &str, version: &str, license: &str, files: &[(&str, &str)]) -> Crate {
        let dir = dir.join(format!("{name}-{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        for (file, text) in files {
            std::fs::write(dir.join(file), text).unwrap();
        }
        Crate { name: name.into(), version: version.into(), license: license.into(), dir, license_file: None }
    }

    #[test]
    fn every_crate_with_its_files_in_order_each_text_once() {
        let tmp = tempfile::tempdir().unwrap();
        let crates = [
            krate(tmp.path(), "zeta", "1.0.0", "MIT", &[("LICENSE", "MIT\r\nCopyright Zeta\r\n")]),
            krate(tmp.path(), "alpha", "0.2.0", "MIT OR Apache-2.0", &[("LICENSE-MIT", "MIT\nCopyright Alpha"), ("LICENSE-APACHE", "Apache text")]),
            krate(tmp.path(), "beta", "1.0.0", "Apache-2.0", &[("LICENSE", "Apache text"), ("README.md", "not a licence")]),
            krate(tmp.path(), "bare", "3.0.0", "MIT/Apache-2.0", &[]),
            krate(tmp.path(), "im", "15.1.0", "MPL-2.0+", &[("LICENSE.md", "Mozilla Public License")]),
        ];
        let text = render("NOTICE\r\n", "Craft MIT", "Craft Apache", &crates).unwrap();
        let order = ["NOTICE", "Craft MIT", "Craft Apache", "--- alpha 0.2.0", "--- bare 3.0.0", "--- beta 1.0.0", "--- im 15.1.0", "--- zeta 1.0.0"];
        let at: Vec<usize> = order.iter().map(|part| text.find(part).unwrap_or_else(|| panic!("{part} missing:\n{text}"))).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{text}");
        assert!(text.contains("LICENSE: the same text as alpha 0.2.0 LICENSE-APACHE, above."), "{text}");
        assert!(!text.contains("not a licence"));
        assert!(text.contains("--- bare 3.0.0 (MIT/Apache-2.0)\nIt ships no licence file"), "{text}");
        assert!(text.contains("source: https://crates.io/crates/im/15.1.0"), "{text}");
        assert!(text.contains("MIT\nCopyright Zeta") && !text.contains('\r'), "one kind of line end");
    }

    #[test]
    fn a_crate_without_a_file_and_not_plain_mit_or_apache_fails() {
        let tmp = tempfile::tempdir().unwrap();
        for license in ["BSL-1.0", "MIT AND BSD-3-Clause", ""] {
            let crates = [krate(tmp.path(), "odd", "1.0.0", license, &[])];
            let e = render("", "", "", &crates).unwrap_err();
            assert!(e.contains("odd 1.0.0"), "{license}: {e}");
        }
        assert!(plain_mit_or_apache("(MIT OR Apache-2.0)") && plain_mit_or_apache("Apache-2.0 OR BSL-1.0"));
        // clipboard-win's text is error-code's, when that is in the programs too.
        let crates = [
            krate(tmp.path(), "clipboard-win", "5.4.1", "BSL-1.0", &[]),
            krate(tmp.path(), "error-code", "3.4.0", "BSL-1.0", &[("LICENSE", "Boost Software License")]),
        ];
        let text = render("", "", "", &crates).unwrap();
        assert!(text.contains("its licence text is error-code 3.4.0's"), "{text}");
        assert!(render("", "", "", &crates[..1]).is_err(), "without error-code");
    }

    #[test]
    fn cargos_outputs_are_read() {
        let tree = "xxhash-rust v0.8.19\nim v15.1.0 (*)\ncraft-cloud-agent v0.0.1 (/w/apps/craft-cloud-agent)\n";
        let names: Vec<String> = parse_tree(tree).into_iter().map(|(n, v)| format!("{n} {v}")).collect();
        assert_eq!(names, ["craft-cloud-agent 0.0.1", "im 15.1.0", "xxhash-rust 0.8.19"]);
        let json = r#"{"packages":[
            {"name":"im","version":"15.1.0","license":"MPL-2.0+","license_file":null,"source":"registry+https://github.com/rust-lang/crates.io-index","manifest_path":"/r/im-15.1.0/Cargo.toml"},
            {"name":"craft-cloud-agent","version":"0.0.1","license":"MIT OR Apache-2.0","source":null,"manifest_path":"/w/apps/craft-cloud-agent/Cargo.toml"}]}"#;
        let crates = parse_metadata(json).unwrap();
        assert_eq!(crates[&("craft-cloud-agent".into(), "0.0.1".into())], None, "the workspace's own");
        let im = crates[&("im".into(), "15.1.0".into())].clone().unwrap();
        assert_eq!((im.license.as_str(), im.dir.as_path()), ("MPL-2.0+", Path::new("/r/im-15.1.0")));
    }

    /// The real file, as `cargo xtask package` writes it: every crate the licence gate makes an exception for that is
    /// in the programs is in it, by name and version, with the texts its licence asks for.
    #[test]
    fn every_excepted_crate_in_the_programs_is_in_the_real_file() {
        let root = crate::root();
        let shipped = shipped(&root).unwrap();
        let dir = tempfile::tempdir().unwrap();
        write(&root, &dir.path().join(LICENSES)).unwrap();
        let text = std::fs::read_to_string(dir.path().join(LICENSES)).unwrap();
        let mut checked = 0;
        for name in crate::licenses::excepted_crates() {
            for krate in shipped.iter().filter(|c| c.name == name) {
                assert!(text.contains(&format!("--- {} {} (", krate.name, krate.version)), "{name} missing");
                checked += 1;
            }
        }
        assert!(checked >= 3, "the fonts, Loro's MPL crates and xxhash-rust are in the programs everywhere: {checked}");
        for part in ["UBUNTU FONT LICENCE", "SIL OPEN FONT LICENSE", "Boost Software License", "https://crates.io/crates/im/"] {
            assert!(text.contains(part), "{part} missing");
        }
    }
}
