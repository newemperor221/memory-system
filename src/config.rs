//! Configuration loaded from environment variables

use std::env;

/// Memory system configuration
#[derive(Debug, Clone)]
pub struct Config {
    // L0
    pub l0_capacity: usize,

    // L1
    pub l1_path: String,
    pub l1_max_entries: usize,
    pub l1_ttl_secs: i64,
    pub l1_gc_interval_secs: u64,

    // L2
    pub l2_max_entries: usize,
    pub l2_pending_batch: usize,
    pub l2_sync_interval_secs: u64,
    pub l2_embed_batch_size: usize,
    pub l2_vector_persist_path: String,

    // L3
    pub l3_archive_dir: String,
    pub l3_archive_after_days: i64,
    pub l3_gc_interval_secs: u64,

    // API
    pub api_key: Option<String>,
    pub listen: String,

    // Gemini
    pub gemini_key: Option<String>,

    // Paths
    pub workspace_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // L0
            l0_capacity: env_var_or("MEMORY_L0_MAX_ENTRIES", 10_000),

            // L1
            l1_path: env_var_or("MEMORY_L1_PATH", "/tmp/memory_v2_l1".to_string()),
            l1_max_entries: env_var_or("MEMORY_L1_MAX_ENTRIES", 50_000),
            l1_ttl_secs: env_var_or("MEMORY_L1_MAX_AGE_SECS", 604_800), // 7 days
            l1_gc_interval_secs: env_var_or("MEMORY_L1_GC_INTERVAL_SECS", 3600),

            // L2
            l2_max_entries: env_var_or("MEMORY_L2_MAX_ENTRIES", 100_000),
            l2_pending_batch: env_var_or("MEMORY_L2_PENDING_BATCH", 500),
            l2_sync_interval_secs: env_var_or("MEMORY_L2_SYNC_INTERVAL_SECS", 300),
            l2_embed_batch_size: env_var_or("MEMORY_L2_EMBED_BATCH_SIZE", 50),
            l2_vector_persist_path: env_var_or(
                "MEMORY_L2_VECTOR_PERSIST_PATH",
                "/tmp/memory_v2_l2_vectors.json".to_string(),
            ),

            // L3
            l3_archive_dir: env_var_or("MEMORY_L3_ARCHIVE_DIR", "memory-archive".to_string()),
            l3_archive_after_days: env_var_or("MEMORY_L3_ARCHIVE_AFTER_DAYS", 30),
            l3_gc_interval_secs: env_var_or("MEMORY_L3_GC_INTERVAL_SECS", 86400),

            // API
            api_key: std::env::var("MEMORY_API_KEY").ok(),
            listen: env_var_or("MEMORY_LISTEN", "127.0.0.1:7891".to_string()),

            // Gemini
            gemini_key: std::env::var("GEMINI_EMBEDDINGS_TOKEN").ok(),

            // Workspace
            workspace_path: env_var_or("WORKSPACE", ".".to_string()),
        }
    }
}

fn env_var_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
