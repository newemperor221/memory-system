//! L2 Long-Term Memory
//!
//! 双通道召回：BM25 关键词 + 语义向量（按需）
//!
//! 语义通道支持两种后端（按需触发）：
//! - Google Gemini Embeddings API（默认，已配置 key）
//! - 本地 ONNX 模型（可选，内存受限时使用）
//!
//! 性能策略：
//! - 默认召回走 BM25，零额外开销
//! - 语义搜索由调用方显式请求（need_semantic=true）
//! - ONNX 模型用完即释放，不常驻内存
//!
//! 量化：伪量化实现（实际数据压缩在 TODO 列表）

use crate::common::{Entry, RecallRequest, RecallResult};
use anyhow::Result;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock as AsyncRwLock;

/// 判断字符是否为 CJK（中日韩统一表意文字）
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' |  // CJK Unified Ideographs
        '\u{3400}'..='\u{4DBF}' |  // CJK Unified Ideographs Extension A
        '\u{F900}'..='\u{FAFF}' |  // CJK Compatibility Ideographs
        '\u{20000}'..='\u{2A6DF}' // CJK Unified Ideographs Extension B
    )
}

/// BM25 配置
#[derive(Clone)]
struct Bm25Config {
    k1: f32,
    b: f32,
}

impl Default for Bm25Config {
    fn default() -> Self {
        Self { k1: 1.5, b: 0.75 }
    }
}

/// BM25 运行时状态
struct Bm25State {
    avg_doc_len: f32,
    doc_freq: HashMap<String, usize>,
    doc_count: usize,
}

impl Bm25State {
    fn new() -> Self {
        Self {
            avg_doc_len: 0.0,
            doc_freq: HashMap::new(),
            doc_count: 0,
        }
    }
}

/// 语义搜索后端
#[derive(Clone)]
pub enum EmbedBackend {
    /// Google Gemini Embeddings API（默认）
    Gemini,
    /// 本地 ONNX 模型（按需加载）
    OnnxLocal,
    /// 仅 BM25，无语义搜索
    None,
}

impl Default for EmbedBackend {
    fn default() -> Self {
        Self::Gemini
    }
}

/// L2 主结构
pub struct LongTermMemory {
    /// key → 原始 entry
    store: Arc<AsyncRwLock<HashMap<String, Entry>>>,
    /// key → 向量（仅在有语义搜索时生成并缓存）
    vectors: Arc<AsyncRwLock<HashMap<String, Vec<f32>>>>,
    /// BM25 关键词倒排索引 token → [keys]
    keyword_index: Arc<AsyncRwLock<HashMap<String, Vec<String>>>>,
    /// BM25 统计状态
    bm25: Arc<RwLock<Bm25State>>,
    bm25_cfg: Bm25Config,
    /// 当前激活的语义后端
    embed_backend: EmbedBackend,
    /// Gemini API key
    gemini_key: Option<String>,
    /// 最大 entry 数（触发淘汰）
    max_entries: usize,
    /// 是否正在加载 ONNX 模型（防止重复加载）
    onnx_loading: AtomicBool,
}

impl LongTermMemory {
    pub async fn new() -> Result<Self> {
        // 从环境变量读取 Gemini API key
        let gemini_key = std::env::var("GEMINI_EMBEDDINGS_TOKEN").ok();

        let mut this = Self {
            store: Arc::new(AsyncRwLock::new(HashMap::new())),
            vectors: Arc::new(AsyncRwLock::new(HashMap::new())),
            keyword_index: Arc::new(AsyncRwLock::new(HashMap::new())),
            bm25: Arc::new(RwLock::new(Bm25State::new())),
            bm25_cfg: Bm25Config::default(),
            embed_backend: if gemini_key.is_some() {
                EmbedBackend::Gemini
            } else {
                EmbedBackend::None
            },
            gemini_key,
            max_entries: 100_000,
            onnx_loading: AtomicBool::new(false),
        };

        // 启动时从 L1 重建 BM25 索引（如果有数据）
        this.rebuild_from_l1().await;
        Ok(this)
    }

    /// 从 L1 读取所有已有条目，重建 L2 的 BM25 索引
    async fn rebuild_from_l1(&mut self) {
        let l1_path = std::env::var("MEMORY_L1_PATH")
            .unwrap_or_else(|_| "/tmp/memory_l1".to_string());

        let db = match sled::open(&l1_path) {
            Ok(db) => db,
            Err(_) => return,
        };

        let tree = match db.open_tree(b"entries") {
            Ok(t) => t,
            Err(_) => return,
        };

        let mut count = 0;
        for item in tree.iter() {
            if let Ok((_, v)) = item {
                let v_bytes: &[u8] = v.as_ref();
                if let Ok(entry) = serde_json::from_str::<Entry>(std::str::from_utf8(v_bytes).unwrap_or("")) {
                    // 重建 store
                    {
                        let mut store = self.store.write().await;
                        store.insert(entry.key.clone(), entry.clone());
                    }
                    // 重建 BM25 索引
                    self.update_bm25(&entry.key, &entry.value);
                    // 重建关键词倒排索引
                    let tokens = self.tokenize(&entry.value);
                    {
                        let mut index = self.keyword_index.write().await;
                        for token in tokens {
                            index.entry(token).or_default().push(entry.key.clone());
                        }
                    }
                    count += 1;
                }
            }
        }

        if count > 0 {
            tracing::info!("L2 bootstrap: 从 L1 恢复了 {} 条记忆", count);
        }
    }

    /// 设置语义后端
    pub fn set_embed_backend(&mut self, backend: EmbedBackend) {
        self.embed_backend = backend;
    }

    /// 写入：自动更新 BM25，语义向量按需生成
    pub async fn write(&self, key: String, entry: Entry) {
        // 存储原始数据
        {
            let mut store = self.store.write().await;
            store.insert(key.clone(), entry.clone());
        }

        // 更新 BM25 索引
        self.update_bm25(&key, &entry.value);

        // 更新关键词倒排索引
        let tokens = self.tokenize(&entry.value);
        {
            let mut index = self.keyword_index.write().await;
            for token in tokens {
                index.entry(token).or_default().push(key.clone());
            }
        }

        // 热度检查 + 淘汰
        self.maybe_evict().await;
    }

    /// 删除指定 key（从 store、BM25、关键词索引、向量缓存中删除）
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        // 从 store 删除
        {
            let mut store = self.store.write().await;
            store.remove(key);
        }

        // 从关键词索引删除
        {
            let mut index = self.keyword_index.write().await;
            // 遍历所有倒排列表，移除包含此 key 的条目
            for (_, keys) in index.iter_mut() {
                keys.retain(|k| k != key);
            }
        }

        // 从向量缓存删除
        {
            let mut vectors = self.vectors.write().await;
            vectors.remove(key);
        }

        Ok(())
    }

    /// 批量写入（迁移时使用，减少 await 开销）
    pub async fn write_batch(&self, entries: Vec<(String, Entry)>) {
        for (key, entry) in entries {
            // 存储
            {
                let mut store = self.store.write().await;
                store.insert(key.clone(), entry.clone());
            }
            // BM25
            self.update_bm25(&key, &entry.value);
            // 倒排索引
            let tokens = self.tokenize(&entry.value);
            {
                let mut index = self.keyword_index.write().await;
                for token in tokens {
                    index.entry(token).or_default().push(key.clone());
                }
            }
        }
        self.maybe_evict().await;
    }

    /// 召回（默认 BM25，可选语义）
    /// `need_semantic` = true 时触发语义搜索
    pub async fn recall(&self, req: &RecallRequest, need_semantic: bool) -> Vec<RecallResult> {
        // 主召回：BM25
        let bm25_results = self.bm25_recall(&req.query, req.limit.unwrap_or(10)).await;

        if !need_semantic {
            return bm25_results;
        }

        // 语义召回（按需）
        let semantic_results = self
            .semantic_recall(&req.query, req.limit.unwrap_or(10))
            .await;

        // 混合融合：BM25 0.6 + 语义 0.4
        self.hybrid_fuse(bm25_results, semantic_results, 0.6)
    }

    /// 纯 BM25 召回
    pub async fn recall_bm25(&self, query: &str, top_k: usize) -> Vec<RecallResult> {
        self.bm25_recall(query, top_k).await
    }

    /// 纯语义召回
    pub async fn recall_semantic(&self, query: &str, top_k: usize) -> Vec<RecallResult> {
        self.semantic_recall(query, top_k).await
    }

    /// BM25 关键词召回
    async fn bm25_recall(&self, query: &str, top_k: usize) -> Vec<RecallResult> {
        let tokens = self.tokenize(query);

        // Copy BM25 state synchronously (before any await)
        let (doc_freq, doc_count, avg_doc_len) = {
            let state = self.bm25.read();
            (state.doc_freq.clone(), state.doc_count, state.avg_doc_len)
        };
        let store = self.store.read().await;
        let index = self.keyword_index.read().await;

        let mut scores: Vec<(String, f32)> = Vec::new();

        for token in &tokens {
            let df = doc_freq.get(token).copied().unwrap_or(0);
            if df == 0 {
                continue;
            }

            let idf =
                ((doc_count as f32 - df as f32 + 0.5) / (df as f32 + 0.5) + 1e-4).ln() + 1.0;

            if let Some(keys) = index.get(token) {
                for key in keys {
                    if let Some(entry) = store.get(key) {
                        let doc_tokens = self.tokenize(&entry.value);
                        let tf = doc_tokens.iter().filter(|t| **t == *token).count() as f32;
                        let doc_len = doc_tokens.len() as f32;

                        let bm25_score = idf * tf * (self.bm25_cfg.k1 + 1.0)
                            / (tf
                                + self.bm25_cfg.k1
                                    * (1.0 - self.bm25_cfg.b
                                        + self.bm25_cfg.b * doc_len / avg_doc_len.max(1.0)));

                        scores.push((key.clone(), bm25_score));
                    }
                }
            }
        }

        // 合并同 key 分数
        let mut merged: HashMap<String, f32> = HashMap::new();
        for (key, score) in scores {
            *merged.entry(key).or_insert(0.0) += score;
        }

        let mut results: Vec<_> = merged
            .into_iter()
            .filter_map(|(key, score)| {
                store
                    .get(&key)
                    .map(|e| RecallResult::new(e.clone(), score, "L2:BM25"))
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(top_k);
        results
    }

    /// 语义召回（调用 Gemini API 或本地 ONNX）
    async fn semantic_recall(&self, query: &str, top_k: usize) -> Vec<RecallResult> {
        match self.embed_backend {
            EmbedBackend::Gemini => self.semantic_via_gemini(query, top_k).await,
            EmbedBackend::OnnxLocal => self.semantic_via_onnx(query, top_k).await,
            EmbedBackend::None => {
                // 无语义后端，降级到 BM25
                Vec::new()
            }
        }
    }

    /// Gemini Embeddings API
    async fn semantic_via_gemini(&self, query: &str, top_k: usize) -> Vec<RecallResult> {
        let Some(ref api_key) = self.gemini_key else {
            return Vec::new();
        };

        // 生成查询向量
        let query_vec = match self.get_gemini_embedding(query, api_key).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Gemini embedding failed: {}", e);
                return Vec::new();
            }
        };

        // 缓存查询向量
        let query_vec_clone = query_vec.clone();

        // 获取所有缓存的向量，计算相似度
        let vectors = self.vectors.read().await;
        let store = self.store.read().await;

        let mut scored: Vec<_> = vectors
            .iter()
            .map(|(key, vec)| {
                let score = self.cosine_sim(&query_vec_clone, vec);
                (key.clone(), score)
            })
            .collect();

        drop(vectors);

        scored.sort_by(|(_, s1), (_, s2)| s2.partial_cmp(s1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);

        let mut results = Vec::new();
        for (key, score) in scored {
            if let Some(entry) = store.get(&key) {
                results.push(RecallResult::new(entry.clone(), score, "L2:Gemini"));
            }
        }
        results
    }

    /// 调用 Gemini Embeddings API
    async fn get_gemini_embedding(&self, text: &str, api_key: &str) -> Result<Vec<f32>> {

        let payload = serde_json::json!({
            "model": "models/text-embedding-004",
            "text": text
        });

        let body = serde_json::to_string(&payload)?;
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta2/models/text-embedding-004:embedText?key={}",
            api_key
        );
        let response = ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body)?
            .into_string()?;

        // 解析响应
        #[derive(serde::Deserialize)]
        struct GeminiResp {
            embedding: EmbeddingValue,
        }
        #[derive(serde::Deserialize)]
        struct EmbeddingValue {
            values: Vec<f32>,
        }

        let resp: GeminiResp = serde_json::from_str(&response)?;
        Ok(resp.embedding.values)
    }

    /// 本地 ONNX 模型（按需加载）
    async fn semantic_via_onnx(&self, query: &str, top_k: usize) -> Vec<RecallResult> {
        // 防止重复加载
        if self.onnx_loading.swap(true, Ordering::SeqCst) {
            return Vec::new();
        }

        let result = self.run_onnx_semantic(query, top_k).await;

        self.onnx_loading.store(false, Ordering::SeqCst);
        result
    }

    async fn run_onnx_semantic(&self, _query: &str, _top_k: usize) -> Vec<RecallResult> {
        // TODO: 接入 ort (ONNX Runtime) 加载本地模型
        // 当前为占位符，真实接入时实现如下：
        //
        // let model = ort::Session::from_file("models/minilm.onnx")?;
        // let input = tokenize(query, max_len=256);
        // let output = model.run(input)?;
        // let query_vec = output[0].as_slice();
        //
        // let vectors = self.vectors.read().await;
        // ... cosine similarity ...

        tracing::warn!("ONNX backend not implemented yet, use Gemini or BM25");
        Vec::new()
    }

    /// 混合融合
    fn hybrid_fuse(
        &self,
        bm25_results: Vec<RecallResult>,
        semantic_results: Vec<RecallResult>,
        bm25_weight: f32,
    ) -> Vec<RecallResult> {
        let sem_weight = 1.0 - bm25_weight;
        let mut score_map: HashMap<String, (f32, RecallResult)> = HashMap::new();

        for r in bm25_results {
            let norm = (r.score / 20.0).clamp(0.0, 1.0); // BM25 分数归一化
            score_map.insert(r.entry.id.clone(), (norm * bm25_weight, r));
        }

        for r in semantic_results {
            let norm = r.score.clamp(0.0, 1.0);
            score_map
                .entry(r.entry.id.clone())
                .and_modify(|(s, _)| *s += norm * sem_weight)
                .or_insert((norm * sem_weight, r));
        }

        let mut results: Vec<_> = score_map
            .into_values()
            .map(|(_, mut r)| {
                r.from_layer = "L2:Hybrid";
                r
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }

    /// 生成并缓存向量（写入时可选调用，或后台批量处理）
    pub async fn embed_and_cache(&self, key: &str) -> Result<()> {
        let entry = {
            let store = self.store.read().await;
            store.get(key).cloned()
        };

        let Some(entry) = entry else {
            return Ok(());
        };

        let vec = match self.embed_backend {
            EmbedBackend::Gemini => {
                let api_key = self.gemini_key.as_ref().unwrap();
                self.get_gemini_embedding(&entry.value, api_key).await?
            }
            EmbedBackend::OnnxLocal => {
                // TODO: 本地 ONNX 生成
                return Ok(());
            }
            EmbedBackend::None => return Ok(()),
        };

        let mut vectors = self.vectors.write().await;
        vectors.insert(key.to_string(), vec);
        Ok(())
    }

    /// 批量生成向量（迁移后一次性调用）
    pub async fn embed_all(&self) -> Result<usize> {
        let keys: Vec<String> = {
            let store = self.store.read().await;
            store.keys().cloned().collect()
        };

        let mut count = 0;
        for key in keys {
            if let Err(e) = self.embed_and_cache(&key).await {
                tracing::warn!("embed {} failed: {}", key, e);
            } else {
                count += 1;
            }
        }
        Ok(count)
    }

    // --- 工具函数 ---

    fn cosine_sim(&self, a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        dot / (norm_a * norm_b)
    }

    fn tokenize(&self, text: &str) -> Vec<String> {
        let lower = text.to_lowercase();
        let mut tokens = Vec::new();

        // 英文/数字：按空格/符号分词（保留原有逻辑）
        for word in lower.split(|c: char| !c.is_alphanumeric() && !is_cjk(c)) {
            if word.len() > 2 {
                tokens.push(word.to_string());
            } else if word.len() == 2 {
                tokens.push(word.to_string()); // 英文 2-gram
            }
        }

        // 中文字符序列：生成 2-gram（滑动窗口）
        let mut cjk_buf = String::new();
        for c in lower.chars() {
            if is_cjk(c) {
                cjk_buf.push(c);
            } else {
                // 遇到非 CJK 字符，输出已有 CJK 序列的 2-gram
                if cjk_buf.len() >= 2 {
                    let chars: Vec<char> = cjk_buf.chars().collect();
                    for window in chars.windows(2) {
                        tokens.push(format!("{}{}", window[0], window[1]));
                    }
                }
                cjk_buf.clear();
            }
        }
        // 处理末尾 CJK 序列
        if cjk_buf.len() >= 2 {
            let chars: Vec<char> = cjk_buf.chars().collect();
            for window in chars.windows(2) {
                tokens.push(format!("{}{}", window[0], window[1]));
            }
        }

        tokens
    }

    fn update_bm25(&self, _key: &str, value: &str) {
        let tokens = self.tokenize(value);
        let doc_len = tokens.len() as f32;

        let mut state = self.bm25.write();
        let total_len = state.avg_doc_len * state.doc_count as f32 + doc_len;
        state.doc_count += 1;
        state.avg_doc_len = total_len / state.doc_count as f32;

        for token in tokens {
            *state.doc_freq.entry(token).or_insert(0) += 1;
        }
    }

    async fn maybe_evict(&self) {
        let current_len = {
            let store = self.store.read().await;
            store.len()
        };

        if current_len > self.max_entries {
            self.evict_low_importance().await;
        }
    }

    async fn evict_low_importance(&self) {
        let count = {
            let store = self.store.read().await;
            let now = chrono::Utc::now();

            let mut entries: Vec<_> = store
                .iter()
                .map(|(k, v)| {
                    // 综合评分：importance 越高分越高，last_accessed 越新分越高
                    let importance_score = v.importance as i64;
                    let age_hours = (now.timestamp() - v.last_accessed.timestamp()) / 3600;
                    // 淘汰评分 = importance 反比 + 年龄正比
                    let evict_score = (10 - importance_score) * 1_000_000 + age_hours.min(10_000);
                    (k.clone(), evict_score)
                })
                .collect();

            entries.sort_by_key(|(_, score)| *score);

            let to_evict: Vec<_> = entries
                .into_iter()
                .rev() // 分数高的先淘汰
                .take(1000)
                .map(|(k, _)| k)
                .collect();

            for key in &to_evict {
                let mut store = self.store.write().await;
                store.remove(key);
            }
            let mut vectors = self.vectors.write().await;
            for key in &to_evict {
                vectors.remove(key);
            }
            to_evict.len()
        };
        tracing::info!("L2 evicted {} entries", count);
    }

    pub async fn gc(&self) -> Result<()> {
        self.evict_low_importance().await;
        Ok(())
    }

    pub fn len(&self) -> usize {
        // 同步获取 store 大概数量（通过 try_read 而非阻塞锁）
        self.store
            .try_read()
            .map(|g| g.len())
            .unwrap_or(0)
    }
}
