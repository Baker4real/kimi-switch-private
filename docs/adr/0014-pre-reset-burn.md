---
type: adr
title: 重置前抢用：临近重置主动切到快清零账号、烧完自动切回
status: accepted
tags: [auto-swap, pre-reset-burn, round-trip, manual-hold]
intent: Record that auto-swap gains an opt-in pre-reset burn path that swaps to a parked account whose 5h/7d window is near reset and mostly used, then swaps back after the reset passes or the target exhausts, keeping round-trip memory in the auto-swap state file; a manual swap clears that memory and the manual hold blocks new burns.
documents:
  config_keys:
    - pre_reset_burn
    - pre_reset_burn_min_ratio
    - pre_reset_burn_near_5h_ms
    - pre_reset_burn_near_7d_ms
    - manual_hold_ms
  symbols:
    - decide_auto_swap
    - AutoSwapKind
---
# 重置前抢用：临近重置主动切到快清零账号、烧完自动切回

既有自动切换只在当前账号将耗尽（或 pace 预警）时切走；停泊账号窗口快重置而剩余配额
即将清零时，系统不会主动去利用。issue #146 增加可选子开关「重置前抢用」：临近重置且
该窗口已用超过水位的合格停泊账号被主动切上去烧尾巴，烧完经「往返记忆」自动切回原
账号。本 ADR 钉死键定义、临近与水位谓词、判定顺序、防抖边际豁免、往返记忆的写入与
回程规则，以及燃烧期间对其他路径的挂起。

## 决策

1. **键名、默认值、缺省兼容。** `auto_swap.pre_reset_burn`（`bool`）默认 `false`，隶属
   自动切换总开关之下。`auto_swap.pre_reset_burn_min_ratio`（`f64`）默认 `0.7`，加载
   时须为有限数值，并收敛到 `0.0..=1.0`（与 `threshold` / `candidate_margin` 同一套
   区间）。`auto_swap.pre_reset_burn_near_5h_ms` 默认 `3_600_000`（1 小时）；
   `auto_swap.pre_reset_burn_near_7d_ms` 默认 `172_800_000`（2 天）。`config.toml`
   缺这些键时与未配置行为一致。开关进 GUI 行为面板（第四个开关，名「重置前抢用」）；
   三个参数不进 GUI，只在 `config.toml` 调。关闭时 `decide_auto_swap` 不读抢用路径，
   既有表驱动用例原样通过。

2. **临近 = `reset_at − now ≤ 该窗口类型的临近窗口`。** 只认 `QuotaWindow::FiveHour`
   与 `SevenDay`；`Month` / `Custom` / 其它时长未知的窗口不参与（Kimi 适配器本就只产
   5h / 7d / Custom）。缺 `reset_at` 的窗口不参与。与 pace 不同：临近判定不需要窗口
   起点，不用已知窗口时长推算。

3. **触发谓词：临近 ∧ 该窗口已用比例 > `min_ratio`。** 比例取临近窗口自身，不是账号
   最大已用——要烧的就是这个快清零的窗口。最坏情况（刚好过水位）要在重置前烧掉约
   30%：5h 窗最后一小时约 1.5 倍匀速、7d 窗最后两天约 1.05 倍匀速，均在可行区间。

4. **判定顺序：耗尽 → pace → 抢用。** 抢用只在前两者的 Stay 分支里评估，不抢占：
   保命（当前账号将用尽）优先于占便宜（抢烧别人的尾巴）。

5. **候选资格与排序。** 候选先过既有合格线（快照可用、非当前、未摘出、所有已知窗口
   已用 < `threshold`）；**不适用防抖边际**——`candidate_margin` 的语义是「比当前更
   健康 − 边际」，抢用奔着高已用账号去，两者自相矛盾。多候选按最早 `reset_at` 升序
   （与快重置偏好同键），回落既有比较链。无合格候选时静默 Stay，不出耗尽路径的
   「没有可切换的账号」提示（与 pace 同哲学）。

6. **往返记忆。** 触发抢用时把（原账号 id、目标账号 id、目标窗口、记录的 `reset_at`）
   写入 `auto_swap.json` 状态文件，沿用「可丢弃、读改写保留无关字段、失败只 warn」的
   既有形状与 `#[serde(default)]` 兼容惯例；决策侧仿照 `decide_resident_return_from_state`
   做 from_state 门面。记忆跨重启有效。

7. **回程时机取两者任一先到**：快照显示目标窗口 `reset_at` 翻过记录值（额度已刷新，
   没什么可抢了），或目标任一窗口达耗尽阈值。回程是一次普通自动切换：走既有
   `activate`，写 `last_auto_swap_at`，照常受冷却与安顿宽限约束（5 分钟 / 60 秒相对
   1 小时与 2 天的燃烧窗口不会真撞到，不开豁免通道）。

8. **回程资格。** 切回前原账号重过停泊候选合格线（快照可用、未摘出、所有已知窗口
   < `threshold`）；不合格或已被删除则留在目标账号上、清除记忆——覆盖「燃烧期间
   原账号在另一台机器被用尽」这一罕见但可达的路径；随后周期的常规耗尽判定会接管。

9. **燃烧期间 pace 与回驻挂起，不新开抢用，只留耗尽与回程。** 燃烧是有意的停留：目标按定义已用
   > 水位，pace 预警可能命中、回驻可能想切回低档、其它停泊账号也可能临近重置，都违背燃烧意图。
   往返记忆存续时不评估抢用路径。此期间耗尽命中（即决策 7 的第二回程时机）目的地锁定
   记忆中的原账号，不重新排序选候选。

10. **手动切换即接管。** 燃烧期间或回程前任何手动切换清除往返记忆，且手动切换写入的
    手动保持期（ADR-0008 第 4 条，`manual_hold_ms` 默认 8 小时）内不新开抢用（2026-10-24
    修订，见文末）。保持期只挡新开抢用：耗尽、pace 与回程语义不变，届满后抢用照常评估。
    摘出、快照不可用不触发、只读已收集快照不打网等既有约束全部沿用（规则 3）。

11. **「7d 燃烧 ↔ 5h 耗尽」循环是预期行为。** 烧 7 天尾巴时 5 小时窗先撞线 → 回程；
    目标 5h 窗重置后若 7d 仍临近且超水位 → 再次抢用。两天内可能往返多次；5 分钟
    冷却与 60 秒安顿宽限兜住频率。不得当缺陷修掉。

12. **不引入新 I/O。** 判定只读已收集快照；不新增网络请求、定时器或 daemon；执行
    只走既有 `activate`。CLI status 路径与 GUI 快照路径共用同一决策，自动继承。

13. **审计与 JSON 只加字段。** `AutoSwapKind` 增加 `Burn` / `BurnReturn`。抢用成功为
    `kind=burn from=… window=… used=…%`，回程为 `kind=burn_return from=…`。CLI JSON
    事件（#113）在既有 `swap.kind` 上增加 `burn` / `burn_return`；退出码映射不变。

## Considered Options

- **只做排序轴（既有「快重置偏好」）**：`prefer_soonest_reset`（ADR-0011）只在切换已
  触发后排候选，没有「当……的时候」的触发语义。用户要的是主动切过去，故新增触发
  路径而非复用排序轴。两者可同时开启，语义正交。
- **可烧速率上限**（剩余额度 ÷ 剩余时间超过某值则不触发，复用 pace 的已流逝机制）：
  误触发的代价只是白切两次，那笔额度本来也要作废，没有额外伤害；为一个良性结果
  引入新概念不成比例。将来真嫌吵再加。
- **回程豁免冷却**：数值上（5 分钟冷却 vs 1 小时 / 2 天窗口）不会真撞到，不值得开
  豁免通道。

## 修订（2026-10-24）：手动保持期挡新开抢用

原第 10 条只清往返记忆。记忆一清，判定回到无记忆分支；抢用条件若仍成立，60 秒安顿宽限
一过，下一轮快照就会再次抢用，接管名存实亡。真机复现：燃烧期间用户两次手动切回原账号，
目标 7 天窗口距重置不足 2 天、已用 94%～95%（高于水位 0.7、低于阈值 0.99），两次都在手动
切换后 90 秒被 `kind=burn` 切回（审计 2026-10-24 17:55:54、18:01:26，东八区）。根因是保持期
原先只挡回驻（ADR-0008 第 4 条），抢用路径不看它。

处置：第 10 条扩为保持期内不新开抢用，复用既有 `manual_hold_until`，不新增键或状态字段；
判定仍只读 `auto_swap.json` 与已收集快照（决策 12）。保持期内放弃抢用机会（含 5 小时窗
最后一小时这类短窗口）是有意取舍：手动选择优先于占便宜，与决策 4「保命优先于占便宜」
同一方向。届满时条件仍成立则照常抢用；嫌长可调 `manual_hold_ms`（与回驻共用）。

不采用：只拒绝被手动打断的那个目标窗口、直到它重置（「拒绝记忆」）。更精确，但要新增
状态字段与过期规则；保持期已表达「用户接管」，不为此另造一套状态。
