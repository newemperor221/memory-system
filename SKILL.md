---
name: memory-system
description: 团队共享三层记忆系统。记东西、搜记忆、管理公私层级。所有 Agent 协作的知识库。
---

# 🧠 Memory-System — 团队共享三层记忆

## 概述

memory-system 是一个 Rust 编写的三层记忆系统，支持：
- L0 工作内存（内存级速度）
- L1 短时持久化（sled 数据库）
- L2 长期语义搜索（BM25 + Gemini 向量）

所有 Agent 可通过 HTTP API 或 Python 客户端共享记忆。

---

## 目录结构

```
/home/woioeow/.openclaw/workspace/code/skills/memory-system/
├── memory-system          # 编译好的二进制程序
├── scripts/
│   ├── memory-tool.py    # Python 客户端工具
│   ├── start.sh          # 启动脚本
│   ├── stop.sh           # 停止脚本
│   └── build.sh          # 编译脚本
├── src/                  # Rust 源代码
├── SKILL.md              # 本文档
└── _meta.json
```

**数据目录：**
```
/home/woioeow/.openclaw/workspace/
├── .memory-l1/           # L1 数据库（sled）
└── memory/               # L0 缓存 + L2 索引
```

---

## 快速使用

### 启动服务

```bash
cd /home/woioeow/.openclaw/workspace/code/skills/memory-system/scripts
./start.sh
```

### 写入记忆

```bash
cd /home/woioeow/.openclaw/workspace/code/skills/memory-system/scripts
MEMORY_AGENT_ID=aicode ./memory-tool.py remember "记忆内容" [high|normal|low] [public|private]
```

### 搜索记忆

```bash
cd /home/woioeow/.openclaw/workspace/code/skills/memory-system/scripts
MEMORY_AGENT_ID=aicode ./memory-tool.py recall "搜索关键词" [条数] [public|private|all]
```

### 查看状态

```bash
./memory-tool.py stats
```

---

## API 接口

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` | 健康检查 |
| GET | `/stats` | 系统状态（L0/L1/L2 条数） |
| GET | `/recall?query=xxx&layer=public&semantic=true` | 召回记忆 |
| GET | `/get?key=xxx` | 精确获取 |
| POST | `/remember` | 写入记忆 |

### 召回参数

| 参数 | 说明 |
|------|------|
| `query` | 搜索关键词 |
| `layer` | `public` / `private` / `all` |
| `semantic` | `true` 启用 Gemini 向量搜索 |

---

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MEMORY_AGENT_ID` | `unknown` | 当前 Agent ID（必填） |
| `MEMORY_API_KEY` | 空 | API 密钥验证 |
| `MEMORY_BASE_URL` | `http://127.0.0.1:7890` | 服务地址 |
| `MEMORY_WORKSPACE` | `.` | workspace 路径 |
| `MEMORY_L1_PATH` | `/tmp/memory_l1` | **重要**：L1 数据库路径，必须设置 |

---

## 重要配置

### 启动脚本（start.sh）

**注意**：必须设置 `MEMORY_L1_PATH` 环境变量，否则数据库会放到 `/tmp/`：

```bash
mkdir -p "$WORKSPACE/.memory-l1"

MEMORY_L1_PATH="$WORKSPACE/.memory-l1" \
MEMORY_WORKSPACE="$WORKSPACE" \
nohup "$DIR/../memory-system" serve \
    --workspace "$WORKSPACE" \
    --listen "$LISTEN" \
    </dev/null > "$LOG" 2>&1 &
```

### 向量搜索配置

向量搜索使用 Gemini Embeddings API，API Key 从环境变量 `GEMINI_EMBEDDINGS_TOKEN` 读取。

服务地址：`127.0.0.1:7890`

---

## 服务管理（systemd）

已配置 systemd 用户服务，支持开机自启和进程崩溃自动重启。

### systemd 服务文件

位置：`~/.config/systemd/user/memory-system.service`

```ini
[Unit]
Description=Memory-System (Triple-Layer Memory)
After=network.target

[Service]
ExecStart=/home/woioeow/.openclaw/workspace/code/skills/memory-system/memory-system serve --workspace /home/woioeow/.openclaw/workspace --listen 127.0.0.1:7890
Restart=always
RestartSec=5
TimeoutStopSec=30
Environment=MEMORY_L1_PATH=/home/woioeow/.openclaw/workspace/.memory-l1
Environment=MEMORY_WORKSPACE=/home/woioeow/.openclaw/workspace
Environment=HOME=/home/woioeow

[Install]
WantedBy=default.target
```

### 服务管理命令

```bash
# 启动服务
systemctl --user start memory-system

# 停止服务
systemctl --user stop memory-system

# 重启服务
systemctl --user restart memory-system

# 查看状态
systemctl --user status memory-system

# 开机自启（已启用）
systemctl --user enable memory-system

# 禁用开机自启
systemctl --user disable memory-system
```

### HTTP API 状态检查

```bash
# 健康检查
curl http://127.0.0.1:7890/health

# 查看统计
curl http://127.0.0.1:7890/stats
```

**说明**：服务已配置 `Restart=always`，进程崩溃后会自动在 5 秒后重启。

---

## 架构说明

### 三层记忆

| 层级 | 存储 | 速度 | 说明 |
|------|------|------|------|
| L0 | 内存 | ⚡最快 | 当前会话工作记忆 |
| L1 | sled 数据库 | ⚡快 | 短时持久化，mmap 加速 |
| L2 | 向量+BM25 | 🔍搜索 | 长期记忆，支持语义搜索 |

### 公私层级

| 层级 | 说明 |
|------|------|
| `public` | 所有 Agent 共享 |
| `private` | 只有写入者可见 |

### 召回结果标记

| 标记 | 说明 |
|------|------|
| `🔒` | 私有记忆 |
| `🟢` | 公共记忆 |
| `L2:BM25` | 关键词搜索 |
| `L2:Hybrid` | 语义+关键词混合 |

---

## Layer 使用建议

| 场景 | Layer |
|------|-------|
| 团队决策、技术方案 | `public` |
| 会议结论、工作安排 | `public` |
| 项目背景、上下文 | `public` |
| 个人笔记、偏好 | `private` |
| 敏感信息 | `private` |

---

## OpenClaw Agent 集成

OpenClaw 内置的 `memory_search` 工具是两套独立的系统：
- **内置 memory_search**：索引工作区的 `.md` 文件
- **memory-system**：独立的 HTTP 服务，三层记忆

两者可以并存使用。

如需配置 OpenClaw 内置 memory_search 使用 Gemini 向量，在 `openclaw.json` 中添加：

```json
{
  "agents": {
    "defaults": {
      "memorySearch": {
        "enabled": true,
        "provider": "gemini",
        "model": "gemini-embedding-001",
        "sources": ["memory"]
      }
    }
  }
}
```

---

## 故障排查

### 服务无法启动
```bash
# 检查端口占用
curl http://127.0.0.1:7890/health

# 查看日志
cat /tmp/memory-system.log
```

### 向量搜索返回空
- 检查 `GEMINI_EMBEDDINGS_TOKEN` 环境变量是否设置
- 确认服务启动时加载了正确的 API Key

### 数据库路径问题
- 确认 `MEMORY_L1_PATH` 设置为 `/home/woioeow/.openclaw/workspace/.memory-l1/`
- 旧数据在 `/tmp/memory_l1/`（可删除）

---

## 相关文件

- 服务程序：`/home/woioeow/.openclaw/workspace/code/skills/memory-system/memory-system`
- 启动脚本：`/home/woioeow/.openclaw/workspace/code/skills/memory-system/scripts/start.sh`
- 数据库：`/home/woioeow/.openclaw/workspace/.memory-l1/`
