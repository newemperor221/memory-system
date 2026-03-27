"""
memory-agent core — 内置 OpenClaw skill，零进程开销
所有操作在调用进程内完成，直接读写 SQLite

修正版 v3：
- State Builder 滑动窗口（LIMIT 50）
- emotion_vector 归一化
- trust_levels 以 0.5 为基准值
- 自动注入（pre-context hook）
"""

from __future__ import annotations

import sqlite3
import json
import re
import uuid
import time
from pathlib import Path
from dataclasses import dataclass, asdict, field
from typing import Optional


# ─────────────────────────────────────────────────────────────────
# 路径配置
# ─────────────────────────────────────────────────────────────────
SKILL_DIR = Path(__file__).parent
DB_PATH = Path("~/.openclaw/workspace/code/memory-agent/memory.db").expanduser()
DB_PATH.parent.mkdir(parents=True, exist_ok=True)

# State Builder 滑动窗口大小（控制每次计算的事件数量）
EMOTION_WINDOW = 50

# trust 基准值（角色初始信任度）
BASE_TRUST = 0.5


# ─────────────────────────────────────────────────────────────────
# 数据模型
# ─────────────────────────────────────────────────────────────────

@dataclass
class EmotionEvent:
    """一次情感事件"""
    character: str
    chapter: str
    event_type: str          # event_type 枚举
    event_raw: str
    emotion_vector: dict     # {"紧张": 0.8, "好奇": 0.5}
    intensity: float         # 0.0 ~ 1.0
    id: str = ""
    decay_rate: float = 0.95
    subjective_desc: Optional[str] = None
    impression_delta: Optional[dict] = None
    bias_formed: Optional[str] = None
    decision_tendency: Optional[str] = None
    related_fact_keys: Optional[list] = None
    created_at: int = 0

    def __post_init__(self):
        if not self.id:
            self.id = str(uuid.uuid4())
        if not self.created_at:
            self.created_at = int(time.time() * 1000)

    def save(self, conn: sqlite3.Connection):
        conn.execute("""
            INSERT OR REPLACE INTO emotion_events
            (id, character, chapter, event_type, event_raw,
             emotion_vector, intensity, decay_rate, subjective_desc,
             impression_delta, bias_formed, decision_tendency,
             related_fact_keys, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """, (
            self.id, self.character, self.chapter, self.event_type,
            self.event_raw, json.dumps(self.emotion_vector), self.intensity,
            self.decay_rate,
            self.subjective_desc,
            json.dumps(self.impression_delta) if self.impression_delta else None,
            self.bias_formed, self.decision_tendency,
            json.dumps(self.related_fact_keys) if self.related_fact_keys else None,
            self.created_at,
        ))


@dataclass
class CharacterState:
    """角色实时心理状态（由 State Builder 计算）"""
    character: str
    mood_vector: dict        # {"主导": "压抑", "次级": ["不安","渴望"], "magnitude": 0.7}
    trust_levels: dict        # {"女主": 0.65, "反派": 0.1}
    emotional_wounds: list
    defense_mechanisms: list
    hidden_feeling: str
    self_perception: str = ""   # "觉得自己配不上她"
    decision_bias: str = ""
    growth_arc: str = ""
    is_derived: int = 1
    state_version: int = 1
    last_chapter: str = ""
    last_processed_event_id: str = ""
    last_processed_event_seq: int = 0
    last_updated: int = 0

    def __post_init__(self):
        if not self.last_updated:
            self.last_updated = int(time.time() * 1000)

    def to_prompt_text(self) -> str:
        """生成注入到 Novel Agent 的 prompt 片段"""
        dominant = self.mood_vector.get("主导", "平静")
        secondary = self.mood_vector.get("次级", [])
        magnitude = self.mood_vector.get("magnitude", 0.0)

        intensity_label = "（极强）" if magnitude > 0.8 else \
                          "（强）" if magnitude > 0.5 else \
                          "（弱）"

        trust_lines = []
        for char, val in self.trust_levels.items():
            bar = "█" * int(val * 10) + "░" * (10 - int(val * 10))
            trust_lines.append(f"  {char}: {bar} ({val:.2f})")

        wounds_str = ", ".join(self.emotional_wounds) if self.emotional_wounds else "无"
        defenses_str = ", ".join(self.defense_mechanisms) if self.defense_mechanisms else "（未形成）"

        return f"""## 当前角色心理状态（自动生成，请勿修改）

角色：{self.character}
章节：{self.last_chapter}
主导情绪：{dominant}{intensity_label}  次级：{secondary}
情绪强度：{magnitude:.2f}

信任状态：
{chr(10).join(trust_lines) if trust_lines else '  无明显关系'}

情感创伤：{wounds_str}
行为特征：{defenses_str}
深层情感：{self.hidden_feeling or '（未显现）'}
自我认知：{self.self_perception or '（未形成）'}

决策倾向：{self.decision_bias or '中性'}
当前成长弧：{self.growth_arc or '（未定义）'}

## 写作指导
请以上述心理状态为基准，以角色第一人称视角写作。
情绪基调应自然体现当前心理状态，但不要机械复述上述标签。
"""

    def save(self, conn: sqlite3.Connection):
        self.last_updated = int(time.time() * 1000)
        conn.execute("""
            INSERT OR REPLACE INTO character_states
            (character, mood_vector, trust_levels, emotional_wounds,
             defense_mechanisms, hidden_feeling, self_perception,
             decision_bias, growth_arc, is_derived, state_version,
             last_chapter, last_processed_event_id, last_processed_event_seq,
             last_updated)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """, (
            self.character,
            json.dumps(self.mood_vector),
            json.dumps(self.trust_levels),
            json.dumps(self.emotional_wounds),
            json.dumps(self.defense_mechanisms),
            self.hidden_feeling,
            self.self_perception,
            self.decision_bias,
            self.growth_arc,
            self.is_derived,
            self.state_version,
            self.last_chapter,
            self.last_processed_event_id,
            self.last_processed_event_seq,
            self.last_updated,
        ))


# ─────────────────────────────────────────────────────────────────
# 核心系统
# ─────────────────────────────────────────────────────────────────

class MemorySystem:
    """
    零进程开销的内置记忆系统。
    所有操作在同一进程内完成，直接读写 SQLite WAL。
    """

    def __init__(self, db_path: str = None):
        self.db_path = Path(db_path or DB_PATH)
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_schema()
        self._conn = sqlite3.connect(str(self.db_path), check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA foreign_keys = ON")

    # ── 建表（幂等）───────────────────────────────────────────

    def _init_schema(self):
        schema = (SKILL_DIR / "schema.sql").read_text()
        conn = sqlite3.connect(str(self.db_path))
        conn.executescript(schema)
        conn.close()

    # ── Fact 层读写───────────────────────────────────────────

    def remember_fact(
        self,
        key: str,
        value: str,
        source: str,
        layer: str = "private",
        importance: int = 2,
        tags: list = None,
        agent_id: str = None,
    ) -> bool:
        """
        写入事实记忆。

        权限规则：
        - layer=public 只能 boss 写
        - 其他 agent 只能写 private
        """
        if layer == "public" and agent_id != "boss":
            raise PermissionError("只有 boss 可以写 public 层")

        now_ms = int(time.time() * 1000)

        # seq 自增
        seq_row = self._conn.execute(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM entries"
        ).fetchone()
        seq = seq_row[0] if seq_row else 1

        self._conn.execute("""
            INSERT OR REPLACE INTO entries
            (key, id, value, importance, source, layer, tags,
             created_at, last_accessed, seq)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """, (
            key, str(uuid.uuid4()), value, importance, source,
            layer, json.dumps(tags or []),
            now_ms, now_ms, seq,
        ))
        self._conn.commit()
        return True

    def recall_fact(
        self,
        query: str,
        agent_id: str,
        limit: int = 20,
    ) -> list:
        """
        按 agent 过滤读取事实记忆。

        过滤规则（数据源级别隔离，prompt 不可见）：
        - boss:   importance >= 3，layer in (public, private)
        - code:   source='code' AND tag in ('code', 'config')
        - content: source='content' AND tag in ('world', 'plot')
        """
        rules = {
            "boss":     ("importance >= 3", "layer IN ('public','private')"),
            "code":     ("source = 'code'", "tags LIKE '%code%' OR tags LIKE '%config%'"),
            "content":  ("source = 'content'", "tags LIKE '%world%' OR tags LIKE '%plot%'"),
        }

        importance_cond, extra_cond = rules.get(agent_id, rules["boss"])

        # 先粗筛（limit 50，避免 LIKE 全表）
        sql = f"""
            SELECT key, value, importance, source, layer, tags,
                   created_at, last_accessed
            FROM entries
            WHERE {importance_cond}
              AND ({extra_cond})
            ORDER BY importance DESC, last_accessed DESC
            LIMIT 50
        """
        rows = self._conn.execute(sql).fetchall()

        # 关键词评分重排序（而非 LIKE 精确匹配）
        scored = []
        for r in rows:
            score = self._score_text(r[1], query) + self._score_text(r[0], query)
            if score > 0:
                scored.append((score, r))

        scored.sort(key=lambda x: -x[0])
        results = scored[:limit]

        return [
            {
                "key": r[1][0],
                "value": r[1][1],
                "importance": r[1][2],
                "source": r[1][3],
                "layer": r[1][4],
                "tags": json.loads(r[1][5]) if r[1][5] else [],
                "created_at": r[1][6],
                "last_accessed": r[1][7],
                "_score": r[0],
            }
            for r in results
        ]

    @staticmethod
    def _score_text(text: str, query: str) -> float:
        """
        关键词评分：query 分词后命中的 token 数量。
        不做语义，不依赖外部模型。
        """
        text_lower = text.lower()
        tokens = query.lower().split()
        return sum(1 for t in tokens if t in text_lower)

    def update_fact_access(self, key: str):
        """更新 last_accessed 时间戳"""
        self._conn.execute(
            "UPDATE entries SET last_accessed = ? WHERE key = ?",
            (int(time.time() * 1000), key)
        )
        self._conn.commit()

    def delete_fact(self, key: str, agent_id: str = None) -> bool:
        """删除事实记忆（带权限校验）"""
        row = self._conn.execute(
            "SELECT layer, source FROM entries WHERE key = ?", (key,)
        ).fetchone()
        if not row:
            return False

        layer, source = row

        if layer == "public" and agent_id != "boss":
            raise PermissionError("只有 boss 可删除 public 条目")
        if agent_id and source != agent_id and agent_id != "boss":
            raise PermissionError(f"{agent_id} 无权删除 source={source} 的条目")

        self._conn.execute("DELETE FROM entries WHERE key = ?", (key,))
        self._conn.commit()
        return True

    # ── Emotion 层读写─────────────────────────────────────────

    def remember_emotion(self, event: EmotionEvent) -> str:
        """写入情感事件（content 专用）"""
        event.save(self._conn)
        self._conn.commit()

        # 注意：不在此处触发 build_character_state
        # 由调用方（hooks.py）显式触发，以确保是同一个实例
        return event.id

    # ── State Builder（核心修正版）────────────────────────────

    def build_character_state(
        self,
        character: str,
        current_chapter: str,
        force_full: bool = False,
    ) -> CharacterState:
        """
        State Builder — 从情感事件实时计算角色状态。

        修正点（v3）：
        1. 滑动窗口 LIMIT 50（避免全表扫描）
        2. emotion_vector 归一化（使总和=1，防爆炸）
        3. trust 以 0.5 为基准值（而非从0累积）
        4. 增量计算（last_processed_event_id 做游标）
        """
        # 取当前已处理的事件游标
        state_row = self._conn.execute(
            "SELECT state_version, last_processed_event_id, last_processed_event_seq "
            "FROM character_states WHERE character = ?",
            (character,)
        ).fetchone()

        prev_version = state_row[0] if state_row else 0
        last_proc_id = state_row[1] if state_row else ""

        # 增量：只取新增事件（用 last_processed_event_id 做游标）
        if force_full or not last_proc_id:
            rows = self._conn.execute("""
                SELECT id, chapter, event_type, emotion_vector,
                       intensity, decay_rate, decision_tendency, created_at
                FROM emotion_events
                WHERE character = ?
                ORDER BY created_at ASC
                LIMIT ?
            """, (character, EMOTION_WINDOW)).fetchall()
        else:
            rows = self._conn.execute("""
                SELECT id, chapter, event_type, emotion_vector,
                       intensity, decay_rate, decision_tendency, created_at
                FROM emotion_events
                WHERE character = ? AND created_at > (
                    SELECT created_at FROM emotion_events WHERE id = ?
                )
                ORDER BY created_at ASC
                LIMIT ?
            """, (character, last_proc_id, EMOTION_WINDOW)).fetchall()

        # ── 空状态 ─────────────────────────────────────────
        if not rows:
            return CharacterState(
                character=character,
                mood_vector={"主导": "平静", "次级": [], "magnitude": 0.0},
                trust_levels={},
                emotional_wounds=[],
                defense_mechanisms=[],
                hidden_feeling="",
                self_perception="",
                decision_bias="",
                growth_arc="",
                last_chapter=current_chapter,
                last_updated=int(time.time()),
            )

        # ── 情绪池 + 衰减计算 ──────────────────────────────
        emotion_pool: dict[str, float] = {}  # {情绪名: 加权分}
        trust_deltas: dict[str, float] = {}  # {目标角色: 信任变化量}
        tendency_list: list[str] = []
        wounds: list[str] = []
        defenses: list[str] = []
        last_event_id = rows[-1][0]
        last_event_seq = rows[-1][7]

        # ── recent override 追踪 ──────────────────────────
        latest_event_intensity: float = 0.0
        latest_event_primary_emotion: str = ""
        latest_event_created_at: int = 0

        # 以第一条事件的章节为基准
        base_ch = rows[0][1]
        base_cn = self._parse_chapter(base_ch) or 0

        for row in rows:
            event_id, chapter, event_type = row[0], row[1], row[2]
            ev_json, intensity = row[3], row[4]
            decay_rate, tendency = row[5], row[6]
            event_created_at = row[7]

            ev: dict = json.loads(ev_json)
            cn = self._parse_chapter(chapter) or 0

            # 时间衰减
            delta = max(cn - base_cn, 0) if base_cn else 0
            decay_factor = (decay_rate or 0.95) ** delta
            effective = intensity * decay_factor

            # ── 情绪合并（归一化基准）─────────────────────
            for em, val in ev.items():
                emotion_pool[em] = emotion_pool.get(em, 0.0) + val * effective

            # ── trust_shift 事件 ─────────────────────────
            if event_type == "trust_shift":
                for target, delta_t in ev.items():
                    trust_deltas[target] = trust_deltas.get(target, 0.0) + delta_t * decay_factor

            # ── emotional_wound ────────────────────────────
            if event_type == "emotional_wound":
                wounds.append(ev.get("wound", chapter))

            # ── bias_formed / breakthrough ─────────────────
            if event_type in ("bias_formed", "breakthrough", "defense_reveal"):
                if desc := row[3]:  # 有主观描述
                    pass  # 在 impression_delta 里处理

            # ── recent override：追踪最近事件 ─────────────
            if event_created_at >= latest_event_created_at:
                latest_event_intensity = intensity
                latest_event_primary_emotion = list(ev.keys())[0] if ev else ""
                latest_event_created_at = event_created_at

            # ── 决策倾向收集 ──────────────────────────────
            if tendency:
                tendency_list.append(tendency)

        # ── 情绪归一化（防爆炸）──────────────────────────
        total_em = sum(emotion_pool.values())
        if total_em > 0:
            emotion_pool = {k: v / total_em for k, v in emotion_pool.items()}

        # ── magnitude：情绪总强度（用于判断行为触发）─────
        magnitude = sum(emotion_pool.values()) / max(len(emotion_pool), 1)

        # 排序取主导 + 次级
        sorted_em = sorted(emotion_pool.items(), key=lambda x: -x[1])
        dominant = sorted_em[0][0] if sorted_em else "平静"
        secondary = [e[0] for e in sorted_em[1:4]]  # 取次级情绪 Top3

        # ── recent override（修正：避免100次轻微+1次强事件失真）──
        # 如果最近事件强度 > 0.8，用它的主导情绪覆盖当前 dominant
        if latest_event_intensity > 0.8 and latest_event_primary_emotion:
            dominant = latest_event_primary_emotion

        mood_vector = {"主导": dominant, "次级": secondary, "magnitude": magnitude}

        # ── 信任度计算（修正点3：base_trust=0.5）──────────
        trust_levels = {}
        for target, delta_t in trust_deltas.items():
            raw = BASE_TRUST + delta_t
            trust_levels[target] = max(0.0, min(1.0, raw))

        # ── 决策倾向聚合 ──────────────────────────────────
        decision_bias = ""
        if tendency_list:
            from collections import Counter
            decision_bias = Counter(tendency_list).most_common(1)[0][0]

        # ── 构建状态 ──────────────────────────────────────
        new_state = CharacterState(
            character=character,
            mood_vector=mood_vector,
            trust_levels=trust_levels,
            emotional_wounds=list(set(wounds))[-5:],  # 保留最近5个创伤
            defense_mechanisms=list(set(defenses))[-3:],
            hidden_feeling="",
            self_perception="",
            decision_bias=decision_bias,
            growth_arc="",
            is_derived=1,
            state_version=prev_version + 1,
            last_chapter=current_chapter,
            last_processed_event_id=last_event_id,
            last_processed_event_seq=event_created_at,
        )

        new_state.save(self._conn)
        self._conn.commit()

        return new_state

    @staticmethod
    def _parse_chapter(ch: str) -> Optional[int]:
        """提取章节号整数（用于衰减计算）"""
        if not ch:
            return None
        m = re.search(r'\d+', ch)
        return int(m.group()) if m else None

    # ── 关系状态 ────────────────────────────────────────────

    def update_relationship(
        self,
        char_a: str,
        char_b: str,
        delta_intimacy: float = 0.0,
        delta_trust: float = 0.0,
        delta_tension: float = 0.0,
        chapter: str = "",
        trigger_event_id: str = None,
    ) -> dict:
        """
        增量更新关系状态（以当前值为基准叠加变化）。
        """
        pair_key = "|".join(sorted([char_a, char_b]))

        row = self._conn.execute(
            "SELECT intimacy, trust, tension FROM relationship_states WHERE character_pair = ?",
            (pair_key,)
        ).fetchone()

        intimacy = row[0] if row else 0.0
        trust    = row[1] if row else BASE_TRUST  # 基准 0.5
        tension  = row[2] if row else 0.0

        intimacy = max(0.0, min(1.0, intimacy + delta_intimacy))
        trust    = max(0.0, min(1.0, trust + delta_trust))
        tension  = max(0.0, min(1.0, tension + delta_tension))

        now = int(time.time() * 1000)
        self._conn.execute("""
            INSERT OR REPLACE INTO relationship_states
            (character_pair, intimacy, trust, tension, dominance, last_chapter, last_updated)
            VALUES (?, ?, ?, ?, ?, ?, ?)
        """, (pair_key, intimacy, trust, tension, 0.5, chapter, now))

        # 记录变化
        self._conn.execute("""
            INSERT INTO relationship_dynamics
            (id, character_pair, event_type, from_state, to_state, delta,
             trigger_event_id, transition_type, chapter, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """, (
            str(uuid.uuid4()), pair_key, "delta_update",
            json.dumps({"intimacy": row[0] if row else 0, "trust": row[1] if row else BASE_TRUST, "tension": row[2] if row else 0}),
            json.dumps({"intimacy": intimacy, "trust": trust, "tension": tension}),
            json.dumps({"intimacy": delta_intimacy, "trust": delta_trust, "tension": delta_tension}),
            trigger_event_id, "gradual", chapter, now,
        ))

        self._conn.commit()
        return {"intimacy": intimacy, "trust": trust, "tension": tension}

    def get_relationship(self, char_a: str, char_b: str) -> dict:
        pair_key = "|".join(sorted([char_a, char_b]))
        row = self._conn.execute(
            "SELECT intimacy, trust, tension FROM relationship_states WHERE character_pair = ?",
            (pair_key,)
        ).fetchone()
        if row:
            return {"intimacy": row[0], "trust": row[1], "tension": row[2]}
        return {"intimacy": 0.0, "trust": BASE_TRUST, "tension": 0.0}

    # ── World ────────────────────────────────────────────────

    def set_world(self, key: str, category: str, value: str,
                  author_notes: str = None, consistency_hints: str = None) -> bool:
        now = int(time.time() * 1000)
        self._conn.execute("""
            INSERT OR REPLACE INTO novel_world
            (key, category, value, author_notes, consistency_hints,
             version, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, 1, ?, ?)
        """, (key, category, value, author_notes, consistency_hints, now, now))
        self._conn.commit()
        return True

    def get_world(self, key: str) -> Optional[dict]:
        row = self._conn.execute(
            "SELECT * FROM novel_world WHERE key = ?", (key,)
        ).fetchone()
        if not row:
            return None
        cursor = self._conn.execute("PRAGMA table_info(novel_world)")
        cols = [r[1] for r in cursor.fetchall()]
        return dict(zip(cols, row))

    # ── 统计 ─────────────────────────────────────────────────

    def stats(self) -> dict:
        return {
            "fact_entries":   self._conn.execute("SELECT COUNT(*) FROM entries").fetchone()[0],
            "emotion_events": self._conn.execute("SELECT COUNT(*) FROM emotion_events").fetchone()[0],
            "characters":     self._conn.execute("SELECT COUNT(*) FROM character_states").fetchone()[0],
            "relationships":  self._conn.execute("SELECT COUNT(*) FROM relationship_states").fetchone()[0],
            "world_entries":  self._conn.execute("SELECT COUNT(*) FROM novel_world").fetchone()[0],
            "db_path":        str(self.db_path),
        }


# ─────────────────────────────────────────────────────────────────
# 全局单例（OpenClaw skill 内多 agent 共享同一实例）
# ─────────────────────────────────────────────────────────────────
_system: Optional[MemorySystem] = None


def get_system() -> MemorySystem:
    global _system
    if _system is None:
        _system = MemorySystem()
    return _system
