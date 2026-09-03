---
type: adr
title: 环境代理不可达时按回放安全性分级回退直连
status: accepted
tags: [network, proxy, fallback, refresh-safety]
intent: Record the shared Kimi HTTP send entry, its env-proxy direct fallback, and the replay-safety classes that keep one-time refresh tokens safe.
documents:
  symbols:
    - net::send
    - net::ProxyFallback
    - request_device_code_at
    - poll_for_token_at
    - refresh_blob_at
    - fetch_official_nickname
    - fetch_quota_at
    - get_subscription_at
---
# 环境代理不可达时按回放安全性分级回退直连

用户级环境常驻 `HTTP(S)_PROXY`/`ALL_PROXY`（本机为 Clash `127.0.0.1:7897`），
GUI 从 Explorer 继承后 reqwest（`default-features = false`，不读 Windows 系统
代理）会把全部 Kimi 请求经本地代理隧道发出；而 reqwest 错误 Display 只剩
`error sending request for url (...)`，用户无法自助定位。2026-09-03 设备码
授权持续报错的实锤根因：环境变量是进程启动时刻的快照，GUI 进程攥着旧值
`127.0.0.1:10808`（无监听死端口），而当前注册表与 Clash 已是 7897——代理
本身健康，旧进程却一直在拨死端口（读进程环境块 + netstat 确认；新起进程
探针全部 200，代理指向死端口可逐字复现报错文案）。代理关闭、重启、切节点
或换端口后的旧进程快照都属同类网络层失败，本 ADR 的回退机制兜住这一整类
场景；彻底消除仍需重启进程拿新快照。

## 决策

1. **统一发送入口 `net::send`**：kimi crate 内全部 6 个发送点（设备码请求、
   设备码轮询、token 轮换、额度 `/usages`、官方昵称 `/me`、会员
   `GetSubscription`）改为经 `crates/providers/kimi/src/net.rs` 发送。第一次
   尝试与历史行为逐字节一致（reqwest 默认 env 代理、各站点原有超时与错误
   构造器不动）。

2. **回退门槛按回放安全性分两级**：
   - `Always`（任何 send 层错误都回退直连）只用于重放安全端点——GET、
     幂等或无副作用的 POST。覆盖「代理活着但出口节点到 Kimi 域名超时」
     的失败形态。
   - `ConnectOnly`（仅 `err.is_connect()` 回退）用于一次性 refresh token 的
     `POST /api/oauth/token`：connect 类错误（TCP/DNS/TLS 握手、代理隧道
     建立失败）都发生在请求字节写出之前，直连重试不可能双消费 token；
     超时等无法排除「已发出」的形态不回退，避免「首次实际成功轮换、
     直连重放被拒 → 误标 DeadToken 锁死账号」。

3. **回退条件**：环境代理变量存在（大小写任一形态非空白）且错误满足策略
   门槛。环境无代理变量时行为与历史完全一致，不做任何额外请求。回退触发
   时打 `tracing::warn`（英文结构化字段，对齐 oauth.rs 既有风格）；两次都
   失败时返回直连尝试的错误。

4. **已知限制**：额度路径 client 无自身超时（外层 20s 预算，见
   quota_query），代理挂起耗尽外层预算时回退不会发生——与历史行为相同，
   非回归。非目标：不新增 Windows 系统代理（WinInet）支持，不改各调用点
   错误文案，不加代理探测常驻逻辑。
