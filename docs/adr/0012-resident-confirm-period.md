---
type: adr
title: 回驻确认期
status: accepted
tags: [auto-swap, resident-return, confirm-period]
intent: Record that resident return waits N consecutive eligible snapshot cycles (default 2) before swapping, resetting the per-account streak on any ineligible cycle, without changing exhaustion, cooldown, settle grace, or manual hold.
documents:
  config_keys:
    - resident_confirm_n
  symbols:
    - decide_resident_return
    - decide_resident_return_from_state
---
# 回驻确认期

回驻（ADR-0008）在低档账号刚恢复时立刻切回，容易碰上「恢复 → 回驻 → 立刻再耗尽」的往返。
本 ADR 钉死新增的确认期：目标停泊账号须连续 N 个额度快照周期保持合格，才真正切回。

## 决策

1. **键名、默认值、缺省兼容。** `auto_swap.resident_confirm_n`（`u32`）默认 `2`。
   `config.toml` 缺此键时与未配置行为一致。`0` 与 `1` 同义：第一个合格周期即切回。
   不进 GUI，只在 `config.toml` 调。

2. **只约束回驻路径。** 计数加在 `decide_resident_return` 选出本周期目标之后：目标连续
   合格周期数 ≥ N 才返回 `Some`。耗尽路径 `decide_auto_swap` 不读此键。全局冷却、
   安顿宽限、手动保持期语义不变，且仍先于确认计数；这些闸门挡住时不改计数。
   手动切换不受影响。判定只读已收集快照，不新增网络请求、定时器或 daemon。

3. **per-account 连续合格计数。** 状态文件增加 `resident_confirm` 映射（账号 id → 连续
   合格周期数）。缺字段 / 文件损坏 / 缺账号视为 0。本周期无合格回驻目标则整表清零；
   目标更换则只保留新目标的计数（从 1 起）。成功写入 `last_auto_swap_at` 或
   `manual_hold_until` 时清零——账号角色或手动决定已经变了，确认期从头计。

4. **默认 2 改变回驻时机，不改变资格线。** N=2 时首个合格周期静默（调用方无「没有可
   切换的账号」提示），第 2 个连续合格周期才切。资格线仍是 ADR-0008 的阈值合格 +
   档位严格更低；不套用防抖边际（ADR-0010）与快重置排序（ADR-0011）。
