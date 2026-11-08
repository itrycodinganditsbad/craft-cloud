//! The Files page's data: the recent files of every Craft app, as the agent keeps them for the account (each app's list is
//! shared by all the account's computers), merged into one list, and whether each file can be opened on this computer.
//!
//! The agent has no call that lists the apps that have recents; a list is asked for by the app's name. The app therefore
//! asks for the lists of [`APPS`], the suite's apps, one after the other. An entry carries no app and no "live" mark, so the
//! app it was asked under is kept with it, and the page can't say which files are being edited right now.
//!
//! A path in a list comes from the network (another computer wrote it), so it is never handed to the OS as it is:
//! [`resolve`] joins it to the synced folder part by part and refuses everything that could leave the folder or start a
//! program, and [`place`] also checks on disk that the file is there and still inside the folder after links are followed.

use std::path::{Path, PathBuf};

use craft_cloud_app::RecentEntry;

/// The Craft apps whose recent files the page asks for: the lower-case name an app uses for its list, and its name in the
/// suite.
pub const APPS: [(&str, &str); 12] = [
    ("photocraft", "PhotoCraft"),
    ("lightcraft", "LightCraft"),
    ("filmcraft", "FilmCraft"),
    ("designcraft", "DesignCraft"),
    ("vectorcraft", "VectorCraft"),
    ("pdfcraft", "PdfCraft"),
    ("effectcraft", "EffectCraft"),
    ("wordcraft", "WordCraft"),
    ("deckcraft", "DeckCraft"),
    ("gridcraft", "GridCraft"),
    ("cadcraft", "CADCraft"),
    ("soundcraft", "SoundCraft"),
];

/// The most files the page keeps (the newest of all the apps' lists).
pub const MAX_FILES: usize = 200;

/// The name an app is shown under: its name in the suite, or the name of its list for one the app doesn't know.
pub fn app_label(app: &str) -> &str {
    APPS.iter().find(|(id, _)| *id == app).map_or(app, |(_, label)| label)
}

/// Whether a recent file can be opened on this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// It is in the synced folder here.
    Here,
    /// It isn't on this computer (yet): it was made on another one and hasn't synced here, or it is gone.
    NotYet,
    /// Its path leaves the synced folder.
    Outside,
    /// It is a program or a script, which the page never starts.
    Program,
}

impl Place {
    /// Why the file can't be opened, for the button's hover text (`None` when it can).
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Place::Here => None,
            Place::NotYet => Some("Not on this computer yet"),
            Place::Outside => Some("Outside the synced folder"),
            Place::Program => Some("Programs and scripts aren't opened from here"),
        }
    }
}

/// One recent file: the app whose list it is in, the entry, and whether it can be opened here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentFile {
    pub app: String,
    pub entry: RecentEntry,
    pub place: Place,
}

impl RecentFile {
    /// The file's name for the first line: its title, or the last part of its path when the title is empty.
    pub fn name(&self) -> &str {
        let title = self.entry.title.trim();
        if title.is_empty() { self.entry.path.rsplit('/').next().unwrap_or(&self.entry.path) } else { title }
    }

    /// When it was last opened or saved here or on another computer, in milliseconds since the Unix epoch.
    pub fn when_ms(&self) -> i64 {
        self.entry.saved_ms.map_or(self.entry.opened_ms, |saved| saved.max(self.entry.opened_ms))
    }
}

/// File name extensions that run something when the OS opens them.
const PROGRAMS: [&str; 30] = [
    "exe", "com", "bat", "cmd", "msi", "scr", "pif", "cpl", "lnk", "url", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta", "jar",
    "reg", "sh", "command", "desktop", "app", "appimage", "run", "bin", "dll", "msc",
];

/// Names Windows keeps for devices, with or without an extension.
fn reserved(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT")).is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
}

/// The file `rel` (relative to the synced folder, `/`-separated) in `root`, looking at the text alone: `Err(Place::Outside)`
/// for a path that is empty, absolute, has a drive letter or stream (`:`), a backslash, a NUL, an empty, `.` or `..` part, or
/// a reserved Windows name; `Err(Place::Program)` for a program or script.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, Place> {
    if rel.is_empty() || rel.contains(['\0', '\\', ':']) {
        return Err(Place::Outside);
    }
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        if matches!(part, "" | "." | "..") || reserved(part) || part.ends_with([' ', '.']) {
            return Err(Place::Outside);
        }
        path.push(part);
    }
    let extension = rel.rsplit('/').next().and_then(|name| name.rsplit_once('.')).map(|(_, ext)| ext.to_ascii_lowercase());
    if extension.is_some_and(|ext| PROGRAMS.contains(&ext.as_str())) {
        return Err(Place::Program);
    }
    Ok(path)
}

/// Where the file `rel` is in relation to `root` (the synced folder here, `None` while the agent doesn't say): [`resolve`],
/// then the disk: the file must exist and, with links followed, still be inside the folder.
pub fn place(root: Option<&Path>, rel: &str) -> Place {
    let Some(root) = root else { return Place::NotYet };
    let path = match resolve(root, rel) {
        Ok(path) => path,
        Err(place) => return place,
    };
    match (std::fs::canonicalize(root), std::fs::canonicalize(&path)) {
        (Ok(root), Ok(file)) if file.starts_with(&root) && file.is_file() => Place::Here,
        (Ok(root), Ok(file)) if !file.starts_with(&root) => Place::Outside,
        _ => Place::NotYet,
    }
}

/// The lists the agent answered, one result per app, as one list: newest first, at most [`MAX_FILES`], each file placed on
/// this computer by `place`. `Err` only when no list could be had at all (the first error is given).
pub fn merge(
    lists: Vec<(String, Result<Vec<RecentEntry>, craft_cloud_app::Error>)>,
    place: impl Fn(&str) -> Place,
) -> Result<Vec<RecentFile>, craft_cloud_app::Error> {
    let mut files = Vec::new();
    let mut first_error = None;
    let mut answered = false;
    for (app, list) in lists {
        match list {
            Ok(entries) => {
                answered = true;
                files.extend(entries.into_iter().map(|entry| RecentFile { place: place(&entry.path), app: app.clone(), entry }));
            }
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) if !answered => Err(e),
        _ => {
            files.sort_by_key(|f| std::cmp::Reverse(f.when_ms()));
            files.truncate(MAX_FILES);
            Ok(files)
        }
    }
}

/// The Files page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Files {
    /// The merged list, once asked.
    pub list: Option<Vec<RecentFile>>,
    pub loading: bool,
    /// Why the list couldn't be had.
    pub error: Option<String>,
    /// Why the last Open didn't work.
    pub open_error: Option<String>,
    /// Which answer is the current one.
    pub ticket: u64,
    /// The app whose files are shown (`None`: all of them).
    pub app: Option<String>,
    /// What the name or the path of a shown file contains (any case).
    pub search: String,
}

impl Files {
    /// The files that pass the app filter and the search.
    pub fn visible(&self) -> Vec<&RecentFile> {
        let needle = self.search.trim().to_lowercase();
        self.list
            .iter()
            .flatten()
            .filter(|f| self.app.as_deref().is_none_or(|app| f.app == app))
            .filter(|f| needle.is_empty() || f.name().to_lowercase().contains(&needle) || f.entry.path.to_lowercase().contains(&needle))
            .collect()
    }

    /// How many files there are in all, and how many of each app that has some (in the suite's order, then the others).
    pub fn counts(&self) -> (usize, Vec<(String, usize)>) {
        let files = self.list.as_deref().unwrap_or_default();
        let mut apps: Vec<(String, usize)> = Vec::new();
        for file in files {
            match apps.iter_mut().find(|(app, _)| *app == file.app) {
                Some((_, n)) => *n += 1,
                None => apps.push((file.app.clone(), 1)),
            }
        }
        let rank = |app: &str| APPS.iter().position(|(id, _)| *id == app).unwrap_or(APPS.len());
        apps.sort_by_key(|(app, _)| rank(app));
        (files.len(), apps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, title: &str, opened_ms: i64) -> RecentEntry {
        RecentEntry { node: None, path: path.into(), title: title.into(), opened_ms, saved_ms: None, machine: "BEAST".into() }
    }

    #[test]
    fn a_path_inside_the_folder_is_joined_to_it() {
        let root = Path::new("/home/fabio/Craft Cloud");
        assert_eq!(resolve(root, "Letters/doc1.docx"), Ok(root.join("Letters").join("doc1.docx")));
        assert_eq!(resolve(root, "a..b/c.txt"), Ok(root.join("a..b").join("c.txt")));
    }

    #[test]
    fn a_path_that_could_leave_the_folder_is_refused() {
        let root = Path::new("/home/fabio/Craft Cloud");
        for bad in [
            "",
            "..",
            "../x.txt",
            "a/../../x.txt",
            "a/./b.txt",
            "a//b.txt",
            "/etc/passwd",
            "C:/Windows/x.txt",
            "C:x.txt",
            "a\\b.txt",
            "..\\x.txt",
            "a/b.txt:stream",
            "a\0b.txt",
            "nul",
            "dir/COM1.txt",
            "dir/trailing.",
            "dir/space ",
            "a/",
        ] {
            assert_eq!(resolve(root, bad), Err(Place::Outside), "{bad:?}");
        }
    }

    #[test]
    fn programs_and_scripts_are_not_opened() {
        let root = Path::new("/r");
        for program in ["setup.exe", "a/b/run.BAT", "x.ps1", "link.lnk", "tool.sh", "App.app"] {
            assert_eq!(resolve(root, program), Err(Place::Program), "{program}");
        }
        assert!(resolve(root, "notes.txt").is_ok());
        assert!(resolve(root, "exe").is_ok(), "a file called exe has no extension");
    }

    #[test]
    fn a_file_is_here_only_when_it_exists_in_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Letters")).unwrap();
        std::fs::write(dir.path().join("Letters/doc1.docx"), b"x").unwrap();
        assert_eq!(place(Some(dir.path()), "Letters/doc1.docx"), Place::Here);
        assert_eq!(place(Some(dir.path()), "Letters/other.docx"), Place::NotYet);
        assert_eq!(place(Some(dir.path()), "Letters"), Place::NotYet, "a folder is not a file");
        assert_eq!(place(Some(dir.path()), "../x.docx"), Place::Outside);
        assert_eq!(place(Some(dir.path()), "setup.exe"), Place::Program);
        assert_eq!(place(None, "Letters/doc1.docx"), Place::NotYet, "no synced folder known");
        assert_eq!(place(Some(&dir.path().join("missing")), "Letters/doc1.docx"), Place::NotYet);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_folder_is_outside() {
        let (inside, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(outside.path().join("secret.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), inside.path().join("link")).unwrap();
        assert_eq!(place(Some(inside.path()), "link/secret.txt"), Place::Outside);
    }

    #[test]
    fn lists_are_merged_newest_first_and_placed() {
        let lists = vec![
            ("wordcraft".to_owned(), Ok(vec![entry("a.docx", "Letter", 10), entry("../b.docx", "", 30)])),
            ("photocraft".to_owned(), Ok(vec![entry("p.png", "Sunset", 20)])),
        ];
        let files = merge(lists, |path| if path.starts_with("..") { Place::Outside } else { Place::Here }).unwrap();
        let order: Vec<(&str, &str)> = files.iter().map(|f| (f.app.as_str(), f.name())).collect();
        assert_eq!(order, [("wordcraft", "b.docx"), ("photocraft", "Sunset"), ("wordcraft", "Letter")]);
        assert_eq!(files.first().map(|f| f.place), Some(Place::Outside));
    }

    #[test]
    fn a_saved_time_counts_when_it_is_newer() {
        let mut file = RecentFile { app: "a".into(), entry: entry("a", "a", 10), place: Place::Here };
        file.entry.saved_ms = Some(50);
        assert_eq!(file.when_ms(), 50);
        file.entry.saved_ms = Some(5);
        assert_eq!(file.when_ms(), 10);
    }

    #[test]
    fn one_list_that_fails_is_skipped_and_all_failing_is_an_error() {
        let refused = || craft_cloud_app::Error::Refused { code: "quota_exceeded".into(), message: "full".into(), name_taken: None };
        let some = vec![("a".to_owned(), Err(refused())), ("b".to_owned(), Ok(vec![entry("x.txt", "x", 1)]))];
        assert_eq!(merge(some, |_| Place::Here).unwrap().len(), 1);
        let none = vec![("a".to_owned(), Err(refused())), ("b".to_owned(), Err(craft_cloud_app::Error::NotRunning("gone".into())))];
        assert!(matches!(merge(none, |_| Place::Here), Err(craft_cloud_app::Error::Refused { .. })));
        assert_eq!(merge(Vec::new(), |_| Place::Here).unwrap(), Vec::new());
    }

    #[test]
    fn the_list_is_cut_at_the_most_files() {
        let many = (0..MAX_FILES + 50).map(|n| entry(&format!("f{n}.txt"), "f", i64::try_from(n).unwrap_or(0))).collect();
        let files = merge(vec![("a".to_owned(), Ok(many))], |_| Place::Here).unwrap();
        assert_eq!(files.len(), MAX_FILES);
        assert_eq!(files.first().map(|f| f.entry.opened_ms), Some(i64::try_from(MAX_FILES + 49).unwrap_or(0)));
    }

    #[test]
    fn counts_and_filters_follow_the_apps_and_the_search() {
        let file = |app: &str, path: &str, title: &str| RecentFile { app: app.into(), entry: entry(path, title, 1), place: Place::Here };
        let mut files = Files {
            list: Some(vec![
                file("wordcraft", "Letters/a.docx", "Letter"),
                file("photocraft", "Art/p.png", "Sunset"),
                file("wordcraft", "b.docx", "Memo"),
            ]),
            ..Files::default()
        };
        assert_eq!(files.counts(), (3, vec![("photocraft".into(), 1), ("wordcraft".into(), 2)]));
        assert_eq!(files.visible().len(), 3);
        files.app = Some("wordcraft".into());
        assert_eq!(files.visible().len(), 2);
        files.search = " LETTERS ".into();
        assert_eq!(files.visible().len(), 1, "the path counts, in any case");
        files.search = "sunset".into();
        assert!(files.visible().is_empty(), "the app filter stays");
        files.app = None;
        assert_eq!(files.visible().len(), 1);
    }

    #[test]
    fn apps_are_named_as_in_the_suite() {
        assert_eq!(app_label("wordcraft"), "WordCraft");
        assert_eq!(app_label("unknown"), "unknown");
    }
}
