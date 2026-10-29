---
type: adr
title: 自动切换可选先烧快重置排序轴
status: accepted
tags: [auto-swap, reset-at, prefer-soonest-reset]
intent: Record that exhaustion auto-swap may optionally rank eligible parked accounts by the earliest known snapshot reset_at, defaulting off, after tier preference and before the existing usage-ratio chain, without changing eligibility, trigger, or resident return.
documents:
  config_keys:
    - prefer_soonest_reset
  symbols:
    - decide_auto_swap
    - compare_candidates
---
# 自动切换可选先烧快重置排序轴

额度耗尽自动切换默认按合格停泊账号的已用比例升序选号。issue #109 增加可选子开关
`prefer_soonest_reset`（默认关）：开启后，在档位偏好之后、既有比较链之前，按快照
`reset_at` 从早到晚排序，让快过期的配额先被消耗。本 ADR 钉死键定义、未知语义、与
`prefer_lower_tier` 的组合顺序，以及明确不采用「单位时间可消耗量」公式。

## 决策

1. **键名、默认值、缺省兼容。** `auto_swap.prefer_soonest_reset`（`bool`）默认 `false`。
   `config.toml` 缺此键时与未配置行为一致。不进 GUI，只在 `config.toml` 调。关闭时
   `compare_candidates` 不读取 `reset_at`，既有表驱动用例原样通过。

2. **账号级排序键 = 已知窗口上最早的 `reset_at`。** 只读已收集快照，不新增网络请求。
   已知窗口指 `limit > 0` 的额度窗口。账号的快重置键是这些窗口里 `Some(reset_at)` 的
   最小值（时间戳升序，越早越优先）。某一窗口缺 `reset_at` 则忽略该窗口，不把整个账号
   打成未知。账号在该键上未知，当且仅当没有任何已知窗口给出 `reset_at`。`limit = 0`
   的窗口（窗口未知）即使带 `reset_at` 也不参与。不按窗口类型（5h / 7d）单独取键：
   通常 5 小时窗更早，7 天窗若更早则自然胜出。已过期的 `reset_at` 仍按时间戳比较
   （更早即更优先）；缓存层会丢弃已过期窗口，决策里几乎只见到未来时刻。
   **#191 起本句由 ADR-0020「过点重置」决策 2 取代：判定读过点重置视图，已过的
   `reset_at` 置空、按缺 `reset_at` 处理，不再排成最早。**

3. **未知不占优先位，回落既有比较链。** 比较规则：双方已知则较早者优先；一方已知一方
   未知则已知者优先；双方未知则该键相等，回落已用比例 → `last_used_at` → 账号 id。
   缺 `reset_at` 不得被当成「最早」，以免错烧数据不全的账号（宁稳不错烧）。

4. **组合顺序：先档位，后重置时间，再既有比较链。** 与 `prefer_lower_tier` 同开时：
   会员档位（ADR-0006）→ 本 ADR 的快重置键 → 最大已用比例 → `last_used_at` → 账号 id。
   只开本开关、未开档位偏好时，跳过档位键，从快重置开始。这保持「便宜先用」为最高准则。
   不采用 remaining / (`reset_at` − now) 一类「单位时间可消耗量」：它把剩余额与时间混成
   一个数，与已用比例轴重叠，也偏离「先烧快过期配额」。

5. **只改耗尽路径候选排序。** 不改触发线、停泊资格线（threshold）、防抖边际
   （ADR-0010）、冷却、安顿宽限、手动保持期。手动切换不受影响。
   `decide_resident_return` 不套用本键：回驻同档仍走最大已用比例 → `last_used_at` →
   账号 id（ADR-0008）。
