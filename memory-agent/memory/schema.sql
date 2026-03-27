-- memory-agent SQLite Schema v3
-- 事实记忆 + 情感记忆双通道，3 Agent 隔离读取

-- ─────────────────────────────────────────────────────────────
-- 1. 事实记忆表
-- ─────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS entries (
    key              TEXT PRIMARY KEY,
    id               TEXT NOT NULL,
    value            TEXT NOT NULL,
    importance       INTEGER NOT NULL DEFAULT 2,  -- 1=low 2=normal 3=high 4=critical
    source           TEXT NOT NULL,               -- boss | code | content
    layer            TEXT NOT NULL DEFAULT 'private',  -- private | public
    tags             TEXT NOT NULL DEFAULT '[]',   -- JSON: ["world","plot","code","config","temp"]
    created_at       INTEGER NOT NULL,
    last_accessed    INTEGER NOT NULL,
    seq              INTEGER NOT NULL DEFAULT 0    -- 事件序号，用于增量重算
);

CREATE INDEX IF NOT EXISTS idx_entries_source  ON entries(source);
CREATE INDEX IF NOT EXISTS idx_entries_importance ON entries(importance);
CREATE INDEX IF NOT EXISTS idx_entries_layer   ON entries(layer);
CREATE INDEX IF NOT EXISTS idx_entries_seq     ON entries(seq);

-- ─────────────────────────────────────────────────────────────
-- 2. 情感事件表（novel 角色情绪）
-- ─────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS emotion_events (
    id               TEXT PRIMARY KEY,
    character        TEXT NOT NULL,
    chapter          TEXT NOT NULL,               -- "chapter_03"
    event_type       TEXT NOT NULL,
    -- 格式：first_impression | trust_shift | emotional_wound | breakthrough
    --       secret_reveal | conflict_climax | comfort_received | betrayal
    --       growth_marker | bias_formed | bias_shattered | decision_point
    event_raw        TEXT NOT NULL,               -- 原文事件描述
    emotion_vector    TEXT NOT NULL,               -- JSON: {"紧张": 0.8, "好奇": 0.5}
    intensity        REAL NOT NULL,               -- 原始强度 0.0~1.0
    decay_rate       REAL NOT NULL DEFAULT 0.95,  -- 衰减率
    subjective_desc  TEXT,                        -- 角色主观感受片段
    impression_delta TEXT,                        -- JSON: {"女主": "有点冷→声音好听"}
    bias_formed      TEXT,
    decision_tendency TEXT,                       -- 回避 | 主动 | 观望
    related_fact_keys TEXT,                        -- JSON数组: ["fact_key1"]
    created_at       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_emotion_character  ON emotion_events(character);
CREATE INDEX IF NOT EXISTS idx_emotion_chapter   ON emotion_events(chapter);
CREATE INDEX IF NOT EXISTS idx_emotion_created   ON emotion_events(created_at DESC);

-- ─────────────────────────────────────────────────────────────
-- 3. 角色状态快照（由 State Builder 计算写入，可重建）
-- ─────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS character_states (
    character        TEXT PRIMARY KEY,
    mood_vector      TEXT NOT NULL,               -- JSON: {"主导": "压抑", "次级": ["不安","渴望"]}
    trust_levels     TEXT NOT NULL,               -- JSON: {"女主": 0.65}
    emotional_wounds TEXT NOT NULL DEFAULT '[]',  -- JSON数组
    defense_mechanisms TEXT NOT NULL DEFAULT '[]',-- JSON数组
    hidden_feeling   TEXT NOT NULL DEFAULT '',
    self_perception  TEXT NOT NULL DEFAULT '',   -- "觉得自己配不上她"
    decision_bias    TEXT NOT NULL DEFAULT '',
    growth_arc       TEXT NOT NULL DEFAULT '',
    is_derived       INTEGER NOT NULL DEFAULT 1,  -- 1=计算 0=手工
    state_version    INTEGER NOT NULL DEFAULT 1,
    last_chapter     TEXT NOT NULL DEFAULT '',
    -- 增量重算用的游标
    last_processed_event_id TEXT,                 -- 最后处理过的事件 ID
    last_processed_event_seq INTEGER,             -- 最后处理过的事件 seq
    last_updated     INTEGER NOT NULL
);

-- ─────────────────────────────────────────────────────────────
-- 4. 关系状态机
-- ─────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS relationship_states (
    character_pair   TEXT PRIMARY KEY,            -- "角色A|角色B" (字典序)
    intimacy         REAL NOT NULL DEFAULT 0.0,   -- 亲密度 0.0~1.0
    trust            REAL NOT NULL DEFAULT 0.5,  -- 信任度（有基准值）
    tension          REAL NOT NULL DEFAULT 0.0,   -- 冲突张力
    dominance        REAL NOT NULL DEFAULT 0.5,   -- 谁主导
    last_chapter     TEXT NOT NULL DEFAULT '',
    last_updated     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS relationship_dynamics (
    id               TEXT PRIMARY KEY,
    character_pair   TEXT NOT NULL,
    event_type       TEXT NOT NULL,
    from_state       TEXT NOT NULL,              -- JSON快照
    to_state         TEXT NOT NULL,              -- JSON快照
    delta            TEXT NOT NULL,              -- 变化量
    trigger_event_id TEXT,
    transition_type  TEXT NOT NULL,             -- gradual | sudden | pivot
    chapter          TEXT NOT NULL,
    created_at       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rel_pair ON relationship_dynamics(character_pair);

-- ─────────────────────────────────────────────────────────────
-- 5. 小说世界观设定（稳定参考，不过滤）
-- ─────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS novel_world (
    key              TEXT PRIMARY KEY,
    category         TEXT NOT NULL,              -- 人物 | 地点 | 势力 | 规则
    value            TEXT NOT NULL,
    author_notes     TEXT,
    consistency_hints TEXT,
    version          INTEGER NOT NULL DEFAULT 1,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
