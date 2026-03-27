# memory-agent — 3 Agent 记忆系统 v3

## 用途
OpenClaw 内置记忆系统，服务于 3 个 Agent 的不同需求：

| Agent | 角色 | 记忆需求 |
|-------|------|---------|
| **boss** | 协调/调度 | 重要事实摘要（importance >= 3） |
| **code** | 写代码/系统 | 技术记忆（source=code, tag=code/config） |
| **content** | 写小说 | 世界观 + 情感状态 + 实时角色心理 + 决策提示 |

---

## 核心设计原则

### 1. 数据源隔离（最重要）
不同 Agent 只能读到自己应该看到的数据，**不在 prompt 里做过滤规则**。

```
boss   → entries (importance >= 3)
code   → entries (source='code', tag in code/config)
content → entries (source='content', tag in world/plot)
        + emotion_events（全部）
        + character_states（全部）
        + character_parameters（全部）
        + relationship_states（全部）
```

### 2. 状态是算出来的，不是写进去的
character_state 由 State Builder 实时计算，emotion_events 是唯一真实来源。

### 3. Decision Layer 是降维，不是推理
Decision Layer 输出连续驱动力值，让 LLM 解释行为。避免离散标签爆炸。

### 4. 参数演化带惯性（Inertia）
性格参数每次只变化 10%，防止因单次事件剧烈跳变。

### 5. Recent Override
最近事件强度 > 0.8 时，自动覆盖主导情绪。避免 100 次轻微 + 1 次强烈 = "整体偏轻微" 的失真。

---

## content agent 标准工作流

### 章节开始前
```python
# 1. 召回世界观事实
facts = memory.recall_fact(query="当前剧情", agent_id="content")

# 2. 构建角色心理状态
state = memory.build_character_state(character="男主", current_chapter="chapter_12")

# 3. 获取关系状态
rel = memory.get_relationship("男主", "女主")

# 4. 生成决策提示（可选）
from memory.decision import build_decision
decision = build_decision(
    mood_vector=state.mood_vector,
    trust_levels=state.trust_levels,
    emotional_wounds=state.emotional_wounds,
    hidden_feeling=state.hidden_feeling,
    attachment_style="回避型",  # 从 novel_world 读取
    risk_tolerance=0.3,
    params={}  # 从 character_parameters 读取
)

# state.to_prompt_text() → 注入 prompt
# decision.to_prompt_text() → 注入 prompt
```

### 章节结束后
```python
memory.write_chapter_memory(
    character="男主",
    chapter="chapter_12",
    scene_summary="他看到她帮别人挡了麻烦...",
    emotion_vector={"感动": 0.8, "紧张": 0.3},
    intensity=0.85,
    event_type="trust_shift",
    trust_changes={"女主": 0.15},  # 可选
)
# 等价于：remember_emotion() + build_character_state() + update_relationship()
```

---

## API Reference

### 事实层

#### 写事实记忆
```
memory.remember_fact(
    key="...",         # 唯一键，格式: layer:category:sub:id
    value="...",       # 内容
    source="boss",     # boss | code | content
    layer="private",   # private | public
    importance=2,      # 1=low 2=normal 3=high 4=critical
    tags=["world"],   # world | plot | code | config | temp
    agent_id="boss"
)
```

#### 读事实记忆
```
memory.recall_fact(
    query="爬虫 状态",
    agent_id="code",   # boss | code | content
    limit=20
)
→ 返回过滤后的记忆列表（关键词评分排序，非 LIKE）
```

---

### 情感层（content 专用）

#### 写情感事件
```
memory.remember_emotion(
    event=EmotionEvent(
        character="男主",
        chapter="chapter_12",
        event_type="trust_shift",    # 见枚举
        event_raw="她帮他挡了一次麻烦",
        emotion_vector={"感动": 0.8, "紧张": 0.3},
        intensity=0.85,              # 0.0~1.0
        decay_rate=0.95,            # 衰减率
        subjective_desc="她挡在我面前的样子...",
        impression_delta={"女主": "有点冷→声音好听"},
        bias_formed="她其实很温柔",
        decision_tendency="主动",
        related_fact_keys=["fact_key1"]
    )
)
→ 返回事件 ID
→ 不自动触发 State Builder（由调用方显式触发）
```

#### 构建角色状态（核心）
```
state = memory.build_character_state(
    character="男主",
    current_chapter="chapter_12",
    force_full=False   # False=增量，True=全量重算
)
→ 返回 CharacterState
→ 自动保存到 character_states 表

# CharacterState 字段：
{
    "character": "男主",
    "mood_vector": {"主导": "感动", "次级": ["紧张"], "magnitude": 0.7},
    "trust_levels": {"女主": 0.65, "反派": 0.1},
    "emotional_wounds": ["失恋"],
    "defense_mechanisms": ["用冷淡掩饰"],
    "hidden_feeling": "自卑",
    "self_perception": "我不擅长表达情感",
    "decision_bias": "主动",
    "growth_arc": "逃避→面对",
    "state_version": 3,
    "last_chapter": "chapter_12"
}
```

#### 组合写入（推荐）
```
memory.write_chapter_memory(...)  # 等价于 remember_emotion + build_character_state + update_relationship
```

---

### Decision Layer（轻量降维）

```python
from memory.decision import build_decision

decision = build_decision(
    mood_vector=state.mood_vector,
    trust_levels=state.trust_levels,
    emotional_wounds=state.emotional_wounds,
    hidden_feeling=state.hidden_feeling,
    attachment_style="回避型",    # 回避型 | 焦虑型 | 安全型
    risk_tolerance=0.3,          # 0.0~1.0
    params={}                    # character_parameters 当前值
)

# Decision 输出：
{
    "approach_drive": 0.62,     # 接近动机 0.0~1.0
    "avoidance_drive": 0.71,    # 回避动机 0.0~1.0
    "confidence": 0.40,        # 行动信心 0.0~1.0
    "dominant_emotion": "紧张",
    "magnitude": 0.7,
    "emotional_tone": "拘谨",
    "tone_intensity": 0.49
}

# 注入 prompt：
print(decision.to_prompt_text())
# 驱动力状态：
#   接近动机：0.62 (中)
#   回避动机：0.71 (强)
#   行动信心：0.40
# 情绪基调：拘谨（强度 0.5）
```

---

### 关系状态

```
memory.update_relationship(
    char_a="男主", char_b="女主",
    delta_intimacy=0.1,
    delta_trust=0.15,
    delta_tension=-0.05,
    chapter="chapter_12",
    trigger_event_id="uuid"
)

memory.get_relationship("男主", "女主")
→ {"intimacy": 0.6, "trust": 0.65, "tension": 0.2}
```

---

### 世界观设定

```
memory.set_world(key="女主-人设", category="人物", value="...", author_notes="...")
memory.get_world(key="女主-人设")
```

---

### 统计

```
memory.stats()
→ {"fact_entries": 50, "emotion_events": 120, "characters": 5, "relationships": 10}
```

---

## State Builder 算法要点

### 情绪衰减
```
effective = intensity × (decay_rate ^ Δchapter)
```

### Recent Override（修正历史加权失真）
```
# 如果最近事件 intensity > 0.8，用它的主导情绪覆盖 dominant
if latest_event.intensity > 0.8:
    dominant = latest_event.primary_emotion
```

### trust 非线性递推
```
trust(t+1) = clamp(
    trust(t) + delta × (1 - |trust(t) - 0.5|),
    0.0, 1.0
)
# 越接近极端值（0或1）越难改变
```

### magnitude 计算
```
magnitude = Σ(emotion_pool.values()) / len(emotion_pool)
# 用于判断"是否该触发行为变化"
```

---

## Decision Layer 映射函数（无 if-else 树）

```
approach = trust_avg + risk_tolerance × 0.3 + magnitude × 0.2 - attachment_avoidance
avoidance = wound_factor + (1-magnitude) × 0.2 + attachment_avoidance
confidence = trust_avg - magnitude × 0.3 + risk_tolerance × 0.2
```

---

## 行为参数演化（Inertia）

```
new_value = old_value × 0.9 + delta × 0.1
# 每次只变化 10%，防止性格跳变
# 边界 clamp：approach_threshold ∈ [0.1, 0.6]
```

---

## event_type 枚举

```
first_impression   初遇印象
trust_shift        信任变化（上升/下降）
emotional_wound    情感创伤
breakthrough       突破时刻
secret_reveal      秘密揭露
conflict_climax    冲突高潮
comfort_received   被安慰/被帮助
betrayal           背叛/失望（触发参数漂移）
growth_marker      成长标记
bias_formed        偏见形成（沉淀 self_perception）
bias_shattered     偏见打破
decision_point     关键决策
defense_reveal     防御机制显现
rejection          被拒绝（触发 approach_threshold +0.05）
```

---

## 数据隔离规则（机器执行，非 prompt）

| 维度 | boss | code | content |
|------|------|------|---------|
| entries 读 | importance≥3 | source=code + tag | source=content + tag |
| entries 写 | public/private | private | private |
| emotion_events | ❌ | ❌ | ✅ |
| character_states | ❌ | ❌ | ✅ |
| character_parameters | ❌ | ❌ | ✅ |
| relationship_states | ❌ | ❌ | ✅ |
| novel_world | ❌ | ❌ | ✅ |

---

## 性能说明

- State Builder：每次最多处理最近 50 条事件（EMOTION_WINDOW）
- 增量计算：`last_processed_event_id` 做游标，不重复扫已处理事件
- SQLite WAL 模式，支持多线程并发读
- 无外部进程，所有操作在 OpenClaw 进程内完成
- Decision Layer 是纯数值映射，无 LLM 调用，零额外开销

---

## 文件结构

```
memory-agent/
├── SKILL.md              ← 本文件
├── DESIGN.md             ← 完整设计文档
├── memory/
│   ├── __init__.py
│   ├── core.py           ← MemorySystem + EmotionEvent + CharacterState
│   ├── decision.py       ← Decision Layer（连续驱动力）
│   ├── hooks.py          ← inject_character_context + write_chapter_memory
│   └── schema.sql       ← 建表语句（含 character_parameters 表）
└── tests/
    └── test_memory.py   ← 全部测试用例
```
