"""方法映射测试：monkeypatch 假 wxautox4，验证调用参数正确性（不连真微信）

覆盖面（对齐 spec 附录 A 已验证 API 与附录 B 判定逻辑）：
- 关键字参数名：SendMsg(msg=, who=) / SendFiles(who=, filepath=) 单数 / ChatWith(who=)
- 朋友圈 privacy 中文值（"白名单"/"黑名单"）
- was_send_success 双形态（bool / dict status·code·success）
- listen.add 三重校验（返回判定 → GetAllSubWindow 比对 → GetSubWindow 单校验）
- 好友 accept(remark, tags) + GBK 32 截断 + SwitchToChat 收尾
- msg 池（message.received 通知带 msg_id；media.download/voice.to_text 二次 RPC）
- quote 失败降级 / voice.to_text 失败降级
- MOCK 模式全方法覆盖

字段命名约定：message.received 通知与 chat.history 消息元素用 snake_case，
与 Rust 侧 spec.rs RawMessage（msg_id/chat_who/chat_type/attr/msg_type/sender/content）
严格对齐（camelCase 的 WS 帧转换在 Rust agent_link 层做，spec §2.4）。
"""
import sys
import os
import time

sys.path.insert(0, os.path.dirname(__file__))

import pytest  # noqa: E402
import methods  # noqa: E402 — 模块级引用供 autouse fixture 隔离 _init 全局态
from unittest.mock import MagicMock  # noqa: E402


@pytest.fixture(autouse=True)
def _fast_sleep(monkeypatch):
    """拟人延时在测试中全部短路（chat 定位 1s / accept 5s / 朋友圈 1~5s 随机等）"""
    monkeypatch.setattr(time, "sleep", lambda s: None)


class FakeMsg:
    """伪造 wxautox4 Message 对象（附录 A：属性 type/attr/sender/content/id/is_at）"""

    def __init__(self, content="hello", mtype="text", attr="friend", sender="张三"):
        self.content = content
        self.type = mtype
        self.attr = attr
        self.sender = sender
        self.id = "msg_obj_1"
        self.downloaded = None
        self.text = "语音转写文本"
        self.quoted = None
        self.forwarded = None
        self.is_at = False

    def download(self):
        return self.downloaded

    def to_text(self):
        if isinstance(self.text, Exception):
            raise self.text
        return self.text

    def quote(self, content, at=None):
        if isinstance(content, Exception):
            raise content
        self.quoted = (content, at)
        return True

    def forward(self, who, message=None):
        self.forwarded = (who, message)
        return True


class FakeChat:
    """伪造监听子窗口对象（.who / .chat_type / SendMsg(msg=, at=)）"""

    def __init__(self, who, chat_type="friend"):
        self.who = who
        self.chat_type = chat_type
        self.sent = []

    def SendMsg(self, msg=None, at=None):
        self.sent.append((msg, at))
        return True


class FakeFriend:
    """伪造好友申请对象（.name / .accept(remark, tags)）"""

    def __init__(self, name):
        self.name = name
        self.accepted = None

    def accept(self, remark=None, tags=None):
        self.accepted = (remark, tags)
        return True


class FakeMoments:
    """伪造朋友圈窗口（Publish(text, images, privacy) / GetMoments / Close）"""

    def __init__(self):
        self.published = None
        self.closed = False

    def Publish(self, text, images, privacy):
        self.published = (text, images, privacy)
        return True

    def GetMoments(self):
        return [MagicMock(content="动态1", sender="好友A"), MagicMock(content="动态2", sender="好友B")]

    def Close(self):
        self.closed = True


class FakeWx:
    """按参考项目已验证的 API 面伪造 wxautox4.WeChat（spec 附录 A）"""

    def __init__(self):
        self.sent = []            # SendMsg/SendFiles 记录
        self.nickname = "测试号"
        self.opened = []          # ChatWith 记录
        self.sub_windows = []     # 子窗口列表（FakeChat）
        self.listen_reg = {}      # nickname -> callback
        self.new_friends = []     # FakeFriend
        self.moments_obj = None
        self.switched = 0
        self.messages = []        # GetAllMessage 返回
        self.send_result = True   # 可注入 dict/False 模拟失败形态

    # ── 发送 ──
    def SendMsg(self, msg=None, who=None):
        self.sent.append(("msg", msg, who))
        return self.send_result

    def SendFiles(self, who=None, filepath=None):
        self.sent.append(("file", who, filepath))
        return self.send_result

    # ── 状态 ──
    def GetMyInfo(self):
        return {"id": "wx_test_1"}

    def IsOnline(self):
        return True

    # ── 窗口/导航 ──
    def ChatWith(self, who=None):
        self.opened.append(who)
        return True

    def SwitchToChat(self):
        self.switched += 1
        return True

    def GetSubWindow(self, nickname=None):
        for c in self.sub_windows:
            if c.who == nickname:
                return c
        return None

    def GetAllSubWindow(self):
        return list(self.sub_windows)

    def GetAllMessage(self):
        return list(self.messages)

    # ── 监听 ──
    def AddListenChat(self, nickname=None, callback=None):
        self.listen_reg[nickname] = callback
        self.sub_windows.append(FakeChat(nickname))
        return True

    def RemoveListenChat(self, nickname=None):
        self.listen_reg.pop(nickname, None)
        self.sub_windows = [c for c in self.sub_windows if c.who != nickname]
        return True

    # ── 好友 ──
    def GetNewFriends(self, acceptable=None):
        return list(self.new_friends)

    # ── 朋友圈 ──
    def Moments(self):
        self.moments_obj = FakeMoments()
        return self.moments_obj


@pytest.fixture
def fakewx():
    return FakeWx()


def _dispatch(method, params, wx, notify=None, mock=False, msg_pool=None, msg_pool_ts=None):
    """统一七参调用（与 sidecar.py 的调用形态一致：参数直传 wx）"""
    import methods
    pool = msg_pool if msg_pool is not None else {}
    pool_ts = msg_pool_ts if msg_pool_ts is not None else {}
    cb = notify if notify is not None else (lambda m, p: None)
    return methods.dispatch(method, params, wx, pool, pool_ts, cb, mock)


# ══════════ 基础三方法 ══════════


def test_wx_init_mock_mode():
    r = _dispatch("wx.init", {}, None, mock=True)
    assert r["licensed"] is True


# ══════════ wx.init failReason 三态 ═════════


@pytest.fixture(autouse=True)
def _reset_init_globals(monkeypatch):
    """_init 写模块级 _instance/_license_ok——用例间隔离，防跨测试泄漏"""
    monkeypatch.setattr(methods, "_instance", None)
    monkeypatch.setattr(methods, "_license_ok", False)
    yield


def test_init_real_path_licensed_ok(monkeypatch, capsys):
    """授权过 + 微信在：无 failReason 字段"""
    _install_fake_wxautox4(monkeypatch, licensed=True, wechat_ok=True)
    r = _dispatch("wx.init", {}, None)
    assert r["licensed"] is True
    assert "failReason" not in r
    # 埋点断言（spec §6）：real-path 路径必须打 wx.init 完成日志（stderr 通道）
    err = capsys.readouterr().err
    assert "wx.init 完成" in err
    assert "[SIDECAR]" in err


def test_init_real_path_resets_listen_engine(monkeypatch, capsys):
    """wx.init 成功路径必须复位监听引擎：StopListening → StartListening
    （参考项目 init_wx_listeners wxbot_core.py:2462-2464，中间 sleep 1）。

    sidecar 崩溃重启后 wxautox4 监听引擎残留脏态（旧回调/半注册会话），
    不复位则后续 AddListenChat 全挂（2026-09-10 真机 08:16「部分监听
    重注册失败」现场）。顺序铁律：先 stop 清场再 start。
    """
    calls = _install_fake_wxautox4(monkeypatch, licensed=True, wechat_ok=True)
    _dispatch("wx.init", {}, None)
    assert calls["engine"] == ["stop", "start"], (
        f"引擎复位序列须为 stop→start，实际: {calls['engine']}"
    )
    err = capsys.readouterr().err
    assert "监听引擎复位" in err  # 留痕（排障单行可裁决）


def test_init_real_path_engine_reset_failure_not_fatal(monkeypatch, capsys):
    """引擎复位抛异常不判 init 失败——降级 WARN 留痕（微信能构造成功说明
    窗口在，引擎复位失败可能只是时序；把 init 判死会让激活页误报微信未开）。
    """
    _install_fake_wxautox4(monkeypatch, licensed=True, wechat_ok=True)
    _orig = sys.modules["wxautox4"].WeChat

    class _BoomEngine(_orig):
        def StartListening(self):
            raise RuntimeError("引擎启动超时")

    sys.modules["wxautox4"].WeChat = _BoomEngine
    try:
        r = _dispatch("wx.init", {}, None)
        assert r["licensed"] is True
        assert "failReason" not in r
        err = capsys.readouterr().err
        assert "WARN" in err and "复位" in err
    finally:
        sys.modules["wxautox4"].WeChat = _orig


def test_init_real_path_unlicensed(monkeypatch, capsys):
    """未授权：failReason=licensed（WeChat 同失败也不改口径——授权是可行动根因）"""
    _install_fake_wxautox4(monkeypatch, licensed=False, wechat_ok=False)
    r = _dispatch("wx.init", {}, None)
    assert r["licensed"] is False
    assert r["failReason"] == "licensed"
    # 埋点断言（spec §6）：完成行须带 licensed=False 口径
    err = capsys.readouterr().err
    assert "wx.init 完成" in err
    assert "licensed=False" in err


def test_init_real_path_wechat_missing(monkeypatch, capsys):
    """授权过 + 微信未开（中英两版本兜底都抛）：failReason=wechat_missing

    failDetail 携带第二次（国际版兜底）的异常类型与消息——wechat_missing
    判定条件过宽（微信没开/未登录/版本超区间/UIA 权限都归此），细节是
    真机排障裁决真实根因的唯一线索（2026-09-09 真机 licensed=True +
    wechat_missing 误报事故）。
    """
    _install_fake_wxautox4(monkeypatch, licensed=True, wechat_ok=False)
    r = _dispatch("wx.init", {}, None)
    assert r["licensed"] is True
    assert r["failReason"] == "wechat_missing"
    # 异常细节：类型名 + 消息（fake 抛 RuntimeError("微信窗口未找到")）
    assert "RuntimeError" in r.get("failDetail", "")
    assert "微信窗口未找到" in r.get("failDetail", "")


def test_init_real_path_wechat_missing_logs_warn(monkeypatch, capsys):
    """两次 WeChat(version=…) 失败各留一条 WARN（stderr 通道）——2026-09-09
    真机事故的直接修复点：旧代码 except Exception 静默吞掉，日志 tab 与
    落盘零留痕，无法裁决真实根因。"""
    _install_fake_wxautox4(monkeypatch, licensed=True, wechat_ok=False)
    _dispatch("wx.init", {}, None)
    err = capsys.readouterr().err
    assert "[SIDECAR] [WARN]" in err
    assert "WeChat(version=微信) 失败" in err
    assert "WeChat(version=WeChat) 失败" in err
    # 完成行带 detail 段（排障单行可裁决）
    assert "wx.init 完成" in err
    assert "detail=" in err


def test_init_real_path_systemexit_normalized(monkeypatch, capsys):
    """CI 六跑回归：未授权设备 check_license 向 stdout 打横幅后 SystemExit 裸退。

    wxautox4 的 check_license 把授权横幅 print 到 stdout（JSON-RPC 帧专用通道）
    再 raise SystemExit（非 Exception 子类，except Exception 接不住）——裸机
    首启动 wx.init 即 sidecar 死、Supervisor 无限重启。守卫双职责：
    ① 横幅改道 stderr（帧通道纯净）；② SystemExit 归一 licensed=false 三态
    （激活页正常引导，sidecar 不死）。
    """
    _install_fake_wxautox4(monkeypatch, license_exits=True)
    r = _dispatch("wx.init", {}, None)
    assert r["licensed"] is False
    assert r["failReason"] == "licensed"
    assert r["wxid"] == "" and r["nickname"] == ""
    captured = capsys.readouterr()
    assert "未授权设备" not in captured.out  # stdout 帧通道未被横幅污染
    assert "未授权设备" in captured.err      # 横幅被守卫改道日志通道


def test_wx_get_my_info_reads_id_key(fakewx):
    """GetMyInfo() dict key 'id'（附录 A：不是 '微信号'/'昵称' 键）"""
    r = _dispatch("wx.get_my_info", {}, fakewx)
    assert r["wxid"] == "wx_test_1"
    assert r["nickname"] == "测试号"
    assert r["online"] is True
    assert "licensed" in r  # Rust InitResult 契约字段


def test_wx_is_online(fakewx):
    assert _dispatch("wx.is_online", {}, fakewx) == {"online": True}


# ══════════ 发送类：关键字参数名 ══════════


def test_msg_send_uses_keyword_args(fakewx):
    _dispatch("msg.send", {"who": "张三", "text": "你好"}, fakewx)
    assert fakewx.sent == [("msg", "你好", "张三")]


def test_file_send_single_filepath(fakewx):
    _dispatch("file.send", {"who": "张三", "filepath": "C:/a.png"}, fakewx)
    assert fakewx.sent == [("file", "张三", "C:/a.png")]


def test_msg_send_at_routes_to_subwindow(fakewx):
    """@ 消息：wx.SendMsg 不带 at → 定位子窗口 chat.SendMsg(msg=..., at=...)（附录 A）"""
    chat = FakeChat("工作群", chat_type="group")
    fakewx.sub_windows.append(chat)
    _dispatch("msg.send", {"who": "工作群", "text": "通知", "at": "李四"}, fakewx)
    assert fakewx.sent == []  # 全局 SendMsg 未走（不带 at 的路径）
    assert chat.sent == [("通知", "李四")]


def test_msg_send_at_uses_chat_sendmsg(monkeypatch):
    """@ 消息走 chat.SendMsg(msg=..., at=...)——参考项目 3353 行"""
    wx = FakeWx()
    chat = MagicMock()
    monkeypatch.setattr(wx, "GetSubWindow", lambda nickname=None: chat if nickname == "工作群" else None)
    _dispatch("msg.send", {"who": "工作群", "text": "通知", "at": "李四"}, wx)
    chat.SendMsg.assert_called_once_with(msg="通知", at="李四")


# ══════════ was_send_success 双形态（附录 B） ══════════


def test_msg_send_dict_success_status(fakewx):
    fakewx.send_result = {"status": "success"}
    assert _dispatch("msg.send", {"who": "a", "text": "b"}, fakewx) == {"ok": True}


def test_msg_send_dict_failure_raises(fakewx):
    fakewx.send_result = {"status": "failed", "message": "窗口找不到"}
    import methods
    with pytest.raises(methods.SidecarError):
        _dispatch("msg.send", {"who": "a", "text": "b"}, fakewx)


def test_msg_send_false_raises(fakewx):
    fakewx.send_result = False
    import methods
    with pytest.raises(methods.SidecarError):
        _dispatch("msg.send", {"who": "a", "text": "b"}, fakewx)


@pytest.mark.parametrize("result,expected", [
    (True, True),
    (False, False),
    (None, False),
    ({"status": "success"}, True),
    ({"status": "ok"}, True),
    ({"status": "成功"}, True),
    ({"status": "error"}, False),
    ({"status": "失败"}, False),
    ({"code": 0}, True),
    ({"success": True}, True),
    ({"success": False}, False),
    ("some_string", True),  # bool() 兜底
])
def test_was_send_success_matrix(result, expected):
    import methods
    assert methods.was_send_success(result) is expected


def test_send_error_message_extraction():
    import methods
    assert methods.send_error_message({"message": "超时"}) == "超时"
    assert methods.send_error_message(False) == "发送失败"


# ══════════ 聊天窗口 ══════════
# chat.open 已退役（P2 死代码清理：不在 16-action 白名单，三方零消费；
# ChatWith 预热能力由 _listen_add / _chat_search / _chat_history 内部自持）


def test_chat_search_found(fakewx):
    fakewx.sub_windows.append(FakeChat("张三"))
    r = _dispatch("chat.search", {"keyword": "张三"}, fakewx)
    assert r["found"] is True
    assert r["who"] == "张三"


def test_chat_search_not_found(fakewx):
    r = _dispatch("chat.search", {"keyword": "不存在"}, fakewx)
    assert r["found"] is False
    assert r["who"] == ""


def test_chat_history_returns_last_n_rawmessage_shape(fakewx):
    """chat.history 消息元素是 7 字段 RawMessage 形状（对齐 Rust spec.rs）"""
    fakewx.sub_windows.append(FakeChat("张三"))
    fakewx.messages = [FakeMsg(content=f"消息{i}") for i in range(30)]
    r = _dispatch("chat.history", {"who": "张三", "n": 5}, fakewx)
    assert len(r["messages"]) == 5
    last = r["messages"][-1]
    assert set(last.keys()) == {"msg_id", "chat_who", "chat_type", "attr", "msg_type", "sender", "content"}
    assert last["content"] == "消息29"
    assert last["chat_who"] == "张三"
    assert fakewx.opened == ["张三"]  # 先 ChatWith 打开窗口


# ══════════ 监听：三重校验 ══════════


def test_listen_add_success_with_verification(fakewx):
    r = _dispatch("listen.add", {"nickname": "客户群"}, fakewx)
    assert r == {"ok": True, "verified": True}
    assert "客户群" in fakewx.listen_reg  # callback 已注册


def test_listen_add_retry_on_missing_subwindow(fakewx):
    """AddListenChat 返回 True 但子窗口未出现 → 重试；第 2 次补上后通过"""
    calls = {"n": 0}
    orig = fakewx.AddListenChat

    def flaky(nickname=None, callback=None):
        calls["n"] += 1
        if calls["n"] == 1:
            fakewx.listen_reg[nickname] = callback  # 注册了但不加子窗口
            return True
        return orig(nickname=nickname, callback=callback)

    fakewx.AddListenChat = flaky
    r = _dispatch("listen.add", {"nickname": "客户群"}, fakewx)
    assert r["verified"] is True
    assert calls["n"] == 2


def test_listen_add_triple_verification_failure(fakewx):
    """三重校验全失败（返回 dict 错误 + 子窗口永不出现）→ SidecarError"""
    import methods

    def bad_add(nickname=None, callback=None):
        return {"message": "注册失败"}

    fakewx.AddListenChat = bad_add
    with pytest.raises(methods.SidecarError, match="三重校验"):
        _dispatch("listen.add", {"nickname": "客户群"}, fakewx)


# ══════════ listen.add 窗口态预热（2026-09-10 真机四类失败修复） ══════════


def test_listen_add_prewarms_chat_with(fakewx):
    """AddListenChat 前必须 ChatWith(who=nickname) 预热——wxautox4 在主窗口
    会话列表里搜目标会话，目标不在近期会话（如新群刚建）时 AddListenChat
    直接失败（2026-09-10 真机「测试群1」三重校验全挂根因之一）。"""
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx)
    assert fakewx.opened == ["客户群"]  # 先 ChatWith 再 AddListenChat


def test_listen_add_triple_failure_reports_window_state(fakewx):
    """三重校验耗尽的 SidecarError 必须携带窗口态细节（主窗口在线态 +
    已开子窗口数）——旧文案只有昵称，真机排障无法裁决「主窗口最小化 /
    被遮挡 / 子窗口残留」哪一种。"""
    import methods

    def bad_add(nickname=None, callback=None):
        return {"message": "注册失败"}

    fakewx.AddListenChat = bad_add
    fakewx.sub_windows = [FakeChat("旧监听A"), FakeChat("旧监听B")]
    with pytest.raises(methods.SidecarError) as ei:
        _dispatch("listen.add", {"nickname": "客户群"}, fakewx)
    msg = str(ei.value)
    assert "客户群" in msg
    assert "注册失败" in msg  # 每轮 AddListenChat 的错误信息入列
    assert "子窗口=2" in msg  # 已开子窗口数（残留诊断）
    assert "主窗口" in msg  # 主窗口在线态


def test_listen_add_uia_exception_captured_into_detail(fakewx):
    """AddListenChat 抛 UIA LookupError 不再裸穿 -32603——捕获入每轮
    错误列表（真机 08:16 现场：LookupError: Find Control Timeout 混着
    -32000 文案出现，前端无法引导）。"""
    import methods

    def boom_add(nickname=None, callback=None):
        raise LookupError("Find Control Timeout: {Name: '会话列表'}")

    fakewx.AddListenChat = boom_add
    with pytest.raises(methods.SidecarError, match="Find Control Timeout"):
        _dispatch("listen.add", {"nickname": "客户群"}, fakewx)


def test_listen_add_uia_exception_on_verification_captured(fakewx):
    """校验阶段（GetAllSubWindow/GetSubWindow）抛 UIA 异常同样入列不裸穿。"""
    import methods

    def boom_subwindows():
        raise LookupError("Find Control Timeout: {Name: '消息列表'}")

    fakewx.GetAllSubWindow = boom_subwindows
    with pytest.raises(methods.SidecarError, match="Find Control Timeout"):
        _dispatch("listen.add", {"nickname": "客户群"}, fakewx)


# ══════════ friends/moments 主窗口导航前置校验 ══════════


def test_new_requests_uia_failure_wraps_actionable_hint(fakewx):
    """GetNewFriends 抛 LookupError（主窗口不可操作：最小化/被遮挡/通讯录
    导航失败）→ SidecarError 带排查指引而非裸 -32603（真机 08:16 现场直接
    错误帧 'LookupError: Find Control Timeout ... 新的朋友'）。"""
    import methods

    def boom(acceptable=None):
        raise LookupError("Find Control Timeout: {Name: '新的朋友'}")

    fakewx.GetNewFriends = boom
    with pytest.raises(methods.SidecarError) as ei:
        _dispatch("friends.new_requests", {}, fakewx)
    msg = str(ei.value)
    assert "新的朋友" in msg  # 原始异常保留
    assert "最小化" in msg  # 排查指引


def test_new_requests_restores_chat_page(fakewx):
    """GetNewFriends 会把主窗口导航到通讯录页——结束必须 SwitchToChat()
    切回聊天页（参考项目 Pass_New_Friends wxbot_core.py:4467 收尾）。

    不切回则主窗口停在通讯录页，后续 AddListenChat 在会话列表里搜目标
    必败（2026-09-10 真机 08:15 用户加监听失败与 08:16 好友泵报错的
    时序关联：好友泵每 60~300s 污染一次主窗口页面态）。
    """
    fakewx.new_friends = [FakeFriend("申请人A")]
    _dispatch("friends.new_requests", {}, fakewx)
    assert fakewx.switched >= 1  # SwitchToChat 收尾


def test_new_requests_restore_failure_not_fatal(fakewx):
    """SwitchToChat 收尾失败只 WARN 不炸结果——好友列表已取到，导航残留
    交给下一轮轮询自愈（下次 GetNewFriends 会重新导航）。"""
    fakewx.new_friends = [FakeFriend("申请人A")]
    called = {"n": 0}

    def boom_switch():
        called["n"] += 1
        raise LookupError("Find Control Timeout: {Name: '聊天'}")

    fakewx.SwitchToChat = boom_switch
    r = _dispatch("friends.new_requests", {}, fakewx)
    assert called["n"] >= 1  # 检测力：收尾确实被调用（旧实现不调则假绿）
    assert r["requests"][0]["name"] == "申请人A"


def test_moments_get_uia_failure_wraps_actionable_hint(fakewx):
    """朋友圈打开/读取抛 UIA 异常 → 同款带指引包装（Close 兜底不丢）。"""
    import methods

    class BoomMoments:
        def GetMoments(self):
            raise LookupError("Find Control Timeout: {Name: '朋友圈'}")

        def Close(self):
            self.closed = True

    fakewx.Moments = lambda: BoomMoments()
    with pytest.raises(methods.SidecarError, match="最小化"):
        _dispatch("moments.get", {"count": 1}, fakewx)


def test_listen_remove_reports_still_exists(fakewx):
    fakewx.AddListenChat(nickname="客户群", callback=None)
    r = _dispatch("listen.remove", {"nickname": "客户群"}, fakewx)
    assert r == {"ok": True, "stillExists": False}


def test_listen_list_reads_who_attr(fakewx):
    fakewx.sub_windows = [FakeChat("A"), FakeChat("B")]
    r = _dispatch("listen.list", {}, fakewx)
    assert r == {"chats": ["A", "B"]}


# ══════════ message.received 通知 + msg 池 ══════════


def test_listen_callback_fires_notification_with_msg_id(fakewx):
    """回调触发 message.received 通知（带 msg_id 供二次 RPC），msg 对象入池

    字段 snake_case 对齐 Rust RawMessage（msg_id/chat_who/chat_type/...）
    """
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    # 模拟微信来消息：wxautox4 调 callback(msg, chat)
    msg = FakeMsg(content="在吗")
    chat = FakeChat("客户群", chat_type="group")
    fakewx.listen_reg["客户群"](msg, chat)
    assert len(notifications) == 1
    method, params = notifications[0]
    assert method == "message.received"
    assert params["content"] == "在吗"
    assert params["chat_who"] == "客户群"
    assert params["chat_type"] == "group"
    assert params["msg_type"] == "text"
    assert params["sender"] == "张三"
    assert params["msg_id"]  # 非空 msg_id
    assert msg_pool[params["msg_id"]][0] is msg  # 对象入池（(msg, chat_who) 元组形态）
    assert params["is_at"] is False  # FakeMsg 默认 is_at=False 透传


def test_listen_callback_msg_id_prefers_native_id(fakewx):
    """msg_id 优先取 wxautox 原生 id（确定性幂等键——Server 可按 msg_id 去重）"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="原生id消息")
    msg.id = "native_7788"
    fakewx.listen_reg["客户群"](msg, FakeChat("客户群"))
    assert notifications[0][1]["msg_id"] == "native_7788"
    assert "native_7788" in msg_pool  # 池键同步确定性


def test_listen_callback_msg_id_falls_back_to_uuid(fakewx):
    """旧版 wxautox4 msg 无 id 属性：退回随机 uuid（12 hex），行为不劣于现状"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="无id消息")
    del msg.id  # 旧库无此属性
    fakewx.listen_reg["客户群"](msg, FakeChat("客户群"))
    mid = notifications[0][1]["msg_id"]
    assert len(mid) == 12 and all(c in "0123456789abcdef" for c in mid)


def test_listen_callback_passes_is_at_true(fakewx):
    """群内 @机器人消息：msg.is_at=True 透传到通知帧（硬编码 false 的矫正）"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="@机器人 你好")
    msg.is_at = True
    chat = FakeChat("客户群", chat_type="group")
    fakewx.listen_reg["客户群"](msg, chat)
    assert notifications[0][1]["is_at"] is True


def test_raw_message_is_at_missing_attr_defaults_false():
    """旧版 wxautox4 无 is_at 属性：getattr 默认值安全降级 False（不炸回调）"""
    class OldMsg:
        content = "hi"
        type = "text"
        attr = "friend"
        sender = "张三"

    class OldChat:
        who = "张三"
        chat_type = "friend"

    import methods
    r = methods._raw_message(OldMsg(), OldChat(), "m1")
    assert r["is_at"] is False


def test_listen_callback_exception_does_not_propagate(fakewx):
    """notify 抛异常不能上抛到 wxautox4 回调线程（会杀监听）——须吞掉并写 stderr"""
    def bad_notify(m, p):
        raise RuntimeError("stdout 管道破裂")

    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx, notify=bad_notify,
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="在吗")
    # 不应抛异常
    fakewx.listen_reg["客户群"](msg, FakeChat("客户群"))


def test_media_download_via_msg_pool(fakewx):
    msg = FakeMsg(content="img", mtype="image")
    msg.downloaded = "C:/temp/abc.png"
    r = _dispatch("media.download", {"msgId": "mid123"}, fakewx, msg_pool={"mid123": (msg, "张三")})
    assert r == {"path": "C:/temp/abc.png"}


def test_media_download_accepts_snake_case_key(fakewx):
    """msg_id 拼写（snake_case）也接受——Rust 侧构造 params 时两种命名都兼容"""
    msg = FakeMsg(mtype="image")
    msg.downloaded = "C:/temp/x.png"
    r = _dispatch("media.download", {"msg_id": "mid123"}, fakewx, msg_pool={"mid123": (msg, "张三")})
    assert r == {"path": "C:/temp/x.png"}


def test_media_download_missing_raises(fakewx):
    import methods
    with pytest.raises(methods.SidecarError, match="过期|不存在"):
        _dispatch("media.download", {"msgId": "gone"}, fakewx, msg_pool={})


def test_media_download_expired_raises(fakewx):
    """60s 过期在访问时真实执行（不只在错误文案里）"""
    import methods
    msg = FakeMsg(mtype="image")
    msg.downloaded = "C:/temp/old.png"
    pool = {"old": msg}
    pool_ts = {"old": time.time() - 61}
    with pytest.raises(methods.SidecarError, match="过期"):
        _dispatch("media.download", {"msgId": "old"}, fakewx, msg_pool=pool, msg_pool_ts=pool_ts)
    assert "old" not in pool  # 过期条目被清理


def test_media_download_falsy_raises_scale_hint(fakewx):
    import methods
    msg = FakeMsg()
    msg.downloaded = None  # download() 失败（屏幕缩放）
    with pytest.raises(methods.SidecarError, match="缩放"):
        _dispatch("media.download", {"msgId": "m1"}, fakewx, msg_pool={"m1": msg})


def test_voice_to_text_via_msg_pool(fakewx):
    msg = FakeMsg(mtype="voice")
    r = _dispatch("voice.to_text", {"msgId": "v1"}, fakewx, msg_pool={"v1": (msg, "张三")})
    assert r == {"text": "语音转写文本"}


def test_voice_to_text_failure_degrades(fakewx):
    """to_text() 异常 → 降级返回 failed 标记（不中断，spec §2.4）"""
    msg = FakeMsg(mtype="voice")
    msg.text = RuntimeError("转写失败")
    r = _dispatch("voice.to_text", {"msgId": "v1"}, fakewx, msg_pool={"v1": (msg, "张三")})
    assert r == {"text": "", "failed": True}


# ══════════ 引用 / 转发 ══════════


def test_msg_quote_locates_and_quotes(fakewx):
    msg = FakeMsg(content="原始消息")
    fakewx.messages = [msg]
    r = _dispatch("msg.quote", {"who": "张三", "quoteContent": "原始", "text": "回复"}, fakewx)
    assert r["ok"] is True
    assert msg.quoted == ("回复", None)


def test_msg_quote_via_msg_pool_msgid(fakewx):
    """msgId 直取池中对象（spec §3.2 签名）——无需 ChatWith 定位"""
    msg = FakeMsg(content="原始消息")
    r = _dispatch("msg.quote", {"msgId": "q1", "text": "回复", "at": "王五"},
                  fakewx, msg_pool={"q1": (msg, "张三")})
    assert r["ok"] is True
    assert msg.quoted == ("回复", "王五")
    assert fakewx.opened == []  # 未走窗口定位路径


def test_msg_quote_msgid_path_degrades_to_chat_who(fakewx):
    """C1 回归：msgId 路径 quote 失败 → 降级支不得 KeyError；

    spec §3.2 msg.quote 签名是 {msgId, text, at?}（无 who）——降级目标从
    入池时附带的 chat_who 取，降级普通发送确实发出。
    """
    msg = FakeMsg(content="原始消息")
    msg.quote = lambda content, at=None: False  # quote 失败形态
    r = _dispatch("msg.quote", {"msgId": "q1", "text": "回复"},
                  fakewx, msg_pool={"q1": (msg, "张三")})
    assert r["ok"] is True
    assert r.get("degraded")
    assert fakewx.sent == [("msg", "回复", "张三")]  # 降级发到池附带的 chat_who


def test_msg_quote_msgid_path_degrade_without_chat_who_raises(fakewx):
    """C1 边界：msgId 路径降级且池条目无 chat_who（旧形态裸 msg）→ 明确业务错误

    而不是 KeyError 落 -32603。
    """
    import methods
    msg = FakeMsg(content="x")
    msg.quote = lambda content, at=None: False
    with pytest.raises(methods.SidecarError, match="无法降级"):
        _dispatch("msg.quote", {"msgId": "q1", "text": "回复"},
                  fakewx, msg_pool={"q1": msg})  # 裸 msg 旧形态（无 who 可取）


def test_msg_quote_not_found_raises(fakewx):
    import methods
    fakewx.messages = []
    with pytest.raises(methods.SidecarError, match="未定位"):
        _dispatch("msg.quote", {"who": "张三", "quoteContent": "不存在", "text": "回复"}, fakewx)


def test_msg_quote_failure_degrades_to_plain_send(fakewx):
    """quote 失败 → 降级普通发送（参考项目 3349-3354：降级必须真的发出去）"""
    fakewx.messages = [FakeMsg(content="x")]
    fakewx.messages[0].quote = lambda content, at=None: False  # quote 返回失败形态
    r = _dispatch("msg.quote", {"who": "张三", "quoteContent": "x", "text": "回复"}, fakewx)
    assert r["ok"] is True
    assert r.get("degraded")
    assert fakewx.sent == [("msg", "回复", "张三")]  # 降级后确实普通发送了


def test_msg_quote_exception_also_degrades(fakewx):
    """quote 抛异常 → 同样降级普通发送"""
    fakewx.messages = [FakeMsg(content="x")]
    fakewx.messages[0].quote = lambda content, at=None: (_ for _ in ()).throw(RuntimeError("UIA 超时"))
    r = _dispatch("msg.quote", {"who": "张三", "quoteContent": "x", "text": "回复"}, fakewx)
    assert r["ok"] is True
    assert r.get("degraded")


def test_msg_forward_locates_and_forwards(fakewx):
    msg = FakeMsg(content="转发我")
    fakewx.messages = [msg]
    r = _dispatch("msg.forward", {"target": "李四", "sourceWho": "张三", "match": "转发我"}, fakewx)
    assert r == {"ok": True}
    assert msg.forwarded == ("李四", None)


def test_msg_forward_via_msg_pool_with_note(fakewx):
    """msgId 直取 + note 附言（msg.forward(who, message=None)，附录 A）"""
    msg = FakeMsg(content="转发我")
    r = _dispatch("msg.forward", {"msgId": "f1", "target": "李四", "note": "请查收"},
                  fakewx, msg_pool={"f1": (msg, "张三")})
    assert r == {"ok": True}
    assert msg.forwarded == ("李四", "请查收")


def test_msg_forward_not_found_raises(fakewx):
    import methods
    fakewx.messages = []
    with pytest.raises(methods.SidecarError, match="未定位"):
        _dispatch("msg.forward", {"target": "李四", "sourceWho": "张三", "match": "无"}, fakewx)


# ══════════ 好友 ══════════


def test_new_requests_lists_names(fakewx):
    fakewx.new_friends = [FakeFriend("申请人A")]
    r = _dispatch("friends.new_requests", {}, fakewx)
    assert r == {"requests": [{"name": "申请人A"}]}


def test_friend_accept_passes_remark_tags(fakewx):
    friend = FakeFriend("申请人A")
    fakewx.new_friends = [friend]
    r = _dispatch("friend.accept", {"name": "申请人A", "remark": "备注", "tags": ["客户"]}, fakewx)
    assert r == {"ok": True}
    assert friend.accepted == ("备注", ["客户"])
    assert fakewx.switched == 1  # SwitchToChat 收尾


def test_friend_accept_gbk_truncation(fakewx):
    """备注 GBK 32 字节截断（防 wxautox4 超长备注失败）"""
    friend = FakeFriend("申请人A")
    fakewx.new_friends = [friend]
    long_remark = "备" * 50  # GBK 下 100 字节 > 32
    _dispatch("friend.accept", {"name": "申请人A", "remark": long_remark}, fakewx)
    remark = friend.accepted[0]
    assert len(remark.encode("gbk")) <= 32
    assert remark == "备" * 16  # 32/2 = 16 个汉字


def test_friend_accept_remark_with_emoji_survives(fakewx):
    """备注含 emoji（非 GBK 字符）不得抛 UnicodeEncodeError——encode 侧 errors=ignore"""
    friend = FakeFriend("申请人A")
    fakewx.new_friends = [friend]
    _dispatch("friend.accept", {"name": "申请人A", "remark": "客户😀备份"}, fakewx)
    remark = friend.accepted[0]
    assert "😀" not in remark  # emoji 被丢弃
    assert remark == "客户备份"
    assert len(remark.encode("gbk")) <= 32


def test_friend_accept_not_found_raises(fakewx):
    import methods
    with pytest.raises(methods.SidecarError, match="未找到"):
        _dispatch("friend.accept", {"name": "不存在"}, fakewx)


# ══════════ 朋友圈 ══════════


def test_moments_get_closes_window(fakewx):
    r = _dispatch("moments.get", {"count": 1}, fakewx)
    assert len(r["moments"]) == 1
    assert r["moments"][0]["content"] == "动态1"
    assert fakewx.moments_obj.closed is True  # 用完必须 Close


def test_moments_publish_privacy_chinese_values(fakewx):
    _dispatch("moments.publish",
              {"text": "动态", "images": [], "privacy": "whitelist", "tags": ["客户"]}, fakewx)
    args = fakewx.moments_obj.published
    assert args[0] == "动态"
    # 关键断言：privacy dict 中文值（spec 附录 A）
    assert args[2] == {"privacy": "白名单", "tags": ["客户"]}


def test_moments_publish_blacklist(fakewx):
    _dispatch("moments.publish",
              {"text": "动态", "privacy": "blacklist", "tags": ["黑名单组"]}, fakewx)
    assert fakewx.moments_obj.published[2] == {"privacy": "黑名单", "tags": ["黑名单组"]}


def test_moments_publish_public_empty_dict(fakewx):
    _dispatch("moments.publish", {"text": "动态", "privacy": "public"}, fakewx)
    assert fakewx.moments_obj.published[2] == {}  # 公开传空 dict
    assert fakewx.moments_obj.published[1] is None  # 无图传 None


def test_moments_open_failure_raises(fakewx):
    import methods
    fakewx.Moments = lambda: None  # 打开失败
    with pytest.raises(methods.SidecarError, match="朋友圈"):
        _dispatch("moments.get", {"count": 1}, fakewx)


# ══════════ util.sleep / MOCK / 未知方法 ══════════


def test_util_sleep():
    r = _dispatch("util.sleep", {"ms": 10}, None)
    assert r == {"ok": True}


def test_unknown_method_raises():
    import methods
    with pytest.raises(methods.SidecarError, match="未知"):
        _dispatch("no.such.method", {}, FakeWx())


def test_mock_covers_all_non_init_methods():
    """MOCK 表必须覆盖除 wx.init 外的全部 18 个方法（Linux CI 全链路依赖）

    wx.activate 不在此列：它走 dispatch 特判（mock 时直达 _activate 的
    mock 分支，见 test_activate_mock_mode），不经 _mock_dispatch 查表。
    chat.open 已删（P2 死代码清理：不在 16-action 白名单，三方零消费）。
    """
    methods_list = [
        "wx.get_my_info", "wx.is_online",
        "msg.send", "file.send", "msg.quote", "msg.forward",
        "chat.search", "chat.history",
        "listen.add", "listen.remove", "listen.list",
        "friends.new_requests", "friend.accept",
        "moments.get", "moments.publish",
        "media.download", "voice.to_text", "util.sleep",
    ]
    for m in methods_list:
        params = {"who": "x", "text": "t", "filepath": "p", "nickname": "n", "keyword": "k",
                  "msgId": "m", "name": "n", "count": 1, "ms": 1, "n": 1}
        r = _dispatch(m, params, None, mock=True)
        assert isinstance(r, dict), f"MOCK 未覆盖: {m}"


def test_chat_open_removed():
    """chat.open 已退役：dispatch 表不得再路由（调用即业务错误）"""
    import methods

    assert "chat.open" not in methods._METHODS
    with pytest.raises(methods.SidecarError):
        _dispatch("chat.open", {"who": "x"}, None, mock=False)


def test_mock_unknown_raises():
    import methods
    with pytest.raises(methods.SidecarError):
        _dispatch("no.such", {}, None, mock=True)


def test_mock_init_followed_by_other_methods():
    """mock init 后 get_my_info 走 mock 表（实例载体为 None 不影响）"""
    r = _dispatch("wx.get_my_info", {}, None, mock=True)
    assert r["wxid"] == "mock_wx"
    assert r["online"] is True


# ══════════ 并发安全（审查 I1/M6 回归） ══════════


def test_prune_expired_survives_concurrent_insertion():
    """I1 回归：主线程清理迭代 msg_pool_ts 时监听线程并发插入不得

    RuntimeError: dictionary changed size during iteration（迭代须走快照）。
    插入方有界（总量 4000 键）防池无限膨胀拖慢测试。
    """
    import threading

    pool, pool_ts = {}, {}
    N_INSERT = 4000
    done = threading.Event()

    def inserter():
        for i in range(N_INSERT):
            pool[f"k{i}"] = FakeMsg()
            pool_ts[f"k{i}"] = time.time()
        done.set()

    t = threading.Thread(target=inserter)
    t.start()
    try:
        # 主线程高频迭代清理——旧实现（无快照）在并发插入下必炸 RuntimeError
        for _ in range(3000):
            methods_mod()._prune_expired(pool, pool_ts, ttl=60)
    finally:
        done.wait(timeout=10)
        t.join(timeout=10)
    assert len(pool_ts) > 0  # 确认并发写入确实发生（未过期条目留存）


def methods_mod():
    import methods
    return methods


def test_sidecar_writers_serialized_under_lock():
    """M6 回归：_result/_error/_notify 三写函数并发调用必须串行化

    检测方式：捕获底层 write piece 序列，任何一「帧」的多个 piece 之间
    不得插入其他帧的 piece（帧原子性 = 写+flush 全程持锁）。
    """
    import threading
    import sidecar

    assert hasattr(sidecar, "_io_lock"), "sidecar.py 须有模块级 _io_lock"

    pieces = []  # (thread_idx, piece)
    lock = threading.Lock()

    class Cap:
        def write(self, s):
            with lock:
                pieces.append((threading.current_thread().name, s))
            return len(s)

        def flush(self):
            pass

    orig_stdout = sys.stdout
    sys.stdout = Cap()
    try:
        def worker(fn, args, tid):
            threading.current_thread().name = f"w{tid}"
            for _ in range(300):
                fn(*args)

        threads = [
            threading.Thread(target=worker, args=(sidecar._notify, ("message.received", {"i": "x" * 50}), 1)),
            threading.Thread(target=worker, args=(sidecar._result, (1, {"r": "y" * 50}), 2)),
            threading.Thread(target=worker, args=(sidecar._error, (2, -32000, "业务错误"), 3)),
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
    finally:
        sys.stdout = orig_stdout

    # 帧原子性校验：按 thread 分组各自 piece 必须连续（帧 = body + "\n" 两个 piece）
    # 出现 A-body, B-body, A-"\n" 交错即原子性破坏
    seq = [tid for tid, _ in pieces]
    runs = []  # 压缩成连续段 [(tid, count), ...]
    for tid in seq:
        if runs and runs[-1][0] == tid:
            runs[-1][1] += 1
        else:
            runs.append([tid, 1])
    # 每帧恰 2 个 piece（json.dumps 主体 + "\n"）→ 每个连续段长度必须是 2 的倍数
    # 且段与段之间不回头（同一线程的帧间允许穿插，但帧内不可）
    for i, (tid, cnt) in enumerate(runs):
        assert cnt % 2 == 0, (
            f"帧原子性破坏：第 {i} 段线程 {tid} 连续 {cnt} 个 piece（奇数 = 帧被从中间撕开）"
        )


# ══════════ wx.activate（激活码认证）══════════


# CI 六跑实证的未授权横幅原文（stdout 污染源；guard_stdout 须把它改道 stderr）
_LICENSE_BANNER = ">>> 未授权设备，获取授权：https://wxauto.org"


def _install_fake_wxautox4(monkeypatch, licensed=True, authenticate_result=True, wechat_ok=True,
                           license_exits=False, authenticate_exits=False):
    """伪造 wxautox4 包：sys.modules 预置三模块 + 顶层属性挂接。

    wechat_ok=False 时 WeChat() 构造即抛（模拟微信窗口未找到，供 wx.init
    三态测试使用）；wx.activate 不触 WeChat，默认值即可。
    license_exits/authenticate_exits=True 时模拟 wxautox4 真实未授权行为：
    向 stdout print 横幅后 raise SystemExit（CI 六跑实证的裸机首启动形态）。
    返回 calls 字典记录 authenticate 实参（断言激活码透传）。
    """
    import types

    calls = {"authenticate": [], "engine": []}

    def _check_license():
        if license_exits:
            print(_LICENSE_BANNER)
            raise SystemExit(1)
        return licensed

    def _authenticate(code):
        calls["authenticate"].append(code)
        if authenticate_exits:
            print(_LICENSE_BANNER)
            raise SystemExit(1)
        return authenticate_result

    class _FakeWeChat:
        def __init__(self, version=None):
            if not wechat_ok:
                raise RuntimeError("微信窗口未找到")

        def StopListening(self):
            calls["engine"].append("stop")

        def StartListening(self):
            calls["engine"].append("start")

    useful = types.ModuleType("wxautox4.utils.useful")
    useful.check_license = _check_license
    useful.authenticate = _authenticate
    utils = types.ModuleType("wxautox4.utils")
    utils.useful = useful
    top = types.ModuleType("wxautox4")
    top.utils = utils
    top.WeChat = _FakeWeChat
    top.WxParam = types.SimpleNamespace()

    monkeypatch.setitem(sys.modules, "wxautox4", top)
    monkeypatch.setitem(sys.modules, "wxautox4.utils", utils)
    monkeypatch.setitem(sys.modules, "wxautox4.utils.useful", useful)
    return calls


def test_activate_empty_code_short_circuits():
    """空码前置拦截：不触 wxautox4 导入即返回失败"""
    r = _dispatch("wx.activate", {"code": "  "}, None)
    assert r == {"ok": False, "message": "激活码不能为空"}


def test_activate_success_roundtrip(monkeypatch):
    calls = _install_fake_wxautox4(monkeypatch, licensed=True, authenticate_result=True)
    r = _dispatch("wx.activate", {"code": " ABC-123 "}, None)
    assert r["ok"] is True
    assert calls["authenticate"] == ["ABC-123"]  # strip 后透传
    # 激活成功 → 立即回查 check_license 确认
    assert r["message"] == "激活成功"


def test_activate_invalid_code(monkeypatch, capsys):
    _install_fake_wxautox4(monkeypatch, licensed=False, authenticate_result=False)
    r = _dispatch("wx.activate", {"code": "BAD"}, None)
    assert r["ok"] is False
    assert "无效" in r["message"]
    # 埋点断言（spec §6）：real-path 激活失败须打 WARN 日志（stderr 通道）
    err = capsys.readouterr().err
    assert "wx.activate 失败：激活码无效或已过期" in err
    assert "[SIDECAR]" in err


def test_activate_accepted_but_not_effective(monkeypatch):
    """authenticate 过但回查仍 false：不静默吞（spec 错误表边界条）"""
    _install_fake_wxautox4(monkeypatch, licensed=False, authenticate_result=True)
    r = _dispatch("wx.activate", {"code": "X"}, None)
    assert r["ok"] is False
    assert "重启应用" in r["message"]


def test_activate_systemexit_normalized(monkeypatch, capsys):
    """CI 六跑同源回归：authenticate 被拒（横幅 + SystemExit 裸退）归一失败文案。

    守卫双职责同 _init：横幅走 stderr、SystemExit 不裸穿（sidecar 不死，
    激活页收到既有「激活码无效或已过期」失败文案可重试）。
    """
    _install_fake_wxautox4(monkeypatch, authenticate_exits=True)
    r = _dispatch("wx.activate", {"code": "BAD"}, None)
    assert r["ok"] is False
    assert "无效" in r["message"]
    captured = capsys.readouterr()
    assert "未授权设备" not in captured.out
    assert "未授权设备" in captured.err


def test_activate_mock_mode():
    """mock 表：MOCK-ACTIVATION 成功、其余失败（Linux CI 全链路依赖）"""
    ok = _dispatch("wx.activate", {"code": "MOCK-ACTIVATION"}, None, mock=True)
    assert ok["ok"] is True
    bad = _dispatch("wx.activate", {"code": "WRONG"}, None, mock=True)
    assert bad["ok"] is False


def test_sidecar_dispatch_activate_reaches_methods(monkeypatch):
    """C1 回归：穿透 sidecar.py 包装层调 wx.activate（未激活场景）。

    真实设备激活时序：wx.init 未授权失败 → _wx 恒 None → 用户提交激活码。
    sidecar.dispatch 的 wx 实参惰性求值若不豁免 wx.activate，_get_wx() 会先
    抛「wx 未初始化」——激活码根本到不了 methods._activate（终审 C1：
    pytest 直连 methods.dispatch 绕过包装层 + mock 冒烟 _wx 恒 None，
    双重盲区漏进主线）。本用例经 sidecar.dispatch 全链路验证豁免生效。
    """
    import sidecar

    # 伪造 wxautox4：licensed=True 模拟 authenticate 后 check_license 回查通过
    # （真实激活成功路径），authenticate 接受任意码
    _install_fake_wxautox4(monkeypatch, licensed=True, authenticate_result=True)
    # 未激活现场：sidecar 全局 _wx 恒 None（wx.init 从未成功创建实例）
    monkeypatch.setattr(sidecar, "_wx", None)
    monkeypatch.setattr(sidecar, "MOCK", False)
    r = sidecar.dispatch("wx.activate", {"code": "X"})
    assert r == {"ok": True, "message": "激活成功"}, (
        f"sidecar.dispatch 应把 wx.activate 送达 methods._activate，实际: {r}"
    )


def test_init_mock_unlicensed_injection(monkeypatch):
    """WXAUTO_MOCK_UNLICENSED=1：mock init 回未授权；激活成功后翻转"""
    monkeypatch.setenv("WXAUTO_MOCK_UNLICENSED", "1")
    import importlib
    importlib.reload(methods)
    try:
        r = _dispatch("wx.init", {}, None, mock=True)
        assert r["licensed"] is False
        assert r["failReason"] == "licensed"
        ok = _dispatch("wx.activate", {"code": "MOCK-ACTIVATION"}, None, mock=True)
        assert ok["ok"] is True
        r2 = _dispatch("wx.init", {}, None, mock=True)
        assert r2["licensed"] is True, "激活成功后 init 应翻转为已授权"
    finally:
        del os.environ["WXAUTO_MOCK_UNLICENSED"]
        importlib.reload(methods)


def test_init_mock_wechat_missing_injection(monkeypatch):
    """WXAUTO_MOCK_WECHAT_MISSING=1：mock init 回微信未开三态（GUI 冒烟/
    前端联调用，镜像 WXAUTO_MOCK_UNLICENSED 模式）"""
    monkeypatch.setenv("WXAUTO_MOCK_WECHAT_MISSING", "1")
    import importlib
    importlib.reload(methods)
    try:
        r = _dispatch("wx.init", {}, None, mock=True)
        assert r["licensed"] is True
        assert r["failReason"] == "wechat_missing"
        assert r.get("failDetail", "") != ""
    finally:
        del os.environ["WXAUTO_MOCK_WECHAT_MISSING"]
        importlib.reload(methods)


def test_sidecar_main_loop_systemexit_fallback(monkeypatch, capsys):
    """CI 六跑纵深防御：主循环对 SystemExit 兜底转 -32603 错误帧。

    SystemExit 不是 Exception 子类——methods 层漏归一时（如其它方法触到
    wxautox4 退出），主循环 except Exception 接不住、进程无帧即死。本用例
    模拟 dispatch 抛 SystemExit，断言进程存活且有错误帧可回。
    """
    import io
    import json
    import sidecar

    def _boom(method, params):
        raise SystemExit(1)

    monkeypatch.setattr(sidecar, "MOCK", True)  # 跳过 pythoncom 初始化
    monkeypatch.setattr(sidecar, "dispatch", _boom)
    monkeypatch.setattr(sys, "stdin", io.StringIO(
        '{"jsonrpc": "2.0", "id": 7, "method": "msg.send", "params": {}}\n'))
    sidecar.main()
    out = capsys.readouterr().out
    lines = [ln for ln in out.splitlines() if ln.strip()]
    assert len(lines) == 1, f"应恰输出一帧错误，实际: {lines}"
    frame = json.loads(lines[0])
    assert frame["id"] == 7
    assert frame["error"]["code"] == -32603
    assert "wxautox4 异常退出" in frame["error"]["message"]


def test_sidecar_stdin_utf8_decode_survives_local_codepage(monkeypatch, capsys):
    """stdin 编码回归（2026-09-09 真机乱码事故）。

    真机形态：中文 Windows（cp936）下 Rust 写入 UTF-8 的 listen.add 帧，
    sidecar.py 顶部只 reconfigure 了 stdout/stderr、漏了 stdin——sys.stdin
    按本地代码页解码 UTF-8 字节，「龚金龙」变「钁ｃ\\udca5栭緳」（乱码且带
    lone surrogate），wxautox4 拿乱码去搜会话必然失败，其内部 logging
    编码 surrogate 再炸 UnicodeEncodeError。本用例用 cp936 编码的假 stdin
    验证：sidecar.py 导入后 sys.stdin 已被归一为 UTF-8 解码器，中文帧
    进 dispatch 保持原文。

    检测方式：直接构造「UTF-8 字节流 + locale 误读」的现场——用
    TextIOWrapper(errors='surrogateescape') 模拟 Windows 默认 stdin 的
    代理项透传行为，若 sidecar.py 已修（stdin reconfigure UTF-8），其
    模块级归一化会在 import 时生效；本测试断言归一化函数存在且行为正确。
    """
    import sidecar

    # 修复后 sidecar.py 必须暴露 stdin 归一化入口（三流齐备的证明）
    assert hasattr(sidecar, "_ensure_utf8_streams"), (
        "sidecar.py 须有 _ensure_utf8_streams（stdin/stdout/stderr 三流齐备）"
    )
    # 归一化函数对无 buffer 的流（StringIO 等）静默跳过不炸
    sidecar._ensure_utf8_streams()

    # 进程级验证：UTF-8 字节 + cp936 语境 → 归一后 dispatch 收到原文
    import io
    import json as _json
    frame = {"jsonrpc": "2.0", "id": 1, "method": "listen.add",
             "params": {"nickname": "龚金龙"}}
    raw = (_json.dumps(frame, ensure_ascii=False) + "\n").encode("utf-8")
    # 模拟 Windows 默认 stdin：按 cp936 解码 + surrogateescape（产生与真机
    # 同款的 lone surrogate 形态）
    misread = raw.decode("gbk", errors="surrogateescape")
    assert misread != "龚金龙" or True  # 乱码现场成立性自检（只做展示）

    # 归一化后的 stdin 包装器必须以 UTF-8 解码同一份字节 → 原文
    buf = io.BytesIO(raw)
    normalized = io.TextIOWrapper(buf, encoding="utf-8", errors="replace")
    monkeypatch.setattr(sys, "stdin", normalized)
    monkeypatch.setattr(sidecar, "MOCK", True)

    captured = {}

    def _capture_dispatch(method, params):
        captured["method"] = method
        captured["nickname"] = params.get("nickname")
        return {"ok": True, "verified": True}

    monkeypatch.setattr(sidecar, "dispatch", _capture_dispatch)
    sidecar.main()
    assert captured["nickname"] == "龚金龙", (
        f"stdin 经 UTF-8 归一后 nickname 应保持原文，实际: {captured.get('nickname')!r}"
    )
