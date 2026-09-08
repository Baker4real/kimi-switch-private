---
type: adr
title: 自动切换候选防抖边际
status: accepted
tags: [auto-swap, candidate-margin]
intent: Record that auto-swap candidates must beat the current account by a configurable usage-ratio margin, defaulting to zero, without changing the trigger line, cooldown, settle grace, manual hold, or resident-return eligibility; pace reuses the same predicate (ADR-0013).
documents:
  config_keys:
    - candidate_margin
  symbols:
    - decide_auto_swap
    - is_eligible_candidate
---
# 自动切换候选防抖边际

额度耗尽自动切换在多个账号都贴近阈值时，会在贴线停泊账号之间来回切。冷却只限制两次成功切换的最短间隔，并不要求「下一手明显更空」。本 ADR 钉死候选防抖边际：停泊账号必须比当前账号空出一段可配差值，才成为耗尽（以及后来的 pace）切换目标。

## 决策

1. **键名、默认值、缺省兼容。** `auto_swap.candidate_margin`（`f64`，0.0~1.0）默认 `0.0`。`config.toml` 缺此键时与未配置行为一致。加载时须为有限数值，并收敛到 `0.0..=1.0`（与 `threshold` 同一套区间）。不进 GUI，只在 `config.toml` 调。

2. **谓词加在耗尽路径的候选资格上，pace 复用同一条。** `decide_auto_swap` 在既有资格线（所有已知窗口已用比例 < `threshold`）之后，再要求：停泊账号最大已用比例 ≤ 当前账号最大已用比例 − 边际。触发线、`BelowThreshold` / 触达阈值后进入候选评估的路径不变。比较只读已收集快照上的已知窗口，不新增网络请求、定时器或 daemon。issue #114 / ADR-0013 让 pace 预切换复用本谓词，不另造差值键。

3. **默认 0 零行为变化。** 边际为 0 时，阈值合格的停泊账号最大已用已经 < 阈值 ≤ 当前最大已用，因此额外谓词恒真，既有表驱动用例原样通过。

4. **不改其它闸门，不作用于回驻。** 全局冷却、安顿宽限、手动保持期语义不变，且先于候选评估。`decide_resident_return` 的资格线仍是 ADR-0008 的阈值合格线，不套用本边际。手动切换不受影响。
