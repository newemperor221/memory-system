# Memory-System

三重层记忆系统 | Triple-Layer Memory System

## 特性

- 🧠 **L0 工作内存** - 内存级速度，当前会话缓存
- 💾 **L1 短时记忆** - sled 数据库持久化
- 🔍 **L2 长期搜索** - BM25 + Gemini 向量语义搜索

## 快速开始

### 编译

```bash
cargo build --release
```

### 启动服务

```bash
./target/release/memory-system serve --workspace /path/to/workspace --listen 127.0.0.1:7890
```

### 环境变量

| 变量 | 说明 |
|------|------|
| `MEMORY_L1_PATH` | L1 数据库路径 |
| `MEMORY_WORKSPACE` | workspace 路径 |
| `GEMINI_EMBEDDINGS_TOKEN` | Gemini API Key（用于向量搜索） |
| `MEMORY_API_KEY` | API 密钥（可选） |

### 使用示例

```bash
# 写入记忆
curl -X POST http://127.0.0.1:7890/remember \
  -H "Content-Type: application/json" \
  -d '{"value": "test memory", "importance": "normal", "layer": "public"}'

# 搜索记忆
curl "http://127.0.0.1:7890/recall?query=test&layer=public"
```

## API

- `GET /health` - 健康检查
- `GET /stats` - 系统状态
- `GET /recall?query=xxx&layer=public` - 召回记忆
- `POST /remember` - 写入记忆

## License

MIT
