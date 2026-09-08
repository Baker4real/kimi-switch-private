---
type: adr
title: pace 预切换：预测撑不到 reset 时在预警水位提前切
status: accepted
tags: [auto-swap, pace, candidate-margin]
intent: Record that exhaustion auto-swap may optionally swap early when the current account's usage-over-elapsed pace exceeds 1 at a warn threshold, reusing candidate_margin for healthier parked accounts, without preempting exhaustion or adding a new delta key.
documents:
  config_keys:
    - pace_pre_swap
    - pace_warn_threshold
    - candidate_margin
  symbols:
    - decide_auto_swap
    - AutoSwapKind
---
# pace 预切换：预测撑不到 reset 时在预警水位提前切

额度耗尽自动切换只在已用比例达到 `threshold`（默认 0.99）才切号。窗口前半段烧得过快时，
等撞线才切往往已经来不及。issue #114 增加可选子开关：预测当前账号按现速撑不到
`reset_at` 时，在预警水位提前切到更健康的停泊账号。本 ADR 钉死键定义、pace 公式、
已知窗口时长、与防抖边际（ADR-0010）的复用，以及明确不抢占耗尽路径。

## 决策

1. **键名、默认值、缺省兼容。** `auto_swap.pace_pre_swap`（`bool`）默认 `false`。
   `auto_swap.pace_warn_threshold`（`f64`）默认 `0.8`；加载时须为有限数值，并收敛到
   `0.0..=1.0`（与 `threshold` / `candidate_margin` 同一套区间）。`config.toml` 缺这些键
   时与未配置行为一致。不进 GUI，只在 `config.toml` 调。关闭时 `decide_auto_swap` 不读
   pace，既有表驱动用例原样通过。

2. **pace = 已用比例 ÷ 窗口已流逝比例。** 已流逝比例只用快照里该窗口的 `reset_at` 与
   **已知窗口时长**：`QuotaWindow::FiveHour` = 5 小时，`SevenDay` = 7 天。窗口起点 =
   `reset_at − 时长`，**不用** `now − 时长` 去推。`Month` / `Custom` / 其它时长未知的窗口
   不参与。缺 `reset_at`、尚未进入窗口（`now` ≤ 起点）、或已过 `reset_at` → 该窗口
   pace 路径不参与。判定只读已收集快照，不新增网络请求、定时器或 daemon。

3. **「撑不到 reset」= 任一参与窗口 pace > 1。** 按当前速度会在 reset 前用尽。pace = 1
   视为恰好撑到，不提前切。账号级触发窗取 pace>1 的窗口中 pace 最高者（并列保留先见到的）；
   `AutoSwapTarget` 的 `trigger_window` / `usage_ratio` 对应该窗。

4. **预警水位。** 当前账号最大已用比例（所有已知窗口）≥ `pace_warn_threshold` 才考虑
   pace 路径。低于预警即使 pace>1 也不切，避免窗口刚开始的噪声。

5. **「候选更健康」复用 `candidate_margin`，不另造差值键，也不比较 pace。** 停泊候选
   仍须先过耗尽路径同一套资格线（所有已知窗口已用 < `threshold`、未摘出、快照可用），
   再满足最大已用 ≤ 当前最大已用 − `candidate_margin`（ADR-0010 同一谓词）。不要求候选
   自身 pace ≤ 1，不对 pace 值做差值比较：`candidate_margin` 的单位是已用比例，不是 pace。
   多候选时沿用耗尽路径的排序（档位偏好 → 快重置 → 已用比例 → `last_used_at` → id）。
   无合格候选时仍返回 `BelowThreshold`，不套用耗尽路径的「没有可切换的账号」。

6. **不抢占耗尽。** 当前账号任一窗口已达 `threshold` 时，仍走既有 `decide_auto_swap`
   耗尽逻辑（含边际、摘出、快重置排序），`AutoSwapKind::Exhausted`。pace 只在未达阈值
   的 Stay 分支里评估。命中则返回 `Swap` 且 `kind = Pace`；调用方仍走既有 `activate`。
   因此 pace 优先于回驻（回驻只在耗尽判定 Stay 之后）。冷却、安顿宽限仍先于两条路径；
   手动保持期只挡回驻，不挡 pace（与耗尽相同）。摘出账号（#112）不能当 pace 候选；
   作为当前账号时仍可被切走。

7. **审计与 JSON 只加字段。** 耗尽审计 detail 保持 `from=… window=… used=…%`（无 `kind=`）。
   回驻已是 `kind=return`。pace 成功为 `kind=pace from=… window=… used=…%`。
   `AutoSwapTarget` 增加 `kind`（`Exhausted` / `Pace`）。CLI JSON 事件（#113）在既有
   `swap.kind` 上增加值 `pace`；退出码映射不变（成功仍为 0）。
