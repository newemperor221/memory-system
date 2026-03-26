//! MemorySystem v2 — CLI + HTTP API entry point

use anyhow::Result;
use axum::{
    extract::Query,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;

use std::sync::Arc;
use tokio::net::TcpListener;

use memory_system_v2::{
    common::{Entry, Importance, Layer, RecallRequest},
    Config, MemorySystem,
};
use axum_server::tls_rustls::RustlsConfig;
#[derive(Parser)]
struct Cli {
    #[clap(subcommand)]
    cmd: Command,
}

#[derive(Parser)]
enum Command {
    /// Start HTTP API server
    Serve {
        #[clap(long, default_value = "127.0.0.1:7891")]
        listen: String,
        #[clap(long, default_value = ".")]
        workspace: String,
    },
    /// Write a memory entry
    Remember {
        #[clap(long)]
        key: String,
        #[clap(long)]
        value: String,
        #[clap(long, default_value = "normal")]
        importance: String,
        #[clap(long, default_value = "private")]
        layer: String,
        #[clap(long)]
        tags: Option<String>,
    },
    /// Recall memories
    Recall {
        #[clap(long)]
        query: String,
        #[clap(long, default_value = "false")]
        semantic: bool,
        #[clap(long, default_value = "10")]
        limit: usize,
    },
    /// Get system statistics
    Stats,
    /// Verify data integrity
    Verify,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.cmd {
        Command::Serve { listen, workspace } => {
            std::env::set_var("WORKSPACE", &workspace);

            let config = Config::default();
            let memory = Arc::new(MemorySystem::new(config).await?);

            // Build Axum router
            let app = Router::new()
                .route("/health", get(health_handler))
                .route("/stats", get(stats_handler))
                .route("/recall", get(recall_handler))
                .route("/remember", post(remember_handler))
                .route("/get", get(get_handler))
                .route("/delete", post(delete_handler))
                .route("/metrics", get(metrics_handler))
                .with_state(memory);

            let listen_addr: std::net::SocketAddr = listen
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid listen address: {}", e))?;
            let listener = TcpListener::bind(listen_addr).await?;
            tracing::info!("MemorySystem API listening on http://{}", listen_addr);

            axum::serve(listener, app).await?;
        }
        Command::Remember {
            key,
            value,
            importance,
            layer,
            tags,
        } => {
            let config = Config::default();
            let memory = MemorySystem::new(config).await?;

            let entry = Entry::new(
                key,
                value,
                Importance::from(importance.as_str()),
                tags.map(|s| s.split(',').map(String::from).collect())
                    .unwrap_or_default(),
                "cli".to_string(),
                Layer::from(layer.as_str()),
            );

            memory.remember(entry).await?;
            println!("✓ 记忆已保存");
        }
        Command::Recall {
            query,
            semantic,
            limit,
        } => {
            let config = Config::default();
            let memory = MemorySystem::new(config).await?;

            let req = RecallRequest {
                query,
                keys: None,
                agent_id: None,
                tags: None,
                limit: Some(limit),
                layer: None,
                semantic,
            };

            let results = memory.recall(req).await;
            println!("找到 {} 条结果:", results.len());
            for r in results {
                println!(
                    "  [{}] {:.2} | {} | {}",
                    r.from_layer, r.score, r.entry.key, r.entry.source
                );
                let preview = &r.entry.value[..r.entry.value.len().min(100)];
                println!("    {}\n", preview);
            }
        }
        Command::Stats => {
            let config = Config::default();
            let memory = MemorySystem::new(config).await?;
            let stats = memory.stats();
            println!("L0: {} 条", stats.l0_entries);
            println!("L1: {} 条", stats.l1_entries);
            println!("L2: {} 条 (向量: {}, pending: {})",
                stats.l2_entries, stats.l2_vectors_cached, stats.l2_pending_vectors);
            println!("L3: {} 个归档文件", stats.l3_archived_files);
        }
        Command::Verify => {
            let config = Config::default();
            let memory = MemorySystem::new(config).await?;
            let stats = memory.stats();
            println!("✓ L0: {}", stats.l0_entries);
            println!("✓ L1: {}", stats.l1_entries);
            println!("✓ L2: {}", stats.l2_entries);
            println!("✓ L3: {}", stats.l3_archived_files);
        }
    }

    Ok(())
}

// ─── HTTP Handlers ────────────────────────────────────────────────────────────

async fn health_handler(memory: axum::extract::State<Arc<MemorySystem>>) -> impl IntoResponse {
    let stats = memory.stats();
    if stats.layer_health.iter().any(|(_, s)| s.is_some()) {
        let issues: Vec<_> = stats.layer_health.iter()
            .filter_map(|(k, v)| v.as_ref().map(|vv| format!("{}: {}", k, vv)))
            .collect();
        (StatusCode::SERVICE_UNAVAILABLE, format!("DEGRADED: {}", issues.join(", ")))
    } else {
        (StatusCode::OK, "OK".to_string())
    }
}

async fn stats_handler(memory: axum::extract::State<Arc<MemorySystem>>) -> impl IntoResponse {
    Json(memory.stats())
}

async fn recall_handler(
    memory: axum::extract::State<Arc<MemorySystem>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let query = params.get("query").cloned().unwrap_or_default();
    let semantic = params.get("semantic").map(|v| v == "true").unwrap_or(false);
    let limit = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(10);

    let req = RecallRequest {
        query,
        keys: None,
        agent_id: params.get("agent_id").cloned(),
        tags: params.get("tags").cloned().map(|s|
            s.split(',').map(String::from).collect()
        ),
        limit: Some(limit),
        layer: params.get("layer").as_ref().map(|s| Layer::from(s.as_str())),
        semantic,
    };

    let results = memory.recall(req).await;
    Json(serde_json::json!({
        "ok": true,
        "results": results,
        "total": results.len(),
    }))
}

async fn remember_handler(
    memory: axum::extract::State<Arc<MemorySystem>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let key = match body.get("key").and_then(|v| v.as_str()) {
        Some(k) => k.to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false,
                "error": "missing_field",
                "message": "key is required"
            })))
        }
    };

    let value = match body.get("value").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false,
                "error": "missing_field",
                "message": "value is required"
            })))
        }
    };

    let importance = body
        .get("importance")
        .and_then(|v| v.as_str())
        .map(Importance::from)
        .unwrap_or(Importance::Normal);

    let layer = body
        .get("layer")
        .and_then(|v| v.as_str())
        .map(Layer::from)
        .unwrap_or(Layer::Private);

    let tags = body
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let entry = Entry::new(
        key,
        value,
        importance,
        tags,
        "http-api".to_string(),
        layer,
    );

    match memory.remember(entry).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "indexed": false,
            "note": "语义索引将在后台处理"
        }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "ok": false,
            "error": "write_failed",
            "message": e.to_string()
        }))),
    }
}

async fn get_handler(
    memory: axum::extract::State<Arc<MemorySystem>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let key = match params.get("key") {
        Some(k) => k,
        None => {
            let body = serde_json::json!({
                "ok": false,
                "error": "missing_param",
                "param": "key"
            });
            return Err((StatusCode::BAD_REQUEST, axum::response::Json(body)));
        }
    };

    match memory.get(key).await {
        Some(entry) => {
            let body = serde_json::json!({"ok": true, "entry": entry});
            Ok((StatusCode::OK, axum::response::Json(body)))
        }
        None => {
            let body = serde_json::json!({"ok": false, "error": "not_found", "key": key});
            Ok((StatusCode::NOT_FOUND, axum::response::Json(body)))
        }
    }
}

async fn delete_handler(
    memory: axum::extract::State<Arc<MemorySystem>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let key = match body.get("key").and_then(|v| v.as_str()) {
        Some(k) => k,
        None => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false,
                "error": "missing_field",
                "message": "key is required"
            })))
        }
    };

    match memory.delete(key).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({
            "ok": true
        }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "ok": false,
            "error": "delete_failed",
            "message": e.to_string()
        }))),
    }
}

async fn metrics_handler(memory: axum::extract::State<Arc<MemorySystem>>) -> impl IntoResponse {
    let stats = memory.stats();
    let body = format!(
        r#"# HELP memory_entries_total Memory entries per layer
# TYPE memory_entries_total gauge
memory_l0_entries_total {}
memory_l1_entries_total {}
memory_l2_entries_total {}
memory_l2_vectors_cached_total {}
memory_l2_pending_vectors_total {}
memory_l3_archived_files_total {}
"#,
        stats.l0_entries,
        stats.l1_entries,
        stats.l2_entries,
        stats.l2_vectors_cached,
        stats.l2_pending_vectors,
        stats.l3_archived_files,
    );
    axum::response::Response::builder()
        .header("Content-Type", "text/plain; version=0.0.4")
        .body(axum::body::Body::from(body))
        .unwrap()
}
