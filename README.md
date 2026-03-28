# memory-agent

OpenClaw 三层记忆系统 v3 — 支持多 Agent 共享的情感记忆系统。

## 架构

```
Memory System (SQLite)
  ├── L0: 事实记忆 (facts) — agent 读写，高优先级持久化
  ├── L1: 近期事件 (recent_events) — 自动滚动窗口
  └── L2: 长期记忆 (long_term_memory) — 重要事件持久化

Character State Builder
  └── 从 emotion_events 重建角色心理状态

Decision Layer
  └── 输出驱动力数值 (approach_drive, avoidance_drive, confidence)

Hooks (自动注入)
  ├── inject_character_context() — 写作前注入角色心理
  └── write_chapter_memory() — 写作后沉淀情感记忆
```

## 核心文件

| 文件 | 说明 |
|------|------|
| `memory/core.py` | MemorySystem 核心类，SQLite 读写 |
| `memory/hooks.py` | inject_character_context / write_chapter_memory |
| `memory/decision.py` | Decision Layer 驱动力计算 |
| `memory/schema.sql` | 数据库 Schema |
| `SKILL.md` | 使用文档 |
| `DESIGN.md` | 设计文档 |
| `tests/test_memory.py` | 单元测试 |

## Quick Start

```python
import sys
sys.path.insert(0, '/home/woioeow/.openclaw/skills/memory-agent')
from memory.core import get_system
from memory.hooks import inject_character_context, write_chapter_memory

mem = get_system()

# 写作前：注入角色心理
enhanced_prompt = inject_character_context(
    agent_id="content",
    character="林晖",
    chapter="chapter_10",
    current_prompt=your_prompt,
    mem=mem
)

# 写作后：沉淀情感记忆
write_chapter_memory(
    character="林晖",
    chapter="chapter_10",
    scene_summary="...",
    emotion_vector={"恐惧": 0.6, "信任感": 0.8},
    trust_changes={"赵灵儿": +0.15},
    event_type="trust_shift",
    intensity=0.75,
    mem=mem
)
```

## 三个 Agent 的职责

| Agent | 读写范围 |
|-------|---------|
| boss | 只读 importance≥3 的 fact，项目级记忆 |
| code | 读写 source=code 的 fact，技术记忆 |
| content | 完整 emotion_events + decision layer，写作记忆 |

## 数据库

- 路径：`memory/memory.db`
- 隔离：agent_id 字段隔离，非 prompt 级别
- 驱动力：连续浮点数输出，非离散标签
