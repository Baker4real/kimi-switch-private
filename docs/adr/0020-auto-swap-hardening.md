---
type: adr
title: 自动切换侧加固：判定口径与并发收敛
status: accepted
tags: [auto-swap, past-reset, earliest-recovery, passive-swap, quota-cache]
intent: Record the auto-swap side of spec 185 (implementation decisions D-F), one section per ticket; the credential side (A-C) lives in ADR-0019. Past reset (issue 191, decision E) - when deciding, a quota window whose reset_at is at or before now counts as already reset (used 0, limit unchanged, next reset unknown) for the current and parked accounts alike, across the exhaustion trigger, candidate eligibility, candidate margin, resident return, burn candidates and burn return; pace and the burn trigger already skipped passed resets, the burn-return flip still needs a later reset_at, and prefer_soonest_reset no longer ranks a passed reset_at first (amends ADR-0011 decision 2). The passive overlay applies the same view to its cached base under one shared now, so the authoritative exhausted window survives (amends ADR-0017 decision 5). The passive path reads the quota cache through get_keeping_past_resets so accounts whose windows have all passed keep a snapshot. The earliest-recovery hint returns only future times through earliest_recovery_at, shared by the GUI toast and the CLI line and JSON. Nothing writes the quota cache or changes displayed numbers; no new network request, config key or state field.
documents:
  symbols:
    - reset_passed_windows
    - earliest_recovery_at
    - overlay_exhausted_snapshot
    - get_keeping_past_resets
    - decide_auto_swap
    - decide_resident_return
---
# 自动切换侧加固：判定口径与并发收敛

spec #185 的自动切换侧（实现决策 D–F）记在本 ADR，按票分节续写：过点重置（#191，决策 E）
先落地；耗尽账本（#192，决策 D）、期望当前账号（#193）与自动切换串行化锁（#194，决策 F）
落地时各补一节。凭证侧（决策 A–C）见 ADR-0019。

## 过点重置（#191，spec #185 决策 E）

GUI 很久没开时，额度缓存停在上次真实查询的结果。缓存里 `reset_at` 已经过去的窗口其实
已经翻篇，账号早已恢复，判定却可能仍把它当成不可用，误报「没有可自动切换的账号」；
「最早恢复」提示也可能给出一个已经过去的时刻。

复现时看到的机制与 spec 问题 3 的推断（「已过窗口仍按旧比例计算」）不完全一致：

- 展示路径的 `QuotaCache::get` 本就丢掉 `reset_at` 已过的窗口，窗口全部过期时整条返回
  `None`。只读缓存的被动路径照用它：部分窗口过点的账号，余下窗口的判定结果恰好与过点
  重置一致；**窗口全部过点的账号整条失去快照**，按「无快照 → 候选资格线不通过」被排除。
  CLI 被动端到端复现用例先红。
- 判定本身不看 `reset_at` 是否已过：快照里带着已过的时刻时（查询与判定之间刚好过点，或
  调用方直接传入），耗尽线与合格线照旧比例算，快重置偏好把它排成最早（ADR-0011 决策 2
  原文），「最早恢复」也照样给出。

### 决策

1. **判定视图。** `decide_auto_swap_with_trip`（耗尽、pace、抢用、回程）与
   `decide_resident_return`（回驻）在入口处取过点重置视图（`reset_passed_windows`），
   之后读快照的判定都只读这份视图：`Ready` 与 `Cached` 快照里 `reset_at` 早于或等于此刻
   的窗口，已用按 0 计、上限不变，`reset_at` 置空（下一次重置时刻未知）；`Failed` 快照
   原样保留。恰好等于此刻也算过点。当前账号与停泊账号同口径。

2. **作用面一视同仁。** 耗尽触发线、停泊候选合格线、防抖边际（比较过点重置后的最大
   已用）、回驻资格（目标合格线，以及当前账号「任一窗口已达阈值则不回驻」）、抢用候选
   合格线、回程（原账号合格线与目标耗尽线）都读这份视图；被动触发进同一判定单点，自然
   继承。其余口径：
   - pace 与重置前抢用触发本就跳过已过 `reset_at` 的窗口（`window_pace` / `burn_window`），
     不变。
   - 回程「翻过记录的重置点」仍只认快照里比记录值更晚的 `reset_at`，过点本身不算翻过。
     抢用目标就是当前账号，轮询拿到的是新窗口的未来时刻，回程时机不变。
   - 快重置偏好：过点窗口置空后不再给出排序键，已过的时刻不再排成最早。**修订 ADR-0011
     决策 2**「已过期的 `reset_at` 仍按时间戳比较」：过点窗口已经重置，没有快过期的配额
     可抢先烧，按陈旧时刻排第一只是数据残留。

3. **只作用于判定与「最早恢复」提示。** 视图是判定内的副本：不写回 `quota_cache.json`，
   不改 GUI 与 CLI 显示的额度数字。不新增网络请求、配置键或状态字段；手动切换不经判定，
   不受影响。

4. **被动 overlay 的耗尽窗口不参与过点重置。** 耗尽原文是「此刻已耗尽」的权威证据，不能
   被陈旧的 `reset_at` 抵消。做法是结构性的：`overlay_exhausted_snapshot` 先对缓存底按
   `now` 过点重置，再覆写耗尽窗口。
   - 文案点名的窗口若 `reset_at` 已过，那个时刻属于已翻篇的旧窗口，随底一起置空；覆写后
     的耗尽窗口不带已过的时刻，判定的过点重置抹不掉它。
   - 文案辨认不出窗口时，过点窗口按已用 0 参与「已用最高」的挑选：靠陈旧比例挑中一个
     已经翻篇的窗口，不如仍在当期、已用最高的窗口可信。
   - 被动子命令里 overlay 与判定共用同一个 `now`，两处划线一致。

   **修订 ADR-0017 决策 5**「其余窗口保留缓存真值」为「其余窗口保留过点重置后的缓存
   值」；overlay 仍是纯函数、不写额度缓存。spec 决策 E 规定覆写窗口的恢复时刻以耗尽账本
   为准，由 #192 续写。

5. **被动路径读缓存保留过点窗口。** 新增 `QuotaCache::get_keeping_past_resets`：与 `get`
   相同，但保留 `reset_at` 已过的窗口（原样返回缓存值，交给判定过点重置）；缺 `reset_at`
   的窗口仍按兜底 TTL 失效。被动路径的 `cached_quota_snapshot` 改用它。`get` 与 `fresh`
   不变，GUI 行与 CLI 列表的数字照旧。轮询路径的可用停泊快照来自真实查询或节流窗内的
   缓存：`get` / `fresh` 丢掉过点窗口后，余下窗口与过点重置等价；窗口全部过点的账号在
   下一次真实查询后回到候选。

6. **「最早恢复」只给未来时刻，一处计算。** 新增 core `earliest_recovery_at(snapshots,
   now)`：在过点重置视图上，取可用快照（`Ready` / 非 stale `Cached`）窗口里最早的
   `reset_at`。视图已把过点时刻置空，结果必在未来；没有就返回 `None`，提示不写时刻。
   GUI 无候选 toast、CLI 无候选说明行与 `--json` 的 `earliestReset` 由调用方各算一次、
   共用这一个函数；原 `quota_snapshot::earliest_reset_at`（不看此刻）删除。范围沿用
   #113 起的既有口径：全部可用快照，含当前账号——所有停泊账号都不可用时，当前账号自己
   的重置往往就是最先到的恢复。spec 决策 E 的措辞是「各停泊账号」，本票不改范围。耗尽
   账本（#192）的恢复时刻并进这一处。

### 被拒方案

- **只把已用清零、保留已过的 `reset_at`**：pace、抢用与回程本就跳过已过时刻，但快重置
  偏好会把它排成最早，「最早恢复」也得另行过滤。置空让「下次重置未知」只有一种表示，
  各处按缺 `reset_at` 统一处理。
- **在判定里直接丢掉过点窗口（与 `QuotaCache::get` 同口径）**：窗口全部过点的账号没有
  已知窗口、资格线不通过，正是要修的误报。
- **改 `QuotaCache::get`，全局保留过点窗口**：GUI 行与 CLI 列表也读 `get`，陈旧的已用比例
  会重新显示出来，违背「不改界面上的额度数字」。只给只读缓存的被动路径开一个保留过点
  窗口的读法。
- **给 overlay 覆写的窗口打豁免标记**（`Quota` 新字段或旁路集合）：要改公开数据结构，
  还要把标记穿过判定各处。overlay 先过点重置底、再覆写，加上同一个 `now`，权威证据已能
  在视图里存活。
- **过点即视为回程「翻过记录的重置点」**：spec 明定回程逻辑不变；抢用目标就是当前账号，
  轮询会拿到新窗口的未来 `reset_at`，回程时机本就可靠。
- **快重置偏好改用带 `now` 的键（已过的时刻排最后或忽略）**：要给比较链穿 `now`；视图
  置空后，既有「未知不占优先位」（ADR-0011 决策 3）已给出同样结果。

### 验收

- 判定单点（`crates/core/src/auto_swap.rs` 测试）：`decide_auto_swap_past_reset_table`
  （停泊账号过点重新合格、恰好等于此刻也算、当前账号耗尽窗口过点不再触发、防抖边际、
  快重置偏好、抢用候选合格线与抢用触发）、`decide_auto_swap_burn_return_past_reset`
  （回程）、`decide_resident_return_past_reset`（回驻）、
  `earliest_recovery_at_skips_passed_resets`、`earliest_recovery_at_counts_only_usable_windows`。
- 被动 overlay 豁免：`crates/core/tests/passive_auto_swap.rs` 的
  `passive_overlay_window_is_exempt_from_past_reset`（文案点名的窗口与辨认不出窗口两种）。
- 缓存读法：`get_keeping_past_resets_keeps_reset_passed_windows_only`。
- CLI 被动端到端（tempdir 入口、真实 activate）：
  `passive_swaps_to_account_whose_cached_exhaustion_already_reset`——当前账号撞上额度
  耗尽，另一账号缓存显示耗尽但 `reset_at` 已过，切到该账号；额度缓存字节不变，观察型
  内核不向上报错（入口恒退出码 0）。
- 提示同口径：CLI `status_auto_swap_no_candidate_skips_passed_reset`（说明行与
  `earliestReset`）、GUI `snapshot_auto_swap_no_candidate_skips_passed_reset`（toast）。
