//! 配置文件支持
//!
//! 支持 YAML/TOML 配置文件，默认路径：
//!   ./memory-system.toml
//!   ~/.config/memory-system.toml

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 记忆系统配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Workspace 路径（记忆文件所在目录）
    pub workspace: PathBuf,
    /// L0 工作内存上限（条数）
    pub l0_max_entries: Option<usize>,
    /// L1 持久化路径
    pub l1_path: Option<PathBuf>,
    /// L1 WAL 路径
    pub l1_wal_path: Option<PathBuf>,
    /// L1 最大内存占用（MB）
    pub l1_max_mb: Option<usize>,
    /// L2 最大 entry 数
    pub l2_max_entries: Option<usize>,
    /// 语义搜索后端
    pub embed_backend: Option<String>,
    /// Gemini API Key（优先从环境变量 GEMINI_EMBEDDINGS_TOKEN）
    pub gemini_api_key: Option<String>,
    /// 语义搜索权重（BM25 vs 向量）
    pub hybrid_alpha: Option<f32>,
    /// 背景同步间隔（秒）
    pub sync_interval_secs: Option<u64>,
    /// 日志级别
    pub log_level: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace: PathBuf::from("."),
            l0_max_entries: Some(10_000),
            l1_path: None,
            l1_wal_path: None,
            l1_max_mb: Some(100),
            l2_max_entries: Some(100_000),
            embed_backend: Some("gemini".to_string()),
            gemini_api_key: None,
            hybrid_alpha: Some(0.6),
            sync_interval_secs: Some(30),
            log_level: Some("info".to_string()),
        }
    }
}

impl Config {
    /// 从文件加载
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let content = std::fs::read_to_string(&path)?;
        let config: Config = if path.extension().and_then(|s| s.to_str()) == Some("toml") {
            toml::from_str(&content)?
        } else {
            serde_yaml::from_str(&content)?
        };
        Ok(config)
    }

    /// 搜索配置文件
    pub fn find() -> Option<Self> {
        let candidates = [
            PathBuf::from("memory-system.toml"),
            PathBuf::from("memory-system.yaml"),
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config/memory-system.toml")),
        ];

        for p in candidates.into_iter().flatten() {
            if p.exists() {
                if let Ok(c) = Self::load(&p) {
                    tracing::info!("配置文件: {:?}", p);
                    return Some(c);
                }
            }
        }
        None
    }

    /// 从环境变量获取有效值
    pub fn resolve(self) -> Self {
        Self {
            workspace: std::env::var("MEMORY_WORKSPACE")
                .ok()
                .map(PathBuf::from)
                .unwrap_or(self.workspace),
            l1_path: std::env::var("MEMORY_L1_PATH")
                .ok()
                .map(PathBuf::from)
                .or(self.l1_path),
            l1_wal_path: std::env::var("MEMORY_L1_WAL_PATH")
                .ok()
                .map(PathBuf::from)
                .or(self.l1_wal_path),
            gemini_api_key: std::env::var("GEMINI_EMBEDDINGS_TOKEN")
                .ok()
                .or(self.gemini_api_key),
            ..self
        }
    }

    /// 生成默认配置文件
    pub fn generate_template() -> String {
        r#"# Triple-Layer Memory System 配置

# Workspace 路径（.md 文件所在目录）
workspace = "."

# L0 工作内存最大条目数
l0_max_entries = 10000

# L1 持久化数据库路径（默认 /tmp/memory_l1.redb）
# l1_path = "/var/lib/memory-system/l1.redb"

# L1 WAL 路径（崩溃恢复用）
# l1_wal_path = "/var/lib/memory-system/wal"

# L2 最大条目数（触发淘汰）
l2_max_entries = 100000

# 语义搜索后端: gemini | onnx | none
embed_backend = "gemini"

# Gemini API Key（优先从环境变量 GEMINI_EMBEDDINGS_TOKEN 读取）
# gemini_api_key = "your-key-here"

# 混合召回权重: 0.0=纯向量, 1.0=纯BM25
hybrid_alpha = 0.6

# 背景同步间隔（秒）
sync_interval_secs = 30

# 日志级别: trace | debug | info | warn | error
log_level = "info"
"#
        .to_string()
    }
}
