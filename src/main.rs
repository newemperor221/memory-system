//! Triple-Layer Memory System for Multi-Agent
//!
//! 用法：
//!   cargo run -- serve --workspace /path/to/workspace
//!   cargo run -- remember "这是一条测试记忆"
//!   cargo run -- recall "测试"

mod l0;
mod l1;
mod l2;
mod common;
mod openclaw;

use anyhow::Result;
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub use common::{Entry, Importance, Layer, RecallRequest, RecallResult};
use crate::common::encoding::percent_decode;
pub use l0::WorkingMemory;
pub use l1::ShortTermMemory;
pub use l2::{EmbedBackend, LongTermMemory};
pub use openclaw::OpenClawBridge;

/// 三层记忆系统
pub struct MemorySystem {
    l0: Arc<WorkingMemory>,
    l1: Arc<ShortTermMemory>,
    pub l2: Arc<LongTermMemory>,
}

impl MemorySystem {
    pub async fn new() -> Result<Self> {
        let l0 = Arc::new(WorkingMemory::new());
        let l1 = Arc::new(ShortTermMemory::new().await?);
        let l2 = Arc::new(LongTermMemory::new().await?);
        Ok(Self { l0, l1, l2 })
    }

    pub async fn remember(&self, mut entry: Entry) -> Result<()> {
        if entry.id.is_empty() {
            entry.id = uuid::Uuid::new_v4().to_string();
        }
        entry.created_at = chrono::Utc::now();
        self.l0.write(entry.key.clone(), entry.clone()).await;
        self.l1.write(entry.key.clone(), entry.clone()).await?;
        self.l2.write(entry.key.clone(), entry.clone()).await;
        Ok(())
    }

    pub async fn recall(&self, req: RecallRequest) -> Vec<RecallResult> {
        self.l2.recall(&req, false).await
    }

    pub async fn recall_with_semantic(&self, req: RecallRequest) -> Vec<RecallResult> {
        self.l2.recall(&req, true).await
    }

    pub async fn recall_bm25(&self, query: &str, limit: usize) -> Vec<RecallResult> {
        self.l2.recall_bm25(query, limit).await
    }

    pub async fn get(&self, key: &str) -> Option<Entry> {
        if let Some(v) = self.l0.get(key) {
            return Some(v);
        }
        self.l1.get(key).await
    }

    pub async fn gc(&self) -> Result<()> {
        self.l1.gc().await?;
        self.l2.gc().await?;
        Ok(())
    }

    pub fn stats(&self) -> MemoryStats {
        MemoryStats {
            l0_entries: self.l0.len(),
            l1_entries: self.l1.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryStats {
    pub l0_entries: usize,
    pub l1_entries: usize,
}

// ─── CLI Entry Point ──────────────────────────────────────────────────

use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[clap(subcommand)]
    cmd: Command,
}

#[derive(Parser)]
enum Command {
    /// 启动 HTTP API 服务
    Serve {
        #[clap(long, default_value = ".")]
        workspace: std::path::PathBuf,
        #[clap(long, default_value = "127.0.0.1:7890")]
        listen: String,
    },
    /// 写入记忆
    Remember {
        #[clap(long, default_value = "normal")]
        importance: String,
        #[clap(last = true)]
        text: String,
    },
    /// 召回记忆（BM25）
    Recall {
        #[clap(long)]
        semantic: bool,
        #[clap(last = true)]
        query: String,
    },
    /// 查看状态
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.cmd {
        Command::Serve { workspace, listen } => {
            let bridge = OpenClawBridge::new_async(workspace).await?;
            bridge.load_workspace().await?;
            bridge.start_background_sync();
            let addr: std::net::SocketAddr = listen.parse()?;
            tracing::info!("记忆系统监听 {}", addr);
            let listener = tokio::net::TcpListener::bind(addr).await?;
            loop {
                if let Ok((mut stream, _peer)) = listener.accept().await {
                    let bridge = bridge.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(&mut stream, &bridge).await {
                            tracing::warn!("请求处理失败: {}", e);
                        }
                    });
                }
            }
        }
        Command::Remember { text, importance } => {
            let bridge = OpenClawBridge::new_async(".").await?;
            let imp = match importance.as_str() {
                "critical" => Importance::Critical,
                "high" => Importance::High,
                "low" => Importance::Low,
                _ => Importance::Normal,
            };
            bridge.remember_text(text, imp, vec![], "cli", Layer::Private).await?;
            println!("✓ 记忆已保存");
        }
        Command::Recall { semantic, query } => {
            let bridge = OpenClawBridge::new_async(".").await?;
            let results = if semantic {
                bridge.recall_semantic(&query, None).await
            } else {
                bridge.recall(&query, None).await
            };
            println!("找到 {} 条结果:", results.len());
            for r in results {
                println!("  [{}] {:.2} | {}", r.from_layer, r.score, r.entry.key);
                let preview = &r.entry.value[..r.entry.value.len().min(100)];
                println!("    {}\n", preview);
            }
        }
        Command::Status => {
            let bridge = OpenClawBridge::new_async(".").await?;
            let stats = bridge.stats();
            println!("L0: {} 条", stats.l0_entries);
            println!("L1: {} 条", stats.l1_entries);
        }
    }

    Ok(())
}

// ─── HTTP 请求处理 ──────────────────────────────────────────────────

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};

/// 单个请求超时（防止慢客户端占用连接）
const REQUEST_TIMEOUT_SECS: u64 = 10;
/// 最大请求体大小（1MB，防止内存耗尽）
const MAX_BODY_SIZE: usize = 1 * 1024 * 1024;

/// 读取 HTTP 请求行和 headers，返回 (method, path?query, headers_map)
async fn read_request_line(
    stream: &mut tokio::net::TcpStream,
) -> anyhow::Result<(String, String, std::collections::HashMap<String, String>)> {
    let mut header_buf = Vec::with_capacity(4096);
    let mut prev_was_crlf = false;

    // 逐字节读取直到遇到空行 \r\n\r\n
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        header_buf.push(byte[0]);

        if header_buf.len() > MAX_BODY_SIZE + 8192 {
            anyhow::bail!("HTTP headers 超出大小限制");
        }

        // 检测 \r\n\r\n 完整序列
        if byte[0] == b'\n' && prev_was_crlf {
            // 检查倒数第3、倒数第2个字节是不是 \r\n
            if header_buf.len() >= 4 {
                let last4 = [
                    header_buf[header_buf.len() - 4],
                    header_buf[header_buf.len() - 3],
                    header_buf[header_buf.len() - 2],
                    header_buf[header_buf.len() - 1],
                ];
                if last4 == [b'\r', b'\n', b'\r', b'\n'] {
                    break; // 完整的 \r\n\r\n
                }
            }
        }
        prev_was_crlf = byte[0] == b'\r';
    }

    let header_str = String::from_utf8_lossy(&header_buf);
    let mut lines = header_str.lines();

    let first_line = lines.next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        anyhow::bail!("无效请求行");
    }
    let method = parts[0].to_string();
    let path_with_query = parts[1].to_string();

    // 解析 headers
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_lowercase();
            let val = line[colon_pos + 1..].trim().to_string();
            headers.insert(key, val);
        }
    }

    Ok((method, path_with_query, headers))
}

async fn handle_connection(
    stream: &mut tokio::net::TcpStream,
    bridge: &OpenClawBridge,
) -> anyhow::Result<()> {
    // 读取请求行 + headers（带超时）
    let (method, path_with_query, headers) = match timeout(
        Duration::from_secs(REQUEST_TIMEOUT_SECS),
        read_request_line(stream),
    ).await {
        Ok(r) => match r {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("请求解析失败: {}", e);
                return Ok(());
            }
        },
        Err(_) => {
            tracing::warn!("请求读取超时");
            return Ok(());
        }
    };

    let path_parts: Vec<&str> = path_with_query.split('?').collect();
    let path = path_parts[0];
    let query_str = path_parts.get(1).unwrap_or(&"");

    // 解析 query string（同时解码 percent-encoded）
    let params: std::collections::HashMap<String, String> = query_str
        .split('&')
        .filter_map(|pair| {
            let mut kv = pair.split('=');
            let k = kv.next()?;
            let v = kv.next().unwrap_or("");
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect();

    // 从 headers 取 Content-Length
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX_BODY_SIZE);

    // API 密钥验证（环境变量 MEMORY_API_KEY，未设置则跳过）
    if let Some(expected_key) = std::env::var("MEMORY_API_KEY").ok() {
        let provided_key = headers.get("x-api-key").map(|s| s.as_str()).unwrap_or("");
        if provided_key != &expected_key {
            tracing::warn!("API 密钥验证失败 from {:?}", stream.peer_addr());
            let resp = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(resp.as_bytes()).await?;
            return Ok(());
        }
    }

    // 读取 body（POST 请求）
    let body = if method == "POST" && content_length > 0 {
        let mut buf = vec![0u8; content_length];
        match timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), stream.read_exact(&mut buf)).await {
            Ok(Ok(_n)) => {},
            Ok(Err(e)) => {
                tracing::warn!("POST body 读取失败: {}", e);
                return Ok(());
            }
            Err(_) => {
                tracing::warn!("POST body 读取超时");
                return Ok(());
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    } else {
        String::new()
    };

    let response = match (method.as_str(), path) {
        // GET /recall?query=xxx&semantic=true&layer=public|private
        ("GET", "/recall") => {
            let query = params.get("query").map(|s| s.as_str()).unwrap_or("");
            let semantic = params.get("semantic").map_or(false, |v| v == "true");
            let layer = params.get("layer").and_then(|v| match v.as_str() {
                "public" => Some(Layer::Public),
                "private" => Some(Layer::Private),
                _ => None,
            });
            let results = if semantic {
                bridge.recall_semantic(query, layer).await
            } else {
                bridge.recall(query, layer).await
            };
            json_response(&results)
        }

        // GET /get?key=xxx
        ("GET", "/get") => {
            match params.get("key").map(|s| s.as_str()) {
                Some(key) => {
                    match bridge.get(key).await {
                        Some(entry) => json_response(&entry),
                        None => json_response(&serde_json::json!({
                            "error": "not_found",
                            "key": key
                        })),
                    }
                }
                None => json_response(&serde_json::json!({
                    "error": "missing_param",
                    "param": "key"
                })),
            }
        }

        // GET /stats
        ("GET", "/stats") => {
            let stats = bridge.stats();
            json_response(&serde_json::json!({
                "l0_entries": stats.l0_entries,
                "l1_entries": stats.l1_entries,
                "l2_entries": bridge.memory.l2.len()
            }))
        }

        // GET /health
        ("GET", "/health") => {
            plain_response("OK")
        }

        // POST /remember
        ("POST", "/remember") => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            #[allow(dead_code)]
            struct RememberRequest {
                value: String,
                key: Option<String>,
                importance: Option<String>,
                tags: Option<Vec<String>>,
                /// Agent ID（必填）
                agent_id: String,
                /// 层级：public 或 private（默认 private）
                layer: Option<String>,
            }
            match serde_json::from_str::<RememberRequest>(&body) {
                Ok(req) => {
                    let imp = match req.importance.as_deref().unwrap_or("normal") {
                        "critical" => Importance::Critical,
                        "high" => Importance::High,
                        "low" => Importance::Low,
                        _ => Importance::Normal,
                    };
                    let layer = match req.layer.as_deref().unwrap_or("private") {
                        "public" => Layer::Public,
                        _ => Layer::Private,
                    };
                    bridge
                        .remember_text(req.value, imp, req.tags.unwrap_or_default(), &req.agent_id, layer)
                        .await?;
                    json_response(&serde_json::json!({
                        "ok": true,
                        "layer": if layer == Layer::Public { "public" } else { "private" },
                        "message": "记忆已保存"
                    }))
                }
                Err(e) => json_response(&serde_json::json!({
                    "error": "parse_error",
                    "detail": e.to_string()
                })),
            }
        }

        // POST /gc
        ("POST", "/gc") => {
            if let Err(e) = bridge.memory.gc().await {
                json_response(&serde_json::json!({
                    "error": "gc_failed",
                    "detail": e.to_string()
                }))
            } else {
                json_response(&serde_json::json!({
                    "ok": true,
                    "message": "GC 完成"
                }))
            }
        }

        _ => not_found(),
    };

    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

fn json_response<T: serde::Serialize>(data: &T) -> String {
    let body = serde_json::to_string(data).unwrap_or_else(|_| r#"{"error":"serialize_error"}"#.to_string());
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\n\r\n{}",
        body.len(),
        body
    )
}

fn plain_response(text: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        text.len(),
        text
    )
}

fn not_found() -> String {
    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
}

