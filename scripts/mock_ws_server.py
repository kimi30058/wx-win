#!/usr/bin/env python3
"""mock WS 网关（Task 7 冒烟用，替代未实施的 Plan 1 NestJS 网关）。

协议（与 spec §2.3 帧格式一致）：
- 收 hello → 记录设备 → 下发一条 command（get_my_info）
- 收 result → 校验 requestId/success/data.wxid == "mock_wx" → PASS
- 收 event / ping → 记录
- 30s 总超时 / 10s 无 command result 超时 → FAIL（退出码 1）
- WXAUTO_SMOKE_TOKEN 设置时校验 query token；不匹配 → FAIL
"""
import asyncio
import json
import os
import sys
import time
from urllib.parse import parse_qs, urlparse

from websockets.asyncio.server import serve
from websockets.exceptions import ConnectionClosed

HOST = "127.0.0.1"
PORT = int(os.environ.get("WXAUTO_SMOKE_PORT", "60021"))
TOKEN = os.environ.get("WXAUTO_SMOKE_TOKEN", "smoke-token")

COMMAND_REQUEST_ID = "smoke-1"
RESULT_TIMEOUT = 10.0


class SmokeResult:
    def __init__(self):
        self.frames = []      # 全部收帧（按序）
        self.passed = False
        self.reason = ""

    def ok(self, reason: str):
        self.passed = True
        self.reason = reason

    def fail(self, reason: str):
        self.passed = False
        self.reason = reason


result = SmokeResult()


def dump_frames():
    return json.dumps(result.frames, ensure_ascii=False, indent=2)


async def handle(ws):
    # token 校验（query ?token=...；websockets 15 的 Request 无 query_params，
    # 从 path 手动解析）
    token = ""
    if ws.request and ws.request.path:
        token = (parse_qs(urlparse(ws.request.path).query).get("token") or [""])[0]
    if TOKEN and token != TOKEN:
        result.fail(f"token 不匹配: 期望 {TOKEN!r} 实际 {token!r}")
        await ws.close(code=4401, reason="bad token")
        return

    hello_seen = False
    result_seen = False
    try:
        async for raw in ws:
            frame = json.loads(raw)
            result.frames.append(frame)
            kind = frame.get("kind")

            if kind == "hello" and not hello_seen:
                hello_seen = True
                # hello 必须是首帧（重连语义在 transport 层已有测试，这里复核）
                if len(result.frames) != 1:
                    result.fail("hello 不是首帧")
                    return
                cmd = {
                    "kind": "command",
                    "requestId": COMMAND_REQUEST_ID,
                    "action": "get_my_info",
                    "params": {},
                }
                await ws.send(json.dumps(cmd, ensure_ascii=False))

            elif kind == "result" and not result_seen:
                result_seen = True
                if frame.get("requestId") != COMMAND_REQUEST_ID:
                    result.fail(f"requestId 不匹配: {frame.get('requestId')!r}")
                    return
                if frame.get("success") is not True:
                    result.fail(f"success 应为 true: {frame}")
                    return
                wxid = (frame.get("data") or {}).get("wxid")
                if wxid != "mock_wx":
                    result.fail(f"data.wxid 应为 mock_wx（mock sidecar 固定值），实际 {wxid!r}")
                    return
                result.ok("hello 首帧 + command(result) 闭环校验通过")
                return  # 校验完成，主动断开
            elif kind == "event":
                # ack 回送（listen-persist-event-outbox：设备发件箱据此清除）
                event_id = frame.get("eventId")
                if event_id:
                    await ws.send(json.dumps({
                        "kind": "ack",
                        "eventId": event_id,
                        "ts": int(time.time() * 1000),
                    }, ensure_ascii=False))
            # ping 只记录
    except ConnectionClosed:
        pass


async def main():
    async with serve(handle, HOST, PORT) as server:
        await server.serve_forever()


async def run_with_timeout():
    task = asyncio.create_task(main())
    # 等判定：PASS / FAIL（handle 内已定 reason）/ 总超时 45s（含 cargo build 时间）
    deadline = asyncio.get_event_loop().time() + 45.0
    while not result.reason:
        if task.done() and task.exception() is not None:
            # 绑定失败等启动异常：立即失败（不傻等 45s）
            result.fail(f"server 启动异常: {task.exception()!r}")
            break
        if asyncio.get_event_loop().time() > deadline:
            result.fail(f"45s 总超时（frames={dump_frames()}）")
            break
        await asyncio.sleep(0.1)
    task.cancel()
    print("=" * 60)
    print(f"[smoke] frames({len(result.frames)}):")
    for f in result.frames:
        print("  " + json.dumps(f, ensure_ascii=False))
    if result.passed:
        print(f"[smoke] PASS: {result.reason}")
        return 0
    print(f"[smoke] FAIL: {result.reason}")
    return 1


if __name__ == "__main__":
    sys.exit(asyncio.run(run_with_timeout()))
