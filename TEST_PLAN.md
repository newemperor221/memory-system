# Memory System v2 — 测试方案

## 概述

覆盖范围：L0 / L1 / L2 / L3 四层全部 API 接口，bug 修复验证，回归测试。

---

## 前置条件

```bash
# 服务已启动
curl http://127.0.0.1:7891/health   # 期望: OK

# 依赖工具
pip3 install -q requests   # 或用 curl
```

---

## 0. 健康检查（每次测试前必做）

```bash
curl -s http://127.0.0.1:7891/health
curl -s http://127.0.0.1:7891/stats | python3 -m json.tool
```

---

## 一、核心 API 测试

### TC-01: 写入记忆（POST /remember）

```bash
curl -s -X POST http://127.0.0.1:7891/remember \
  -H "Content-Type: application/json" \
  -d '{
    "key": "tc01:test",
    "value": "测试记忆内容，用于验证写入接口。",
    "importance": "normal",
    "layer": "private",
    "tags": ["测试", "TC-01"]
  }'
```

**期望输出：** `{"ok": true, "indexed": false, "note": "..."}`

---

### TC-02: 精确读取（GET /get）

```bash
curl -s "http://127.0.0.1:7891/get?key=tc01:test"
```

**期望：** 能读到刚才写入的 `value`

---

### TC-03: BM25 关键词检索（GET /recall，不带 semantic）

```bash
curl -s "http://127.0.0.1:7891/recall?query=测试记忆"
```

**期望：** 返回匹配的记忆条，`source` 包含 `"BM25"`

---

### TC-04: 语义检索（GET /recall，带 semantic=true）

```bash
sleep 3
curl -s "http://127.0.0.1:7891/recall?query=内容验证&semantic=true"
```

**期望：** 返回结果，`source` 包含 `"Hybrid"` 或 `"L2"`

> ⚠️ 首次语义搜索会触发向量生成（调 Gemini API），等待 3 秒后再查 stats

---

### TC-05: 删除记忆（POST /delete）

```bash
curl -s -X POST http://127.0.0.1:7891/delete \
  -H "Content-Type: application/json" \
  -d '{"key": "tc01:test"}'
```

**期望：** `{"ok": true}`

---

## 二、L2 同步 Bug 修复验证

**这是最重要的回归测试！** 验证新增记忆能同时写入 L1 和 L2。

### TC-10: L1→L2 同步验证

```bash
# 1. 记录当前 L1 和 L2 数量
curl -s http://127.0.0.1:7891/stats
# 记下 l1_entries 和 l2_entries（重启后首次会重建）

# 2. 写入新记忆
curl -s -X POST http://127.0.0.1:7891/remember \
  -H "Content-Type: application/json" \
  -d '{"key":"tc10:l2sync","value":"验证L2同步bug修复","importance":"normal","layer":"private"}'

# 3. 立即检查 stats
curl -s http://127.0.0.1:7891/stats
# 期望: l1_entries += 1, l2_entries += 1（两者同时增加！）
```

**成功标准：**
- `l1_entries` 增加 1
- `l2_entries` 同时增加 1（之前 bug 会导致 L1 增加但 L2 不变）

---

## 三、向量持久化测试

### TC-20: 重启后向量加载验证

```bash
# 1. 触发语义搜索，确保向量已生成
curl -s "http://127.0.0.1:7891/recall?query=memory&semantic=true" > /dev/null
sleep 5

# 2. 检查向量文件大小
ls -lh /tmp/memory_v2_l2_vectors.json
# 期望: 文件存在且 > 100KB（有21条向量）

# 3. 重启服务
pkill -f memory-system-v2
sleep 1
bash /home/woioeow/.openclaw/workspace/code/memory-system-v2/start.sh &
sleep 5

# 4. 重启后立即检查向量数量（应该从磁盘加载，不需要重新调用API）
curl -s http://127.0.0.1:7891/stats
# 期望: l2_vectors_cached > 0（不重新调API就有值）
```

**成功标准：**
- 重启后 `l2_vectors_cached` 非零
- `l2_pending_vectors` 为零

---

### TC-21: 断网情况下的降级测试

```bash
# 模拟 API 不可用时（不实际断网，仅验证降级逻辑存在）
curl -s "http://127.0.0.1:7891/recall?query=测试&semantic=false"
# 期望: BM25 仍可工作（semantic=false 绕开 API）
```

---

## 四、分层架构验证

### TC-30: L0 / L1 / L2 / L3 数据分层

```bash
curl -s http://127.0.0.1:7891/stats
# 观察各层数量
```

**期望 L2 数量 >= L1 数量**（向量已生成的情况下）

---

## 五、性能/压力测试（可选）

### TC-40: 批量写入

```bash
for i in $(seq 1 10); do
  curl -s -X POST http://127.0.0.1:7891/remember \
    -H "Content-Type: application/json" \
    -d "{\"key\":\"tc40:perf$i\",\"value\":\"批量测试记忆$i\",\"importance\":\"normal\",\"layer\":\"private\"}"
done
sleep 1
curl -s http://127.0.0.1:7891/stats
# 期望: l1_entries += 10
```

---

## 六、测试清理（每个测试会话结束后执行）

```bash
# 删除 TC-* 测试留下的记忆
for key in tc01:test tc10:l2sync tc40:perf1 tc40:perf2 tc40:perf3 tc40:perf4 tc40:perf5 tc40:perf6 tc40:perf7 tc40:perf8 tc40:perf9 tc40:perf10; do
  curl -s -X POST http://127.0.0.1:7891/delete \
    -H "Content-Type: application/json" \
    -d "{\"key\":\"$key\"}" > /dev/null
done

# 验证清理完毕
curl -s http://127.0.0.1:7891/stats
```

---

## 七、自动化测试脚本

```python
#!/usr/bin/env python3
"""memory-system-v2 自动化测试"""
import requests, json, time

BASE = "http://127.0.0.1:7891"

def stats():
    return requests.get(f"{BASE}/stats").json()

def test(name, fn):
    r = fn()
    ok = r.get("ok", True) if isinstance(r, dict) else True
    print(f"{'✅' if ok else '❌'} {name}: {r}")

def main():
    print("=== Health ===")
    print(requests.get(f"{BASE}/health").text)

    print("\n=== Stats ===")
    print(json.dumps(stats(), indent=2))

    print("\n=== TC-01 Write ===")
    test("Write", lambda: requests.post(f"{BASE}/remember", json={
        "key": "tc:auto",
        "value": "自动化测试记忆",
        "importance": "normal",
        "layer": "private"
    }).json())

    print("\n=== TC-02 Get ===")
    test("Get", lambda: requests.get(f"{BASE}/get?key=tc:auto").json())

    print("\n=== TC-03 BM25 Recall ===")
    r = requests.get(f"{BASE}/recall?query=自动化").json()
    print(f"  BM25 results: {r.get('total', len(r.get('results',[])))}")
    for res in r.get("results",[])[:3]:
        print(f"  - {res.get('entry',{}).get('key')} | score={res.get('score',0):.3f}")

    print("\n=== TC-04 Semantic Recall ===")
    time.sleep(2)
    r = requests.get(f"{BASE}/recall?query=自动化测试&semantic=true").json()
    print(f"  Semantic results: {r.get('total', len(r.get('results',[])))}")

    print("\n=== TC-05 Delete ===")
    test("Delete", lambda: requests.post(f"{BASE}/delete", json={
        "key": "tc:auto"
    }).json())

    print("\n=== Final Stats ===")
    print(json.dumps(stats(), indent=2))

if __name__ == "__main__":
    main()
```

---

## 测试检查清单

| ID | 测试项 | 预期 | 状态 |
|----|--------|------|------|
| TC-01 | 写入记忆 | ok=true | ⬜ |
| TC-02 | 精确读取 | 能读到 | ⬜ |
| TC-03 | BM25检索 | 有结果 | ⬜ |
| TC-04 | 语义检索 | 有结果 | ⬜ |
| TC-05 | 删除记忆 | ok=true | ⬜ |
| TC-10 | L1→L2同步 | L2+=1 | ⬜ |
| TC-20 | 向量持久化 | 重启后加载 | ⬜ |
| TC-30 | 分层数据 | 各层正常 | ⬜ |
