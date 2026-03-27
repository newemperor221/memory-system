"""
memory-agent OpenClaw 集成 hooks

content agent 写作前自动注入角色心理状态。
这是 hook，不是工具调用——模型不可见此模块。
"""

from .core import get_system, MemorySystem, EmotionEvent


def inject_character_context(
    agent_id: str,
    character: str,
    chapter: str,
    current_prompt: str,
    active_characters: list = None,
    mem: MemorySystem = None,
) -> str:
    """
    content agent 写作前注入角色心理状态。

    同一 mem 实例确保 State Builder 能看到刚写入的情感事件。
    """
    if agent_id != "content":
        return current_prompt

    # 优先用传入的实例，否则用全局单例
    m = mem or get_system()

    # 构建主角状态
    main_state = m.build_character_state(character, chapter)

    # 预热其他活跃角色的状态
    if active_characters:
        for other in active_characters:
            if other != character:
                m.build_character_state(other, chapter)

    # 拼接注入
    injection = "\n\n" + main_state.to_prompt_text() + "\n\n"
    if "## 写作指导" in current_prompt:
        return current_prompt.replace("## 写作指导", injection + "## 写作指导")
    else:
        return current_prompt + injection


def write_chapter_memory(
    character: str,
    chapter: str,
    scene_summary: str,
    emotion_vector: dict,
    trust_changes: dict = None,
    event_type: str = "scene",
    intensity: float = 0.5,
    decision_tendency: str = None,
    related_facts: list = None,
    mem: MemorySystem = None,
) -> str:
    """
    content agent 每章节结束后沉淀情感记忆（原子操作：写事件 + 重建状态）。

    推荐在章节完成后的总结 prompt 中调用此函数。
    """
    m = mem or get_system()

    event = EmotionEvent(
        character=character,
        chapter=chapter,
        event_type=event_type,
        event_raw=scene_summary,
        emotion_vector=emotion_vector,
        intensity=intensity,
        decision_tendency=decision_tendency,
        related_fact_keys=related_facts or [],
    )

    event_id = m.remember_emotion(event)

    # 立即重建状态（确保同一实例内）
    m.build_character_state(character, chapter)

    # 同步更新关系
    if trust_changes:
        for target, delta_t in trust_changes.items():
            m.update_relationship(
                char_a=character,
                char_b=target,
                delta_trust=delta_t,
                chapter=chapter,
                trigger_event_id=event_id,
            )

    return event_id
