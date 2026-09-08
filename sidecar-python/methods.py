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
import sys
import time


class SidecarError(Exception):
    """业务错误 → JSON-RPC -32000（sidecar.py 按本类捕获）"""


# wxautox4.WeChat 实例（仅 _init 成功后非 None；sidecar.py 从这里拉取）
_instance = None
# 授权状态（_init 真实分支写入；供上层判断是否进就绪态）
_license_ok = False


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
    }


def _loc_msg_str(m):
    """消息内容的安全字符串化（content 可能是非 str 类型）"""
    return str(getattr(m, "content", ""))


# ── 各方法实现 ──────────────────────────────────────────────


def _init(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.init：WxParam 全局参数 → WeChat(version) 中英文双兜底 → check_license

    wx 实参为 None（sidecar.py 对 wx.init 特判惰性求值）；实例存 _instance。
    失败三态：failReason 取 "licensed"（未授权）或 "wechat_missing"
    （授权过但微信未开）；成功时无该字段。
    """
    global _instance, _license_ok
    if mock:
        _instance, _license_ok = None, True
        return {"licensed": True, "wxid": "mock_wx", "nickname": "模拟设备"}
    # 真实导入（仅 Windows + 已 pip install wxautox4 时可达）
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
    try:
        _instance = WeChat(version="微信")
    except Exception:  # noqa: BLE001 — 国际版微信兜底（参考项目验证的双版本尝试）
        try:
            _instance = WeChat(version="WeChat")
        except Exception:  # noqa: BLE001
            if _license_ok:
                fail_reason = "wechat_missing"
    result = {
        "licensed": _license_ok,
        "wxid": getattr(_instance, "wxid", ""),
        "nickname": getattr(_instance, "nickname", ""),
    }
    if fail_reason:
        result["failReason"] = fail_reason
    return result


def _activate(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.activate：authenticate(code) 激活 → 立即 check_license() 回查确认

    激活状态由 wxautox4 自持久化（一机一码、跨重启有效），本方法无副作用存储。
    """
    code = (params.get("code") or "").strip()
    if not code:
        return {"ok": False, "message": "激活码不能为空"}
    if mock:
        # mock 分支：MOCK-ACTIVATION 成功、其余失败（Linux CI 全链路依赖）
        ok = code == "MOCK-ACTIVATION"
        return {
            "ok": ok,
            "message": "模拟激活成功" if ok else "激活失败：激活码无效或已过期",
        }
    # 真实导入（仅 Windows + 已 pip install wxautox4 时可达）
    from wxautox4.utils.useful import authenticate  # noqa: PLC0415 — 延迟导入是硬要求

    if not authenticate(code):
        return {"ok": False, "message": "激活失败：激活码无效或已过期"}
    # 回查确认：authenticate 过但状态未生效的极端情况不静默吞
    from wxautox4.utils.useful import check_license  # noqa: PLC0415

    if not check_license():
        return {"ok": False, "message": "激活码已接受但授权状态未生效，请重启应用"}
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
        print(f"[sidecar] quote 失败({e})，降级普通发送", file=sys.stderr)
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


def _chat_open(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """chat.open：ChatWith(who=...) 关键字 who + 1s 等待窗口就绪"""
    wx.ChatWith(who=params["who"])
    time.sleep(1)
    return {"ok": True}


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
    """listen.add：AddListenChat(nickname=..., callback=...) + 三重校验重试

    三重校验（spec §2.3）：①返回值判定（dict 带错误信息=失败）
    ②GetAllSubWindow 集合比对 ③GetSubWindow 单个校验；每轮 AddListenChat
    前置 sleep 0.5s；三重全失败才报错。
    """
    import uuid  # noqa: PLC0415 — 回调热路径外导入，避免模块级依赖

    nickname = params["nickname"]

    def on_msg(msg, chat):
        """监听回调：callback(msg, chat) 签名（附录 A）；msg 入池 + 发通知

        入池形态 (msg, chat_who)——chat_who 供 quote 降级等二次 RPC 取回
        消息归属会话（msg 对象本身无 who 属性，附录 A）。
        """
        try:
            mid = uuid.uuid4().hex[:12]
            _pool_put(msg_pool, msg_pool_ts, mid, msg, str(getattr(chat, "who", "")))
            notify("message.received", _raw_message(msg, chat, mid))
        except Exception as e:  # noqa: BLE001 — 回调内异常上抛会杀 wxautox4 监听线程
            print(f"[sidecar] message.received 回调异常: {e}", file=sys.stderr)

    for _attempt in range(3):
        time.sleep(0.5)
        r = wx.AddListenChat(nickname=nickname, callback=on_msg)
        # 校验①：返回 dict 带错误信息 → 本轮失败，重试
        if isinstance(r, dict) and r.get("message"):
            continue
        # 校验②：子窗口集合比对
        names = [str(getattr(c, "who", "")) for c in (wx.GetAllSubWindow() or [])]
        if nickname in names:
            return {"ok": True, "verified": True}
        # 校验③：单窗口校验
        if wx.GetSubWindow(nickname=nickname):
            return {"ok": True, "verified": True}
    raise SidecarError(f"监听注册失败(三重校验未通过): {nickname}")


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
    """friends.new_requests：GetNewFriends(acceptable=True) 读 .name（附录 A）"""
    friends = wx.GetNewFriends(acceptable=True) or []
    return {"requests": [{"name": str(getattr(f, "name", ""))} for f in friends]}


def _friend_accept(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """friend.accept：accept(remark, tags) + GBK 32 截断 + sleep 5s + SwitchToChat 收尾

    （附录 A「好友」节：备注 GBK 32 截断；accept 后 sleep 5s；SwitchToChat 收尾）
    """
    name = params["name"]
    remark = params.get("remark", "")
    if remark:
        # GBK 32 字节截断（wxautox4 备注列超长会失败）；encode 侧 errors=ignore
        # 防 emoji 等非 GBK 字符直接 UnicodeEncodeError，decode 侧防截在多字节中间
        remark = remark.encode("gbk", errors="ignore")[:32].decode("gbk", errors="ignore")
    friends = wx.GetNewFriends(acceptable=True) or []
    for f in friends:
        if str(getattr(f, "name", "")) == name:
            f.accept(remark=remark or None, tags=params.get("tags") or None)
            time.sleep(5)
            wx.SwitchToChat()
            return {"ok": True}
    raise SidecarError(f"未找到好友申请: {name}")


def _moments_get(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """moments.get：Moments() → GetMoments() → Close()，步骤间 1~5s 拟人延时"""
    import random  # noqa: PLC0415

    pyq = wx.Moments()
    if not pyq:
        raise SidecarError("朋友圈窗口打开失败")
    try:
        time.sleep(random.uniform(1, 5))
        moments = pyq.GetMoments() or []
        items = [
            {"content": str(getattr(m, "content", "")), "sender": str(getattr(m, "sender", ""))}
            for m in moments[: params.get("count", 10)]
        ]
        time.sleep(random.uniform(1, 5))
        return {"moments": items}
    finally:
        pyq.Close()  # 无论成功失败都关窗（UIA 窗口泄漏会卡后续操作）


def _moments_publish(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """moments.publish：Publish(text, images, privacy)——privacy 中文值（附录 A）

    privacy：{} 公开 / {'privacy': '白名单'|'黑名单', 'tags': [...]}；images 空→None。
    """
    import random  # noqa: PLC0415

    pyq = wx.Moments()
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
        pyq.Publish(params.get("text", ""), images, cfg)
        time.sleep(random.uniform(2, 5))
        return {"ok": True}
    finally:
        pyq.Close()


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
        print(f"[sidecar] 语音转写失败: {e}", file=sys.stderr)
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
        "chat.open": {"ok": True},
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
    "chat.open": _chat_open,
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
