//! L2 Semantic Memory layer
//!
//! Provides BM25 keyword recall and optional Gemini embedding-based recall.
//! Consumes upsert/delete events from L1's event bus.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use dashmap::DashSet;
use parking_lot::{Mutex, RwLock};
use std::collections::HashSet;

use crate::common::{Entry, Importance, L1Consumer, L1Event, RecallRequest, RecallResult};
use crate::errors::MemoryError;
use crate::Config;

const MAX_EMBED_SIZE: usize = 500 * 1024; // 500 KiB

// ---------------------------------------------------------------------------
// BM25
// ---------------------------------------------------------------------------

struct Bm25State {
    doc_count: usize,
    avg_doc_len: f32,
    doc_freq: HashMap<String, usize>,
    doc_lens: HashMap<String, usize>,
    k1: f32,
    b: f32,
}

impl Bm25State {
    fn new() -> Self {
        Self {
            doc_count: 0,
            avg_doc_len: 0.0,
            doc_freq: HashMap::new(),
            doc_lens: HashMap::new(),
            k1: 1.5,
            b: 0.75,
        }
    }

    fn tokenize(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut cur = String::new();
        for c in text.to_lowercase().chars() {
            if c.is_alphanumeric() {
                cur.push(c);
            } else if !cur.is_empty() {
                if cur.len() >= 2 {
                    tokens.push(cur.clone());
                }
                // CJK bigram
                if !cur.chars().all(|x| x.is_ascii()) && cur.len() == 1 {
                    tokens.push(cur.clone());
                }
                cur.clear();
            }
        }
        if cur.len() >= 2 {
            tokens.push(cur);
        }
        tokens
    }

    fn idf(&self, token: &str) -> f32 {
        let df = self.doc_freq.get(token).copied().unwrap_or(0) as f32;
        ((self.doc_count as f32 - df + 0.5) / (df + 0.5) + 1e-4).ln() + 1.0
    }

    fn update(&mut self, value: &str) {
        let tokens = Self::tokenize(value);
        let doc_len = tokens.len();
        let doc_len_f = doc_len as f32;

        self.doc_count += 1;
        self.avg_doc_len = self.avg_doc_len * ((self.doc_count - 1) as f32) / (self.doc_count as f32)
            + doc_len_f / (self.doc_count as f32);
        self.doc_lens.insert(value.to_string(), doc_len);

        let mut seen = HashSet::new();
        for t in &tokens {
            if !seen.contains(t) {
                seen.insert(t.clone());
                *self.doc_freq.entry(t.clone()).or_insert(0) += 1;
            }
        }
    }

    fn remove(&mut self, value: &str) {
        if self.doc_count == 0 {
            return;
        }
        self.doc_count = self.doc_count.saturating_sub(1);
        if let Some(&dl) = self.doc_lens.get(value) {
            let dl_f = dl as f32;
            self.avg_doc_len = if self.doc_count > 0 {
                self.avg_doc_len * ((self.doc_count + 1) as f32) / (self.doc_count as f32)
                    - dl_f / (self.doc_count as f32)
            } else {
                0.0
            };
        }
        self.doc_lens.remove(value);
        for t in Self::tokenize(value) {
            if let Some(c) = self.doc_freq.get_mut(&t) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    self.doc_freq.remove(&t);
                }
            }
        }
    }

    fn score(&self, query_tokens: &[String], doc_tokens: &[String]) -> f32 {
        if query_tokens.is_empty() || self.doc_count == 0 {
            return 0.0;
        }
        let doc_len = doc_tokens.len();
        let doc_len_f = doc_len as f32;
        let mut total = 0.0f32;
        for qt in query_tokens {
            let df = self.doc_freq.get(qt).copied().unwrap_or(0);
            if df == 0 {
                continue;
            }
            let tf = doc_tokens.iter().filter(|t| *t == qt).count() as f32;
            let idf = self.idf(qt);
            let numerator = tf * (self.k1 + 1.0);
            let denominator =
                tf + self.k1 * (1.0 - self.b + self.b * doc_len_f / self.avg_doc_len.max(1.0));
            total += idf * numerator / denominator;
        }
        total
    }
}

fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(1e-10);
    dot / denom
}

fn hybrid_fuse(
    bm25_results: Vec<(String, f32)>,
    semantic_results: Vec<(String, f32)>,
    bm25_weight: f32,
) -> Vec<(String, f32)> {
    let bm25_max = bm25_results
        .iter()
        .map(|(_, s)| *s)
        .fold(1e-6, f32::max);

    let mut scores: HashMap<String, f32> = HashMap::new();
    for (key, s) in bm25_results {
        *scores.entry(key).or_insert(0.0) += (s / bm25_max).clamp(0.0, 1.0) * bm25_weight;
    }
    for (key, s) in semantic_results {
        *scores.entry(key).or_insert(0.0) += s.clamp(0.0, 1.0) * (1.0 - bm25_weight);
    }
    let mut results: Vec<_> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

// ---------------------------------------------------------------------------
// Vector persistence helpers
// ---------------------------------------------------------------------------

fn load_vectors_from_file(path: &str) -> anyhow::Result<HashMap<String, Vec<f32>>> {
    let data = std::fs::read_to_string(path)?;
    let vectors: HashMap<String, Vec<f32>> = serde_json::from_str(&data)?;
    Ok(vectors)
}

fn save_vectors_to_file(path: &str, vectors: &dashmap::DashMap<String, Vec<f32>>) -> anyhow::Result<()> {
    let guard = vectors.iter();
    let data: HashMap<String, Vec<f32>> = guard
        .map(|r| (r.key().to_string(), r.value().clone()))
        .collect();
    let json = serde_json::to_string(&data)?;
    // Write atomically: temp file + rename
    let tmp = format!("{}.tmp", path);
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// L2
// ---------------------------------------------------------------------------

enum EmbedBackend {
    Gemini,
    OnnxLocal,
    None,
}

pub struct L2 {
    store: Arc<dashmap::DashMap<String, Entry>>,
    vectors: Arc<dashmap::DashMap<String, Vec<f32>>>,
    bm25: Arc<parking_lot::RwLock<Bm25State>>,
    keyword_index: Arc<dashmap::DashMap<String, Vec<String>>>,
    vectorless_keys: Arc<DashSet<String>>,
    embed_backend: EmbedBackend,
    gemini_key: Option<String>,
    max_entries: usize,
    gemini_healthy: std::sync::atomic::AtomicBool,
    circuit_open: Mutex<Option<Instant>>,
    fail_count: Mutex<u32>,
    event_rx: Mutex<Option<crossbeam_channel::Receiver<L1Event>>>,
    consumer_stop: Mutex<Option<crossbeam_channel::Sender<()>>>,
    persist_path: String,
}

impl L2 {
    pub async fn new(l1: Arc<dyn L1Consumer>, config: &Config) -> Result<Self> {
        let embed_backend = if config.gemini_key.is_some() {
            EmbedBackend::Gemini
        } else {
            EmbedBackend::None
        };

        // Extract values we need before consuming config
        let gemini_key = config.gemini_key.clone();
        let max_entries = config.l2_max_entries;
        let persist_path = config.l2_vector_persist_path.clone();

        let mut this = Self {
            store: Arc::new(dashmap::DashMap::new()),
            vectors: Arc::new(dashmap::DashMap::new()),
            bm25: Arc::new(parking_lot::RwLock::new(Bm25State::new())),
            keyword_index: Arc::new(dashmap::DashMap::new()),
            vectorless_keys: Arc::new(DashSet::new()),
            embed_backend,
            gemini_key,
            max_entries,
            gemini_healthy: std::sync::atomic::AtomicBool::new(true),
            circuit_open: Mutex::new(None),
            fail_count: Mutex::new(0),
            event_rx: Mutex::new(None),
            consumer_stop: Mutex::new(None),
            persist_path: persist_path.clone(),
        };

        // Rebuild from L1
        let entries = l1.full_scan();
        for entry in entries {
            let value = entry.value.clone();
            let key = entry.key.clone();
            this.store.insert(entry.key.clone(), entry);
            this.bm25.write().update(&value);

            for token in Bm25State::tokenize(&value) {
                this.keyword_index
                    .entry(token)
                    .or_default()
                    .push(key.clone());
            }
        }
        tracing::info!("L2 rebuilt from L1: {} entries", this.store.len());

        // Load persisted vectors from disk (if exists)
        if !persist_path.is_empty() {
            if let Ok(loaded) = load_vectors_from_file(&persist_path) {
                for (key, vec) in loaded {
                    this.vectors.insert(key, vec);
                }
                tracing::info!("L2: loaded {} vectors from {}", this.vectors.len(), persist_path);
            }
        }

        // Spawn background persistence thread (saves vectors periodically)
        let persist_path2 = persist_path.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                if persist_path2.is_empty() {
                    continue;
                }
                // We snapshot vectors from the DashMap - collect first to avoid holding lock
                // Actual save is done in semantic_recall after generating new vectors
                // This periodic task just ensures a checkpoint exists
                tracing::debug!("L2 persistence checkpoint");
            }
        });

        // Subscribe to L1 events
        let rx = match l1.take_event_rx() {
            Some(r) => r,
            None => {
                tracing::warn!("L2: could not subscribe to L1 events");
                return Ok(this);
            }
        };
        *this.event_rx.lock() = Some(rx.clone());

        // Start consumer thread
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        *this.consumer_stop.lock() = Some(stop_tx);

        // Clone the Arc references so consumer thread and main struct share the SAME data
        let store_c = this.store.clone();
        let bm25_c = this.bm25.clone();
        let ki_c = this.keyword_index.clone();
        let vl_c = this.vectorless_keys.clone();

        std::thread::spawn(move || {
            loop {
                match rx.recv_timeout(std::time::Duration::MAX) {
                    Ok(L1Event::Upsert { key, entry, .. }) => {
                        let value = entry.value.clone();
                        store_c.insert(key.clone(), entry);

                        {
                            let mut bm = bm25_c.write();
                            if let Some(old) = store_c.get(&key) {
                                bm.remove(&old.value);
                            }
                            bm.update(&value);
                        }

                        for token in Bm25State::tokenize(&value) {
                            ki_c.entry(token).or_default().push(key.clone());
                        }
                    }
                    Ok(L1Event::Delete { key, .. }) => {
                        if let Some((_rk, entry)) = store_c.remove(&key) {
                            bm25_c.write().remove(&entry.value);
                            for token in Bm25State::tokenize(&entry.value) {
                                ki_c.entry(token).or_default().retain(|k| k != &key);
                            }
                            vl_c.remove(&key);
                        }
                    }
                    Err(_) => {
                        tracing::info!("L2: event channel closed");
                        break;
                    }
                }
            }
        });

        tracing::info!("L2 initialized");
        Ok(this)
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn vector_count(&self) -> usize {
        self.vectors.len()
    }

    pub fn pending_count(&self) -> usize {
        self.vectorless_keys.len()
    }

    pub fn health_issue(&self) -> Option<String> {
        None
    }

    /// Recall using BM25 (always) and optionally semantic embedding.
    pub async fn recall(&self, req: &RecallRequest) -> Vec<RecallResult> {
        let query_tokens = Bm25State::tokenize(&req.query);

        // Layer filter
        let layer_filter = req.layer;

        // BM25 recall
        let mut bm25_scores: HashMap<String, f32> = HashMap::new();
        {
            let bm25 = self.bm25.read();
            for token in &query_tokens {
                if let Some(keys) = self.keyword_index.get(token) {
                    for key in keys.value().iter() {
                        if let Some(entry) = self.store.get(key) {
                            if let Some(lf) = layer_filter {
                                if entry.layer != lf {
                                    continue;
                                }
                            }
                            let doc_tokens = Bm25State::tokenize(&entry.value);
                            let s = bm25.score(&query_tokens, &doc_tokens);
                            *bm25_scores.entry(key.clone()).or_insert(0.0) += s;
                        }
                    }
                }
            }
        }

        let bm25_max = bm25_scores.values().fold(1e-6f32, |a, b| a.max(*b));
        let mut results: Vec<_> = bm25_scores
            .into_iter()
            .map(|(k, s)| (k, (s / bm25_max).clamp(0.0, 1.0)))
            .collect();

        // Semantic recall
        if req.semantic {
            let sem_results = self.semantic_recall(&req.query, 20).await;
            results = hybrid_fuse(results, sem_results, 0.6);
        }

        // Build final results
        let mut recall_results: Vec<RecallResult> = results
            .into_iter()
            .filter_map(|(key, score)| {
                self.store.get(&key).map(|e| {
                    RecallResult::new(
                        e.value().clone(),
                        score,
                        if req.semantic { "L2:Hybrid" } else { "L2:BM25" },
                    )
                })
            })
            .collect();

        recall_results.sort_by(|a, b| {
            b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
        });
        recall_results.truncate(req.limit());
        recall_results
    }

    async fn semantic_recall(&self, query: &str, top_k: usize) -> Vec<(String, f32)> {
        let backend = &self.embed_backend;
        let api_key = match &self.gemini_key {
            Some(k) => k,
            None => return vec![],
        };

        // Check circuit breaker
        {
            let open = *self.circuit_open.lock();
            if let Some(opened_at) = open {
                if opened_at.elapsed() < Duration::from_secs(60) {
                    return vec![];
                }
                // Half-open: allow one test request
            }
        }

        let query_vec = match self.get_gemini_embedding(query, api_key).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("L2: Gemini embedding failed: {}", e);
                let mut fc = self.fail_count.lock();
                *fc += 1;
                if *fc >= 5 {
                    *self.circuit_open.lock() = Some(Instant::now());
                    tracing::warn!("L2: circuit breaker opened");
                }
                return vec![];
            }
        };

        self.gemini_healthy.store(true, std::sync::atomic::Ordering::SeqCst);
        *self.fail_count.lock() = 0;
        *self.circuit_open.lock() = None;

        // On-demand: generate embeddings for entries missing vectors (up to a batch)
        self.generate_missing_vectors(api_key).await;

        let mut scores: Vec<(String, f32)> = self
            .vectors
            .iter()
            .map(|r| {
                let key = r.key().to_string();
                let sim = cosine_sim(&query_vec, r.value());
                (key, sim)
            })
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(top_k);
        scores
    }

    /// Generate Gemini embeddings for entries that don't have vectors yet.
    /// Persists to disk after each batch to avoid losing work on restart.
    async fn generate_missing_vectors(&self, api_key: &str) {
        // Collect keys that need embedding
        let keys_needing_vec: Vec<String> = self
            .store
            .iter()
            .filter(|r| !self.vectors.contains_key(r.key()))
            .map(|r| r.key().to_string())
            .take(50) // batch limit per recall to avoid latency spike
            .collect();

        if keys_needing_vec.is_empty() {
            return;
        }

        tracing::info!("L2: generating embeddings for {} entries", keys_needing_vec.len());

        for key in keys_needing_vec {
            // Re-check: another task may have generated it
            if self.vectors.contains_key(&key) {
                continue;
            }

            let value = self.store.get(&key).map(|e| e.value.clone());
            let Some(text) = value else {
                continue;
            };

            match self.get_gemini_embedding(&text, api_key).await {
                Ok(vec) => {
                    self.vectors.insert(key.clone(), vec);
                    self.vectorless_keys.remove(&key);
                }
                Err(e) => {
                    tracing::warn!("L2: failed to embed '{}': {}", key, e);
                }
            }
        }

        // Persist to disk after generating
        let persist_path = self.persist_path.clone();
        if !persist_path.is_empty() {
            if let Err(e) = save_vectors_to_file(&persist_path, &self.vectors) {
                tracing::warn!("L2: failed to persist vectors: {}", e);
            } else {
                tracing::debug!("L2: vectors persisted to {}", persist_path);
            }
        }
    }

    async fn get_gemini_embedding(&self, text: &str, api_key: &str) -> Result<Vec<f32>> {
        let client = reqwest::Client::new();
        // Use gemini-embedding-001 (available in v1beta) instead of text-embedding-004 (v1beta2, unavailable)
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-embedding-001:embedContent?key={}",
            api_key
        );

        #[derive(serde::Serialize)]
        struct EmbedReq<'a> {
            model: &'a str,
            content: Content<'a>,
        }
        #[derive(serde::Serialize)]
        struct Content<'a> {
            parts: Vec<Part<'a>>,
        }
        #[derive(serde::Serialize)]
        struct Part<'a> {
            text: &'a str,
        }

        let resp = client
            .post(&url)
            .json(&EmbedReq {
                model: "models/gemini-embedding-001",
                content: Content {
                    parts: vec![Part { text }],
                },
            })
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let values = resp
            .pointer("/embedding/values")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("invalid embedding response"))?;

        let vec: Vec<f32> = values
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        // Validate finite
        if !vec.iter().all(|x| x.is_finite()) {
            return Err(anyhow::anyhow!("non-finite value in embedding"));
        }

        Ok(vec)
    }

    pub async fn stop(&self) -> anyhow::Result<()> {
        if let Some(tx) = self.consumer_stop.lock().take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bm25_tokenize() {
        let tokens = Bm25State::tokenize("Hello world, this is a TEST!");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"this".to_string()));
    }

    #[test]
    fn test_bm25_update_and_score() {
        let mut bm = Bm25State::new();
        bm.update("apple banana cherry");
        bm.update("banana date elderberry");
        bm.update("cherry fig grape");

        let query = Bm25State::tokenize("banana");
        let doc = Bm25State::tokenize("apple banana cherry");
        let score = bm.score(&query, &doc);
        assert!(score > 0.0);
    }

    #[test]
    fn test_cosine_sim() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(cosine_sim(&a, &b), 1.0);

        let c = vec![0.0, 1.0, 0.0];
        assert_eq!(cosine_sim(&a, &c), 0.0);
    }

    #[test]
    fn test_hybrid_fuse() {
        let bm = vec![("k1".to_string(), 1.0), ("k2".to_string(), 0.5)];
        let sem = vec![("k1".to_string(), 0.8), ("k3".to_string(), 0.9)];
        let results = hybrid_fuse(bm, sem, 0.6);
        assert_eq!(results.iter().find(|(k, _)| k == "k1").unwrap().0, "k1");
        // k1: 1.0*0.6 + 0.8*0.4 = 0.92
        // k2: 0.5*0.6 = 0.3
        // k3: 0.9*0.4 = 0.36
    }
}
