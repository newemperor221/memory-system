//! Shared types for the four-layer memory system

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Importance level of a memory entry
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(i32)]
pub enum Importance {
    Low = 1,
    Normal = 2,
    High = 3,
    Critical = 4,
}

impl Default for Importance {
    fn default() -> Self {
        Self::Normal
    }
}

impl std::fmt::Display for Importance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Normal => write!(f, "normal"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

impl From<i32> for Importance {
    fn from(v: i32) -> Self {
        match v {
            4 => Self::Critical,
            3 => Self::High,
            2 | _ => Self::Normal,
            1 => Self::Low,
        }
    }
}

impl From<&str> for Importance {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "critical" => Self::Critical,
            "high" => Self::High,
            "normal" => Self::Normal,
            "low" => Self::Low,
            _ => Self::Normal,
        }
    }
}

/// Access layer: Private (agent-only) or Public (all agents)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layer {
    Private,
    Public,
}

impl Default for Layer {
    fn default() -> Self {
        Self::Private
    }
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Private => write!(f, "private"),
            Self::Public => write!(f, "public"),
        }
    }
}

impl From<&str> for Layer {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "public" => Self::Public,
            _ => Self::Private,
        }
    }
}

impl Layer {
    pub fn prefix(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

/// A single memory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Unique identifier (UUID v4)
    pub id: String,
    /// Semantic key: <layer>:<category>:<subcategory>:<identifier>
    pub key: String,
    /// Memory content
    pub value: String,
    /// Importance level
    pub importance: Importance,
    /// Custom tags for filtering
    pub tags: Vec<String>,
    /// Source agent ID
    pub source: String,
    /// Access layer
    pub layer: Layer,
    /// Creation time
    pub created_at: DateTime<Utc>,
    /// Last access time (auto-updated on read)
    pub last_accessed: DateTime<Utc>,
    /// Expiration time (None = never expires)
    pub expires_at: Option<DateTime<Utc>>,
}

impl Entry {
    /// Create a new entry with generated UUID and current timestamp
    pub fn new(
        key: String,
        value: String,
        importance: Importance,
        tags: Vec<String>,
        source: String,
        layer: Layer,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            key,
            value,
            importance,
            tags,
            source,
            layer,
            created_at: now,
            last_accessed: now,
            expires_at: None,
        }
    }
}

/// Recall request parameters
#[derive(Debug, Clone, Deserialize)]
pub struct RecallRequest {
    /// Search query (keywords or natural language)
    pub query: String,
    /// Exact keys to prioritize (optional)
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// Filter by source agent (optional)
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Filter by tags (AND logic)
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Result limit (default 10)
    #[serde(default)]
    pub limit: Option<usize>,
    /// Filter by layer (default: all)
    #[serde(default)]
    pub layer: Option<Layer>,
    /// Use semantic vector search (default false = BM25 only)
    #[serde(default)]
    pub semantic: bool,
}

impl RecallRequest {
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(10)
    }
}

/// A single recall result
#[derive(Debug, Clone, Serialize)]
pub struct RecallResult {
    pub entry: Entry,
    /// Combined similarity score (0.0 ~ 1.0)
    pub score: f32,
    /// Source layer: "L0" / "L1" / "L2:BM25" / "L2:Gemini" / "L2:Hybrid"
    pub from_layer: String,
}

impl RecallResult {
    pub fn new(entry: Entry, score: f32, from_layer: impl Into<String>) -> Self {
        Self {
            entry,
            score,
            from_layer: from_layer.into(),
        }
    }
}

/// System statistics across all layers
#[derive(Debug, Clone, Serialize)]
pub struct MemoryStats {
    pub l0_entries: usize,
    pub l1_entries: usize,
    pub l2_entries: usize,
    pub l2_vectors_cached: usize,
    pub l2_pending_vectors: usize,
    pub l3_archived_files: usize,
    /// Health status per layer: None = healthy, Some(msg) = degraded
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub layer_health: Vec<(&'static str, Option<String>)>,
}

impl Default for MemoryStats {
    fn default() -> Self {
        Self {
            l0_entries: 0,
            l1_entries: 0,
            l2_entries: 0,
            l2_vectors_cached: 0,
            l2_pending_vectors: 0,
            l3_archived_files: 0,
            layer_health: Vec::new(),
        }
    }
}


// ---------------------------------------------------------------------------
// L1 Event Bus — unified event type across all layers
// ---------------------------------------------------------------------------

use crossbeam_channel as channel;

/// Unified event type emitted by L1 and consumed by L2/L3.
/// Defined once here to avoid circular type definitions.
#[derive(Debug, Clone)]
pub enum L1Event {
    /// New entry written or existing key replaced.
    /// seq is the monotonic sequence number from L1.
    Upsert { key: String, entry: Entry, seq: u64 },
    /// Entry deleted.
    Delete { key: String, seq: u64 },
}

/// Minimal interface that L2 and L3 use to subscribe to L1 events.
pub trait L1Consumer: Send + Sync {
    /// Returns a receiver for L1 events. Returns None if already taken.
    fn take_event_rx(&self) -> Option<channel::Receiver<L1Event>>;

    /// Full scan of all entries (used by L2 rebuild).
    fn full_scan(&self) -> Vec<Entry>;
}
