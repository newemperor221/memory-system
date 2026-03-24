//! L1 Short-Term Memory
//!
//! 基于 sled（纯 Rust，无 C 依赖）的秒级持久化层

use crate::common::{Entry, RecallRequest, RecallResult};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

/// 主结构
pub struct ShortTermMemory {
    db: Arc<sled::Db>,
    write_tx: mpsc::Sender<(String, Entry)>,
}

impl ShortTermMemory {
    /// 暴露共享的 sled::Db，给 sync_to_workspace_files 等外部调用使用
    pub fn db(&self) -> Arc<sled::Db> {
        self.db.clone()
    }
}

impl ShortTermMemory {
    pub async fn new() -> Result<Self> {
        let db_path =
            std::env::var("MEMORY_L1_PATH").unwrap_or_else(|_| "/tmp/memory_l1".to_string());

        let db = sled::open(&db_path)?;
        let db = Arc::new(db);

        let (tx, mut rx) = mpsc::channel::<(String, Entry)>(1000);

        // 后台批量写线程
        let db_clone = db.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut batch: Vec<(String, Entry)> = Vec::with_capacity(100);
                let mut interval =
                    tokio::time::interval(tokio::time::Duration::from_millis(50));

                loop {
                    tokio::select! {
                        item = rx.recv() => {
                            match item {
                                Some(i) => {
                                    batch.push(i);
                                    if batch.len() >= 100 {
                                        Self::flush_batch(&db_clone, &mut batch);
                                    }
                                }
                                None => {
                                    if !batch.is_empty() {
                                        Self::flush_batch(&db_clone, &mut batch);
                                    }
                                    break;
                                }
                            }
                        }
                        _ = interval.tick() => {
                            if !batch.is_empty() {
                                Self::flush_batch(&db_clone, &mut batch);
                            }
                        }
                    }
                }
            });
        });

        Ok(Self { db, write_tx: tx })
    }

    fn flush_batch(db: &sled::Db, batch: &mut Vec<(String, Entry)>) {
        if batch.is_empty() {
            return;
        }
        let tree = db.open_tree(b"entries").unwrap();
        for (key, entry) in batch.drain(..) {
            if let Ok(json) = serde_json::to_string(&entry) {
                let _ = tree.insert(sled::IVec::from(key.as_bytes()), sled::IVec::from(json.as_bytes()));
            }
        }
        // 注意：不调用 db.flush()。sled 会自动在后台做 mmap flush，
        // 显式同步 flush 在独立线程里会阻塞 DB 引擎，造成死锁。
    }

    pub async fn write(&self, key: String, entry: Entry) -> Result<()> {
        self.write_tx
            .send((key, entry))
            .await
            .map_err(|_| anyhow::anyhow!("L1 writer closed"))?;
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Option<Entry> {
        let tree = self.db.open_tree(b"entries").ok()?;
        let iv = tree.get(key).ok()??;
        let mut entry: Entry = serde_json::from_str(std::str::from_utf8(&*iv).ok()?).ok()?;
        // 更新 last_accessed 并写回（不主动 flush，由后台批量线程定期刷盘）
        entry.last_accessed = chrono::Utc::now();
        if let Ok(json) = serde_json::to_string(&entry) {
            let _ = tree.insert(sled::IVec::from(key.as_bytes()), sled::IVec::from(json.as_bytes()));
        }
        Some(entry)
    }

    pub async fn get_by_prefix(&self, prefix: &str) -> Vec<Entry> {
        let tree = match self.db.open_tree(b"entries") {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let mut results = Vec::new();

        for item in tree.iter() {
            if let Ok((k, v)) = item {
                let k_str = std::str::from_utf8(&k).unwrap_or("");
                if k_str.starts_with(prefix) || k_str == prefix {
                    if let Ok(entry) =
                        serde_json::from_str::<Entry>(std::str::from_utf8(&v).unwrap_or(""))
                    {
                        results.push(entry);
                    }
                }
            }
        }
        results
    }

    pub async fn recall(&self, req: &RecallRequest) -> Vec<RecallResult> {
        let mut results = Vec::new();

        if let Some(keys) = &req.keys {
            for key in keys {
                if let Some(entry) = self.get(key).await {
                    results.push(RecallResult::new(entry, 1.0, "L1"));
                }
            }
        }

        let all_entries = self.get_all_entries().await;
        let query_lower = req.query.to_lowercase();

        for entry in all_entries {
            if entry.value.to_lowercase().contains(&query_lower) {
                if !results
                    .iter()
                    .any(|r: &RecallResult| r.entry.id == entry.id)
                {
                    results.push(RecallResult::new(entry, 0.8, "L1"));
                }
            }
        }
        results
    }

    async fn get_all_entries(&self) -> Vec<Entry> {
        let tree = match self.db.open_tree(b"entries") {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let mut entries = Vec::new();
        for item in tree.iter() {
            if let Ok((_, v)) = item {
                let v_bytes: &[u8] = &v;
                if let Ok(entry) =
                    serde_json::from_str(std::str::from_utf8(v_bytes).unwrap_or(""))
                {
                    entries.push(entry);
                }
            }
        }
        entries
    }

    pub async fn gc(&self) -> Result<()> {
        let db = self.db.clone();

        // Fire-and-forget: spawn 独立线程，立即返回。
        // 不 join()，避免 HTTP handler 等待线程完成。
        std::thread::spawn(move || {
            if let Err(e) = Self::gc_sync(&db) {
                tracing::warn!("后台 GC 失败: {}", e);
            }
        });

        Ok(())
    }

    fn gc_sync(db: &sled::Db) -> Result<()> {
        let tree = match db.open_tree(b"entries") {
            Ok(t) => t,
            Err(_) => return Ok(()),
        };

        let now = chrono::Utc::now();
        let max_age_secs = std::env::var("MEMORY_L1_MAX_AGE_SECS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(7 * 24 * 3600); // 默认 7 天

        let max_entries = std::env::var("MEMORY_L1_MAX_ENTRIES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(50_000);

        let total = tree.len();
        if total <= max_entries {
            // 仅检查过期，不过期
            let mut to_delete = Vec::new();
            for item in tree.iter() {
                if let Ok((k, v)) = item {
                    if let Ok(entry) = serde_json::from_str::<Entry>(
                        std::str::from_utf8(&v).unwrap_or(""),
                    ) {
                        if let Some(expires) = entry.expires_at {
                            if expires < now {
                                to_delete.push(k.to_vec());
                            }
                        } else if entry.last_accessed.timestamp() < (now.timestamp() - max_age_secs) {
                            if entry.importance == crate::common::Importance::Low {
                                to_delete.push(k.to_vec());
                            }
                        }
                    }
                }
            }
            for k in to_delete {
                let _ = tree.remove(&k);
            }
            return Ok(());
        }

        // 超过 max_entries 时，按 importance + last_accessed 综合淘汰
        let mut entries: Vec<(String, Entry)> = Vec::new();
        for item in tree.iter() {
            if let Ok((k, v)) = item {
                if let Ok(entry) = serde_json::from_str::<Entry>(
                    std::str::from_utf8(&v).unwrap_or(""),
                ) {
                    entries.push((std::str::from_utf8(&k).unwrap_or("").to_string(), entry));
                }
            }
        }

        entries.sort_by(|a, b| {
            let a_score = (a.1.importance as u8) as i64 * 1_000_000_000 + a.1.last_accessed.timestamp();
            let b_score = (b.1.importance as u8) as i64 * 1_000_000_000 + b.1.last_accessed.timestamp();
            a_score.cmp(&b_score)
        });

        let to_delete = total.saturating_sub(max_entries);
        let to_delete = &entries[..to_delete.min(entries.len())];

        for (k, _) in to_delete {
            let _ = tree.remove(sled::IVec::from(k.as_bytes()));
        }

        tracing::info!("L1 GC: 淘汰了 {} 条记录", to_delete.len());
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.db
            .open_tree(b"entries")
            .map(|t| t.len())
            .unwrap_or(0)
    }
}

impl Drop for ShortTermMemory {
    fn drop(&mut self) {
        let _ = self.db.flush();
    }
}
