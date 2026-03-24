#!/usr/bin/env bash
# 停止 memory-system 服务
# 用法: ./stop.sh

PIDFILE="/tmp/memory-system.pid"

if [ -f "$PIDFILE" ]; then
    PID=$(cat "$PIDFILE")
    if kill -0 "$PID" 2>/dev/null; then
        kill "$PID"
        rm -f "$PIDFILE"
        echo "✅ 已停止 (PID: $PID)"
    else
        echo "⚠️  进程不存在，清理 PID 文件"
        rm -f "$PIDFILE"
    fi
else
    # 尝试用 pgrep 找
    PIDS=$(pgrep -f "memory-system serve" 2>/dev/null || true)
    if [ -n "$PIDS" ]; then
        echo "==> 找到进程: $PIDS，杀掉..."
        echo "$PIDS" | xargs kill 2>/dev/null
        echo "✅ 已停止"
    else
        echo "⚠️  未找到运行中的 memory-system"
    fi
fi
