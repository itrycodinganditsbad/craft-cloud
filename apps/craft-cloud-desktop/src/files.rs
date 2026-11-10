//! The Files page's data: the recent files of every Craft app, as the agent keeps them for the account (each app's list is
//! shared by all the account's computers), merged into one list, and whether each file can be opened on this computer.
//!
//! The agent has no call that lists the apps that have recents; a list is asked for by the app's name. The app therefore
//! asks for the lists of [`APPS`], the suite's apps, one after the other. An entry carries no app and no "live" mark, so the
//! app it was asked under is kept with it, and the page can't say which files are being edited right now.
//!
//! A path in a list comes from the network (another computer wrote it), so it is never handed to the OS as it is:
//! [`resolve`] joins it to the synced folder part by part and refuses everything that could leave the folder or start a
//! program (only the kinds of file in [`OPENABLE`] are opened, by name), and [`place`] also checks on disk that the file is
//! there, still inside the folder after links are followed, and not marked as executable.

use std::path::{Path, PathBuf};

use craft_cloud_app::{ConflictCopy, ConflictsResponse, RecentEntry};

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
    /// It is here, but not a kind of file the page asks the OS to open: a program, a script, a bundle, a disk image, a file
    /// without an extension, or anything not in [`OPENABLE`]. It can still be shown in its folder.
    NotOpenable,
}

impl Place {
    /// Why the file can't be opened, for the button's hover text (`None` when it can).
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Place::Here => None,
            Place::NotYet => Some("Not on this computer yet"),
            Place::Outside => Some("Outside the synced folder"),
            Place::NotOpenable => Some("Open it from its folder"),
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

/// The file name extensions (lower case) the page asks the OS to open: the Craft suite's own formats (PhotoCraft `pcraft`,
/// FigCraft `figcraft`, VectorCraft `vcraft`, FilmCraft `fcproj`, EffectCraft `effectcraft`, and the Photoshop, OpenRaster,
/// Figma, SVG and Illustrator files they read), office documents without macros, PDF, text and markdown, images, audio, video,
/// fonts, 3D and CAD interchange, and the archives an opener only lists. Everything else (programs, scripts, shortcuts, disk
/// images, macro documents, files nobody here has heard of) is opened from its folder, never from the page.
const OPENABLE: &[&str] = &[
    // The Craft suite's formats and the files its apps read.
    "pcraft",
    "figcraft",
    "fig",
    "vcraft",
    "fcproj",
    "filmcraft",
    "effectcraft",
    "psd",
    "psb",
    "psdt",
    "ora",
    "svg",
    "ai",
    "eps",
    // Office documents, PDF, text.
    "docx",
    "odt",
    "rtf",
    "xlsx",
    "ods",
    "csv",
    "tsv",
    "pptx",
    "odp",
    "pdf",
    "txt",
    "md",
    "markdown",
    "log",
    "json",
    "yaml",
    "yml",
    "toml",
    // Images.
    "png",
    "jpg",
    "jpeg",
    "gif",
    "bmp",
    "webp",
    "tif",
    "tiff",
    "tga",
    "ico",
    "heic",
    "heif",
    "avif",
    "exr",
    "hdr",
    "qoi",
    "apng",
    "dng",
    // Audio.
    "wav",
    "mp3",
    "flac",
    "ogg",
    "opus",
    "m4a",
    "aac",
    "aif",
    "aiff",
    "mid",
    "midi",
    "wma",
    // Video.
    "mp4",
    "mov",
    "mkv",
    "webm",
    "avi",
    "m4v",
    "mpg",
    "mpeg",
    "ogv",
    "3gp",
    "wmv",
    "m2ts",
    // Fonts.
    "ttf",
    "otf",
    "ttc",
    "woff",
    "woff2",
    // 3D and CAD interchange.
    "obj",
    "fbx",
    "gltf",
    "glb",
    "stl",
    "ply",
    "3mf",
    "dae",
    "usd",
    "usda",
    "usdc",
    "usdz",
    "dxf",
    "dwg",
    "step",
    "stp",
    "iges",
    "igs",
    // Archives the OS only shows (never disk images such as iso, vhd or dmg).
    "zip",
    "7z",
    "rar",
    "tar",
    "gz",
    "tgz",
];

/// What a folder or file name ends in when the OS treats it as one program: opening it would start it.
const BUNDLE_SUFFIXES: [&str; 9] = [".app", ".pkg", ".mpkg", ".prefpane", ".workflow", ".saver", ".bundle", ".framework", ".plugin"];

/// Names Windows keeps for devices, with or without an extension.
fn reserved(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT")).is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
}

/// Whether the name `part` ends in a bundle suffix (any case).
fn is_bundle(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    BUNDLE_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// Whether the file name `name` is of a kind the page asks the OS to open: it has an extension, and it is in [`OPENABLE`].
fn is_openable(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, extension)| OPENABLE.contains(&extension.to_ascii_lowercase().as_str()))
}

/// `rel` (relative to the synced folder, `/`-separated) in `root`, looking at the text alone and not at what it names:
/// `Err(Place::Outside)` for a path that is empty, absolute, has a drive letter or stream (`:`), a backslash, a NUL, an empty,
/// `.` or `..` part, or a reserved Windows name; `Err(Place::NotOpenable)` for a part that is a bundle (`.app` and the like).
pub fn resolve_folder(root: &Path, rel: &str) -> Result<PathBuf, Place> {
    if rel.is_empty() || rel.contains(['\0', '\\', ':']) {
        return Err(Place::Outside);
    }
    let mut path = root.to_path_buf();
    let mut bundle = false;
    for part in rel.split('/') {
        if matches!(part, "" | "." | "..") || reserved(part) || part.ends_with([' ', '.']) {
            return Err(Place::Outside);
        }
        bundle |= is_bundle(part);
        path.push(part);
    }
    if bundle { Err(Place::NotOpenable) } else { Ok(path) }
}

/// The file `rel` (relative to the synced folder, `/`-separated) in `root`, looking at the text alone: what
/// [`resolve_folder`] refuses, and `Err(Place::NotOpenable)` for a name without an extension or with one that isn't in
/// [`OPENABLE`].
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, Place> {
    let path = resolve_folder(root, rel)?;
    if rel.rsplit('/').next().is_some_and(is_openable) { Ok(path) } else { Err(Place::NotOpenable) }
}

/// Whether the file `file` (as the disk has it) is marked as executable by anyone. Never on Windows, which has no such mark.
#[cfg(unix)]
fn is_executable(file: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_file: &Path) -> bool {
    false
}

/// The path without the `\\?\` that [`std::fs::canonicalize`] puts before a drive letter on Windows, which the file manager
/// doesn't take; any other path as it is.
fn plain(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

/// The file `rel` in the synced folder `root` (`None` while the agent doesn't say), checked on the disk as it is now:
/// [`resolve`] for the text, then the file must exist, with links followed still be inside the folder, and not be marked as
/// executable. `Ok` is the path with links followed, which is the one to open: opening what was checked leaves no gap for a
/// link to be swapped in. `Err` says why not.
pub fn check(root: Option<&Path>, rel: &str) -> Result<PathBuf, Place> {
    let Some(root) = root else { return Err(Place::NotYet) };
    let textual = resolve(root, rel);
    // A name that isn't openable is still looked for on the disk: "not here" and "not for opening" are different answers.
    let path = match &textual {
        Ok(path) => path.clone(),
        Err(Place::NotOpenable) => resolve_folder(root, rel)?,
        Err(place) => return Err(*place),
    };
    match (std::fs::canonicalize(root), std::fs::canonicalize(&path)) {
        (Ok(root), Ok(file)) if file.starts_with(&root) && file.is_file() => {
            if textual.is_err() || is_executable(&file) {
                Err(Place::NotOpenable)
            } else {
                Ok(plain(file))
            }
        }
        (Ok(root), Ok(file)) if !file.starts_with(&root) => Err(Place::Outside),
        _ => Err(Place::NotYet),
    }
}

/// Where the file `rel` is in relation to `root` (the synced folder here, `None` while the agent doesn't say), by [`check`].
pub fn place(root: Option<&Path>, rel: &str) -> Place {
    check(root, rel).map_or_else(|place| place, |_| Place::Here)
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

/// One conflict copy: what the agent lists, whether the copy can be opened on this computer, and whether the file it is a copy
/// of is here (`None` when the copy's name doesn't say which file that is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub copy: ConflictCopy,
    pub place: Place,
    pub original: Option<Place>,
}

impl ConflictFile {
    /// The copy's file name for the first line: the last part of its path.
    pub fn name(&self) -> &str {
        self.copy.path.rsplit('/').next().unwrap_or(&self.copy.path)
    }

    /// When the copy was made, as the agent knows it: the minute in its name (marked when it is UTC), or `None` when the name
    /// has none and the page falls back to the copy's modification time.
    pub fn made_at(&self) -> Option<String> {
        let at = self.copy.at.as_deref()?;
        Some(if self.copy.at_utc { format!("{at} UTC") } else { at.to_owned() })
    }
}

/// The conflict copies the agent listed, placed on this computer, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConflictList {
    pub files: Vec<ConflictFile>,
    /// There are more copies than the agent lists (it leaves out the oldest).
    pub truncated: bool,
}

/// The agent's conflict copies, each placed against the synced folder by `place` (see [`place`]).
pub fn conflicts(response: ConflictsResponse, place: impl Fn(&str) -> Place) -> ConflictList {
    let files = response
        .copies
        .into_iter()
        .map(|copy| ConflictFile { place: place(&copy.path), original: copy.original_path.as_deref().map(&place), copy })
        .collect();
    ConflictList { files, truncated: response.truncated }
}

/// What the Files page lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilesView {
    /// The shared recent files of every app.
    #[default]
    Recent,
    /// The conflict copies in the synced folder.
    Conflicts,
}

/// The conflict copies of the Files page: asked for with the recents, shown by their own segment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Conflicts {
    /// The list, once asked.
    pub list: Option<ConflictList>,
    pub loading: bool,
    /// Why the list couldn't be had.
    pub error: Option<String>,
    /// Which answer is the current one.
    pub ticket: u64,
    /// Why the last Open or Show original didn't work.
    pub open_error: Option<String>,
}

/// The Files page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Files {
    /// Which list is shown.
    pub view: FilesView,
    /// The conflict copies.
    pub conflicts: Conflicts,
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
    fn only_documents_are_opened() {
        let root = Path::new("/r");
        for allowed in [
            "notes.txt",
            "Letters/doc1.docx",
            "a/b/Photo.PNG",
            "art/cover.pcraft",
            "Art/Cover.PsD",
            "film/trailer.fcproj",
            "brand/logo.vcraft",
            "design.figcraft",
            "sheet.xlsx",
            "deck.PPTX",
            "book.pdf",
            "README.md",
            "song.Flac",
            "clip.MP4",
            "face.otf",
            "part.STL",
            "scene.glb",
            "scene.usdc",
            "bundle.ZIP",
            "archive.tar.gz",
            "a..b/c.txt",
        ] {
            assert!(resolve(root, allowed).is_ok(), "{allowed}");
        }
        for refused in [
            "setup.exe",
            "a/b/run.BAT",
            "x.ps1",
            "link.lnk",
            "tool.sh",
            "tool.desktop",
            "disk.iso",
            "disk.VHD",
            "installer.dmg",
            "macro.docm",
            "macro.xlsm",
            "page.html",
            "page.hta",
            "Makefile",
            "exe",
            ".bashrc",
            "dir/README",
            "file.unknown",
            "photo.png.exe",
            "weird.",
        ] {
            let expected = if refused.ends_with('.') { Place::Outside } else { Place::NotOpenable };
            assert_eq!(resolve(root, refused), Err(expected), "{refused}");
        }
    }

    #[test]
    fn a_bundle_is_never_opened_or_entered() {
        let root = Path::new("/r");
        for bundle in [
            "Tool.app",
            "Tool.APP/readme.txt",
            "a/Setup.pkg/x.txt",
            "a/Setup.mpkg",
            "Pane.prefPane/notes.txt",
            "Flow.workflow",
            "Screen.saver/doc.pdf",
            "Thing.bundle/doc.pdf",
            "Lib.framework/doc.pdf",
            "Thing.PLUGIN/doc.pdf",
        ] {
            assert_eq!(resolve(root, bundle), Err(Place::NotOpenable), "{bundle}");
            assert_eq!(resolve_folder(root, bundle), Err(Place::NotOpenable), "{bundle}");
        }
        assert!(resolve_folder(root, "a/Setup.pkg.d/x.txt").is_ok(), "only the end of a part counts");
        assert!(resolve_folder(root, "myapp").is_ok(), "app without a dot is a name");
    }

    #[test]
    fn a_folder_is_resolved_without_the_program_check() {
        let root = Path::new("/r");
        for folder in ["Letters", "Letters.exe", "a/b.sh", "photo.png", "x.iso", "no extension"] {
            assert_eq!(resolve_folder(root, folder), Ok(folder.split('/').fold(root.to_path_buf(), |p, part| p.join(part))), "{folder}");
        }
        for bad in ["", "..", "a/../b", "/etc", "C:/x", "a\\b", "nul"] {
            assert_eq!(resolve_folder(root, bad), Err(Place::Outside), "{bad:?}");
        }
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
        assert_eq!(place(Some(dir.path()), "setup.exe"), Place::NotYet, "not there, so not a question of opening");
        std::fs::write(dir.path().join("setup.exe"), b"x").unwrap();
        std::fs::write(dir.path().join("README"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("Tool.app")).unwrap();
        std::fs::write(dir.path().join("Tool.app/notes.txt"), b"x").unwrap();
        assert_eq!(place(Some(dir.path()), "setup.exe"), Place::NotOpenable);
        assert_eq!(place(Some(dir.path()), "README"), Place::NotOpenable, "no extension");
        assert_eq!(place(Some(dir.path()), "Tool.app/notes.txt"), Place::NotOpenable, "inside a bundle");
        assert_eq!(place(None, "Letters/doc1.docx"), Place::NotYet, "no synced folder known");
        assert_eq!(place(Some(&dir.path().join("missing")), "Letters/doc1.docx"), Place::NotYet);
    }

    #[test]
    fn what_is_checked_is_the_path_to_open() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let checked = check(Some(dir.path()), "a.txt").unwrap();
        assert!(checked.is_absolute() && checked.ends_with("a.txt") && checked.is_file(), "{checked:?}");
        assert!(!checked.to_string_lossy().starts_with(r"\\?\"), "no verbatim prefix for the file manager: {checked:?}");
        assert_eq!(check(Some(dir.path()), "b.txt"), Err(Place::NotYet));
        assert_eq!(check(None, "a.txt"), Err(Place::NotYet));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_with_an_exec_bit_is_not_opened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("script.txt");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(place(Some(dir.path()), "script.txt"), Place::Here);
        for mode in [0o700, 0o010, 0o001, 0o755] {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(place(Some(dir.path()), "script.txt"), Place::NotOpenable, "{mode:o}");
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(place(Some(dir.path()), "script.txt"), Place::Here);
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
