---
type: adr
title: 回驻：非耗尽触发的档位偏好自动切换
status: accepted
tags: [auto-swap, membership, resident-return, manual-hold]
intent: Record why an auto switch may fire without exhaustion, the strict-lower-known-tier predicate, no-tier exclusion, the 8h manual hold, and the shared cooldown.
documents:
  config_keys:
    - resident_lower_tier
    - manual_hold_ms
  symbols:
    - decide_auto_swap
    - membership_tier_rank
---
# 回驻：非耗尽触发的档位偏好自动切换

耗尽自动切换（ADR-0006 及其排序键）只会单向逃亡：被迫切到高档账号后，低档账号额度恢复也不会回去；`prefer_lower_tier` 在两账号库里只有一个候选、形同虚设。本 ADR 钉死新增子行为「回驻」的立场取舍：这是本工具第一个**非耗尽触发**的自动切换。

## 决策

1. **独立子开关，默认关。** `auto_swap.resident_lower_tier` 默认 false；`prefer_lower_tier` 语义不变（仍只是耗尽路径的候选排序键），回驻的档位排序也不依赖它。判定只在既有额度收集完成后进行，不新增网络请求、定时器或 daemon。

2. **触发谓词：未耗尽 + 双方档位已知 + 严格更低 + 沿用合格线。** 当前账号快照可用且所有已知窗口已用比例 < 自动切换阈值；存在停泊账号满足同样可用性、所有窗口 < 阈值、且原始 `/usages` 档位序号严格低于当前账号。目标取档位最低者，同档回落现有比较链（最大已用比例 → `last_used_at` → 账号 id）。耗尽路径零改动且优先于回驻。

3. **无档位语义：不比较、不作为目标。** 序号 5（`None` / 未识别，见 ADR-0006 排名表）的停泊账号永不作为回驻目标；当前账号无已知档位则完全不回驻——把「未知」当作「不宜比较 / 不宜离开」，而不是排序表里的最低档。动机：骑在会员到期的免费号上时被切走去烧付费号，不符合「便宜先用」直觉。耗尽路径的排序语义不变。

4. **手动决定优先：8 小时保持期。** 任何手动切换（GUI 切换 / CLI swap）成功后写 `manual_hold_until = now + manual_hold_ms`（默认 8 小时；该键只在 config.toml，GUI 不暴露）。保持期只挡回驻，不挡耗尽被迫切换——否则会被锁死在耗尽账号上。提前恢复策略：手动切回低档即可。

5. **共享冷却、静默无目标。** 回驻与耗尽切换共用 `last_auto_swap_at` 冷却（默认 5 分钟）与同一状态文件；状态文件升级为读改写以同时保留两个时间戳。审计沿用 `auto_swap` 动作、细节加 `kind=return`。无合格回驻目标时静默跳过——回驻是机会行为，不套用耗尽路径的「没有可切换的账号」提示。

## 后果（已接受）

开关打开后约一个静默周期（≤90s）内就会离开当前高档账号；低档 5 小时窗口「烧穿 → 被迫切高档 → 恢复 → 回驻」会形成约每 5 小时一轮的双向切换；骑高已用（如 7 天 90%）账号时，撞线到下一次判定/切换之间有 ≤90 秒的请求失败间隙。已核实当事两账号额度上限相同（5h / 7d 均 100）：回驻是档位偏好而非容量经济决策。
