# memory-agent 完整设计方案 v3

> 3 Agent 团队记忆系统 — 2026-03-27
> 架构评审：boss（协调）、code（工程）、content（网文）

---

## 一、设计目标

让写小说的 Agent（content）能够：
- 记住情感事件（不是日志，是可推导的状态）
- 角色按"自己以为的自己"行动，而不是按真实状态
- 同一事件 → 不同角色 → 不同行为（靠参数差异，不是硬编码）
- 随剧情发展自然演化（反馈回路）

同时不影响 code 和 boss 正常工作。

---

## 二、架构全貌

```
┌─────────────────────────────────────────────────────────┐
│                    OpenClaw Agent                       │
├─────────────┬─────────────────────┬────────────────────┤
│   boss      │       code          │      content        │
│  (协调)     │     (工程)          │     (网文)          │
└──────┬──────┴──────────┬──────────┴─────────┬──────────┘
       │                 │                    │
       ▼                 ▼                    ▼
┌─────────────────────────────────────────────────────────┐
│                    Memory System                         │
│                    (同进程 SQLite)                       │
│                                                         │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐ │
│  │  entries    │  │  emotion_   │  │  character_     │ │
│  │  (事实)     │  │  events     │  │  parameters     │ │
│  └──────┬──────┘  └──────┬──────┘  └────────┬────────┘ │
│         │                │                    │          │
│         │         ┌─────▼────────────────────▼─────┐    │
│         │         │       State Builder           │    │
│         │         │  (确定性, 衰减, 非线性 trust)  │    │
│         │         └─────────────┬──────────────────┘    │
│         │                       │                       │
│         │              ┌────────▼────────┐               │
│         │              │  Decision Layer │               │
│         │              │  (降维, 不推理) │               │
│         │              └────────┬────────┘               │
│         │                       │                        │
│         │              ┌────────▼────────┐               │
│         │              │ Auto Injection  │               │
│         │              │ (Pre-task hook) │               │
│         │              └─────────────────┘               │
│         │                       │                        │
└─────────┼───────────────────────┼────────────────────────┘
          │                       │
          ▼                       ▼
   boss / code 读            content 写作
   filtered fact           → 角色心理状态
                              + 决策提示
                              + 世界设定
                                  ↓
                              LLM 生成
```

---

## 三、数据层（SQLite Schema）

### 3.1 entries（事实记忆，跨 Agent 共享）

```sql
CREATE TABLE entries (
    key              TEXT PRIMARY KEY,
    id               TEXT NOT NULL,
    value            TEXT NOT NULL,
    importance       INTEGER NOT NULL DEFAULT 2,  -- 1=low 2=normal 3=high 4=critical
    source           TEXT NOT NULL,               -- boss | code | content
    layer            TEXT NOT NULL DEFAULT 'private',  -- private | public
    tags             TEXT NOT NULL DEFAULT '[]',  -- JSON: ["world","plot","code","config"]
    created_at       INTEGER NOT NULL,
    last_accessed    INTEGER NOT NULL,
    seq              INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_entries_source  ON entries(source);
CREATE INDEX idx_entries_importance ON entries(importance);
```

### 3.2 emotion_events（情感事件，content 专用）

```sql
CREATE TABLE emotion_events (
    id               TEXT PRIMARY KEY,
    character        TEXT NOT NULL,
    chapter          TEXT NOT NULL,
    event_type       TEXT NOT NULL,    -- 枚举见 SKILL.md
    event_raw        TEXT NOT NULL,
    emotion_vector    TEXT NOT NULL,   -- JSON: {"紧张": 0.8, "好奇": 0.5}
    intensity        REAL NOT NULL,    -- 0.0~1.0 原始强度
    decay_rate       REAL NOT NULL DEFAULT 0.95,
    subjective_desc  TEXT,
    impression_delta TEXT,             -- JSON: {"女主": "有点冷→声音好听"}
    bias_formed      TEXT,
    decision_tendency TEXT,
    related_fact_keys TEXT,
    created_at       INTEGER NOT NULL
);
CREATE INDEX idx_emotion_character ON emotion_events(character);
CREATE INDEX idx_emotion_chapter   ON emotion_events(chapter);
CREATE INDEX idx_emotion_created   ON emotion_events(created_at DESC);
```

### 3.3 character_states（状态快照，可重建）

```sql
CREATE TABLE character_states (
    character        TEXT PRIMARY KEY,
    mood_vector      TEXT NOT NULL,    -- {"主导": "压抑", "次级": ["不安","渴望"]}
    trust_levels     TEXT NOT NULL,    -- {"女主": 0.65, "反派": 0.1}
    emotional_wounds TEXT NOT NULL DEFAULT '[]',
    defense_mechanisms TEXT NOT NULL DEFAULT '[]',
    hidden_feeling   TEXT NOT NULL DEFAULT '',
    self_perception  TEXT NOT NULL DEFAULT '',  -- "觉得自己不擅长表达情感"
    decision_bias    TEXT NOT NULL DEFAULT '',
    growth_arc       TEXT NOT NULL DEFAULT '',
    is_derived       INTEGER NOT NULL DEFAULT 1,  -- 1=计算 0=手工
    state_version    INTEGER NOT NULL DEFAULT 1,
    last_chapter     TEXT NOT NULL DEFAULT '',
    last_processed_event_id TEXT,
    last_processed_event_seq INTEGER,
    last_updated     INTEGER NOT NULL
);
```

### 3.4 character_parameters（行为参数，动态演化）

```sql
CREATE TABLE character_parameters (
    character        TEXT PRIMARY KEY,
    -- 行为阈值
    approach_threshold  REAL NOT NULL DEFAULT 0.3,
    avoidance_threshold REAL NOT NULL DEFAULT -0.3,
    emotion_trigger_threshold REAL NOT NULL DEFAULT 0.7,
    -- 性格系数
    risk_tolerance   REAL NOT NULL DEFAULT 0.5,
    attachment_style TEXT NOT NULL DEFAULT '安全型',  -- 回避型 | 焦虑型 | 安全型
    -- 边界
    approach_floor   REAL NOT NULL DEFAULT 0.1,
    approach_ceiling REAL NOT NULL DEFAULT 0.6,
    -- 元数据
    last_adjusted    INTEGER NOT NULL
);
```

### 3.5 relationship_states + relationship_dynamics

```sql
CREATE TABLE relationship_states (
    character_pair   TEXT PRIMARY KEY,   -- "角色A|角色B" 字典序
    intimacy         REAL NOT NULL DEFAULT 0.0,
    trust            REAL NOT NULL DEFAULT 0.5,  -- 基准 0.5
    tension          REAL NOT NULL DEFAULT 0.0,
    dominance        REAL NOT NULL DEFAULT 0.5,
    last_chapter     TEXT NOT NULL DEFAULT '',
    last_updated     INTEGER NOT NULL
);

CREATE TABLE relationship_dynamics (
    id               TEXT PRIMARY KEY,
    character_pair   TEXT NOT NULL,
    event_type       TEXT NOT NULL,
    from_state       TEXT NOT NULL,   -- JSON 快照
    to_state         TEXT NOT NULL,
    delta            TEXT NOT NULL,
    trigger_event_id TEXT,
    transition_type  TEXT NOT NULL,   -- gradual | sudden | pivot
    chapter          TEXT NOT NULL,
    created_at       INTEGER NOT NULL
);
CREATE INDEX idx_rel_pair ON relationship_dynamics(character_pair);
```

### 3.6 novel_world（世界观设定，静态参考）

```sql
CREATE TABLE novel_world (
    key              TEXT PRIMARY KEY,
    category         TEXT NOT NULL,   -- 人物 | 地点 | 势力 | 规则
    value            TEXT NOT NULL,
    author_notes     TEXT,
    consistency_hints TEXT,
    version          INTEGER NOT NULL DEFAULT 1,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
```

---

## 四、Agent 隔离规则（数据源级别，非 Prompt）

| 维度 | boss | code | content |
|------|------|------|---------|
| entries 读 | importance≥3 | source=code + tag∈{code,config} | source=content + tag∈{world,plot} |
| entries 写 | public/private | private | private |
| emotion_events | ❌ | ❌ | ✅ |
| character_states | ❌ | ❌ | ✅（自动构建） |
| character_parameters | ❌ | ❌ | ✅（自动演化） |
| relationship_states | ❌ | ❌ | ✅ |
| novel_world | ❌ | ❌ | ✅ |

---

## 五、State Builder（核心）

### 5.1 输入
- `emotion_events`（该角色所有事件，滑动窗口 50 条）
- `character_parameters`（当前参数）

### 5.2 输出
`CharacterState`（见 3.3）

### 5.3 关键算法

**情绪衰减：**
```
effective = intensity × (decay_rate ^ Δchapter)
```

**情绪合并（归一化基准，防爆炸）：**
```
emotion_pool[em] = Σ(val × effective)
# 不做全局归一化，保留绝对强度供 magnitude 使用
```

**trust 非线性递推：**
```
trust(t+1) = clamp(
    trust(t) + delta × (1 - |trust(t) - 0.5|),
    0.0, 1.0
)
```
> 越接近极端值越难改变

**magnitude 计算：**
```
magnitude = Σ(emotion_pool.values()) / len(emotion_pool)
```
> 用于判断"是否该触发行为变化"

**self_perception**：
> 由 bias_formed 事件自动沉淀，非手工写入

---

## 六、Decision Layer（轻量降维）

### 6.1 输入
`CharacterState` + `character_parameters`

### 6.2 输出
```python
{
    "intent": "想接近但保持距离",
    "action_bias": "被动回应",   # 主动 | 回避 | 犹豫
    "emotional_tone": "克制",    # 压抑 | 低落 | 克制 | 拘谨
    "urgency": "中"              # 高 | 中 | 低
}
```

### 6.3 映射函数（无 if-else 树）

```python
def build_decision(state: CharacterState, params: CharacterParameters) -> Decision:
    approach = state.approach_score()   # 0.0~1.0
    avoidance = state.avoidance_score() # 0.0~1.0
    delta = approach - avoidance

    # 1. 行为倾向（阈值来自参数层）
    if delta > params.approach_threshold:
        bias = "主动"
    elif delta < params.avoidance_threshold:
        bias = "回避"
    else:
        bias = "犹豫"

    # 2. 情绪基调
    dominant = state.mood_vector.get("主导", "平静")
    tone_map = {
        "愤怒": "压抑", "悲伤": "低落",
        "期待": "克制", "紧张": "拘谨",
        "感动": "柔和", "恐惧": "防备"
    }
    tone = tone_map.get(dominant, "平静")

    # 3. 意图（模板拼接）
    intent_map = {
        "犹豫":    "想接近但保持距离",
        "主动":    "尝试推进关系",
        "回避":    "避免情感暴露",
    }
    intent = intent_map.get(bias, "中性行为")

    # 4. 紧迫度（magnitude 触发）
    if state.magnitude > 0.8:
        urgency = "高"
    elif state.magnitude > 0.5:
        urgency = "中"
    else:
        urgency = "低"

    return Decision(intent, bias, tone, urgency)
```

---

## 七、Feedback Loop（参数演化）

### 7.1 自动参数漂移

```python
# 由 emotion_events 自动触发，无人工规则
DRIFT_MAP = {
    "rejection":        {"approach_threshold": +0.05},
    "betrayal":         {"approach_threshold": +0.08, "risk_tolerance": -0.05},
    "breakthrough":     {"approach_threshold": -0.05},
    "comfort_received": {"approach_threshold": -0.03},
    "emotional_wound":  {"approach_threshold": +0.05},
}

def apply_drift(event: EmotionEvent, params: dict):
    deltas = DRIFT_MAP.get(event.event_type, {})
    for key, delta in deltas.items():
        floor = params.get(f"{key}_floor", 0.1)
        ceiling = params.get(f"{key}_ceiling", 0.6)
        params[key] = clamp(params.get(key, 0.3) + delta, floor, ceiling)
```

> 注意：不是"剧情规则"，是"角色被事件塑造"——区别在于这是通用的性格演化机制，不依赖具体剧情。

### 7.2 演化边界

```
approach_threshold ∈ [0.1, 0.6]
risk_tolerance     ∈ [0.0, 1.0]
```

防止角色退化到"永远主动"或"永远回避"。

---

## 八、Prompt 注入格式

```markdown
## 当前角色心理状态（自动生成）

角色：男主
章节：chapter_12
主导情绪：不安（强）次级：渴望、压抑
当前心情：内心矛盾，既想靠近又害怕失去

信任状态：
  女主: ████████░░ 0.65（较信任，但有保留）
  反派: █░░░░░░░░░ 0.10（高度警惕）

决策倾向：犹豫（想接近但保持距离）
深层情感：自卑（觉得自己配不上她）
行为特征：用冷淡掩饰在意
自我认知：我不擅长表达情感

当前成长弧：从"逃避亲密"到"尝试表达"

## 写作指导
请以上述心理状态为基准，以角色第一人称视角写作。
情绪基调应自然体现当前心理状态，但不要机械复述。
```
> content agent 的 system prompt 会自动 pre-inject 这段内容，模型不可见写入过程。

---

## 九、event_type 枚举

```
first_impression   初遇印象
trust_shift        信任变化（上升/下降）
emotional_wound    情感创伤
breakthrough       突破时刻
secret_reveal      秘密揭露
conflict_climax    冲突高潮
comfort_received   被安慰/被帮助
betrayal           背叛/失望
growth_marker      成长标记
bias_formed        偏见形成
bias_shattered     偏见打破
decision_point     关键决策
defense_reveal     防御机制显现
rejection          被拒绝（触发参数漂移）
```

---

## 十、与 v2 的区别

| 维度 | v2 | v3 |
|------|----|----|
| 架构 | 独立进程 HTTP | OpenClaw 内置 skill，同进程 |
| 情感建模 | 无 | emotion_events + State Builder |
| 状态类型 | 字符串（不可计算） | emotion_vector（结构化） |
| trust | 线性累计 | 非线性递推 + 基准 0.5 |
| 角色差异 | 无 | character_parameters |
| 决策层 | 无 | Decision Layer（降维） |
| 自我认知 | 无 | self_perception |
| 参数演化 | 无 | Feedback Loop |
| 注入方式 | 工具调用 | Auto pre-inject hook |

---

## 十一、文件结构

```
~/.openclaw/skills/memory-agent/
├── SKILL.md                     ← Skill 定义（OpenClaw 加载）
├── DESIGN.md                    ← 本文档
├── memory/
│   ├── __init__.py
│   ├── core.py                  ← MemorySystem + 所有模型
│   ├── hooks.py                 ← Auto inject + write_chapter_memory
│   ├── schema.sql               ← 建表语句
│   └── decision.py              ← Decision Layer 映射函数
├── scripts/
│   └── migrate_from_v2.py       ← v2 数据迁移脚本（可选）
└── tests/
    └── test_memory.py           ← 全部测试用例
```

---

## 十二、工程成熟度

- ✅ 记忆结构（事实 + 情感双通道）
- ✅ 状态建模（确定性，衰减，非线性 trust）
- ✅ 决策抽象（降维，不推理）
- ✅ 自我认知（self_perception）
- ✅ 参数系统（character_parameters + 反馈漂移）
- ✅ 自动注入（pre-task hook）
- ✅ Agent 隔离（数据源级别，非 prompt）
- ✅ 测试覆盖（全部通过）
- 🔲 Decision Layer 映射函数（已设计，尚未独立成文件）
- 🔲 migrate_from_v2.py（可选，暂不需要）
