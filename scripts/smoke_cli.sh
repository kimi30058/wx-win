#!/usr/bin/env bash
# Task 7 CLI 冒烟：mock WS 网关 ↔ Rust CLI ↔ mock sidecar 全链路闭环。
#
# 流程：
#   1. 起 python mock WS 服务端（127.0.0.1:60021，校验 ?token=）
#   2. 起 wxauto-desktop CLI（WXAUTO_MOCK=1 → sidecar 全方法假数据）
#   3. mock 服务端收 hello → 下发 command get_my_info → 收 result → 断言
#      data.wxid == "mock_wx"（mock sidecar 固定值）
#   4. SIGINT CLI → 验证显式 shutdown（无 python sidecar 残留）
#
# 用法：bash desktop/scripts/smoke_cli.sh [cargo|binary]
#   cargo  —— cargo run（默认；开发联调）
#   binary —— 跑已构建的 target/debug/wxauto-desktop（CI/快速）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # desktop/
SRC_TAURI="$ROOT/src-tauri"
# 默认端口避开主 checkout 的 NestJS 网关（60021 已被占用会绑架冒烟结果）
PORT="${WXAUTO_SMOKE_PORT:-60921}"
TOKEN="smoke-token"
MODE="${1:-cargo}"

echo "[smoke] 模式: $MODE  端口: $PORT"

# 1. mock WS 服务端（后台；日志落 tmp）
SMOKE_LOG="$(mktemp /tmp/wxauto-smoke-server.XXXXXX.log)"
WXAUTO_SMOKE_PORT="$PORT" WXAUTO_SMOKE_TOKEN="$TOKEN" \
    python3 "$ROOT/scripts/mock_ws_server.py" >"$SMOKE_LOG" 2>&1 &
SERVER_PID=$!
trap 'kill -9 "$SERVER_PID" 2>/dev/null || true' EXIT
echo "[smoke] mock server pid=$SERVER_PID log=$SMOKE_LOG"
# 端口就位探测（绑定失败立即失败，不空等）
for _ in $(seq 1 20); do
    if ss -ltn 2>/dev/null | grep -q "127.0.0.1:$PORT "; then
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "[smoke] FAIL: mock server 启动即退出（端口被占？）"
        cat "$SMOKE_LOG"
        exit 1
    fi
    sleep 0.25
done

# 2. 起 CLI（后台；日志落 tmp）。等端口被 CLI 连上前先让 server 就位
sleep 0.5
CLI_LOG="$(mktemp /tmp/wxauto-smoke-cli.XXXXXX.log)"
export WXAUTO_MOCK=1
export WXAUTO_SERVER_URL="ws://127.0.0.1:$PORT"
export WXAUTO_DEVICE_TOKEN="$TOKEN"
export WXAUTO_SIDECAR_CMD="python3 $ROOT/sidecar-python/sidecar.py"
if [ "$MODE" = "binary" ]; then
    "$SRC_TAURI/target/debug/wxauto-desktop" --cli >"$CLI_LOG" 2>&1 &
else
    (cd "$SRC_TAURI" && cargo run --quiet -- --cli) >"$CLI_LOG" 2>&1 &
fi
CLI_PID=$!
echo "[smoke] CLI pid=$CLI_PID log=$CLI_LOG"

# 3. 等 mock server 自了断（PASS/FAIL 都会退出；上限 90s 含 cargo build）
for _ in $(seq 1 180); do
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        break
    fi
    sleep 0.5
done
SERVER_RC=0
wait "$SERVER_PID" || SERVER_RC=$?

# 4. SIGINT CLI → 验证显式 shutdown
kill -INT "$CLI_PID" 2>/dev/null || true
CLI_RC=0
for _ in $(seq 1 20); do
    if ! kill -0 "$CLI_PID" 2>/dev/null; then
        break
    fi
    sleep 0.5
done
if kill -0 "$CLI_PID" 2>/dev/null; then
    echo "[smoke] WARN: CLI 未在 10s 内退出，SIGKILL 兜底"
    kill -9 "$CLI_PID" 2>/dev/null || true
    CLI_RC=1
fi
wait "$CLI_PID" 2>/dev/null || CLI_RC=${CLI_RC:-$?}

echo "===== mock server 日志 ====="
cat "$SMOKE_LOG"
echo "===== CLI 日志（尾部 40 行） ====="
tail -40 "$CLI_LOG"

# 5. outbox 断言：冒烟全程事件被 ack 清除（文件不存在或空）
OUTBOX="$HOME/.wxauto-desktop/outbox.jsonl"
if [ -s "$OUTBOX" ]; then
    echo "[smoke] FAIL: outbox 残留积压（事件未被 ack）: $(wc -l < "$OUTBOX") 行"
    cat "$OUTBOX"
    exit 1
fi
echo "[smoke] outbox 残留断言 PASS（文件空/不存在）"

# 6. 残留进程断言：CLI 退出后不允许再有 sidecar python 进程挂着。
#    匹配模式锚定 python 解释器 + 脚本路径（^python3?），排除 pgrep 自身与
#    外层 bash -c 包装（其 cmdline 含整段脚本文本会误匹配）
RESIDUAL=$(pgrep -f "^python3? .*/sidecar-python/sidecar\.py" || true)
if [ -n "$RESIDUAL" ]; then
    echo "[smoke] FAIL: sidecar 进程残留: $RESIDUAL（显式 shutdown 未生效）"
    kill -9 $RESIDUAL 2>/dev/null || true
    exit 1
fi

if [ "$SERVER_RC" -ne 0 ]; then
    echo "[smoke] FAIL: mock server 校验未通过（rc=$SERVER_RC）"
    exit 1
fi
if [ "$CLI_RC" -ne 0 ]; then
    echo "[smoke] WARN: CLI 退出码 $CLI_RC（见上方日志）"
fi
echo "[smoke] 全链路冒烟 PASS（hello → command → result → Ctrl-C 显式回收）"
