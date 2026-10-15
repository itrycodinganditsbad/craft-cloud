//! `cargo xtask licenses`: every crate in the build (every target, normal, build and dev dependencies) must carry a
//! licence on the allow-list in `AGENTS.md`. Reads `cargo tree`'s output, so it needs nothing beyond cargo itself.

use std::collections::BTreeMap;

/// Licences a dependency may carry: the permissive ones `AGENTS.md` lists ("BSD" is the 2- and 3-clause forms;
/// "Unicode" both of Unicode's licences).
const ALLOWED: &[&str] = &["MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib", "Unicode-3.0", "Unicode-DFS-2016", "CC0-1.0"];

/// Licences allowed only for data, in exactly these crates (`AGENTS.md`: "CDLA-Permissive-2.0 (data only, e.g. root
/// certificates)").
const DATA_ONLY: &[(&str, &[&str])] = &[("CDLA-Permissive-2.0", &["webpki-root-certs", "webpki-roots"])];

/// Licence exceptions that only add permissions (`Apache-2.0 WITH LLVM-exception`).
const EXCEPTIONS: &[&str] = &["LLVM-exception"];

pub fn run() -> Result<(), String> {
    let mut cmd = crate::cargo();
    cmd.args(["tree", "--workspace", "--locked", "--target", "all", "-e", "normal,build,dev", "--prefix", "none", "--format", "{p}|{l}"]);
    let output = cmd.output().map_err(|e| format!("cargo tree: failed to spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo tree: exited with {}: {}", output.status, String::from_utf8_lossy(&output.stderr).trim()));
    }
    let crates = parse_tree(&String::from_utf8_lossy(&output.stdout));
    let refused: Vec<String> = crates
        .iter()
        .filter_map(|((name, version), licence)| match permitted(name, licence) {
            Ok(true) => None,
            Ok(false) => Some(format!("{name} {version}: {}", if licence.is_empty() { "no licence given" } else { licence })),
            Err(why) => Some(format!("{name} {version}: {licence} ({why})")),
        })
        .collect();
    if refused.is_empty() {
        println!("licences: all {} crates are on the allow-list", crates.len());
        Ok(())
    } else {
        Err(format!("licences off the allow-list (see AGENTS.md):\n  {}", refused.join("\n  ")))
    }
}

/// `cargo tree`'s `{p}|{l}` lines as (name, version) → licence, each crate once.
fn parse_tree(text: &str) -> BTreeMap<(String, String), String> {
    let mut crates = BTreeMap::new();
    for line in text.lines() {
        let Some((package, licence)) = line.split_once('|') else { continue };
        let mut words = package.split_whitespace();
        let (Some(name), Some(version)) = (words.next(), words.next()) else { continue };
        // A crate already printed above is marked `(*)`.
        let licence = licence.trim().trim_end_matches("(*)").trim();
        crates.insert((name.to_owned(), version.to_owned()), licence.to_owned());
    }
    crates
}

/// Whether the crate `name` may be used under `licence` (an SPDX expression; `/` counts as `OR`, as old manifests
/// write it): true if the licences it can be taken under include an allowed set. An error for an expression that
/// doesn't parse.
fn permitted(name: &str, licence: &str) -> Result<bool, String> {
    let spaced = licence.replace('(', " ( ").replace(')', " ) ").replace('/', " OR ");
    let tokens: Vec<&str> = spaced.split_whitespace().collect();
    if tokens.is_empty() {
        return Ok(false);
    }
    let mut parser = Parser { tokens, at: 0, name };
    let ok = parser.or()?;
    match parser.tokens.get(parser.at) {
        None => Ok(ok),
        Some(extra) => Err(format!("unexpected `{extra}`")),
    }
}

struct Parser<'a> {
    tokens: Vec<&'a str>,
    at: usize,
    name: &'a str,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.at).copied()
    }

    fn next(&mut self) -> Option<&'a str> {
        let token = self.tokens.get(self.at).copied();
        self.at += 1;
        token
    }

    /// `and (OR and)*`: any one will do.
    fn or(&mut self) -> Result<bool, String> {
        let mut ok = self.and()?;
        while self.peek() == Some("OR") {
            self.next();
            ok |= self.and()?;
        }
        Ok(ok)
    }

    /// `one (AND one)*`: all of them must be allowed.
    fn and(&mut self) -> Result<bool, String> {
        let mut ok = self.one()?;
        while self.peek() == Some("AND") {
            self.next();
            ok &= self.one()?;
        }
        Ok(ok)
    }

    /// `( or )` or a licence, maybe `WITH` an exception.
    fn one(&mut self) -> Result<bool, String> {
        match self.next() {
            Some("(") => {
                let ok = self.or()?;
                match self.next() {
                    Some(")") => Ok(ok),
                    _ => Err("a `(` isn't closed".into()),
                }
            }
            Some(id) if !matches!(id, ")" | "AND" | "OR" | "WITH") => {
                let ok = ALLOWED.contains(&id) || DATA_ONLY.iter().any(|(data, crates)| *data == id && crates.contains(&self.name));
                if self.peek() == Some("WITH") {
                    self.next();
                    let exception = self.next().ok_or("`WITH` names no exception")?;
                    return Ok(ok && EXCEPTIONS.contains(&exception));
                }
                Ok(ok)
            }
            other => Err(format!("expected a licence, found {}", other.map_or("the end".into(), |t| format!("`{t}`")))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions_are_judged_by_what_they_allow() {
        for ok in [
            "MIT",
            "MIT OR Apache-2.0",
            "MIT/Apache-2.0",
            "Apache-2.0 / MIT",
            "Unlicense OR MIT",
            "MIT OR Apache-2.0 OR LGPL-2.1-or-later",
            "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT",
            "CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception",
            "(MIT OR Apache-2.0) AND Unicode-3.0",
            "ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)",
            "Apache-2.0 OR BSL-1.0",
        ] {
            assert_eq!(permitted("some-crate", ok), Ok(true), "{ok}");
        }
        for refused in ["", "GPL-3.0-only", "MIT AND GPL-2.0", "MPL-2.0", "Unlicense", "Apache-2.0 WITH Some-exception", "BSL-1.0"] {
            assert_eq!(permitted("some-crate", refused), Ok(false), "{refused}");
        }
        // Data licences only for the crates that are data.
        assert_eq!(permitted("webpki-root-certs", "CDLA-Permissive-2.0"), Ok(true));
        assert_eq!(permitted("some-crate", "CDLA-Permissive-2.0"), Ok(false));
        for broken in ["MIT OR", "(MIT", "MIT)", "AND MIT", "MIT WITH"] {
            assert!(permitted("some-crate", broken).is_err(), "{broken}");
        }
    }

    #[test]
    fn tree_lines_give_each_crate_once() {
        let text = "a v1.0.0|MIT\nb v0.2.1 (proc-macro)|MIT OR Apache-2.0\na v1.0.0|MIT (*)\n\nc v3.0.0 (/path/to/c)|\n";
        let crates = parse_tree(text);
        assert_eq!(crates.len(), 3);
        assert_eq!(crates.get(&("a".into(), "v1.0.0".into())).map(String::as_str), Some("MIT"));
        assert_eq!(crates.get(&("b".into(), "v0.2.1".into())).map(String::as_str), Some("MIT OR Apache-2.0"));
        assert_eq!(crates.get(&("c".into(), "v3.0.0".into())).map(String::as_str), Some(""));
    }
}
