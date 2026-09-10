"""卡死自 watchdog 测试（P2：区分「微信掉线」与「sidecar 卡死」的自愈半边）

2026-09-10 事故形态：msg.send 的 UIA 挂起占死 sidecar 单线程 dispatch 循环
→ 心跳 is_online 连坐排队 → 服务端 60s 指令超时连环炸。进程内无人能打断
主线程，但守护线程可以 os._exit(81)：Rust reaper 收尸 → Supervisor 既有
退避重启路径拉起新 sidecar 重跑 wx.init（state.rs run 循环，无需 Rust 改动）。

此处测纯决策与自杀路径（monkeypatch os._exit 防真杀 pytest）。
"""
import sys
import os
import time

sys.path.insert(0, os.path.dirname(__file__))

import pytest  # noqa: E402
import sidecar  # noqa: E402


@pytest.fixture(autouse=True)
def _reset_dispatch_since():
    """测试隔离：_dispatch_since 是模块级全局，用例间复位"""
    sidecar._dispatch_since = None
    yield
    sidecar._dispatch_since = None


# ══════════ 纯决策函数：在途超阈值才裁卡死，空闲永不 ══════════


def test_watchdog_should_exit_idle_never():
    """空闲（started=None）永不退出——没有在途 dispatch 就没有卡死可言"""
    assert sidecar._watchdog_should_exit(None, time.time(), 120.0) is False


def test_watchdog_should_exit_below_threshold_no():
    """在途但未超阈值：正常慢操作（wx.init ~15s / listen.add 重试）不误杀"""
    now = time.time()
    assert sidecar._watchdog_should_exit(now - 60, now, 120.0) is False
    assert sidecar._watchdog_should_exit(now - 119.9, now, 120.0) is False


def test_watchdog_should_exit_above_threshold_yes():
    """在途超过阈值：UIA 挂死（永不返回）裁定自杀重启"""
    now = time.time()
    assert sidecar._watchdog_should_exit(now - 120.1, now, 120.0) is True
    assert sidecar._watchdog_should_exit(now - 600, now, 120.0) is True


# ══════════ 阈值解析：env 可调 + 非法回退 + 下限防误配秒杀 ══════════


def test_stuck_threshold_default_and_env():
    """缺省 120s；env 数字生效；非法回退默认"""
    assert sidecar._stuck_threshold_from_env({}) == 120.0
    assert sidecar._stuck_threshold_from_env({"WXAUTO_STUCK_TIMEOUT": "300"}) == 300.0
    assert sidecar._stuck_threshold_from_env({"WXAUTO_STUCK_TIMEOUT": "abc"}) == 120.0


def test_stuck_threshold_floor():
    """下限 30s：误配 1s 会把正常 wx.init（~15s）杀成重启风暴"""
    assert sidecar._stuck_threshold_from_env({"WXAUTO_STUCK_TIMEOUT": "1"}) == 30.0
    assert sidecar._stuck_threshold_from_env({"WXAUTO_STUCK_TIMEOUT": "0"}) == 30.0


# ══════════ 自杀路径：卡死裁定 → ERROR 留痕 → os._exit(81) ══════════


def test_run_watchdog_exits_81_when_stuck(monkeypatch, capsys):
    """在途超阈值：watchdog 以退出码 81 自杀（Rust reaper 按 code 留痕，
    Supervisor 任意退出码都会走退避重启——退出码仅作排障锚点）"""
    exits = []
    monkeypatch.setattr(sidecar.os, "_exit", lambda code: exits.append(code))
    sleeps = []
    monkeypatch.setattr(sidecar.time, "sleep", lambda s: sleeps.append(s))
    # 模拟 dispatch 已卡死 200s（超 120s 阈值）
    sidecar._dispatch_since = time.time() - 200
    sidecar._run_watchdog(threshold=120.0, check_interval=10.0)
    assert exits == [81], "卡死裁定必须自杀且退出码为 81"
    assert sleeps, "判定前应有检查间隔等待"
    err = capsys.readouterr().err
    assert "[SIDECAR] [ERROR]" in err
    assert "卡死" in err


def test_run_watchdog_injected_clock(monkeypatch):
    """注入 sleep/now 双时钟：watchdog 判定不依赖真实墙钟（可测性注入点）"""
    exits = []
    monkeypatch.setattr(sidecar.os, "_exit", lambda code: exits.append(code))
    clock = {"now": 1000.0}
    sidecar._dispatch_since = 800.0  # 按注入时钟已卡 200s
    sidecar._run_watchdog(
        threshold=120.0,
        check_interval=10.0,
        sleep=lambda s: None,
        now=lambda: clock["now"],
    )
    assert exits == [81]


def test_mark_dispatch_enter_exit():
    """在途标记：enter 写时间戳 / exit 清 None（main 循环 try/finally 的两半）"""
    sidecar._mark_dispatch_enter()
    assert sidecar._dispatch_since is not None
    sidecar._mark_dispatch_exit()
    assert sidecar._dispatch_since is None
