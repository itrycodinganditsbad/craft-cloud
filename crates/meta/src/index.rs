//! The reference index: which chunks each manifest names, kept in the database next to the manifests themselves.
//!
//! A manifest is a file's content: a list of chunks. The list lives in the manifest's object in the store, which is
//! the authority; this index is a copy of it that the database can query ("which chunks does this manifest need",
//! "which manifests use this chunk"). Because it is a copy it is only trusted when it is *complete*: every manifest has
//! a row in `manifest_index`, and its state is 1 only after the whole list of edges was compared, ordinal by ordinal,
//! with the manifest it was made from. A row count is never enough.
//!
//! The index is kept in the transaction that records a manifest (`Meta::add_manifest`): up to a budget of rows per
//! transaction. A longer manifest is recorded as being built, and the builder ([`Meta::index_complete`]) writes the
//! rest in further transactions of at most that many rows, so no one holds the write lock for a million rows. The same
//! builder indexes manifests that existed before the index did; it resumes where an interrupted run stopped.
//!
//! When the edges of a manifest do not match it, they are thrown away and written again, in batches of the same size
//! (state 2, "resetting", marks it): an index of a million edges is never deleted in one transaction, and a run that stops
//! between two batches is resumed by the next stage. Rows are compared from the lowest ordinal there is, so an edge at a
//! negative ordinal is found too.
//!
//! Nothing here frees anything. The index is read by the checker (`verify`) and by whatever later decides what is
//! garbage; until it says a manifest is complete, that manifest's chunks are unknown, and unknown means kept.

use std::collections::HashSet;

use craft_cloud_proto::{BlobKind, ChunkId, Manifest, ManifestId};
use sqlx::Row;

use crate::blobs::{KindCode, placeholders};
use crate::codec::{hash_from, to_i64, to_u64};
use crate::{Dialect, Meta, MetaError};

/// Edges one transaction writes by default (and how many rows a manifest may have to be indexed whole in the
/// transaction that records it).
pub const INDEX_BATCH_ROWS: usize = 5_000;

/// Edges per `INSERT` statement.
const ROWS_PER_STATEMENT: usize = 500;

/// How often a builder starts over for one manifest whose edges do not match it before giving up.
const REBUILDS: usize = 2;

const BUILDING: i64 = 0;
const COMPLETE: i64 = 1;
/// The edges are being thrown away to start the index again, a batch of rows to a transaction. Nothing is written
/// and nothing is marked complete until the last edge is gone and the state is back to [`BUILDING`]. A run that stops
/// halfway leaves this marker, and the next one carries on from the rows that are left.
const RESETTING: i64 = 2;

/// How far a manifest's index has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    /// No row: the manifest was recorded before the index existed and nothing has indexed it yet.
    Absent,
    /// Some edges are written (`done` of `total`), or all of them but not yet compared with the manifest.
    Building { done: u64, total: u64 },
    /// Every edge was compared with the manifest.
    Complete { total: u64 },
}

/// What a comparison of a manifest's stored edges with the manifest found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeCheck {
    /// The edges are exactly the manifest's chunks, in order, nothing more.
    Exact,
    /// The first place they differ.
    Mismatch(EdgeProblem),
}

/// Where and how stored edges differ from their manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeProblem {
    pub ordinal: u64,
    pub kind: EdgeProblemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeProblemKind {
    /// The edge at the ordinal names another chunk than the manifest does there.
    WrongChunk,
    /// The manifest has a chunk at the ordinal that has no edge.
    MissingEdge,
    /// There is an edge at an ordinal the manifest has no chunk for.
    ExtraEdge,
    /// There is an edge at a negative ordinal, which no manifest has. The ordinal of the problem is its absolute value.
    NegativeOrdinal,
}

impl EdgeProblemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeProblemKind::WrongChunk => "wrong_chunk",
            EdgeProblemKind::MissingEdge => "missing_edge",
            EdgeProblemKind::ExtraEdge => "extra_edge",
            EdgeProblemKind::NegativeOrdinal => "negative_ordinal",
        }
    }
}

/// The result of checking a manifest's edges and, when they are exact, marking its index complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    Complete,
    Mismatch(EdgeProblem),
    /// Another builder changed the index meanwhile; look again.
    Raced,
}

/// What the database alone says about the index (the checker adds what only the store can tell).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexAudit {
    /// Manifests recorded (`blobs` of kind manifest), and how many of them are in each state of the index.
    pub manifests: u64,
    pub complete: u64,
    pub building: u64,
    pub absent: u64,
    /// Rows of `manifest_index` whose manifest is not recorded.
    pub index_without_manifest: Finding,
    /// Manifests that have edges but no `manifest_index` row.
    pub edges_without_index: Finding,
    /// Complete manifests whose number of edges is not the number of chunks they were indexed with.
    pub complete_with_wrong_count: Finding,
    /// Edges that name a chunk that is not recorded: (manifest, chunk).
    pub unknown_dependencies: Finding,
    /// Versions and snapshots that name a manifest that is not recorded.
    pub roots_without_manifest: Finding,
    /// Manifests named by a version or snapshot whose index is not complete.
    pub roots_not_indexed: Finding,
    /// Recorded manifests that no version, snapshot or import snapshot names. These may be live-document assets, whose
    /// references sit inside the documents (the server cannot read them); they are reported, never judged.
    pub unrooted_manifests: Finding,
    /// Recorded chunks no complete manifest names (only meaningful once every manifest is complete).
    pub unreferenced_chunks: Finding,
    /// Space memberships of a blob that is not recorded.
    pub space_blobs_without_blob: Finding,
}

/// A count and a few examples (hex IDs, `manifest` or `manifest/chunk`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Finding {
    pub count: u64,
    pub examples: Vec<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A row of `manifest_index`.
struct IndexRow {
    chunk_count: i64,
    indexed: i64,
    state: i64,
}

async fn read_state(conn: &mut sqlx::AnyConnection, id: &ManifestId, for_update: bool) -> Result<Option<IndexRow>, MetaError> {
    let sql = if for_update {
        "SELECT chunk_count, indexed_ordinals, state FROM manifest_index WHERE manifest = $1 FOR UPDATE"
    } else {
        "SELECT chunk_count, indexed_ordinals, state FROM manifest_index WHERE manifest = $1"
    };
    let row = sqlx::query(sql).bind(id.as_bytes().to_vec()).fetch_optional(&mut *conn).await?;
    row.map(|r| Ok(IndexRow { chunk_count: r.try_get("chunk_count")?, indexed: r.try_get("indexed_ordinals")?, state: r.try_get("state")? }))
        .transpose()
}

/// The row of a manifest, made (as being built, nothing written) if there is none. On Postgres the row stays locked until the
/// transaction ends, so two builders of one manifest take turns.
async fn ensure_state(conn: &mut sqlx::AnyConnection, id: &ManifestId, total: i64, for_update: bool) -> Result<IndexRow, MetaError> {
    sqlx::query("INSERT INTO manifest_index (manifest, chunk_count, indexed_ordinals, state) VALUES ($1, $2, 0, 0) ON CONFLICT DO NOTHING")
        .bind(id.as_bytes().to_vec())
        .bind(total)
        .execute(&mut *conn)
        .await?;
    read_state(conn, id, for_update).await?.ok_or(MetaError::Corrupt("an index row vanished"))
}

/// Marks a manifest's index as starting again: the row takes the manifest's chunk count, writes nothing, and is not
/// complete (the edges are derived: the manifest is the authority). The edges themselves go in [`reset_step`].
async fn begin_reset(conn: &mut sqlx::AnyConnection, id: &ManifestId, total: i64) -> Result<(), MetaError> {
    sqlx::query("UPDATE manifest_index SET chunk_count = $1, indexed_ordinals = 0, state = $2 WHERE manifest = $3")
        .bind(total)
        .bind(RESETTING)
        .bind(id.as_bytes().to_vec())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Throws away at most `budget` of the edges of a manifest whose index starts again, the lowest ordinals first (a
/// negative one included), so no transaction holds the write lock for the up to a million rows a manifest can have.
/// Returns whether none are left: then the row is building again. Fewer rows deleted than the budget means none are.
async fn reset_step(conn: &mut sqlx::AnyConnection, id: &ManifestId, budget: usize) -> Result<bool, MetaError> {
    let limit = i64::try_from(budget.max(1)).map_err(|_| MetaError::Invalid("batch too large"))?;
    let deleted = sqlx::query(
        "DELETE FROM manifest_chunks WHERE manifest = $1 AND ordinal IN (SELECT ordinal FROM manifest_chunks WHERE manifest = $1 ORDER BY ordinal LIMIT $2)",
    )
    .bind(id.as_bytes().to_vec())
    .bind(limit)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let finished = i64::try_from(deleted).is_ok_and(|n| n < limit);
    if finished {
        sqlx::query("UPDATE manifest_index SET state = $1 WHERE manifest = $2 AND state = $3")
            .bind(BUILDING)
            .bind(id.as_bytes().to_vec())
            .bind(RESETTING)
            .execute(&mut *conn)
            .await?;
    }
    Ok(finished)
}

/// Writes the edges for the ordinals `from..to` of `manifest`, 500 to a statement. Edges already there are left alone
/// (the comparison catches one that is wrong).
async fn write_edges(conn: &mut sqlx::AnyConnection, manifest: &Manifest, from: usize, to: usize) -> Result<(), MetaError> {
    let id = manifest.id();
    let chunks = manifest.chunks().get(from..to).ok_or(MetaError::Corrupt("an index range outside its manifest"))?;
    for (part_index, part) in chunks.chunks(ROWS_PER_STATEMENT).enumerate() {
        let rows = (0..part.len()).map(|i| format!("({})", placeholders(i * 3 + 1, 3))).collect::<Vec<_>>().join(", ");
        let mut query =
            sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES {rows} ON CONFLICT DO NOTHING")));
        for (i, chunk) in part.iter().enumerate() {
            let ordinal = from
                .checked_add(part_index.saturating_mul(ROWS_PER_STATEMENT))
                .and_then(|n| n.checked_add(i))
                .ok_or(MetaError::Corrupt("ordinal overflow"))?;
            query = query.bind(id.as_bytes().to_vec()).bind(to_i64(ordinal as u64)?).bind(chunk.id.as_bytes().to_vec());
        }
        query.execute(&mut *conn).await?;
    }
    Ok(())
}

/// Compares the stored edges of `manifest`, a page of `page` rows at a time, with its chunks.
async fn compare(conn: &mut sqlx::AnyConnection, manifest: &Manifest, page: usize) -> Result<EdgeCheck, MetaError> {
    let id = manifest.id();
    let chunks = manifest.chunks();
    let limit = i64::try_from(page.max(1)).map_err(|_| MetaError::Invalid("page too large"))?;
    // Pagination starts at the lowest number there is, so an edge at a negative ordinal is not skipped.
    let (mut next, mut from): (usize, i64) = (0, i64::MIN);
    loop {
        let rows = sqlx::query("SELECT ordinal, chunk FROM manifest_chunks WHERE manifest = $1 AND ordinal >= $2 ORDER BY ordinal LIMIT $3")
            .bind(id.as_bytes().to_vec())
            .bind(from)
            .bind(limit)
            .fetch_all(&mut *conn)
            .await?;
        for row in &rows {
            let raw: i64 = row.try_get("ordinal")?;
            if raw < 0 {
                // Rows come lowest first, so a negative ordinal is the first thing seen: no manifest has such an edge.
                return Ok(EdgeCheck::Mismatch(EdgeProblem { ordinal: raw.unsigned_abs(), kind: EdgeProblemKind::NegativeOrdinal }));
            }
            let ordinal = to_u64(raw)?;
            let chunk = hash_from(&row.try_get::<Vec<u8>, _>("chunk")?)?;
            let expected = next as u64;
            if ordinal > expected {
                return Ok(EdgeCheck::Mismatch(EdgeProblem { ordinal: expected, kind: EdgeProblemKind::MissingEdge }));
            }
            match chunks.get(next) {
                None => return Ok(EdgeCheck::Mismatch(EdgeProblem { ordinal, kind: EdgeProblemKind::ExtraEdge })),
                Some(c) if *c.id.as_bytes() != chunk => return Ok(EdgeCheck::Mismatch(EdgeProblem { ordinal, kind: EdgeProblemKind::WrongChunk })),
                Some(_) => {}
            }
            next += 1;
            from = match raw.checked_add(1) {
                Some(n) => n,
                None => return Ok(EdgeCheck::Mismatch(EdgeProblem { ordinal, kind: EdgeProblemKind::ExtraEdge })),
            };
        }
        if rows.len() < page.max(1) {
            break;
        }
    }
    if next < chunks.len() {
        return Ok(EdgeCheck::Mismatch(EdgeProblem { ordinal: next as u64, kind: EdgeProblemKind::MissingEdge }));
    }
    Ok(EdgeCheck::Exact)
}

/// Marks a manifest's index complete if it is still as it was when its edges were compared (`indexed_ordinals` and
/// `chunk_count` both `total`, state still 0). Returns whether it did.
async fn mark_complete(conn: &mut sqlx::AnyConnection, id: &ManifestId, total: i64) -> Result<bool, MetaError> {
    let done = sqlx::query("UPDATE manifest_index SET state = 1 WHERE manifest = $1 AND state = 0 AND chunk_count = $2 AND indexed_ordinals = $2")
        .bind(id.as_bytes().to_vec())
        .bind(total)
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected() == 1)
}

/// Writes the next batch of edges of `manifest` (at most `budget`) and moves the frontier. Returns the state after it,
/// and whether the index is still being thrown away to start again (then this step only deleted edges, at most
/// `budget` of them, and `done` says nothing about edges being written).
pub(crate) async fn stage(conn: &mut sqlx::AnyConnection, pg: bool, manifest: &Manifest, budget: usize) -> Result<(IndexState, bool), MetaError> {
    let id = manifest.id();
    let total = manifest.chunks().len();
    let total_i = to_i64(total as u64)?;
    let row = ensure_state(conn, &id, total_i, pg).await?;
    // A row that does not belong to this manifest's list starts again: the manifest is the authority.
    let foreign = row.chunk_count != total_i || row.indexed < 0 || row.indexed > total_i;
    if foreign {
        begin_reset(conn, &id, total_i).await?;
    } else if row.state == COMPLETE {
        return Ok((IndexState::Complete { total: total as u64 }, false));
    }
    if foreign || row.state == RESETTING {
        let finished = reset_step(conn, &id, budget).await?;
        return Ok((IndexState::Building { done: 0, total: total as u64 }, !finished));
    }
    let start = usize::try_from(row.indexed).map_err(|_| MetaError::Corrupt("a negative index frontier"))?;
    let end = start.saturating_add(budget.max(1)).min(total);
    if end > start {
        write_edges(conn, manifest, start, end).await?;
        sqlx::query("UPDATE manifest_index SET indexed_ordinals = $1 WHERE manifest = $2 AND indexed_ordinals < $1")
            .bind(to_i64(end as u64)?)
            .bind(id.as_bytes().to_vec())
            .execute(&mut *conn)
            .await?;
    }
    Ok((IndexState::Building { done: end as u64, total: total as u64 }, false))
}

/// Called in the transaction that records a manifest: makes its index row and writes up to a budget of edges, and when
/// that is all of them compares them with the manifest and marks the index complete. A longer manifest is left being
/// built for [`Meta::index_complete`]. A row that already exists is not touched beyond that.
pub(crate) async fn register(conn: &mut sqlx::AnyConnection, pg: bool, manifest: &Manifest, budget: usize) -> Result<(), MetaError> {
    let id = manifest.id();
    let total = to_i64(manifest.chunks().len() as u64)?;
    if let (IndexState::Building { done, total: n }, false) = stage(conn, pg, manifest, budget).await?
        && done >= n
        && compare(conn, manifest, budget).await? == EdgeCheck::Exact
    {
        mark_complete(conn, &id, total).await?;
    }
    Ok(())
}

impl Meta {
    /// The store with another budget of edges per index transaction (the default is [`INDEX_BATCH_ROWS`]). For tests.
    pub fn with_index_batch_rows(mut self, rows: usize) -> Self {
        self.index_rows = rows.max(1);
        self
    }

    /// Writes the next batch of `manifest`'s edges, at most the index budget, in one transaction, and says how far the
    /// index has come. Call it again until it says all edges are written, then [`Meta::index_validate`]; or call
    /// [`Meta::index_complete`], which does both. Each call is safe to repeat and to run beside commits and other builders.
    pub async fn index_stage(&self, manifest: &Manifest) -> Result<IndexState, MetaError> {
        let mut w = self.begin_write().await?;
        let (state, _) = stage(&mut w.tx, self.dialect == Dialect::Postgres, manifest, self.index_rows).await?;
        w.commit().await?;
        Ok(state)
    }

    /// Compares every edge of `manifest` with it, in a read of one snapshot, and if they are exact marks its index
    /// complete (provided every edge was written and nothing changed meanwhile). Nothing else marks an index complete.
    pub async fn index_validate(&self, manifest: &Manifest) -> Result<Validation, MetaError> {
        let id = manifest.id();
        let total = to_i64(manifest.chunks().len() as u64)?;
        let check = {
            let mut tx = self.read_snapshot().await?;
            let state = read_state(&mut tx, &id, false).await?;
            let ready = state.is_some_and(|s| s.chunk_count == total && s.indexed == total && s.state != RESETTING);
            let check = if ready { Some(compare(&mut tx, manifest, self.index_rows).await?) } else { None };
            tx.commit().await?;
            check
        };
        match check {
            None => Ok(Validation::Raced),
            Some(EdgeCheck::Mismatch(problem)) => Ok(Validation::Mismatch(problem)),
            Some(EdgeCheck::Exact) => {
                let mut w = self.begin_write().await?;
                let marked = mark_complete(&mut w.tx, &id, total).await?;
                w.commit().await?;
                // Already complete (another builder was first) is as good.
                if marked || matches!(self.index_state(&id).await?, IndexState::Complete { .. }) {
                    Ok(Validation::Complete)
                } else {
                    Ok(Validation::Raced)
                }
            }
        }
    }

    /// Indexes `manifest` to the end: edges in batches of at most the index budget, each its own transaction, then the
    /// comparison that marks it complete. Resumes where an earlier run stopped. Edges that do not match the manifest
    /// are thrown away and written again (the manifest is the authority), at most twice; then it is `Corrupt`.
    pub async fn index_complete(&self, manifest: &Manifest) -> Result<(), MetaError> {
        let id = manifest.id();
        let total = manifest.chunks().len() as u64;
        let mut rebuilds = 0;
        for _ in 0..(REBUILDS + 1) * 4 {
            loop {
                let mut w = self.begin_write().await?;
                let (state, resetting) = stage(&mut w.tx, self.dialect == Dialect::Postgres, manifest, self.index_rows).await?;
                w.commit().await?;
                match state {
                    IndexState::Complete { .. } => return Ok(()),
                    // Still throwing edges away: nothing is written yet, whatever the frontier says.
                    IndexState::Building { .. } if resetting => {}
                    IndexState::Building { done, .. } if done >= total => break,
                    IndexState::Building { .. } | IndexState::Absent => {}
                }
            }
            match self.index_validate(manifest).await? {
                Validation::Complete => return Ok(()),
                Validation::Raced => {}
                Validation::Mismatch(_) => {
                    if rebuilds >= REBUILDS {
                        return Err(MetaError::Corrupt("a manifest's index does not match it after being rebuilt"));
                    }
                    rebuilds += 1;
                    let mut w = self.begin_write().await?;
                    // Only a row that is not complete is rebuilt here: a complete one that is wrong is the checker's to report.
                    let row = read_state(&mut w.tx, &id, self.dialect == Dialect::Postgres).await?;
                    // The edges go in batches of the next stages, each its own transaction, and a stop between them resumes.
                    if row.is_some_and(|r| r.state != COMPLETE) {
                        begin_reset(&mut w.tx, &id, to_i64(total)?).await?;
                    }
                    w.commit().await?;
                }
            }
        }
        Err(MetaError::Corrupt("a manifest's index could not be completed"))
    }

    /// How far `manifest`'s index has come.
    pub async fn index_state(&self, manifest: &ManifestId) -> Result<IndexState, MetaError> {
        let mut conn = self.pool.acquire().await?;
        Ok(match read_state(&mut conn, manifest, false).await? {
            None => IndexState::Absent,
            Some(r) if r.state == COMPLETE => IndexState::Complete { total: to_u64(r.chunk_count)? },
            Some(r) => IndexState::Building { done: to_u64(r.indexed)?, total: to_u64(r.chunk_count)? },
        })
    }

    /// Compares the edges stored for `manifest` with it, without changing anything.
    pub async fn index_check(&self, manifest: &Manifest) -> Result<EdgeCheck, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let check = compare(&mut tx, manifest, self.index_rows).await?;
        tx.commit().await?;
        Ok(check)
    }

    /// The stored edges of a manifest, in order: `(ordinal, chunk)`. For tests and the checker.
    pub async fn index_edges(&self, manifest: &ManifestId) -> Result<Vec<(u64, ChunkId)>, MetaError> {
        let rows = sqlx::query("SELECT ordinal, chunk FROM manifest_chunks WHERE manifest = $1 ORDER BY ordinal")
            .bind(manifest.as_bytes().to_vec())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|r| Ok((to_u64(r.try_get("ordinal")?)?, ChunkId::from_bytes(hash_from(&r.try_get::<Vec<u8>, _>("chunk")?)?)))).collect()
    }

    /// How many edges the index holds in all.
    pub async fn index_edge_count(&self) -> Result<u64, MetaError> {
        let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM manifest_chunks").fetch_one(&self.pool).await?.try_get("n")?;
        to_u64(n)
    }

    /// Up to `limit` recorded manifests after `after` (in ID order) whose index is not complete: the work of a builder.
    pub async fn manifests_to_index(&self, after: Option<ManifestId>, limit: usize) -> Result<Vec<ManifestId>, MetaError> {
        let after = after.map_or_else(|| vec![0u8; 32], |m| m.as_bytes().to_vec());
        let rows = sqlx::query(
            "SELECT b.id FROM blobs b LEFT JOIN manifest_index i ON i.manifest = b.id WHERE b.kind = $1 AND b.id > $2 AND (i.manifest IS NULL OR i.state <> 1) \
             ORDER BY b.id LIMIT $3",
        )
        .bind(BlobKind::Manifest.code())
        .bind(after)
        .bind(i64::try_from(limit).map_err(|_| MetaError::Invalid("limit too large"))?)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| Ok(ManifestId::from_bytes(hash_from(&r.try_get::<Vec<u8>, _>("id")?)?))).collect()
    }

    /// Up to `limit` recorded blobs of `kind` after `after`, in ID order.
    pub async fn blob_ids_after(&self, kind: BlobKind, after: Option<[u8; 32]>, limit: usize) -> Result<Vec<[u8; 32]>, MetaError> {
        let after = after.map_or_else(|| vec![0u8; 32], |id| id.to_vec());
        let rows = sqlx::query("SELECT id FROM blobs WHERE kind = $1 AND id > $2 ORDER BY id LIMIT $3")
            .bind(kind.code())
            .bind(after)
            .bind(i64::try_from(limit).map_err(|_| MetaError::Invalid("limit too large"))?)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|r| hash_from(&r.try_get::<Vec<u8>, _>("id")?)).collect()
    }

    /// Which of `ids` are recorded chunks.
    pub async fn recorded_chunks(&self, ids: &[ChunkId]) -> Result<HashSet<ChunkId>, MetaError> {
        let mut found = HashSet::new();
        for part in ids.chunks(ROWS_PER_STATEMENT) {
            let sql = format!("SELECT id FROM blobs WHERE kind = $1 AND id IN ({})", placeholders(2, part.len()));
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(BlobKind::Chunk.code());
            for id in part {
                query = query.bind(id.as_bytes().to_vec());
            }
            for row in query.fetch_all(&self.pool).await? {
                found.insert(ChunkId::from_bytes(hash_from(&row.try_get::<Vec<u8>, _>("id")?)?));
            }
        }
        Ok(found)
    }

    /// What the database says about the index, its roots and the blobs it names, in a read of one snapshot, with up to
    /// `examples` examples of each problem. Changes nothing.
    pub async fn index_audit(&self, examples: usize) -> Result<IndexAudit, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let mut audit = IndexAudit::default();
        audit.manifests = scalar(&mut tx, "SELECT COUNT(*) AS n FROM blobs WHERE kind = 1").await?;
        audit.complete =
            scalar(&mut tx, "SELECT COUNT(*) AS n FROM blobs b JOIN manifest_index i ON i.manifest = b.id WHERE b.kind = 1 AND i.state = 1").await?;
        audit.building =
            scalar(&mut tx, "SELECT COUNT(*) AS n FROM blobs b JOIN manifest_index i ON i.manifest = b.id WHERE b.kind = 1 AND i.state <> 1").await?;
        audit.absent = audit.manifests.saturating_sub(audit.complete).saturating_sub(audit.building);
        // Every manifest a version, a live document's snapshot or its import snapshot names.
        const ROOTS: &str = "(SELECT manifest AS a FROM versions UNION SELECT snapshot AS a FROM live_docs WHERE snapshot IS NOT NULL                              UNION SELECT import_snapshot AS a FROM live_docs WHERE import_snapshot IS NOT NULL) r";
        audit.index_without_manifest = finding(
            &mut tx,
            examples,
            &["i.manifest"],
            "manifest_index i WHERE NOT EXISTS (SELECT 1 FROM blobs b WHERE b.kind = 1 AND b.id = i.manifest)",
        )
        .await?;
        audit.edges_without_index = finding(
            &mut tx,
            examples,
            &["mc.manifest"],
            "manifest_chunks mc WHERE NOT EXISTS (SELECT 1 FROM manifest_index i WHERE i.manifest = mc.manifest)",
        )
        .await?;
        audit.complete_with_wrong_count = finding(
            &mut tx,
            examples,
            &["i.manifest"],
            "manifest_index i WHERE i.state = 1 AND i.chunk_count <> (SELECT COUNT(*) FROM manifest_chunks mc WHERE mc.manifest = i.manifest)",
        )
        .await?;
        audit.unknown_dependencies = finding(
            &mut tx,
            examples,
            &["mc.manifest", "mc.chunk"],
            "manifest_chunks mc WHERE NOT EXISTS (SELECT 1 FROM blobs b WHERE b.kind = 0 AND b.id = mc.chunk)",
        )
        .await?;
        audit.roots_without_manifest =
            finding(&mut tx, examples, &["r.a"], &format!("{ROOTS} WHERE NOT EXISTS (SELECT 1 FROM blobs b WHERE b.kind = 1 AND b.id = r.a)"))
                .await?;
        audit.roots_not_indexed = finding(
            &mut tx,
            examples,
            &["r.a"],
            &format!(
                "{ROOTS} WHERE EXISTS (SELECT 1 FROM blobs b WHERE b.kind = 1 AND b.id = r.a)                  AND NOT EXISTS (SELECT 1 FROM manifest_index i WHERE i.manifest = r.a AND i.state = 1)"
            ),
        )
        .await?;
        audit.unrooted_manifests =
            finding(&mut tx, examples, &["b.id"], &format!("blobs b WHERE b.kind = 1 AND NOT EXISTS (SELECT 1 FROM {ROOTS} WHERE r.a = b.id)"))
                .await?;
        audit.unreferenced_chunks =
            finding(&mut tx, examples, &["b.id"], "blobs b WHERE b.kind = 0 AND NOT EXISTS (SELECT 1 FROM manifest_chunks mc WHERE mc.chunk = b.id)")
                .await?;
        audit.space_blobs_without_blob =
            finding(&mut tx, examples, &["sb.id"], "space_blobs sb WHERE NOT EXISTS (SELECT 1 FROM blobs b WHERE b.kind = sb.kind AND b.id = sb.id)")
                .await?;
        tx.commit().await?;
        Ok(audit)
    }
}

/// A count query's number.
async fn scalar(conn: &mut sqlx::AnyConnection, sql: &'static str) -> Result<u64, MetaError> {
    to_u64(sqlx::query(sql).fetch_one(&mut *conn).await?.try_get("n")?)
}

/// How many distinct `columns` the rows of `from_where` have, and the first few (hex, joined by `/`). Only constant
/// text goes into the SQL.
async fn finding(conn: &mut sqlx::AnyConnection, examples: usize, columns: &[&str], from_where: &str) -> Result<Finding, MetaError> {
    let select = columns.iter().enumerate().map(|(i, c)| format!("{c} AS c{i}")).collect::<Vec<_>>().join(", ");
    let counted = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) AS n FROM (SELECT DISTINCT {select} FROM {from_where}) q")))
        .fetch_one(&mut *conn)
        .await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT DISTINCT {select} FROM {from_where} ORDER BY 1 LIMIT {examples}")))
        .fetch_all(&mut *conn)
        .await?;
    let mut shown = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut parts = Vec::with_capacity(columns.len());
        for i in 0..columns.len() {
            parts.push(hex(&row.try_get::<Vec<u8>, _>(format!("c{i}").as_str())?));
        }
        shown.push(parts.join("/"));
    }
    Ok(Finding { count: to_u64(counted.try_get("n")?)?, examples: shown })
}
