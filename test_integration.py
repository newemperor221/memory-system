#!/usr/bin/env python3
"""
memory-system-v2 集成测试脚本
用法: python3 test_integration.py
"""
import requests, json, sys, time

BASE = "http://127.0.0.1:7891"

def stats():
    r = requests.get(f"{BASE}/stats")
    return r.json()

def ok(name, cond, detail=""):
    symbol = "✅" if cond else "❌"
    msg = f"{symbol} {name}"
    if detail:
        msg += f": {detail}"
    print(msg)
    return cond

def main():
    print("=" * 50)
    print("Memory System v2 集成测试")
    print("=" * 50)

    # 0. 健康检查
    print("\n[ 0 ] 健康检查")
    r = requests.get(f"{BASE}/health")
    ok("服务在线", r.status_code == 200, r.text.strip())

    s = stats()
    l1_before = s["l1_entries"]
    l2_before = s["l2_entries"]
    print(f"  L1={l1_before} L2={l2_before} L2vec={s['l2_vectors_cached']}")

    # TC-01: 写入记忆
    print("\n[ TC-01 ] 写入记忆")
    test_key = f"tc:{int(time.time())}"
    r = requests.post(f"{BASE}/remember", json={
        "key": test_key,
        "value": "自动化集成测试记忆",
        "importance": "normal",
        "layer": "private",
        "tags": ["测试", "集成"]
    })
    ok("写入成功", r.json().get("ok"), r.text)

    # TC-02: L1→L2 同步验证（核心 bug 验证）
    print("\n[ TC-02 ] L1→L2 同步验证（最重要）")
    time.sleep(1)
    s = stats()
    l1_after = s["l1_entries"]
    l2_after = s["l2_entries"]
    print(f"  写入后: L1={l1_after} L2={l2_after}")
    sync_ok = ok("L1 增加", l1_after == l1_before + 1, f"{l1_before}→{l1_after}")
    sync_ok = ok("L2 同时增加", l2_after == l2_before + 1, f"{l2_before}→{l2_after}") and sync_ok

    # TC-03: 精确读取
    print("\n[ TC-03 ] 精确读取")
    r = requests.get(f"{BASE}/get?key={test_key}")
    d = r.json()
    ok("能找到记录", "entry" in d and d["entry"]["key"] == test_key,
       d["entry"]["value"][:20] if "entry" in d else d)

    # TC-04: BM25 检索
    print("\n[ TC-04 ] BM25 关键词检索")
    r = requests.get(f"{BASE}/recall?query=自动化")
    d = r.json()
    results = d.get("results", [])
    ok("BM25 有结果", len(results) > 0, f"{len(results)} 条")

    # TC-05: 语义检索（可能触发向量生成）
    print("\n[ TC-05 ] 语义检索")
    time.sleep(2)
    r = requests.get(f"{BASE}/recall?query=自动化测试&semantic=true")
    d = r.json()
    results = d.get("results", [])
    ok("语义搜索有结果", len(results) > 0, f"{len(results)} 条")
    s = stats()
    ok("向量已缓存", s["l2_vectors_cached"] > 0, f"{s['l2_vectors_cached']} 条")

    # TC-06: 删除
    print("\n[ TC-06 ] 删除记忆")
    r = requests.post(f"{BASE}/delete", json={"key": test_key})
    ok("删除成功", r.json().get("ok"), r.text)

    # TC-07: 删除后确认不存在
    print("\n[ TC-07 ] 删除验证")
    r = requests.get(f"{BASE}/get?key={test_key}")
    d = r.json()
    ok("已删除不存在", "entry" not in d, d.get("error", ""))

    # TC-08: 重启后向量持久化（可选，需要等一会儿）
    print("\n[ TC-08 ] 向量持久化（可选）")
    s = stats()
    vecs_before = s["l2_vectors_cached"]
    print(f"  重启前向量: {vecs_before}")

    print("\n" + "=" * 50)
    print("测试完成！L1→L2 同步测试最关键，请确认两个 ✅")
    print("=" * 50)

if __name__ == "__main__":
    try:
        main()
    except requests.exceptions.ConnectionError:
        print("❌ 无法连接服务，请确认 memory-system-v2 已启动")
        sys.exit(1)
