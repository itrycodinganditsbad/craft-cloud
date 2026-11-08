//! The admin's storage commands: `store recompress` (rewrite objects at a higher zstd level where that saves more),
//! `store estimate` (what other levels would save, measured on a sample, read only), and `store measure` (fill in the
//! sizes of objects written before sizes were tracked).
//!
//! Nothing here deletes an object, and every rewrite is the store's atomic replace of an object whose plain bytes
//! were checked against its ID before and after, so the commands are safe to run while the server serves requests.

use std::io::Write;

use craft_cloud_meta::{BlobKind, Meta};
use craft_cloud_store::{BlobKey, MAX_ZSTD_LEVEL, MIN_ZSTD_LEVEL, Store, StoreError};
use serde::{Deserialize, Serialize};

use crate::cli::CliError;

/// The `server_state` key of the last estimate.
pub const ESTIMATE_KEY: &str = "compression_estimate";

/// The `server_state` key of the level an admin set while the server runs.
pub const LEVEL_KEY: &str = "compression_level";

/// How many blobs `store recompress` and `store measure` read from the database at a time.
const PAGE: usize = 500;

/// Errors listed one by one in a report; the rest are only counted.
const LISTED_ERRORS: usize = 20;

/// The most blobs `store estimate` samples.
pub const MAX_SAMPLE: usize = 5000;

fn key_of(kind: BlobKind, id: [u8; 32]) -> BlobKey {
    BlobKey { kind, id }
}

/// What a recompress pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecompressReport {
    /// Blobs looked at (rows read from the database).
    pub scanned: u64,
    /// Skipped without reading: an earlier pass already did this level or a higher one.
    pub already_done: u64,
    /// Read, and left alone because a rewrite would not save enough.
    pub kept: u64,
    /// Replaced by a smaller object (with `dry_run`: that would be).
    pub rewritten: u64,
    /// The sizes of the rewritten objects before and after.
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Rows whose object is missing from the store.
    pub missing: u64,
    /// Objects that could not be read or checked, or a rewrite that failed. They are left as they were.
    pub failed: u64,
    /// The first of the failures, as `kind/id: reason`.
    pub failures: Vec<String>,
    /// The pass stopped because it read `--limit-bytes`; run it again to go on.
    pub stopped_at_limit: bool,
}

impl RecompressReport {
    pub fn saved(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Rewrites stored objects at zstd `level` where that makes them smaller, in database order. `limit_bytes` stops the
/// pass after that many bytes of objects were read. A pass that is stopped or interrupted continues where it left off
/// when run again: every blob it finished is recorded at the level, and blobs recorded at that level or higher are
/// skipped. With `dry_run`, nothing is written, in the store or the database.
pub async fn recompress(
    meta: &Meta,
    store: &Store,
    level: u8,
    dry_run: bool,
    limit_bytes: Option<u64>,
    out: &mut impl Write,
) -> Result<RecompressReport, CliError> {
    if !(MIN_ZSTD_LEVEL..=MAX_ZSTD_LEVEL).contains(&level) {
        return Err(CliError::Usage(format!("--level must be from {MIN_ZSTD_LEVEL} to {MAX_ZSTD_LEVEL}")));
    }
    let mut report = RecompressReport::default();
    let mut read_bytes = 0u64;
    let mut after = None;
    'pages: loop {
        let page = meta.blob_page(after, PAGE).await?;
        let Some(last) = page.last() else { break };
        after = Some((last.kind, last.id));
        for row in &page {
            report.scanned += 1;
            if row.physical_size.is_some() && row.level.is_some_and(|done| done >= level) {
                report.already_done += 1;
                continue;
            }
            let key = key_of(row.kind, row.id);
            match store.recompress(key, level, !dry_run).await {
                Ok(done) => {
                    read_bytes = read_bytes.saturating_add(done.before);
                    if done.rewritten || (dry_run && done.after < done.before) {
                        report.rewritten += 1;
                        report.bytes_before = report.bytes_before.saturating_add(done.before);
                        report.bytes_after = report.bytes_after.saturating_add(done.after);
                    } else {
                        report.kept += 1;
                    }
                    if !dry_run {
                        // The object is replaced already: a failure here only leaves the size unknown or stale.
                        if let Err(e) = meta.set_physical(row.kind, &row.id, done.after, level).await {
                            tracing::warn!(error = %e, "couldn't record a blob's stored size");
                        }
                    }
                }
                Err(StoreError::NotFound(_)) => report.missing += 1,
                Err(e) => {
                    report.failed += 1;
                    if report.failures.len() < LISTED_ERRORS {
                        report.failures.push(format!("{}: {e}", key.path()));
                    }
                }
            }
            if report.scanned % 1000 == 0 {
                writeln!(out, "… {} blobs looked at, {} rewritten, {} saved", report.scanned, report.rewritten, report.saved())?;
            }
            if limit_bytes.is_some_and(|limit| read_bytes >= limit) {
                report.stopped_at_limit = true;
                break 'pages;
            }
        }
    }
    Ok(report)
}

/// The summary `store recompress` prints.
pub fn print_recompress(report: &RecompressReport, level: u8, dry_run: bool, out: &mut impl Write) -> std::io::Result<()> {
    let verb = if dry_run { "would rewrite" } else { "rewrote" };
    writeln!(
        out,
        "{} blobs looked at at level {level}: {verb} {}, kept {}, skipped {} done before, {} without an object, {} failed",
        report.scanned, report.rewritten, report.kept, report.already_done, report.missing, report.failed
    )?;
    if report.rewritten > 0 {
        writeln!(
            out,
            "{} went from {} to {} bytes: {} bytes less ({:.1}% of what was rewritten)",
            if dry_run { "they would go" } else { "they" },
            report.bytes_before,
            report.bytes_after,
            report.saved(),
            percent(report.saved(), report.bytes_before)
        )?;
    }
    for failure in &report.failures {
        writeln!(out, "left alone, not readable: {failure}")?;
    }
    if report.failed as usize > report.failures.len() {
        writeln!(out, "… and {} more", report.failed as usize - report.failures.len())?;
    }
    if report.stopped_at_limit {
        writeln!(out, "stopped at --limit-bytes; run the same command again to go on where this left off")?;
    }
    if dry_run {
        writeln!(out, "dry run: nothing was changed")?;
    }
    Ok(())
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 * 100.0 / whole as f64 }
}

/// Fills in the stored size of every blob that has none, by asking the store for the object's length. Returns how
/// many it filled in and how many have no object.
pub async fn measure(meta: &Meta, store: &Store, out: &mut impl Write) -> Result<(u64, u64), CliError> {
    let (mut filled, mut missing) = (0u64, 0u64);
    let mut after = None;
    loop {
        let page = meta.blob_page(after, PAGE).await?;
        let Some(last) = page.last() else { break };
        after = Some((last.kind, last.id));
        for row in page.iter().filter(|row| row.physical_size.is_none()) {
            match store.object_len(key_of(row.kind, row.id)).await? {
                Some(len) => {
                    meta.set_measured_size(row.kind, &row.id, len).await?;
                    filled += 1;
                }
                None => missing += 1,
            }
        }
    }
    writeln!(out, "measured {filled} blobs, {missing} have no object")?;
    Ok((filled, missing))
}

/// What one level would save on the sample, and how fast it compresses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelEstimate {
    pub level: u8,
    pub saving_percent: f64,
    pub mib_per_second: f64,
}

/// The result of `store estimate`, kept in `server_state` for the admin overview.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub sampled_blobs: u64,
    pub sampled_bytes: u64,
    /// What the sampled blobs save as they are stored now, in percent of their logical size.
    pub current_saving_percent: f64,
    pub levels: Vec<LevelEstimate>,
}

/// Parses `1,3,9,19`.
pub fn parse_levels(text: &str) -> Result<Vec<u8>, CliError> {
    let mut levels = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let level: u8 = part.parse().map_err(|_| CliError::Usage(format!("{part:?} is not a zstd level")))?;
        if !(MIN_ZSTD_LEVEL..=MAX_ZSTD_LEVEL).contains(&level) {
            return Err(CliError::Usage(format!("level {level} is outside {MIN_ZSTD_LEVEL} to {MAX_ZSTD_LEVEL}")));
        }
        if !levels.contains(&level) {
            levels.push(level);
        }
    }
    if levels.is_empty() {
        return Err(CliError::Usage("--levels needs at least one level, like 1,3,9,19".into()));
    }
    Ok(levels)
}

/// Samples up to `sample` stored chunks (read only: the store and the blob rows are not changed), compresses each at
/// every level, and returns what that would save and how fast it is. `None` when there is nothing to sample.
pub async fn estimate(meta: &Meta, store: &Store, sample: usize, levels: &[u8]) -> Result<Option<Estimate>, CliError> {
    let sample = sample.clamp(1, MAX_SAMPLE);
    let mut seen = std::collections::HashSet::new();
    let (mut plain, mut logical, mut physical_now, mut count) = (0u64, 0u64, 0u64, 0u64);
    let mut per_level: Vec<(u64, u64)> = vec![(0, 0); levels.len()];
    let mut nanos: Vec<u64> = vec![0; levels.len()];
    let mut attempts = 0usize;
    while seen.len() < sample && attempts < sample.saturating_mul(3) {
        attempts += 1;
        let mut from = [0u8; 32];
        getrandom::fill(&mut from).map_err(|e| CliError::Usage(format!("the system random source failed: {e}")))?;
        let Some(row) = meta.blob_from(BlobKind::Chunk, &from).await? else { break };
        if !seen.insert(row.id) {
            continue;
        }
        let key = key_of(row.kind, row.id);
        let samples = match store.sample_levels(key, levels).await {
            Ok(Some(samples)) => samples,
            // A blob without an object, or one that does not read, is not part of the sample.
            Ok(None) | Err(_) => continue,
        };
        let Some(now) = store.object_len(key).await.ok().flatten() else { continue };
        count += 1;
        physical_now += now;
        for (slot, level_sample) in samples.iter().enumerate() {
            if let (Some(sizes), Some(time)) = (per_level.get_mut(slot), nanos.get_mut(slot)) {
                sizes.0 = sizes.0.saturating_add(level_sample.plain_len);
                sizes.1 = sizes.1.saturating_add(level_sample.stored_len);
                *time = time.saturating_add(level_sample.nanos);
            }
        }
        if let Some(first) = samples.first() {
            plain = plain.saturating_add(first.plain_len);
            logical = logical.saturating_add(first.plain_len + 1);
        }
    }
    if count == 0 {
        return Ok(None);
    }
    let levels = levels
        .iter()
        .zip(per_level.iter().zip(&nanos))
        .map(|(level, ((plain_bytes, stored), nanos))| LevelEstimate {
            level: *level,
            saving_percent: 100.0 - percent(*stored, plain_bytes.saturating_add(count)),
            mib_per_second: if *nanos == 0 { 0.0 } else { *plain_bytes as f64 / (1024.0 * 1024.0) / (*nanos as f64 / 1e9) },
        })
        .collect();
    Ok(Some(Estimate { sampled_blobs: count, sampled_bytes: plain, current_saving_percent: 100.0 - percent(physical_now, logical), levels }))
}

/// The summary `store estimate` prints.
pub fn print_estimate(estimate: &Estimate, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(
        out,
        "sampled {} chunks, {} bytes; as stored now they save {:.1}%",
        estimate.sampled_blobs, estimate.sampled_bytes, estimate.current_saving_percent
    )?;
    writeln!(out, "level  saving  compress speed")?;
    for level in &estimate.levels {
        writeln!(out, "{:>5}  {:>5.1}%  {:>8.0} MiB/s", level.level, level.saving_percent, level.mib_per_second)?;
    }
    Ok(())
}

/// The last estimate and when it was made (ms), if the admin ran one.
pub async fn last_estimate(meta: &Meta) -> Result<Option<(Estimate, i64)>, craft_cloud_meta::MetaError> {
    let Some((json, at)) = meta.server_value(ESTIMATE_KEY).await? else { return Ok(None) };
    // A value written by a newer or older server that does not parse is no estimate, not an error.
    Ok(serde_json::from_str(&json).ok().map(|estimate| (estimate, at)))
}

/// Stores the estimate for the overview.
pub async fn save_estimate(meta: &Meta, estimate: &Estimate, now_ms: i64) -> Result<(), CliError> {
    let json = serde_json::to_string(estimate).map_err(|e| CliError::Usage(format!("can't store the estimate: {e}")))?;
    meta.set_server_value(ESTIMATE_KEY, &json, now_ms).await?;
    Ok(())
}

/// The compression settings the server started with (the config file's), kept for the admin's storage overview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionSettings {
    /// `compression = "none"`: nothing is written compressed, whatever the level.
    pub none: bool,
    /// The config file's level.
    pub config_level: u8,
    pub min_saving_percent: u8,
}

impl CompressionSettings {
    /// The settings a store was built with.
    pub fn of(store: &Store) -> Self {
        Self {
            none: store.compression() == craft_cloud_store::Compression::None,
            config_level: store.level(),
            min_saving_percent: store.min_saving(),
        }
    }
}
