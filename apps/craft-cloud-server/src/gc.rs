//! Read-only store inventory and garbage-collection preview.

use craft_cloud_meta::{BlobKind, Meta, MetaError};
use craft_cloud_store::{ListCursor, MAX_LIST_PAGE_SIZE, Store};
use serde_json::{Value, json};

use crate::verify::{self, VerifyOptions};

const EXAMPLES: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error(transparent)]
    Meta(#[from] MetaError),
}

#[derive(Default)]
struct Class {
    count: u64,
    bytes: u64,
    examples: Vec<String>,
}

/// Objects the database records: how many and their stored bytes as `blobs.stored_size` has them (no object is read).
#[derive(Default)]
struct Recorded {
    count: u64,
    bytes: u64,
}

pub struct Report {
    chunks_recorded: Recorded,
    manifests_recorded: Recorded,
    chunks_orphans: Class,
    manifests_orphans: Class,
    foreign: Class,
    inventory_error: Option<String>,
    db_check: verify::Report,
    incomplete: bool,
    chunk_count: u64,
    chunk_candidates: u64,
    chunk_candidate_bytes: u64,
    chunk_protected: u64,
    chunk_examples: Vec<String>,
}

fn add(class: &mut Class, id: String, bytes: u64) {
    class.count = class.count.saturating_add(1);
    class.bytes = class.bytes.saturating_add(bytes);
    if class.examples.len() < EXAMPLES {
        class.examples.push(id);
    }
}

pub async fn dry_run(meta: &Meta, store: &Store) -> Result<Report, GcError> {
    let audit = meta.index_audit(EXAMPLES).await?;
    let incomplete = audit.absent > 0 || audit.building > 0;
    let mut chunks_recorded = Recorded::default();
    let mut manifests_recorded = Recorded::default();
    let mut chunks_orphans = Class::default();
    let mut manifests_orphans = Class::default();
    let mut foreign = Class::default();
    let mut inventory_error = None;
    for (kind, label) in [(BlobKind::Chunk, "chunks"), (BlobKind::Manifest, "manifests")] {
        let mut cursor: Option<ListCursor> = None;
        let mut page_no = 1u64;
        loop {
            let page = match store.list(kind, cursor.clone(), MAX_LIST_PAGE_SIZE).await {
                Ok(p) => p,
                Err(source) => {
                    inventory_error = Some(format!("store listing failed for {label} page {page_no}: {source}"));
                    break;
                }
            };
            let ids: Vec<[u8; 32]> = page.blobs.iter().map(|b| b.id).collect();
            let recorded = meta.recorded_blob_sizes(kind, &ids).await?;
            let (recorded_total, orphans) =
                if kind == BlobKind::Chunk { (&mut chunks_recorded, &mut chunks_orphans) } else { (&mut manifests_recorded, &mut manifests_orphans) };
            for blob in &page.blobs {
                if let Some(size) = recorded.get(&blob.id) {
                    recorded_total.count = recorded_total.count.saturating_add(1);
                    recorded_total.bytes = recorded_total.bytes.saturating_add(*size);
                    continue;
                }
                match store.stored_size(*blob).await {
                    Ok(size) => add(orphans, hex(&blob.id), size),
                    Err(error) => {
                        add(orphans, hex(&blob.id), 0);
                        inventory_error = Some(format!("could not read stored size for {} object {}: {error}", label, hex(&blob.id)));
                        break;
                    }
                }
            }
            if inventory_error.is_some() {
                // A size that cannot be read (a backend without metadata-only sizes, or an I/O error) is reported once and
                // ends the inventory; it is not retried for every remaining object.
                break;
            }
            for key in &page.foreign {
                let id = blake3::hash(key.as_bytes()).to_hex().to_string();
                add(&mut foreign, id, 0);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
            page_no = page_no.saturating_add(1);
        }
        if inventory_error.is_some() {
            break;
        }
    }
    let db_check = verify::verify(meta, store, VerifyOptions { read_chunks: true }).await?;
    let chunk_count = audit.chunks;
    let incomplete = incomplete || db_check.audit.absent > 0 || db_check.audit.building > 0;
    let fail_closed = incomplete || db_check.problem_count() > 0;
    // `index_audit` reads all reference roots in one read snapshot. The inventory is diagnostic; a later collector must
    // recheck roots before claiming anything because store listing and metadata snapshots are not atomic together.
    let chunk_candidates = if fail_closed { 0 } else { audit.unreferenced_chunks.count };
    let chunk_candidate_bytes = if fail_closed { 0 } else { audit.unreferenced_chunk_bytes };
    let chunk_protected = if fail_closed { chunk_count } else { chunk_count.saturating_sub(chunk_candidates) };
    let chunk_examples = if fail_closed { Vec::new() } else { audit.unreferenced_chunks.examples };
    Ok(Report {
        chunks_recorded,
        manifests_recorded,
        chunks_orphans,
        manifests_orphans,
        foreign,
        inventory_error,
        db_check,
        incomplete,
        chunk_count,
        chunk_candidates,
        chunk_candidate_bytes,
        chunk_protected,
        chunk_examples,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Report {
    pub fn has_problems(&self) -> bool {
        self.inventory_error.is_some() || self.db_check.problem_count() > 0
    }
    pub fn to_json(&self) -> Value {
        json!({"schema_version":1,"store":{"chunk_recorded":{"count":self.chunks_recorded.count,"stored_bytes":self.chunks_recorded.bytes,"bytes_source":"database blobs.stored_size"},"manifest_recorded":{"count":self.manifests_recorded.count,"stored_bytes":self.manifests_recorded.bytes,"bytes_source":"database blobs.stored_size"},"chunk_orphans":{"count":self.chunks_orphans.count,"stored_bytes":self.chunks_orphans.bytes,"examples":self.chunks_orphans.examples},"manifest_orphans":{"count":self.manifests_orphans.count,"stored_bytes":self.manifests_orphans.bytes,"examples":self.manifests_orphans.examples},"foreign":{"count":self.foreign.count,"stored_bytes":null,"examples":self.foreign.examples},"listing_error":self.inventory_error},"database":{"problems":self.db_check.problem_count(),"verify":self.db_check.to_json()},"candidates":{"orphans":{"count":0,"potential_count":self.chunks_orphans.count.saturating_add(self.manifests_orphans.count),"reason":"eligible only after first-sight grace; would need two runs apart","protection":"first sight is not recorded"},"chunks":{"count":self.chunk_candidates,"stored_bytes":self.chunk_candidate_bytes,"examples":self.chunk_examples,"reason":"no complete-indexed manifest names chunk","protected_count":self.chunk_protected,"protection":if self.incomplete {"index incomplete: dependencies unknown"} else if self.db_check.problem_count()>0 {"protected: database verification reported problems"} else {"versions, saved versions, trash items, live snapshots and import snapshots in the database read snapshot; future collection must recheck"}},"manifests":{"count":0,"stored_bytes":0,"protected_count":self.db_check.audit.unrooted_manifests.count,"protection":"protected: may be a live document's asset"}},"index_incomplete":self.incomplete,"nothing_was_changed":true})
    }
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "store chunks: {} recorded, {} stored bytes (as the database records them)\nstore manifests: {} recorded, {} stored bytes (as the database records them)\nstore chunks: {} orphan(s), {} stored bytes\nstore manifests: {} orphan(s), {} stored bytes\nforeign objects: {}\n",
            self.chunks_recorded.count,
            self.chunks_recorded.bytes,
            self.manifests_recorded.count,
            self.manifests_recorded.bytes,
            self.chunks_orphans.count,
            self.chunks_orphans.bytes,
            self.manifests_orphans.count,
            self.manifests_orphans.bytes,
            self.foreign.count
        );
        out.push_str(&format!(
            "orphan candidates: {}; potential: {}; protection: first sight is not recorded; would need two runs apart\nchunk candidates: {}; {} stored bytes; protected: {}\nforeign stored bytes: not reported\n",
            0, self.chunks_orphans.count.saturating_add(self.manifests_orphans.count), self.chunk_candidates, self.chunk_candidate_bytes, self.chunk_protected
        ));
        for (label, examples) in [
            ("chunk orphan IDs", &self.chunks_orphans.examples),
            ("manifest orphan IDs", &self.manifests_orphans.examples),
            ("foreign object pseudonyms", &self.foreign.examples),
        ] {
            if !examples.is_empty() {
                out.push_str(&format!("{label}: {}\n", examples.join(", ")));
            }
        }
        if self.incomplete {
            out.push_str(&format!("chunk protection: index incomplete: dependencies unknown; protected {} recorded chunk(s)\n", self.chunk_count));
        }
        out.push_str("chunk roots: current and saved file versions (including trash history), live snapshots and import snapshots\n");
        if !self.chunk_examples.is_empty() {
            out.push_str(&format!("chunk candidate IDs from database snapshot: {}\n", self.chunk_examples.join(", ")));
        }
        out.push_str(&format!(
            "manifest protection: protected: may be a live document's asset ({} unrooted recorded manifest(s))\n",
            self.db_check.audit.unrooted_manifests.count
        ));
        if let Some(error) = &self.inventory_error {
            out.push_str(&format!("listing error: {error}\n"));
        }
        if self.db_check.problem_count() > 0 {
            out.push_str(&self.db_check.to_text());
        }
        out.push_str(&format!("database problems: {}\nnothing was changed\n", self.db_check.problem_count()));
        out
    }
}
