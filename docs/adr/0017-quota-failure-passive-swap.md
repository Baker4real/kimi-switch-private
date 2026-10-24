---
type: adr
title: 配额失败被动切换：StopFailure hook 秒级唤起，403 原文即权威耗尽证据
status: accepted
tags: [auto-swap, passive-swap, stop-failure-hook, install-hook, credential-health]
intent: Record the quota-failure passive swap trigger (spec #152 decisions 14-17): Kimi Code 0.42.0 provides the official StopFailure hook channel, reversing CONTEXT.md's old avoidance of "swap on 403"; an observer-only CLI subcommand always exits 0, classifies "usage limit" as a case-insensitive substring plus a word-boundary 403 token (official copy is "You've reached your ... usage limit"), overlays the authoritative exhaustion evidence without touching quota cache, inherits the exhaustion decision semantics via a dedicated serialization lock, installs the hook only through an idempotent install-hook subcommand, and narrows the local credential health check to invalid_grant fingerprints only (JWT exp excluded by an explicit, reversible ruling). Live 403 is manual acceptance and does not block release. Ruling change on 2026-10-24 (issue 184) - the official error reference moved quota exhaustion to HTTP 429, so a 429 signal now also matches when one string contains both "reached your" and "usage limit"; legacy 403 plus usage limit is kept, other 429s never match, and live 429 is manual acceptance.
documents:
  symbols:
    - classify_stop_failure
    - overlay_exhausted_snapshot
    - decide_auto_swap_passive_from_state
    - PassiveSwapLock
    - install_hook_into
    - locally_invalid_accounts
---
# 配额失败被动切换：StopFailure hook 秒级唤起，403 原文即权威耗尽证据

CONTEXT.md「自动切换」词条长期把「按 403 被动换号」列为回避词：历史上仓库明确不做这件事，
当时的理由是 Kimi Code 尚无官方 hook 通道——工具无从得知「此刻真的撞了 403」，只能靠轮询
被动发现，自己拦截请求做被动换号既无官方依据也违背「判定只读已收集快照」。**这一回避现在
反转**：Kimi Code 0.42.0 起官方支持 hooks（事件 JSON 走 stdin、可执行任意外部命令），其中
`StopFailure` 在本轮因错误失败时触发，观察型、不可阻断。官方通道使「403 即时唤起切换」从
无据抓取变成官方支持的观察路径。规范词定为「配额失败被动切换」（CONTEXT.md 词条），取代
旧回避词。spec #152 决策 14–17（实现 #158 / #162）。

关键事实（spec 已核实）：配额耗尽 = HTTP 403，5 小时窗 / 7 天窗 / 月度各有
"You've reached your ... usage limit" 文案，官方明示重试无意义、会话不销毁；429 仅为瞬时限流/引擎过载，与配额无关。
（2026-10-24 起官方已把额度耗尽改为 429，见文末「裁定变迁」。）

## 决策

1. **触发链路：官方 `StopFailure` hook 唤起 CLI 观察型子命令（#162，spec 决策 14）。**
   新子命令 `on-quota-failure`：从 stdin 读事件 JSON → 分类 → 命中 403 配额耗尽时按耗尽
   路径语义复用既有自动切换判定与 `activate`。**恒以退出码 0 结束**——观察型 hook 绝不
   阻断用户会话；任何内部错误（含上下文构建失败、stdin 读失败）只写日志。stdin 畸形静默
   放行。hook 唤起的是独立 CLI 进程，顺带覆盖「GUI 没开」场景：用户在 Kimi Code 活跃会话
   期间即使没开 GUI 也有兜底。

2. **总开关沿用既有语义。** `auto_swap.enabled` 关闭时被动路径同闸、零副作用——被动切换是
   自动切换的触发源，不是独立功能。

3. **分类：403 ∧ "usage limit" 子串，词边界只防 403 误报（#162）。** 纯函数
   `classify_stop_failure` 对整份事件 JSON 递归扫描：任一字符串含 "usage limit"
   （大小写不敏感**子串**，不是词边界）**且**存在 403 信号——数值 403，或任一字符串含
   **作为独立 token** 的 "403"（前后均非 ASCII 字母数字才算边界；时间戳、request id、
   行号等数字串里的 "403" 子串不算）。词边界只约束 "403"，不约束 "usage limit"。官方现役
   配额 403 文案形如 `You've reached your 5-hour usage limit...`，仍含该子串；匹配器
   不另认 `You've reached` 前缀。两者缺一归其他：429 过载/瞬时限流绝不匹配。官方未枚举
   事件 JSON 的错误字段，因此不依赖 hook 的 matcher 做过滤，而是在子命令内部对 stdin
   原文分类——hook schema 变化最坏只会**漏**触发，不会**误**触发。非合法 JSON 一律归其他。
   **2026-10-24 起本条的「429 绝不匹配」由文末「裁定变迁」取代。**

4. **窗口辨认是尽力而为。** 从含 "usage limit" 的字符串辨认耗尽窗口（"month" → 月度；
   "5"+"hour" → 5 小时；"7"+"day" → 7 天），辨认不出为 None——只影响权威证据标注哪个
   窗口，不影响切换与否。

5. **403 原文 = 权威耗尽证据，overlay 不回写缓存（#162）。** `overlay_exhausted_snapshot`
   以快照形态把「当前账号已耗尽」证据送进判定：缓存快照可用（`Ready` / 非 stale
   `Cached`）时以其为底，把目标窗口覆写为已用 = 上限，其余窗口保留缓存真值；底上没有该
   窗口则追加一条合成窗口；无可用缓存时整条合成（缺省 5 小时、`reset_at` 未知）。纯函数、
   不落盘、**不写 `quota_cache.json`**——被动路径零新增网络查询，被切走账号的真实额度仍由
   后续快照照常跟踪。

6. **耗尽路径语义继承，单点保证（#162）。** `decide_auto_swap_passive_from_state` 与轮询
   路径进同一判定实现（触发来源只影响安顿宽限是否放行）：冷却、合格线、防抖边际、摘出、
   回驻确认期、往返记忆、候选本地凭证快检（见决策 9）全部继承，不另写一套。与轮询路径的
   唯一差异是**安顿宽限不阻挡**——403 原文是「此刻已耗尽」的权威证据，等宽限期过去只会
   多失败几轮请求；手动保持期本就只约束回驻、不约束耗尽路径（ADR-0008）。本应触发但无
   合格候选时记审计（与轮询同口径）；冷却、无当前账号等其余 Stay 静默落空（观察型不打扰
   会话）。activate 段计时同口径写入审计 `duration_ms`（ADR-0015）；成功审计 detail 带
   `trigger=passive`。

7. **决定之后、activate 之前重读防线 + 被动串行化锁收敛并发（#162）。** 决策返回 Swap 后、
   真正 activate 前重读当前账号与冷却：并发的 GUI/CLI 轮询切换已完成时放弃，不重复
   activate、不审计。多个会话同时撞 403 由 `<state_dir>/passive-swap.lock` 排他文件锁
   串行化（独立于交换锁 `registry 目录/.kimi-switch.lock`，二者无嵌套；等待上限对齐
   3 秒，等不到视为另一触发处理中、观察型放弃）：锁把「读状态 → 判定 → activate → 写
   冷却」整段排成队列，后到者在锁内重读状态，看到冷却或新当前账号健康即自然落空——没有
   这把锁时，[提交, 写冷却] 间隙里迟到的触发会把 403 证据错按在新当前账号上再切一跳。

8. **hook 安装只经 CLI `install-hook` 子命令，幂等、可卸载（#162，spec 决策 16）。** 向
   Kimi Code 工作目录（`KIMI_CODE_HOME` > `~/.kimi-code`）的 `config.toml` 写入 `[[hooks]]`
   条目：`event = "StopFailure"`、`command = "<当前 exe> on-quota-failure"`、
   `timeout = 10`（官方默认 30s；被动切换纯本地毫秒级，10s 兜底即可，不让 hook 拖慢会话）。
   工具任何其他路径不得隐式修改 Kimi Code 配置——hook 只通过用户显式执行安装命令写入。
   幂等与卸载：识别标记是 `event = StopFailure` 且 `command` 含 `on-quota-failure` 的条目
   视为本工具条目；重复安装只刷新 command（可执行文件可能移动过）与 timeout，并把多余的
   重复条目收敛成一个；`--uninstall` 只删自己的条目，数组清空时连 `hooks` 键一起移除、
   还原到没有 hooks 段的原始形态，用户其它 hooks 条目与注释、顺序经 toml_edit 保留；
   解析失败报错退出、绝不覆写；无变化时不重写文件（字节不变）。

9. **候选本地凭证快检：只认 invalid_grant 指纹（#158，spec 决策 15；口径经 #158 续跑裁定
   收紧）。** 自动切换候选过滤新增纯本地快检：refresh token SHA-256 指纹命中本地拒止集合
   文件 `<kimi_home>/credentials/.kimi-switch-dead-refresh`（invalid_grant 被官方拒止时由
   token 刷新协调写入，ADR-0009 同族）的停泊账号，直接排除出全部自动触发路径（耗尽 /
   pace / 回驻 / 重置前抢用与回程 / 被动），避免切到死卡。**裁定（偏离 spec 决策 15 原措辞
   「JWT 已过期或有 invalid_grant 指纹」，明示可推翻）：只认指纹命中，不认 JWT exp 已过**，
   理由：
   - 停泊账号 access token 只在额度轮询时轮换保鲜，GUI 长期关闭后全部自然过期——恰是
     被动切换的主场景，exp 口径会把全部候选排空、被动切换静默落空；
   - access 过期但 refresh 有效的账号激活后下次额度查询自愈，不是真死卡；
   - 与 ADR-0009「无法确证不算失效」同向：unknown ≠ 失效，任何读取/解析失败 fail-open。
   手动切换死卡提示（ADR-0015 决策 10）复用同一口径与共享原语（core `credential_health`）。
   口径记录在 `crates/core/src/credential_health.rs` 模块 doc；若未来恢复时间依赖口径，
   门面参数已保留决策基准时刻，可重开本裁定。

10. **不引入第二触发源（spec 决策 17）。** SessionHeartbeat 等常驻信号不做：hook 已覆盖
    「工具未运行」场景，第二个触发源只增加状态机复杂度。官方客户端采纳段（切换何时被
    Kimi Code 会话真正使用）不进保证范围、不设计观测，至多由写后回读校验
    （ADR-0015 决策 5）间接压缩风险。

## 真机验收

真机 403 正路径按 2026-09-14 用户决定改为**手动验收，不阻塞** #162 / #163 关闭。
其余项在 2026-09-13 发布链与 2026-09-14 复验中已回填。自动化正路径与并发以 tempdir
端到端测试为可观察背书。

**未做**：向真机会话或真机 `KIMI_SWITCH_HOME` 注入合成 403（会真实换号）；也未烧配额
制造 403。真机会话 403 尚未自然发生，留待之后手动验收。

1. **release CLI（通过）**：`target/release/kimi-switch-cli.exe` 存在（2026-09-13 发布链
   重建；2026-09-14 复验文件仍在，约 7.0 MiB）。hook `command` 指向该绝对路径。
2. **总开关（通过）**：`%APPDATA%\kimi-switch\kimi-switch\config\config.toml` 为
   `auto_swap = { enabled = true, ... }`。
3. **install-hook（通过，保持安装）**：`~/.kimi-code/config.toml` 在既有三条用户 hook
   之后新增一条 `[[hooks]]`：`event = "StopFailure"`、`command` 指向 release CLI 的
   `on-quota-failure`、`timeout = 10`。2026-09-13 记录原有 90 行配置零改动、重复安装
   幂等；2026-09-14 复验条目仍在，未执行 `--uninstall`（避免卸掉已武装的兜底）。
4. **真机 403 正路径（未发生）**：`audit.log` 共 31 条 `auto_swap`、**0** 条
   `trigger=passive`。126 份会话日志里 "usage limit" + 403 共 32 行，最新一条
   `2026-09-11T13:45:33Z`（hook 安装于 2026-09-13 之前）。tempdir 正路径：
   `passive_403_swaps_via_real_activate_despite_grace_and_hold`（live 切到候选、
   审计 `trigger=passive`、安顿宽限与手动保持期不挡）。
5. **负向 429 / 畸形 stdin（通过）**：2026-09-13 与 2026-09-14 均向**已安装** release
   CLI 喂 429 事件与畸形/空 stdin，退出码 0、stdout/stderr 空、`audit.log` 字节与
   SHA-256 不变。
6. **并发（测试背书，未做真机多会话）**：
   `passive_concurrent_triggers_converge_without_oscillation`（5 触发恰 1 切）。
7. **uninstall（未在真机执行）**：tempdir 覆盖卸载与保留用户其它 hook；真机保持
   安装，待 403 正路径发生后再考虑还原。

## Considered Options

- **维持回避（不反转）**：官方已提供 hook 通道，回避的理由（无官方依据）不复存在；而
  403 是官方明示「重试无意义」的终态，被动切换是唯一正确的即时响应，轮询加密
  （ADR-0016）再快也有秒到分钟级的残余间隙。
- **依赖 hook 的 matcher 在 Kimi Code 配置里预过滤**：官方未枚举 StopFailure 错误类型的
  matcher 取值，依赖它等于把分类建立在未核实的行为上；分类放子命令内部对 stdin 原文做，
  schema 变化最坏漏触发不误触发。
- **并发收敛靠冷却单独兜住**：冷却写在 activate 返回之后，[提交, 写冷却] 间隙里迟到的并发
  触发会把 403 证据错按在新当前账号上再切一跳；被动锁把整段排成队列，冷却才有干净的
  判定起点。
- **快检认 JWT exp（spec 原措辞）**：见决策 9 裁定——exp 口径会在被动切换主场景排空全部
  候选，把新功能变静默空转；裁定可推翻，推翻时须先解停泊账号自然过期问题。

## 裁定变迁（2026-10-24，#182 / #184）：额度耗尽改认官方 429

**推翻决策 3 与 spec #152 决策 14 的「429 绝不匹配」。** 重开的原因是官方语义变了：额度
耗尽本身改成了 429。

- 官方[错误参考](https://www.kimi.com/code/docs/kimi-code/error-reference.html)（2026-10-24
  访问）把 5 小时、7 天（weekly，仅老会员）和月度额度耗尽统一归为 HTTP 429，前缀为
  `error, status code: 429, message:`，文案仍是固定的 "You've reached your … usage limit"：
  `You've reached your 5-hour usage limit.`、`You've reached your weekly (7-day) usage limit.`、
  `You've reached your monthly usage limit for this billing cycle.`。并发上限
  （`You've reached your concurrent request limit.`）、too many requests、engine overloaded
  是另外几类 429；403 改为套餐权益类错误。
- 本机会话日志对得上：2026-10-08 还是 `403 … APIStatusError`；2026-10-24 12:39:42（东八区）
  已是 `429 You've reached your 5-hour usage limit`（`APIProviderQuotaExhaustedError`，
  Kimi Code CLI 2.3.0，2026-10-21 发布）。旧分类器只认 403，把这次 429 静默放行，只打
  debug 日志，审计里没有任何痕迹。

**新口径。** `classify_stop_failure` 仍对整份事件 JSON 递归扫描、大小写不敏感、不依赖官方
未文档化的字段名，满足任一条即判耗尽：

1. **官方 429**：同一个字符串同时含官方耗尽措辞 "reached your" 与 "usage limit"，且存在
   429 信号——数值 429，或作为独立 token 的 "429"（词边界规则与 403 相同，时间戳、
   request id 等数字串里的子串不算）。
2. **旧口径 403（保留兜底）**：任一字符串含 "usage limit"，且存在 403 信号（决策 3 原规则）。

其余一律归其他、静默放行，宁漏勿误：429 只提 usage limit 而无 "reached your"、
"reached your" 与 "usage limit" 分在两个字符串、并发上限、too many requests、engine
overloaded、403 的非配额文案、没有状态信号的耗尽文案。原决定「429 绝不匹配」的本意是防误
触发；429 不再只代表瞬时限流之后，这个本意改由「同一字符串须含官方耗尽措辞」守住：只认
"usage limit" 会把 `usage limit window busy` 一类 429 当成耗尽，只认 "reached your" 会把
并发上限当成耗尽。窗口辨认（决策 4）规则不变，已覆盖 5-hour、"weekly (7-day)" 与
"monthly … for this billing cycle"。

**不变。** 决策 1、2、5–8 的编排原样继承：恒退出码 0、被动串行化锁、overlay 不回写额度
缓存、耗尽路径语义（安顿宽限不阻挡）、审计写 `trigger=passive` 或 `no eligible candidate`、
零新增网络请求；不新增配置项，hook 仍只经 `install-hook` 安装。上文的「403 原文」「撞
403」一律读作「额度耗尽原文」「撞上额度耗尽」。

**验收。** tempdir 端到端：`passive_429_usage_limit_swaps_via_real_activate`（真实 activate
切到合格停泊账号、恰好一条 `trigger=passive` 审计）、`passive_429_no_eligible_candidate_keeps_live`、
`passive_non_exhaustion_429_is_silent`；分类表在 `crates/core/tests/passive_auto_swap.rs`。
真机 429 正路径沿用上文先例，手动验收、不阻塞关闭：重建 release CLI 后，下一次自然撞上
429 额度耗尽时，审计里应出现 `trigger=passive` 的 auto_swap 记录；不向真机会话注入合成
429，也不故意烧额度。官方 [hooks 文档](https://www.kimi.com/code/docs/kimi-code-cli/customization/hooks.html)
仍没有列出 StopFailure 的 payload 字段，Kimi Code 2.3.0 的真实 StopFailure 样本尚未核实；
真机没命中时，以真实样本为准补规则。
