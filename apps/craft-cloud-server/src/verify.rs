//! The invariant checker behind `craft-cloud-server verify`, and the index builder behind `craft-cloud-server index
//! build`.
//!
//! `verify` reads and reports; it changes nothing: the database and the storage must exist already (a wrong path is an
//! error, not a new empty server with no problems), and a database whose schema is older is refused, not upgraded. It
//! checks what a later garbage collector would have to trust: that every manifest's list of
//! chunks in the database is complete and equal to the manifest object in the store, that every chunk a manifest names
//! is recorded and its object is there and intact, and that nothing in the database names a manifest or a chunk that does
//! not exist. Anything it cannot establish is reported, not guessed: a manifest it cannot read or whose index is not
//! complete has unknown dependencies, and unknown means kept.
//!
//! Two kinds of finding. *Problems* are inconsistencies and make `verify` fail. *Notices* are facts a reader needs but
//! that are not wrong by themselves: manifests whose index is not complete yet (`index build` completes it), manifests
//! no version or snapshot names (a live document's assets are named only inside the document, where the server cannot
//! read them, so these are not garbage and must not be treated as garbage), and chunks no complete manifest names.

use std::collections::BTreeMap;

use craft_cloud_meta::{EdgeCheck, Finding, IndexAudit, IndexState, Meta, MetaError};
use craft_cloud_proto::{BlobKind, ChunkId, Manifest, ManifestId};
use craft_cloud_store::{BlobKey, Store, StoreError};
use serde_json::{Value, json};

/// Problems listed in full in a report; further ones are only counted.
const MAX_LISTED_PROBLEMS: usize = 1000;
/// Examples of each audit finding.
const EXAMPLES: usize = 20;
/// IDs one metadata query covers.
const PAGE: usize = 200;

/// What `verify` should look at.
#[derive(Debug, Clone, Copy)]
pub struct VerifyOptions {
    /// Read every chunk object from the store, which checks its hash. Without it only manifests are read and chunks are
    /// checked against the database.
    pub read_chunks: bool,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self { read_chunks: true }
    }
}

/// One inconsistency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub kind: &'static str,
    /// The IDs involved in hex (`manifest` or `manifest/chunk`).
    pub subject: String,
    pub detail: Option<String>,
}

/// The result of a `verify` run.
#[derive(Debug, Clone)]
pub struct Report {
    pub audit: IndexAudit,
    /// The first [`MAX_LISTED_PROBLEMS`] problems.
    pub problems: Vec<Problem>,
    /// Every problem, by kind.
    pub counts: BTreeMap<&'static str, u64>,
    pub manifests_checked: u64,
    pub chunks_checked: u64,
    pub chunk_objects_read: bool,
}

impl Report {
    /// How many problems there are in all.
    pub fn problem_count(&self) -> u64 {
        self.counts.values().sum()
    }

    /// Whether the index covers every recorded manifest, completely.
    pub fn index_complete(&self) -> bool {
        self.audit.manifests == self.audit.complete
    }

    fn add(&mut self, kind: &'static str, subject: String, detail: Option<String>) {
        *self.counts.entry(kind).or_insert(0) += 1;
        if self.problems.len() < MAX_LISTED_PROBLEMS {
            self.problems.push(Problem { kind, subject, detail });
        }
    }

    /// An audit finding that is a problem: counted in full, its examples listed.
    fn add_finding(&mut self, kind: &'static str, finding: &Finding) {
        if finding.count == 0 {
            return;
        }
        *self.counts.entry(kind).or_insert(0) += finding.count;
        for example in &finding.examples {
            if self.problems.len() < MAX_LISTED_PROBLEMS {
                self.problems.push(Problem { kind, subject: example.clone(), detail: None });
            }
        }
    }

    /// The report as JSON.
    pub fn to_json(&self) -> Value {
        let notice = |f: &Finding| json!({ "count": f.count, "examples": f.examples });
        json!({
            "ok": self.problem_count() == 0,
            "index": {
                "manifests": self.audit.manifests,
                "complete": self.audit.complete,
                "building": self.audit.building,
                "absent": self.audit.absent,
                "all_complete": self.index_complete(),
            },
            "problems": {
                "total": self.problem_count(),
                "by_kind": self.counts,
                "listed": self.problems.iter().map(|p| json!({ "kind": p.kind, "id": p.subject, "detail": p.detail })).collect::<Vec<_>>(),
                "truncated": self.problem_count() > u64::try_from(self.problems.len()).unwrap_or(u64::MAX),
            },
            "notices": {
                "manifests_without_complete_index": self.audit.absent.saturating_add(self.audit.building),
                "roots_not_indexed": notice(&self.audit.roots_not_indexed),
                "unrooted_manifests": notice(&self.audit.unrooted_manifests),
                "unreferenced_chunks": notice(&self.audit.unreferenced_chunks),
            },
            "checked": {
                "manifests": self.manifests_checked,
                "chunks": self.chunks_checked,
                "chunk_objects_read": self.chunk_objects_read,
            },
        })
    }

    /// The report for people.
    pub fn to_text(&self) -> String {
        let a = &self.audit;
        let mut out = format!(
            "manifests: {} recorded, index complete for {}, being built for {}, not indexed for {}\nchecked {} manifest(s) and {} chunk(s){}\n",
            a.manifests,
            a.complete,
            a.building,
            a.absent,
            self.manifests_checked,
            self.chunks_checked,
            if self.chunk_objects_read { " (chunk objects read and hashed)" } else { " (chunk objects not read)" },
        );
        if self.problem_count() == 0 {
            out.push_str("no problems found\n");
        } else {
            out.push_str(&format!("{} problem(s) found:\n", self.problem_count()));
            for (kind, count) in &self.counts {
                out.push_str(&format!("  {kind}: {count}\n"));
            }
            for p in &self.problems {
                match &p.detail {
                    Some(detail) => out.push_str(&format!("    {} {} ({detail})\n", p.kind, p.subject)),
                    None => out.push_str(&format!("    {} {}\n", p.kind, p.subject)),
                }
            }
            if self.problem_count() > u64::try_from(self.problems.len()).unwrap_or(u64::MAX) {
                out.push_str("    (more problems than are listed; use --json for counts by kind)\n");
            }
        }
        if !self.index_complete() {
            out.push_str("notice: the index is not complete; `craft-cloud-server index build` completes it. Manifests without a complete index have unknown dependencies.\n");
        }
        for (what, finding) in [
            (
                "manifests named by no version or snapshot (live-document assets are named only inside their documents, so these are not garbage)",
                &a.unrooted_manifests,
            ),
            ("recorded chunks no complete manifest names", &a.unreferenced_chunks),
        ] {
            if finding.count > 0 {
                out.push_str(&format!("notice: {} {what}\n", finding.count));
            }
        }
        out
    }
}

/// Why a manifest or chunk object could not be read.
fn store_problem(error: &StoreError) -> (&'static str, Option<String>) {
    match error {
        StoreError::NotFound(_) => ("object_missing", None),
        StoreError::HashMismatch(_) | StoreError::Corrupt { .. } | StoreError::TooLarge { .. } => ("object_corrupt", Some(error.to_string())),
        StoreError::Io(_) | StoreError::Backend(_) => ("store_error", Some(error.to_string())),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads a manifest object and decodes it; `Err` says what is wrong with it.
pub async fn read_manifest(store: &Store, id: &ManifestId) -> Result<Manifest, (&'static str, Option<String>)> {
    read_manifest_sized(store, id).await.map(|(manifest, _)| manifest)
}

/// Like [`read_manifest`], with the bytes the object is stored as (the format byte included).
async fn read_manifest_sized(store: &Store, id: &ManifestId) -> Result<(Manifest, u64), (&'static str, Option<String>)> {
    let (bytes, stored) = store.get_sized(BlobKey::manifest(id)).await.map_err(|e| {
        let (kind, detail) = store_problem(&e);
        (
            if kind == "object_missing" {
                "manifest_object_missing"
            } else if kind == "object_corrupt" {
                "manifest_object_corrupt"
            } else {
                "store_error"
            },
            detail,
        )
    })?;
    let manifest = Manifest::decode(&bytes).map_err(|e| ("manifest_not_decodable", Some(e.to_string())))?;
    if manifest.id() != *id {
        return Err(("manifest_id_mismatch", None));
    }
    Ok((manifest, stored))
}

/// Checks the database and the store against each other and reports. Changes nothing.
pub async fn verify(meta: &Meta, store: &Store, options: VerifyOptions) -> Result<Report, MetaError> {
    let audit = meta.index_audit(EXAMPLES).await?;
    let mut report = Report {
        audit: audit.clone(),
        problems: Vec::new(),
        counts: BTreeMap::new(),
        manifests_checked: 0,
        chunks_checked: 0,
        chunk_objects_read: options.read_chunks,
    };
    report.add_finding("index_without_manifest", &audit.index_without_manifest);
    report.add_finding("orphan_edges", &audit.edges_without_index);
    report.add_finding("index_wrong_count", &audit.complete_with_wrong_count);
    report.add_finding("unknown_dependency", &audit.unknown_dependencies);
    report.add_finding("root_manifest_unknown", &audit.roots_without_manifest);
    report.add_finding("space_blob_without_blob", &audit.space_blobs_without_blob);

    // Every recorded manifest: its object, its index, the chunks it names.
    let mut after: Option<[u8; 32]> = None;
    loop {
        let records = meta.blob_records_after(BlobKind::Manifest, after, PAGE).await?;
        let Some(last) = records.last().map(|r| r.id) else { break };
        after = Some(last);
        for record in records {
            let raw = record.id;
            report.manifests_checked += 1;
            let id = ManifestId::from_bytes(raw);
            let (manifest, stored) = match read_manifest_sized(store, &id).await {
                Ok(read) => read,
                Err((kind, detail)) => {
                    report.add(kind, hex(&raw), detail);
                    continue;
                }
            };
            // What the database records of the manifest is what the object is: the bytes it is stored as, and the size
            // of the file it describes.
            if record.stored_size != stored {
                report.add(
                    "manifest_stored_size_mismatch",
                    hex(&raw),
                    Some(format!("the database records {} bytes, the object is {stored}", record.stored_size)),
                );
            }
            if record.file_size != Some(manifest.size()) {
                let recorded = record.file_size.map_or_else(|| "none".to_owned(), |n| n.to_string());
                report.add(
                    "manifest_file_size_mismatch",
                    hex(&raw),
                    Some(format!("the database records a file of {recorded} bytes, the manifest says {}", manifest.size())),
                );
            }
            if let IndexState::Complete { .. } = meta.index_state(&id).await?
                && let EdgeCheck::Mismatch(problem) = meta.index_check(&manifest).await?
            {
                report.add("index_mismatch", hex(&raw), Some(format!("ordinal {}: {}", problem.ordinal, problem.kind.as_str())));
            }
            for refs in manifest.chunks().chunks(500) {
                let part: Vec<ChunkId> = refs.iter().map(|c| c.id).collect();
                let recorded = meta.recorded_chunk_sizes(&part).await?;
                for chunk in &part {
                    if !recorded.contains_key(chunk) {
                        report.add("chunk_not_recorded", format!("{}/{}", hex(&raw), hex(chunk.as_bytes())), None);
                    }
                }
                // The manifest says how long each chunk is; the database records the bytes it is stored as, one more
                // than that (the format byte). With the chunk objects read below, this ties the length the manifest
                // names to the bytes in the store.
                for chunk in refs {
                    if let Some(stored) = recorded.get(&chunk.id)
                        && *stored != u64::from(chunk.len).saturating_add(1)
                    {
                        report.add(
                            "chunk_length_mismatch",
                            format!("{}/{}", hex(&raw), hex(chunk.id.as_bytes())),
                            Some(format!("the manifest says {} bytes, the database records {stored} stored", chunk.len)),
                        );
                    }
                }
            }
        }
    }

    // Every recorded chunk's object (it also finds chunks no manifest names, which a manifest walk would not).
    let mut after: Option<[u8; 32]> = None;
    loop {
        let records = meta.blob_records_after(BlobKind::Chunk, after, PAGE).await?;
        let Some(last) = records.last().map(|r| r.id) else { break };
        after = Some(last);
        for record in records {
            let raw = record.id;
            report.chunks_checked += 1;
            if !options.read_chunks {
                continue;
            }
            match store.get_sized(BlobKey::chunk(&ChunkId::from_bytes(raw))).await {
                // Read, and its hash is right: its length is what the database records.
                Ok((_, stored)) => {
                    if stored != record.stored_size {
                        report.add(
                            "chunk_stored_size_mismatch",
                            hex(&raw),
                            Some(format!("the database records {} bytes, the object is {stored}", record.stored_size)),
                        );
                    }
                }
                Err(e) => {
                    let (kind, detail) = store_problem(&e);
                    let kind = match kind {
                        "object_missing" => "chunk_object_missing",
                        "object_corrupt" => "chunk_object_corrupt",
                        other => other,
                    };
                    report.add(kind, hex(&raw), detail);
                }
            }
        }
    }
    Ok(report)
}

/// What an `index build` run did.
#[derive(Debug, Clone, Default)]
pub struct BuildReport {
    /// Manifests whose index is complete after the run that were not before.
    pub indexed: u64,
    /// Manifests that could not be indexed: `(manifest, why)`. Their index stays incomplete.
    pub unreadable: Vec<(String, &'static str)>,
}

/// Completes the index of every recorded manifest whose index is not complete, in batches of at most the index budget
/// per transaction, so it can run beside the server and be stopped and run again. A manifest is read from the store and
/// checked against its ID first; one that cannot be read is reported and left unindexed (its chunks stay unknown). Only
/// the derived index is written.
pub async fn build_index(meta: &Meta, store: &Store) -> Result<BuildReport, MetaError> {
    let mut report = BuildReport::default();
    let mut after: Option<ManifestId> = None;
    loop {
        let ids = meta.manifests_to_index(after, PAGE).await?;
        let Some(last) = ids.last().copied() else { break };
        after = Some(last);
        for id in ids {
            match read_manifest(store, &id).await {
                Ok(manifest) => {
                    meta.index_complete(&manifest).await?;
                    report.indexed += 1;
                }
                Err((kind, _)) => report.unreadable.push((hex(id.as_bytes()), kind)),
            }
        }
    }
    Ok(report)
}
