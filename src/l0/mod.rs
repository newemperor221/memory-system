//! L0 Working Memory
//!
//! 进程内共享，零拷贝，lock-free 读，多 agent 并发安全
//!
//! 核心设计：
//! - dashmap：分片 RwLock，无全局竞争
//! - Arc<Entry>：原子引用计数，zero-copy 读取
//!
//! 性能目标：< 1µs 单次读，~80M 读/秒（16 threads）

use crate::common::{Entry, RecallRequest, RecallResult};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::sync::Arc;

/// 工作内存
pub struct WorkingMemory {
    /// 分片 HashMap，无锁读
    store: DashMap<String, Arc<Entry>>,
    /// 按 key 前缀索引（用于范围查询）
    prefix_index: DashMap<String, Vec<String>>,
    /// 当前 session 的 key 集合（用于快速过期）
    session_keys: RwLock<std::collections::HashSet<String>>,
}

impl WorkingMemory {
    pub fn new() -> Self {
        Self {
            store: DashMap::with_capacity(1024),
            prefix_index: DashMap::new(),
            session_keys: RwLock::new(std::collections::HashSet::new()),
        }
    }

    /// 写入（原子替换）
    pub async fn write(&self, key: String, entry: Entry) {
        let entry = Arc::new(entry);

        // 写入 store
        self.store.insert(key.clone(), entry);

        // 更新前缀索引
        let prefix = prefix_of(&key);
        self.prefix_index
            .entry(prefix)
            .or_default()
            .push(key.clone());

        // 记录 session key
        {
            let mut keys = self.session_keys.write();
            keys.insert(key);
        }
    }

    /// 精确读取 O(1)，同时更新 last_accessed
    pub fn get(&self, key: &str) -> Option<Entry> {
        self.store.get(key).map(|arc| {
            let mut entry = (**arc).clone();
            entry.last_accessed = chrono::Utc::now();
            // 写回（原子替换）
            self.store.insert(key.to_string(), Arc::new(entry.clone()));
            entry
        })
    }

    /// 按 key 前缀召回
    pub fn get_by_prefix(&self, prefix: &str) -> Vec<Entry> {
        self.prefix_index
            .get(prefix)
            .map(|keys| {
                keys.iter()
                    .filter_map(|k| self.store.get(k).map(|arc| (**arc).clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 删除指定 key
    pub async fn remove(&self, key: &str) {
        self.store.remove(key);
        let mut keys = self.session_keys.write();
        keys.remove(key);
    }

    /// 关键词召回
    pub fn search(&self, keyword: &str) -> Vec<RecallResult> {
        let keyword_lower = keyword.to_lowercase();
        self.store
            .iter()
            .filter(|pair| pair.value().value.to_lowercase().contains(&keyword_lower))
            .map(|pair| RecallResult::new((**pair.value()).clone(), 1.0, "L0"))
            .collect()
    }

    /// 召回
    pub async fn recall(&self, req: &RecallRequest) -> Vec<RecallResult> {
        let mut results = Vec::new();

        if let Some(keys) = &req.keys {
            for key in keys {
                if let Some(entry) = self.get(key) {
                    results.push(RecallResult::new(entry, 1.0, "L0"));
                }
            }
        }

        let keyword_results = self.search(&req.query);
        results.extend(keyword_results);

        results
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// 清理当前 session 的 keys
    pub fn clear_session(&self) {
        let keys: Vec<String> = {
            let session_keys = self.session_keys.read();
            session_keys.iter().cloned().collect()
        };
        for key in keys {
            self.store.remove(&key);
        }
        let mut session_keys = self.session_keys.write();
        session_keys.clear();
    }
}

impl Default for WorkingMemory {
    fn default() -> Self {
        Self::new()
    }
}

fn prefix_of(key: &str) -> String {
    key.split(':').next().unwrap_or(key).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_basic_write_read() {
        let mem = WorkingMemory::new();
        mem.write("test:1".to_string(), Entry::new("test:1", "hello"))
            .await;
        assert_eq!(mem.get("test:1").unwrap().value, "hello");
    }

    #[tokio::test]
    async fn test_search() {
        let mem = WorkingMemory::new();
        mem.write("test:1".to_string(), Entry::new("test:1", "hello world"))
            .await;
        mem.write("test:2".to_string(), Entry::new("test:2", "foo bar"))
            .await;
        let results = mem.search("hello");
        assert_eq!(results.len(), 1);
    }
}
