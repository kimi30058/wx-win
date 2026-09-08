"""WxAuto sidecar — wxautox4 薄封装，JSON-RPC 2.0 over stdio（spec §3.2）

单线程 dispatch 循环；WXAUTO_MOCK=1 时全方法假数据（Linux 开发/CI）。
本 Task（Task 1）交付骨架 + wx.init / wx.get_my_info / wx.is_online 三方法，
其余 17 方法在 Task 2 的 methods.py 补齐。
"""
import json
import os
import sys
import threading

# stdout 强制 UTF-8：Windows 默认代码页（GBK 等）下 ensure_ascii=False 的
# 中文 JSON 帧会 UnicodeEncodeError（PyInstaller 冻结后在中文 Windows 上
# 同样命中——Rust 侧按 UTF-8 字节读，两端必须一致）。stderr 不动（人读）。
for _stream in (sys.stdout,):
    if hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8", errors="replace")
    else:  # PyInstaller 某些嵌入流无 reconfigure → 用 TextIOWrapper 重包
        import io
        sys.stdout = io.TextIOWrapper(
            _stream.buffer, encoding="utf-8", errors="replace", line_buffering=True
        )

# 业务错误类型以 methods.py 为准（-32000 契约源）；本模块不再定义同名类，
# 历史上 sidecar.SidecarError 与 methods.SidecarError 同名不同源，导致
# except 永不命中、业务错误全落 -32603（审查 C1）。
import methods  # noqa: E402 — 顶部导入保证 except methods.SidecarError 可解析

MOCK = os.environ.get("WXAUTO_MOCK", "") == "1"

# ── wxautox4 全局（真实模式延迟导入；本文件不 import wxautox4——Linux 上无此库）──
_wx = None                 # wxautox4.WxAuto 实例
_msg_pool = {}             # {msg_id: (msg, chat_who)}，60s 过期（Task 2 的 quote/forward 用）
_msg_pool_ts = {}

# stdout 写锁：Task 2 起 _notify 会被 wxautox4 监听线程并发调用（on_msg 回调），
# 与主线程 _result/_error 无锁并发会产生交错帧（写+flush 非原子）——三写函数
# 共持此锁，写+flush 全程串行（审查 M6）。
_io_lock = threading.Lock()


def _prune_msg_pool():
    """清理 60s 过期的 msg 对象（迭代走快照——监听线程会并发插入，审查 I1）"""
    import time
    now = time.time()
    expired = [k for k, ts in list(_msg_pool_ts.items()) if now - ts > 60]
    for k in expired:
        _msg_pool.pop(k, None)
        _msg_pool_ts.pop(k, None)


def _result(id_, result):
    with _io_lock:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id_, "result": result}, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def _error(id_, code, message):
    with _io_lock:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id_, "error": {"code": code, "message": message}}, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def _notify(method, params):
    with _io_lock:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def _get_wx():
    """取全局 wx 实例；MOCK 模式恒 None（methods.py 走 mock 分支短路）"""
    if MOCK:
        return None
    if _wx is None:
        raise RuntimeError("wx 未初始化，先调用 wx.init")
    return _wx


def dispatch(method, params):
    """方法路由；返回 result 或抛异常（由外层转 error 帧）
    真实 wxautox4 导入在 methods.py 的 _init 里且被 MOCK 分支短路——本文件零 import。
    wx.init 成功后从 methods.get_wx_instance() 拉实例回写本模块全局 _wx
    （不 `import sidecar`——本文件以 __main__ 运行时会加载第二份模块实例）。

    wx 实参惰性求值豁免名单（wx.init / wx.activate）：wx.init 本身创建实例
    （_init 忽略 wx 参数），调用前实例必为 None，不能先求值 _get_wx()（Python
    在调用前求值全部实参，否则首次 wx.init 直接 RuntimeError，本体不可达）。
    wx.activate 同理：未激活场景 wx.init 必失败、_wx 恒 None，而激活恰是
    「未激活」场景的用户动作——不豁免则 _get_wx() 先抛「wx 未初始化」，
    激活码根本到不了 methods._activate（终审 C1：真实设备激活 100% 不可用；
    pytest 直连 methods.dispatch 绕过本包装层 + mock 冒烟 _wx 恒 None，
    双重盲区漏进主线）。_activate 真实分支只触 wxautox4.utils.useful，
    不消费 wx 实参，豁免无副作用。
    """
    global _wx
    if _wx is None and not MOCK:
        # 首次（或重启后）从 methods 拉实例——覆盖「init 后同进程再调用」的场景
        inst = methods.get_wx_instance()
        if inst is not None:
            _wx = inst
    wx_arg = None if method in ("wx.init", "wx.activate") else _get_wx()
    result = methods.dispatch(method, params, wx_arg, _msg_pool, _msg_pool_ts, _notify, MOCK)
    # wx.init 刚创建实例 → 立即回写，后续调用直接走全局
    if method == "wx.init" and not MOCK and _wx is None:
        inst = methods.get_wx_instance()
        if inst is not None:
            _wx = inst
    return result


def main():
    if not MOCK:
        # wxautox4 是 COM/UIA 库，主线程初始化（参考项目 web_server.py:1256 验证）
        try:
            import pythoncom
            pythoncom.CoInitialize()
        except ImportError:
            pass  # 非 Windows 开发态兜底（真实部署必有）
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue  # 非法行丢弃（Rust 侧超时兜底）
        if "id" not in req or not isinstance(req.get("id"), (int, str)):
            continue  # sidecar 不接收通知；只有服务器→设备方向
        try:
            result = dispatch(req.get("method", ""), req.get("params", {}))
            _result(req["id"], result)
        except methods.SidecarError as e:
            # 业务错误 → -32000（methods.SidecarError 是 methods.py 抛出的类型；
            # 本文件顶部的 SidecarError 类与之同名不同源，捕获它永不命中）
            _error(req["id"], -32000, str(e))
        except Exception as e:  # noqa: BLE001 — sidecar 边界统一转 error 帧
            _error(req["id"], -32603, f"{type(e).__name__}: {e}")


if __name__ == "__main__":
    main()
