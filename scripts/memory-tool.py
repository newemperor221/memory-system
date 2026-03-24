#!/usr/bin/env python3
"""
memory-tool.py - memory-system 共享记忆客户端
供 OpenClaw Agent 通过 exec 工具调用

用法：
  python3 memory-tool.py remember "记忆内容" [high|critical|normal|low] [public|private]
  python3 memory-tool.py recall "搜索词" [limit] [public|private|all]
  python3 memory-tool.py stats
  python3 memory-tool.py health

环境变量：
  MEMORY_API_KEY   API 密钥（可选）
  MEMORY_AGENT_ID  当前 Agent 的 ID（必填，如 aicode/ainews/aitask/aicontent）
  MEMORY_BASE_URL  服务地址（默认 http://127.0.0.1:7890）
"""

import sys
import os
import json
import urllib.request
import urllib.parse
import urllib.error

BASE = os.environ.get("MEMORY_BASE_URL", "http://127.0.0.1:7890")
API_KEY = os.environ.get("MEMORY_API_KEY", "")
AGENT_ID = os.environ.get("MEMORY_AGENT_ID", "unknown")


def api_headers() -> dict:
    h = {"Content-Type": "application/json"}
    if API_KEY:
        h["X-API-Key"] = API_KEY
    return h


def get(path: str) -> dict:
    url = f"{BASE}{path}"
    try:
        req = urllib.request.Request(url, headers=api_headers())
        with urllib.request.urlopen(req, timeout=5) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        if e.code == 401:
            return {"error": "API 密钥验证失败"}
        return {"error": f"HTTP {e.code}: {e.reason}"}
    except Exception as e:
        return {"error": str(e)}


def post(path: str, body: dict) -> dict:
    url = f"{BASE}{path}"
    data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers=api_headers())
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        if e.code == 401:
            return {"error": "API 密钥验证失败"}
        return {"error": f"HTTP {e.code}: {e.reason}"}
    except Exception as e:
        return {"error": str(e)}


def cmd_remember(content: str, importance: str = "normal", layer: str = "private") -> str:
    r = post("/remember", {
        "value": content,
        "importance": importance,
        "layer": layer,
        "agentId": AGENT_ID,
    })
    if "error" in r:
        return f"写入失败: {r['error']}"
    return f"✅ 记忆已保存 [{r.get('layer','?')}] [重要性: {importance}]"


def cmd_recall(query: str, limit: int = 5, layer: str = "all") -> str:
    encoded_query = urllib.parse.quote(query, safe="")
    path = f"/recall?query={encoded_query}"
    if layer != "all":
        path += f"&layer={layer}"
    r = get(path)
    if "error" in r:
        return f"召回失败: {r['error']}"
    if not r:
        return "没有找到相关记忆"
    lines = [f"找到 {len(r)} 条记忆:"]
    for item in r[:limit]:
        e = item["entry"]
        score = item["score"]
        layer_tag = "🔒" if e.get("layer") == "private" else "🟢"
        from_layer = item["from_layer"]
        preview = e["value"][:80]
        lines.append(f"  {layer_tag} [{from_layer}] {score:.1f} | {preview}")
    return "\n".join(lines)


def cmd_stats() -> str:
    r = get("/stats")
    if "error" in r:
        return f"查询失败: {r['error']}"
    return (f"L0(工作内存): {r.get('l0_entries', 0)} 条\n"
            f"L1(短时记忆): {r.get('l1_entries', 0)} 条\n"
            f"L2(长期记忆): {r.get('l2_entries', 0)} 条")


def cmd_health() -> str:
    try:
        with urllib.request.urlopen(f"{BASE}/health", timeout=5) as r:
            return r.read().decode().strip() or "OK"
    except Exception as e:
        return f"服务异常: {e}"


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)

    cmd = sys.argv[1]

    if cmd == "remember":
        if len(sys.argv) < 3:
            print("用法: memory-tool.py remember <内容> [重要性] [public|private]")
            sys.exit(1)
        content = sys.argv[2]
        importance = sys.argv[3] if len(sys.argv) > 3 else "normal"
        layer = sys.argv[4] if len(sys.argv) > 4 else "private"
        print(cmd_remember(content, importance, layer))

    elif cmd == "recall":
        if len(sys.argv) < 3:
            print("用法: memory-tool.py recall <搜索词> [limit] [public|private|all]")
            sys.exit(1)
        query = sys.argv[2]
        limit = int(sys.argv[3]) if len(sys.argv) > 3 else 5
        layer = sys.argv[4] if len(sys.argv) > 4 else "all"
        print(cmd_recall(query, limit, layer))

    elif cmd == "stats":
        print(cmd_stats())

    elif cmd == "health":
        print(cmd_health())

    else:
        print(f"未知命令: {cmd}")
        print(__doc__)
        sys.exit(1)
