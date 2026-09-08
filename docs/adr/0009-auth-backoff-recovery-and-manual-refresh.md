---
type: adr
title: 鉴权退避的恢复信号与手动刷新放行口
status: accepted
tags: [quota, auth-backoff, credential-expired]
intent: Record the two auth-backoff bypasses (one-shot live-mtime recovery signal and UserInitiated direct query except 429), the mtime-over-fingerprint choice, and the exclusions.
documents:
  symbols:
    - credential_changed_since
    - QuotaCollectReason
    - record_failure
    - silentQuotaIntervalMs
---
# 鉴权退避的恢复信号与手动刷新放行口

当前账号额度查询读 live 凭证、默认不主动续期（ADR-0003）。access token 约 15 分钟过期后，官方用量接口返回凭证过期型 401，卡片进入「凭证待续期」。用户按引导在 Kimi CLI 发消息，官方客户端会重写 Kimi Code 当前登录文件（即「凭证恢复信号」），但鉴权失败退避（90s）原先连手动点「刷新」也跳过真实额度查询，再叠加 90s 静默轮询，实测滞后约 4 分半。本 ADR 钉死鉴权退避新增的两个放行口、一次性规则、mtime 取舍与排除项。术语见 CONTEXT.md「凭证待续期」「凭证恢复信号」。前置实现 #102 / #103 / #104。

## 决策

1. **凭证恢复信号放行。** `Provider::credential_changed_since` 默认返回 false。`FileBlobProvider` 只对当前账号比较当前登录文件 mtime 是否严格晚于该账号最近一次失败的 `failed_at`；停泊账号、未知账号、读不到 live mtime 恒为 false。计划内收集（`QuotaCollectReason::Scheduled`：启动、静默、CLI 等）在鉴权退避跳过前检查该信号，有信号则对该账号发起真实额度查询。429 与超时/网络失败不走此口。kimi 适配器不另写实现。

2. **手动刷新直查（429 除外）。** 用户点「刷新」（`QuotaCollectReason::UserInitiated`）对鉴权失败（401 / 403 / 凭证待续期）忽略退避、立即真实额度查询；答复仍为 401 则保持凭证待续期，失败记录按现有语义更新。429 限流退避仍跳过。超时/网络等既有「用户刷新可重试」语义不变。此口每次点击都直查，不是一次性。

3. **恢复信号一次性。** 放行后的真实查询若仍失败，`record_failure` 刷新 `failed_at`。比较是严格晚于：同一 mtime 不再满足 `mtime > failed_at`，账号回到 90s 鉴权退避，等下一次当前登录文件重写。查询成功则清掉失败记录。不新增失败记录字段。

4. **信号实现选当前登录文件 mtime，不选 token 指纹。** 官方 token 轮换的可观察事件就是重写当前登录文件，mtime 与该恢复事件因果对齐；检查只读本地文件 mtime，不读 token。`quota_cache.json` schema 不变。指纹方案要在失败记录里持久化 token 指纹，精度过剩且引入迁移。

5. **明确排除。** 429 限流退避语义不动。停泊账号无恢复信号，恢复路径仍是切换与重新授权。主动续期默认关，不改 ADR-0003。

## 后果（已接受）

GUI 快照含凭证待续期行时，静默轮询间隔改为 5s，否则 90s。无恢复信号时这些 tick 只做本地 mtime 检查，计划内收集仍跳过真实查询，不增加用量接口请求。CLI 无持续轮询，但共享收集逻辑同样获得恢复信号放行。不引入 file watcher。
