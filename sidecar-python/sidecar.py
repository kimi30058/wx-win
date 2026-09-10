"""WxAuto sidecar — wxautox4 薄封装，JSON-RPC 2.0 over stdio（spec §3.2）

单线程 dispatch 循环；WXAUTO_MOCK=1 时全方法假数据（Linux 开发/CI）。
本 Task（Task 1）交付骨架 + wx.init / wx.get_my_info / wx.is_online 三方法，
其余 17 方法在 Task 2 的 methods.py 补齐。
"""
import json
import os
import sys
import threading
import time

# ── stdio 三流全部强制 UTF-8（2026-09-09 真机乱码事故：stdin 是第三个漏网流）──
# stdout：Rust 读循环按 UTF-8 消费 JSON-RPC 帧，Windows 默认代码页（GBK 等）
#   下 ensure_ascii=False 的中文帧会 UnicodeEncodeError（PyInstaller 冻结后
#   在中文 Windows 上同样命中）。
# stderr：日志走 Rust 按 UTF-8 消费（单窗口日志 tab+落盘），两端必须一致——
#   否则 zh-CN locale（cp936）下中文日志行编码成 GBK，进前端与落盘全是乱码。
# stdin（本次补齐）：Rust 侧 serde_json 写出的请求帧恒为 UTF-8 字节，而
#   Windows 下 sys.stdin 默认按本地代码页（cp936）解码——「龚金龙」变
#   「钁ｃ\udca5栭緳」式乱码且带 lone surrogate：listen.add 拿乱码去搜微信
#   会话必然失败，wxautox4 内部 logging 编码 surrogate 再炸 UnicodeEncodeError
#   连锁刷屏。stdin 归一 UTF-8 后整条链路（Rust UTF-8 写 → stdin UTF-8 读）
#   与 locale 无关。
def _ensure_utf8_streams():
    """stdout/stderr/stdin 三流归一 UTF-8（幂等；无 buffer 的流静默跳过）

    stdin 用 errors="surrogateescape"：与 Windows 默认 stdin 行为对齐——
    字节流里偶发非 UTF-8 残片（理论不该有，Rust 恒写 UTF-8）时以代理项
    透传而非炸 UnicodeDecodeError 杀死主循环；JSON 层会把它解析成非法帧
    丢弃（main 循环对 JSONDecodeError 有兜底）。
    stdout/stderr 维持 errors="replace"：出向流绝不能因不可编码字符断帧。
    """
    import io

    for _stream, _errors in ((sys.stdout, "replace"), (sys.stderr, "replace"),
                             (sys.stdin, "surrogateescape")):
        if hasattr(_stream, "reconfigure"):
            _stream.reconfigure(encoding="utf-8", errors=_errors)
        else:
            _buf = getattr(_stream, "buffer", None)
            # buffer 必须是真二进制层（有 readinto/write）才可重包：pytest 的
            # DontReadFromInput 假 buffer 返回自身（TextIO 假件），重包会在
            # flush 时 UnsupportedOperation——测试环境下跳过归一（假 stdin
            # 本就不承担编码职责，monkeypatch 会替换成真流）
            if _buf is None or not hasattr(_buf, "readinto"):
                continue
            _wrapped = io.TextIOWrapper(
                _buf, encoding="utf-8", errors=_errors, line_buffering=True
            )
            if _stream is sys.stdout:
                sys.stdout = _wrapped
            elif _stream is sys.stderr:
                sys.stderr = _wrapped
            else:
                sys.stdin = _wrapped


_ensure_utf8_streams()

# 业务错误类型以 methods.py 为准（-32000 契约源）；本模块不再定义同名类，
# 历史上 sidecar.SidecarError 与 methods.SidecarError 同名不同源，导致
# except 永不命中、业务错误全落 -32603（审查 C1）。
import methods  # noqa: E402 — 顶部导入保证 except methods.SidecarError 可解析
import sidecar_log  # noqa: E402 — 同目录；日志走 stderr（stdout 铁律专用 JSON-RPC）

MOCK = os.environ.get("WXAUTO_MOCK", "") == "1"


# ── CA 证书持久化（P2 任务 9）────────────────────────────────────────────
# PyInstaller onefile 的 cacert.pem 位于 %TEMP%\_MEIxxxx 临时目录，挂机
# 电脑的「存储感知/管家类软件」会清理 %TEMP% → TLS 全炸、重启后恢复
# （参考项目 web_server.py install_persistent_ca_bundle 的真实生产事故）。
# sidecar 同为 onefile 冻结 + wxautox4 激活/授权走 HTTPS（requests）——
# 同款暴露面。启动时把 pem 复制到持久目录（与 config.json 同目录），
# 并让全部 HTTP 库改用持久副本。


def install_persistent_ca_bundle(persistent_dir=None):
    """onefile 冻结态把 cacert.pem 落到持久目录并接管证书路径。

    - 非冻结（开发/CI）：直接返回 False，不动任何 env
    - 冻结态：certifi.where() 原子复制（tmp+os.replace）到持久目录，
      设 REQUESTS_CA_BUNDLE / CURL_CA_BUNDLE / SSL_CERT_FILE 三 env
      （requests/httpx/openai 系各自读取），并 monkeypatch certifi.where
      ——httpcore 的默认证书上下文也走持久副本
    - 任何失败只 WARN 不炸：证书复制失败时各库仍走原 _MEI 路径，
      行为不劣于现状
    """
    if not getattr(sys, "frozen", False):
        return False
    try:
        import certifi  # noqa: PLC0415 — 冻结态才有意义，延迟导入
        import shutil

        if persistent_dir is None:
            home = os.path.expanduser("~") or "."
            persistent_dir = os.path.join(home, ".wxauto-desktop")
        os.makedirs(persistent_dir, exist_ok=True)
        persistent = os.path.join(persistent_dir, "cacert.pem")

        src = certifi.where()
        if src and os.path.exists(src):
            tmp = persistent + ".tmp"
            shutil.copyfile(src, tmp)
            os.replace(tmp, persistent)
        if not os.path.exists(persistent):
            sidecar_log.log("WARN", f"持久 CA 证书不可得（源缺失）: {src}")
            return False

        os.environ["REQUESTS_CA_BUNDLE"] = persistent   # requests 每请求时读取
        os.environ["CURL_CA_BUNDLE"] = persistent       # 双保险
        os.environ["SSL_CERT_FILE"] = persistent        # httpx(openai 系) trust_env
        certifi.where = lambda: persistent              # httpcore 默认证书上下文
        sidecar_log.log("INFO", f"持久 CA 证书包已安装: {persistent}")
        return True
    except Exception as e:  # noqa: BLE001 — 诊断通道不得阻断启动
        sidecar_log.log("WARN", f"安装持久 CA 证书包失败（继续用默认路径）: {e}")
        return False


install_persistent_ca_bundle()

# ── wxautox4 全局（真实模式延迟导入；本文件不 import wxautox4——Linux 上无此库）──
_wx = None                 # wxautox4.WxAuto 实例
_msg_pool = {}             # {msg_id: (msg, chat_who)}，60s 过期（Task 2 的 quote/forward 用）
_msg_pool_ts = {}

# stdout 写锁：Task 2 起 _notify 会被 wxautox4 监听线程并发调用（on_msg 回调），
# 与主线程 _result/_error 无锁并发会产生交错帧（写+flush 非原子）——三写函数
# 共持此锁，写+flush 全程串行（审查 M6）。
_io_lock = threading.Lock()


# ── 卡死自 watchdog（P2，2026-09-10 指令超时事故的自愈半边）──────────────
# 事故形态：msg.send 的 UIA 调用挂起占死本进程的单线程 dispatch 循环 →
# 后续所有 RPC（含 wx.is_online 心跳）排队 → Rust 侧 60s 指令超时连环炸、
# wxOnline 假翻转。进程内无人能打断主线程——但守护线程可以 os._exit：
# Rust reaper 收尸（exit code 81 留痕）→ Supervisor 既有退避重启路径拉起
# 新 sidecar 重跑 wx.init（state.rs run 循环，Rust 侧零改动）。
#
# 阈值取舍：> RPC_TIMEOUT(30s) 且 > 正常慢操作上限（wx.init ~15s 冷启动、
# listen.add 三重校验重试可达数十秒）——120s 只裁「永不返回」的真挂死。
_dispatch_since = None  # None=空闲；否则当前 dispatch 开始时刻（time.time()）


def _mark_dispatch_enter():
    """main 循环进入 dispatch 前打点（watchdog 据此判「在途」）"""
    global _dispatch_since
    _dispatch_since = time.time()


def _mark_dispatch_exit():
    """dispatch 结束（含异常路径，try/finally 调用）清在途标记"""
    global _dispatch_since
    _dispatch_since = None


def _stuck_threshold_from_env(env=None):
    """WXAUTO_STUCK_TIMEOUT 解析：缺省 120s；非法回退默认；下限 30s
    （误配 1s 会把正常 wx.init ~15s 杀成重启风暴）。纯函数可单测。"""
    raw = (os.environ if env is None else env).get("WXAUTO_STUCK_TIMEOUT")
    try:
        v = float(raw) if raw else 120.0
    except (TypeError, ValueError):
        sidecar_log.log("WARN", f"WXAUTO_STUCK_TIMEOUT 非法({raw!r})，回退默认 120s")
        v = 120.0
    return max(v, 30.0)


def _watchdog_should_exit(started, now, threshold):
    """卡死裁定（纯函数可单测）：在途超过阈值才裁；空闲（None）永不退出。"""
    return started is not None and now - started > threshold


def _run_watchdog(threshold=None, check_interval=10.0, sleep=None, now=None):
    """watchdog 主循环（守护线程跑；sleep/now 可注入供测试）。

    裁定卡死后：stderr 留 ERROR 痕（GUI 运行日志可见）→ os._exit(81)。
    不走 _notify（stdout 帧）：_io_lock 若恰被卡住的主线程持有会连坐
    watchdog；stderr 独立流无锁，Rust stderr 读循环恒在消费。
    残余风险（接受）：UIA 的 C 扩展若不释放 GIL，本线程同样无法调度——
    那是全进程冻结，只能靠服务端 90s 心跳超时断 WS 兜底。
    """
    _sleep = sleep or time.sleep
    _now = now or time.time
    _threshold = _stuck_threshold_from_env() if threshold is None else threshold
    while True:
        _sleep(check_interval)
        started = _dispatch_since
        if not _watchdog_should_exit(started, _now(), _threshold):
            continue
        dur = _now() - started
        sidecar_log.log(
            "ERROR",
            f"dispatch 在途 {dur:.0f}s 超过阈值 {_threshold:.0f}s，判定 UIA 卡死，"
            f"watchdog 自杀(退出码81)触发 Supervisor 重启",
        )
        os._exit(81)
        return  # 测试桩 _exit 返回后的防自旋（生产不可达：_exit 不返回）


def _start_watchdog(check_interval=10.0):
    """main() 启动守护 watchdog 线程（daemon：主进程退出不阻收尸）"""
    threading.Thread(
        target=_run_watchdog,
        kwargs={"check_interval": check_interval},
        name="wxauto-watchdog",
        daemon=True,
    ).start()


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
    # 卡死自 watchdog：dispatch 在途超阈值（默认 120s）自杀重启（P2）
    _start_watchdog()
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
            _mark_dispatch_enter()
            result = dispatch(req.get("method", ""), req.get("params", {}))
            _result(req["id"], result)
        except methods.SidecarError as e:
            # 业务错误 → -32000（methods.SidecarError 是 methods.py 抛出的类型；
            # 本文件顶部的 SidecarError 类与之同名不同源，捕获它永不命中）
            _error(req["id"], -32000, str(e))
        except SystemExit as e:
            # wxautox4 未授权等场景的裸退（SystemExit 非 Exception 子类，下方
            # except Exception 接不住）——纵深防御：methods 层漏归一时仍有帧
            sidecar_log.log("ERROR", f"dispatch {req.get('method', '?')} 触发 SystemExit: {e}")
            _error(req["id"], -32603, f"wxautox4 异常退出: {e}")
        except Exception as e:  # noqa: BLE001 — sidecar 边界统一转 error 帧
            sidecar_log.log("ERROR", f"dispatch {req.get('method', '?')} 失败: {type(e).__name__}: {e}")
            _error(req["id"], -32603, f"{type(e).__name__}: {e}")
        finally:
            _mark_dispatch_exit()


if __name__ == "__main__":
    main()
