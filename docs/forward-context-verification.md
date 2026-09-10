# msg.forward 上下文坑——真机验证清单

> 2026-09-10 P1 交付物。背景：参考项目 SiverWXbot 的深度注释指出
> 「转发必须在 add_chat_to_listen 之前执行——msg.forward() 依赖主窗口
> 上下文，加入监听后上下文切到子窗口会失效」（wxbot_core.py:4714-4715）。
> 本项目生产环境监听恒处于注册态（init/connect 双路径 resync），所有
> forward 都运行在「已注册监听」环境下。代码已有缓解但未经验证：
> `_msg_forward`（sidecar-python/methods.py）池直取路径先
> `wx.ChatWith(who=target)` 再 `msg.forward(target)`，docstring 自述
> 「切回主窗口上下文」与 ChatWith(target) 实际语义（切到目标会话）有偏差。

## 风险假设

H1（缓解等价性）：ChatWith(target) 预切换后 forward 不受监听子窗口
    上下文影响——**未验证**。
H2（locate 路径扰动）：_locate_msg 内 GetAllMessage 遍历（UIA 读取）
    期间监听回调扰动使定位到的 msg 控件引用失效——**未验证**。
H3（quote 同族）：_msg_quote 的 msg.quote() 无 ChatWith 预切换，若
    forward 的上下文约束同样适用于 quote，此处未设防——**未验证**。

## 链路 A：池直取 forward（msgId 路径）

**前置条件**
- Windows 真机，微信已登录，desktop App 已连接（Ready 态）
- 监听名单 ≥1 个群（如「测试群1」），群里另一账号发一条文本消息
- 面板「指令日志」tab 可见；sidecar 运行日志可见

**步骤**
1. 群里发消息 M（等待消息流水出现该消息——确认回调路径已把 M 入池）
2. 立即（60s 池 TTL 内）经面板「手动执行」下发 forward_message：
   `{"msgId": "<消息流水里的 msg_id>", "target": "文件传输助手"}`
   （或经服务端 agent 指令下发同 action）
3. 观察微信客户端：文件传输助手会话是否收到 M 的转发

**预期**：转发成功，指令日志 ok=true，无 UIA 异常。

**失败特征与裁决**
- `LookupError: Find Control Timeout` → H1 不成立：ChatWith(target)
  预切换不等价主窗口上下文。修复方向：ChatWith 切**源会话**
  （methods.py `_take_msg` 返回的 `_chat_who`——当前取了弃用，是
  现成修复钩子）后 sleep 1 再 forward。
- 成功但慢（>5s）→ 记录耗时，评估 sleep 1 是否需加长。

## 链路 B：locate forward（无 msgId 路径）

**前置条件**：同链路 A，但操作前先在群里连续发 3+ 条消息（制造
GetAllMessage 遍历期间的监听回调活动）。

**步骤**
1. 不带 msgId 下发 forward_message：
   `{"sourceWho": "测试群1", "match": "<M 的内容片段>", "target": "文件传输助手"}`
2. 观察同链路 A 第 3 步。

**预期**：定位成功且转发成功。

**失败特征与裁决**
- 「未定位到要转发的消息」但消息确实存在 → 定位本身失败（match
  精度问题），与上下文坑无关，另行处理。
- 定位成功但 forward 抛 UIA 超时 → H2 成立：遍历期间回调扰动使
  控件引用失效。修复方向：locate 后补 ChatWith(who=sourceWho)+sleep 1
  再 forward（与池直取路径同款缓解）。

## 链路 C（顺带观察）：quote 无预切换

**前置条件**：同链路 A。

**步骤**
1. 60s 内下发 quote_message：
   `{"msgId": "<M 的 msg_id>", "text": "收到"}`
2. 观察群内是否出现引用回复。

**预期**：成功。若失败抛 UIA 超时 → H3 成立，同款 ChatWith 预切换
补进 _msg_quote。

## 结果记录

| 链路 | 日期 | 结果 | 失败特征 | 裁决 |
|------|------|------|----------|------|
| A | | | | |
| B | | | | |
| C | | | | |

（验证完成后回填本表；任一 H 成立则开新修复任务，勿在此清单内顺手改代码。）
