---
type: adr
title: 自动切换可选优先低档位账号
status: accepted
tags: [auto-swap, membership, prefer-lower-tier]
intent: Record the prefer-lower-tier auto-swap ranking table, unknown-last tradeoff, and that the decision key is the raw /usages membership.level string.
documents:
  config_keys:
    - prefer_lower_tier
  symbols:
    - decide_auto_swap
    - membership_tier_rank
---
# 自动切换可选优先低档位账号

额度耗尽自动切换默认按合格停泊账号的已用比例升序选号。issue #70 / #71 增加可选子开关
`prefer_lower_tier`（默认关）：开启后 `decide_auto_swap` 先按会员档位从低到高，同档再回落
原比较链（已用比例 → `last_used_at` → 账号 id）。档位排序需要一份全序和「未知怎么办」的
取舍；本 ADR 钉死决策键、排名表来源与宁晚烧权衡。

## 决策

1. **决策键是快照上的原始 `/usages` 字符串，不是展示名。**
   `parse_usages` 从 `user.membership.level` 写入 `Quota.membership`，额度快照
   `QuotaSnapshot.membership` 原样保留。`membership_tier_rank` 只匹配这些原始枚举。
   GUI 展示仍走 `prettify_membership`（Allegretto、Allegro 等）。用户可见文案继续避免
   Intermediate / Advanced，避免「套餐」「plan」。

2. **排名表（序号越小越优先）**：

   | 原始 `membership.level` | 序号 | 展示名（仅辨认，不参与比较） |
   | --- | --- | --- |
   | `LEVEL_ANDANTE` | 0 | Andante |
   | `LEVEL_MODERATO` | 1 | Moderato |
   | `LEVEL_INTERMEDIATE` | 2 | Allegretto |
   | `LEVEL_ADVANCED` | 3 | Allegro |
   | `LEVEL_TEAM` | 4 | Team |
   | `None` 与未列出的字符串 | 5 | — |

   全序按官网档位从低到高：Andante < Moderato < Allegretto < Allegro < Team。
   2026-09-04 确认：账号库没有 Andante / Moderato 账号，不要求用真实 `/usages`
   再核实这两档。内部枚举按已核实的 `LEVEL_` 前缀惯例写成 `LEVEL_ANDANTE` /
   `LEVEL_MODERATO`。

   已用真实 `/usages` 与 `/me` `user_level_name` 交叉的只有
   `LEVEL_INTERMEDIATE` → Allegretto、`LEVEL_ADVANCED` → Allegro。`LEVEL_TEAM`
   走 `prettify_membership` 去掉前缀后的 Title Case。第三方映射（tokenbank /
   kiage）与该交叉矛盾，不采用。未列出的字符串与 `None` 落入序号 5。

3. **未知排最后。**
   免费账号若 `/usages` 不给 `level`，快照 `membership` 为 `None`，被排到最后。这是已知
   隐患：优先低档位时可能先烧掉已识别的付费档、把无档位号留到最后。本 ADR 不改这一语义；
   落地后用真实号验证，不在文档票里「修」。

4. **默认关闭；范围只限自动切换候选排序。**
   `AutoSwap.prefer_lower_tier` 默认 `false`，与未开子开关时的比较链一致。不影响手动切换，
   不影响账号列表 / 总览的展示排序。不新增网络请求，只读已收集的额度快照。只剩同档或更高档
   合格候选时仍切换——子开关改的是优先顺序，不是资格门槛。
