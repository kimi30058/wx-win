"""21 个方法 → wxautox4 映射（全部按参考项目已验证用法；MOCK 模式返回假数据）

铁律：只使用 spec 附录 A 已验证 API；幻觉 API 清单（AddNewFriend / EditFriendInfo /
GetFriendDetails / GetHistoryMessage / KeepRunning / GetNextNewMessage(callback=) 等）
禁止出现。

模块级不 import wxautox4（Linux 上无此库）；真实导入只发生在 _init 的
非 MOCK 分支内，CI/开发态被 MOCK 短路。

实例回传约定：_init 成功后实例存本模块级 `_instance`，sidecar.py 的
dispatch 包装层通过 get_wx_instance() 拉取回写它自己的全局 _wx——
禁止 methods.py 里 `import sidecar` 回写（sidecar.py 以 __main__ 运行时，
import sidecar 会加载第二份模块实例，回写落空，真实模式下后续调用全部
误报「wx 未初始化」）。

字段命名：message.received 通知与 chat.history 消息元素用 snake_case
（msg_id/chat_who/chat_type/attr/msg_type/sender/content），与 Rust 侧
spec.rs RawMessage 严格对齐；camelCase 的 WS 帧转换在 Rust agent_link 层做。
"""
import hashlib
import os
import threading
import time
from collections import OrderedDict

import sidecar_log  # noqa: E402 — 同目录；日志走 stderr（stdout 铁律专用 JSON-RPC）


class SidecarError(Exception):
    """业务错误 → JSON-RPC -32000（sidecar.py 按本类捕获）"""


# wxautox4.WeChat 实例（仅 _init 成功后非 None；sidecar.py 从这里拉取）
_instance = None
# 授权状态（_init 真实分支写入；供上层判断是否进就绪态）
_license_ok = False
# mock 注入：未授权初始态（集成冒烟用；真实路径不受影响）。
# wx.activate 成功后翻转（见 _activate mock 分支）
_MOCK_LICENSE_STATE = os.environ.get("WXAUTO_MOCK_UNLICENSED", "") == "1"
# mock 注入：微信未开态（GUI 冒烟/前端联调用，镜像上面同款模式）
_MOCK_WECHAT_MISSING_STATE = os.environ.get("WXAUTO_MOCK_WECHAT_MISSING", "") == "1"


def get_wx_instance():
    """返回 _init 创建的 wxautox4 实例（供 sidecar.py 主模块回写自己的 _wx）"""
    return _instance


# ── 发送结果判定（spec 附录 B，参考项目验证逻辑）──────────────


def was_send_success(result):
    """发送结果双形态判定：bool 或 dict（status/success/code 键）"""
    if result is True:
        return True
    if result is False or result is None:
        return False
    if isinstance(result, dict):
        status = str(result.get("status", "")).lower()
        if status in ("success", "ok", "true", "成功"):
            return True
        if status in ("error", "fail", "failed", "false", "失败", "错误"):
            return False
        if result.get("code") == 0:
            return True
        if result.get("success") is True:
            return True
        if result.get("success") is False:
            return False
    return bool(result)


def send_error_message(result):
    """从 dict 结果提取错误信息；非 dict 统一「发送失败」"""
    if isinstance(result, dict):
        return str(result.get("message", result))
    return "发送失败"


# ── 窗口态工具（2026-09-10 真机四类失败：UIA 超时的可诊断化）────────


def _reset_listen_engine(wx):
    """监听引擎复位：StopListening → sleep 1 → StartListening（对齐参考
    项目 init_wx_listeners wxbot_core.py:2462-2464）。

    sidecar 崩溃重启后 wxautox4 引擎可能残留脏态（旧回调/半注册会话），
    不复位则后续 AddListenChat 全挂。任何一步失败降级 WARN——实例构造
    成功说明窗口在，引擎复位失败可能只是时序，不判 init 失败。
    """
    try:
        wx.StopListening()
        time.sleep(1)
        wx.StartListening()
        sidecar_log.log("INFO", "监听引擎复位完成(StopListening→StartListening)")
    except Exception as e:  # noqa: BLE001 — 复位失败降级：窗口在（构造成功），不判 init 死
        sidecar_log.log("WARN", f"监听引擎复位失败(不阻断 init，后续 AddListenChat 可能需重试): {type(e).__name__}: {e}")


# Windows 防睡眠常量（SetThreadExecutionState，参考项目 web_server.py:1221）
_ES_CONTINUOUS = 0x80000000
_ES_SYSTEM_REQUIRED = 0x00000001
_ES_DISPLAY_REQUIRED = 0x00000002


def _prevent_sleep():
    """阻止 Windows 自动锁屏/黑屏/睡眠（对齐参考项目 _prevent_sleep）。

    UIA 模拟操作要求窗口可见——屏幕睡眠/锁屏后控件搜索全部超时，正是
    「Find Control Timeout」四类失败的环境杀手。非 Windows 静默跳过；
    失败只 WARN（不阻断 init——桌面用户可能有意保持锁屏策略）。
    """
    try:
        import ctypes  # noqa: PLC0415

        ctypes.windll.kernel32.SetThreadExecutionState(
            _ES_CONTINUOUS | _ES_SYSTEM_REQUIRED | _ES_DISPLAY_REQUIRED
        )
        sidecar_log.log("INFO", "已阻止系统自动锁屏/睡眠（UIA 自动化要求窗口可见）")
    except AttributeError:
        pass  # 非 Windows（开发/CI）无 windll——静默跳过
    except Exception as e:  # noqa: BLE001 — 防睡眠失败不阻断主流程
        sidecar_log.log("WARN", f"设置防睡眠状态失败: {e}")


def _window_state(wx):
    """采集窗口态细节（IsOnline + 已开子窗口数）——三重校验/导航类失败的
    SidecarError 文案带上它，真机日志可直接裁决「主窗口最小化 / 被遮挡 /
    子窗口残留」哪一种。探测本身失败不炸（返回占位文本）。"""
    try:
        online = bool(wx.IsOnline())
    except Exception:  # noqa: BLE001 — 诊断探测不得引入新失败面
        online = "?"
    try:
        n = len(wx.GetAllSubWindow() or [])
    except Exception:  # noqa: BLE001
        n = "?"
    return f"主窗口={'正常' if online is True else '离线' if online is False else online}, 子窗口={n}"


def _main_window_hint(prefix, e):
    """主窗口导航类 UIA 失败的统一文案：原始异常 + 排查指引。

    wxautox4 是 UIA 模拟操作：主窗口最小化/被遮挡/目标不在会话列表时
    控件搜索超时（LookupError: Find Control Timeout）。参考项目文档
    「注意事项」：主窗口不得最小化、启动时只保留主窗口。"""
    return (
        f"{prefix}失败（微信主窗口不可操作——请确认：主窗口未最小化、"
        f"未被其他窗口遮挡、微信在前台登录态）: {type(e).__name__}: {e}"
    )


# ── msg 池工具 ──────────────────────────────────────────────


def _prune_expired(msg_pool, msg_pool_ts, ttl=60):
    """清理 60s 过期的 msg 对象（访问时惰性清理，无需后台线程）

    迭代走 list() 快照——监听回调线程（on_msg）会并发插入双 dict，
    直接迭代会在 GIL 切换点炸 RuntimeError: dictionary changed size
    during iteration（审查 I1）。
    """
    now = time.time()
    for k in [k for k, ts in list(msg_pool_ts.items()) if now - ts > ttl]:
        msg_pool.pop(k, None)
        msg_pool_ts.pop(k, None)


def _take_msg(params, msg_pool, msg_pool_ts):
    """按 msgId（兼容 msg_id 拼写）取池中条目；过期/不存在抛错

    池 value 是 (msg, chat_who) 元组（chat_who 入池时附带，供 quote
    降级等场景取回消息归属会话）；兼容旧形态裸 msg 对象。
    """
    _prune_expired(msg_pool, msg_pool_ts)
    mid = params.get("msgId") or params.get("msg_id") or ""
    entry = msg_pool.get(mid) if mid else None
    if entry is None:
        raise SidecarError("消息对象已过期(>60s)或不存在")
    if isinstance(entry, tuple):
        return entry
    return (entry, "")  # 旧形态裸 msg：无 chat_who 信息


# ── 滑动窗去重（UIA 重复回调吞除，2026-09-10 P1）─────────────────
# wxautox4 的 MESSAGE_HASH 挡库内重复采集；本层挡回调层重复（通知通道
# Lagged 重订阅、监听重注册等场景）。窗口 5s、容量 1024，命中即整条
# 丢弃（不 _pool_put、不 notify——二次 RPC 也不会重复触发）。

_DEDUP_WINDOW_MS = 5000      # 指纹保留时长（毫秒）
_DEDUP_CAPACITY = 1024       # 窗口容量上限（防长期运行内存膨胀）
_DEDUP_LOCK = threading.RLock()
# OrderedDict[指纹 hex] = 记录时刻（time.monotonic 秒）——有序供容量淘汰
_DEDUP_WINDOW: "OrderedDict[str, float]" = OrderedDict()


def _dedup_key(msg, chat_who):
    """内容指纹：会话+发送人+内容+类型 四元组 md5"""
    raw = f"{chat_who}|{getattr(msg, 'sender', '')}|{getattr(msg, 'content', '')}|{getattr(msg, 'type', '')}"
    return hashlib.md5(raw.encode("utf-8", errors="replace")).hexdigest()


def _seen_recently(key, now=None):
    """窗口内命中返 True 并刷新时刻；未命中记录返 False。

    now 参数注入（测试控时用）；默认 time.monotonic()。容量超限淘汰最旧
    （OrderedDict 首项）——被淘汰指纹可重新通过（保守方向：宁可放行勿误吞）。
    """
    ts = time.monotonic() if now is None else now
    with _DEDUP_LOCK:
        # 先淘汰过期项（从最旧侧弹出，遇到首个未过期即停）
        while _DEDUP_WINDOW:
            oldest_k, oldest_ts = next(iter(_DEDUP_WINDOW.items()))
            if ts - oldest_ts > _DEDUP_WINDOW_MS / 1000.0:
                _DEDUP_WINDOW.popitem(last=False)
            else:
                break
        if key in _DEDUP_WINDOW:
            del _DEDUP_WINDOW[key]      # 先删——重插移到最新侧,保住清扫的有序不变量
            _DEDUP_WINDOW[key] = ts
            return True
        _DEDUP_WINDOW[key] = ts
        if len(_DEDUP_WINDOW) > _DEDUP_CAPACITY:
            _DEDUP_WINDOW.popitem(last=False)
        return False


def _pool_put(msg_pool, msg_pool_ts, mid, msg, chat_who):
    """入池 (msg, chat_who) 元组（监听回调线程调用；dict 单键赋值原子）"""
    msg_pool[mid] = (msg, chat_who)
    msg_pool_ts[mid] = time.time()


def _raw_message(msg, chat, mid=""):
    """构造 RawMessage 形状（snake_case，对齐 Rust spec.rs）"""
    return {
        "msg_id": mid,
        "chat_who": str(getattr(chat, "who", "")),
        "chat_type": str(getattr(chat, "chat_type", "friend")),
        "attr": str(getattr(msg, "attr", "friend")),
        "msg_type": str(getattr(msg, "type", "text")),
        "sender": str(getattr(msg, "sender", "")),
        "content": str(getattr(msg, "content", "")),
        "is_at": bool(getattr(msg, "is_at", False)),
    }


def _loc_msg_str(m):
    """消息内容的安全字符串化（content 可能是非 str 类型）"""
    return str(getattr(m, "content", ""))


# ── 各方法实现 ──────────────────────────────────────────────


def _init(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.init：WxParam 全局参数 → WeChat(version) 中英文双兜底 → check_license
    → 监听引擎复位（StopListening → sleep 1 → StartListening）

    wx 实参为 None（sidecar.py 对 wx.init 特判惰性求值）；实例存 _instance。
    失败三态：failReason 取 "licensed"（未授权）或 "wechat_missing"
    （授权过但微信未开）；成功时无该字段。failDetail 携带 wechat_missing
    场景的异常细节（类型: 消息）——判定条件无法区分微信没开/未登录/版本
    超区间，细节是真机排障裁决真实根因的唯一线索。

    监听引擎复位（2026-09-10 补齐，对齐参考项目 init_wx_listeners
    wxbot_core.py:2462-2464）：sidecar 崩溃重启后 wxautox4 内部引擎残留
    脏态（旧回调/半注册会话），不复位则后续 AddListenChat 全挂（真机
    08:16「部分监听重注册失败」现场）。实例构造成功但引擎复位失败时降级
    WARN 不判 init 失败（窗口在，引擎可能只是时序问题）。
    """
    global _instance, _license_ok
    if mock:
        _instance = None
        # 未授权注入态：licensed=false + failReason=licensed（激活成功后由
        # wx.activate 翻转 _MOCK_LICENSE_STATE）
        _license_ok = not _MOCK_LICENSE_STATE
        if _MOCK_LICENSE_STATE:
            return {"licensed": False, "wxid": "", "nickname": "", "failReason": "licensed"}
        if _MOCK_WECHAT_MISSING_STATE:
            return {
                "licensed": True,
                "wxid": "",
                "nickname": "",
                "failReason": "wechat_missing",
                "failDetail": "模拟：微信客户端未打开",
            }
        return {"licensed": True, "wxid": "mock_wx", "nickname": "模拟设备"}
    # 真实导入（仅 Windows + 已 pip install wxautox4 时可达）
    _prevent_sleep()
    sidecar_log.log("INFO", "wx.init 开始（真实模式）")
    # 先清上一轮实例：重试 init 时构造失败路径不清会残留旧实例——本轮
    # wechat_missing 但 _instance 仍指旧窗口，引擎复位操作死句柄 + sidecar
    # 后续调用误用陈旧实例
    _instance = None
    try:
        with sidecar_log.guard_stdout():
            from wxautox4 import WeChat, WxParam  # noqa: PLC0415 — 延迟导入是硬要求（Linux 无此库）
            from wxautox4.utils.useful import check_license  # noqa: PLC0415

            # 全局参数（spec 附录 A 已验证配置）
            WxParam.MESSAGE_HASH = True
            WxParam.FORCE_MESSAGE_XBIAS = True
            WxParam.CHAT_WINDOW_SIZE = (1500, 6000)
            WxParam.DEFAULT_MESSAGE_YBIAS = 40

            _license_ok = bool(check_license())
            # 失败三态：未授权恒 licensed（可行动根因优先）；授权过但微信未开才是
            # wechat_missing——旧版两版本兜底都抛会整体 RPC 报错，现降为带原因返回
            fail_reason = None if _license_ok else "licensed"
            fail_detail = None
            try:
                _instance = WeChat(version="微信")
            except Exception as e:  # noqa: BLE001 — 国际版微信兜底（参考项目验证的双版本尝试）
                # 留痕铁律（2026-09-09 真机事故）：静默吞掉=真机零线索
                sidecar_log.log("WARN", f"wx.init WeChat(version=微信) 失败: {type(e).__name__}: {e}")
                try:
                    _instance = WeChat(version="WeChat")
                except Exception as e2:  # noqa: BLE001
                    sidecar_log.log("WARN", f"wx.init WeChat(version=WeChat) 失败: {type(e2).__name__}: {e2}")
                    if _license_ok:
                        fail_reason = "wechat_missing"
                        fail_detail = f"{type(e2).__name__}: {e2}"
            if _instance is not None:
                _reset_listen_engine(_instance)
            result = {
                "licensed": _license_ok,
                "wxid": getattr(_instance, "wxid", ""),
                "nickname": getattr(_instance, "nickname", ""),
            }
            if fail_reason:
                result["failReason"] = fail_reason
            if fail_detail:
                result["failDetail"] = fail_detail
    except SystemExit:
        # wxautox4 check_license 语境的 SystemExit=未授权设备裸退（2026-09-09
        # CI 六跑实证：横幅 print 到 stdout + raise SystemExit，except Exception
        # 接不住）——归一为既有三态，激活页正常引导，sidecar 不死
        _license_ok = False
        sidecar_log.log("WARN", "wx.init：wxautox4 未授权 SystemExit 裸退，归一 licensed 三态")
        return {"licensed": False, "wxid": "", "nickname": "", "failReason": "licensed"}
    detail_seg = f" detail={fail_detail}" if fail_detail else ""
    sidecar_log.log("INFO", f"wx.init 完成: licensed={_license_ok} failReason={fail_reason}{detail_seg}")
    return result


def _activate(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.activate：authenticate(code) 激活 → 立即 check_license() 回查确认

    激活状态由 wxautox4 自持久化（一机一码、跨重启有效），本方法无副作用存储。
    """
    code = (params.get("code") or "").strip()
    if not code:
        return {"ok": False, "message": "激活码不能为空"}
    if mock:
        global _MOCK_LICENSE_STATE
        ok = code == "MOCK-ACTIVATION"
        if ok:
            _MOCK_LICENSE_STATE = False  # 激活成功翻转（后续 wx.init 即 licensed）
        return {
            "ok": ok,
            "message": "模拟激活成功" if ok else "激活失败：激活码无效或已过期",
        }
    # 真实导入（仅 Windows + 已 pip install wxautox4 时可达）
    sidecar_log.log("INFO", "wx.activate 开始（真实模式）")
    try:
        with sidecar_log.guard_stdout():
            from wxautox4.utils.useful import authenticate  # noqa: PLC0415 — 延迟导入是硬要求

            if not authenticate(code):
                sidecar_log.log("WARN", "wx.activate 失败：激活码无效或已过期")
                return {"ok": False, "message": "激活失败：激活码无效或已过期"}
            # 回查确认：authenticate 过但状态未生效的极端情况不静默吞
            from wxautox4.utils.useful import check_license  # noqa: PLC0415

            if not check_license():
                sidecar_log.log("WARN", "wx.activate：码已接受但授权未生效")
                return {"ok": False, "message": "激活码已接受但授权状态未生效，请重启应用"}
    except SystemExit:
        # authenticate/check_license 语境的 SystemExit=码被拒/设备未授权裸退
        # （横幅+SystemExit，同 wx.init 实证）——归一为既有失败文案，可重试
        sidecar_log.log("WARN", "wx.activate：wxautox4 SystemExit 裸退，归一激活失败文案")
        return {"ok": False, "message": "激活失败：激活码无效或已过期"}
    sidecar_log.log("INFO", "wx.activate 成功")
    return {"ok": True, "message": "激活成功"}


def _get_my_info(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.get_my_info：GetMyInfo() dict key 'id'（附录 A）+ wx.nickname + IsOnline

    licensed 取 _license_ok（wx.init 时 check_license 的结果；mock 路径
    _init 已置 True，语义不变）——此前硬编码 True 掩盖了授权失效。
    """
    info = wx.GetMyInfo() or {}
    return {
        "licensed": _license_ok,
        "wxid": str(info.get("id", "")),
        "nickname": getattr(wx, "nickname", ""),
        "online": wx.IsOnline(),
    }


def _is_online(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.is_online：IsOnline() bool（附录 A）"""
    return {"online": bool(wx.IsOnline())}


def _msg_send(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.SendMsg(msg=..., who=...)——关键字名必须是 msg/who（附录 A）

    at 场景：wx.SendMsg 不带 at → 定位子窗口 chat.SendMsg(msg=..., at=...)。
    """
    p = params
    if p.get("at"):
        chat = wx.GetSubWindow(nickname=p["who"])
        if chat is None:
            wx.ChatWith(who=p["who"])
            time.sleep(0.5)
            chat = wx.GetSubWindow(nickname=p["who"])
        if chat is None:
            raise SidecarError(f"窗口未找到: {p['who']}")
        r = chat.SendMsg(msg=p["text"], at=p["at"])
    else:
        r = wx.SendMsg(msg=p["text"], who=p["who"])
    if not was_send_success(r):
        raise SidecarError(send_error_message(r))
    return {"ok": True}


def _file_send(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.SendFiles(who=..., filepath=...)——单数 filepath（附录 A）"""
    r = wx.SendFiles(who=params["who"], filepath=params["filepath"])
    if not was_send_success(r):
        raise SidecarError(send_error_message(r))
    return {"ok": True}


def _msg_quote(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """引用回复：msgId 池直取（spec §3.2 签名）或 ChatWith 定位（回调外路径）

    quote 失败降级普通发送（参考项目 3349-3354）——降级必须真的发出去。
    """
    p = params
    chat_who = ""
    msg = None
    if p.get("msgId") or p.get("msg_id"):
        msg, chat_who = _take_msg(p, msg_pool, msg_pool_ts)
    else:
        msg = _locate_msg(wx, p["who"], p.get("quoteContent", ""))
    if not msg:
        raise SidecarError("未定位到要引用的消息")
    try:
        r = msg.quote(p["text"], p.get("at")) if p.get("at") else msg.quote(p["text"])
        if was_send_success(r):
            return {"ok": True}
    except Exception as e:  # noqa: BLE001 — UIA 异常与失败形态同样降级
        sidecar_log.log("WARN", f"quote 失败({e})，降级普通发送")
    # 降级：普通发送（wx.SendMsg 全局路径，附录 A）
    # 降级目标：显式 who 优先；msgId 路径（spec §3.2 签名无 who）取入池时
    # 附带的 chat_who；两者皆无则明确报错而非 KeyError（审查 C1）
    who = p.get("who") or chat_who
    if not who:
        raise SidecarError("quote 失败且无法降级(缺少 who)")
    r2 = wx.SendMsg(msg=p["text"], who=who)
    if not was_send_success(r2):
        raise SidecarError(send_error_message(r2))
    return {"ok": True, "degraded": "quote 失败已降级普通发送"}


def _msg_forward(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """转发：msgId 池直取或定位消息后 msg.forward(target, note)

    forward 依赖主窗口上下文（AddListenChat 后失效）——池直取路径先
    ChatWith 切回主窗口再 forward，顺序在方法内保证。
    """
    p = params
    if p.get("msgId") or p.get("msg_id"):
        msg, _chat_who = _take_msg(p, msg_pool, msg_pool_ts)
        # 切回目标窗口上下文再转发（forward 的上下文约束在方法内保证）
        wx.ChatWith(who=p["target"])
        time.sleep(1)
        r = msg.forward(p["target"], p.get("note"))
    else:
        msg = _locate_msg(wx, p["sourceWho"], p.get("match", ""))
        if not msg:
            raise SidecarError("未定位到要转发的消息")
        r = msg.forward(p["target"], p.get("note"))
    if not was_send_success(r):
        raise SidecarError(send_error_message(r))
    return {"ok": True}


def _locate_msg(wx, who, content_match):
    """打开窗口并按内容片段定位消息对象（回调上下文外的 msg 获取路径）"""
    wx.ChatWith(who=who)
    time.sleep(1)
    for m in reversed(wx.GetAllMessage()):
        if content_match and content_match in _loc_msg_str(m):
            return m
    return None


def _chat_search(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """chat.search：ChatWith 定位 + GetSubWindow 校验（spec §2.3 待真机验证项）"""
    kw = params["keyword"]
    wx.ChatWith(who=kw)
    time.sleep(1)
    chat = wx.GetSubWindow(nickname=kw)
    return {"found": bool(chat), "who": str(getattr(chat, "who", "")) if chat else ""}


def _chat_history(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """chat.history：ChatWith + GetAllMessage 取尾部 n 条（替代幻觉 API GetHistoryMessage）

    消息元素是 7 字段 RawMessage 形状（snake_case，对齐 Rust spec.rs）。
    """
    who = params["who"]
    wx.ChatWith(who=who)
    time.sleep(1)
    msgs = wx.GetAllMessage() or []
    tail = msgs[-params.get("n", 20):] if params.get("n", 20) > 0 else []
    chat_type = "group" if any(
        str(getattr(c, "chat_type", "")) == "group" for c in (wx.GetSubWindow(nickname=who),) if c
    ) else "friend"
    return {"messages": [
        {
            "msg_id": str(getattr(m, "id", "")),
            "chat_who": who,
            "chat_type": chat_type,
            "attr": str(getattr(m, "attr", "friend")),
            "msg_type": str(getattr(m, "type", "text")),
            "sender": str(getattr(m, "sender", "")),
            "content": _loc_msg_str(m),
        }
        for m in tail
    ]}


def _listen_add(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """listen.add：ChatWith 预热 + AddListenChat(nickname=..., callback=...) + 三重校验重试

    三重校验（spec §2.3）：①返回值判定（dict 带错误信息=失败）
    ②GetAllSubWindow 集合比对 ③GetSubWindow 单个校验；每轮 AddListenChat
    前置 sleep 0.5s；三重全失败才报错。

    2026-09-10 真机修复（「测试群1」三重校验全挂）：
    - AddListenChat 前先 ChatWith(who=nickname) 预热——wxautox4 在主窗口
      会话列表里搜目标会话，目标不在近期会话（新群/久未活跃）时直接失败；
    - 每轮 AddListenChat / 校验阶段的 UIA 异常捕获入错误列表（旧实现裸穿
      -32603 LookupError，与 -32000 文案混杂，前端无法引导）；
    - 耗尽的 SidecarError 携带窗口态（IsOnline + 子窗口数），真机日志
      可直接裁决主窗口最小化/遮挡/子窗口残留。
    """
    import uuid  # noqa: PLC0415 — 回调热路径外导入，避免模块级依赖

    nickname = params["nickname"]

    def on_msg(msg, chat):
        """监听回调：callback(msg, chat) 签名（附录 A）；msg 入池 + 发通知

        入池形态 (msg, chat_who)——chat_who 供 quote 降级等二次 RPC 取回
        消息归属会话（msg 对象本身无 who 属性，附录 A）。
        """
        try:
            chat_who = str(getattr(chat, "who", ""))
            # 原生 id 优先（确定性幂等键——Server 可按 msg_id 去重；
            # 同一消息重复回调命中同一池键，二次 RPC 不重复触发）。
            # 旧版 wxautox4 msg 无 id 属性 → 退回随机 uuid（不劣于现状）。
            native_id = str(getattr(msg, "id", "")).strip()
            # 去重键 id 优先：UIA 重复回调携带相同原生 id——按 id 吞；
            # 旧库无 id 才退回内容指纹（此时同内容 5s 重复是已知接受的误吞面）。
            # 判定用 native_id 而非 mid：mid 有 uuid 兜底恒非空，
            # 按 mid 判会让旧库永远走 id 分支，指纹兜底成死代码
            dedup_key = f"id:{chat_who}:{native_id}" if native_id else _dedup_key(msg, chat_who)
            if _seen_recently(dedup_key):
                return  # 窗口内重复回调：整条丢弃（不入池不通知）
            mid = native_id or uuid.uuid4().hex[:12]
            _pool_put(msg_pool, msg_pool_ts, mid, msg, chat_who)
            notify("message.received", _raw_message(msg, chat, mid))
        except Exception as e:  # noqa: BLE001 — 回调内异常上抛会杀 wxautox4 监听线程
            sidecar_log.log("ERROR", f"message.received 回调异常: {e}")

    errors = []
    for _attempt in range(3):
        time.sleep(0.5)
        # 预热：把目标会话顶入主窗口会话列表（ChatWith 定位是参考项目所有
        # UIA 操作的统一前置）；失败不重试预热——直接进 AddListenChat 三重
        # 校验兜底（预热失败大概率与 AddListenChat 同因，避免双倍超时等待）
        try:
            wx.ChatWith(who=nickname)
        except Exception as e:  # noqa: BLE001 — UIA 异常入列，不阻断校验流程
            errors.append(f"ChatWith 预热失败: {type(e).__name__}: {e}")
        try:
            r = wx.AddListenChat(nickname=nickname, callback=on_msg)
        except Exception as e:  # noqa: BLE001 — UIA 异常入列（不再裸穿 -32603）
            errors.append(f"AddListenChat 异常: {type(e).__name__}: {e}")
            continue
        # 校验①：返回 dict 带错误信息 → 本轮失败，重试
        if isinstance(r, dict) and r.get("message"):
            errors.append(f"AddListenChat 返回错误: {r['message']}")
            continue
        # 校验②：子窗口集合比对
        try:
            names = [str(getattr(c, "who", "")) for c in (wx.GetAllSubWindow() or [])]
        except Exception as e:  # noqa: BLE001
            errors.append(f"GetAllSubWindow 异常: {type(e).__name__}: {e}")
            continue
        if nickname in names:
            return {"ok": True, "verified": True}
        # 校验③：单窗口校验
        try:
            if wx.GetSubWindow(nickname=nickname):
                return {"ok": True, "verified": True}
        except Exception as e:  # noqa: BLE001
            errors.append(f"GetSubWindow 异常: {type(e).__name__}: {e}")
    detail = "; ".join(errors[-3:])  # 尾部 3 条防文案爆炸
    state = _window_state(wx)
    raise SidecarError(
        f"监听注册失败(三重校验未通过): {nickname}（{state}）"
        f"——请确认微信主窗口未最小化/未被遮挡、目标会话名与微信一致。{detail}"
    )


def _listen_remove(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """listen.remove：RemoveListenChat + 0.2s 后 GetAllSubWindow 校验消失"""
    wx.RemoveListenChat(nickname=params["nickname"])
    time.sleep(0.2)
    names = [str(getattr(c, "who", "")) for c in (wx.GetAllSubWindow() or [])]
    return {"ok": True, "stillExists": params["nickname"] in names}


def _listen_list(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """listen.list：GetAllSubWindow() 读 .who（附录 A）"""
    return {"chats": [str(getattr(c, "who", "")) for c in (wx.GetAllSubWindow() or [])]}


def _new_requests(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """friends.new_requests：GetNewFriends(acceptable=True) 读 .name（附录 A）

    UIA 异常包装（2026-09-10 真机）：GetNewFriends 要在主窗口导航到
    通讯录→新的朋友页，主窗口最小化/被遮挡时控件搜索超时抛 LookupError
    ——包装为带排查指引的业务错误（-32000），而非裸 -32603。

    页面复位（对齐参考项目 Pass_New_Friends wxbot_core.py:4467 收尾）：
    读完 SwitchToChat() 切回聊天页——不切回则主窗口停在通讯录页，后续
    AddListenChat 在会话列表搜目标必败（好友泵每 60~300s 污染一次页面态，
    与真机 08:15 加监听失败的时序吻合）。收尾失败只 WARN（列表已取到，
    残留态由下一轮轮询自愈）。
    """
    try:
        friends = wx.GetNewFriends(acceptable=True) or []
    except Exception as e:  # noqa: BLE001 — 主窗口导航类 UIA 失败统一包装
        _restore_chat_page_quietly(wx)
        raise SidecarError(_main_window_hint("获取新的好友", e)) from e
    result = {"requests": [{"name": str(getattr(f, "name", ""))} for f in friends]}
    _restore_chat_page_quietly(wx)
    return result


def _restore_chat_page_quietly(wx):
    """主窗口切回聊天页（SwitchToChat）——失败只 WARN 不炸结果路径。

    即便 GetNewFriends 失败也尝试复位（导航可能已完成了一半，停在
    通讯录页会污染后续操作）。
    """
    try:
        wx.SwitchToChat()
    except Exception as e:  # noqa: BLE001 — 收尾失败降级
        sidecar_log.log("WARN", f"切回聊天页失败(主窗口可能停在通讯录页): {type(e).__name__}: {e}")


def _friend_accept(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """friend.accept：accept(remark, tags) + GBK 32 截断 + sleep 5s + SwitchToChat 收尾

    （附录 A「好友」节：备注 GBK 32 截断；accept 后 sleep 5s；SwitchToChat 收尾）
    GetNewFriends 的 UIA 异常包装同 _new_requests（2026-09-10 真机修复）。
    """
    name = params["name"]
    remark = params.get("remark", "")
    if remark:
        # GBK 32 字节截断（wxautox4 备注列超长会失败）；encode 侧 errors=ignore
        # 防 emoji 等非 GBK 字符直接 UnicodeEncodeError，decode 侧防截在多字节中间
        remark = remark.encode("gbk", errors="ignore")[:32].decode("gbk", errors="ignore")
    try:
        friends = wx.GetNewFriends(acceptable=True) or []
    except Exception as e:  # noqa: BLE001 — 主窗口导航类 UIA 失败统一包装
        raise SidecarError(_main_window_hint("获取新的好友", e)) from e
    for f in friends:
        if str(getattr(f, "name", "")) == name:
            f.accept(remark=remark or None, tags=params.get("tags") or None)
            time.sleep(5)
            wx.SwitchToChat()
            return {"ok": True}
    raise SidecarError(f"未找到好友申请: {name}")


def _moments_get(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """moments.get：Moments() → GetMoments() → Close()，步骤间 1~5s 拟人延时

    UIA 异常包装（2026-09-10 真机）：朋友圈窗口开在主窗口之上，主窗口
    最小化/被遮挡时 Moments()/GetMoments() 控件搜索超时——包装为带排查
    指引的业务错误；Close 兜底不丢（窗口泄漏会卡后续操作）。
    """
    import random  # noqa: PLC0415

    try:
        pyq = wx.Moments()
    except Exception as e:  # noqa: BLE001 — 主窗口导航类 UIA 失败统一包装
        raise SidecarError(_main_window_hint("打开朋友圈", e)) from e
    if not pyq:
        raise SidecarError("朋友圈窗口打开失败")
    try:
        time.sleep(random.uniform(1, 5))
        try:
            moments = pyq.GetMoments() or []
        except Exception as e:  # noqa: BLE001
            raise SidecarError(_main_window_hint("读取朋友圈", e)) from e
        items = [
            {"content": str(getattr(m, "content", "")), "sender": str(getattr(m, "sender", ""))}
            for m in moments[: params.get("count", 10)]
        ]
        time.sleep(random.uniform(1, 5))
        return {"moments": items}
    finally:
        try:
            pyq.Close()  # 无论成功失败都关窗（UIA 窗口泄漏会卡后续操作）
        except Exception as e:  # noqa: BLE001 — 关窗失败留痕不炸结果路径
            sidecar_log.log("WARN", f"朋友圈 Close 失败(窗口可能残留): {e}")


def _moments_publish(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """moments.publish：Publish(text, images, privacy)——privacy 中文值（附录 A）

    privacy：{} 公开 / {'privacy': '白名单'|'黑名单', 'tags': [...]}；images 空→None。
    UIA 异常包装同 moments.get（2026-09-10 真机四类失败修复）。
    """
    import random  # noqa: PLC0415

    try:
        pyq = wx.Moments()
    except Exception as e:  # noqa: BLE001 — 主窗口导航类 UIA 失败统一包装
        raise SidecarError(_main_window_hint("打开朋友圈", e)) from e
    if not pyq:
        raise SidecarError("朋友圈窗口打开失败")
    try:
        time.sleep(random.uniform(2, 5))
        privacy = params.get("privacy", "public")
        if privacy == "whitelist":
            cfg = {"privacy": "白名单", "tags": params.get("tags", [])}
        elif privacy == "blacklist":
            cfg = {"privacy": "黑名单", "tags": params.get("tags", [])}
        else:
            cfg = {}
        images = params.get("images") or None
        try:
            pyq.Publish(params.get("text", ""), images, cfg)
        except Exception as e:  # noqa: BLE001
            raise SidecarError(_main_window_hint("发布朋友圈", e)) from e
        time.sleep(random.uniform(2, 5))
        return {"ok": True}
    finally:
        try:
            pyq.Close()
        except Exception as e:  # noqa: BLE001 — 关窗失败留痕不炸结果路径
            sidecar_log.log("WARN", f"朋友圈 Close 失败(窗口可能残留): {e}")


def _media_download(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """media.download：msg.download() → 本地路径；falsy 即失败（屏幕缩放提示）"""
    msg, _ = _take_msg(params, msg_pool, msg_pool_ts)
    path = msg.download()
    if not path:
        raise SidecarError("图片下载失败（建议将 Windows 屏幕缩放设为 100%）")
    return {"path": str(path)}


def _voice_to_text(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """media→voice.to_text：msg.to_text()；失败降级返回 failed 标记（不中断）"""
    msg, _ = _take_msg(params, msg_pool, msg_pool_ts)
    try:
        return {"text": str(msg.to_text())}
    except Exception as e:  # noqa: BLE001 — 转写失败降级（spec §2.4：WARNING 不中断）
        sidecar_log.log("WARN", f"语音转写失败: {e}")
        return {"text": "", "failed": True}


def _util_sleep(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """util.sleep：调试/节流用（默认 500ms）"""
    time.sleep(params.get("ms", 500) / 1000)
    return {"ok": True}


# ── MOCK 模式（Linux 开发/CI 全假数据）────────────────────


def _mock_dispatch(method, params):
    """MOCK 表：除 wx.init / wx.activate 外全部 19 方法

    （wx.init / wx.activate 需操作授权态，在 dispatch 的 mock 短路先行处理）
    """
    table = {
        "wx.get_my_info": {"licensed": True, "wxid": "mock_wx", "nickname": "模拟设备", "online": True},
        "wx.is_online": {"online": True},
        "msg.send": {"ok": True},
        "file.send": {"ok": True},
        "msg.quote": {"ok": True},
        "msg.forward": {"ok": True},
        "chat.search": {"found": True, "who": params.get("keyword", "")},
        "chat.history": {"messages": [{
            "msg_id": "mock_m1", "chat_who": params.get("who", ""), "chat_type": "friend",
            "attr": "friend", "msg_type": "text", "sender": "模拟", "content": "模拟消息",
        }]},
        "listen.add": {"ok": True, "verified": True},
        "listen.remove": {"ok": True, "stillExists": False},
        "listen.list": {"chats": ["模拟窗口A", "模拟窗口B"]},
        "friends.new_requests": {"requests": [{"name": "模拟申请人"}]},
        "friend.accept": {"ok": True},
        "moments.get": {"moments": [{"content": "模拟朋友圈", "sender": "模拟好友"}]},
        "moments.publish": {"ok": True},
        "media.download": {"path": "C:/mock/path.png"},
        "voice.to_text": {"text": "模拟语音转写"},
        "util.sleep": {"ok": True},
    }
    entry = table.get(method)
    if entry is None:
        raise SidecarError(f"MOCK 未覆盖: {method}")
    return entry() if callable(entry) else entry


# ── 统一入口（七参签名，sidecar.py 调用；参数直传 wx）────────


_METHODS = {
    "wx.init": _init,
    "wx.activate": _activate,
    "wx.get_my_info": _get_my_info,
    "wx.is_online": _is_online,
    "msg.send": _msg_send,
    "file.send": _file_send,
    "msg.quote": _msg_quote,
    "msg.forward": _msg_forward,
    "chat.search": _chat_search,
    "chat.history": _chat_history,
    "listen.add": _listen_add,
    "listen.remove": _listen_remove,
    "listen.list": _listen_list,
    "friends.new_requests": _new_requests,
    "friend.accept": _friend_accept,
    "moments.get": _moments_get,
    "moments.publish": _moments_publish,
    "media.download": _media_download,
    "voice.to_text": _voice_to_text,
    "util.sleep": _util_sleep,
}


def dispatch(method, params, wx, msg_pool, msg_pool_ts, notify, mock):
    """统一入口：返回 result dict；业务错误抛 SidecarError（→ -32000）

    MOCK 短路优先于方法路由（wx.init / wx.activate 需操作授权态，
    特判直达真实函数的 mock 分支）；未知方法统一 SidecarError。
    """
    if mock and method == "wx.activate":
        return _activate(params, wx, msg_pool, msg_pool_ts, notify, mock)
    if mock and method != "wx.init":
        return _mock_dispatch(method, params)
    fn = _METHODS.get(method)
    if fn is None:
        raise SidecarError(f"未知或未实现的方法: {method}")
    return fn(params, wx, msg_pool, msg_pool_ts, notify, mock)


if __name__ == "__main__":
    # 独立自测：python methods.py 走一遍 mock dispatch
    print(dispatch("wx.init", {}, None, {}, {}, lambda *_: None, True))
    print(dispatch("wx.get_my_info", {}, None, {}, {}, lambda *_: None, True))
