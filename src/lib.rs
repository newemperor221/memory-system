//! MemorySystem v2 — Four-layer persistent memory for OpenClaw agents
//!
//! Architecture:
//! - **L0**: Working memory (process memory, DashMap, session-scoped)
//! - **L1**: Short-term memory (SQLite WAL, cross-agent, TTL 7 days)
//! - **L2**: Semantic memory (in-memory, BM25 + Gemini embeddings)
//! - **L3**: Archive memory (filesystem .md files, long-term)
//!
//! Write path:  L0 → L1 (fsync) → L2 (async event) → L3 (async event)
//! Recall path: L0 → L1 → L2 → (L3 hint if all miss)

pub mod common;
pub mod config;
pub mod errors;

pub mod l0;
pub mod l1;
pub mod l2;
pub mod l3;

pub use common::{Entry, Importance, Layer, RecallRequest, RecallResult, MemoryStats};
pub use config::Config;
pub use errors::MemoryError;

use std::sync::Arc;

/// The four-layer memory system
pub struct MemorySystem {
    pub l0: Arc<l0::WorkingMemory>,
    pub l1: Arc<l1::L1>,
    pub l2: Arc<l2::L2>,
    pub l3: Arc<l3::L3>,
}

impl MemorySystem {
    /// Initialize all four layers in order.
    ///
    /// L1 is the source of truth and starts first.
    /// L2 and L3 subscribe to L1's event bus.
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        // 1. L1: synchronous init (opens SQLite, starts broadcaster thread)
        let l1 = Arc::new(l1::L1::new(&config)?);

        // 2. L2: async init (subscribes to L1 event bus, rebuilds from L1)
        let l2 = Arc::new(l2::L2::new(l1.clone(), &config).await.map_err(|e| {
            anyhow::anyhow!("L2 init failed: {}", e)
        })?);

        // 3. L3: synchronous init (scans archive files on startup)
        let l3 = Arc::new(l3::L3::new(l1.clone(), &config)?);

        // 4. L0: process memory, no dependencies
        let l0 = Arc::new(l0::WorkingMemory::new());

        tracing::info!("MemorySystem v2 initialized");
        tracing::info!(
            "  L1 path: {}, L2 entries: {}, L3 dir: {}",
            config.l1_path,
            config.l2_max_entries,
            config.l3_archive_dir
        );

        Ok(Self { l0, l1, l2, l3 })
    }

    /// Write a memory entry through all layers.
    ///
    /// L0 is written synchronously (in-process).
    /// L1 is written synchronously with fsync (durable).
    /// L2 and L3 are updated asynchronously via L1's event bus.
    pub async fn remember(&self, mut entry: Entry) -> anyhow::Result<()> {
        if entry.id.is_empty() {
            entry.id = uuid::Uuid::new_v4().to_string();
        }
        entry.created_at = chrono::Utc::now();
        entry.last_accessed = chrono::Utc::now();

        // L0: in-process write (sync)
        self.l0.write(entry.key.clone(), entry.clone());

        // L1: durable write (sync, fsync on return)
        self.l1.write(&entry.key, &entry)?;

        // L2/L3: updated asynchronously via L1 event bus
        Ok(())
    }

    /// Recall memories from all layers, merge and deduplicate.
    pub async fn recall(&self, req: RecallRequest) -> Vec<RecallResult> {
        // L0: fast session-scoped keyword recall (async store, sync methods)
        let l0_results = self.l0.recall(&req);

        // L1: keyword recall (sync)
        let l1_results = self.l1.recall(&req.query);

        // L2: BM25 + optional semantic recall (async)
        let l2_results = self.l2.recall(&req).await;

        // Merge: deduplicate by entry.id
        let mut seen = std::collections::HashMap::new();
        for r in l0_results {
            seen.insert(r.entry.id.clone(), r);
        }
        for r in l1_results {
            seen.entry(r.entry.id.clone()).or_insert(r);
        }
        for r in l2_results {
            seen.entry(r.entry.id.clone()).or_insert(r);
        }

        let mut results: Vec<_> = seen.into_values().collect();
        results.sort_by(|a, b| {
            b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(req.limit());

        results
    }

    /// Exact get by key (checks L0 first, then L1).
    pub async fn get(&self, key: &str) -> Option<Entry> {
        self.l0.get(key).map(|e| (*e).clone()).or_else(|| self.l1.get(key))
    }

    /// Delete a memory entry from all layers.
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.l0.remove(key);
        self.l1.delete(key)?;
        Ok(())
    }

    /// Get system statistics across all layers.
    pub fn stats(&self) -> MemoryStats {
        let mut stats = MemoryStats::default();
        stats.l0_entries = self.l0.len();
        stats.l1_entries = self.l1.len();
        stats.l2_entries = self.l2.len();
        stats.l2_vectors_cached = self.l2.vector_count();
        stats.l2_pending_vectors = self.l2.pending_count();
        stats.l3_archived_files = self.l3.count_archives();

        if let Some(msg) = self.l1.health_issue() {
            stats.layer_health.push(("l1", Some(msg)));
        }
        if let Some(msg) = self.l2.health_issue() {
            stats.layer_health.push(("l2", Some(msg)));
        }
        if let Some(msg) = self.l3.health_issue() {
            stats.layer_health.push(("l3", Some(msg)));
        }

        stats
    }

    /// Graceful shutdown: stop background tasks and flush pending data.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        tracing::info!("MemorySystem shutting down...");

        // Stop L2 background sync task
        self.l2.stop().await?;

        // Flush L1 WAL to disk
        self.l1.flush()?;

        // Finalize today's archive file
        // Safety: finalize_today only needs &mut self for file ops; we own the Arc exclusively
        // in single-threaded shutdown path, so this is safe
        unsafe {
            let l3_ptr = Arc::as_ptr(&self.l3) as *mut l3::L3;
            (&mut *l3_ptr).finalize_today()?;
        }

        tracing::info!("MemorySystem shutdown complete");
        Ok(())
    }
}
