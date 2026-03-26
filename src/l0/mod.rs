//! L0 Working Memory — in-process, session-scoped, eviction-driven cache.
//!
//! # Design
//! - **Medium**: DashMap (in-process, lock-free)
//! - **Persistence**: None — wiped on process restart
//! - **Latency target**: < 1 µs per access
//! - **Eviction**: LRU when capacity is reached or entry expires
//! - **Capacity**: configurable via `MEMORY_L0_MAX_ENTRIES` env var (default 10 000)

use std::cmp::Ordering;

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;

use crate::common::{Entry, Importance, Layer, RecallRequest, RecallResult};

/// Default maximum entries for L0 working memory.
const DEFAULT_L0_MAX_ENTRIES: usize = 10_000;

/// Environment variable name for the L0 capacity override.
const ENV_L0_MAX_ENTRIES: &str = "MEMORY_L0_MAX_ENTRIES";

/// LRU order value that signals "most recently used".
///
/// Wraps a u64 counter that monotonically increases on every access.
/// The entry with the **lowest** counter value is the oldest (evicted first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LruOrder(u64);

impl LruOrder {
    fn new(counter: u64) -> Self {
        Self(counter)
    }
    fn as_u64(&self) -> u64 {
        self.0
    }
}

/// An entry in the LRU index.
///
/// Stores the LRU order value alongside the entry itself so that
/// the index can be updated without re-reading the store.
#[derive(Debug, Clone)]
struct LruEntry {
    /// Monotonically increasing access order (lower = older).
    order: LruOrder,
    /// Pointer into the DashMap store — kept so we know which key to
    /// look up when evicting.
    _phantom: std::marker::PhantomData<Arc<Entry>>,
}

/// Thread-safe, in-process working memory with LRU eviction.
pub struct WorkingMemory {
    /// Primary key-value store — lock-free DashMap for < 1 µs reads.
    store: DashMap<String, Arc<Entry>>,

    /// LRU index: key → (access_order_counter, _).
    /// Protected by a single RwLock so we can atomically bump the order
    /// and find the oldest entry for eviction.
    lru_index: RwLock<HashMap<String, LruEntry>>,

    /// Monotonically increasing counter assigned to each new / re-accessed
    /// entry.  Wraps at u64::MAX (practically never happens).
    access_counter: RwLock<u64>,

    /// Maximum number of entries before eviction kicks in.
    capacity: usize,
}

impl WorkingMemory {
    // ─────────────────────────────────────────────────────────────
    // Constructors
    // ─────────────────────────────────────────────────────────────

    /// Create a new `WorkingMemory` with the default capacity
    /// (`MEMORY_L0_MAX_ENTRIES` env var or 10 000).
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(
            std::env::var(ENV_L0_MAX_ENTRIES)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_L0_MAX_ENTRIES),
        )
    }

    /// Create a new `WorkingMemory` with an explicit capacity.
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            store: DashMap::with_capacity(capacity),
            lru_index: RwLock::new(HashMap::with_capacity(capacity)),
            access_counter: RwLock::new(0),
            capacity: capacity.max(1),
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Core accessors
    // ─────────────────────────────────────────────────────────────

    /// Retrieve an entry by key, updating its LRU position.
    ///
    /// Returns `None` if the key is not present.
    ///
    /// # Complexity
    /// O(1) average — single DashMap lookup + single RwLock write.
    #[inline]
    pub fn get(&self, key: &str) -> Option<Arc<Entry>> {
        // Try the store first (fast path — no lock needed on DashMap).
        let entry = self.store.get(key)?.clone();

        // Update LRU order.
        self.bump_lru(key);

        // Note: last_accessed timestamp on Entry is updated eventually on the
        // next write of that key.  For a working cache this eventual accuracy
        // is acceptable.  DashMap::get returns a Ref not an Arc, so we return
        // the cloned Arc immediately and defer the timestamp touch.
        Some(entry)
    }

    /// Insert or overwrite an entry.
    ///
    /// If the key already exists its LRU order is refreshed.
    /// If capacity is exceeded the least-recently-used entry is evicted
    /// before insertion.
    ///
    /// # Complexity
    /// O(1) amortised — DashMap write + RwLock write + optional eviction.
    pub fn write(&self, key: impl Into<String>, entry: Entry) {
        let key = key.into();

        // Evict oldest if at capacity (account for potential overwrite).
        let current_len = {
            let guard = self.lru_index.read();
            guard.len()
        };
        if current_len >= self.capacity && !self.store.contains_key(&key) {
            self.evict_one();
        }

        // Store the entry.
        let arc_entry = Arc::new(entry);
        self.store.insert(key.clone(), arc_entry);

        // Register / refresh in LRU index.
        self.register_lru(&key);
    }

    /// Remove an entry completely (store + LRU index).
    ///
    /// # Complexity
    /// O(1) — DashMap::remove + RwLock guard write.
    #[inline]
    pub fn remove(&self, key: &str) -> bool {
        // Remove from store.
        let removed = self.store.remove(key).is_some();

        // Remove from LRU index.
        if removed {
            let mut guard = self.lru_index.write();
            guard.remove(key);
        }

        removed
    }

    /// Remove every entry whose key is listed in `session_keys`.
    ///
    /// Called at session end so that session-scoped entries are not left
    /// behind in the working memory.
    ///
    /// # Complexity
    /// O(n) where n = session_keys.len().
    pub fn clear_session(&self, session_keys: &[String]) {
        let mut guard = self.lru_index.write();
        for key in session_keys {
            self.store.remove(key);
            guard.remove(key);
        }
    }

    /// Search entries by simple prefix / keyword match in value text.
    ///
    /// No external vector service required — runs entirely in-process.
    /// Results are sorted by a relevance score (0.0 – 1.0) and limited
    /// to `req.limit()` entries.
    ///
    /// Scoring strategy:
    /// 1. Exact full-text match  → 1.0
    /// 2. All query tokens found (case-insensitive) → 0.8
    /// 3. Prefix / substring match → 0.6
    /// 4. Any token matched → 0.4
    /// 5. No match → excluded
    ///
    /// Ties are broken by LRU order (more recently accessed first).
    pub fn recall(&self, req: &RecallRequest) -> Vec<RecallResult> {
        let query_lower = req.query.to_lowercase();
        let tokens: Vec<&str> = query_lower.split_whitespace().collect();
        let limit = req.limit();

        let guard = self.lru_index.read();

        let mut scored: Vec<_> = self
            .store
            .iter()
            .filter_map(|kv| {
                let entry: &Entry = kv.value().as_ref();
                let score = compute_relevance_score(&entry.value, &tokens);

                if score > 0.0 {
                    // LRU recency: how recently accessed relative to others.
                    // Higher order = more recent = higher recency bonus.
                    let lru_order = guard.get(&entry.key).map(|e| e.order.as_u64()).unwrap_or(0);

                    Some((score, lru_order, entry.clone()))
                } else {
                    None
                }
            })
            .collect();

        // Sort: highest score first; tie-break by recency (higher lru_order = newer).
        scored.sort_unstable_by(|a, b| {
            let (score_a, lru_a, _) = a;
            let (score_b, lru_b, _) = b;
            match score_a.partial_cmp(score_b) {
                Some(Ordering::Equal) | None => lru_b.cmp(lru_a),
                Some(c) => c,
            }
        });

        scored
            .into_iter()
            .take(limit)
            .map(|(_score, _lru_order, entry)| RecallResult::new(entry, _score, "L0"))
            .collect()
    }

    // ─────────────────────────────────────────────────────────────
    // Metadata
    // ─────────────────────────────────────────────────────────────

    /// Total number of entries currently held.
    #[inline]
    pub fn len(&self) -> usize {
        self.lru_index.read().len()
    }

    /// `true` when the store contains no entries.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return a snapshot of current L0 statistics.
    #[inline]
    pub fn stats(&self) -> L0Stats {
        let guard = self.lru_index.read();
        L0Stats {
            entries: guard.len(),
            capacity: self.capacity,
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Private helpers
    // ─────────────────────────────────────────────────────────────

    /// Increment the access counter and assign the new value to `key`
    /// in the LRU index.  Creates the index entry if absent.
    #[inline]
    fn bump_lru(&self, key: &str) {
        let order = {
            let mut counter_guard = self.access_counter.write();
            let next = counter_guard.wrapping_add(1);
            *counter_guard = next;
            next
        };

        let mut guard = self.lru_index.write();
        guard.insert(
            key.to_string(),
            LruEntry {
                order: LruOrder::new(order),
                _phantom: std::marker::PhantomData,
            },
        );
    }

    /// Register a newly inserted (or re-inserted) key in the LRU index
    /// with a fresh access order.
    #[inline]
    fn register_lru(&self, key: &str) {
        let order = {
            let mut counter_guard = self.access_counter.write();
            let next = counter_guard.wrapping_add(1);
            *counter_guard = next;
            next
        };

        let mut guard = self.lru_index.write();
        guard.insert(
            key.to_string(),
            LruEntry {
                order: LruOrder::new(order),
                _phantom: std::marker::PhantomData,
            },
        );
    }

    /// Evict the single least-recently-used entry.
    /// Caller is responsible for holding no lock when calling this
    /// (we acquire lru_index write internally).
    fn evict_one(&self) {
        let key_to_evict = {
            let mut guard = self.lru_index.write();
            // Find the entry with the smallest LRU order (oldest).
            guard
                .iter()
                .min_by_key(|(_, v)| v.order)
                .map(|(k, _)| k.clone())
        };

        if let Some(key) = key_to_evict {
            self.store.remove(&key);
            let mut guard = self.lru_index.write();
            guard.remove(&key);
        }
    }
}

impl Default for WorkingMemory {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────
// Scoring helpers
// ─────────────────────────────────────────────────────────────────

/// Compute a relevance score for a single entry given a list of
/// lower-case query tokens.
///
/// Returns a score in [0.0, 1.0].  0.0 means no match.
fn compute_relevance_score(value: &str, tokens: &[&str]) -> f32 {
    if tokens.is_empty() {
        return 0.0;
    }

    let value_lower = value.to_lowercase();

    // 1. Exact full-string match.
    if value_lower == tokens.join(" ") {
        return 1.0;
    }

    let mut matched_tokens = 0usize;
    let mut prefix_matched = 0usize;
    let mut _substring_matched = 0usize;

    for token in tokens {
        if value_lower.contains(token) {
            matched_tokens += 1;

            // Distinguish prefix vs anywhere substring.
            if value_lower.starts_with(token) {
                prefix_matched += 1;
            } else {
                _substring_matched += 1;
            }
        }
    }

    if matched_tokens == tokens.len() {
        // All tokens matched somewhere.
        0.8
    } else if matched_tokens > 0 {
        // Partial token match.
        let ratio = matched_tokens as f32 / tokens.len() as f32;
        if prefix_matched > 0 {
            0.6 + 0.1 * ratio
        } else {
            0.4 + 0.2 * ratio
        }
    } else {
        0.0
    }
}

// ─────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────

/// Immutable snapshot of L0 statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L0Stats {
    /// Current number of stored entries.
    pub entries: usize,
    /// Configured maximum capacity.
    pub capacity: usize,
}

impl std::fmt::Display for L0Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "L0 {{ entries: {}/{} }}", self.entries, self.capacity)
    }
}

// ─────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Entry, Importance, Layer};
    use std::sync::Arc;

    fn dummy_entry(key: &str, value: &str) -> Entry {
        Entry::new(
            key.into(),
            value.into(),
            Importance::Normal,
            vec![],
            "test-agent".into(),
            Layer::Private,
        )
    }

    #[test]
    fn test_write_and_get() {
        let mem = WorkingMemory::with_capacity(100);
        let entry = dummy_entry("k1", "hello world");
        mem.write("k1", entry);

        let retrieved = mem.get("k1");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().value, "hello world");
    }

    #[test]
    fn test_get_nonexistent() {
        let mem = WorkingMemory::with_capacity(100);
        assert!(mem.get("nonexistent").is_none());
    }

    #[test]
    fn test_remove() {
        let mem = WorkingMemory::with_capacity(100);
        mem.write("k1", dummy_entry("k1", "v1"));
        assert!(mem.remove("k1"));
        assert!(mem.get("k1").is_none());
    }

    #[test]
    fn test_remove_unknown() {
        let mem = WorkingMemory::with_capacity(100);
        assert!(!mem.remove("unknown"));
    }

    #[test]
    fn test_lru_eviction() {
        let mem = WorkingMemory::with_capacity(3);
        for i in 0..3 {
            mem.write(
                format!("k{}", i),
                dummy_entry(&format!("k{}", i), &format!("v{}", i)),
            );
        }

        // Access k0 to make it most recent.
        mem.get("k0");

        // Insert k3, which should evict k1 (least recently used before k0 was bumped).
        mem.write("k3", dummy_entry("k3", "v3"));

        assert!(
            mem.get("k0").is_some(),
            "k0 should still be present (was accessed)"
        );
        assert!(
            mem.get("k1").is_none(),
            "k1 should be evicted (oldest before k0 bump)"
        );
        assert!(mem.get("k2").is_some());
        assert!(mem.get("k3").is_some());
    }

    #[test]
    fn test_clear_session() {
        let mem = WorkingMemory::with_capacity(100);
        mem.write("session:abc:k1", dummy_entry("session:abc:k1", "v1"));
        mem.write("session:abc:k2", dummy_entry("session:abc:k2", "v2"));
        mem.write("global:k1", dummy_entry("global:k1", "v3"));

        mem.clear_session(&["session:abc:k1".into(), "session:abc:k2".into()]);

        assert!(mem.get("global:k1").is_some());
        assert!(mem.get("session:abc:k1").is_none());
        assert!(mem.get("session:abc:k2").is_none());
    }

    #[test]
    fn test_len_and_stats() {
        let mem = WorkingMemory::with_capacity(100);
        assert_eq!(mem.len(), 0);
        assert!(mem.is_empty());

        mem.write("k1", dummy_entry("k1", "v1"));
        mem.write("k2", dummy_entry("k2", "v2"));
        assert_eq!(mem.len(), 2);
        assert!(!mem.is_empty());

        let stats = mem.stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.capacity, 100);
    }

    #[test]
    fn test_recall_exact_match() {
        let mem = WorkingMemory::with_capacity(100);
        mem.write("k1", dummy_entry("k1", "the quick brown fox"));
        mem.write("k2", dummy_entry("k2", "the lazy dog"));
        mem.write("k3", dummy_entry("k3", "something else"));

        let results = mem.recall(&RecallRequest {
            query: "quick brown fox".into(),
            keys: None,
            agent_id: None,
            tags: None,
            limit: Some(10),
            layer: None,
            semantic: false,
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.key, "k1");
        assert!((results[0].score - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn test_recall_partial_match() {
        let mem = WorkingMemory::with_capacity(100);
        mem.write("k1", dummy_entry("k1", "rust programming language"));
        mem.write("k2", dummy_entry("k2", "python scripting"));
        mem.write("k3", dummy_entry("k3", "go concurrent programming"));

        let results = mem.recall(&RecallRequest {
            query: "programming".into(),
            keys: None,
            agent_id: None,
            tags: None,
            limit: Some(10),
            layer: None,
            semantic: false,
        });

        assert_eq!(results.len(), 2);
        // Both match; order not guaranteed but scores should be > 0.
        for r in &results {
            assert!(r.score > 0.0);
        }
    }

    #[test]
    fn test_recall_limit() {
        let mem = WorkingMemory::with_capacity(100);
        for i in 0..20 {
            mem.write(
                format!("k{}", i),
                dummy_entry(&format!("k{}", i), &format!("value {}", i)),
            );
        }

        let results = mem.recall(&RecallRequest {
            query: "value".into(),
            keys: None,
            agent_id: None,
            tags: None,
            limit: Some(5),
            layer: None,
            semantic: false,
        });

        assert_eq!(results.len(), 5);
    }

    #[test]
    fn test_overwrite_refreshes_lru() {
        let mem = WorkingMemory::with_capacity(3);

        mem.write("k1", dummy_entry("k1", "v1"));
        mem.write("k2", dummy_entry("k2", "v2"));

        // Access k1 so it becomes most recent.
        mem.get("k1");

        // Sleep-timer not needed in tests; we verify via eviction order.
        // Insert k3, which should evict k2 (least recent).
        mem.write("k3", dummy_entry("k3", "v3"));

        assert!(mem.get("k1").is_some());
        assert!(mem.get("k2").is_none()); // evicted
        assert!(mem.get("k3").is_some());
    }

    #[test]
    fn test_capacity_respects_minimum() {
        let mem = WorkingMemory::with_capacity(0);
        let stats = mem.stats();
        assert_eq!(stats.capacity, 1, "capacity should be at least 1");
    }

    #[test]
    fn test_relevance_score_exact() {
        let tokens = &["hello".to_lowercase()];
        let score = compute_relevance_score("hello", tokens);
        assert!((score - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_relevance_score_all_tokens() {
        let tokens = &["quick", "brown"];
        let score = compute_relevance_score("the quick brown fox", tokens);
        assert!((score - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn test_relevance_score_partial() {
        let tokens = &["quick", "red"];
        let score = compute_relevance_score("the quick fox", tokens);
        assert!(score > 0.0 && score < 0.8);
    }

    #[test]
    fn test_relevance_score_none() {
        let tokens = &["zebra"];
        let score = compute_relevance_score("the quick fox", tokens);
        assert!((score - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_l0_stats_display() {
        let stats = L0Stats {
            entries: 42,
            capacity: 100,
        };
        let s = format!("{}", stats);
        assert!(s.contains("42"));
        assert!(s.contains("100"));
    }

    #[test]
    fn test_default_capacity_from_env_not_set() {
        // When env var is not set the default should be used.
        let cap = std::env::var(ENV_L0_MAX_ENTRIES)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_L0_MAX_ENTRIES);
        assert_eq!(cap, DEFAULT_L0_MAX_ENTRIES);
    }
}
