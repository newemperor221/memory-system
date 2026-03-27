"""
decision.py — Decision Layer（轻量降维，不推理）

输出连续值而非离散标签，避免标签爆炸。
让 LLM 解释驱动力，生成符合人物的行为。
"""

from dataclasses import dataclass
from typing import Optional

# 情绪基调映射（连续值 → 描述词，供 prompt 用）
TONE_MAP = {
    "愤怒": ("压抑", 0.8),
    "悲伤": ("低落", 0.7),
    "期待": ("克制", 0.6),
    "紧张": ("拘谨", 0.7),
    "感动": ("柔和", 0.5),
    "恐惧": ("防备", 0.8),
    "平静": ("平稳", 0.3),
}


@dataclass
class Decision:
    """Decision Layer 输出（连续值）"""
    approach_drive: float      # 0.0~1.0 接近动机
    avoidance_drive: float     # 0.0~1.0 回避动机
    confidence: float          # 0.0~1.0 行动信心
    dominant_emotion: str      # 主导情绪（用于 tone 映射）
    magnitude: float          # 情绪强度（决定紧迫度）
    emotional_tone: str        # 情绪基调描述词
    tone_intensity: float      # 0.0~1.0 基调强度

    def to_prompt_text(self) -> str:
        """生成注入到 prompt 的文本"""
        tone_desc, tone_int = TONE_MAP.get(
            self.dominant_emotion, ("平稳", 0.3)
        )
        return (
            f"驱动力状态：\n"
            f"  接近动机：{self.approach_drive:.2f} "
            f"({'强' if self.approach_drive > 0.7 else '中' if self.approach_drive > 0.4 else '弱'})\n"
            f"  回避动机：{self.avoidance_drive:.2f} "
            f"({'强' if self.avoidance_drive > 0.7 else '中' if self.avoidance_drive > 0.4 else '弱'})\n"
            f"  行动信心：{self.confidence:.2f}\n"
            f"情绪基调：{tone_desc}（强度 {tone_int:.1f}）"
        )


def build_decision(
    mood_vector: dict,         # {"主导": "压抑", "次级": [...], "magnitude": 0.7}
    trust_levels: dict,        # {"女主": 0.65, ...}
    emotional_wounds: list,
    hidden_feeling: str,
    attachment_style: str,     # 回避型 | 焦虑型 | 安全型
    risk_tolerance: float,     # 0.0~1.0
    params: dict,               # character_parameters 当前值
) -> Decision:
    """
    轻量降维：State → 连续驱动力

    不做 if-else 树，只做数值映射。
    """
    dominant = mood_vector.get("主导", "平静")
    magnitude = mood_vector.get("magnitude", 0.5)

    # ── 1. approach / avoidance 驱动力 ─────────────────────
    # 信任度高 → approach+，回避型 → avoidance+
    trust_avg = sum(trust_levels.values()) / max(len(trust_levels), 1)

    # attachment_style 影响基准偏移
    attachment_bias = {
        "回避型": (0.15, 0.25),
        "焦虑型": (0.20, 0.10),
        "安全型": (0.0, 0.0),
    }.get(attachment_style, (0.0, 0.0))

    # 接近 = 信任 + 风险承受 + 情绪强度 - 回避偏移
    approach = min(1.0, trust_avg + risk_tolerance * 0.3 + magnitude * 0.2 - attachment_bias[1])
    approach = max(0.0, approach)

    # 回避 = 情绪创伤数 + 隐式情感压抑 + 回避型偏移
    wound_factor = min(len(emotional_wounds) * 0.1, 0.3)
    avoidance = min(1.0, wound_factor + (1 - magnitude) * 0.2 + attachment_bias[0])
    avoidance = max(0.0, avoidance)

    # ── 2. 行动信心 ───────────────────────────────────────
    # 信心 = 信任度 - 不确定感（情绪强度高时信心反而低）
    confidence = max(0.0, min(1.0, trust_avg - magnitude * 0.3 + risk_tolerance * 0.2))

    # ── 3. 情绪基调 ──────────────────────────────────────
    tone_desc, tone_int = TONE_MAP.get(dominant, ("平稳", 0.3))
    tone_intensity = tone_int * (0.5 + magnitude * 0.5)  # magnitude 高时基调更强

    return Decision(
        approach_drive=round(approach, 3),
        avoidance_drive=round(avoidance, 3),
        confidence=round(confidence, 3),
        dominant_emotion=dominant,
        magnitude=round(magnitude, 3),
        emotional_tone=tone_desc,
        tone_intensity=round(tone_intensity, 3),
    )


def apply_drift_with_inertia(
    current_value: float,
    delta: float,
    inertia: float = 0.9,
) -> float:
    """
    参数演化（带惯性）。

    new_value = old_value * inertia + delta * (1 - inertia)
    inertia=0.9 → 每次只变化 10%
    防止性格因单次事件剧烈跳变。
    """
    return current_value * inertia + delta * (1 - inertia)
