//! What the local API answers from the saved sync state alone: the conflict copies (`GET /v1/conflicts`) and the sizes
//! by folder and kind (`GET /v1/usage/folders`). Both read a [`SavedState`] the caller loaded (the state file is
//! replaced atomically, so reading it never waits for a pass) and touch neither the disk nor the server.

use std::collections::{BTreeMap, HashSet};

use craft_cloud_proto::Name;
use craft_cloud_proto::conflict::parse_conflict_name;
use craft_cloud_proto::local::{
    ConflictCopy, ConflictsResponse, FileKind, FolderUsage, FolderUsageResponse, KindUsage, MAX_CONFLICT_COPIES, MAX_USAGE_FOLDERS, OtherFolders,
};
use craft_cloud_sync::BaseKind;

use crate::account::SavedState;

/// The conflict copies among the synced files, newest first, at most [`MAX_CONFLICT_COPIES`].
///
/// A file is listed when its name is one the engine makes for a conflict copy ([`parse_conflict_name`]) and a file with
/// the original's name sits beside it, or when the last finished pass reported it as a conflict copy it made. A user's own
/// file can match the pattern and have a sibling of that name; the list then shows it. A copy whose original was
/// deleted or renamed since is missed unless the last pass made it. Folders are never listed.
pub fn conflicts(saved: &SavedState) -> ConflictsResponse {
    let state = &saved.state;
    let reported: HashSet<&str> = saved
        .last_pass
        .as_ref()
        .and_then(|last| last.summary.as_ref())
        .map(|s| s.conflict_copies.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let mut found: Vec<((i64, u32), ConflictCopy)> = Vec::new();
    for (id, entry) in state.base.iter() {
        let BaseKind::File { size, mtime_ms, .. } = &entry.kind else { continue };
        let (Some(path), Some(name)) = (state.base_path(*id), entry.name.as_ref()) else { continue };
        let text = path.to_string();
        let parsed = parse_conflict_name(name.as_str()).filter(|parsed| {
            let original = Name::new(&parsed.original).ok();
            let sibling = path.parent().zip(original).map(|(parent, original)| parent.join(&original));
            sibling.is_some_and(|sibling| sibling != path && state.base_has_path(&sibling))
        });
        if parsed.is_none() && !reported.contains(text.as_str()) {
            continue;
        }
        let from_name = parse_conflict_name(name.as_str());
        let shown = parsed.as_ref().or(from_name.as_ref().filter(|_| reported.contains(text.as_str())));
        let original_path = shown.and_then(|p| Name::new(&p.original).ok()).and_then(|o| path.parent().map(|parent| parent.join(&o).to_string()));
        let stamp = shown.map(|p| p.at);
        let key = (stamp.and_then(|at| at.naive_ms()).unwrap_or(*mtime_ms), shown.map_or(1, |p| p.counter));
        found.push((
            key,
            ConflictCopy {
                path: text,
                original_path,
                device: shown.map(|p| p.device.clone()),
                at: stamp.map(|at| format!("{:04}-{:02}-{:02} {:02}:{:02}", at.year, at.month, at.day, at.hour, at.minute)),
                at_utc: stamp.is_some_and(|at| at.utc),
                at_ms: stamp.and_then(|at| at.unix_ms()),
                size: *size,
                mtime_ms: *mtime_ms,
            },
        ));
    }
    // Newest first; the path settles ties so the order is the same on every call.
    found.sort_by(|(a, x), (b, y)| b.cmp(a).then_with(|| x.path.cmp(&y.path)));
    let truncated = found.len() > MAX_CONFLICT_COPIES;
    found.truncate(MAX_CONFLICT_COPIES);
    ConflictsResponse { copies: found.into_iter().map(|(_, copy)| copy).collect(), truncated }
}

/// The kind class of a file by its extension (case-insensitive); anything unlisted or without one is `Other`.
pub fn kind_of(name: &str) -> FileKind {
    let Some((stem, ext)) = name.rsplit_once('.') else { return FileKind::Other };
    if stem.is_empty() {
        return FileKind::Other;
    }
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "heif" | "avif" | "svg" | "psd" | "psb" | "ai" | "raw"
        | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf" | "rw2" | "exr" | "hdr" | "ico" | "xcf" | "kra" | "ora" => FileKind::Image,
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "mpg" | "mpeg" | "wmv" | "flv" | "mxf" | "prores" | "braw" | "r3d" => FileKind::Video,
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "oga" | "opus" | "aif" | "aiff" | "wma" | "mid" | "midi" => FileKind::Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "rtf" | "pages" | "numbers" | "key" | "epub" => {
            FileKind::Document
        }
        "obj" | "fbx" | "stl" | "gltf" | "glb" | "blend" | "3ds" | "dae" | "step" | "stp" | "iges" | "igs" | "usd" | "usdz" | "ply" | "3mf"
        | "dwg" | "dxf" => FileKind::Model3d,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" | "dmg" => FileKind::Archive,
        "txt" | "md" | "csv" | "tsv" | "json" | "toml" | "yaml" | "yml" | "xml" | "html" | "htm" | "css" | "js" | "ts" | "rs" | "py" | "c" | "h"
        | "cpp" | "hpp" | "cs" | "java" | "go" | "sh" | "ini" | "log" | "srt" | "tex" => FileKind::Code,
        _ => FileKind::Other,
    }
}

/// The logical bytes of the synced files, by top-level folder (the first component of the path; `""` for the files
/// directly in the sync root) and by kind. At most [`MAX_USAGE_FOLDERS`] folders are listed, the largest first (by bytes,
/// then name); the rest add up in `other`.
pub fn folder_usage(saved: &SavedState) -> FolderUsageResponse {
    let state = &saved.state;
    let mut folders: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut kinds: BTreeMap<usize, (u64, u64)> = BTreeMap::new();
    let (mut total_bytes, mut total_files) = (0u64, 0u64);
    for (id, entry) in state.base.iter() {
        let BaseKind::File { size, .. } = &entry.kind else { continue };
        let Some(path) = state.base_path(*id) else { continue };
        let Some(file_name) = path.file_name() else { continue };
        let top = if path.depth() > 1 { path.names().first().map(|n| n.as_str().to_owned()).unwrap_or_default() } else { String::new() };
        let add = |slot: &mut (u64, u64)| *slot = (slot.0.saturating_add(*size), slot.1.saturating_add(1));
        add(folders.entry(top).or_default());
        let position = FileKind::ALL.iter().position(|k| *k == kind_of(file_name.as_str())).unwrap_or(FileKind::ALL.len() - 1);
        add(kinds.entry(position).or_default());
        total_bytes = total_bytes.saturating_add(*size);
        total_files = total_files.saturating_add(1);
    }
    let mut folders: Vec<FolderUsage> = folders.into_iter().map(|(name, (bytes, files))| FolderUsage { name, bytes, files }).collect();
    folders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    let other = (folders.len() > MAX_USAGE_FOLDERS).then(|| {
        folders.split_off(MAX_USAGE_FOLDERS).into_iter().fold(OtherFolders::default(), |sum, f| OtherFolders {
            folders: sum.folders.saturating_add(1),
            bytes: sum.bytes.saturating_add(f.bytes),
            files: sum.files.saturating_add(f.files),
        })
    });
    let kinds = FileKind::ALL
        .iter()
        .enumerate()
        .map(|(position, kind)| {
            let (bytes, files) = kinds.get(&position).copied().unwrap_or_default();
            KindUsage { kind: *kind, bytes, files }
        })
        .collect();
    FolderUsageResponse { total_bytes, total_files, folders, other, kinds }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{LastPass, Outcome, PassSummary};
    use craft_cloud_proto::{FileHash, ManifestId, VersionId};
    use craft_cloud_sync::{BaseEntry, RelPath, State};

    fn rel(path: &str) -> RelPath {
        RelPath::from_names(path.split('/').filter_map(|n| Name::new(n).ok()).collect())
    }

    /// A saved state with these files (path, size) below one root; folders are made as needed.
    fn saved(files: &[(&str, u64)]) -> SavedState {
        let root = craft_cloud_proto::NodeId::generate();
        let mut state = State { root: Some(root), ..State::default() };
        let mut dirs: BTreeMap<RelPath, craft_cloud_proto::NodeId> = BTreeMap::new();
        dirs.insert(RelPath::root(), root);
        for (path, size) in files {
            let path = rel(path);
            let mut at = RelPath::root();
            let Some((last, parents)) = path.names().split_last() else { continue };
            for name in parents {
                let below = at.join(name);
                if !dirs.contains_key(&below) {
                    let id = craft_cloud_proto::NodeId::generate();
                    state
                        .base
                        .insert(id, BaseEntry { parent: dirs.get(&at).copied(), name: Some(name.clone()), kind: BaseKind::Dir { local_id: None } });
                    dirs.insert(below.clone(), id);
                }
                at = below;
            }
            state.base.insert(
                craft_cloud_proto::NodeId::generate(),
                BaseEntry {
                    parent: dirs.get(&at).copied(),
                    name: Some(last.clone()),
                    kind: BaseKind::File {
                        version: VersionId::generate(),
                        manifest: ManifestId::from_bytes([0; 32]),
                        size: *size,
                        mtime_ms: i64::try_from(*size).unwrap_or(0),
                        exec: false,
                        local_id: None,
                        hash: FileHash::from_bytes([0; 32]),
                        seen_ms: 0,
                    },
                },
            );
        }
        SavedState { schema: 1, state, ..SavedState::default() }
    }

    fn reported(saved: &mut SavedState, paths: &[&str]) {
        let summary = PassSummary { conflict_copies: paths.iter().map(|p| (*p).to_owned()).collect(), ..PassSummary::default() };
        saved.last_pass = Some(LastPass { finished_ms: 0, outcome: Outcome::Done, summary: Some(summary) });
    }

    #[test]
    fn a_copy_with_its_original_beside_it_is_listed_with_its_parts() {
        let copy = "Art/Shot (conflict, laptop, 2026-10-11 14-03Z, 2).psd";
        let saved = saved(&[("Art/Shot.psd", 10), (copy, 7), ("Art/notes.txt", 1)]);
        let list = conflicts(&saved);
        assert!(!list.truncated);
        assert_eq!(list.copies.len(), 1, "{list:?}");
        let found = &list.copies[0];
        assert_eq!(found.path, copy);
        assert_eq!(found.original_path.as_deref(), Some("Art/Shot.psd"));
        assert_eq!(found.device.as_deref(), Some("laptop"));
        assert_eq!(found.at.as_deref(), Some("2026-10-11 14:03"));
        assert_eq!((found.at_utc, found.at_ms, found.size), (true, Some(1_791_727_380_000), 7));
    }

    #[test]
    fn a_lookalike_without_an_original_is_left_out_unless_the_pass_reported_it() {
        let lookalike = "a (conflict, pc, 2026-10-11 14-03).txt";
        let mut saved = saved(&[(lookalike, 3), ("other (conflict, x, nonsense).txt", 1)]);
        assert!(conflicts(&saved).copies.is_empty());
        reported(&mut saved, &[lookalike, "other (conflict, x, nonsense).txt", "gone.txt"]);
        let list = conflicts(&saved);
        let paths: Vec<&str> = list.copies.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["a (conflict, pc, 2026-10-11 14-03).txt", "other (conflict, x, nonsense).txt"], "a path no longer synced is not listed");
        assert_eq!(list.copies[0].device.as_deref(), Some("pc"), "a reported copy keeps its parts when its name has them");
        assert_eq!((list.copies[1].device.clone(), list.copies[1].original_path.clone(), list.copies[1].at.clone()), (None, None, None));
    }

    #[test]
    fn the_original_must_be_a_sibling_in_the_same_folder() {
        let saved = saved(&[("A/a.txt", 1), ("B/a (conflict, pc, 2026-10-11 14-03).txt", 1)]);
        assert!(conflicts(&saved).copies.is_empty());
    }

    #[test]
    fn copies_come_newest_first() {
        let names = ["a (conflict, pc, 2026-10-11 14-03).txt", "a (conflict, pc, 2026-10-12 08-00).txt", "a (conflict, pc, 2026-10-11 14-03, 2).txt"];
        let mut files = vec![("a.txt", 1)];
        files.extend(names.iter().map(|n| (*n, 1)));
        let list = conflicts(&saved(&files));
        let order: Vec<&str> = list.copies.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(order, [names[1], names[2], names[0]]);
    }

    #[test]
    fn the_list_is_capped_with_a_flag() {
        let names: Vec<String> = (0..1005).map(|i| format!("a (conflict, pc, 2026-10-11 14-03, {}).txt", i + 2)).collect();
        let mut files: Vec<(&str, u64)> = vec![("a.txt", 1)];
        files.extend(names.iter().map(|n| (n.as_str(), 1)));
        let list = conflicts(&saved(&files));
        assert_eq!(list.copies.len(), MAX_CONFLICT_COPIES);
        assert!(list.truncated);
        let exact = conflicts(&saved(&files[..=MAX_CONFLICT_COPIES]));
        assert_eq!((exact.copies.len(), exact.truncated), (MAX_CONFLICT_COPIES, false));
    }

    #[test]
    fn extensions_pick_the_kind() {
        for (name, kind) in [
            ("a.JPG", FileKind::Image),
            ("a.mp4", FileKind::Video),
            ("a.flac", FileKind::Audio),
            ("a.pdf", FileKind::Document),
            ("a.glb", FileKind::Model3d),
            ("a.tar.gz", FileKind::Archive),
            ("a.rs", FileKind::Code),
            ("a.unknown", FileKind::Other),
            ("Makefile", FileKind::Other),
            (".txt", FileKind::Other),
        ] {
            assert_eq!(kind_of(name), kind, "{name}");
        }
    }

    #[test]
    fn folder_usage_sums_by_top_level_folder_and_kind() {
        let saved = saved(&[("top.txt", 5), ("Art/a.png", 100), ("Art/deep/b.png", 50), ("Art/c.mp4", 1000), ("Docs/d.pdf", 20), ("Docs/e", 3)]);
        let usage = folder_usage(&saved);
        assert_eq!((usage.total_bytes, usage.total_files), (1178, 6));
        let folders: Vec<(&str, u64, u64)> = usage.folders.iter().map(|f| (f.name.as_str(), f.bytes, f.files)).collect();
        assert_eq!(folders, [("Art", 1150, 3), ("Docs", 23, 2), ("", 5, 1)]);
        assert_eq!(usage.other, None);
        assert_eq!(usage.kinds.iter().map(|k| k.kind).collect::<Vec<_>>(), FileKind::ALL);
        let by = |kind| usage.kinds.iter().find(|k| k.kind == kind).map(|k| (k.bytes, k.files)).unwrap_or_default();
        assert_eq!(by(FileKind::Image), (150, 2));
        assert_eq!(by(FileKind::Video), (1000, 1));
        assert_eq!(by(FileKind::Document), (20, 1));
        assert_eq!(by(FileKind::Code), (5, 1));
        assert_eq!(by(FileKind::Other), (3, 1));
        assert_eq!(by(FileKind::Audio), (0, 0));
    }

    #[test]
    fn folder_usage_of_nothing_is_empty_but_lists_every_kind() {
        let usage = folder_usage(&SavedState::default());
        assert_eq!((usage.total_bytes, usage.total_files, usage.folders.len(), usage.other), (0, 0, 0, None));
        assert_eq!(usage.kinds.len(), FileKind::ALL.len());
    }

    #[test]
    fn folder_usage_lists_the_top_50_and_sums_the_rest() {
        let names: Vec<String> = (0..60).map(|i| format!("F{i:02}/x.bin")).collect();
        let files: Vec<(&str, u64)> = names.iter().enumerate().map(|(i, n)| (n.as_str(), 100 + i as u64)).collect();
        let usage = folder_usage(&saved(&files));
        assert_eq!(usage.folders.len(), MAX_USAGE_FOLDERS);
        assert_eq!(usage.folders.first().map(|f| f.name.as_str()), Some("F59"));
        let other = usage.other.expect("ten folders are left over");
        let left: u64 = (0..10).map(|i| 100 + i).sum();
        assert_eq!((other.folders, other.files, other.bytes), (10, 10, left));
        let listed: u64 = usage.folders.iter().map(|f| f.bytes).sum();
        assert_eq!(listed + other.bytes, usage.total_bytes);
    }
}
