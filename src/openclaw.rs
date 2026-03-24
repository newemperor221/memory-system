//! OpenClaw Workspace Integration
//!
//! 将三层记忆系统接入 OpenClaw workspace：
//! - 读取现有 memory/*.md / MEMORY.md
//! - 把新记忆同步写入 workspace（保持文件可读性）
//! - 提供 OpenClaw agent 调用的 API

use crate::{Entry, Importance, Layer, MemorySystem, RecallRequest, RecallResult};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use chrono::{Duration, NaiveDate, TimeZone, Utc};
use sled;

// Manual Clone since Mutex<Option<JoinHandle>> doesn't derive Clone
impl Clone for OpenClawBridge {
    fn clone(&self) -> Self {
        Self {
            memory: self.memory.clone(),
            workspace: self.workspace.clone(),
            sync_handle: std::sync::Mutex::new(None), // don't clone the background thread handle
        }
    }
}

pub struct OpenClawBridge {
    /// 三层记忆系统
    pub memory: Arc<MemorySystem>,
    /// workspace 根目录
    workspace: PathBuf,
    /// 后台同步任务
    sync_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl OpenClawBridge {
    /// 构造（在 #[tokio::main] 里调用，使用当前 runtime）
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let rt = tokio::runtime::Handle::current();
        let memory = rt.block_on(MemorySystem::new()).expect("MemorySystem init failed");

        Self {
            memory: Arc::new(memory),
            workspace,
            sync_handle: std::sync::Mutex::new(None),
        }
    }

    /// 异步构造（备用）
    pub async fn new_async(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let memory = MemorySystem::new().await?;
        Ok(Self {
            memory: Arc::new(memory),
            workspace,
            sync_handle: std::sync::Mutex::new(None),
        })
    }

    /// 启动时调用：从 workspace 加载旧记忆
    pub async fn load_workspace(&self) -> Result<usize> {
        let migrator = Migrator::new(self.workspace.clone());
        let entries = migrator.scan_and_parse_all()?;
        let count = entries.len();

        for entry in entries {
            self.memory.remember(entry).await?;
        }

        tracing::info!("从 workspace 加载了 {} 条记忆", count);
        Ok(count)
    }

    /// 启动后台同步：定期把 L1 数据写回文件
    pub fn start_background_sync(&self) {
        let memory = self.memory.clone();
        let workspace = self.workspace.clone();

        let _handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let db = memory.l1.db();
                let workspace = workspace.clone();
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    // spawn_blocking 把同步 I/O 操作放到专用线程池，避免阻塞 tokio 调度
                    if let Err(e) = tokio::task::spawn_blocking({
                        let db = db.clone();
                        let workspace = workspace.clone();
                        move || Self::sync_to_workspace_files_sync(&db, workspace)
                    }).await {
                        tracing::warn!("workspace 同步失败: {}", e);
                    }
                }
            });
        });

        *self.sync_handle.lock().unwrap() = Some(_handle);
    }

    /// 同步 L1 数据到 workspace 文件（同步版本，给 std::thread 调用）
    /// - private 记忆 → <workspace>/memory/YYYY-MM-DD.md
    /// - public 记忆 → <workspace>/memory-shared/YYYY-MM-DD.md
    fn sync_to_workspace_files_sync(
        db: &Arc<sled::Db>,
        workspace: PathBuf,
    ) -> Result<()> {
        // 分离 private 和 public 记忆
        let mut private_by_date: std::collections::HashMap<String, Vec<Entry>> =
            std::collections::HashMap::new();
        let mut public_by_date: std::collections::HashMap<String, Vec<Entry>> =
            std::collections::HashMap::new();

        let tree = db.open_tree(b"entries")?;
        for item in tree.iter() {
            if let Ok((_, v)) = item {
                let v_str = std::str::from_utf8(&v).unwrap_or("");
                if let Ok(entry) = serde_json::from_str::<Entry>(v_str) {
                    let date = entry.created_at.format("%Y-%m-%d").to_string();
                    match entry.layer {
                        Layer::Public => {
                            public_by_date.entry(date).or_default().push(entry);
                        }
                        Layer::Private => {
                            private_by_date.entry(date).or_default().push(entry);
                        }
                    }
                }
            }
        }
        drop(tree);

        // 写 private 文件
        Self::write_daily_files(&workspace.join("memory"), &private_by_date)?;
        // 写 public 文件
        Self::write_daily_files(&workspace.join("memory-shared"), &public_by_date)?;

        tracing::debug!("workspace 同步完成");
        Ok(())
    }

    /// 将一组按日期分组的记忆写入 daily .md 文件
    fn write_daily_files(
        dir: &PathBuf,
        entries_by_date: &std::collections::HashMap<String, Vec<Entry>>,
    ) -> Result<()> {
        std::fs::create_dir_all(dir)?;

        // 清理 30 天前的旧文件
        if let Ok(files) = std::fs::read_dir(dir) {
            let cutoff = Utc::now() - Duration::days(30);
            for entry in files.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(file_date) = NaiveDate::parse_from_str(name, "%Y-%m-%d") {
                            if let Some(file_dt) = file_date
                                .and_hms_opt(0, 0, 0)
                                .and_then(|dt| Utc.from_local_datetime(&dt).single())
                            {
                                if file_dt < cutoff {
                                    let _ = std::fs::remove_file(&path);
                                }
                            }
                        }
                    }
                }
            }
        }

        for (date, entries) in entries_by_date {
            let file_path = dir.join(format!("{}.md", date));
            let mut content = format!("# 记忆 - {}\n\n", date);
            content.push_str(&format!("共 {} 条记忆\n\n", entries.len()));

            for entry in entries {
                let layer_tag = match entry.layer {
                    Layer::Public => "🟢 公共",
                    Layer::Private => "🔒 私有",
                };
                content.push_str(&format!(
                    "## {}\n**Key:** `{}`\n**来源:** {} | **重要性:** {:?} | {}\n\n{}\n\n",
                    &entry.id[..8],
                    entry.key,
                    entry.source,
                    entry.importance,
                    layer_tag,
                    entry.value
                ));
            }
            std::fs::write(&file_path, content)?;
        }

        Ok(())
    }

    /// 记忆 → 写入三层
    pub async fn remember(&self, entry: Entry) -> Result<()> {
        self.memory.remember(entry).await
    }

    /// 快捷方法：写入一条记忆
    pub async fn remember_text(
        &self,
        value: impl Into<String>,
        importance: Importance,
        tags: Vec<String>,
        agent_id: &str,
        layer: Layer,
    ) -> Result<()> {
        let key = format!(
            "{}:agent:{}:{}",
            layer.prefix(),
            agent_id,
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        );

        self.memory
            .remember(Entry {
                id: uuid::Uuid::new_v4().to_string(),
                key,
                value: value.into(),
                importance,
                tags,
                source: agent_id.to_string(),
                layer,
                created_at: chrono::Utc::now(),
                last_accessed: chrono::Utc::now(),
                expires_at: None,
            })
            .await
    }

    /// 召回 → 返回结果（搜指定 layer）
    pub async fn recall(&self, query: &str, layer: Option<Layer>) -> Vec<RecallResult> {
        let req = RecallRequest {
            query: query.to_string(),
            keys: None,
            agent_id: None,
            tags: None,
            limit: Some(10),
            layer,
        };
        let results = self.memory.recall(req).await;
        // layer 过滤：按 entry.key 的 layer: 前缀筛选
        match layer {
            Some(l) => {
                let prefix = format!("{}:", l.prefix());
                results.into_iter().filter(|r| r.entry.key.starts_with(&prefix)).collect()
            }
            None => results,
        }
    }

    /// 语义召回（搜指定 layer）
    pub async fn recall_semantic(&self, query: &str, layer: Option<Layer>) -> Vec<RecallResult> {
        let req = RecallRequest {
            query: query.to_string(),
            keys: None,
            agent_id: None,
            tags: None,
            limit: Some(10),
            layer,
        };
        let results = self.memory.recall_with_semantic(req).await;
        match layer {
            Some(l) => {
                let prefix = format!("{}:", l.prefix());
                results.into_iter().filter(|r| r.entry.key.starts_with(&prefix)).collect()
            }
            None => results,
        }
    }

    /// 精确获取
    pub async fn get(&self, key: &str) -> Option<Entry> {
        self.memory.get(key).await
    }

    /// 系统状态
    pub fn stats(&self) -> crate::MemoryStats {
        self.memory.stats()
    }
}

impl Drop for OpenClawBridge {
    fn drop(&mut self) {
        if let Some(_handle) = self.sync_handle.lock().unwrap().take() {
            // std thread cannot be aborted, just let it run
        }
    }
}

// ─── Migrator ────────────────────────────────────────────────────────

struct Migrator {
    workspace_root: PathBuf,
}

impl Migrator {
    fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }

    fn scan_and_parse_all(&self) -> Result<Vec<Entry>> {
        let mut all_entries = Vec::new();

        let mem_path = self.workspace_root.join("MEMORY.md");
        if mem_path.exists() {
            let content = std::fs::read_to_string(&mem_path)?;
            all_entries.extend(self.parse_memory_md(&content)?);
        }

        let mem_dir = self.workspace_root.join("memory");
        if mem_dir.is_dir() {
            if let Ok(mut entries) = std::fs::read_dir(&mem_dir) {
                while let Some(entry) = entries.next().transpose().ok().flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                all_entries.extend(self.parse_daily_log(&content, name)?);
                            }
                        }
                    }
                }
            }
        }

        Ok(all_entries)
    }

    fn parse_memory_md(&self, content: &str) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        let mut current_section = String::new();
        let mut current_content = String::new();

        for line in lines {
            let trimmed = line.trim();
            if trimmed.starts_with("## ") {
                if !current_content.trim().is_empty() {
                    entries.push(Entry {
                        id: uuid::Uuid::new_v4().to_string(),
                        key: format!("memory:longterm:{}", slugify(&current_section)),
                        value: current_content.trim().to_string(),
                        importance: Importance::High,
                        tags: vec!["memory".to_string(), "longterm".to_string()],
                        source: "MEMORY.md".to_string(),
                        layer: Layer::Private,
                        created_at: chrono::Utc::now(),
                        last_accessed: chrono::Utc::now(),
                        expires_at: None,
                    });
                }
                current_section = trimmed.trim_start_matches("## ").to_string();
                current_content.clear();
            } else if trimmed.starts_with("- **") || trimmed.starts_with("- ") {
                let text = trimmed
                    .trim_start_matches("- **")
                    .trim_start_matches("- ");
                current_content.push_str(text);
                current_content.push('\n');
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current_content.push_str(trimmed);
                current_content.push('\n');
            }
        }

        if !current_content.trim().is_empty() {
            entries.push(Entry {
                id: uuid::Uuid::new_v4().to_string(),
                key: format!("memory:longterm:{}", slugify(&current_section)),
                value: current_content.trim().to_string(),
                importance: Importance::High,
                tags: vec!["memory".to_string(), "longterm".to_string()],
                source: "MEMORY.md".to_string(),
                layer: Layer::Private,
                created_at: chrono::Utc::now(),
                last_accessed: chrono::Utc::now(),
                expires_at: None,
            });
        }

        Ok(entries)
    }

    fn parse_daily_log(&self, content: &str, source: &str) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed.len() > 10 {
                entries.push(Entry {
                    id: uuid::Uuid::new_v4().to_string(),
                    key: format!("memory:daily:{}", source.trim_end_matches(".md")),
                    value: trimmed.to_string(),
                    importance: Importance::Normal,
                    tags: vec!["memory".to_string(), "daily".to_string()],
                    source: source.to_string(),
                    layer: Layer::Private,
                    created_at: chrono::Utc::now(),
                    last_accessed: chrono::Utc::now(),
                    expires_at: None,
                });
            }
        }
        Ok(entries)
    }
}

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .take(5)
        .collect::<Vec<_>>()
        .join("-")
}


