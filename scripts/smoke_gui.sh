#!/usr/bin/env bash
# Task 9 GUI 冒烟（Linux 无显示环境经 Xvfb）：
#   1. 起 python mock WS 服务端（127.0.0.1:60931，校验 ?token=）
#   2. 写入临时 HOME 的 autoConnect=true 配置（GUI 启动即自动连 mock 网关）
#   3. Xvfb 起 GUI 二进制（WXAUTO_MOCK=1 → sidecar 全方法假数据）
#   4. mock 服务端收 hello → 下发 command get_my_info → 收 result → 断言
#      data.wxid == "mock_wx" 后自了断（PASS 信号）
#   5. 断言 GUI 进程存活窗口内无崩溃；退出后无 sidecar 残留
#
# 用法：bash desktop/scripts/smoke_gui.sh  （需先 cargo build 出 debug 二进制）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # desktop/
BIN="$ROOT/src-tauri/target/debug/wxauto-desktop"
PORT="${WXAUTO_GUI_SMOKE_PORT:-60931}"
TOKEN="gui-smoke-token"

if [ ! -x "$BIN" ]; then
    echo "[gui-smoke] FAIL: 二进制不存在（先 cargo build）: $BIN"
    exit 1
fi
command -v xvfb-run >/dev/null || { echo "[gui-smoke] FAIL: 缺 xvfb-run"; exit 1; }

# 隔离 HOME：GUI 的 ~/.wxauto-desktop/config.json 不污染开发者真 home
SMOKE_HOME="$(mktemp -d /tmp/wxauto-gui-home.XXXXXX)"
mkdir -p "$SMOKE_HOME/.wxauto-desktop"
cat > "$SMOKE_HOME/.wxauto-desktop/config.json" <<EOF
{
  "serverUrl": "ws://127.0.0.1:$PORT",
  "channelId": "",
  "autoConnect": true,
  "listenNames": [],
  "delayMinMs": 500,
  "delayMaxMs": 1000
}
EOF

# 1. mock WS 服务端
SMOKE_LOG="$(mktemp /tmp/wxauto-gui-server.XXXXXX.log)"
WXAUTO_SMOKE_PORT="$PORT" WXAUTO_SMOKE_TOKEN="$TOKEN" \
    python3 "$ROOT/scripts/mock_ws_server.py" >"$SMOKE_LOG" 2>&1 &
SERVER_PID=$!
cleanup() {
    kill -9 "$SERVER_PID" "$GUI_PID" 2>/dev/null || true
    pkill -9 -f "^python3? .*/sidecar-python/sidecar\.py" 2>/dev/null || true
    rm -rf "$SMOKE_HOME"
}
trap cleanup EXIT
for _ in $(seq 1 20); do
    ss -ltn 2>/dev/null | grep -q "127.0.0.1:$PORT " && break
    kill -0 "$SERVER_PID" 2>/dev/null || { echo "[gui-smoke] FAIL: mock server 启动即退出"; cat "$SMOKE_LOG"; exit 1; }
    sleep 0.25
done
echo "[gui-smoke] mock server pid=$SERVER_PID port=$PORT"

# 2. Xvfb 起 GUI（HOME 隔离 + mock 链路 env）
GUI_LOG="$(mktemp /tmp/wxauto-gui-app.XXXXXX.log)"
env -i HOME="$SMOKE_HOME" PATH="$PATH" DISPLAY=:99 WXAUTO_MOCK=1 \
    WXAUTO_SERVER_URL="ws://127.0.0.1:$PORT" WXAUTO_DEVICE_TOKEN="$TOKEN" \
    WXAUTO_SIDECAR_CMD="python3 $ROOT/sidecar-python/sidecar.py" RUST_LOG=info \
    xvfb-run -a "$BIN" >"$GUI_LOG" 2>&1 &
GUI_PID=$!
echo "[gui-smoke] GUI pid=$GUI_PID log=$GUI_LOG"

# 3. GUI 存活断言：5s 内不崩（窗口/装配失败多数 2s 内 exit）
sleep 5
if ! kill -0 "$GUI_PID" 2>/dev/null; then
    echo "[gui-smoke] FAIL: GUI 5s 内退出（装配崩溃）"
    cat "$GUI_LOG"
    exit 1
fi
echo "[gui-smoke] GUI 存活 OK（事件循环 + sidecar 装配通过）"

# 4. 等 mock server 自了断（收到 hello→command→result 断言链后退出；上限 60s）
for _ in $(seq 1 120); do
    kill -0 "$SERVER_PID" 2>/dev/null || break
    sleep 0.5
done
SERVER_RC=0
wait "$SERVER_PID" || SERVER_RC=$?

kill -TERM "$GUI_PID" 2>/dev/null || true
sleep 2
kill -9 "$GUI_PID" 2>/dev/null || true

echo "===== mock server 日志 ====="
cat "$SMOKE_LOG"
echo "===== GUI 日志（尾部 30 行） ====="
tail -30 "$GUI_LOG"

# 5. sidecar 残留断言（GUI SIGKILL 下可能残留——记 WARN 不 FAIL：
#    正常退出路径（窗口关闭）才走 graceful_shutdown；本脚本 kill -9 模拟强杀）
RESIDUAL=$(pgrep -f "^python3? .*/sidecar-python/sidecar\.py" || true)
if [ -n "$RESIDUAL" ]; then
    echo "[gui-smoke] WARN: 强杀 GUI 后 sidecar 残留（预期内，正常退出走 graceful回收）: $RESIDUAL"
fi

# 6. invoke 通路断言（C1 防回归，审查要求）：mock-runtime 经 generate_handler
#    注册 → manage(AppStateCtx) → State<'_, AppStateCtx> 取态 → resolve 的
#    完整 invoke 链。Xvfb 的 webview 无 devtools 协议跑不了 JS evaluate，
#    以 tauri::test 的 get_ipc_response 作为 invoke 链路等效探针（同一
#    命令注册表/状态管理器/分发代码路径）。
if (cd "$ROOT/src-tauri" && cargo test --quiet --bin wxauto-desktop test_invoke_get_app_state_managed_type_key_matches 2>&1 | tail -5 | grep -q "test result: ok"); then
    echo "[gui-smoke] invoke 通路 OK（mock-runtime get_app_state resolve）"
else
    echo "[gui-smoke] FAIL: invoke 通路断言失败（C1 回归？manage/State 类型键不匹配）"
    (cd "$ROOT/src-tauri" && cargo test --bin wxauto-desktop test_invoke_get_app_state_managed_type_key_matches 2>&1 | tail -15)
    exit 1
fi

if [ "$SERVER_RC" -eq 0 ]; then
    echo "[gui-smoke] PASS: GUI 全链路（hello/command/result）闭环 + invoke 通路"
else
    echo "[gui-smoke] FAIL: mock server 断言失败（rc=$SERVER_RC）"
    exit 1
fi
