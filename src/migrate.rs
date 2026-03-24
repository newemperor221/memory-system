//! 旧记忆文件迁移工具
//!
//! 读取现有的 memory/*.md 和 MEMORY.md，
//! 提取高价值条目写入新系统，然后删除旧文件。

use crate::common::{Entry, Importance, Layer};
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub struct Migrator {
    workspace_root: std::path::PathBuf,
}

impl Migrator {
    pub fn new(workspace_root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
        }
    }

    /// 扫描所有旧记忆文件
    pub fn scan_old_files(&self) -> Vec<(String, std::path::PathBuf)> {
        let mut files = Vec::new();
        let root = self.workspace_root.clone();

        // MEMORY.md
        let mem_path = root.join("MEMORY.md");
        if mem_path.exists() {
            files.push(("MEMORY.md".to_string(), mem_path));
        }

        // memory/*.md
        let mem_dir = root.join("memory");
        if mem_dir.is_dir() {
            if let Ok(mut entries) = std::fs::read_dir(&mem_dir) {
                while let Some(entry) = entries.next().transpose().ok() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            files.push((format!("memory/{}", name), path));
                        }
                    }
                }
            }
        }

        files
    }

    /// 解析记忆文件，提取条目
    pub fn parse_file(&self, path: &Path, source: &str) -> Result<Vec<Entry>> {
        let content = std::fs::read_to_string(path)?;
        let mut entries = Vec::new();

        // 简单的 Markdown 解析
        // MEMORY.md 格式：## 节名 / - 条目
        // memory/YYYY-MM-DD.md 格式：类似结构化日志

        let lines: Vec<&str> = content.lines().collect();

        if source == "MEMORY.md" {
            entries.extend(self.parse_memory_md(&lines)?);
        } else {
            entries.extend(self.parse_daily_log(&lines, source)?);
        }

        Ok(entries)
    }

    fn parse_memory_md(&self, lines: &[&str]) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut current_section = String::new();
        let mut current_content = String::new();

        for line in lines {
            let trimmed = line.trim();

            if trimmed.starts_with("## ") {
                // 提交上一个 section
                if !current_content.trim().is_empty() {
                    let key = format!("memory:longterm:{}", slugify(&current_section));
                    entries.push(Entry {
                        id: uuid::Uuid::new_v4().to_string(),
                        key,
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
                current_content = String::new();
            } else if trimmed.starts_with("- **") || trimmed.starts_with("- ") {
                // 列表条目
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

        // 最后一个 section
        if !current_content.trim().is_empty() {
            let key = format!("memory:longterm:{}", slugify(&current_section));
            entries.push(Entry {
                id: uuid::Uuid::new_v4().to_string(),
                key,
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

    fn parse_daily_log(&self, lines: &[&str], source: &str) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();

        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // 尝试识别时间戳行（如 "## 2026-03-23" 或 "- [时间] 内容"）
            if trimmed.starts_with("## ") {
                continue; // 日期标题，跳过
            }

            // 普通内容行 → 作为独立条目
            if trimmed.len() > 10 {
                let date_part = source
                    .split('/')
                    .last()
                    .unwrap_or("daily")
                    .replace(".md", "");

                entries.push(Entry {
                    id: uuid::Uuid::new_v4().to_string(),
                    key: format!("memory:daily:{}", date_part),
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

    /// 执行迁移
    pub async fn migrate(&self, system: &crate::MemorySystem) -> Result<MigrateReport> {
        let files = self.scan_old_files();
        let mut report = MigrateReport {
            files_found: files.len(),
            entries_extracted: 0,
            entries_imported: 0,
            errors: Vec::new(),
        };

        for (name, path) in &files {
            match self.parse_file(path, name) {
                Ok(entries) => {
                    report.entries_extracted += entries.len();
                    for entry in entries {
                        match system.remember(entry).await {
                            Ok(_) => report.entries_imported += 1,
                            Err(e) => report.errors.push(format!("{}: {}", name, e)),
                        }
                    }
                }
                Err(e) => report.errors.push(format!("parse {}: {}", name, e)),
            }
        }

        // 删除旧文件（所有）
        if report.errors.is_empty() {
            for (_, path) in &files {
                if let Err(e) = std::fs::remove_file(path) {
                    report.errors.push(format!("delete {:?}: {}", path, e));
                } else {
                    info!("已删除旧文件: {:?}", path);
                }
            }
            // 删除 memory/ 目录（如果空了）
            let mem_dir = self.workspace_root.join("memory");
            let _ = std::fs::remove_dir(&mem_dir);
        }

        Ok(report)
    }
}

#[derive(Debug)]
pub struct MigrateReport {
    pub files_found: usize,
    pub entries_extracted: usize,
    pub entries_imported: usize,
    pub errors: Vec<String>,
}

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .take(5)
        .collect::<Vec<_>>()
        .join("-")
}

impl std::fmt::Display for MigrateReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== 迁移报告 ===")?;
        writeln!(f, "旧文件数量: {}", self.files_found)?;
        writeln!(f, "提取条目: {}", self.entries_extracted)?;
        writeln!(f, "成功导入: {}", self.entries_imported)?;
        if !self.errors.is_empty() {
            writeln!(f, "错误:")?;
            for e in &self.errors {
                writeln!(f, "  - {}", e)?;
            }
        }
        Ok(())
    }
}
