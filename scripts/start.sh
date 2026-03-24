#!/usr/bin/env bash
# 启动 memory-system 服务
# 用法: ./start.sh [workspace路径] [监听地址]
# 默认 workspace: /home/woioeow/.openclaw/workspace/boss
# 默认监听: 127.0.0.1:7890

DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="${1:-$HOME/.openclaw/workspace}"
LISTEN="${2:-127.0.0.1:7890}"
LOG="/tmp/memory-system.log"
PIDFILE="/tmp/memory-system.pid"

# 检查是否已在运行
if curl -s http://127.0.0.1:7890/health > /dev/null 2>&1; then
    echo "⚠️  memory-system 已在运行 (PID: $(pgrep -f memory-system | head -1))"
    exit 0
fi

echo "==> 启动 memory-system..."
echo "   workspace: $WORKSPACE"
echo "   listen: $LISTEN"

mkdir -p "$WORKSPACE/.memory-l1"

MEMORY_L1_PATH="$WORKSPACE/.memory-l1" \
MEMORY_WORKSPACE="$WORKSPACE" \
nohup "$DIR/../memory-system" serve \
    --workspace "$WORKSPACE" \
    --listen "$LISTEN" \
    </dev/null > "$LOG" 2>&1 &

echo $! > "$PIDFILE"
sleep 2

if curl -s http://127.0.0.1:7890/health > /dev/null 2>&1; then
    echo "✅ 服务已启动 (PID: $(cat $PIDFILE))"
    echo "   健康检查: http://$LISTEN/health"
    echo "   日志: $LOG"
else
    echo "❌ 启动失败，查看日志: $LOG"
    cat "$LOG"
fi
