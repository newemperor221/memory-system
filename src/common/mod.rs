//! 公共数据类型，三层共享

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub(crate) mod encoding;

/// 记忆条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// 唯一 ID
    pub id: String,
    /// 访问 key，格式如 "agent:session:topic"
    pub key: String,
    /// 原始文本内容
    pub value: String,
    /// 重要性评分，影响 L2 淘汰优先级
    pub importance: Importance,
    /// 标签，用于过滤
    pub tags: Vec<String>,
    /// 来源标识（Agent ID）
    pub source: String,
    /// 记忆层级：private（默认）或 public
    pub layer: Layer,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 最后访问时间（用于 LRU 淘汰）
    pub last_accessed: DateTime<Utc>,
    /// 过期时间（None = 不过期）
    pub expires_at: Option<DateTime<Utc>>,
}

impl Entry {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            key: key.into(),
            value: value.into(),
            importance: Importance::Normal,
            tags: Vec::new(),
            source: "memory-system".to_string(),
            layer: Layer::Private,
            created_at: now,
            last_accessed: now,
            expires_at: None,
        }
    }
}

/// 记忆层级：私有（仅创建者）/ 公共（所有 Agent 共享）
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    #[default]
    Private,
    Public,
}

impl Layer {
    pub fn prefix(&self) -> &'static str {
        match self {
            Layer::Private => "private",
            Layer::Public => "public",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Importance {
    Critical = 4,
    High = 3,
    Normal = 2,
    Low = 1,
}

/// 召回请求
#[derive(Debug, Clone)]
pub struct RecallRequest {
    /// 语义查询文本
    pub query: String,
    /// 精确匹配的 key 前缀（可选）
    pub keys: Option<Vec<String>>,
    /// 指定 agent ID（可选）
    pub agent_id: Option<String>,
    /// 指定标签过滤（可选）
    pub tags: Option<Vec<String>>,
    /// 返回上限
    pub limit: Option<usize>,
    /// 限定召回哪层记忆（None = 两层都搜）
    pub layer: Option<Layer>,
}

/// 召回结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResult {
    pub entry: Entry,
    /// 相关性得分（0.0 ~ 1.0）
    pub score: f32,
    /// 召回来源层
    pub from_layer: &'static str,
}

impl RecallResult {
    pub fn new(entry: Entry, score: f32, from_layer: &'static str) -> Self {
        Self {
            entry,
            score,
            from_layer,
        }
    }
}
