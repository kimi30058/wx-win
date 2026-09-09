"""sidecar 日志：stderr 单通道（stdout 是 JSON-RPC 帧专用——铁律不得混入）。

编码前提：sidecar.py 启动时已把 sys.stderr reconfigure 成 UTF-8——本模块
只管写，编码由入口统一保证（Rust 读循环按 UTF-8 字节消费）。
Rust 侧 RingStderrSink 逐行消费本输出进内存 ring + 前端日志 tab + 落盘
（[SIDECAR] [ERROR] 行被 ERROR 关键字升 error）。写失败静默——日志
永不阻断业务。参考原型 SiverWXbot logger.py 精简（无文件通道：落盘由
Rust 侧 FileSink 统一收口，Python 不重复写文件）。
"""
import sys
import threading
import time
from contextlib import contextmanager

_lock = threading.Lock()


def log(level: str, msg: str) -> None:
    """写一条日志到 stderr。level: "INFO"|"WARN"|"ERROR"。永不抛异常。"""
    try:
        line = time.strftime("[%Y-%m-%d %H:%M:%S]") + f" [SIDECAR] [{level}] {msg}\n"
        with _lock:
            sys.stderr.write(line)
            sys.stderr.flush()
    except Exception:  # noqa: BLE001 — 日志永不阻断业务
        pass


@contextmanager
def guard_stdout():
    """真实路径期间把 sys.stdout 临时换成 stderr——wxautox4 会向 stdout
    print 授权横幅（2026-09-09 CI 六跑实证），污染 JSON-RPC 帧通道；
    重定向后横幅走日志通道（ring+落盘），协议恢复纯净。"""
    old = sys.stdout
    sys.stdout = sys.stderr
    try:
        yield
    finally:
        sys.stdout = old
