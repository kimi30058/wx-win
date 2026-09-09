"""sidecar_log 单元测试：格式 / 线程锁 / 异常吞没（spec §4）"""
import io
import sys
import threading

import os
sys.path.insert(0, os.path.dirname(__file__))

import sidecar_log  # noqa: E402


def _capture(func):
    """临时替换 stderr 捕获输出"""
    buf = io.StringIO()
    old = sys.stderr
    sys.stderr = buf
    try:
        func()
    finally:
        sys.stderr = old
    return buf.getvalue()


def test_format_contains_timestamp_tag_level():
    out = _capture(lambda: sidecar_log.log("ERROR", "boom"))
    assert "[SIDECAR]" in out
    assert "[ERROR]" in out
    assert "boom" in out
    # 时间戳形态 YYYY-MM-DD HH:MM:SS
    parts = out.split("] [")
    assert len(parts[0]) == 20  # "[YYYY-MM-DD HH:MM:SS" = 1(方括号)+19(时间戳)


def test_info_level_passthrough():
    out = _capture(lambda: sidecar_log.log("INFO", "hello"))
    assert "[INFO]" in out and "hello" in out


def test_write_failure_silent(capsys):
    """stderr 写失败必须静默——日志永不阻断业务（spec 铁律）"""
    class Boom:
        def write(self, *_):
            raise OSError("disk full")

        def flush(self):
            pass

    old = sys.stderr
    sys.stderr = Boom()
    try:
        sidecar_log.log("ERROR", "x")  # 不抛即通过
    finally:
        sys.stderr = old


def test_thread_safety_no_interleave():
    """并发 50 线程各写一行——输出行数完整无交错（锁的意义）"""
    buf = io.StringIO()
    old = sys.stderr
    sys.stderr = buf
    try:
        threads = [threading.Thread(target=lambda i=i: sidecar_log.log("INFO", f"line-{i}")) for i in range(50)]
        [t.start() for t in threads]
        [t.join() for t in threads]
    finally:
        sys.stderr = old
    lines = [l for l in buf.getvalue().splitlines() if l.strip()]
    assert len(lines) == 50
    assert sum(1 for l in lines if "line-" in l) == 50
